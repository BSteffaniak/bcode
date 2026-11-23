//! Multi-source, multi-part, stack, and browse structure.

use super::super::{
    DocumentConfig, DocumentNeed, FileKey, ItemKey, LineKind, ReviewDocument, ReviewItem,
    SourceInput, SourceStack,
};
use super::support::{Edit, Fake, SynthFile, items, resolved};
use bcode_code_review_models::{
    ResolvedReviewStack, ResolvedSourceMode, ResolvedSourcePart, ResolvedStackLayer,
    ReviewBlobSelector, ReviewLineInterval, ReviewStackAttributionResponse,
    ReviewStackLineAttribution,
};
use std::path::PathBuf;

fn one_file() -> Fake {
    Fake::new(vec![SynthFile::modified(
        "s.rs",
        40,
        &[
            Edit {
                at: 5,
                remove: 1,
                add: 2,
            },
            Edit {
                at: 30,
                remove: 1,
                add: 1,
            },
        ],
        3,
    )])
}

#[test]
fn several_sources_and_parts_get_headers_and_distinct_file_keys() {
    let fake = one_file();
    let part = |index: u32| {
        let mut input = fake.input("commits");
        input.source.part = Some(ResolvedSourcePart {
            index,
            count: 2,
            label: format!("commit {index}"),
        });
        input
    };
    let doc = ReviewDocument::new(
        PathBuf::from("/repo"),
        vec![fake.input("worktree"), part(0), part(1)],
        DocumentConfig::default(),
    );
    let headers: Vec<(String, Option<u32>, String)> = items(&doc)
        .iter()
        .filter_map(|item| match item {
            ReviewItem::SourceHeader(header) => {
                Some((header.source_id.clone(), header.part, header.label.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        headers,
        vec![
            ("worktree".into(), None, "worktree".into()),
            ("commits".into(), Some(0), "commits · commit 0".into()),
            ("commits".into(), Some(1), "commits · commit 1".into()),
        ]
    );
    let files: Vec<FileKey> = doc.file_keys().map(|file| (**file).clone()).collect();
    assert_eq!(
        files,
        vec![
            FileKey::new("worktree", None, "s.rs"),
            FileKey::new("commits", Some(0), "s.rs"),
            FileKey::new("commits", Some(1), "s.rs"),
        ]
    );
    let persisted = ItemKey::FileHeader(files[2].clone().into())
        .to_persisted()
        .expect("persisted");
    assert_eq!(
        doc.resolve_persisted(&persisted),
        Some(ItemKey::FileHeader(files[2].clone().into()))
    );
    // A single plain source shows no header.
    assert!(
        !items(&fake.document(DocumentConfig::default()))
            .iter()
            .any(|item| matches!(item, ReviewItem::SourceHeader(_)))
    );
}

#[test]
fn series_focus_and_alternates_do_not_hide_or_count_the_wrong_group() {
    use bcode_code_review_models::{
        ReviewGroupKey, ReviewTarget, ReviewWorkspace, RevisionRef, ViewedMark,
    };

    let fake = one_file();
    let oid = "a".repeat(40);
    let mut member = fake.input("commits");
    member.source.head = RevisionRef::commit(oid.clone());
    member.source.part = Some(ResolvedSourcePart {
        index: 7,
        count: 8,
        label: "member".into(),
    });
    let member_key = ReviewGroupKey::Member {
        source_id: "commits".into(),
        member: oid,
    };
    let aggregate = ReviewGroupKey::Combined {
        source_id: "commits".into(),
    };
    let mut doc = ReviewDocument::new_with_series(
        PathBuf::from("/repo"),
        vec![member, fake.input("commits"), fake.input("other")],
        DocumentConfig::default(),
        series("commits", &["a".repeat(40)], false),
    );
    let file = |source: &str, part| ItemKey::FileHeader(FileKey::new(source, part, "s.rs").into());
    let member_file = file("commits", Some(7));
    assert_eq!(doc.group_of_key(&member_file), Some(&member_key));
    assert_eq!(doc.group_of_key(&file("commits", None)), Some(&aggregate));
    let anchor = doc.anchor_for(&member_file).expect("file anchor");
    assert_eq!(doc.draft_anchor(&anchor).group.as_ref(), Some(&member_key));
    let mut workspace =
        ReviewWorkspace::from_target(PathBuf::from("/repo"), ReviewTarget::WorkingTreeUnstaged);
    workspace
        .group_viewed_marks
        .insert(member_key.selector(), vec![ViewedMark::legacy("s.rs")]);
    doc.set_viewed_marks(&workspace);
    assert_eq!(
        doc.groups().map(|group| group.viewed).collect::<Vec<_>>(),
        [1, 0, 0]
    );
    doc.set_group_focus("commits", Some(&member_key));
    assert!(doc.index_of(&file("other", None)).is_some());
    assert!(doc.index_of(&file("commits", None)).is_none());
    assert_eq!(doc.viewed_progress().total, 3);
    doc.set_alternate_groups([aggregate].into());
    doc.set_group_focus("commits", None);
    assert_eq!(doc.viewed_progress().total, 2);
    assert!(doc.index_of(&member_file).is_some());
    assert!(doc.index_of(&file("commits", None)).is_none());
    let entry = &fake.input("other").entries[0];
    assert_eq!(
        doc.totals(),
        (
            2 * u64::from(entry.additions),
            2 * u64::from(entry.deletions)
        )
    );
}

#[test]
fn presentation_focus_survives_streaming_without_hiding_unrelated_sources() {
    use bcode_code_review_models::{ReviewGroupKey, ReviewItemKey, RevisionRef};

    let fake = one_file();
    let mut member = fake.input("commits");
    member.source.head = RevisionRef::commit("a".repeat(40));
    member.source.part = Some(ResolvedSourcePart {
        index: 2,
        count: 3,
        label: "third".into(),
    });
    let member_key = ReviewGroupKey::Member {
        source_id: "commits".into(),
        member: "a".repeat(40),
    };
    let mut doc = ReviewDocument::new_with_series(
        PathBuf::from("/repo"),
        vec![member, fake.input("commits")],
        DocumentConfig::default(),
        series("commits", &["a".repeat(40)], false),
    );
    assert!(doc.set_hidden_groups([member_key.clone()].into()));
    assert_eq!(doc.focus(), Some("commits"));
    let header = ReviewItemKey::SourceHeader {
        source_id: "commits".into(),
    };
    assert_eq!(
        doc.resolve_persisted(&header),
        Some(ItemKey::SourceHeader {
            source_id: "commits".into(),
            part: None,
        })
    );
    assert!(doc.upsert_group(fake.input("other"), 1));
    assert_eq!(doc.focus(), None);
    assert!(doc.active_group("other").is_some());
    assert!(doc.active_group(&member_key.selector()).is_none());
    assert_eq!(doc.viewed_progress().total, 3);
    assert!(doc.set_hidden_groups(std::collections::BTreeSet::new()));
    assert_eq!(doc.active_group(&member_key.selector()), Some(&member_key));
}

/// Kind, old line, new line, and layer of one changed line.
type LineLayer = (LineKind, Option<u32>, Option<u32>, Option<u64>);

fn stack() -> ResolvedReviewStack {
    ResolvedReviewStack {
        source_id: "stack".into(),
        trunk: "main".into(),
        trunk_oid: "t".into(),
        base_oid: "b".into(),
        layers: vec![
            ResolvedStackLayer {
                number: 11,
                title: "first".into(),
                base_oid: "b".into(),
                head_oid: "h1".into(),
                base_branch: String::new(),
                head_branch: String::new(),
            },
            ResolvedStackLayer {
                number: 12,
                title: "second".into(),
                base_oid: "h1".into(),
                head_oid: "h2".into(),
                base_branch: String::new(),
                head_branch: String::new(),
            },
        ],
    }
}

#[test]
fn whole_stack_sources_label_files_hunks_and_lines_with_layers() {
    let mut fake = one_file();
    fake.attribution.insert(
        "s.rs".into(),
        ReviewStackAttributionResponse {
            path: "s.rs".into(),
            new_ranges: vec![
                ReviewStackLineAttribution {
                    start: 5,
                    end: 5,
                    layer: 11,
                    layer_line: None,
                },
                ReviewStackLineAttribution {
                    start: 6,
                    end: 6,
                    layer: 12,
                    layer_line: None,
                },
            ],
            old_ranges: vec![ReviewStackLineAttribution {
                start: 30,
                end: 30,
                layer: 12,
                layer_line: None,
            }],
            binary: false,
        },
    );
    let mut input: SourceInput = fake.input("stack");
    input.stack = Some(SourceStack {
        stack: stack(),
        whole: true,
    });
    let mut doc = ReviewDocument::new(
        PathBuf::from("/repo"),
        vec![input],
        DocumentConfig::default(),
    );
    doc.set_viewport(0, doc.len() - 1);
    let mut attribution_requests = 0;
    for _ in 0..10 {
        let needs = doc.take_needs();
        if needs.is_empty() {
            break;
        }
        for need in &needs {
            if let DocumentNeed::Attribution { request, .. } = need {
                attribution_requests += 1;
                assert_eq!(request.stack, stack());
                assert_eq!(request.path, "s.rs");
            }
            assert!(fake.answer(&mut doc, need));
        }
    }
    assert_eq!(attribution_requests, 1);
    let all = items(&doc);
    let ReviewItem::SourceHeader(header) = &all[0] else {
        panic!("stack sources have a header");
    };
    let layer = header.layer.as_ref().expect("layer label");
    assert_eq!((layer.number, layer.position, layer.count), (12, 2, 2));
    assert!(layer.whole_stack);
    let ReviewItem::FileHeader(file) = &all[1] else {
        panic!("file header");
    };
    assert_eq!(file.layers, vec![11, 12]);
    let hunk_layers: Vec<Vec<u64>> = all
        .iter()
        .filter_map(|item| match item {
            ReviewItem::HunkHeader(hunk) => Some(hunk.layers.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(hunk_layers, vec![vec![11, 12], vec![12]]);
    let line_layers: Vec<LineLayer> = all
        .iter()
        .filter_map(|item| match item {
            ReviewItem::Line(line) if line.kind.is_change() => {
                Some((line.kind, line.old_line, line.new_line, line.layer))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        line_layers,
        vec![
            (LineKind::Removed, Some(5), None, None),
            (LineKind::Added, None, Some(5), Some(11)),
            (LineKind::Added, None, Some(6), Some(12)),
            (LineKind::Removed, Some(30), None, Some(12)),
            (LineKind::Added, None, Some(31), None),
        ]
    );
}

#[test]
fn browse_sources_reveal_their_line_range_without_hunks() {
    let fake = one_file();
    let mut input = fake.input("repo");
    input.source.mode = ResolvedSourceMode::Browse;
    input.source.line_range = Some(ReviewLineInterval { start: 3, end: 6 });
    let mut doc = ReviewDocument::new(
        PathBuf::from("/repo"),
        vec![input],
        DocumentConfig::default(),
    );
    doc.set_viewport(0, 0);
    let needs = doc.take_needs();
    assert!(
        needs
            .iter()
            .all(|need| matches!(need, DocumentNeed::BlobLines { .. })),
        "browse files never request hunks: {needs:?}"
    );
    let DocumentNeed::BlobLines { request, .. } = &needs[0] else {
        unreachable!()
    };
    assert_eq!(
        request.blob,
        ReviewBlobSelector::Object {
            oid: "new:s.rs".into()
        }
    );
    for need in &needs {
        assert!(fake.answer(&mut doc, need));
    }
    fake.settle(&mut doc);
    let lines: Vec<(Option<u32>, Option<&str>)> = items(&doc)
        .iter()
        .filter_map(|item| match item {
            ReviewItem::Line(line) => Some((line.new_line, None)),
            _ => None,
        })
        .collect();
    assert_eq!(
        lines.iter().map(|(line, _)| *line).collect::<Vec<_>>(),
        (3..=6).map(Some).collect::<Vec<_>>()
    );
    let gaps = items(&doc)
        .iter()
        .filter(|item| matches!(item, ReviewItem::Gap(_)))
        .count();
    assert_eq!(gaps, 2, "lines outside the range stay expandable");
    assert_eq!(resolved("x").source_id, "x");
}

#[test]
fn focusing_a_source_hides_the_others_until_unfocused() {
    let fake = one_file();
    let mut doc = ReviewDocument::new(
        PathBuf::from("/repo"),
        vec![fake.input("one"), fake.input("two"), fake.input("three")],
        DocumentConfig::default(),
    );
    let _ = doc.take_splices();
    let all = items(&doc).len();
    let header = |source: &str| ItemKey::SourceHeader {
        source_id: source.to_owned(),
        part: None,
    };
    let file = |source: &str| std::sync::Arc::new(FileKey::new(source, None, "s.rs"));
    assert_eq!(doc.focus(), None);

    assert!(doc.set_focus(Some("two")));
    doc.assert_consistent("focused");
    assert_eq!(doc.focus(), Some("two"));
    let splices = doc.take_splices();
    assert_ne!(splices.len(), 0, "{splices:?}");
    let shown: Vec<String> = items(&doc)
        .iter()
        .filter_map(|item| match item {
            ReviewItem::SourceHeader(header) => Some(header.source_id.clone()),
            ReviewItem::FileHeader(header) => Some(header.file.source_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(shown, ["two", "two"]);
    // Hidden items do not resolve and cannot be jumped to; the focused ones can.
    assert_eq!(doc.index_of(&header("one")), None);
    assert!(doc.jump_to(&ItemKey::FileHeader(file("three"))).is_none());
    assert_eq!(doc.index_of(&header("two")), Some(0));
    // Groups keep reporting their files while hidden.
    let hidden: Vec<(String, bool, usize)> = doc
        .groups()
        .map(|group| {
            (
                group.source.source_id.clone(),
                group.hidden,
                group.file_count,
            )
        })
        .collect();
    assert_eq!(
        hidden,
        [
            ("one".to_owned(), true, 1),
            ("two".to_owned(), false, 1),
            ("three".to_owned(), true, 1),
        ]
    );
    // Focusing the same source again changes nothing.
    assert!(!doc.set_focus(Some("two")));

    assert!(doc.set_focus(None));
    doc.assert_consistent("unfocused");
    assert_eq!(doc.focus(), None);
    assert_eq!(items(&doc).len(), all);
    assert!(doc.index_of(&header("one")).is_some());
}

#[test]
fn a_loading_group_fills_in_place_and_the_others_keep_their_state_and_loads() {
    use super::support::Shadow;
    let fake = one_file();
    let mut loading = fake.input("two");
    loading.entries.clear();
    loading.loading = true;
    let mut doc = ReviewDocument::new(
        PathBuf::from("/repo"),
        vec![fake.input("one"), loading, fake.input("three")],
        DocumentConfig::default(),
    );
    // Group one's file is loaded; group three's load is still in flight.
    doc.set_viewport(0, doc.len().saturating_sub(1));
    let needs = doc.take_needs();
    let source_of = |need: &DocumentNeed| match need {
        DocumentNeed::Hunks { file, .. } => file.source_id.clone(),
        other => panic!("unexpected need {other:?}"),
    };
    let (one, three): (Vec<_>, Vec<_>) = needs.iter().partition(|need| source_of(need) == "one");
    assert_eq!((one.len(), three.len()), (1, 1));
    assert!(fake.answer(&mut doc, one[0]));
    let _ = doc.take_splices();
    let mut shadow = Shadow::new(&doc);
    // The items after source `source`'s header, up to the next source's.
    let shown_for = |doc: &ReviewDocument, source: &str| -> Vec<ReviewItem> {
        let mut current = String::new();
        items(doc)
            .into_iter()
            .filter(|item| {
                if let ReviewItem::SourceHeader(header) = item {
                    current.clone_from(&header.source_id);
                }
                current == source
            })
            .collect()
    };
    let one_before = shown_for(&doc, "one");

    // Group two is listed: it fills in with one splice; group one keeps its loaded lines.
    assert!(doc.upsert_group(fake.input("two"), 1));
    let splices = doc.take_splices();
    assert_eq!(splices.len(), 1, "{splices:?}");
    let mut replay = doc.clone();
    replay.splices = splices;
    shadow.apply_and_check(&mut replay, "filled");
    assert_eq!(shown_for(&doc, "one"), one_before);
    // The same input again changes nothing.
    assert!(!doc.upsert_group(fake.input("two"), 1));
    // Group three's load, asked for before the change, still lands on its file.
    assert!(fake.answer(&mut doc, three[0]));
    fake.settle(&mut doc);
    doc.assert_consistent("three loaded");
    assert!(
        !shown_for(&doc, "three")
            .iter()
            .any(|item| matches!(item, ReviewItem::Placeholder(_)))
    );

    // A new group goes where it is asked to; groups keep workspace order.
    assert!(doc.upsert_group(fake.input("zero"), 0));
    doc.assert_consistent("inserted");
    let order: Vec<String> = doc
        .groups()
        .map(|group| group.source.source_id.clone())
        .collect();
    assert_eq!(order, ["zero", "one", "two", "three"]);
}

#[test]
fn a_thread_on_a_source_as_a_whole_shows_with_that_source_only() {
    use bcode_code_review_models::{
        DraftAnchor, DraftComment, ReviewAnchorKind, ReviewLineKind, ReviewThreadKind,
        ReviewThreadSeverity,
    };
    let fake = one_file();
    let mut doc = ReviewDocument::new(
        PathBuf::from("/repo"),
        vec![fake.input("one"), fake.input("two"), fake.input("three")],
        DocumentConfig::default(),
    );
    let draft = |thread: &str, source: Option<&str>| DraftComment {
        comment_id: format!("{thread}-1"),
        thread_id: thread.to_owned(),
        anchor: DraftAnchor {
            kind: ReviewAnchorKind::Review,
            file_path: super::super::REVIEW_LEVEL_PATH.to_owned(),
            diff_row: 0,
            start_diff_row: None,
            end_diff_row: None,
            old_start: None,
            old_end: None,
            new_start: None,
            new_end: None,
            old_line: None,
            new_line: None,
            line_kind: ReviewLineKind::Context,
            is_file_anchor: false,
            surface_id: None,
            source_id: source.map(str::to_owned),
            group: None,
            head_oid: None,
            blob_oid: None,
            reply_to: None,
        },
        body: "body".into(),
        created_at_ms: 1,
        updated_at_ms: 1,
        session_id: None,
        resolved_at_ms: None,
        thread_kind: ReviewThreadKind::Note,
        severity: ReviewThreadSeverity::Info,
    };
    doc.set_annotations(&super::super::Annotations {
        drafts: vec![draft("on-two", Some("two")), draft("on-review", None)],
        ..super::super::Annotations::default()
    });
    let _ = doc.take_splices();
    doc.assert_consistent("annotated");
    let thread = |id: &str| ItemKey::Thread {
        thread_id: id.to_owned(),
    };
    let header = |source: &str| ItemKey::SourceHeader {
        source_id: source.to_owned(),
        part: None,
    };
    // The source's thread follows its header, before its file; the review's stays on top.
    let on_two = doc.index_of(&thread("on-two")).expect("shown");
    assert_eq!(doc.index_of(&header("two")), Some(on_two - 1));
    assert!(matches!(
        doc.item(on_two + 1),
        Some(ReviewItem::FileHeader(file)) if file.file.source_id == "two"
    ));
    assert!(
        doc.index_of(&thread("on-review")).expect("shown")
            < doc.index_of(&header("one")).expect("one")
    );
    // Another source in focus: the thread is not shown; its own source in focus: it is.
    doc.set_focus(Some("one"));
    doc.assert_consistent("one focused");
    assert_eq!(doc.index_of(&thread("on-two")), None);
    assert!(doc.index_of(&thread("on-review")).is_some());
    doc.set_focus(Some("two"));
    assert!(doc.index_of(&thread("on-two")).is_some());
    // A composer opened from that thread shows right under it, not at the top of the review.
    doc.set_focus(None);
    let _ = doc.take_splices();
    doc.open_composer_after(super::super::DocAnchor::review(), thread("on-two"));
    doc.assert_consistent("composer under a source's thread");
    let on_two = doc.index_of(&thread("on-two")).expect("shown");
    assert_eq!(doc.index_of(&ItemKey::Composer), Some(on_two + 1));
    assert!(doc.close_composer().is_some());
    assert_eq!(doc.index_of(&ItemKey::Composer), None);
    doc.assert_consistent("composer closed");
}

/// The session's description of a series of `members` (full object ids).
fn series(
    source_id: &str,
    members: &[String],
    contiguous: bool,
) -> std::collections::BTreeMap<String, crate::session::CommitSeriesState> {
    [(
        source_id.to_owned(),
        crate::session::CommitSeriesState {
            source_id: source_id.to_owned(),
            members: members
                .iter()
                .map(|oid| (oid.clone(), format!("{oid} subject")))
                .collect(),
            view: bcode_code_review_models::ReviewSeriesView::AllMembers,
            contiguous,
            unavailable: None,
        },
    )]
    .into()
}

/// A file comment written before the source was a series: no group, its source and head.
fn legacy_draft(thread: &str, head: Option<&str>) -> bcode_code_review_models::DraftComment {
    let anchor: bcode_code_review_models::DraftAnchor = serde_json::from_value(serde_json::json!({
        "kind": "file", "file_path": "s.rs", "diff_row": 0, "line_kind": "context",
        "source_id": "commits", "head_oid": head,
    }))
    .expect("legacy anchor");
    bcode_code_review_models::DraftComment {
        comment_id: format!("{thread}-1"),
        thread_id: thread.to_owned(),
        anchor,
        body: "body".into(),
        created_at_ms: 1,
        updated_at_ms: 1,
        session_id: None,
        resolved_at_ms: None,
        thread_kind: bcode_code_review_models::ReviewThreadKind::Note,
        severity: bcode_code_review_models::ReviewThreadSeverity::Info,
    }
}

fn thread_block(doc: &ReviewDocument, thread: &str) -> Option<usize> {
    doc.annotation_blocks
        .get(&ItemKey::Thread {
            thread_id: thread.to_owned(),
        })
        .copied()
}

fn file_block(doc: &ReviewDocument, group: &bcode_code_review_models::ReviewGroupKey) -> usize {
    let index = doc
        .groups()
        .find(|info| info.key == group)
        .expect("group")
        .index;
    let file = doc.group_files(index).next().expect("file").clone();
    doc.block_of_key(&ItemKey::FileHeader(file))
        .expect("file block")
}

#[test]
fn legacy_anchors_and_marks_of_a_sparse_series_belong_to_their_member() {
    use bcode_code_review_models::{ReviewGroupKey, ReviewWorkspace, RevisionRef};

    let fake = one_file();
    let (a, b) = ("a".repeat(40), "b".repeat(40));
    let member = |oid: &str, index: u32| {
        let mut input = fake.input("commits");
        input.source.head = RevisionRef::commit(oid.to_owned());
        input.source.part = Some(ResolvedSourcePart {
            index,
            count: 2,
            label: oid[..7].to_owned(),
        });
        input
    };
    let series = series("commits", &[a.clone(), b.clone()], false);
    let state = series["commits"].clone();
    let mut doc = ReviewDocument::new_with_series(
        PathBuf::from("/repo"),
        vec![member(&a, 0), member(&b, 1)],
        DocumentConfig::default(),
        series,
    );
    let key = |oid: &str| ReviewGroupKey::Member {
        source_id: "commits".into(),
        member: oid.to_owned(),
    };
    doc.set_annotations(&crate::document::Annotations {
        drafts: vec![
            legacy_draft("second", Some(&b)),
            legacy_draft("unknown", Some(&"c".repeat(40))),
        ],
        exchanges: Vec::new(),
        suggestions: Vec::new(),
    });
    assert_eq!(
        doc.anchor_group(&legacy_draft("second", Some(&b)).anchor),
        Some(key(&b))
    );
    assert_eq!(
        thread_block(&doc, "second"),
        Some(file_block(&doc, &key(&b)))
    );
    // A head that is no member is not attached to a member by position.
    assert_eq!(thread_block(&doc, "unknown"), Some(0));

    // The ungrouped format: workspace-wide marks only, no group state.
    let mut workspace: ReviewWorkspace = serde_json::from_str(
        r#"{"id": "w", "title": "t", "repo_root": "/repo", "sources": [], "viewed_files": ["s.rs"]}"#,
    )
    .expect("old workspace");
    doc.set_viewed_marks(&workspace);
    assert_eq!(doc.groups().map(|g| g.viewed).collect::<Vec<_>>(), [0, 0]);
    assert!(state.adopt_legacy_marks(&mut workspace));
    doc.set_viewed_marks(&workspace);
    assert_eq!(doc.groups().map(|g| g.viewed).collect::<Vec<_>>(), [1, 1]);
    // Adopted once: a later ordinary mark stays out of the series.
    crate::viewed::unmark_group_viewed(
        &mut workspace,
        &key(&a),
        &crate::viewed::ViewedIdentity {
            path: "s.rs",
            old_path: None,
            old_oid: None,
            new_oid: None,
            patch_id: None,
        },
    );
    assert!(!state.adopt_legacy_marks(&mut workspace));
    doc.set_viewed_marks(&workspace);
    assert_eq!(doc.groups().map(|g| g.viewed).collect::<Vec<_>>(), [0, 1]);
}

#[test]
fn legacy_anchors_of_a_contiguous_series_belong_to_the_combined_diff_only() {
    use bcode_code_review_models::{ReviewGroupKey, RevisionRef};

    let fake = one_file();
    let (a, b) = ("a".repeat(40), "b".repeat(40));
    let mut newest = fake.input("commits");
    newest.source.head = RevisionRef::commit(b.clone());
    newest.source.part = Some(ResolvedSourcePart {
        index: 1,
        count: 2,
        label: "newest".into(),
    });
    let combined = ReviewGroupKey::Combined {
        source_id: "commits".into(),
    };
    let annotations = crate::document::Annotations {
        drafts: vec![legacy_draft("old", Some(&b))],
        exchanges: Vec::new(),
        suggestions: Vec::new(),
    };
    let series = series("commits", &[a, b], true);
    let mut shown = ReviewDocument::new_with_series(
        PathBuf::from("/repo"),
        vec![fake.input("commits")],
        DocumentConfig::default(),
        series.clone(),
    );
    shown.set_annotations(&annotations);
    assert_eq!(
        thread_block(&shown, "old"),
        Some(file_block(&shown, &combined))
    );
    // While only the newest member is shown, the comment is not retargeted to it.
    let mut member = ReviewDocument::new_with_series(
        PathBuf::from("/repo"),
        vec![newest],
        DocumentConfig::default(),
        series,
    );
    member.set_annotations(&annotations);
    assert_eq!(thread_block(&member, "old"), None);
}
