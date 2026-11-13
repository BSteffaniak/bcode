//! File chrome: details under each header and an end after each file, keys stable when they
//! come and go, sticky sections still starting at the header, and collapsed files.

use super::super::{DocumentConfig, FileChrome, FileFold, ItemKey, ReviewItem};
use super::support::{Fake, SynthFile, assert_consistent, keys};

fn fake() -> Fake {
    Fake::new(vec![
        SynthFile::added("a.rs", 12),
        SynthFile::added("b.rs", 8),
    ])
}

#[test]
fn details_and_ends_wrap_each_file_and_keep_headers_first() {
    let fake = fake();
    let mut doc = fake.document(DocumentConfig::default());
    fake.load_all(&mut doc);
    let plain = keys(&doc);
    doc.set_file_chrome(FileChrome {
        details: true,
        ends: true,
    });
    let _ = doc.take_splices();
    assert_consistent(&doc, "with chrome");
    let a = std::sync::Arc::new(fake.file("a.rs"));
    let header = doc
        .index_of(&ItemKey::FileHeader(a.clone()))
        .expect("header");
    assert!(matches!(
        doc.item(header + 1),
        Some(ReviewItem::FileDetails(_))
    ));
    // The end is the file's last item, before the next header.
    let b = std::sync::Arc::new(fake.file("b.rs"));
    let next = doc.index_of(&ItemKey::FileHeader(b)).expect("next header");
    assert_eq!(
        doc.item(next - 1).map(ReviewItem::key),
        Some(ItemKey::FileEnd(a))
    );
    // Sections: the details and the end belong to the file, whose header starts it.
    assert_eq!(doc.sections(header + 1).file_header, Some(header));
    assert_eq!(doc.sections(next - 1).file_header, Some(header));
    // The details know what is loaded.
    let Some(ReviewItem::FileDetails(details)) = doc.item(header + 1) else {
        panic!("details");
    };
    assert_eq!(details.hunks, Some(1));
    // Without chrome, the document is as before.
    doc.set_file_chrome(FileChrome::default());
    let _ = doc.take_splices();
    assert_eq!(keys(&doc), plain);
}

#[test]
fn a_collapsed_file_is_its_header_and_loads_nothing() {
    let fake = fake();
    let mut doc = fake.document(DocumentConfig::default());
    doc.set_file_chrome(FileChrome {
        details: true,
        ends: true,
    });
    let a = fake.file("a.rs");
    assert!(doc.set_collapsed(&a, true));
    assert!(!doc.set_collapsed(&a, true));
    fake.load_all(&mut doc);
    let a = std::sync::Arc::new(a);
    let header = doc
        .index_of(&ItemKey::FileHeader(a.clone()))
        .expect("header");
    let Some(ReviewItem::FileHeader(item)) = doc.item(header) else {
        panic!("header");
    };
    assert!(item.collapsed);
    // Header and end: no details, no content, and nothing was requested for it.
    assert_eq!(
        doc.item(header + 1).map(ReviewItem::key),
        Some(ItemKey::FileEnd(a.clone()))
    );
    assert_eq!(doc.file_folds(), vec![(a.clone(), FileFold::Folded)]);
    assert!(doc.set_collapsed(&a, false));
    fake.load_all(&mut doc);
    assert!(matches!(
        doc.item(header + 2),
        Some(ReviewItem::HunkHeader(_))
    ));
    assert_consistent(&doc, "expanded again");
}

#[test]
fn a_source_introduction_comes_before_its_files_and_goes_away_again() {
    let fake = fake();
    let mut doc = fake.document(DocumentConfig::default());
    let plain = keys(&doc);
    let source = super::support::SOURCE.to_owned();
    assert!(doc.set_intros(std::collections::BTreeSet::from([source.clone()])));
    assert!(!doc.set_intros(std::collections::BTreeSet::from([source.clone()])));
    let _ = doc.take_splices();
    assert_consistent(&doc, "with an introduction");
    let intro = ItemKey::SourceIntro {
        source_id: source.clone(),
    };
    let index = doc.index_of(&intro).expect("introduction");
    let a = std::sync::Arc::new(fake.file("a.rs"));
    assert!(index < doc.index_of(&ItemKey::FileHeader(a)).expect("first file"));
    // A single source shows no header, but its introduction still comes first.
    assert_eq!(index, 0);
    assert!(matches!(
        doc.item(index),
        Some(ReviewItem::SourceIntro(item)) if !item.under_header && item.source_id == source
    ));
    assert!(doc.set_intros(std::collections::BTreeSet::new()));
    let _ = doc.take_splices();
    assert_eq!(keys(&doc), plain);
}

fn view(doc: &mut super::super::ReviewDocument, paths: &[&str]) {
    let mut workspace = bcode_code_review_models::ReviewWorkspace::from_target(
        std::path::PathBuf::from("/repo"),
        bcode_code_review_models::ReviewTarget::WorkingTreeUnstaged,
    );
    workspace
        .viewed_files
        .extend(paths.iter().map(|path| (*path).to_owned()));
    doc.set_viewed_marks(&workspace);
}

