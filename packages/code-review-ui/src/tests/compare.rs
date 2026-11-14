//! Comparisons in the app: a stack review reopened after a push compares each layer with what
//! the reviewer last looked at, in place, and switches between the full change and only what
//! changed; looks are recorded as the reviewer works.

use super::app::{create, fixture, settled};
use super::publish::{layer, push_to, resume};
use crate::app::ReviewApp;
use crate::testing::backend::TestBackend;
use crate::testing::fake_provider::{FAKE_PROVIDER, FAKE_REPOSITORY, FakeProvider};
use crate::testing::fixture::{GitFixture, STACK_BRANCHES};
use crate::testing::harness::Scenario;
use crate::testing::harness::Screen;
use bcode_code_review::document::{CompareView, FileKey, ReviewDocument};
use bcode_code_review_models::{
    DeltaKind, FileSince, LayerLook, ListLayerLooksRequest, ListLayerLooksResponse, LookKind,
};
use std::sync::Arc;

fn document(scenario: &Scenario<ReviewApp>) -> &ReviewDocument {
    scenario
        .surface()
        .review()
        .and_then(|review| review.document())
        .expect("a document")
}

/// The source id of the group reviewing layer `number`.
fn source_of(doc: &ReviewDocument, number: u64) -> String {
    doc.groups()
        .find(|group| {
            group
                .stack
                .and_then(|stack| stack.stack.layers.last())
                .is_some_and(|layer| layer.number == number)
        })
        .map(|group| group.source.source_id.clone())
        .expect("a layer source")
}

fn kind_of(doc: &ReviewDocument, number: u64) -> Option<DeltaKind> {
    doc.comparison(&source_of(doc, number))
        .and_then(|comparison| comparison.delta.as_ref())
        .map(|delta| delta.kind)
}

fn looks(backend: &TestBackend, fixture: &GitFixture) -> Vec<LayerLook> {
    let listed: ListLayerLooksResponse = backend.call_now(ListLayerLooksRequest {
        repo_path: fixture.root().to_path_buf(),
        provider: FAKE_PROVIDER.to_owned(),
        repository: FAKE_REPOSITORY.to_owned(),
        numbers: vec![101, 102, 103],
    });
    listed.looks
}

#[test]
fn a_reopened_stack_compares_each_layer_with_what_was_looked_at_in_place() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default().with_provider(FakeProvider::new(fixture.root())));
    let workspace = create(
        &backend,
        &fixture,
        "Compare",
        vec![layer(101), layer(102), layer(103)],
    );
    // First visit: nothing looked at yet, so nothing is compared; completing the pass records
    // a look at every layer.
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    settled(&mut scenario);
    assert!(!scenario.surface().review().unwrap().comparing().active());
    scenario.key("shift+m");
    scenario.key("q");
    settled(&mut scenario);
    let passes = looks(&backend, &fixture);
    assert_eq!(
        passes
            .iter()
            .filter(|look| look.kind == LookKind::Pass)
            .count(),
        3,
        "{passes:?}"
    );

    // The top layer is pushed again; the others stay.
    push_to(&fixture, STACK_BRANCHES[2], "pushed-later.txt");
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    settled(&mut scenario);
    let doc = document(&scenario);
    assert_eq!(kind_of(doc, 101), Some(DeltaKind::Unchanged));
    assert_eq!(kind_of(doc, 102), Some(DeltaKind::Unchanged));
    assert_eq!(kind_of(doc, 103), Some(DeltaKind::Changed));
    let top = source_of(doc, 103);
    let pushed = FileKey::new(top.clone(), None, "pushed-later.txt");
    assert!(matches!(
        doc.file_since(&pushed),
        Some(FileSince::New { .. })
    ));
    let progress = doc.viewed_progress();
    let files: Vec<_> = doc.file_keys().cloned().collect();

    // shift+l shows only what changed, in the same document, and back.
    scenario.key("shift+l");
    settled(&mut scenario);
    let doc = document(&scenario);
    assert_eq!(
        doc.comparison(&top).map(|comparison| comparison.view),
        Some(CompareView::Changes)
    );
    assert!(doc.file_shows_changes(&pushed));
    assert_eq!(doc.file_keys().cloned().collect::<Vec<_>>(), files);
    assert_eq!(doc.viewed_progress(), progress);
    scenario.key("shift+l");
    settled(&mut scenario);
    let doc = document(&scenario);
    assert!(!doc.file_shows_changes(&pushed));
    assert_eq!(doc.viewed_progress(), progress);
}

