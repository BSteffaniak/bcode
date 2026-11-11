//! Context expansion (spec 5.2): gap math, interval-set reveals, order independence, clamping,
//! and per-status behavior.

use super::super::{
    DocumentConfig, DocumentNeed, FileNoticeKind, GapItem, GapState, ItemKey, LayoutMode,
    LineContent, LineKind, ReviewDocument, ReviewItem,
};
use super::assert_lines_match;
use super::support::{Edit, Fake, Rng, SOURCE, SynthFile, items, keys};
use bcode_code_review_models::{
    ReviewBlobSelector, ReviewFileStatus, ReviewHunksTruncation, ReviewLineSide,
};

fn gaps(doc: &ReviewDocument) -> Vec<GapItem> {
    items(doc)
        .into_iter()
        .filter_map(|item| match item {
            ReviewItem::Gap(gap) => Some(gap),
            _ => None,
        })
        .collect()
}

fn gap_key(gap: &GapItem) -> ItemKey {
    ItemKey::Gap {
        file: gap.file.clone(),
        start: gap.start,
    }
}

fn sample() -> SynthFile {
    SynthFile::modified(
        "a.rs",
        100,
        &[
            Edit {
                at: 20,
                remove: 2,
                add: 4,
            },
            Edit {
                at: 60,
                remove: 1,
                add: 0,
            },
            Edit {
                at: 90,
                remove: 0,
                add: 2,
            },
        ],
        3,
    )
}

#[test]
fn gaps_carry_both_line_numbers_and_close_after_the_line_count_arrives() {
    let fake = Fake::new(vec![sample()]);
    let mut doc = fake.document(DocumentConfig::default());
    doc.set_viewport(0, 0);
    let needs = doc.take_needs();
    assert!(matches!(needs.as_slice(), [DocumentNeed::Hunks { .. }]));
    assert!(fake.answer(&mut doc, &needs[0]));
    // Before the line count is known the trailing gap is open-ended.
    let open = gaps(&doc);
    assert_eq!(
        open.last().map(|gap| gap.state.clone()),
        Some(GapState::OpenEnded)
    );
    // The document asks for the line count (a zero-line probe of the new blob).
    let needs = doc.take_needs();
    let [DocumentNeed::BlobLines { request, .. }] = needs.as_slice() else {
        panic!("expected a line-count probe, got {needs:?}");
    };
    assert_eq!(request.count, 0);
    assert_eq!(
        request.blob,
        ReviewBlobSelector::Object {
            oid: "new:a.rs".into()
        }
    );
    assert!(fake.answer(&mut doc, &needs[0]));
    assert_eq!(doc.take_needs(), Vec::new());

    let gaps = gaps(&doc);
    let ranges: Vec<_> = gaps
        .iter()
        .map(|gap| (gap.new_range(), gap.old_range()))
        .collect();
    // Hunks: old 17..=24 → new 17..=26 (+2), old 57..=63 → new 59..=64 (-1), old 87..=92 →
    // new 88..=95 (+2). The new side has 103 lines.
    assert_eq!(
        ranges,
        vec![
            (Some((1, 16)), Some((1, 16))),
            (Some((27, 58)), Some((25, 56))),
            (Some((65, 87)), Some((64, 86))),
            (Some((96, 103)), Some((93, 100))),
        ]
    );
    assert!(gaps.iter().all(|gap| gap.state == GapState::Hidden));
    assert_lines_match(&doc, &fake);
}

