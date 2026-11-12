//! Publish planning: routing threads to pull requests, whole-stack attribution mapping, and
//! bundle threads read from hunks.

use super::*;
use bcode_code_review_models::{
    ResolvedStackLayer, ReviewHunk, ReviewLine, ReviewStackLineAttribution, ReviewThreadKind,
    ReviewThreadSeverity,
};

const PROVIDER: &str = "forge";

fn source(id: &str, kind: ReviewSourceKind) -> ReviewSource {
    ReviewSource {
        id: id.into(),
        label: kind.label(),
        kind,
        included: true,
    }
}

fn resolved(id: &str) -> ResolvedSource {
    ResolvedSource {
        source_id: id.into(),
        base: RevisionRef::commit("b".repeat(40)),
        head: RevisionRef::commit("h".repeat(40)),
        mode: bcode_code_review_models::ResolvedSourceMode::Diff,
        paths: Vec::new(),
        line_range: None,
        part: None,
    }
}

fn stack(id: &str) -> ResolvedReviewStack {
    ResolvedReviewStack {
        source_id: id.into(),
        trunk: "main".into(),
        trunk_oid: "0".repeat(40),
        base_oid: "0".repeat(40),
        layers: [101_u64, 102, 103]
            .into_iter()
            .map(|number| ResolvedStackLayer {
                number,
                title: format!("Layer {number}"),
                base_oid: format!("{number}").repeat(14)[..40].to_string(),
                head_oid: format!("{}", number + 1).repeat(14)[..40].to_string(),
            })
            .collect(),
    }
}

/// Old-side and new-side line ranges of an anchor.
type Sides = (Option<(u32, u32)>, Option<(u32, u32)>);

/// Each bundle's pull request and its threads (id, summary only).
type Summary = Vec<(Option<u64>, Vec<(String, bool)>)>;

fn anchor(
    source: Option<&str>,
    kind: ReviewAnchorKind,
    lines: Sides,
    line_kind: ReviewLineKind,
) -> DraftAnchor {
    let (old, new) = lines;
    DraftAnchor {
        kind,
        file_path: if kind == ReviewAnchorKind::Review {
            "<review>".into()
        } else {
            "src/lib.rs".into()
        },
        diff_row: 0,
        start_diff_row: None,
        end_diff_row: None,
        old_start: old.map(|(start, _)| start),
        old_end: old.map(|(_, end)| end),
        new_start: new.map(|(start, _)| start),
        new_end: new.map(|(_, end)| end),
        old_line: old.map(|(_, end)| end),
        new_line: new.map(|(_, end)| end),
        line_kind,
        is_file_anchor: false,
        surface_id: None,
        source_id: source.map(str::to_string),
        head_oid: None,
        blob_oid: None,
    }
}

fn thread(id: &str, anchor: DraftAnchor) -> PublishThread {
    PublishThread {
        thread_id: id.into(),
        comments: vec![DraftComment {
            comment_id: format!("{id}-c"),
            thread_id: id.into(),
            anchor,
            body: format!("body {id}"),
            created_at_ms: 1,
            updated_at_ms: 1,
            session_id: None,
            resolved_at_ms: None,
            thread_kind: ReviewThreadKind::Note,
            severity: ReviewThreadSeverity::Info,
        }],
    }
}

fn summary(bundles: &[PlannedBundle]) -> Summary {
    bundles
        .iter()
        .map(|bundle| {
            (
                bundle.target.as_ref().map(|target| target.number),
                bundle
                    .threads
                    .iter()
                    .map(|thread| (thread.thread.thread_id.clone(), thread.summary_only))
                    .collect(),
            )
        })
        .collect()
}