#[test]
fn marking_a_file_viewed_records_a_look_and_a_reopen_shows_nothing_new() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default().with_provider(FakeProvider::new(fixture.root())));
    let workspace = create(&backend, &fixture, "Looks", vec![layer(101)]);
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    settled(&mut scenario);
    scenario.key("n");
    scenario.key("v");
    scenario.key("q");
    settled(&mut scenario);
    let recorded = looks(&backend, &fixture);
    assert!(
        recorded
            .iter()
            .any(|look| look.number == 101 && look.kind == LookKind::Viewed),
        "{recorded:?}"
    );
    // The layer did not move since: reopened, it is unchanged since the reviewer looked.
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    settled(&mut scenario);
    assert_eq!(
        kind_of(document(&scenario), 101),
        Some(DeltaKind::Unchanged)
    );
}

/// Edit the bottom layer (#101) and restack the two above it onto it, as a stack tool does:
/// #101 changes, #102 and #103 are only rebased.
fn restack(fixture: &GitFixture) {
    let worktree = tempfile::tempdir().expect("worktree dir");
    let checkout = worktree.path().join("checkout");
    let now = format!(
        "@{} +0000",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_secs()
    );
    let git = |dir: &std::path::Path, args: &[&str]| -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .env("GIT_AUTHOR_DATE", &now)
            .env("GIT_COMMITTER_DATE", &now)
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    };
    let old: Vec<String> = STACK_BRANCHES
        .iter()
        .map(|branch| git(fixture.root(), &["rev-parse", branch]))
        .collect();
    let path = checkout.to_str().expect("path");
    git(
        fixture.root(),
        &["worktree", "add", "-q", path, STACK_BRANCHES[0]],
    );
    std::fs::write(
        checkout.join("restacked.txt"),
        "edited in the bottom layer\n",
    )
    .expect("write");
    git(&checkout, &["add", "restacked.txt"]);
    git(&checkout, &["commit", "-q", "-m", "edit the bottom layer"]);
    for index in 1..STACK_BRANCHES.len() {
        git(
            &checkout,
            &[
                "rebase",
                "-q",
                "--onto",
                STACK_BRANCHES[index - 1],
                &old[index - 1],
                STACK_BRANCHES[index],
            ],
        );
    }
    git(fixture.root(), &["worktree", "remove", "--force", path]);
}

/// A three-layer review looked at (a completed pass), then restacked, reopened.
fn reopened_after_a_restack(
    backend: TestBackend,
) -> (GitFixture, Arc<TestBackend>, String, Scenario<ReviewApp>) {
    let fixture = fixture();
    let backend = Arc::new(backend.with_provider(FakeProvider::new(fixture.root())));
    let workspace = create(
        &backend,
        &fixture,
        "Restacked",
        vec![layer(101), layer(102), layer(103)],
    );
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    settled(&mut scenario);
    scenario.key("shift+m");
    scenario.key("q");
    settled(&mut scenario);
    restack(&fixture);
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    settled(&mut scenario);
    (fixture, backend, workspace.id, scenario)
}

fn run(scenario: &mut Scenario<ReviewApp>, command: &str) -> Screen {
    scenario.key(":");
    scenario.paste(command);
    scenario.key("enter");
    settled(scenario)
}