#[test]
fn revealing_everything_shows_every_line_once_with_correct_text() {
    for layout in [LayoutMode::Unified, LayoutMode::SideBySide] {
        let fake = Fake::new(vec![sample()]);
        let mut doc = fake.document(DocumentConfig::default());
        doc.set_layout(layout);
        assert!(
            doc.reveal_file(&fake.file("a.rs")),
            "reveal works before load"
        );
        fake.load_all(&mut doc);
        assert_eq!(gaps(&doc), Vec::new());
        let (mut old, mut new) = (Vec::new(), Vec::new());
        for item in items(&doc) {
            if let Some((o, n)) = item.line_numbers() {
                old.extend(o);
                new.extend(n);
            }
            if let ReviewItem::Line(line) = &item {
                assert!(!matches!(line.content, LineContent::Loading));
            }
        }
        assert_eq!(old, (1..=100).collect::<Vec<_>>(), "{layout:?}");
        assert_eq!(new, (1..=103).collect::<Vec<_>>(), "{layout:?}");
        assert_lines_match(&doc, &fake);
        assert!(!doc.reveal_file(&fake.file("a.rs")), "idempotent");
    }
}

#[test]
fn reveal_above_below_and_all_act_on_the_right_end_of_a_gap() {
    let fake = Fake::new(vec![sample()]);
    let mut doc = fake.document(DocumentConfig::default());
    fake.load_all(&mut doc);
    let middle = gaps(&doc)[1].clone();
    assert_eq!(middle.new_range(), Some((27, 58)));
    assert!(doc.reveal_below(&gap_key(&middle), 5));
    let shrunk = ItemKey::Gap {
        file: middle.file,
        start: 32,
    };
    assert!(doc.reveal_above(&shrunk, 4));
    fake.settle(&mut doc);
    let middle = gaps(&doc)[1].clone();
    assert_eq!(middle.new_range(), Some((32, 54)));
    assert_eq!(middle.old_range(), Some((30, 52)));
    let expanded: Vec<(Option<u32>, Option<u32>)> = items(&doc)
        .iter()
        .filter_map(|item| match item {
            ReviewItem::Line(line) if line.kind == LineKind::Expanded => {
                Some((line.old_line, line.new_line))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        expanded,
        [25, 26, 27, 28, 29, 53, 54, 55, 56]
            .into_iter()
            .map(|old| (Some(old), Some(old + 2)))
            .collect::<Vec<_>>()
    );
    assert_lines_match(&doc, &fake);
    // A stale gap key (its start is now revealed) does nothing.
    assert!(!doc.reveal_all(&ItemKey::Gap {
        file: middle.file.clone(),
        start: 27
    }));
    assert!(doc.reveal_all(&gap_key(&middle)));
    assert!(!doc.reveal_all(&gap_key(&middle)), "idempotent");
    assert_eq!(gaps(&doc).len(), 3);
    // Counts larger than the gap are clamped.
    let first = gaps(&doc)[0].clone();
    assert!(doc.reveal_above(&gap_key(&first), 1_000));
    assert_eq!(gaps(&doc).len(), 2);
    // Expanded lines are anchorable.
    let key = ItemKey::Line {
        file: first.file,
        side: ReviewLineSide::New,
        line: 3,
    };
    assert!(doc.anchor_for(&key).is_some());
}

/// A random file and a configuration whose hunk context matches the file's diff.
fn random_file(rng: &mut Rng) -> (SynthFile, DocumentConfig) {
    let lines = rng.range(30, 300);
    let mut edits = Vec::new();
    let mut at = rng.range(1, 30);
    while at <= lines && edits.len() < 6 {
        let remove = rng.range(0, 3).min(lines + 1 - at);
        let add = if remove == 0 {
            rng.range(1, 3)
        } else {
            rng.range(0, 3)
        };
        edits.push(Edit { at, remove, add });
        at += remove + rng.range(1, 60);
    }
    let context = rng.range(0, 4);
    let config = DocumentConfig {
        context_lines: context,
        ..DocumentConfig::default()
    };
    (SynthFile::modified("r.rs", lines, &edits, context), config)
}

#[test]
fn explicit_reveals_are_order_independent_and_survive_loading_at_any_time() {
    for seed in 0..60 {
        let mut rng = Rng::new(seed);
        let (file, config) = random_file(&mut rng);
        let new_len = u32::try_from(file.new.as_ref().expect("new").len()).expect("fits");
        let fake = Fake::new(vec![file]);
        let key = fake.file("r.rs");
        let mut ranges: Vec<(u32, u32)> = (0..rng.range(1, 12))
            .map(|_| {
                let start = rng.range(1, new_len + 5);
                (start, start + rng.range(0, 25))
            })
            .collect();

        let mut before = fake.document(config);
        for (start, end) in &ranges {
            before.reveal_lines(&key, *start, *end);
        }
        fake.load_all(&mut before);

        let mut after = fake.document(config);
        fake.load_all(&mut after);
        rng.shuffle(&mut ranges);
        for (start, end) in &ranges {
            after.reveal_lines(&key, *start, *end);
            // Idempotent: repeating a reveal changes nothing.
            assert!(!after.reveal_lines(&key, *start, *end), "seed {seed}");
        }
        fake.settle(&mut after);
        assert_eq!(items(&before), items(&after), "seed {seed}");
        assert_eq!(before.expansions(), after.expansions(), "seed {seed}");
        assert_lines_match(&after, &fake);

        // Restoring persisted expansions reproduces the document.
        let mut restored = fake.document(config);
        restored.restore_expansions(&before.expansions());
        fake.load_all(&mut restored);
        assert_eq!(items(&before), items(&restored), "seed {seed}");
    }
}

#[derive(Clone, Copy, Debug)]
enum GapOp {
    Above(usize, u32),
    Below(usize, u32),
    All(usize),
}

/// Apply `op` to the slot's gap (slots are the stretches between hunk headers).
fn apply(doc: &mut ReviewDocument, op: GapOp) {
    let mut slot = 0;
    let mut by_slot: Vec<(usize, ItemKey)> = Vec::new();
    for item in items(doc) {
        match &item {
            ReviewItem::HunkHeader(_) => slot += 1,
            ReviewItem::Gap(_) => by_slot.push((slot, item.key())),
            _ => {}
        }
    }
    let find = |target: usize| {
        by_slot
            .iter()
            .find(|(s, _)| *s == target)
            .map(|(_, k)| k.clone())
    };
    match op {
        GapOp::Above(slot, count) => {
            if let Some(key) = find(slot) {
                doc.reveal_above(&key, count);
            }
        }
        GapOp::Below(slot, count) => {
            if let Some(key) = find(slot) {
                doc.reveal_below(&key, count);
            }
        }
        GapOp::All(slot) => {
            if let Some(key) = find(slot) {
                doc.reveal_all(&key);
            }
        }
    }
}

#[test]
fn gap_operations_commute_in_any_order() {
    for seed in 0..60 {
        let mut rng = Rng::new(1_000 + seed);
        let (file, config) = random_file(&mut rng);
        let fake = Fake::new(vec![file]);
        let mut ops: Vec<GapOp> = (0..rng.range(1, 14))
            .map(|_| {
                let slot = usize::try_from(rng.below(7)).expect("fits");
                match rng.below(9) {
                    0 => GapOp::All(slot),
                    1..=4 => GapOp::Above(slot, rng.range(1, 30)),
                    _ => GapOp::Below(slot, rng.range(1, 30)),
                }
            })
            .collect();
        let run = |ops: &[GapOp]| {
            let mut doc = fake.document(config);
            fake.load_all(&mut doc);
            for op in ops {
                apply(&mut doc, *op);
                fake.settle(&mut doc);
            }
            doc
        };
        let forward = run(&ops);
        rng.shuffle(&mut ops);
        let shuffled = run(&ops);
        assert_eq!(items(&forward), items(&shuffled), "seed {seed}: {ops:?}");
        assert_lines_match(&shuffled, &fake);
    }
}

#[test]
fn revealed_context_is_clamped_when_hunks_change_and_kept_when_they_do_not() {
    let fake = Fake::new(vec![sample()]);
    let key = fake.file("a.rs");
    let mut doc = fake.document(DocumentConfig::default());
    fake.load_all(&mut doc);
    doc.reveal_lines(&key, 1, 40);
    fake.settle(&mut doc);
    let revealed = doc.revealed(&key).cloned().expect("file");
    // Re-requesting identical hunks keeps the expansion.
    doc.set_ignore_whitespace(true);
    fake.load_all(&mut doc);
    assert_eq!(doc.revealed(&key), Some(&revealed));
    // New hunks covering previously revealed lines clamp the set to the new gaps.
    let changed = SynthFile::modified(
        "a.rs",
        100,
        &[Edit {
            at: 5,
            remove: 1,
            add: 1,
        }],
        3,
    );
    let fake = Fake::new(vec![changed]);
    doc.set_ignore_whitespace(false);
    fake.load_all(&mut doc);
    let after: Vec<(u32, u32)> = doc.revealed(&key).expect("file").iter().collect();
    // Lines that were inside the old hunks were never revealed context.
    assert_eq!(after, vec![(1, 1), (9, 16), (27, 40)]);
    assert_lines_match(&doc, &fake);
    let all = keys(&doc);
    let mut sorted = all.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), all.len(), "keys stay unique");
}

