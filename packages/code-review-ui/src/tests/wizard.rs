//! The new-review wizard through the app: every source type creates a durable review
//! from pickers and opens it, composite reviews collect several sources, and the pull request
//! and stack types explain a missing or failing provider while the other types keep working.

use super::app::{fixture, listed, open, settled};
use crate::app::{ReviewApp, RouteName};
use crate::entry::{Entry, NO_PROVIDER};
use crate::testing::backend::TestBackend;
use crate::testing::fake_provider::{FAKE_PROVIDER, FAKE_REPOSITORY, FakeProvider};
use crate::testing::fixture::{BASE_BRANCH, GitFixture, HEAD_BRANCH, UNICODE_FILE};
use crate::testing::harness::{Scenario, Screen};
use bcode_code_review_models::{
    REVIEW_SOURCE_ERROR_UNAUTHENTICATED, ReviewSourceKind, ReviewWorkspace,
};
use std::sync::Arc;

/// Open home and start the wizard.
fn wizard(fixture: &GitFixture, backend: &Arc<TestBackend>) -> Scenario<ReviewApp> {
    let (mut scenario, _) = open(fixture, backend, Entry::Home, 140, 40);
    scenario.key("n");
    settled(&mut scenario);
    assert_eq!(scenario.surface().route_name(), RouteName::NewReview);
    scenario
}

/// Pick the source type at `index` of step 1.
fn pick_type(scenario: &mut Scenario<ReviewApp>, index: usize) -> Screen {
    for _ in 0..index {
        scenario.key("down");
    }
    scenario.key("enter");
    settled(scenario)
}

fn step(scenario: &Scenario<ReviewApp>) -> usize {
    scenario.surface().wizard().expect("wizard").step_number()
}

/// Create from step 3 and return the workspace the review opened.
fn create(
    scenario: &mut Scenario<ReviewApp>,
    backend: &TestBackend,
    fixture: &GitFixture,
) -> ReviewWorkspace {
    assert_eq!(step(scenario), 3);
    scenario.key("enter");
    settled(scenario);
    assert_eq!(scenario.surface().route_name(), RouteName::Review);
    let review = scenario.surface().review().unwrap();
    assert!(!review.is_transient(), "the wizard creates durable reviews");
    let id = review.workspace().unwrap().id.clone();
    listed(backend, fixture)
        .into_iter()
        .find(|workspace| workspace.id == id)
        .expect("created workspace listed")
}

fn kinds(workspace: &ReviewWorkspace) -> Vec<ReviewSourceKind> {
    workspace
        .sources
        .iter()
        .map(|source| source.kind.clone())
        .collect()
}

#[test]
fn working_tree_with_counts_creates_and_opens_a_review() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default());
    let mut scenario = wizard(&fixture, &backend);
    let screen = scenario.render();
    for text in [
        "Source type",
        "Details",
        "Title & create",
        "Working tree",
        "Pull request",
    ] {
        assert!(screen.contains(text), "{text}:\n{}", screen.text());
    }
    let screen = pick_type(&mut scenario, 0);
    assert_eq!(step(&scenario), 2);
    assert!(screen.contains("Unstaged changes"));
    assert!(
        screen.contains("files · +"),
        "counts shown:\n{}",
        screen.text()
    );
    scenario.key("down");
    scenario.key("enter");
    let screen = settled(&mut scenario);
    assert_eq!(step(&scenario), 3);
    assert_eq!(
        scenario.surface().wizard().unwrap().picker().query(),
        "Staged changes"
    );
    assert!(screen.contains("Create review"));
    let created = create(&mut scenario, &backend, &fixture);
    assert_eq!(created.title, "Staged changes");
    assert_eq!(kinds(&created), [ReviewSourceKind::IndexStaged]);
    // Leaving the review lands on home, which lists it.
    scenario.key("q");
    settled(&mut scenario);
    assert_eq!(scenario.surface().route_name(), RouteName::Home);
    assert!(
        scenario
            .surface()
            .home()
            .unwrap()
            .row_labels()
            .contains(&"Staged changes".to_owned())
    );
}

