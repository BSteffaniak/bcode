//! Anchors, threads, composer and viewed state.

use super::super::{
    AnchorPlacement, AnchorTarget, Annotations, DocumentConfig, ItemKey, OutdatedReason, ReviewItem,
};
use super::support::{Edit, Fake, SynthFile, items};
use crate::viewed::ViewedState;
use bcode_code_review_models::{
    DraftAnchor, DraftComment, ReviewAiExchange, ReviewAiExchangeStatus, ReviewAnchorKind,
    ReviewLineKind, ReviewLineSide, ReviewSuggestion, ReviewSuggestionStatus, ReviewTarget,
    ReviewThreadKind, ReviewThreadSeverity, ReviewWorkspace, ViewedMark,
};
use std::path::PathBuf;

fn anchor(path: &str, kind: ReviewLineKind, old: Option<u32>, new: Option<u32>) -> DraftAnchor {
    DraftAnchor {
        kind: ReviewAnchorKind::Range,
        file_path: path.to_string(),
        diff_row: 0,
        start_diff_row: None,
        end_diff_row: None,
        old_start: None,
        old_end: None,
        new_start: None,
        new_end: None,
        old_line: old,
        new_line: new,
        line_kind: kind,
        is_file_anchor: false,
        surface_id: None,
        source_id: None,
        head_oid: None,
        blob_oid: None,
        reply_to: None,
    }
}

fn draft(thread: &str, anchor: DraftAnchor, created: u64) -> DraftComment {
    DraftComment {
        comment_id: format!("{thread}-{created}"),
        thread_id: thread.to_string(),
        anchor,
        body: "body".into(),
        created_at_ms: created,
        updated_at_ms: created,
        session_id: None,
        resolved_at_ms: None,
        thread_kind: ReviewThreadKind::Note,
        severity: ReviewThreadSeverity::Info,
    }
}

fn sample() -> Fake {
    // Old 40 lines; edit at old 10: remove 2, add 3 (new 10..=12). Context 3.
    Fake::new(vec![SynthFile::modified(
        "a.rs",
        40,
        &[Edit {
            at: 10,
            remove: 2,
            add: 3,
        }],
        3,
    )])
}

/// Placement of the item with `key` and the key of the item it follows.
fn placement_of(items: &[ReviewItem], key: &ItemKey) -> (AnchorPlacement, ItemKey) {
    let index = items
        .iter()
        .position(|item| &item.key() == key)
        .unwrap_or_else(|| panic!("{key:?} is shown"));
    let placement = match &items[index] {
        ReviewItem::Thread(thread) => thread.placement,
        ReviewItem::AgentThread(agent) => agent.placement,
        other => panic!("not an annotation: {other:?}"),
    };
    let previous = items[..index]
        .iter()
        .rev()
        .find(|item| !matches!(item, ReviewItem::Thread(_) | ReviewItem::AgentThread(_)))
        .expect("previous")
        .key();
    (placement, previous)
}

fn thread(id: &str) -> ItemKey {
    ItemKey::Thread {
        thread_id: id.to_string(),
    }
}

/// Annotations covering every placement: line, removed line, changed, hidden, missing line,
/// missing file, review level, file level, and a range.
fn placement_annotations() -> Annotations {
    let mut review = anchor("<review>", ReviewLineKind::Context, None, None);
    review.kind = ReviewAnchorKind::Review;
    let mut file_note = anchor("a.rs", ReviewLineKind::Context, None, None);
    file_note.kind = ReviewAnchorKind::File;
    let mut range = anchor("a.rs", ReviewLineKind::Context, None, None);
    range.new_start = Some(9);
    range.new_end = Some(11);
    Annotations {
        drafts: vec![
            draft(
                "added",
                anchor("a.rs", ReviewLineKind::Added, None, Some(11)),
                1,
            ),
            draft(
                "removed",
                anchor("a.rs", ReviewLineKind::Removed, Some(10), None),
                2,
            ),
            draft(
                "changed",
                anchor("a.rs", ReviewLineKind::Removed, Some(8), None),
                3,
            ),
            draft(
                "hidden",
                anchor("a.rs", ReviewLineKind::Context, Some(30), Some(31)),
                4,
            ),
            draft(
                "missing",
                anchor("a.rs", ReviewLineKind::Context, None, Some(500)),
                5,
            ),
            draft(
                "gone",
                anchor("gone.rs", ReviewLineKind::Context, None, Some(1)),
                6,
            ),
            draft("review", review, 7),
            draft("file", file_note, 8),
            draft("range", range, 9),
        ],
        ..Annotations::default()
    }
}

