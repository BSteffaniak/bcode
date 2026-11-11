//! Sequence API: splices, key stability, layout switching, residency, and sections.

use super::super::{
    Annotations, DocumentConfig, DocumentNeed, FileLoad, ItemKey, LayoutMode, ReviewDocument,
    ReviewItem,
};
use super::support::{Edit, Fake, Rng, Shadow, SynthFile, items, keys};
use bcode_code_review_models::{
    DraftAnchor, DraftComment, ReviewAnchorKind, ReviewFileExpansion, ReviewLineInterval,
    ReviewLineKind, ReviewLineSide, ReviewTarget, ReviewThreadKind, ReviewThreadSeverity,
    ReviewWorkspace,
};
use std::path::PathBuf;

fn files(count: usize) -> Vec<SynthFile> {
    (0..count)
        .map(|index| {
            let at = 5 + u32::try_from(index % 7).expect("fits") * 3;
            SynthFile::modified(
                &format!("f{index:03}.rs"),
                60,
                &[
                    Edit {
                        at,
                        remove: 2,
                        add: 3,
                    },
                    Edit {
                        at: at + 30,
                        remove: 1,
                        add: 1,
                    },
                ],
                3,
            )
        })
        .collect()
}

fn draft(thread: &str, path: &str, new_line: u32) -> DraftComment {
    DraftComment {
        comment_id: format!("{thread}-c"),
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
fn unloaded_files_are_a_header_and_an_estimated_placeholder() {
    let fake = Fake::new(files(3));
    let doc = fake.document(DocumentConfig::default());
    assert_eq!(doc.len(), 6);
    let ReviewItem::Placeholder(placeholder) = doc.item(1).expect("item") else {
        panic!("placeholder");
    };
    // 7 changed lines: one estimated hunk of header plus six context lines.
    assert_eq!(placeholder.estimated_rows, 7 + 7);
    assert_eq!(doc.estimated_rows(1), 14);
    assert_eq!(doc.estimated_rows(0), 1);
    for (index, key) in keys(&doc).iter().enumerate() {
        assert_eq!(doc.index_of(key), Some(index));
    }
}

#[test]
fn splices_reproduce_the_sequence_after_every_operation() {
    for seed in 0..12 {
        let mut rng = Rng::new(seed);
        let fake = Fake::new(files(12));
        let config = DocumentConfig {
            residency_budget_bytes: if seed % 2 == 0 { 30_000 } else { usize::MAX },
            prefetch_rows: 20,
            protected_rows: 10,
            ..DocumentConfig::default()
        };
        let mut doc = fake.document(config);
        let mut shadow = Shadow::new(&doc);
        let mut workspace =
            ReviewWorkspace::from_target(PathBuf::from("/repo"), ReviewTarget::WorkingTreeUnstaged);
        // Needs left unanswered, answered later (possibly after they became stale).
        let mut held = Vec::new();
        for step in 0..120 {
            let context = format!("seed {seed} step {step}");
            random_operation(&mut doc, &fake, &mut rng, &mut workspace, &mut held);
            shadow.apply_and_check(&mut doc, &context);
        }
    }
}

/// A few random persisted expansions of the test files.
fn random_expansions(rng: &mut Rng) -> Vec<ReviewFileExpansion> {
    (0..rng.below(4))
        .map(|_| {
            let start = rng.range(1, 70);
            ReviewFileExpansion {
                source_id: super::support::SOURCE.to_string(),
                path: format!("f{:03}.rs", rng.below(12)),
                part: None,
                revealed_new_lines: vec![ReviewLineInterval {
                    start,
                    end: start + rng.range(0, 10),
                }],
            }
        })
        .collect::<Vec<_>>()
}

/// Answer some needs, fail a few, and hold others back to answer later.
fn serve_some(doc: &mut ReviewDocument, fake: &Fake, rng: &mut Rng, held: &mut Vec<DocumentNeed>) {
    for need in doc.take_needs() {
        match rng.below(10) {
            0 => {
                doc.fail(need.ticket(), "nope");
            }
            1 => held.push(need),
            _ => {
                let _ = fake.answer(doc, &need);
            }
        }
    }
}

/// Apply one random document operation (see the splice test).
fn random_operation(
    doc: &mut ReviewDocument,
    fake: &Fake,
    rng: &mut Rng,
    workspace: &mut ReviewWorkspace,
    held: &mut Vec<DocumentNeed>,
) {
    let len = doc.len();
    match rng.below(14) {
        0..=2 => {
            let first = usize::try_from(rng.below(len as u64)).expect("fits");
            let span = usize::try_from(rng.below(30)).expect("fits");
            doc.set_viewport(first, first + span);
            serve_some(doc, fake, rng, held);
        }
        3 => {
            let gaps: Vec<ItemKey> = items(doc)
                .iter()
                .filter(|item| matches!(item, ReviewItem::Gap(_)))
                .map(ReviewItem::key)
                .collect();
            if let Some(gap) =
                gaps.get(usize::try_from(rng.below(gaps.len() as u64 + 1)).expect("fits"))
            {
                match rng.below(3) {
                    0 => doc.reveal_above(gap, rng.range(1, 10)),
                    1 => doc.reveal_below(gap, rng.range(1, 10)),
                    _ => doc.reveal_all(gap),
                };
            }
        }
        4 => {
            let file = fake.file(&format!("f{:03}.rs", rng.below(12)));
            doc.reveal_file(&file);
        }
        5 => doc.set_layout(if rng.below(2) == 0 {
            LayoutMode::Unified
        } else {
            LayoutMode::SideBySide
        }),
        6 => {
            let annotations = Annotations {
                drafts: (0..rng.below(5))
                    .map(|index| {
                        draft(
                            &format!("t{index}"),
                            &format!("f{:03}.rs", rng.below(12)),
                            rng.range(1, 70),
                        )
                    })
                    .collect(),
                ..Annotations::default()
            };
            doc.set_annotations(&annotations);
        }
        7 => {
            let lines: Vec<ItemKey> = items(doc)
                .iter()
                .filter(|item| item.line_numbers().is_some())
                .map(ReviewItem::key)
                .collect();
            if let Some(line) =
                lines.get(usize::try_from(rng.below(lines.len() as u64 + 1)).expect("fits"))
            {
                let anchor = doc.anchor_for(line).expect("lines are anchorable");
                doc.open_composer(anchor);
            } else {
                doc.close_composer();
            }
        }
        8 => {
            let path = format!("f{:03}.rs", rng.below(12));
            if !workspace.viewed_files.insert(path.clone()) {
                workspace.viewed_files.remove(&path);
            }
            doc.set_viewed_marks(workspace);
        }
        9 => doc.set_ignore_whitespace(rng.below(2) == 0),
        10 => {
            let file = fake.file(&format!("f{:03}.rs", rng.below(12)));
            doc.retry(&file);
        }
        11 => {
            let file = fake.file(&format!("f{:03}.rs", rng.below(12)));
            let start = rng.range(1, 70);
            doc.reveal_lines(&file, start, start + rng.range(0, 15));
        }
        12 => doc.restore_expansions(&random_expansions(rng)),
        _ => {
            for need in std::mem::take(held) {
                let _ = fake.answer(doc, &need);
            }
        }
    }
}

#[test]
fn keys_are_stable_across_load_evict_and_layout_switches() {
    let fake = Fake::new(files(6));
    let mut doc = fake.document(DocumentConfig::default());
    doc.set_annotations(&Annotations {
        drafts: vec![draft("t1", "f002.rs", 9)],
        ..Annotations::default()
    });
    doc.reveal_lines(&fake.file("f002.rs"), 1, 3);
    fake.load_all(&mut doc);
    let loaded = keys(&doc);

    // Evict everything outside a tiny window and reload: identical keys and items.
    let small = ReviewDocument {
        config: DocumentConfig {
            residency_budget_bytes: 0,
            prefetch_rows: 0,
            protected_rows: 0,
            ..DocumentConfig::default()
        },
        ..doc.clone()
    };
    let mut doc = small;
    doc.set_viewport(0, 0);
    let resident = doc.resident_files().count();
    assert_eq!(resident, 1, "only the viewport file stays resident");
    assert_eq!(
        doc.load_state(&fake.file("f003.rs")),
        Some(FileLoad::Unloaded)
    );
    doc.config.residency_budget_bytes = usize::MAX;
    fake.load_all(&mut doc);
    assert_eq!(keys(&doc), loaded, "reloading restores the same keys");

    // Mode switch: every unified line key resolves to the row containing it and back.
    let lines: Vec<ItemKey> = loaded
        .iter()
        .filter(|key| matches!(key, ItemKey::Line { .. }))
        .cloned()
        .collect();
    doc.set_layout(LayoutMode::SideBySide);
    for key in &lines {
        let ItemKey::Line { side, line, .. } = key else {
            unreachable!()
        };
        let index = doc.index_of(key).expect("line maps to a row");
        let row = doc.item(index).expect("row").key();
        assert!(row.covers_line(*side, *line), "{key:?} -> {row:?}");
        // And back: the row maps to a unified line in the same row.
        doc.set_layout(LayoutMode::Unified);
        let back = doc.index_of(&row).expect("row maps to a line");
        let ItemKey::Line {
            side: back_side,
            line: back_line,
            ..
        } = doc.item(back).expect("line").key()
        else {
            panic!("unified line");
        };
        assert!(row.covers_line(back_side, back_line));
        doc.set_layout(LayoutMode::SideBySide);
    }
    // Non-line keys are identical in both layouts.
    let non_lines = |keys: Vec<ItemKey>| -> Vec<ItemKey> {
        keys.into_iter()
            .filter(|key| !matches!(key, ItemKey::Line { .. } | ItemKey::LinePair { .. }))
            .collect()
    };
    let split = non_lines(keys(&doc));
    doc.set_layout(LayoutMode::Unified);
    assert_eq!(non_lines(keys(&doc)), split);
    assert_eq!(keys(&doc), loaded);
}

#[test]
fn side_by_side_pairs_runs_positionally_with_an_emphasis_hint() {
    let fake = Fake::new(vec![SynthFile::modified(
        "p.rs",
        20,
        &[Edit {
            at: 5,
            remove: 3,
            add: 1,
        }],
        1,
    )]);
    let mut doc = fake.document(DocumentConfig::default());
    doc.set_layout(LayoutMode::SideBySide);
    fake.load_all(&mut doc);
    let rows: Vec<(Option<u32>, Option<u32>, bool)> = items(&doc)
        .iter()
        .filter_map(|item| match item {
            ReviewItem::LinePair(pair) => Some((
                pair.old.as_ref().map(|side| side.line),
                pair.new.as_ref().map(|side| side.line),
                pair.emphasize,
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            (Some(4), Some(4), false),
            (Some(5), Some(5), true),
            (Some(6), None, false),
            (Some(7), None, false),
            (Some(8), Some(6), false),
        ]
    );
}

#[test]
fn residency_respects_the_budget_and_never_evicts_the_window() {
    let fake = Fake::new(files(40));
    let config = DocumentConfig {
        residency_budget_bytes: 60_000,
        prefetch_rows: 40,
        protected_rows: 15,
        ..DocumentConfig::default()
    };
    let mut doc = fake.document(config);
    let mut first = 0;
    let mut evictions = 0;
    while first < doc.len() {
        let before: Vec<_> = doc.resident_files().cloned().collect();
        doc.set_viewport(first, first + 20);
        fake.settle(&mut doc);
        let window = doc.window.expect("window");
        for block in window.protected.0..=window.protected.1 {
            if let super::super::document::BlockKind::File(file) = doc.blocks[block].kind {
                assert!(
                    doc.files[file].loaded().is_some(),
                    "protected file {} is resident",
                    doc.files[file].key.path
                );
            }
        }
        // Over the budget, everything resident is inside the prefetch window (which is never
        // evicted and stops growing once the budget is reached).
        if doc.resident_bytes() > config.residency_budget_bytes {
            for file in &doc.files {
                if file.loaded().is_some() {
                    assert!(
                        file.block >= window.prefetch.0 && file.block <= window.prefetch.1,
                        "{} is resident outside the window while over budget",
                        file.key.path
                    );
                }
            }
        }
        let total: usize = doc.files.iter().map(|file| file.resident_bytes).sum();
        assert_eq!(total, doc.resident_bytes(), "byte accounting is exact");
        evictions += before
            .iter()
            .filter(|file| doc.load_state(file) == Some(FileLoad::Unloaded))
            .count();
        first += 15;
    }
    assert!(
        evictions > 0,
        "scrolling the whole review evicts older files"
    );
}

#[test]
fn sections_report_file_and_hunk_headers() {
    let fake = Fake::new(files(2));
    let mut doc = fake.document(DocumentConfig::default());
    doc.reveal_lines(&fake.file("f000.rs"), 1, 2);
    fake.load_all(&mut doc);
    let all = items(&doc);
    let mut file_header = None;
    let mut hunk_header = None;
    for (index, item) in all.iter().enumerate() {
        match item {
            ReviewItem::FileHeader(_) => {
                file_header = Some(index);
                hunk_header = None;
            }
            ReviewItem::HunkHeader(_) => hunk_header = Some(index),
            ReviewItem::Gap(_) => hunk_header = None,
            ReviewItem::Line(line) if line.kind == super::super::LineKind::Expanded => {
                hunk_header = None;
            }
            _ => {}
        }
        let sections = doc.sections(index);
        assert_eq!(sections.file_header, file_header, "item {index}: {item:?}");
        assert_eq!(sections.hunk_header, hunk_header, "item {index}: {item:?}");
        if let Some(level) = item.header_level() {
            assert_eq!(level, u8::from(matches!(item, ReviewItem::HunkHeader(_))));
        }
    }
}

#[test]
fn stale_results_are_ignored() {
    let fake = Fake::new(files(2));
    let mut doc = fake.document(DocumentConfig::default());
    doc.set_viewport(0, 0);
    let needs = doc.take_needs();
    assert_ne!(needs, Vec::new());
    // Toggling whitespace re-requests everything; the old tickets are stale.
    doc.set_ignore_whitespace(true);
    let file = &fake.files[&needs[0].file().path];
    assert!(!doc.accept_hunks(needs[0].ticket(), &file.hunks_response("source-1")));
    assert!(!doc.fail(needs[0].ticket(), "late"));
    let line = ItemKey::Line {
        file: needs[0].file().clone(),
        side: ReviewLineSide::New,
        line: 1,
    };
    assert_eq!(doc.index_of(&line), None);
}
