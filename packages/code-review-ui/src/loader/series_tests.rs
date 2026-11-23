use super::*;
use bcode_code_review_models::{ResolvedSourcePart, ReviewSeriesView};

fn member(index: u32, oid: &str) -> session::OpenedSource {
    session::OpenedSource {
        source: ResolvedSource {
            source_id: "commits".into(),
            base: RevisionRef::EmptyTree,
            head: RevisionRef::commit(oid),
            mode: ResolvedSourceMode::Diff,
            paths: Vec::new(),
            line_range: None,
            part: Some(ResolvedSourcePart {
                index,
                count: 2,
                label: oid.to_owned(),
            }),
            selection: None,
        },
        stack: None,
        entries: Vec::new(),
        untruncated_entries: None,
    }
}

#[test]
fn relisting_or_failing_one_part_does_not_remove_its_siblings() {
    let a = member(0, "a");
    let b = member(1, "b");
    let state = session::CommitSeriesState {
        source_id: "commits".into(),
        members: vec![("a".into(), "first".into()), ("b".into(), "second".into())],
        view: ReviewSeriesView::AllMembers,
        contiguous: false,
        unavailable: None,
    };
    let selector = state.selector(&a.source);
    let mut known = Known::default();
    known.commit_series.insert(state.source_id.clone(), state);
    known.put(b.clone(), false);
    known.put(a.clone(), true);
    known.put(a, false);
    let labels = vec![("commits".into(), "Selected commits".into(), false)];
    let inputs = known.inputs(&labels);
    assert_eq!(inputs.len(), 2);
    assert_eq!(inputs[0].source.head.commit_oid(), Some("a"));
    assert!(!inputs[0].loading);
    known.forget(&selector);
    assert_eq!(known.sources, vec![(b, false)]);
}