#[test]
fn annotations_are_placed_at_their_anchor_or_labeled_outdated() {
    let fake = sample();
    let file = || std::sync::Arc::new(fake.file("a.rs"));
    let annotations = placement_annotations();
    let mut doc = fake.document(DocumentConfig::default());
    doc.set_annotations(&annotations);
    // Before the file loads only file-level annotations are shown; the header counts all.
    let shown = items(&doc);
    assert!(shown.iter().any(|item| item.key() == thread("file")));
    assert!(!shown.iter().any(|item| item.key() == thread("added")));
    let ReviewItem::FileHeader(header) = shown
        .iter()
        .find(|item| matches!(item, ReviewItem::FileHeader(_)))
        .expect("header")
    else {
        unreachable!()
    };
    assert_eq!(header.thread_count, 7);

    fake.load_all(&mut doc);
    let all = items(&doc);
    let line = |side, line| ItemKey::Line {
        file: file(),
        side,
        line,
    };
    assert_eq!(
        placement_of(&all, &thread("added")),
        (AnchorPlacement::Line, line(ReviewLineSide::New, 11))
    );
    assert_eq!(
        placement_of(&all, &thread("removed")),
        (AnchorPlacement::Line, line(ReviewLineSide::Old, 10))
    );
    assert_eq!(
        placement_of(&all, &thread("range")),
        (AnchorPlacement::Line, line(ReviewLineSide::New, 11))
    );
    // Old line 8 is context now: the anchor is labeled, not moved.
    let header_key = ItemKey::FileHeader(file());
    assert_eq!(
        placement_of(&all, &thread("changed")),
        (
            AnchorPlacement::Outdated(OutdatedReason::LineChanged),
            header_key.clone()
        )
    );
    assert_eq!(
        placement_of(&all, &thread("missing")),
        (
            AnchorPlacement::Outdated(OutdatedReason::LineMissing),
            header_key
        )
    );
    assert_eq!(placement_of(&all, &thread("file")).0, AnchorPlacement::File);
    // Hidden context: shown after the gap that hides it.
    let (placement, previous) = placement_of(&all, &thread("hidden"));
    assert_eq!(placement, AnchorPlacement::HiddenContext);
    let ItemKey::Gap { start, .. } = previous else {
        panic!("follows a gap: {previous:?}");
    };
    assert_eq!(start, 16);
    let ReviewItem::Gap(gap) = all.iter().find(|item| item.key() == previous).expect("gap") else {
        unreachable!()
    };
    assert_eq!(gap.hidden_threads, 1);
    // Review-level and missing-file annotations sit at the top.
    assert_eq!(all[0].key(), thread("gone"));
    assert!(
        matches!(&all[0], ReviewItem::Thread(t) if t.placement == AnchorPlacement::Outdated(OutdatedReason::FileMissing))
    );
    assert!(matches!(&all[1], ReviewItem::Thread(t) if t.placement == AnchorPlacement::Review));

    // Revealing the hidden line attaches the thread to it.
    let gap_key = previous;
    assert!(doc.reveal_all(&gap_key));
    fake.settle(&mut doc);
    let all = items(&doc);
    assert_eq!(
        placement_of(&all, &thread("hidden")),
        (AnchorPlacement::Line, line(ReviewLineSide::New, 31))
    );
}

