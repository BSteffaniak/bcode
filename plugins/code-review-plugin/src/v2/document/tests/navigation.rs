//! Navigation targets, search, and jumps (spec 6.2).

use super::super::{
    Annotations, Direction, DocumentConfig, ItemKey, LayoutMode, NavKind, ReviewDocument,
    ReviewItem, SearchQuery,
};
use super::support::{Edit, Fake, SynthFile, items};
use bcode_code_review_models::{
    DraftAnchor, DraftComment, ReviewAnchorKind, ReviewLineKind, ReviewLineSide, ReviewTarget,
    ReviewThreadKind, ReviewThreadSeverity, ReviewWorkspace,
};
use std::path::PathBuf;

fn fake() -> Fake {
    let edits = [
        Edit {
            at: 5,
            remove: 1,
            add: 1,
        },
        Edit {
            at: 30,
            remove: 2,
            add: 0,
        },
    ];
    Fake::new(vec![
        SynthFile::modified("a.rs", 50, &edits, 3),
        SynthFile::modified("b.rs", 50, &edits, 3),
        SynthFile::modified("c.rs", 50, &edits, 3),
    ])
}

/// A document with `a.rs` and `b.rs` resident and `c.rs` not.
fn partially_loaded(fake: &Fake) -> ReviewDocument {
    let mut doc = fake.document(DocumentConfig {
        prefetch_rows: 0,
        protected_rows: 0,
        ..DocumentConfig::default()
    });
    doc.set_viewport(0, 0);
    fake.settle(&mut doc);
    doc.set_viewport(
        doc.index_of(&ItemKey::FileHeader(fake.file("b.rs").into()))
            .expect("b"),
        doc.index_of(&ItemKey::FileHeader(fake.file("b.rs").into()))
            .expect("b"),
    );
    fake.settle(&mut doc);
    assert!(doc.files[0].loaded().is_some() && doc.files[1].loaded().is_some());
    assert!(doc.files[2].loaded().is_none());
    doc
}

fn header(fake: &Fake, path: &str) -> ItemKey {
    ItemKey::FileHeader(fake.file(path).into())
}

#[test]
fn file_hunk_and_change_motions_cover_the_whole_review() {
    let fake = fake();
    let doc = partially_loaded(&fake);
    let all = items(&doc);
    let hunks: Vec<usize> = all
        .iter()
        .enumerate()
        .filter(|(_, item)| matches!(item, ReviewItem::HunkHeader(_)))
        .map(|(index, _)| index)
        .collect();
    assert_eq!(hunks.len(), 4);
    // Forward through hunks of resident files, then into the non-resident file.
    let mut at = 0;
    for expected in &hunks {
        let target = doc
            .navigate(at, NavKind::Hunk, Direction::Forward)
            .expect("hunk");
        assert_eq!(target.index, *expected);
        assert_eq!(target.needs_load, None);
        at = target.index;
    }
    let target = doc
        .navigate(at, NavKind::Hunk, Direction::Forward)
        .expect("c");
    assert_eq!(target.key, header(&fake, "c.rs"));
    assert_eq!(target.needs_load.as_deref(), Some(&fake.file("c.rs")));
    // From the header of a non-resident file its hunks still lie ahead; past it there are none.
    let again = doc.navigate(target.index, NavKind::Hunk, Direction::Forward);
    assert_eq!(again.map(|again| again.index), Some(target.index));
    assert_eq!(
        doc.navigate(doc.len() - 1, NavKind::Hunk, Direction::Forward),
        None
    );
    // Backward.
    let target = doc
        .navigate(hunks[2], NavKind::Hunk, Direction::Backward)
        .expect("back");
    assert_eq!(target.index, hunks[1]);
    // Files.
    let b = doc
        .navigate(0, NavKind::File, Direction::Forward)
        .expect("b");
    assert_eq!(b.key, header(&fake, "b.rs"));
    let a = doc
        .navigate(b.index, NavKind::File, Direction::Backward)
        .expect("a");
    assert_eq!(a.index, 0);
    // Changes: each removed/added run once. a.rs has a 1/1 run and a removal run.
    let mut changes = Vec::new();
    let mut at = 0;
    while let Some(target) = doc.navigate(at, NavKind::Change, Direction::Forward) {
        if target.needs_load.is_some() {
            break;
        }
        changes.push(
            doc.item(target.index)
                .and_then(ReviewItem::line_numbers)
                .expect("line"),
        );
        at = target.index;
    }
    assert_eq!(
        changes,
        vec![
            (Some(5), None),
            (Some(30), None),
            (Some(5), None),
            (Some(30), None)
        ]
    );
}

#[test]
fn change_motions_work_side_by_side() {
    let fake = fake();
    let mut doc = partially_loaded(&fake);
    doc.set_layout(LayoutMode::SideBySide);
    let target = doc
        .navigate(0, NavKind::Change, Direction::Forward)
        .expect("change");
    let ReviewItem::LinePair(pair) = doc.item(target.index).expect("row") else {
        panic!("row");
    };
    assert!(pair.emphasize);
    assert_eq!(pair.old.as_ref().map(|side| side.line), Some(5));
}