#[test]
fn a_restacked_stack_reopens_with_the_banner_its_badges_and_rebased_layers_collapsed() {
    let (_fixture, _backend, id, mut scenario) = reopened_after_a_restack(TestBackend::default());
    let screen = settled(&mut scenario);
    let doc = document(&scenario);
    assert_eq!(kind_of(doc, 101), Some(DeltaKind::Changed));
    assert_eq!(kind_of(doc, 102), Some(DeltaKind::RebasedOnly));
    assert_eq!(kind_of(doc, 103), Some(DeltaKind::RebasedOnly));
    let review = scenario.surface().review().expect("review");
    assert!(review.comparing().banner_shown());
    assert_eq!(review.comparing().view(), CompareView::Full);
    // The comparison row, the banner, and the badges, in the same review.
    assert!(
        screen.contains("Changes since: [you last looked"),
        "{}",
        screen.text()
    );
    assert!(
        screen.contains("1 of 3 PRs changed · 2 rebased only"),
        "{}",
        screen.text()
    );
    assert!(
        screen.contains("Changed since you looked"),
        "{}",
        screen.text()
    );
    assert!(screen.contains("○ rebased"), "{}", screen.text());
    // The new file of the edited layer is new since the baseline.
    let bottom = source_of(doc, 101);
    let edited = FileKey::new(bottom.clone(), None, "restacked.txt");
    assert!(matches!(
        doc.file_since(&edited),
        Some(FileSince::New { .. })
    ));
    // A rebased-only layer shows no files until asked.
    let rebased = source_of(doc, 102);
    assert!(!doc.compare_files_shown(&rebased));
    assert_eq!(
        review.workspace().map(|workspace| workspace.id.clone()),
        Some(id)
    );

    // The edited layer's file headers say in which pushes each file changed.
    let edited_versions = (0..doc.len()).find_map(|index| match doc.item(index) {
        Some(bcode_code_review::document::ReviewItem::FileHeader(header))
            if header.file.path == "restacked.txt" =>
        {
            Some(header.versions)
        }
        _ => None,
    });
    assert!(
        matches!(edited_versions, Some(Some(versions)) if versions.changed_in == 1),
        "{edited_versions:?}"
    );
    // Only what changed, and back: the same files, the same viewed progress.
    let files: Vec<_> = doc.file_keys().cloned().collect();
    let progress = doc.viewed_progress();
    scenario.key("shift+l");
    settled(&mut scenario);
    let doc = document(&scenario);
    assert!(doc.file_shows_changes(&edited) || doc.file_since(&edited).is_some());
    assert_eq!(
        doc.comparison(&bottom).map(|c| c.view),
        Some(CompareView::Changes)
    );
    assert_eq!(doc.file_keys().cloned().collect::<Vec<_>>(), files);
    assert_eq!(doc.viewed_progress(), progress);
    scenario.key("shift+l");
    settled(&mut scenario);
    assert_eq!(document(&scenario).viewed_progress(), progress);
}

#[test]
fn the_baseline_control_compares_with_other_baselines_in_place() {
    use bcode_code_review_models::ReviewCompareBaseline;
    let (_fixture, _backend, id, mut scenario) = reopened_after_a_restack(TestBackend::default());
    // `b` opens the control's menu.
    scenario.key("b");
    let screen = settled(&mut scenario);
    assert!(screen.contains("Changes since…"), "{}", screen.text());
    assert!(screen.contains("You last looked"), "{}", screen.text());
    scenario.key("escape");
    settled(&mut scenario);
    // Full change: nothing compared, no banner, the same review.
    let screen = run(&mut scenario, "Compare: full change");
    let review = scenario.surface().review().expect("review");
    assert!(!review.comparing().active());
    assert_eq!(review.comparing().baseline(), ReviewCompareBaseline::Off);
    assert!(
        screen.contains("Changes since: [full change"),
        "{}",
        screen.text()
    );
    assert_eq!(
        review.workspace().map(|workspace| workspace.id.clone()),
        Some(id)
    );
    // Back to what was last looked at.
    run(&mut scenario, "Compare: with what you last looked at");
    let doc = document(&scenario);
    assert_eq!(kind_of(doc, 101), Some(DeltaKind::Changed));

    // The previous push, from the menu: each layer before the restack.
    scenario.key("b");
    settled(&mut scenario);
    let menu = settled(&mut scenario);
    assert!(menu.contains("Previous push"), "{}", menu.text());
    scenario.key("escape");
    {
        let screen = run(&mut scenario, "Compare pushes of the stack");
        assert!(screen.contains("pushes:"), "{}", screen.text());
        scenario.key("enter");
        let screen = settled(&mut scenario);
        let review = scenario.surface().review().expect("review");
        assert!(
            matches!(
                review.comparing().baseline(),
                ReviewCompareBaseline::Between { .. }
            ),
            "{:?}",
            review.comparing().baseline()
        );
        assert!(screen.contains(" now"), "{}", screen.text());
        assert_eq!(kind_of(document(&scenario), 101), Some(DeltaKind::Changed));
    }
    // The choice is kept for the next open.
    scenario.key("q");
    settled(&mut scenario);
}

