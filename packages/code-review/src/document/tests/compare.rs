//! Comparison overlays end to end: a layer pushed twice, compared with the rebase taken out, on
//! the same document in both views.

use super::super::{
    AnchorPlacement, CompareView, DocumentConfig, DocumentNeed, GroupComparison, ItemKey,
    LayoutMode, NavKind, ReviewDocument, ReviewItem, SourceInput,
};
use super::super::{Annotations, Direction};
use crate::ReviewData;
use bcode_code_review_models::{
    DeltaKind, DraftComment, FileSince, LayerDelta, LayerPair, ResolveReviewSourcesRequest,
    ResolvedSource, ReviewCompareLayersRequest, ReviewIndexRequest, ReviewLineSide, ReviewSource,
    ReviewSourceKind, ReviewTarget, ReviewThreadKind, ReviewThreadSeverity, ReviewWorkspace,
    VersionPoint,
};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Command;

const SOURCE: &str = "pr";

struct Repo {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Repo {
    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .env("GIT_AUTHOR_DATE", "@1700000000 +0000")
            .env("GIT_COMMITTER_DATE", "@1700000000 +0000")
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn commit(&self, files: &[(&str, &str)], message: &str) -> String {
        for (path, text) in files {
            std::fs::write(self.root.join(path), text).expect("write");
            self.git(&["add", path]);
        }
        self.git(&["commit", "-q", "-m", message]);
        self.git(&["rev-parse", "HEAD"])
    }
}

fn numbered(prefix: &str, count: u32) -> String {
    (1..=count).fold(String::new(), |mut text, line| {
        let _ = writeln!(text, "{prefix}{line}");
        text
    })
}

/// A layer pushed twice on the same fork point: `a.txt` edited again, `b.txt` and `c.txt` the
/// same, `d.txt` new in the second version.
struct Layer {
    repo: Repo,
    fork: String,
    first: String,
    second: String,
}

fn layer() -> Layer {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = Repo {
        root: dir.path().to_path_buf(),
        _dir: dir,
    };
    repo.git(&["init", "-q", "-b", "main"]);
    let a = numbered("a", 30);
    let fork = repo.commit(
        &[
            ("a.txt", &a),
            ("b.txt", &numbered("b", 10)),
            ("c.txt", &numbered("c", 10)),
        ],
        "fork",
    );
    let a_first = a.replace("a10\n", "a10\nmine\n");
    let b_first = numbered("b", 10).replace("b5\n", "B5\n");
    let c_first = numbered("c", 10).replace("c2\n", "C2\n");
    let first = repo.commit(
        &[
            ("a.txt", &a_first),
            ("b.txt", &b_first),
            ("c.txt", &c_first),
        ],
        "first",
    );
    // The second version drops `mine`, rewrites a20, and adds d.txt.
    let a_second = a.replace("a20\n", "a20 again\n");
    repo.git(&["reset", "-q", "--hard", &fork]);
    let second = repo.commit(
        &[
            ("a.txt", &a_second),
            ("b.txt", &b_first),
            ("c.txt", &c_first),
            ("d.txt", "new\n"),
        ],
        "second",
    );
    Layer {
        repo,
        fork,
        first,
        second,
    }
}

fn open(service: &ReviewData, layer: &Layer) -> ReviewDocument {
    open_at(service, layer, &layer.second)
}

fn open_at(service: &ReviewData, layer: &Layer, head: &str) -> ReviewDocument {
    let repo = layer.repo.root.as_path();
    let resolved: Vec<ResolvedSource> = service
        .resolve_sources(&ResolveReviewSourcesRequest {
            repo_path: repo.to_path_buf(),
            sources: vec![ReviewSource {
                id: SOURCE.into(),
                label: "PR".into(),
                kind: ReviewSourceKind::CommitRange {
                    base: layer.fork.clone(),
                    head: head.to_owned(),
                    merge_base: false,
                },
                included: true,
            }],
            provider_resolutions: Vec::new(),
        })
        .expect("resolve")
        .sources;
    let inputs: Vec<SourceInput> = resolved
        .into_iter()
        .map(|source| SourceInput {
            entries: service
                .index_get(&ReviewIndexRequest {
                    repo_path: repo.to_path_buf(),
                    source: source.clone(),
                    cursor: None,
                    limit: None,
                })
                .expect("index")
                .entries,
            label: source.source_id.clone(),
            source,
            stack: None,
            loading: false,
        })
        .collect();
    ReviewDocument::new(repo.to_path_buf(), inputs, DocumentConfig::default())
}

fn delta(service: &ReviewData, layer: &Layer) -> LayerDelta {
    delta_between(service, layer, &layer.first, &layer.second)
}

fn delta_between(service: &ReviewData, layer: &Layer, from: &str, to: &str) -> LayerDelta {
    let point = |head: &str| VersionPoint {
        head_oid: head.to_owned(),
        fork_oid: Some(layer.fork.clone()),
    };
    service
        .compare_layers(&ReviewCompareLayersRequest {
            repo_path: layer.repo.root.clone(),
            pairs: vec![LayerPair {
                number: 7,
                from: Some(point(from)),
                to: point(to),
            }],
        })
        .expect("compare")
        .deltas
        .remove(0)
}

fn serve(service: &ReviewData, doc: &mut ReviewDocument) {
    for _ in 0..200 {
        let len = doc.len();
        doc.set_viewport(0, len.saturating_sub(1));
        let needs = doc.take_needs();
        if needs.is_empty() {
            return;
        }
        for need in needs {
            let ticket = need.ticket();
            match &need {
                DocumentNeed::Hunks { request, .. } => {
                    let response = service.file_hunks_get(request).expect("hunks");
                    doc.accept_hunks(ticket, &response);
                }
                DocumentNeed::BlobLines { request, .. } => {
                    let response = service.blob_lines_get(request).expect("lines");
                    doc.accept_blob_lines(ticket, &response);
                }
                DocumentNeed::Attribution { .. } => panic!("no stack here"),
                DocumentNeed::Marks { request, .. } => {
                    let response = service.compare_marks(request).expect("marks");
                    doc.accept_marks(ticket, response);
                }
            }
        }
    }
    panic!("document never settled");
}

fn items(doc: &ReviewDocument) -> Vec<ReviewItem> {
    (0..doc.len())
        .map(|index| doc.item(index).expect("item").clone())
        .collect()
}

fn file(path: &str) -> super::super::FileKey {
    super::super::FileKey::new(SOURCE, None, path)
}

/// Shown files, with whether each is folded.
fn shown(doc: &ReviewDocument) -> Vec<(String, bool)> {
    items(doc)
        .iter()
        .filter_map(|item| match item {
            ReviewItem::FileHeader(header) => Some((header.file.path.clone(), header.collapsed)),
            _ => None,
        })
        .collect()
}

/// New-side lines of `path` shown, with whether each is marked new since the baseline.
fn new_lines(doc: &ReviewDocument, path: &str) -> Vec<(u32, bool, String)> {
    items(doc)
        .iter()
        .filter_map(|item| match item {
            ReviewItem::Line(line) if line.file.path == path => {
                Some((line.new_line?, line.since, line.content.text()?.to_owned()))
            }
            _ => None,
        })
        .collect()
}

fn draft(thread: &str, anchor: &super::super::DocAnchor) -> DraftComment {
    DraftComment {
        comment_id: format!("{thread}-1"),
        thread_id: thread.into(),
        anchor: anchor.to_draft_anchor(),
        body: "check".into(),
        created_at_ms: 1,
        updated_at_ms: 1,
        session_id: None,
        resolved_at_ms: None,
        thread_kind: ReviewThreadKind::Note,
        severity: ReviewThreadSeverity::Info,
    }
}

/// The text of `path`'s lines that exist only on the old side.
fn old_only(doc: &ReviewDocument, path: &str) -> Vec<String> {
    items(doc)
        .iter()
        .filter_map(|item| match item {
            ReviewItem::Line(line) if line.file.path == path && line.new_line.is_none() => {
                line.content.text().map(str::to_owned)
            }
            _ => None,
        })
        .collect()
}

fn mark_viewed(doc: &mut ReviewDocument, workspace: &mut ReviewWorkspace, path: &str) {
    let (identity, patch_id) = doc.viewed_identity(&file(path)).expect("identity");
    crate::viewed::mark_viewed(workspace, &identity.with_patch(patch_id.as_deref()));
    workspace.viewed_files.insert(path.to_owned());
    doc.set_viewed_marks(workspace);
}

#[test]
fn the_full_view_marks_what_is_new_and_folds_what_was_viewed_and_is_unchanged() {
    let layer = layer();
    let service = ReviewData::default();
    let mut doc = open(&service, &layer);
    serve(&service, &mut doc);
    let mut workspace =
        ReviewWorkspace::from_target(layer.repo.root.clone(), ReviewTarget::WorkingTreeUnstaged);
    mark_viewed(&mut doc, &mut workspace, "b.txt");
    let delta = delta(&service, &layer);
    assert_eq!(delta.kind, DeltaKind::Changed);
    assert!(doc.set_comparison(
        SOURCE,
        Some(GroupComparison {
            delta: Some(delta),
            view: CompareView::Full,
            ..GroupComparison::default()
        }),
    ));
    serve(&service, &mut doc);
    assert_eq!(
        doc.file_since(&file("a.txt")),
        Some(FileSince::Changed {
            added: 1,
            removed: 2
        })
    );
    assert_eq!(doc.file_since(&file("b.txt")), Some(FileSince::Unchanged));
    assert!(matches!(
        doc.file_since(&file("d.txt")),
        Some(FileSince::New { .. })
    ));
    // Unchanged and viewed folds; unchanged but never viewed stays open.
    assert_eq!(
        shown(&doc),
        [
            ("a.txt".to_owned(), false),
            ("b.txt".to_owned(), true),
            ("c.txt".to_owned(), false),
            ("d.txt".to_owned(), false),
        ]
    );
    // The rewritten line is marked; lines the baseline also had are not.
    let marked: Vec<(u32, String)> = new_lines(&doc, "a.txt")
        .into_iter()
        .filter(|(_, since, _)| *since)
        .map(|(line, _, text)| (line, text))
        .collect();
    assert_eq!(marked, [(20, "a20 again".to_owned())]);
    // The baseline's own line, gone now, is a marker where it was.
    let removals: Vec<(u32, Vec<String>)> = items(&doc)
        .iter()
        .filter_map(|item| match item {
            ReviewItem::RemovedSince(removal) => {
                Some((removal.before_new_line, removal.lines.to_vec()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(removals, [(11, vec!["mine".to_owned()])]);
    assert!(doc.toggle_removal(&file("a.txt"), 11));
    assert!(
        items(&doc)
            .iter()
            .any(|item| matches!(item, ReviewItem::RemovedSince(removal) if removal.expanded))
    );
    // `]` stops on the marked line first.
    let target = doc
        .navigate(0, NavKind::NewSince, Direction::Forward)
        .expect("a marked line");
    assert!(target.key.covers_line(ReviewLineSide::Old, 20));
    // The summary under the header counts what changed.
    let note = items(&doc)
        .into_iter()
        .find_map(|item| match item {
            ReviewItem::CompareNote(note) => Some(note),
            _ => None,
        })
        .expect("a comparison note");
    assert_eq!((note.changed_files, note.unchanged_files), (2, 2));
    assert_eq!(note.kind, Some(DeltaKind::Changed));
}

#[test]
fn switching_views_keeps_keys_viewed_progress_threads_and_the_cursor_line() {
    let layer = layer();
    let service = ReviewData::default();
    let mut doc = open(&service, &layer);
    serve(&service, &mut doc);
    let mut workspace =
        ReviewWorkspace::from_target(layer.repo.root.clone(), ReviewTarget::WorkingTreeUnstaged);
    mark_viewed(&mut doc, &mut workspace, "c.txt");
    // A draft on the rewritten line, made in the full change.
    let line = ItemKey::Line {
        file: doc.file_keys().next().expect("a file").clone(),
        side: ReviewLineSide::New,
        line: 20,
    };
    let anchor = doc.anchor_for(&line).expect("anchor");
    doc.set_annotations(&Annotations {
        drafts: vec![draft("t1", &anchor)],
        ..Annotations::default()
    });
    let keys: Vec<_> = doc.file_keys().cloned().collect();
    let progress = doc.viewed_progress();
    let thread = ItemKey::Thread {
        thread_id: "t1".into(),
    };
    let placement = |doc: &ReviewDocument| {
        let index = doc.index_of(&thread).expect("thread shown");
        let ReviewItem::Thread(item) = doc.item(index).expect("item") else {
            panic!("not a thread");
        };
        let before = doc.item(index - 1).expect("line").key();
        (item.placement, before)
    };
    let delta = delta(&service, &layer);
    doc.set_comparison(
        SOURCE,
        Some(GroupComparison {
            delta: Some(delta),
            view: CompareView::Full,
            ..GroupComparison::default()
        }),
    );
    serve(&service, &mut doc);
    let (full_placement, full_after) = placement(&doc);
    assert_eq!(full_placement, AnchorPlacement::Line);

    assert!(doc.set_compare_view(CompareView::Changes));
    serve(&service, &mut doc);
    assert_eq!(doc.file_keys().cloned().collect::<Vec<_>>(), keys);
    assert_eq!(doc.viewed_progress(), progress);
    assert!(doc.file_shows_changes(&file("a.txt")));
    // Only changed files show; unchanged ones are behind the summary.
    assert_eq!(
        shown(&doc),
        [("a.txt".to_owned(), false), ("d.txt".to_owned(), false)]
    );
    // The changes view diffs from the baseline replayed: `mine` is a removed line now, and the
    // first version's other lines are context.
    assert_eq!(old_only(&doc, "a.txt"), ["mine", "a20"]);
    // The thread and the cursor's line keep their place.
    assert_eq!(placement(&doc), (full_placement, full_after.clone()));
    assert!(doc.index_of(&line).is_some());
    // A before-side line takes no comment; a current line anchors as in the full change.
    let mine = items(&doc)
        .iter()
        .find_map(|item| match item {
            ReviewItem::Line(item) if item.content.text() == Some("mine") => {
                Some(ReviewItem::Line(item.clone()).key())
            }
            _ => None,
        })
        .expect("mine");
    assert_eq!(doc.anchor_for(&mine), None);
    let again = doc.anchor_for(&line).expect("anchor");
    assert_eq!(again.blob_oid, anchor.blob_oid);
    let super::super::AnchorTarget::Lines(lines) = again.target else {
        panic!("a line anchor");
    };
    assert_eq!((lines.old, lines.new), (None, Some((20, 20))));

    // Unchanged files can be shown, folded; viewed progress never moved.
    assert!(doc.set_compare_files_shown(SOURCE, true));
    assert_eq!(
        shown(&doc),
        [
            ("a.txt".to_owned(), false),
            ("b.txt".to_owned(), true),
            ("c.txt".to_owned(), true),
            ("d.txt".to_owned(), false),
        ]
    );
    // Back to the full change: same keys, the thread where it was.
    assert!(doc.set_compare_view(CompareView::Full));
    serve(&service, &mut doc);
    assert!(!doc.file_shows_changes(&file("a.txt")));
    assert_eq!(placement(&doc), (full_placement, full_after));
    assert_eq!(doc.viewed_progress(), progress);
    crate::testing::document::assert_consistent(&doc, "after switching back");
    doc.set_layout(LayoutMode::SideBySide);
    serve(&service, &mut doc);
    assert!(items(&doc).iter().any(ReviewItem::is_since));
}

#[test]
fn a_layer_only_rebased_collapses_to_its_summary_until_its_files_are_shown() {
    let layer = layer();
    let service = ReviewData::default();
    let mut doc = open(&service, &layer);
    let rebased = LayerDelta {
        pair: LayerPair {
            number: 7,
            from: Some(VersionPoint {
                head_oid: layer.first.clone(),
                fork_oid: None,
            }),
            to: VersionPoint {
                head_oid: layer.second.clone(),
                fork_oid: Some(layer.fork),
            },
        },
        kind: DeltaKind::RebasedOnly,
        replay_oid: None,
        files: Vec::new(),
    };
    doc.set_comparison(
        SOURCE,
        Some(GroupComparison {
            delta: Some(rebased),
            view: CompareView::Full,
            ..GroupComparison::default()
        }),
    );
    assert_eq!(shown(&doc), []);
    assert!(
        items(&doc)
            .iter()
            .any(|item| matches!(item, ReviewItem::CompareNote(note)
                if note.kind == Some(DeltaKind::RebasedOnly)))
    );
    doc.set_compare_files_shown(SOURCE, true);
    assert_eq!(shown(&doc).len(), 4);
    assert!(shown(&doc).iter().all(|(_, folded)| *folded));
    // Turning the comparison off restores the review exactly.
    doc.set_comparison(SOURCE, None);
    assert!(shown(&doc).iter().all(|(_, folded)| !*folded));
    assert!(
        !items(&doc)
            .iter()
            .any(|item| matches!(item, ReviewItem::CompareNote(_)))
    );
}

/// A third version on the same fork point: the second with `a25` rewritten.
fn third(layer: &Layer) -> String {
    let a = numbered("a", 30)
        .replace("a20\n", "a20 again\n")
        .replace("a25\n", "a25 third\n");
    layer.repo.commit(&[("a.txt", &a)], "third")
}

/// Every line text of `path` shown.
fn texts(doc: &ReviewDocument, path: &str) -> Vec<String> {
    items(doc)
        .iter()
        .filter_map(|item| match item {
            ReviewItem::Line(line) if line.file.path == path => {
                line.content.text().map(str::to_owned)
            }
            _ => None,
        })
        .collect()
}

#[test]
fn a_historical_comparison_shows_an_older_version_read_only_and_restores_exactly() {
    let layer = layer();
    let third = third(&layer);
    let service = ReviewData::default();
    let mut doc = open_at(&service, &layer, &third);
    serve(&service, &mut doc);
    // A draft on a current line of a.txt.
    let line = ItemKey::Line {
        file: file("a.txt").into(),
        side: ReviewLineSide::New,
        line: 25,
    };
    let anchor = doc.anchor_for(&line).expect("anchor");
    doc.set_annotations(&Annotations {
        drafts: vec![draft("t1", &anchor)],
        ..Annotations::default()
    });
    let keys: Vec<_> = doc.file_keys().cloned().collect();
    // From the first version to the second, while the review shows the third.
    doc.set_comparison(
        SOURCE,
        Some(GroupComparison {
            delta: Some(delta(&service, &layer)),
            view: CompareView::Full,
            historical: true,
            ..GroupComparison::default()
        }),
    );
    serve(&service, &mut doc);
    assert!(doc.file_is_historical(&file("a.txt")));
    assert!(doc.file_shows_changes(&file("a.txt")));
    // The second version's lines, not the third's.
    let shown_text = texts(&doc, "a.txt");
    assert!(
        shown_text.iter().any(|text| text == "a20 again"),
        "{shown_text:?}"
    );
    assert!(
        !shown_text.iter().any(|text| text == "a25 third"),
        "{shown_text:?}"
    );
    // Unchanged between the two versions: behind the summary.
    assert_eq!(
        shown(&doc),
        [("a.txt".to_owned(), false), ("d.txt".to_owned(), false)]
    );
    // The draft is on the current code: it shows at its file, not on an older line.
    let index = doc
        .index_of(&ItemKey::Thread {
            thread_id: "t1".into(),
        })
        .expect("thread shown");
    let ReviewItem::Thread(thread) = doc.item(index).expect("item") else {
        panic!("not a thread");
    };
    assert_eq!(thread.placement, AnchorPlacement::File);
    // Off again: the third version, every file, the draft on its line.
    doc.set_comparison(SOURCE, None);
    serve(&service, &mut doc);
    assert!(!doc.file_is_historical(&file("a.txt")));
    assert!(texts(&doc, "a.txt").iter().any(|text| text == "a25 third"));
    assert_eq!(doc.file_keys().cloned().collect::<Vec<_>>(), keys);
    let index = doc
        .index_of(&ItemKey::Thread {
            thread_id: "t1".into(),
        })
        .expect("thread shown");
    let ReviewItem::Thread(thread) = doc.item(index).expect("item") else {
        panic!("not a thread");
    };
    assert_eq!(thread.placement, AnchorPlacement::Line);
    crate::testing::document::assert_consistent(&doc, "after a historical comparison");
}

#[test]
fn stepping_one_file_to_other_versions_overrides_only_that_file() {
    let layer = layer();
    let third = third(&layer);
    let service = ReviewData::default();
    let mut doc = open_at(&service, &layer, &third);
    serve(&service, &mut doc);
    doc.set_comparison(
        SOURCE,
        Some(GroupComparison {
            delta: Some(delta_between(&service, &layer, &layer.second, &third)),
            view: CompareView::Changes,
            ..GroupComparison::default()
        }),
    );
    serve(&service, &mut doc);
    // Second to third: only a.txt changed, by a25.
    assert_eq!(shown(&doc), [("a.txt".to_owned(), false)]);
    assert!(texts(&doc, "a.txt").iter().any(|text| text == "a25 third"));
    // a.txt stepped back to the first pair: the second version's change, read-only.
    assert!(doc.set_file_comparison(&file("a.txt"), Some(delta(&service, &layer))));
    serve(&service, &mut doc);
    assert!(doc.file_override(&file("a.txt")).is_some());
    assert!(doc.file_is_historical(&file("a.txt")));
    let stepped = texts(&doc, "a.txt");
    assert!(
        stepped.iter().any(|text| text == "a20 again"),
        "{stepped:?}"
    );
    assert!(
        !stepped.iter().any(|text| text == "a25 third"),
        "{stepped:?}"
    );
    assert!(!doc.file_is_historical(&file("b.txt")));
    // Cleared: back to the source's comparison.
    assert!(doc.set_file_comparison(&file("a.txt"), None));
    serve(&service, &mut doc);
    assert!(!doc.file_is_historical(&file("a.txt")));
    assert!(texts(&doc, "a.txt").iter().any(|text| text == "a25 third"));
    crate::testing::document::assert_consistent(&doc, "after stepping a file");
}

/// The versions file `path`'s header shows.
fn header_versions(doc: &ReviewDocument, path: &str) -> Option<super::super::FileVersionsItem> {
    (0..doc.len()).find_map(|index| match doc.item(index) {
        Some(ReviewItem::FileHeader(header)) if header.file.path == path => header.versions,
        _ => None,
    })
}

#[test]
fn version_numbers_stay_truthful_when_a_version_between_others_is_unavailable() {
    use super::super::{ComparisonSummary, VersionStep};
    let layer = layer();
    let third = third(&layer);
    let service = ReviewData::default();
    let mut doc = open_at(&service, &layer, &third);
    serve(&service, &mut doc);
    let step = delta_between(&service, &layer, &layer.second, &third);
    // Versions 1 to 4 of which 2 is unavailable: the steps are 1 → (none), 3 → 4.
    doc.set_comparison(
        SOURCE,
        Some(GroupComparison {
            delta: Some(step.clone()),
            view: CompareView::Full,
            steps: vec![VersionStep {
                from: 3,
                to: 4,
                delta: step.clone(),
            }],
            current_version: 4,
            ..GroupComparison::default()
        }),
    );
    serve(&service, &mut doc);
    let versions = header_versions(&doc, "a.txt").expect("a.txt's versions");
    assert_eq!(
        (versions.version, versions.steps, versions.changed_in),
        (4, 1, 1)
    );
    // Stepped to the only step: still version 4 (the step's newer version).
    assert!(doc.set_file_comparison(&file("a.txt"), Some(step.clone())));
    serve(&service, &mut doc);
    let versions = header_versions(&doc, "a.txt").expect("a.txt's versions");
    assert_eq!((versions.version, versions.shown), (4, Some(0)));
    // The summary is the delta's, summed once.
    assert_eq!(
        doc.comparison_summary(SOURCE),
        Some(ComparisonSummary::of(&step))
    );
    let summary = ComparisonSummary::of(&step);
    assert_eq!((summary.added, summary.removed), step.totals());
    doc.set_comparison(SOURCE, None);
    assert_eq!(doc.comparison_summary(SOURCE), None);
}