fn draft(thread: &str, path: &str, new_line: u32) -> DraftComment {
    DraftComment {
        comment_id: thread.to_string(),
        thread_id: thread.to_string(),
        anchor: DraftAnchor {
            kind: ReviewAnchorKind::Range,
            file_path: path.to_string(),
            diff_row: 0,
            start_diff_row: None,
            end_diff_row: None,
            old_start: None,
            old_end: None,
            new_start: None,
            new_end: None,
            old_line: None,
            new_line: Some(new_line),
            line_kind: ReviewLineKind::Context,
            is_file_anchor: false,
            surface_id: None,
            source_id: None,
            head_oid: None,
            blob_oid: None,
        },
        body: "body".into(),
        created_at_ms: 1,
        updated_at_ms: 1,
        session_id: None,
        resolved_at_ms: None,
        thread_kind: ReviewThreadKind::Note,
        severity: ReviewThreadSeverity::Info,
    }
}

#[test]
fn thread_and_unviewed_motions_and_jumps() {
    let fake = fake();
    let mut doc = partially_loaded(&fake);
    doc.set_annotations(&Annotations {
        drafts: vec![
            draft("in-a", "a.rs", 7),
            draft("in-b", "b.rs", 3),
            draft("in-c", "c.rs", 4),
        ],
        ..Annotations::default()
    });
    let thread = |id: &str| ItemKey::Thread {
        thread_id: id.to_string(),
    };
    let first = doc
        .navigate(0, NavKind::Thread, Direction::Forward)
        .expect("a");
    assert_eq!(first.key, thread("in-a"));
    let second = doc
        .navigate(first.index, NavKind::Thread, Direction::Forward)
        .expect("b");
    assert_eq!(second.key, thread("in-b"));
    let third = doc
        .navigate(second.index, NavKind::Thread, Direction::Forward)
        .expect("c");
    assert_eq!(third.needs_load.as_deref(), Some(&fake.file("c.rs")));
    // Jumping to a thread in a non-resident file targets its header until it loads.
    let jump = doc.jump_to(&thread("in-c")).expect("jump");
    assert_eq!(jump.key, header(&fake, "c.rs"));
    assert!(jump.needs_load.is_some());
    doc.set_viewport(jump.index, jump.index);
    fake.settle(&mut doc);
    let jump = doc.jump_to(&thread("in-c")).expect("jump");
    assert_eq!(jump.key, thread("in-c"));
    assert_eq!(jump.needs_load, None);

    let mut workspace =
        ReviewWorkspace::from_target(PathBuf::from("/repo"), ReviewTarget::WorkingTreeUnstaged);
    workspace.viewed_files.insert("b.rs".into());
    doc.set_viewed_marks(&workspace);
    let next = doc
        .navigate(0, NavKind::UnviewedFile, Direction::Forward)
        .expect("unviewed");
    assert_eq!(next.key, header(&fake, "c.rs"));
    assert_eq!(next.needs_load, None, "file headers are always resident");
}

#[test]
fn search_reports_matches_in_order_and_unsearched_files() {
    let fake = fake();
    let mut doc = partially_loaded(&fake);
    let results = doc
        .search(&SearchQuery {
            pattern: "OLD 3".into(),
            regex: false,
            case_sensitive: false,
        })
        .expect("search");
    // "old 3" matches resident lines whose text contains it ("a.rs old 3", "a.rs old 30", ...),
    // ordered by position: removed lines follow the context line before them.
    let found: Vec<(String, ReviewLineSide, u32)> = results
        .matches
        .iter()
        .map(|found| (found.file.path.clone(), found.side, found.line))
        .collect();
    assert_eq!(
        found,
        vec![
            ("a.rs".into(), ReviewLineSide::New, 3),
            ("a.rs".into(), ReviewLineSide::Old, 30),
            ("a.rs".into(), ReviewLineSide::Old, 31),
            ("a.rs".into(), ReviewLineSide::New, 30),
            ("a.rs".into(), ReviewLineSide::New, 31),
            ("a.rs".into(), ReviewLineSide::New, 32),
            ("b.rs".into(), ReviewLineSide::New, 3),
            ("b.rs".into(), ReviewLineSide::Old, 30),
            ("b.rs".into(), ReviewLineSide::Old, 31),
            ("b.rs".into(), ReviewLineSide::New, 30),
            ("b.rs".into(), ReviewLineSide::New, 31),
            ("b.rs".into(), ReviewLineSide::New, 32),
        ]
    );
    assert_eq!(results.unsearched, vec![fake.file("c.rs").into()]);
    assert!(
        results.matches[0].ranges == vec![(5, 10)],
        "byte range of the match"
    );
    // Regex, case-sensitive.
    let regex = doc
        .search(&SearchQuery {
            pattern: r"^b\.rs new \d+ \(0\)$".into(),
            regex: true,
            case_sensitive: true,
        })
        .expect("regex");
    assert_eq!(regex.matches.len(), 1);
    assert!(
        doc.search(&SearchQuery {
            pattern: "(".into(),
            regex: true,
            case_sensitive: true,
        })
        .is_err()
    );
    // Expanded context is searched too.
    doc.reveal_lines(&fake.file("a.rs"), 1, 1);
    doc.set_viewport(0, 0);
    fake.settle(&mut doc);
    let expanded = doc
        .search(&SearchQuery {
            pattern: "a.rs old 1".into(),
            regex: false,
            case_sensitive: true,
        })
        .expect("search");
    assert!(expanded.matches.iter().any(|found| found.line == 1));

    // Match navigation in both layouts.
    for layout in [LayoutMode::Unified, LayoutMode::SideBySide] {
        doc.set_layout(layout);
        let first = doc
            .navigate_matches(&results, 0, Direction::Forward)
            .expect("first");
        let second = doc
            .navigate_matches(&results, first.index, Direction::Forward)
            .expect("second");
        assert!(second.index > first.index);
        let back = doc
            .navigate_matches(&results, second.index, Direction::Backward)
            .expect("back");
        assert_eq!(back.index, first.index);
    }
}