#[test]
fn line_anchors_are_exact_while_their_side_keeps_its_blob() {
    let fake = sample();
    let synth = SynthFile::modified(
        "a.rs",
        40,
        &[Edit {
            at: 10,
            remove: 2,
            add: 3,
        }],
        3,
    );
    let (old_oid, new_oid) = (synth.old_oid(), synth.new_oid());
    let other = Some("f".repeat(40));
    let with = |kind, old, new, head: Option<String>, blob: Option<String>| {
        let mut anchor = anchor("a.rs", kind, old, new);
        anchor.head_oid = head;
        anchor.blob_oid = blob;
        anchor
    };
    let annotations = Annotations {
        drafts: vec![
            // Same new-side blob at another head: exact.
            draft(
                "new-same",
                with(
                    ReviewLineKind::Added,
                    None,
                    Some(11),
                    other.clone(),
                    Some(new_oid.clone()),
                ),
                1,
            ),
            // Removed lines check the old side's blob.
            draft(
                "old-same",
                with(
                    ReviewLineKind::Removed,
                    Some(10),
                    None,
                    other.clone(),
                    Some(old_oid.clone()),
                ),
                2,
            ),
            draft(
                "old-changed",
                with(
                    ReviewLineKind::Removed,
                    Some(11),
                    None,
                    None,
                    Some(new_oid.clone()),
                ),
                3,
            ),
            draft(
                "new-changed",
                with(ReviewLineKind::Added, None, Some(12), None, other.clone()),
                4,
            ),
            // No blob recorded: the head decides.
            draft(
                "legacy",
                with(ReviewLineKind::Added, None, Some(10), other, None),
                5,
            ),
        ],
        ..Annotations::default()
    };
    let mut doc = fake.document(DocumentConfig::default());
    doc.set_annotations(&annotations);
    fake.load_all(&mut doc);
    let all = items(&doc);
    let placement = |id: &str| placement_of(&all, &thread(id)).0;
    assert_eq!(placement("new-same"), AnchorPlacement::Line);
    assert_eq!(placement("old-same"), AnchorPlacement::Line);
    for id in ["old-changed", "new-changed"] {
        assert_eq!(
            placement(id),
            AnchorPlacement::Outdated(OutdatedReason::ContentChanged),
            "{id}"
        );
    }
    assert_eq!(
        placement("legacy"),
        AnchorPlacement::Outdated(OutdatedReason::RevisionChanged)
    );
    // New anchors record the blob of the side they end on.
    let file = std::sync::Arc::new(fake.file("a.rs"));
    let recorded = |side, line| {
        let key = ItemKey::Line {
            file: file.clone(),
            side,
            line,
        };
        doc.anchor_for(&key)
            .expect("anchor")
            .to_draft_anchor()
            .blob_oid
    };
    assert_eq!(recorded(ReviewLineSide::New, 11), Some(new_oid));
    assert_eq!(recorded(ReviewLineSide::Old, 10), Some(old_oid));
}

