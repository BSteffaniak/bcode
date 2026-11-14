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
    assert!(screen.contains("Show changes since"), "{}", screen.text());
    assert!(screen.contains("◉ you last looked"), "{}", screen.text());
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
    assert!(menu.contains("○ the previous push"), "{}", menu.text());
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

#[test]
fn clicking_a_summary_rows_view_button_switches_the_whole_screen() {
    let (_fixture, _backend, _id, mut scenario) = reopened_after_a_restack(TestBackend::default());
    let screen = settled(&mut scenario);
    let row = (0..screen.height())
        .find(|y| screen.row_text(*y).contains("[only what changed]"))
        .unwrap_or_else(|| panic!("a summary row: {}", screen.text()));
    let x = screen
        .column_of(row, "[only what changed]")
        .expect("column");
    scenario.click(x + 1, row);
    let screen = settled(&mut scenario);
    let review = scenario.surface().review().expect("review");
    assert_eq!(review.comparing().view(), CompareView::Changes);
    let doc = document(&scenario);
    assert_eq!(
        doc.comparison(&source_of(doc, 101)).map(|c| c.view),
        Some(CompareView::Changes)
    );
    assert!(screen.contains("[full change]"), "{}", screen.text());
    // One key press goes back.
    scenario.key("shift+l");
    settled(&mut scenario);
    let review = scenario.surface().review().expect("review");
    assert_eq!(review.comparing().view(), CompareView::Full);
    let doc = document(&scenario);
    assert_eq!(
        doc.comparison(&source_of(doc, 101)).map(|c| c.view),
        Some(CompareView::Full)
    );
}

/// Refresh the review the way a change in the checkout does (a file staged, named after
/// `tag`), so its sources are resolved again with what the provider says now.
fn refresh(fixture: &GitFixture, scenario: &mut Scenario<ReviewApp>, tag: &str) {
    let path = format!("refresh-{tag}.txt");
    std::fs::write(fixture.root().join(&path), "staged\n").expect("write");
    fixture.run(&["add", &path]);
    scenario.key("r");
    settled(scenario);
}

/// The file headers' versions of `path`, when shown.
fn versions_of(
    doc: &ReviewDocument,
    path: &str,
) -> Option<bcode_code_review::document::FileVersionsItem> {
    (0..doc.len()).find_map(|index| match doc.item(index) {
        Some(bcode_code_review::document::ReviewItem::FileHeader(header))
            if header.file.path == path =>
        {
            header.versions
        }
        _ => None,
    })
}

#[test]
fn a_layer_pushed_while_open_is_compared_from_what_was_engaged_with_this_session() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default().with_provider(FakeProvider::new(fixture.root())));
    let workspace = create(&backend, &fixture, "Moving", vec![layer(101)]);
    // A completed pass at the first version.
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    settled(&mut scenario);
    scenario.key("shift+m");
    scenario.key("q");
    settled(&mut scenario);
    // Pushed once: reopened, the push is new since the pass.
    push_to(&fixture, STACK_BRANCHES[0], "first-push.txt");
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    settled(&mut scenario);
    let doc = document(&scenario);
    let source = source_of(doc, 101);
    let first = FileKey::new(source.clone(), None, "first-push.txt");
    assert!(matches!(
        doc.file_since(&first),
        Some(FileSince::New { .. })
    ));
    // The reviewer marks a file viewed (engaging with this version), then the author pushes
    // again and the review is refreshed.
    scenario.key("n");
    scenario.key("v");
    settled(&mut scenario);
    push_to(&fixture, STACK_BRANCHES[0], "second-push.txt");
    refresh(&fixture, &mut scenario, "second");
    let doc = document(&scenario);
    let second = FileKey::new(source.clone(), None, "second-push.txt");
    // Compared from the version engaged with: only the second push is new.
    assert!(
        matches!(doc.file_since(&second), Some(FileSince::New { .. })),
        "{:?} {:?} {:?}",
        doc.file_since(&second),
        doc.file_keys().collect::<Vec<_>>(),
        doc.comparison(&source)
    );
    assert!(
        !matches!(doc.file_since(&first), Some(FileSince::New { .. })),
        "{:?}",
        doc.file_since(&first)
    );
    // The file headers know the newest push (the history read again after the refresh).
    let versions = versions_of(doc, "second-push.txt");
    assert!(
        versions.is_some_and(
            |versions| versions.changed_in == 1 && versions.version == versions.steps + 1
        ),
        "{versions:?}"
    );
}