#[test]
fn pull_request_and_layer_sources_publish_to_their_own_pull_request() {
    let sources = [
        source(
            "s1",
            ReviewSourceKind::StackLayer {
                provider: PROVIDER.into(),
                repository: "o/r".into(),
                number: 101,
            },
        ),
        source(
            "s2",
            ReviewSourceKind::StackLayer {
                provider: PROVIDER.into(),
                repository: "o/r".into(),
                number: 102,
            },
        ),
        source("local", ReviewSourceKind::LastCommit),
    ];
    let resolved = [resolved("s1"), resolved("s2"), resolved("local")];
    let stack = stack("s1");
    let plan_sources: Vec<PlanSource<'_>> = sources
        .iter()
        .zip(&resolved)
        .map(|(source, resolved)| PlanSource {
            source,
            resolved: Some(resolved),
            stack: Some(&stack),
        })
        .collect();
    let added = |source| {
        anchor(
            Some(source),
            ReviewAnchorKind::Range,
            (None, Some((3, 3))),
            ReviewLineKind::Added,
        )
    };
    let threads = vec![
        thread("t1", added("s1")),
        thread("t2", added("s2")),
        thread(
            "t3",
            anchor(
                Some("s2"),
                ReviewAnchorKind::File,
                (None, None),
                ReviewLineKind::Context,
            ),
        ),
        thread("t4", added("local")),
        thread(
            "t5",
            anchor(
                None,
                ReviewAnchorKind::Review,
                (None, None),
                ReviewLineKind::Context,
            ),
        ),
    ];
    assert_eq!(
        attribution_needs(&threads, &plan_sources, Some(PROVIDER)),
        Vec::<(String, String)>::new()
    );
    let bundles = plan(
        threads.clone(),
        &plan_sources,
        Some(PROVIDER),
        &BTreeMap::new(),
    );
    assert_eq!(
        summary(&bundles),
        vec![
            (Some(101), vec![("t1".into(), false)]),
            // File comments and review comments go into the summary; review comments go to the
            // last pull request source.
            (
                Some(102),
                vec![
                    ("t2".into(), false),
                    ("t3".into(), true),
                    ("t5".into(), true)
                ]
            ),
            (None, vec![("t4".into(), false)]),
        ]
    );
    assert_eq!(bundles[0].label, "#101 Layer 101");
    assert_eq!(bundles[0].source_id.as_deref(), Some("s1"));
    assert_eq!(bundles[0].threads[0].diff.as_ref().unwrap().source_id, "s1");
    assert_eq!(bundles[0].target.as_ref().unwrap().repository, "o/r");

    // A publisher of another provider (or a built-in one) gets one bundle.
    let bundles = plan(threads, &plan_sources, None, &BTreeMap::new());
    assert_eq!(bundles.len(), 1);
    assert_eq!(bundles[0].target, None);
    assert_eq!(bundles[0].threads.len(), 5);
}

/// Attribution of `src/lib.rs`: top lines 10-12 were written by #102 (its lines 4-6), line 20
/// by #103 (its line 20), and base line 7 was removed by #101 (line 7 of the merge base).
fn attribution() -> ReviewStackAttributionResponse {
    let range = |start, end, layer, layer_line| ReviewStackLineAttribution {
        start,
        end,
        layer,
        layer_line: Some(layer_line),
    };
    ReviewStackAttributionResponse {
        path: "src/lib.rs".into(),
        new_ranges: vec![range(10, 12, 102, 4), range(20, 20, 103, 20)],
        old_ranges: vec![range(7, 7, 101, 7)],
        binary: false,
    }
}