#[test]
fn added_deleted_renamed_and_binary_files() {
    let mut renamed = SynthFile::modified(
        "new/name.rs",
        30,
        &[Edit {
            at: 10,
            remove: 1,
            add: 1,
        }],
        3,
    );
    renamed.old_path = Some("old/name.rs".into());
    renamed.status = ReviewFileStatus::Renamed;
    let mut pure_rename = SynthFile::modified("moved.rs", 12, &[], 3);
    pure_rename.old_path = Some("orig.rs".into());
    pure_rename.status = ReviewFileStatus::Renamed;
    let fake = Fake::new(vec![
        SynthFile::added("added.rs", 7),
        SynthFile::deleted("deleted.rs", 9),
        renamed,
        pure_rename,
        SynthFile::binary("blob.bin"),
    ]);
    let mut doc = fake.document(DocumentConfig::default());
    doc.set_viewport(0, doc.len() - 1);
    let needs = doc.take_needs();
    let renamed_request = needs
        .iter()
        .find_map(|need| match need {
            DocumentNeed::Hunks { request, .. } if request.path == "new/name.rs" => Some(request),
            _ => None,
        })
        .expect("hunk request");
    assert_eq!(renamed_request.old_path.as_deref(), Some("old/name.rs"));
    for need in &needs {
        assert!(fake.answer(&mut doc, need));
    }
    fake.load_all(&mut doc);
    assert_lines_match(&doc, &fake);

    let file_items = |doc: &ReviewDocument, path: &str| -> Vec<ReviewItem> {
        items(doc)
            .into_iter()
            .filter(|item| item.file().is_some_and(|file| file.path == path))
            .collect()
    };
    // Added and deleted files are one whole-file hunk: no gaps, no blob reads.
    for path in ["added.rs", "deleted.rs"] {
        assert!(
            !file_items(&doc, path)
                .iter()
                .any(|item| matches!(item, ReviewItem::Gap(_)))
        );
        assert!(!doc.reveal_file(&fake.file(path)));
    }
    let deleted_keys: Vec<ItemKey> = file_items(&doc, "deleted.rs")
        .iter()
        .filter(|item| matches!(item, ReviewItem::Line(_)))
        .map(ReviewItem::key)
        .collect();
    assert_eq!(deleted_keys.len(), 9);
    assert!(deleted_keys.iter().all(|key| matches!(
        key,
        ItemKey::Line {
            side: ReviewLineSide::Old,
            ..
        }
    )));
    // A pure rename has no text changes but its content can still be expanded.
    let moved = file_items(&doc, "moved.rs");
    assert!(moved.iter().any(|item| matches!(
        item,
        ReviewItem::FileNotice(notice) if notice.kind == FileNoticeKind::NoTextChanges
    )));
    let ReviewItem::Gap(gap) = moved.last().expect("gap") else {
        panic!("pure rename shows its content as a gap");
    };
    assert_eq!(gap.new_range(), Some((1, 12)));
    assert_eq!(gap.old_range(), Some((1, 12)));
    // Binary files show a notice and request nothing.
    assert!(file_items(&doc, "blob.bin").iter().any(|item| matches!(
        item,
        ReviewItem::FileNotice(notice) if notice.kind == FileNoticeKind::Binary
    )));
    assert!(!doc.reveal_file(&fake.file("blob.bin")));
    assert_eq!(doc.take_needs(), Vec::new());
}