#[test]
fn a_saved_full_change_yields_to_what_is_new_when_the_stack_moved() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default().with_provider(FakeProvider::new(fixture.root())));
    let workspace = create(
        &backend,
        &fixture,
        "Saved choice",
        vec![layer(101), layer(102), layer(103)],
    );
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    settled(&mut scenario);
    scenario.key("shift+m");
    run(&mut scenario, "Compare: full change");
    scenario.key("q");
    settled(&mut scenario);
    // Nothing moved: the choice is kept.
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    settled(&mut scenario);
    let review = scenario.surface().review().expect("review");
    assert_eq!(
        review.comparing().baseline(),
        bcode_code_review_models::ReviewCompareBaseline::Off
    );
    scenario.key("q");
    settled(&mut scenario);
    // Restacked: the reopened review shows what is new since the reviewer looked.
    restack(&fixture);
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    settled(&mut scenario);
    let review = scenario.surface().review().expect("review");
    assert_eq!(
        review.comparing().baseline(),
        bcode_code_review_models::ReviewCompareBaseline::LastLooked
    );
    assert!(review.comparing().banner_shown());
    assert_eq!(kind_of(document(&scenario), 101), Some(DeltaKind::Changed));
}

#[test]
fn looks_seeded_from_a_pass_keep_their_heads_with_refs() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default().with_provider(FakeProvider::new(fixture.root())));
    let workspace = create(&backend, &fixture, "Seeded", vec![layer(101)]);
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    settled(&mut scenario);
    scenario.key("shift+m");
    scenario.key("q");
    settled(&mut scenario);
    let refs = |fixture: &GitFixture| {
        let output = std::process::Command::new("git")
            .args([
                "for-each-ref",
                "--format=%(refname)",
                "refs/bcode-review/looked",
            ])
            .current_dir(fixture.root())
            .output()
            .expect("git");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    };
    let kept = refs(&fixture);
    assert_ne!(kept, "");
    // Refs lost (as before looks kept them): reopening seeds the pass again and keeps it.
    for name in kept.lines() {
        fixture.run(&["update-ref", "-d", name]);
    }
    assert_eq!(refs(&fixture), "");
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    settled(&mut scenario);
    assert_eq!(refs(&fixture), kept);
}

#[test]
fn looks_that_cannot_be_read_do_not_hold_up_the_review() {
    use bcode_code_review_models::OP_REVIEW_LOOKS_LIST;
    let (fixture, backend, id, scenario) = reopened_after_a_restack(TestBackend::default());
    drop(scenario);
    backend.set_failing(OP_REVIEW_LOOKS_LIST, true);
    let mut scenario = resume(&fixture, &backend, &id);
    let screen = settled(&mut scenario);
    assert!(
        screen.contains("could not read what you looked at before"),
        "{}",
        screen.text()
    );
    let review = scenario.surface().review().expect("review");
    assert!(!review.comparing().active());
    assert!(!review.comparing().banner_shown());
    // Readable again: a refresh asks again and compares.
    backend.set_failing(OP_REVIEW_LOOKS_LIST, false);
    refresh(&fixture, &mut scenario, "again");
    settled(&mut scenario);
    assert_eq!(kind_of(document(&scenario), 101), Some(DeltaKind::Changed));
}