#[test]
fn current_branch_prefills_the_detected_base_and_compare_picks_two_references() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default());
    let mut scenario = wizard(&fixture, &backend);
    pick_type(&mut scenario, 1);
    let current = scenario.surface().wizard().unwrap();
    assert_eq!(current.picker().query(), "origin/main");
    assert_eq!(current.picker().focused(), Some("origin/main"));
    scenario.key("enter");
    settled(&mut scenario);
    let created = create(&mut scenario, &backend, &fixture);
    assert_eq!(created.title, HEAD_BRANCH);
    // The detected base stays automatic.
    assert_eq!(
        kinds(&created),
        [ReviewSourceKind::CurrentBranch { base: None }]
    );

    let mut scenario = wizard(&fixture, &backend);
    pick_type(&mut scenario, 2);
    scenario.paste(BASE_BRANCH);
    settled(&mut scenario);
    assert_eq!(
        scenario.surface().wizard().unwrap().picker().focused(),
        Some(BASE_BRANCH)
    );
    scenario.key("enter");
    let screen = settled(&mut scenario);
    assert!(screen.contains(&format!("base {BASE_BRANCH}")));
    scenario.paste(HEAD_BRANCH);
    settled(&mut scenario);
    scenario.key("enter");
    settled(&mut scenario);
    let created = create(&mut scenario, &backend, &fixture);
    assert_eq!(
        kinds(&created),
        [ReviewSourceKind::BranchCompare {
            base_branch: BASE_BRANCH.to_owned(),
            head_branch: HEAD_BRANCH.to_owned(),
            merge_base: true,
        }]
    );
}

#[test]
fn typed_revisions_work_when_no_reference_matches() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default());
    let mut scenario = wizard(&fixture, &backend);
    pick_type(&mut scenario, 2);
    scenario.paste("HEAD~1");
    let screen = settled(&mut scenario);
    assert!(
        screen.contains("No matching references"),
        "{}",
        screen.text()
    );
    scenario.key("enter");
    settled(&mut scenario);
    scenario.paste(HEAD_BRANCH);
    settled(&mut scenario);
    scenario.key("enter");
    settled(&mut scenario);
    let created = create(&mut scenario, &backend, &fixture);
    assert_eq!(
        kinds(&created),
        [ReviewSourceKind::BranchCompare {
            base_branch: "HEAD~1".to_owned(),
            head_branch: HEAD_BRANCH.to_owned(),
            merge_base: true,
        }]
    );
    // The review opened against the typed base.
    assert!(
        scenario
            .surface()
            .review()
            .unwrap()
            .document()
            .is_some_and(|doc| !doc.is_empty())
    );
}

#[test]
fn commits_mark_single_ranges_and_selections() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default());
    let head = fixture.git(&["rev-parse", "HEAD"]).trim().to_owned();
    let base = fixture.git(&["rev-parse", BASE_BRANCH]).trim().to_owned();
    // The focused commit alone.
    let mut scenario = wizard(&fixture, &backend);
    let screen = pick_type(&mut scenario, 3);
    assert_eq!(
        scenario.surface().wizard().unwrap().picker().focused(),
        Some(head.as_str())
    );
    assert!(screen.contains(&head[..7]));
    scenario.key("enter");
    settled(&mut scenario);
    let created = create(&mut scenario, &backend, &fixture);
    assert_eq!(
        kinds(&created),
        [ReviewSourceKind::Commit { rev: head.clone() }]
    );

    // A contiguous range: mark the newest, move down, mark the range.
    let mut scenario = wizard(&fixture, &backend);
    pick_type(&mut scenario, 3);
    scenario.key("tab");
    scenario.key("down");
    scenario.key("shift+tab");
    settled(&mut scenario);
    assert_eq!(scenario.surface().wizard().unwrap().marked().len(), 2);
    scenario.key("enter");
    settled(&mut scenario);
    let created = create(&mut scenario, &backend, &fixture);
    assert_eq!(
        kinds(&created),
        [ReviewSourceKind::Commits {
            revs: vec![base, head],
        }]
    );

    // A search narrows the log; unmarking with tab.
    let mut scenario = wizard(&fixture, &backend);
    pick_type(&mut scenario, 3);
    scenario.key("tab");
    scenario.key("tab");
    assert_eq!(
        scenario.surface().wizard().unwrap().marked(),
        Vec::<String>::new()
    );
    scenario.paste("no commit says this");
    let screen = settled(&mut scenario);
    assert!(screen.contains("No matching commits"), "{}", screen.text());
}