fn end_of(
    doc: &super::super::ReviewDocument,
    file: &super::super::FileRef,
) -> super::super::FileEndItem {
    let index = doc.index_of(&ItemKey::FileEnd(file.clone())).expect("end");
    let Some(ReviewItem::FileEnd(end)) = doc.item(index) else {
        panic!("end");
    };
    end.clone()
}

#[test]
fn viewed_files_fold_by_themselves_and_a_hand_fold_wins_until_marked_again() {
    let fake = fake();
    let mut doc = fake.document(DocumentConfig {
        fold_viewed: true,
        ..DocumentConfig::default()
    });
    doc.set_file_chrome(FileChrome {
        details: true,
        ends: true,
    });
    fake.load_all(&mut doc);
    let (a, b) = (
        std::sync::Arc::new(fake.file("a.rs")),
        std::sync::Arc::new(fake.file("b.rs")),
    );
    assert!(!doc.is_collapsed(&a));
    // Viewed folds; the fold stays automatic.
    view(&mut doc, &["a.rs"]);
    assert!(doc.is_collapsed(&a));
    assert_eq!(doc.file_fold(&a), Some(FileFold::Auto));
    assert_eq!(doc.file_folds(), Vec::new());
    // Opened by hand while viewed: open, and still viewed.
    assert!(doc.set_collapsed(&a, false));
    assert!(!doc.is_collapsed(&a));
    assert!(
        doc.viewed_state(&a)
            .is_some_and(crate::viewed::ViewedState::is_viewed)
    );
    // Unviewed (and the fold back to automatic): open; viewed again: folded.
    view(&mut doc, &[]);
    doc.set_fold(&a, FileFold::Auto);
    assert!(!doc.is_collapsed(&a));
    view(&mut doc, &["a.rs"]);
    assert!(doc.is_collapsed(&a));
    // Folding by hand needs no viewed mark, and does not make one.
    assert!(doc.set_collapsed(&b, true));
    assert!(
        !doc.viewed_state(&b)
            .is_some_and(crate::viewed::ViewedState::is_viewed)
    );
    // Without the setting, viewed files stay open.
    doc.set_fold_viewed(false);
    assert!(!doc.is_collapsed(&a));
    assert!(doc.is_collapsed(&b));
    let _ = doc.take_splices();
    assert_consistent(&doc, "folds");
}

#[test]
fn the_break_between_two_folded_files_is_tight_and_names_the_next_file() {
    let fake = fake();
    let mut doc = fake.document(DocumentConfig::default());
    doc.set_file_chrome(FileChrome {
        details: false,
        ends: true,
    });
    fake.load_all(&mut doc);
    let (a, b) = (
        std::sync::Arc::new(fake.file("a.rs")),
        std::sync::Arc::new(fake.file("b.rs")),
    );
    let end = end_of(&doc, &a);
    assert_eq!(
        (end.next_position, end.count, end.tight),
        (Some(2), 2, false)
    );
    assert_eq!(end_of(&doc, &b).next_position, None);
    doc.set_collapsed(&a, true);
    assert!(!end_of(&doc, &a).tight);
    // Folding the next file rebuilds this file's break.
    doc.set_collapsed(&b, true);
    assert!(end_of(&doc, &a).tight);
    let _ = doc.take_splices();
    assert_consistent(&doc, "tight");
}

#[test]
fn motions_into_a_folded_file_stop_at_its_header_to_open_it() {
    use super::super::{Direction, NavKind};
    let fake = fake();
    let mut doc = fake.document(DocumentConfig::default());
    doc.set_file_chrome(FileChrome {
        details: true,
        ends: true,
    });
    fake.load_all(&mut doc);
    let b = std::sync::Arc::new(fake.file("b.rs"));
    let b_hunk = doc
        .navigate(
            doc.index_of(&ItemKey::FileHeader(b.clone())).expect("b"),
            NavKind::Hunk,
            Direction::Forward,
        )
        .expect("b's hunk")
        .key;
    doc.set_collapsed(&b, true);
    let header = doc.index_of(&ItemKey::FileHeader(b.clone())).expect("b");
    let end = doc.index_of(&ItemKey::FileEnd(b.clone())).expect("b's end");
    // Forward from before the file, and from its header: its hidden hunk.
    for from in [header - 1, header] {
        let target = doc
            .navigate(from, NavKind::Hunk, Direction::Forward)
            .expect("target");
        assert_eq!((target.index, target.opens.as_ref()), (header, Some(&b)));
    }
    // Backward from its end: the same; from its header: not this file.
    let target = doc
        .navigate(end, NavKind::Hunk, Direction::Backward)
        .expect("target");
    assert_eq!(target.opens.as_ref(), Some(&b));
    let target = doc
        .navigate(header, NavKind::Hunk, Direction::Backward)
        .expect("a's hunk");
    assert_eq!(target.opens, None);
    // File motions stop at the header without opening it.
    let target = doc
        .navigate(0, NavKind::File, Direction::Forward)
        .expect("b");
    assert_eq!((target.index, target.opens), (header, None));
    // A jump to a key inside it opens it.
    let jump = doc.jump_to(&b_hunk).expect("jump");
    assert_eq!((jump.index, jump.opens.as_ref()), (header, Some(&b)));
}
