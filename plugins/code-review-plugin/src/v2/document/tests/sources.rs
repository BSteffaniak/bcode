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
            },
            ResolvedStackLayer {
                number: 12,
                title: "second".into(),
                base_oid: "h1".into(),
                head_oid: "h2".into(),
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
                },
                ReviewStackLineAttribution {
                    start: 6,
                    end: 6,
                    layer: 12,
                },
            ],
            old_ranges: vec![ReviewStackLineAttribution {
                start: 30,
                end: 30,
                layer: 12,
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