#[test]
fn a_single_pull_request_is_compared_with_what_was_looked_at() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default().with_provider(FakeProvider::new(fixture.root())));
    let pull = bcode_code_review_models::ReviewSourceKind::PullRequest {
        provider: FAKE_PROVIDER.to_owned(),
        repository: FAKE_REPOSITORY.to_owned(),
        number: 101,
    };
    let workspace = create(&backend, &fixture, "One pull request", vec![pull]);
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    settled(&mut scenario);
    scenario.key("n");
    scenario.key("v");
    scenario.key("q");
    settled(&mut scenario);
    push_to(&fixture, STACK_BRANCHES[0], "pushed-later.txt");
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    let screen = settled(&mut scenario);
    let doc = document(&scenario);
    let source = doc
        .groups()
        .next()
        .map(|group| group.source.source_id.clone())
        .expect("a source");
    let delta = doc
        .comparison(&source)
        .and_then(|comparison| comparison.delta.as_ref());
    assert_eq!(delta.map(|delta| delta.kind), Some(DeltaKind::Changed));
    let pushed = FileKey::new(source, None, "pushed-later.txt");
    assert!(matches!(
        doc.file_since(&pushed),
        Some(FileSince::New { .. })
    ));
    assert!(
        scenario
            .surface()
            .review()
            .unwrap()
            .comparing()
            .banner_shown()
    );
    assert!(
        screen.contains("Changes since: [you last looked"),
        "{}",
        screen.text()
    );
}

#[test]
fn a_branch_is_compared_with_the_last_completed_pass() {
    use crate::testing::fixture::HEAD_BRANCH;
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default().with_provider(FakeProvider::new(fixture.root())));
    let branch = bcode_code_review_models::ReviewSourceKind::BranchCompare {
        base_branch: HEAD_BRANCH.to_owned(),
        head_branch: STACK_BRANCHES[0].to_owned(),
        merge_base: true,
    };
    let workspace = create(&backend, &fixture, "Branch", vec![branch]);
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    settled(&mut scenario);
    assert!(!scenario.surface().review().unwrap().comparing().active());
    scenario.key("shift+m");
    scenario.key("q");
    settled(&mut scenario);
    push_to(&fixture, STACK_BRANCHES[0], "pushed-later.txt");
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    let screen = settled(&mut scenario);
    let doc = document(&scenario);
    let source = doc
        .groups()
        .next()
        .map(|group| group.source.source_id.clone())
        .expect("a source");
    let pushed = FileKey::new(source.clone(), None, "pushed-later.txt");
    assert!(
        matches!(doc.file_since(&pushed), Some(FileSince::New { .. })),
        "{:?}",
        doc.comparison(&source)
    );
    assert!(
        screen.contains("1 of 1 source changed"),
        "{}",
        screen.text()
    );
    // The banner names sources, and the control says when the pass was.
    assert!(screen.contains(": 1 source changed."), "{}", screen.text());
    assert!(!screen.contains("[you last looked ▾]"), "{}", screen.text());
    // Only what changed, in place.
    scenario.key("shift+l");
    settled(&mut scenario);
    assert!(document(&scenario).file_shows_changes(&pushed));
}

#[test]
fn a_files_versions_step_with_alt_comma_and_alt_period() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default().with_provider(FakeProvider::new(fixture.root())));
    let workspace = create(&backend, &fixture, "Step keys", vec![layer(101)]);
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    settled(&mut scenario);
    scenario.key("shift+m");
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
    let doc = document(&scenario);
    let second = FileKey::new(source_of(doc, 101), None, "second.txt");
    let index = doc
        .index_of(&bcode_code_review::document::ItemKey::FileHeader(Arc::new(
            second.clone(),
        )))
        .expect("the file's header");
    // Put the cursor on the file's header by clicking it.
    let screen = settled(&mut scenario);
    let row = (0..screen.height())
        .find(|y| screen.row_text(*y).contains("second.txt"))
        .unwrap_or_else(|| panic!("the header shows ({index}): {}", screen.text()));
    let x = screen.column_of(row, "second.txt").expect("column");
    scenario.click(x, row);
    settled(&mut scenario);
    scenario.key("alt+,");
    settled(&mut scenario);
    assert!(document(&scenario).file_override(&second).is_some());
    scenario.key("alt+.");
    settled(&mut scenario);
    assert!(document(&scenario).file_override(&second).is_none());
}