#[test]
fn files_pick_whole_files_marked_files_and_line_ranges() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default());
    let mut scenario = wizard(&fixture, &backend);
    pick_type(&mut scenario, 6);
    scenario.paste(&format!("{UNICODE_FILE}:2-4"));
    let screen = settled(&mut scenario);
    assert!(screen.contains(UNICODE_FILE), "{}", screen.text());
    scenario.key("enter");
    settled(&mut scenario);
    let created = create(&mut scenario, &backend, &fixture);
    assert_eq!(
        kinds(&created),
        [ReviewSourceKind::FileRange {
            path: UNICODE_FILE.to_owned(),
            start: 2,
            end: 4,
        }]
    );

    // Two marked files become two sources.
    let mut scenario = wizard(&fixture, &backend);
    pick_type(&mut scenario, 6);
    let first = scenario
        .surface()
        .wizard()
        .unwrap()
        .picker()
        .focused()
        .unwrap()
        .to_owned();
    scenario.key("tab");
    scenario.key("down");
    scenario.key("tab");
    scenario.key("enter");
    settled(&mut scenario);
    let wizard = scenario.surface().wizard().unwrap();
    assert_eq!(wizard.source_labels().len(), 2);
    assert_eq!(wizard.source_labels()[0], first);
}

#[test]
fn composite_reviews_add_and_remove_sources() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default());
    let mut scenario = wizard(&fixture, &backend);
    pick_type(&mut scenario, 0);
    scenario.key("enter");
    settled(&mut scenario);
    // A typed title survives adding another source.
    scenario.paste(" and more");
    scenario.key("down");
    scenario.key("enter");
    settled(&mut scenario);
    assert_eq!(step(&scenario), 1);
    pick_type(&mut scenario, 0);
    scenario.key("down");
    scenario.key("enter");
    let screen = settled(&mut scenario);
    let wizard = scenario.surface().wizard().unwrap();
    assert_eq!(
        wizard.source_labels(),
        ["Unstaged changes", "Staged changes"]
    );
    assert_eq!(wizard.picker().query(), "Unstaged changes and more");
    assert!(screen.contains("Sources (2)"));
    // Removing a source keeps the rest; esc on step 1 with sources returns to step 3.
    scenario.key("end");
    scenario.key("enter");
    settled(&mut scenario);
    assert_eq!(
        scenario.surface().wizard().unwrap().source_labels(),
        ["Unstaged changes"]
    );
    scenario.key("esc");
    settled(&mut scenario);
    assert_eq!(step(&scenario), 1);
    scenario.key("esc");
    settled(&mut scenario);
    assert_eq!(step(&scenario), 3);
    let created = create(&mut scenario, &backend, &fixture);
    assert_eq!(created.title, "Unstaged changes and more");
    assert_eq!(kinds(&created), [ReviewSourceKind::WorkingTreeUnstaged]);
}

#[test]
fn pull_requests_and_stacks_come_from_the_provider() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default().with_provider(FakeProvider::new(fixture.root())));
    let mut scenario = wizard(&fixture, &backend);
    let screen = pick_type(&mut scenario, 4);
    assert!(screen.contains("Review requested"), "{}", screen.text());
    // Review requested from me first.
    assert_eq!(
        scenario.surface().wizard().unwrap().picker().focused(),
        Some("102")
    );
    scenario.key("enter");
    settled(&mut scenario);
    let created = create(&mut scenario, &backend, &fixture);
    assert_eq!(created.title, "#102 Layer 102");
    assert_eq!(
        kinds(&created),
        [ReviewSourceKind::PullRequest {
            provider: FAKE_PROVIDER.to_owned(),
            repository: FAKE_REPOSITORY.to_owned(),
            number: 102,
        }]
    );
    assert!(
        scenario
            .surface()
            .review()
            .unwrap()
            .document()
            .is_some_and(|doc| !doc.is_empty())
    );

    // Stack: the current branch's pull request first, then the mode.
    let mut scenario = wizard(&fixture, &backend);
    let screen = pick_type(&mut scenario, 5);
    assert!(screen.contains("Your branch"));
    assert_eq!(
        scenario.surface().wizard().unwrap().picker().focused(),
        Some("100")
    );
    scenario.key("enter");
    let screen = settled(&mut scenario);
    for text in [
        "Whole stack",
        "Step through every layer",
        "One layer",
        "#103 Layer 103",
    ] {
        assert!(screen.contains(text), "{text}:\n{}", screen.text());
    }
    scenario.key("down");
    scenario.key("enter");
    settled(&mut scenario);
    let created = create(&mut scenario, &backend, &fixture);
    let layer = |number| ReviewSourceKind::StackLayer {
        provider: FAKE_PROVIDER.to_owned(),
        repository: FAKE_REPOSITORY.to_owned(),
        number,
    };
    assert_eq!(
        kinds(&created),
        [layer(100), layer(101), layer(102), layer(103)]
    );

    // Whole stack and a single layer.
    let mut scenario = wizard(&fixture, &backend);
    pick_type(&mut scenario, 5);
    scenario.key("enter");
    settled(&mut scenario);
    scenario.key("enter");
    settled(&mut scenario);
    let created = create(&mut scenario, &backend, &fixture);
    assert_eq!(
        kinds(&created),
        [ReviewSourceKind::Stack {
            provider: FAKE_PROVIDER.to_owned(),
            repository: FAKE_REPOSITORY.to_owned(),
            top: 103,
        }]
    );
    let mut scenario = wizard(&fixture, &backend);
    pick_type(&mut scenario, 5);
    scenario.key("enter");
    settled(&mut scenario);
    scenario.key("end");
    scenario.key("enter");
    settled(&mut scenario);
    let created = create(&mut scenario, &backend, &fixture);
    assert_eq!(kinds(&created), [layer(103)]);
}