#[test]
fn exchanges_and_suggestions_group_by_session_and_anchor() {
    let fake = sample();
    let at = anchor("a.rs", ReviewLineKind::Added, None, Some(10));
    let suggestion = |id: &str, session: Option<&str>, anchor: DraftAnchor| ReviewSuggestion {
        suggestion_id: id.to_string(),
        anchor,
        body: "s".into(),
        session_id: session.map(str::to_string),
        status: ReviewSuggestionStatus::Suggested,
        rationale: None,
        created_at_ms: 5,
        updated_at_ms: 5,
    };
    let annotations = Annotations {
        drafts: vec![draft("t", at.clone(), 1)],
        exchanges: vec![ReviewAiExchange {
            schema_version: 1,
            exchange_id: "x".into(),
            anchor: at.clone(),
            thread_id: None,
            question: "why?".into(),
            session_id: Some("session".into()),
            status: ReviewAiExchangeStatus::Linked,
            error: None,
            created_at_ms: 2,
            updated_at_ms: 2,
        }],
        suggestions: vec![
            suggestion(
                "by-session",
                Some("session"),
                anchor("a.rs", ReviewLineKind::Added, None, Some(12)),
            ),
            suggestion("by-anchor", None, at),
            suggestion(
                "orphan",
                None,
                anchor("a.rs", ReviewLineKind::Added, None, Some(12)),
            ),
        ],
    };
    let mut doc = fake.document(DocumentConfig::default());
    doc.set_annotations(&annotations);
    fake.load_all(&mut doc);
    let all = items(&doc);
    let agent = all
        .iter()
        .find_map(|item| match item {
            ReviewItem::AgentThread(agent) if agent.id == "x" => Some(agent),
            _ => None,
        })
        .expect("exchange");
    assert_eq!(agent.suggestion_ids, vec!["by-session".to_string()]);
    let thread = all
        .iter()
        .find_map(|item| match item {
            ReviewItem::Thread(thread) => Some(thread),
            _ => None,
        })
        .expect("thread");
    assert_eq!(thread.suggestion_ids, vec!["by-anchor".to_string()]);
    let orphan = all
        .iter()
        .find_map(|item| match item {
            ReviewItem::AgentThread(agent) if agent.id == "orphan" => Some(agent),
            _ => None,
        })
        .expect("orphan suggestion item");
    assert_eq!(orphan.exchange_id, None);
}

fn exchange(id: &str, anchor: DraftAnchor, thread: Option<&str>, created: u64) -> ReviewAiExchange {
    ReviewAiExchange {
        schema_version: 1,
        exchange_id: id.into(),
        anchor,
        thread_id: thread.map(str::to_owned),
        question: "why?".into(),
        session_id: None,
        status: ReviewAiExchangeStatus::Linked,
        error: None,
        created_at_ms: created,
        updated_at_ms: created,
    }
}

fn agent(id: &str) -> ItemKey {
    ItemKey::AgentThread { id: id.to_string() }
}

/// The keys of the annotations shown, in document order.
fn annotation_keys(items: &[ReviewItem]) -> Vec<ItemKey> {
    items
        .iter()
        .filter(|item| matches!(item, ReviewItem::Thread(_) | ReviewItem::AgentThread(_)))
        .map(ReviewItem::key)
        .collect()
}