#[test]
fn whole_stack_comments_publish_to_the_layer_that_introduced_their_line() {
    let whole = source(
        "stack",
        ReviewSourceKind::Stack {
            provider: PROVIDER.into(),
            repository: "o/r".into(),
            top: 103,
        },
    );
    let resolved = resolved("stack");
    let stack = stack("stack");
    let sources = [PlanSource {
        source: &whole,
        resolved: Some(&resolved),
        stack: Some(&stack),
    }];
    let attribution = attribution();
    let new = |start, end, kind| {
        anchor(
            Some("stack"),
            ReviewAnchorKind::Range,
            (None, Some((start, end))),
            kind,
        )
    };
    let threads = vec![
        thread("added", new(11, 12, ReviewLineKind::Added)),
        thread("top", new(20, 20, ReviewLineKind::Added)),
        thread(
            "removed",
            anchor(
                Some("stack"),
                ReviewAnchorKind::Range,
                (Some((7, 7)), None),
                ReviewLineKind::Removed,
            ),
        ),
        // A context line no layer wrote: the top pull request's summary.
        thread("context", new(15, 15, ReviewLineKind::Context)),
        // A range across two layers narrows to its last line's layer.
        thread("across", new(12, 20, ReviewLineKind::Added)),
        thread(
            "review",
            anchor(
                None,
                ReviewAnchorKind::Review,
                (None, None),
                ReviewLineKind::Context,
            ),
        ),
    ];
    assert_eq!(
        attribution_needs(&threads, &sources, Some(PROVIDER)),
        vec![("stack".to_string(), "src/lib.rs".to_string())]
    );
    let attributions =
        BTreeMap::from([(("stack".to_string(), "src/lib.rs".to_string()), attribution)]);
    let bundles = plan(threads, &sources, Some(PROVIDER), &attributions);
    assert_eq!(
        summary(&bundles),
        vec![
            (Some(101), vec![("removed".into(), false)]),
            (Some(102), vec![("added".into(), false)]),
            (
                Some(103),
                vec![
                    ("top".into(), false),
                    ("context".into(), true),
                    ("across".into(), false),
                    ("review".into(), true),
                ]
            ),
        ]
    );
    let added = &bundles[1].threads[0];
    assert_eq!(
        (
            added.anchor.new_start,
            added.anchor.new_end,
            added.anchor.new_line
        ),
        (Some(5), Some(6), Some(6))
    );
    // The layer's own diff, not the whole-stack diff.
    let diff = added.diff.as_ref().unwrap();
    assert_eq!(diff.base, RevisionRef::commit(&stack.layers[1].base_oid));
    assert_eq!(diff.head, RevisionRef::commit(&stack.layers[1].head_oid));
    // The persisted anchor is kept on the thread for the publish record.
    assert_eq!(added.thread.anchor().unwrap().new_line, Some(12));
    let across = &bundles[2].threads[2];
    assert_eq!(
        (across.anchor.new_start, across.anchor.new_line),
        (Some(20), Some(20))
    );
    // Without an attribution every line comment falls back to the top summary.
    let bundles = plan(
        vec![thread("added", new(11, 12, ReviewLineKind::Added))],
        &sources,
        Some(PROVIDER),
        &BTreeMap::new(),
    );
    assert_eq!(
        summary(&bundles),
        vec![(Some(103), vec![("added".into(), true)])]
    );
}

#[test]
fn bundle_threads_carry_the_selected_lines_of_the_target_diff() {
    let line = |kind, old_line, new_line, content: &str| ReviewLine {
        kind,
        old_line,
        new_line,
        content: content.into(),
    };
    let hunks = ReviewFileHunksResponse {
        source_id: "s".into(),
        path: "src/lib.rs".into(),
        hunks: vec![ReviewHunk {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 3,
            heading: Some("fn main".into()),
            lines: vec![
                line(ReviewLineKind::Context, Some(1), Some(1), "a"),
                line(ReviewLineKind::Removed, Some(2), None, "b"),
                line(ReviewLineKind::Added, None, Some(2), "B"),
                line(ReviewLineKind::Added, None, Some(3), "C"),
            ],
        }],
        binary: false,
        truncated: None,
        patch_id: None,
    };
    let planned = PlannedThread {
        thread: thread(
            "t",
            anchor(
                Some("s"),
                ReviewAnchorKind::Range,
                (None, Some((2, 3))),
                ReviewLineKind::Added,
            ),
        ),
        anchor: anchor(
            Some("s"),
            ReviewAnchorKind::Range,
            (None, Some((2, 3))),
            ReviewLineKind::Added,
        ),
        summary_only: false,
        diff: None,
    };
    let bundle = bundle_thread(&planned, Some(&hunks));
    assert_eq!(bundle.selected_diff_lines, vec!["+B", "+C"]);
    assert_eq!(bundle.selected_lines[1].new_line, Some(3));
    assert_eq!(bundle.selected_lines[1].source_id.as_deref(), Some("s"));
    assert_eq!(bundle.hunk_context[0], "@@ -1,3 +1,3 @@ fn main");
    assert_eq!(bundle.hunk_context.len(), 5);
    assert!(!bundle.summary_only && bundle.resolved_at_ms.is_none());
    // A line outside every hunk (expanded context) selects nothing; the publisher decides.
    let mut outside = planned;
    outside.anchor = anchor(
        Some("s"),
        ReviewAnchorKind::Range,
        (None, Some((40, 40))),
        ReviewLineKind::Context,
    );
    assert_eq!(
        bundle_thread(&outside, Some(&hunks)).selected_lines,
        Vec::new()
    );
}