#[test]
fn stacks_over_the_limit_warn_and_the_warnings_can_be_dismissed() {
    use bcode_code_review_models::{ReviewStackLimit, StackOverLimit};
    let fixture = fixture();
    let backend = Arc::new(
        TestBackend::default()
            .with_provider(FakeProvider::new(fixture.root()))
            .with_stack_limit(ReviewStackLimit {
                max_layers: 2,
                over_limit: StackOverLimit::Warn,
            }),
    );
    let mut scenario = wizard(&fixture, &backend);
    pick_type(&mut scenario, 5);
    scenario.key("enter");
    let screen = settled(&mut scenario);
    let warning = "4 layers, over the 2-layer limit";
    assert!(screen.contains(warning), "{}", screen.text());
    // The warning does not block the choice; `enter` on it dismisses it.
    assert_eq!(
        scenario.surface().wizard().unwrap().picker().focused(),
        Some("stack-limit-warning")
    );
    scenario.key("enter");
    let screen = settled(&mut scenario);
    assert!(!screen.contains(warning), "{}", screen.text());
    assert_eq!(
        scenario.surface().wizard().unwrap().picker().focused(),
        Some("whole")
    );
    scenario.key("enter");
    settled(&mut scenario);
    let created = create(&mut scenario, &backend, &fixture);
    assert_eq!(created.sources.len(), 1);
    // Every layer is in the review, and a warning that stays until closed says so.
    let toast = "Stack of 4 layers";
    assert!(
        scenario
            .surface()
            .review()
            .unwrap()
            .toasts()
            .contains(&toast),
        "{:?}",
        scenario.surface().review().unwrap().toasts()
    );
    let screen = settled(&mut scenario);
    let close = screen.find("×").expect("toast close button");
    scenario.click(close.x, close.y);
    settled(&mut scenario);
    assert!(
        !scenario
            .surface()
            .review()
            .unwrap()
            .toasts()
            .contains(&toast),
        "{:?}",
        scenario.surface().review().unwrap().toasts()
    );
}

#[test]
fn missing_or_failing_providers_are_explained_and_other_types_still_work() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default());
    let mut scenario = wizard(&fixture, &backend);
    let screen = pick_type(&mut scenario, 4);
    assert!(
        screen.contains("No pull request provider"),
        "{}",
        screen.text()
    );
    let message = scenario
        .surface()
        .wizard()
        .unwrap()
        .picker()
        .message()
        .unwrap();
    assert_eq!(message.body, NO_PROVIDER);
    // Back to step 1: another type still works.
    scenario.key("esc");
    settled(&mut scenario);
    assert_eq!(step(&scenario), 1);
    // The pull request type stays focused; the working tree is four rows up.
    scenario.keys(&["up", "up", "up", "up"]);
    pick_type(&mut scenario, 0);
    scenario.key("enter");
    settled(&mut scenario);
    assert_eq!(step(&scenario), 3);

    let backend = Arc::new(
        TestBackend::default().with_provider(
            FakeProvider::new(fixture.root())
                .failing_pulls(REVIEW_SOURCE_ERROR_UNAUTHENTICATED, "no token"),
        ),
    );
    let mut scenario = wizard(&fixture, &backend);
    let screen = pick_type(&mut scenario, 5);
    assert!(
        screen.contains("Could not list pull requests"),
        "{}",
        screen.text()
    );
    let message = scenario
        .surface()
        .wizard()
        .unwrap()
        .picker()
        .message()
        .unwrap();
    assert!(message.error && message.body.contains("not signed in"));
    // Esc twice leaves the wizard for home.
    scenario.key("esc");
    scenario.key("esc");
    settled(&mut scenario);
    assert_eq!(scenario.surface().route_name(), RouteName::Home);
}