#[test]
fn an_exchange_asked_from_a_thread_is_shown_with_it_wherever_the_thread_is() {
    let fake = sample();
    let file = || std::sync::Arc::new(fake.file("a.rs"));
    let line = |line| ItemKey::Line {
        file: file(),
        side: ReviewLineSide::New,
        line,
    };
    // Asked on new line 10 of content that has changed since: alone, outdated.
    let mut asked_at = anchor("a.rs", ReviewLineKind::Added, None, Some(10));
    asked_at.blob_oid = Some("f".repeat(40));
    let place = |thread_anchor: Option<DraftAnchor>| {
        let mut drafts = vec![draft(
            "later",
            anchor("a.rs", ReviewLineKind::Added, None, Some(11)),
            2,
        )];
        drafts.extend(thread_anchor.map(|at| draft("t", at, 1)));
        let annotations = Annotations {
            drafts,
            exchanges: vec![
                exchange("first", asked_at.clone(), Some("t"), 3),
                exchange("second", asked_at.clone(), Some("t"), 4),
                exchange("plain", asked_at.clone(), None, 5),
            ],
            ..Annotations::default()
        };
        let mut doc = fake.document(DocumentConfig::default());
        doc.set_annotations(&annotations);
        fake.load_all(&mut doc);
        items(&doc)
    };

    // With its thread on line 11: right after the thread, before a later thread on that line.
    let all = place(Some(anchor("a.rs", ReviewLineKind::Added, None, Some(11))));
    let keys = annotation_keys(&all);
    let at = |key: &ItemKey| keys.iter().position(|shown| shown == key).expect("shown");
    assert_eq!(at(&agent("first")), at(&thread("t")) + 1, "{keys:?}");
    assert_eq!(at(&agent("second")), at(&thread("t")) + 2, "{keys:?}");
    assert_eq!(at(&thread("later")), at(&thread("t")) + 3, "{keys:?}");
    for id in ["first", "second"] {
        assert_eq!(
            placement_of(&all, &agent(id)),
            (AnchorPlacement::Line, line(11)),
            "{id}"
        );
    }
    // An exchange asked from no thread stays at its own anchor.
    assert_eq!(
        placement_of(&all, &agent("plain")).0,
        AnchorPlacement::Outdated(OutdatedReason::ContentChanged)
    );

    // The thread moved to line 12: the exchanges move with it.
    let all = place(Some(anchor("a.rs", ReviewLineKind::Added, None, Some(12))));
    assert_eq!(
        placement_of(&all, &agent("first")),
        (AnchorPlacement::Line, line(12))
    );

    // The thread on its file only: so are its exchanges, right after it.
    let mut on_file = anchor("a.rs", ReviewLineKind::Context, None, None);
    on_file.kind = ReviewAnchorKind::File;
    let all = place(Some(on_file));
    let keys = annotation_keys(&all);
    let at = |key: &ItemKey| keys.iter().position(|shown| shown == key).expect("shown");
    assert_eq!(at(&agent("first")), at(&thread("t")) + 1, "{keys:?}");
    assert_eq!(placement_of(&all, &agent("first")).0, AnchorPlacement::File);

    // The thread on the review as a whole (a pull request thread on its layer): the same.
    let mut on_review = anchor("<review>", ReviewLineKind::Context, None, None);
    on_review.kind = ReviewAnchorKind::Review;
    let all = place(Some(on_review));
    assert_eq!(
        annotation_keys(&all)[..3],
        [thread("t"), agent("first"), agent("second")]
    );
    assert!(
        matches!(&all[1], ReviewItem::AgentThread(agent) if agent.placement == AnchorPlacement::Review),
        "{:?}",
        all[1]
    );

    // The thread is gone: each exchange falls back to its own anchor, outdated.
    let all = place(None);
    for id in ["first", "second"] {
        assert_eq!(
            placement_of(&all, &agent(id)).0,
            AnchorPlacement::Outdated(OutdatedReason::ContentChanged),
            "{id}"
        );
    }
}

#[test]
fn composer_and_range_anchors() {
    let fake = sample();
    let file = std::sync::Arc::new(fake.file("a.rs"));
    let mut doc = fake.document(DocumentConfig::default());
    fake.load_all(&mut doc);
    let key = |side, line| ItemKey::Line {
        file: file.clone(),
        side,
        line,
    };
    // A range from a context line through a removed line to an added line.
    let anchor = doc
        .range_anchor(&key(ReviewLineSide::New, 12), &key(ReviewLineSide::New, 8))
        .expect("range");
    let AnchorTarget::Lines(lines) = &anchor.target else {
        panic!("lines");
    };
    assert_eq!(lines.old, Some((8, 11)));
    assert_eq!(lines.new, Some((8, 12)));
    assert_eq!(
        (lines.end_side, lines.end_line()),
        (ReviewLineSide::New, 12)
    );
    assert!(lines.is_range);
    let persisted = anchor.to_draft_anchor();
    assert_eq!(persisted.new_start, Some(8));
    assert_eq!(persisted.old_end, Some(11));
    assert_eq!(persisted.source_id.as_deref(), Some("source-1"));

    // The composer sits after the anchor line and after threads already there.
    doc.set_annotations(&Annotations {
        drafts: vec![draft("t", persisted, 1)],
        ..Annotations::default()
    });
    doc.open_composer(anchor.clone());
    let all = items(&doc);
    let composer = all
        .iter()
        .position(|item| item.key() == ItemKey::Composer)
        .expect("composer");
    assert_eq!(all[composer - 1].key(), thread("t"));
    assert_eq!(all[composer - 2].key(), key(ReviewLineSide::New, 12));
    assert_eq!(doc.index_of(&ItemKey::Composer), Some(composer));
    assert_eq!(doc.close_composer(), Some(anchor));
    assert_eq!(doc.index_of(&ItemKey::Composer), None);

    // File and review anchors.
    let header = doc
        .anchor_for(&ItemKey::FileHeader(file.clone()))
        .expect("file anchor");
    assert_eq!(header.target, AnchorTarget::File);
    assert_eq!(header.to_draft_anchor().kind, ReviewAnchorKind::File);
    doc.open_composer(super::super::DocAnchor::review());
    assert_eq!(doc.item(0).map(ReviewItem::key), Some(ItemKey::Composer));
}