#[test]
fn a_version_of_one_pull_request_is_compared_in_place_and_an_older_one_is_read_only() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default().with_provider(FakeProvider::new(fixture.root())));
    let workspace = create(&backend, &fixture, "Versions", vec![layer(101), layer(102)]);
    // Three versions of #101, each seen by an open.
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    settled(&mut scenario);
    scenario.key("q");
    settled(&mut scenario);
    for path in ["second.txt", "third.txt"] {
        push_to(&fixture, STACK_BRANCHES[0], path);
        let mut scenario = resume(&fixture, &backend, &workspace.id);
        settled(&mut scenario);
        scenario.key("q");
        settled(&mut scenario);
    }
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    settled(&mut scenario);
    let screen = run(&mut scenario, "Compare versions of this pull request");
    assert!(screen.contains("Versions of #101"), "{}", screen.text());
    let Some(crate::surface::Modal::Versions(picker)) = scenario
        .surface()
        .review()
        .and_then(|review| review.modal())
    else {
        panic!("the version picker: {}", screen.text());
    };
    let versions = picker.layer().versions.len();
    assert!(versions >= 3, "{:?}", picker.rows());
    // From the version before the newest to now.
    scenario.key("down");
    scenario.key("enter");
    settled(&mut scenario);
    let review = scenario.surface().review().expect("review");
    assert!(review.comparing().picks().contains_key(&101));
    let doc = document(&scenario);
    assert_eq!(kind_of(doc, 101), Some(DeltaKind::Changed));
    let top = source_of(doc, 101);
    let third = FileKey::new(top.clone(), None, "third.txt");
    assert!(matches!(
        doc.file_since(&third),
        Some(FileSince::New { .. })
    ));
    assert!(!doc.file_is_historical(&third));
    assert_eq!(
        review.workspace().map(|workspace| workspace.id.clone()),
        Some(workspace.id)
    );

    // The pull request's file headers say in which pushes each file changed.
    let doc = document(&scenario);
    let headers: Vec<_> = (0..doc.len())
        .filter_map(|index| match doc.item(index) {
            Some(bcode_code_review::document::ReviewItem::FileHeader(header)) => {
                Some((header.file.path.clone(), header.versions))
            }
            _ => None,
        })
        .collect();
    let third_versions = headers
        .iter()
        .find(|(path, _)| path == "third.txt")
        .and_then(|(_, versions)| *versions);
    assert!(
        third_versions.is_some_and(|versions| versions.changed_in == 1 && versions.steps >= 2),
        "{headers:?}"
    );
    let screen = settled(&mut scenario);
    assert!(screen.contains("changed in 1 of"), "{}", screen.text());

    // Two older versions: read-only, only what changed.
    run(&mut scenario, "Compare versions of this pull request");
    // The newest is `to`; move to the one before it and make it `to`, then pick from below.
    scenario.key("down");
    scenario.key("t");
    scenario.key("down");
    scenario.key("enter");
    settled(&mut scenario);
    let doc = document(&scenario);
    let second = FileKey::new(top, None, "second.txt");
    assert!(
        doc.file_is_historical(&second),
        "{:?}",
        doc.file_since(&second)
    );
    // A line of an older version takes no comment.
    let before = looks(&backend, &fixture).len();
    let screen = settled(&mut scenario);
    let row = (0..screen.height())
        .find(|y| screen.row_text(*y).contains("pushed later"))
        .expect("the older version's line shows");
    let x = screen.column_of(row, "pushed later").expect("column");
    scenario.click(x, row);
    scenario.key("c");
    let screen = settled(&mut scenario);
    assert!(screen.contains("older version"), "{}", screen.text());
    // Nothing about the older version is recorded as looked at.
    scenario.key("q");
    settled(&mut scenario);
    let after = looks(&backend, &fixture);
    assert!(
        !after
            .iter()
            .skip(before)
            .any(|look| look.number == 101 && look.kind == LookKind::Commented),
        "{after:?}"
    );
}

