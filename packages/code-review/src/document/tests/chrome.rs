//! File chrome: details under each header and an end after each file, keys stable when they
//! come and go, sticky sections still starting at the header, and collapsed files.

use super::super::{DocumentConfig, FileChrome, ItemKey, ReviewItem};
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
    // Header, details, end: no content, and nothing was requested for it.
    assert_eq!(
        doc.item(header + 2).map(ReviewItem::key),
        Some(ItemKey::FileEnd(a.clone()))
    );
    assert_eq!(doc.collapsed_files(), vec![a.clone()]);
    assert!(doc.set_collapsed(&a, false));
    fake.load_all(&mut doc);
    assert!(matches!(
        doc.item(header + 2),
        Some(ReviewItem::HunkHeader(_))
    ));
    assert_consistent(&doc, "expanded again");
}