#[test]
fn persisted_keys_resolve_in_either_layout() {
    let fake = fake();
    let mut doc = partially_loaded(&fake);
    let line = ItemKey::Line {
        file: fake.file("a.rs").into(),
        side: ReviewLineSide::New,
        line: 5,
    };
    let persisted = line.to_persisted().expect("persisted");
    assert_eq!(doc.resolve_persisted(&persisted), Some(line));
    doc.set_layout(LayoutMode::SideBySide);
    let resolved = doc.resolve_persisted(&persisted).expect("resolved");
    assert!(resolved.covers_line(ReviewLineSide::New, 5));
    assert!(doc.index_of(&resolved).is_some());
    let hunk = items(&doc)
        .into_iter()
        .find(|item| matches!(item, ReviewItem::HunkHeader(_)))
        .expect("hunk")
        .key();
    assert_eq!(
        doc.resolve_persisted(&hunk.to_persisted().expect("persisted")),
        Some(hunk)
    );
}

#[test]
fn motions_into_a_non_resident_file_reach_its_line_threads_after_its_file_threads() {
    let fake = fake();
    // `a.rs` and `c.rs` resident, `b.rs` (between them) not.
    let mut doc = fake.document(DocumentConfig {
        prefetch_rows: 0,
        protected_rows: 0,
        ..DocumentConfig::default()
    });
    for path in ["a.rs", "c.rs"] {
        let at = doc.index_of(&header(&fake, path)).expect("header");
        doc.set_viewport(at, at);
        fake.settle(&mut doc);
    }
    assert!(doc.files[1].loaded().is_none());
    let mut file_level = draft("file-b", "b.rs", 1);
    file_level.anchor.kind = ReviewAnchorKind::File;
    file_level.anchor.new_line = None;
    doc.set_annotations(&Annotations {
        drafts: vec![file_level, draft("line-b", "b.rs", 4)],
        ..Annotations::default()
    });
    let thread = |id: &str| ItemKey::Thread {
        thread_id: id.to_string(),
    };
    let c = doc.index_of(&header(&fake, "c.rs")).expect("c");

    // Backward from `c.rs`: the line thread (inside the unloaded content) comes before the
    // file-level thread, so the motion asks for the file first instead of skipping past it.
    let back = doc
        .navigate(c, NavKind::Thread, Direction::Backward)
        .expect("back");
    assert_eq!(back.key, header(&fake, "b.rs"));
    assert_eq!(back.needs_load.as_deref(), Some(&fake.file("b.rs")));

    // Forward: the visible file-level thread first, then the unloaded content.
    let first = doc
        .navigate(0, NavKind::Thread, Direction::Forward)
        .expect("file thread");
    assert_eq!(
        (first.key.clone(), first.needs_load.clone()),
        (thread("file-b"), None)
    );
    let next = doc
        .navigate(first.index, NavKind::Thread, Direction::Forward)
        .expect("needs load");
    assert_eq!(next.key, header(&fake, "b.rs"));
    assert_eq!(next.needs_load.as_deref(), Some(&fake.file("b.rs")));

    // Load the file and repeat the motions from their original positions.
    doc.set_viewport(next.index, next.index);
    fake.settle(&mut doc);
    let origin = doc.index_of(&first.key).expect("origin");
    let repeated = doc
        .navigate(origin, NavKind::Thread, Direction::Forward)
        .expect("line thread");
    assert_eq!(
        (repeated.key, repeated.needs_load),
        (thread("line-b"), None)
    );
    let c = doc.index_of(&header(&fake, "c.rs")).expect("c");
    let back = doc
        .navigate(c, NavKind::Thread, Direction::Backward)
        .expect("back");
    assert_eq!((back.key, back.needs_load), (thread("line-b"), None));
}