/// The trailing text of the file panel's row for `path`.
fn tree_row(scenario: &Scenario<ReviewApp>, path: &str) -> (String, Vec<String>) {
    let tree = scenario
        .surface()
        .review()
        .and_then(|review| review.file_tree())
        .expect("the file tree");
    let item = tree
        .items()
        .iter()
        .find(|item| item.label.contains(path))
        .unwrap_or_else(|| panic!("no row {path}: {:?}", tree.items()));
    (
        item.label.clone(),
        item.trailing
            .iter()
            .map(bmux_tui::text::Line::plain_text)
            .collect(),
    )
}

#[test]
fn stepping_a_files_version_updates_its_row_in_the_file_panel() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default().with_provider(FakeProvider::new(fixture.root())));
    let workspace = create(&backend, &fixture, "Step panel", vec![layer(101)]);
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    settled(&mut scenario);
    scenario.key("shift+m");
    scenario.key("q");
    settled(&mut scenario);
    // `early.txt` is pushed and looked at (a completed pass); `late.txt` is pushed after.
    for path in ["early.txt", "late.txt"] {
        push_to(&fixture, STACK_BRANCHES[0], path);
        let mut scenario = resume(&fixture, &backend, &workspace.id);
        settled(&mut scenario);
        if path == "early.txt" {
            scenario.key("shift+m");
        }
        scenario.key("q");
        settled(&mut scenario);
    }
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    settled(&mut scenario);
    let doc = document(&scenario);
    let early = FileKey::new(source_of(doc, 101), None, "early.txt");
    assert_eq!(doc.file_since(&early), Some(FileSince::Unchanged));
    // Only what changed, with the unchanged files shown again (dimmed, hideable).
    scenario.key("shift+l");
    settled(&mut scenario);
    scenario.key(":");
    scenario.paste("Compare: show or hide the files the comparison hides");
    scenario.key("enter");
    let screen = settled(&mut scenario);
    let hideable = |scenario: &Scenario<ReviewApp>| {
        scenario
            .surface()
            .review()
            .and_then(|review| review.file_tree())
            .map(crate::files::FileTree::hideable_shown)
    };
    let before = hideable(&scenario).expect("the tree");
    assert!(before > 0, "{}", screen.text());
    let (label, _) = tree_row(&scenario, "early.txt");
    assert!(label.starts_with("A "), "{label}");
    let row = (0..screen.height())
        .find(|y| {
            let text = screen.row_text(*y);
            text.contains("early.txt") && !text.contains("A early.txt")
        })
        .unwrap_or_else(|| panic!("the header shows: {}", screen.text()));
    let x = screen.column_of(row, "early.txt").expect("column");
    scenario.click(x, row);
    settled(&mut scenario);
    // The push that added it: the panel's row and its counts follow the document at once.
    scenario.key("alt+,");
    let doc = document(&scenario);
    assert!(doc.file_override(&early).is_some());
    assert!(matches!(
        doc.file_since(&early),
        Some(FileSince::New { .. })
    ));
    let (label, trailing) = tree_row(&scenario, "early.txt");
    assert!(label.starts_with("+ "), "{label}");
    assert!(trailing.contains(&"new +1".to_owned()), "{trailing:?}");
    assert_eq!(hideable(&scenario), Some(before - 1));
}