#[test]
fn the_wizard_degrades_at_narrow_sizes() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default());
    let mut scenario = wizard(&fixture, &backend);
    for (width, height) in [(40, 12), (20, 6), (8, 3), (0, 0), (140, 40)] {
        scenario.resize(width, height);
        let screen = settled(&mut scenario);
        if width >= 40 {
            assert!(
                screen.contains("Working tree"),
                "{width}x{height}:\n{}",
                screen.text()
            );
        }
    }
    // Clicking a row picks it.
    let screen = settled(&mut scenario);
    let point = screen.find("Commits").expect("commits row");
    scenario.click(point.x, point.y);
    settled(&mut scenario);
    assert_eq!(step(&scenario), 2);
}

#[test]
fn enter_typed_before_the_listing_answers_picks_from_the_new_answer() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default());
    let mut scenario = wizard(&fixture, &backend);
    pick_type(&mut scenario, 2);
    // The unfiltered listing is on screen; type a reference and press enter at once.
    scenario.paste(HEAD_BRANCH);
    scenario.key("enter");
    let screen = settled(&mut scenario);
    assert!(
        screen.contains(&format!("base {HEAD_BRANCH}")),
        "{}",
        screen.text()
    );
    // Another key before the answer cancels a deferred enter.
    scenario.paste(BASE_BRANCH);
    scenario.key("enter");
    scenario.key("backspace");
    let screen = settled(&mut scenario);
    assert!(
        screen.contains(&format!("base {HEAD_BRANCH}")),
        "still choosing the head: {}",
        screen.text()
    );
    assert_eq!(step(&scenario), 2);
    // Going back to step 1 keeps the type that was being configured focused.
    scenario.keys(&["esc", "esc"]);
    settled(&mut scenario);
    assert_eq!(step(&scenario), 1);
    assert_eq!(
        scenario.surface().wizard().unwrap().picker().focused(),
        Some(crate::wizard::SourceType::CompareBranches.id())
    );
}

/// Clicking an option picks it, whether or not it is the active row, wherever the picker sits.
#[test]
fn clicking_options_picks_them_on_every_step() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default());
    let mut scenario = wizard(&fixture, &backend);
    let screen = settled(&mut scenario);
    // The active first row.
    let point = screen.find("Working tree").expect("row");
    scenario.click(point.x, point.y);
    let screen = settled(&mut scenario);
    assert_eq!(step(&scenario), 2);
    // A row of the smaller step-2 picker, below its first row.
    let point = screen.find("Staged changes").expect("staged row");
    scenario.click(point.x, point.y);
    settled(&mut scenario);
    assert_eq!(step(&scenario), 3);
    assert_eq!(
        scenario.surface().wizard().unwrap().picker().query(),
        "Staged changes"
    );
}

/// Outside a git repository the working-tree counts say they are unavailable instead of
/// counting forever.
#[test]
fn working_tree_counts_outside_a_repository_are_unavailable() {
    let directory = tempfile::tempdir().expect("directory");
    let backend = Arc::new(TestBackend::default());
    let app = ReviewApp::new(
        backend.services(),
        directory.path().to_path_buf(),
        crate::config::ReviewViewConfig::default(),
        Entry::Home,
    );
    let mut scenario = Scenario::new(app, 140, 40);
    settled(&mut scenario);
    scenario.key("n");
    settled(&mut scenario);
    let screen = pick_type(&mut scenario, 0);
    assert_eq!(step(&scenario), 2);
    assert!(screen.contains("counts unavailable"), "{}", screen.text());
    assert!(!screen.contains("counting"), "{}", screen.text());
}