#[test]
fn viewed_state_matches_marks_by_identity_and_patch() {
    let one_edit = |path: &str| {
        SynthFile::modified(
            path,
            20,
            &[Edit {
                at: 3,
                remove: 1,
                add: 1,
            }],
            3,
        )
    };
    let fake = Fake::new(
        ["same.rs", "rebased.rs", "legacy.rs", "fresh.rs"]
            .into_iter()
            .map(one_edit)
            .collect(),
    );
    let mut workspace =
        ReviewWorkspace::from_target(PathBuf::from("/repo"), ReviewTarget::WorkingTreeUnstaged);
    let same = &fake.files["same.rs"];
    let rebased = &fake.files["rebased.rs"];
    workspace.viewed_files.extend(
        ["same.rs", "rebased.rs", "legacy.rs"]
            .into_iter()
            .map(str::to_string),
    );
    workspace.viewed_marks = vec![
        ViewedMark {
            path: "same.rs".into(),
            old_oid: Some(same.old_oid()),
            new_oid: Some(same.new_oid()),
            patch_id: None,
        },
        ViewedMark {
            path: "rebased.rs".into(),
            old_oid: Some("old-before-rebase".into()),
            new_oid: Some("new-before-rebase".into()),
            patch_id: rebased.patch_id.clone(),
        },
    ];
    let mut doc = fake.document(DocumentConfig::default());
    doc.set_viewed_marks(&workspace);
    let state = |doc: &super::super::ReviewDocument, path: &str| {
        doc.viewed_state(&fake.file(path)).expect("file")
    };
    // Blob identity matches without hunks; patch ids need hunks.
    assert_eq!(state(&doc, "same.rs"), ViewedState::Viewed);
    assert_eq!(state(&doc, "rebased.rs"), ViewedState::Unviewed);
    assert_eq!(state(&doc, "legacy.rs"), ViewedState::ViewedNeedsUpdate);
    assert_eq!(state(&doc, "fresh.rs"), ViewedState::Unviewed);
    assert_eq!(doc.viewed_progress().viewed, 2);
    fake.load_all(&mut doc);
    assert_eq!(state(&doc, "rebased.rs"), ViewedState::ViewedNeedsUpdate);
    assert_eq!(doc.viewed_progress().viewed, 3);
    assert_eq!(doc.viewed_progress().total, 4);
    let mut update: Vec<String> = doc
        .viewed_marks_to_update()
        .iter()
        .map(|file| file.path.clone())
        .collect();
    update.sort();
    // A blob-identity mark without a patch id is re-recorded once the patch id is known.
    assert_eq!(update, vec!["legacy.rs", "rebased.rs", "same.rs"]);
    // Re-recording with the current identity makes the marks exact.
    for path in &update {
        let (identity, patch) = doc.viewed_identity(&fake.file(path)).expect("identity");
        crate::viewed::mark_viewed(&mut workspace, &identity.with_patch(patch.as_deref()));
    }
    doc.set_viewed_marks(&workspace);
    assert_eq!(doc.viewed_marks_to_update(), Vec::new());
    let header_viewed: Vec<bool> = items(&doc)
        .iter()
        .filter_map(|item| match item {
            ReviewItem::FileHeader(header) => Some(header.viewed),
            _ => None,
        })
        .collect();
    // Files are in path order: fresh, legacy, rebased, same.
    assert_eq!(header_viewed, vec![false, true, true, true]);
}