#[test]
fn a_failed_comparison_leaves_no_row_saying_it_is_still_comparing() {
    let (fixture, backend, _id, mut scenario) = reopened_after_a_restack(TestBackend::default());
    settled(&mut scenario);
    scenario.key("shift+o");
    settled(&mut scenario);
    // The bottom layer moves and the review is refreshed; comparing it again fails.
    backend.set_failing(bcode_code_review_models::OP_REVIEW_COMPARE_LAYERS, true);
    push_to(&fixture, STACK_BRANCHES[0], "pushed-while-open.txt");
    refresh(&fixture, &mut scenario, "failing");
    let doc = document(&scenario);
    assert!(doc.comparison(&source_of(doc, 101)).is_none());
    let labels = tree_labels(&scenario);
    assert!(
        !labels.iter().any(|label| label.contains("comparing")),
        "{labels:?}"
    );
    scenario.key("b");
    let screen = settled(&mut scenario);
    let Some(crate::surface::Modal::Compare(menu)) = scenario
        .surface()
        .review()
        .and_then(|review| review.modal())
    else {
        panic!("the menu: {}", screen.text());
    };
    assert!(menu.state().progress.is_none(), "{:?}", menu.state());
}

#[test]
fn the_history_marks_versions_the_reviewer_viewed_without_a_completed_pass() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default().with_provider(FakeProvider::new(fixture.root())));
    let workspace = create(&backend, &fixture, "Marks", vec![layer(101), layer(102)]);
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    settled(&mut scenario);
    let viewed_head = document(&scenario)
        .groups()
        .find_map(|group| {
            let layer = group.stack?.stack.layers.last()?;
            (layer.number == 101).then(|| layer.head_oid.clone())
        })
        .expect("#101's head");
    // A file of #101 viewed; no pass completed.
    scenario.key("n");
    scenario.key("v");
    scenario.key("q");
    settled(&mut scenario);
    push_to(&fixture, STACK_BRANCHES[0], "pushed-later.txt");
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    settled(&mut scenario);
    scenario.key("shift+b");
    settled(&mut scenario);
    let Some(crate::surface::Modal::History(panel)) = scenario
        .surface()
        .review()
        .and_then(|review| review.modal())
    else {
        panic!("the history matrix");
    };
    let timeline = panel.timeline().expect("a timeline");
    let versions = &timeline
        .layers
        .iter()
        .find(|layer| layer.number == 101)
        .expect("#101")
        .versions;
    let viewed = versions
        .iter()
        .find(|version| version.version.head_oid == viewed_head)
        .expect("the viewed version");
    assert!(viewed.reviewed, "{versions:?}");
    assert!(!versions.last().expect("newest").reviewed, "{versions:?}");
}

/// The row the open comparison menu's cursor is on.
fn menu_focus(scenario: &Scenario<ReviewApp>) -> Option<crate::compare_menu::MenuChoice> {
    match scenario
        .surface()
        .review()
        .and_then(|review| review.modal())
    {
        Some(crate::surface::Modal::Compare(menu)) => menu.focused(),
        _ => None,
    }
}

/// Move the open menu's cursor to `choice`.
fn menu_to(scenario: &mut Scenario<ReviewApp>, choice: crate::compare_menu::MenuChoice) {
    scenario.key("home");
    for _ in 0..20 {
        if menu_focus(scenario) == Some(choice) {
            return;
        }
        scenario.key("down");
    }
    panic!("no menu row {choice:?}");
}