#[test]
fn the_history_matrix_compares_in_place_without_opening_another_review() {
    let (_fixture, _backend, id, mut scenario) = reopened_after_a_restack(TestBackend::default());
    scenario.key("shift+b");
    let screen = settled(&mut scenario);
    assert!(screen.contains("Stack history"), "{}", screen.text());
    // The bottom pull request (#101 is the matrix's second row), from the push before to now.
    scenario.key("down");
    scenario.key("enter");
    let screen = settled(&mut scenario);
    assert!(!screen.contains("Stack history"), "{}", screen.text());
    let review = scenario.surface().review().expect("the same review");
    assert!(!review.is_transient());
    assert_eq!(
        review.workspace().map(|workspace| workspace.id.clone()),
        Some(id)
    );
    assert!(
        review.comparing().picks().contains_key(&101),
        "{:?}",
        review.comparing().picks()
    );
    assert_eq!(kind_of(document(&scenario), 101), Some(DeltaKind::Changed));
}

#[test]
fn only_real_changes_hides_rebased_layers_from_the_rail_and_from_stepping() {
    let (_fixture, _backend, _id, mut scenario) = reopened_after_a_restack(TestBackend::default());
    run(
        &mut scenario,
        "Compare: only pull requests with real changes",
    );
    let review = scenario.surface().review().expect("review");
    assert!(review.comparing().only_changed());
    // `.` stays on the one layer with real changes; the rail folds the others into one tab.
    let current = |scenario: &Scenario<ReviewApp>| {
        scenario
            .surface()
            .review()
            .and_then(|review| review.cursor().cloned())
            .and_then(|key| match key {
                bcode_code_review::document::ItemKey::SourceHeader { source_id, .. }
                | bcode_code_review::document::ItemKey::SourceIntro { source_id } => {
                    Some(source_id)
                }
                key => key.file().map(|file| file.source_id.clone()),
            })
    };
    scenario.key(".");
    let screen = settled(&mut scenario);
    let doc = document(&scenario);
    let bottom = source_of(doc, 101);
    assert_eq!(current(&scenario), Some(bottom));
    assert!(screen.contains("+2 ○"), "{}", screen.text());
    // Every layer again.
    run(
        &mut scenario,
        "Compare: only pull requests with real changes",
    );
    assert!(
        !scenario
            .surface()
            .review()
            .unwrap()
            .comparing()
            .only_changed()
    );
}

#[test]
fn staying_on_a_layer_records_an_opened_look() {
    let config = crate::config::compare::CompareConfig {
        dwell_seconds: 0,
        ..crate::config::compare::CompareConfig::default()
    };
    let (fixture, backend, _id, mut scenario) =
        reopened_after_a_restack(TestBackend::default().with_compare_config(config));
    settled(&mut scenario);
    scenario.key("q");
    settled(&mut scenario);
    assert!(
        looks(&backend, &fixture)
            .iter()
            .any(|look| look.kind == LookKind::Opened),
        "{:?}",
        looks(&backend, &fixture)
    );
}