#[test]
fn deleted_file_context_reads_the_old_blob() {
    // A deleted file whose diff only covers part of the file (as a truncated or hand-crafted
    // response could) exposes old-side gaps that must be read from the old blob.
    let mut deleted = SynthFile::deleted("gone.rs", 30);
    let hunk = &mut deleted.hunks[0];
    hunk.lines
        .retain(|line| (10..=20).contains(&line.old_line.unwrap_or(0)));
    hunk.old_start = 10;
    hunk.old_count = 11;
    let fake = Fake::new(vec![deleted]);
    let mut doc = fake.document(DocumentConfig::default());
    fake.load_all(&mut doc);
    let gap = gaps(&doc)[0].clone();
    assert_eq!(gap.side, ReviewLineSide::Old);
    assert_eq!(gap.old_range(), Some((1, 9)));
    assert_eq!(gap.new_range(), None);
    assert!(doc.reveal_all(&gap_key(&gap)));
    let needs = doc.take_needs();
    let [DocumentNeed::BlobLines { request, .. }] = needs.as_slice() else {
        panic!("expected one blob read, got {needs:?}");
    };
    assert_eq!(
        request.blob,
        ReviewBlobSelector::Object {
            oid: "old:gone.rs".into()
        }
    );
    assert_eq!((request.start_line, request.count), (1, 9));
    assert!(fake.answer(&mut doc, &needs[0]));
    assert_lines_match(&doc, &fake);
    let expanded = items(&doc)
        .into_iter()
        .filter(|item| matches!(item, ReviewItem::Line(line) if line.kind == LineKind::Expanded))
        .count();
    assert_eq!(expanded, 9);
}