#[test]
fn the_menu_previews_picks_live_keeps_them_on_enter_and_puts_them_back_on_escape() {
    use crate::compare_menu::MenuChoice;
    use bcode_code_review_models::ReviewCompareBaseline;
    let (_fixture, _backend, _id, mut scenario) = reopened_after_a_restack(TestBackend::default());
    let saved = |scenario: &Scenario<ReviewApp>| {
        scenario
            .surface()
            .review()
            .expect("review")
            .presentation()
            .compare
    };
    let before = saved(&scenario);
    scenario.key("b");
    let screen = settled(&mut scenario);
    // The menu opens on the baseline in effect, marked, with what it resolves to.
    assert_eq!(menu_focus(&scenario), Some(MenuChoice::LastLooked));
    assert!(
        screen.contains("◉ you last looked · just now"),
        "{}",
        screen.text()
    );
    assert!(
        screen.contains("[ ] Only what changed"),
        "{}",
        screen.text()
    );

    // Space flips a view option in place: the review behind switches, the menu stays open.
    menu_to(&mut scenario, MenuChoice::View);
    scenario.key("space");
    let screen = settled(&mut scenario);
    let review = scenario.surface().review().expect("review");
    assert!(review.modal().is_some(), "{}", screen.text());
    assert_eq!(review.comparing().view(), CompareView::Changes);
    let doc = document(&scenario);
    assert_eq!(
        doc.comparison(&source_of(doc, 101)).map(|c| c.view),
        Some(CompareView::Changes)
    );
    assert!(
        screen.contains("[x] Only what changed"),
        "{}",
        screen.text()
    );
    assert_eq!(menu_focus(&scenario), Some(MenuChoice::View));

    // Space on a baseline compares with it at once; the mark moves, the menu stays.
    menu_to(&mut scenario, MenuChoice::Off);
    scenario.key("space");
    let screen = settled(&mut scenario);
    let review = scenario.surface().review().expect("review");
    assert!(review.modal().is_some());
    assert_eq!(review.comparing().baseline(), ReviewCompareBaseline::Off);
    assert!(!review.comparing().active());
    assert!(
        screen.contains("◉ nothing: show the full change"),
        "{}",
        screen.text()
    );
    assert!(screen.contains("○ you last looked"), "{}", screen.text());
    // Nothing is saved while previewing.
    assert_eq!(saved(&scenario), before);

    // Esc puts back exactly what the menu opened on, at once (the deltas are kept).
    scenario.key("escape");
    let review = scenario.surface().review().expect("review");
    assert!(review.modal().is_none());
    assert_eq!(
        review.comparing().baseline(),
        ReviewCompareBaseline::LastLooked
    );
    assert_eq!(review.comparing().view(), CompareView::Full);
    assert_eq!(kind_of(document(&scenario), 101), Some(DeltaKind::Changed));
    assert_eq!(
        kind_of(document(&scenario), 102),
        Some(DeltaKind::RebasedOnly)
    );
    assert_eq!(saved(&scenario), before);

    // Enter keeps what was picked, and saves it.
    scenario.key("b");
    settled(&mut scenario);
    menu_to(&mut scenario, MenuChoice::OnlyChanged);
    scenario.key("space");
    settled(&mut scenario);
    assert!(scenario.surface().review().unwrap().modal().is_some());
    scenario.key("enter");
    let screen = settled(&mut scenario);
    let review = scenario.surface().review().expect("review");
    assert!(review.modal().is_none());
    assert!(review.comparing().only_changed());
    assert!(saved(&scenario).only_changed);
    // The option stays visible outside the menu.
    assert!(
        screen.contains("[only PRs with real changes]"),
        "{}",
        screen.text()
    );
}

#[test]
fn clicking_menu_rows_picks_them_in_place() {
    use crate::compare_menu::MenuChoice;
    let (_fixture, _backend, _id, mut scenario) = reopened_after_a_restack(TestBackend::default());
    scenario.key("b");
    let screen = settled(&mut scenario);
    let row = (0..screen.height())
        .find(|y| screen.row_text(*y).contains("[ ] Only what changed"))
        .unwrap_or_else(|| panic!("the option row: {}", screen.text()));
    let x = screen.column_of(row, "[ ] Only").expect("column");
    scenario.click(x + 1, row);
    let screen = settled(&mut scenario);
    let review = scenario.surface().review().expect("review");
    assert!(review.modal().is_some(), "{}", screen.text());
    assert_eq!(review.comparing().view(), CompareView::Changes);
    assert!(
        screen.contains("[x] Only what changed"),
        "{}",
        screen.text()
    );
    assert_eq!(menu_focus(&scenario), Some(MenuChoice::View));
    // A click outside the menu keeps the state and closes it.
    scenario.click(0, screen.height() - 1);
    settled(&mut scenario);
    let review = scenario.surface().review().expect("review");
    assert!(review.modal().is_none());
    assert_eq!(review.comparing().view(), CompareView::Changes);
}

/// The file panel's text: its rows, inside its border.
fn panel_text(scenario: &Scenario<ReviewApp>, screen: &Screen) -> Vec<String> {
    let area = scenario
        .surface()
        .review()
        .and_then(|review| review.body_area(crate::surface::FILES_PANE_ID))
        .expect("the file panel");
    (area.y..area.y + area.height)
        .map(|y| {
            screen
                .row_text(y)
                .chars()
                .skip(usize::from(area.x))
                .take(usize::from(area.width))
                .collect()
        })
        .collect()
}

/// Click the first cell of `needle` inside the file panel.
fn click_in_panel(scenario: &mut Scenario<ReviewApp>, screen: &Screen, needle: &str) {
    let area = scenario
        .surface()
        .review()
        .and_then(|review| review.body_area(crate::surface::FILES_PANE_ID))
        .expect("the file panel");
    let (x, y) = (area.y..area.y + area.height)
        .find_map(|y| {
            let x = screen.column_of(y, needle)?;
            (x >= area.x && x < area.right()).then_some((x, y))
        })
        .unwrap_or_else(|| panic!("no {needle} in the panel: {}", screen.text()));
    scenario.click(x, y);
}

fn tree_labels(scenario: &Scenario<ReviewApp>) -> Vec<String> {
    scenario
        .surface()
        .review()
        .and_then(|review| review.file_tree())
        .map(|tree| tree.items().iter().map(|item| item.label.clone()).collect())
        .unwrap_or_default()
}

#[test]
fn the_file_panel_says_what_changed_since_the_baseline_one_layer_at_a_time() {
    let (_fixture, _backend, _id, mut scenario) = reopened_after_a_restack(TestBackend::default());
    let screen = settled(&mut scenario);
    let panel = panel_text(&scenario, &screen).join("\n");
    // The legend names the comparison and what it found for the layer shown.
    assert!(panel.contains("Changes since you last looked"), "{panel}");
    assert!(panel.contains("1 of 3 files changed since you"), "{panel}");
    // The new file is marked; the unchanged ones are listed (the full view).
    let labels = tree_labels(&scenario);
    assert!(
        labels.iter().any(|label| label == "+ A restacked.txt"),
        "{labels:?}"
    );
    assert!(
        labels.iter().any(|label| label == "A layer_1.rs"),
        "{labels:?}"
    );

    // Only what changed: the unchanged files leave the panel as they leave the document.
    scenario.key("shift+l");
    let screen = settled(&mut scenario);
    let labels = tree_labels(&scenario);
    assert!(
        !labels.iter().any(|label| label.contains("layer_1.rs")),
        "{labels:?}"
    );
    let doc = document(&scenario);
    let bottom = source_of(doc, 101);
    assert!(doc.file_compare_hidden(&FileKey::new(bottom.clone(), None, "stack/layer_1.rs")));
    let panel = panel_text(&scenario, &screen).join("\n");
    assert!(panel.contains("2 files hidden: unchanged"), "{panel}");
    assert!(panel.contains("only what changed"), "{panel}");
    // A click on the legend shows them, dimmed, in both.
    click_in_panel(&mut scenario, &screen, "[show]");
    let screen = settled(&mut scenario);
    let labels = tree_labels(&scenario);
    assert!(
        labels.iter().any(|label| label.contains("layer_1.rs")),
        "{labels:?}"
    );
    assert!(document(&scenario).compare_files_shown(&bottom));
    let panel = panel_text(&scenario, &screen).join("\n");
    assert!(panel.contains("[hide]"), "{panel}");
    click_in_panel(&mut scenario, &screen, "[hide]");
    settled(&mut scenario);
    assert!(!document(&scenario).compare_files_shown(&bottom));
    scenario.key("shift+l");
    settled(&mut scenario);

    // A layer only rebased: the panel says plainly that nothing changed.
    scenario.key(".");
    let screen = settled(&mut scenario);
    let panel = panel_text(&scenario, &screen).join("\n");
    assert!(
        panel.contains("No changes since you last looked") && panel.contains("only rebased"),
        "{panel}"
    );
    assert!(panel.contains("2 files hidden"), "{panel}");
    let tree = scenario.surface().review().unwrap().file_tree().unwrap();
    assert_eq!(tree.file_count(), 0);
    assert_eq!(tree.hidden_files(), 2);
}