#[test]
fn context_reads_show_loading_and_unavailable_states_and_truncation_has_no_trailing_gap() {
    let mut big = sample();
    big.truncated = Some(ReviewHunksTruncation {
        shown_lines: 10,
        total_lines: 40,
        shown_hunks: 2,
        total_hunks: 3,
    });
    big.hunks.truncate(2);
    let fake = Fake::new(vec![big]);
    let key = fake.file("a.rs");
    let mut doc = fake.document(DocumentConfig::default());
    fake.load_all(&mut doc);
    let all = items(&doc);
    assert!(matches!(
        all.last(),
        Some(ReviewItem::FileNotice(notice)) if notice.kind == FileNoticeKind::TooLarge
    ));
    assert_eq!(gaps(&doc).len(), 2, "no gap after the last returned hunk");

    doc.reveal_lines(&key, 1, 4);
    let needs = doc.take_needs();
    assert_eq!(needs.len(), 1);
    let loading = items(&doc)
        .into_iter()
        .filter(
            |item| matches!(item, ReviewItem::Line(line) if line.content == LineContent::Loading),
        )
        .count();
    assert_eq!(loading, 4);
    assert!(doc.fail(needs[0].ticket(), "disk on fire"));
    assert!(
        !doc.fail(needs[0].ticket(), "again"),
        "tickets resolve once"
    );
    let unavailable = items(&doc)
        .into_iter()
        .filter(|item| {
            matches!(item, ReviewItem::Line(line) if matches!(&line.content, LineContent::Unavailable(reason) if &**reason == "disk on fire"))
        })
        .count();
    assert_eq!(unavailable, 4);
    assert!(
        doc.take_needs().is_empty(),
        "failed ranges are not retried automatically"
    );
    assert_eq!(SOURCE, key.source_id);
}