#[test]
fn the_file_panel_says_what_changed_with_every_layer_shown() {
    let (_fixture, _backend, _id, mut scenario) = reopened_after_a_restack(TestBackend::default());
    settled(&mut scenario);
    scenario.key("shift+o");
    let screen = settled(&mut scenario);
    let panel = panel_text(&scenario, &screen).join("\n");
    assert!(
        panel.contains("1 of 3 PRs changed · 2 rebased only"),
        "{panel}"
    );
    assert!(panel.contains("4 files hidden"), "{panel}");
    // Each group says what its comparison found; the rebased ones keep their row.
    let labels = tree_labels(&scenario);
    assert!(
        labels
            .iter()
            .any(|label| label.starts_with("#101") && label.contains("● +1 −0 · 1 file")),
        "{labels:?}"
    );
    assert!(
        labels
            .iter()
            .any(|label| label.starts_with("#102") && label.contains("○ rebased only · 2 hidden")),
        "{labels:?}"
    );
    // Only pull requests with real changes: the panel says what that hides, and shows it again.
    run(
        &mut scenario,
        "Compare: only pull requests with real changes",
    );
    let screen = settled(&mut scenario);
    let panel = panel_text(&scenario, &screen).join("\n");
    assert!(panel.contains("2 pull requests hidden"), "{panel}");
    assert!(
        !tree_labels(&scenario)
            .iter()
            .any(|label| label.starts_with("#102"))
    );
    click_in_panel(&mut scenario, &screen, "[show all]");
    settled(&mut scenario);
    assert!(
        !scenario
            .surface()
            .review()
            .unwrap()
            .comparing()
            .only_changed()
    );
    assert!(
        tree_labels(&scenario)
            .iter()
            .any(|label| label.starts_with("#102"))
    );

    // Narrow and short: the legend wraps inside the panel and its rows still take clicks.
    for (width, height) in [(64_u16, 18_u16), (100, 12), (70, 40)] {
        scenario.resize(width, height);
        let screen = settled(&mut scenario);
        if let Some(area) = scenario
            .surface()
            .review()
            .and_then(|review| review.body_area(crate::surface::FILES_PANE_ID))
        {
            assert!(area.right() <= width && area.bottom() <= height, "{area:?}");
            for index in 0..4 {
                if let Some(row) = scenario
                    .surface()
                    .review()
                    .and_then(|review| review.body_area(&format!("review.files.legend.{index}")))
                {
                    assert!(
                        row.x >= area.x && row.right() <= area.right(),
                        "{width}x{height}: {row:?} {area:?}\n{}",
                        screen.text()
                    );
                }
            }
        }
    }
}

#[test]
fn the_summary_rows_show_button_shows_hidden_files_in_the_document_and_the_panel() {
    let (_fixture, _backend, _id, mut scenario) = reopened_after_a_restack(TestBackend::default());
    settled(&mut scenario);
    scenario.key("shift+l");
    let screen = settled(&mut scenario);
    assert_eq!(
        scenario
            .surface()
            .review()
            .unwrap()
            .file_tree()
            .unwrap()
            .hidden_files(),
        2
    );
    let row = (0..screen.height())
        .find(|y| screen.row_text(*y).contains("[show 2 unchanged]"))
        .unwrap_or_else(|| panic!("the summary row: {}", screen.text()));
    let x = screen.column_of(row, "[show 2 unchanged]").expect("column");
    scenario.click(x + 1, row);
    // Rows measured before lay out again beside the files shown.
    let screen = settled(&mut scenario);
    assert!(screen.contains("[hide them]"), "{}", screen.text());
    let tree = scenario.surface().review().unwrap().file_tree().unwrap();
    assert_eq!(tree.hidden_files(), 0);
    assert!(
        tree.items()
            .iter()
            .any(|item| item.label.contains("layer_1.rs"))
    );
}
