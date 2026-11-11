//! The V2 app (spec 7.1, 7.2, 7.5): home groups, filtering, suggestions from fixture repository
//! state (with and without a review-source provider), continue-latest, rename, archive and
//! restore, route transitions that keep home and review state, entries that resolve before the
//! review opens, and saving a transient review.

use crate::v2::testing::fake_provider::{
    FAKE_PROVIDER, FAKE_REPOSITORY, FAKE_REQUESTED, FakeProvider,
};
use crate::v2::testing::fixture::{BASE_BRANCH, FixtureSpec, GitFixture, HEAD_BRANCH};
use crate::v2::testing::harness::{Scenario, Screen};
use crate::v2::testing::local_backend::LocalBackend;
use crate::v2::tui::app::{ReviewAppV2, RouteName};
use crate::v2::tui::config::ReviewViewConfig;
use crate::v2::tui::entry::{Entry, TRANSIENT_WORKSPACE_ID};
use crate::v2::tui::home::model::{RowKey, SuggestionAction};
use crate::v2::tui::notes::NoteScope;
use bcode_code_review_models::{
    CreateReviewWorkspaceRequest, CreateReviewWorkspaceResponse, GetReviewWorkspaceRequest,
    GetReviewWorkspaceResponse, ListDraftsRequest, ListDraftsResponse, ListReviewWorkspacesRequest,
    ListReviewWorkspacesResponse, OP_REVIEW_WORKSPACE_CREATE, OP_REVIEW_WORKSPACE_GET,
    OP_REVIEW_WORKSPACE_LIST, OP_REVIEW_WORKSPACE_UPDATE, REVIEW_SOURCE_ERROR_UNAUTHENTICATED,
    ReviewSource, ReviewSourceKind, ReviewWorkspace, ReviewedRevision,
    UpdateReviewWorkspaceRequest,
};
use bcode_plugin_sdk::tui::PluginTuiAction;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// A fixture whose `origin/HEAD` points at the base branch, so the current branch has a base.
pub(super) fn fixture() -> GitFixture {
    let fixture = GitFixture::generate(FixtureSpec::small());
    fixture.run(&["update-ref", "refs/remotes/origin/main", BASE_BRANCH]);
    fixture.run(&[
        "symbolic-ref",
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/main",
    ]);
    fixture
}

pub(super) fn app(fixture: &GitFixture, backend: &Arc<LocalBackend>, entry: Entry) -> ReviewAppV2 {
    ReviewAppV2::new(
        Arc::clone(backend) as Arc<dyn crate::v2::tui::backend::ReviewBackend>,
        fixture.root().to_path_buf(),
        ReviewViewConfig::default(),
        entry,
    )
    .with_scroll_motion(Duration::ZERO)
}

/// Frame until the app issues no more work and the screen stops changing.
pub(super) fn settled(scenario: &mut Scenario<ReviewAppV2>) -> Screen {
    let mut previous = scenario.frame();
    for _ in 0..64 {
        let screen = scenario.frame();
        let app = scenario.surface();
        let idle = scenario.host().pending_tasks() == 0
            && app.home().is_none_or(|home| home.loading() == 0)
            && app
                .review()
                .is_none_or(|review| review.outstanding_requests() == 0);
        if idle && screen.text() == previous.text() {
            return screen;
        }
        previous = screen;
    }
    panic!("app did not settle:\n{}", previous.text());
}

pub(super) fn open(
    fixture: &GitFixture,
    backend: &Arc<LocalBackend>,
    entry: Entry,
    width: u16,
    height: u16,
) -> (Scenario<ReviewAppV2>, Screen) {
    let mut scenario = Scenario::new(app(fixture, backend, entry), width, height);
    let screen = settled(&mut scenario);
    (scenario, screen)
}

pub(super) fn create(
    backend: &LocalBackend,
    fixture: &GitFixture,
    title: &str,
    sources: Vec<ReviewSourceKind>,
) -> ReviewWorkspace {
    let response: CreateReviewWorkspaceResponse = backend.call_now(
        OP_REVIEW_WORKSPACE_CREATE,
        &CreateReviewWorkspaceRequest {
            repo_path: fixture.root().to_path_buf(),
            title: Some(title.to_owned()),
            sources: sources
                .into_iter()
                .enumerate()
                .map(|(index, kind)| ReviewSource {
                    id: format!("source-{}", index + 1),
                    label: kind.label(),
                    kind,
                    included: true,
                })
                .collect(),
        },
    );
    response.workspace
}

pub(super) fn update(backend: &LocalBackend, workspace: &ReviewWorkspace) {
    let _: serde_json::Value = backend.call_now(
        OP_REVIEW_WORKSPACE_UPDATE,
        &UpdateReviewWorkspaceRequest {
            repo_path: workspace.repo_root.clone(),
            workspace: workspace.clone(),
        },
    );
}

pub(super) fn stored(backend: &LocalBackend, fixture: &GitFixture, id: &str) -> ReviewWorkspace {
    let response: GetReviewWorkspaceResponse = backend.call_now(
        OP_REVIEW_WORKSPACE_GET,
        &GetReviewWorkspaceRequest {
            repo_path: fixture.root().to_path_buf(),
            workspace_id: id.to_owned(),
        },
    );
    response.workspace.expect("stored workspace")
}

pub(super) fn listed(backend: &LocalBackend, fixture: &GitFixture) -> Vec<ReviewWorkspace> {
    let response: ListReviewWorkspacesResponse = backend.call_now(
        OP_REVIEW_WORKSPACE_LIST,
        &ListReviewWorkspacesRequest {
            repo_path: fixture.root().to_path_buf(),
            include_archived: true,
        },
    );
    response.workspaces
}

fn rows(scenario: &Scenario<ReviewAppV2>) -> Vec<String> {
    scenario.surface().home().expect("home").row_labels()
}

fn closed(scenario: &Scenario<ReviewAppV2>) -> bool {
    scenario
        .actions()
        .iter()
        .any(|action| matches!(action, PluginTuiAction::Close { .. }))
}

fn rev_parse(fixture: &GitFixture, rev: &str) -> String {
    fixture.git(&["rev-parse", rev]).trim().to_owned()
}

#[test]
fn home_groups_reviews_collapses_archived_and_restores() {
    let fixture = fixture();
    let backend = Arc::new(LocalBackend::default());
    create(
        &backend,
        &fixture,
        "Alpha",
        vec![ReviewSourceKind::LastCommit],
    );
    create(&backend, &fixture, "Beta", Vec::new());
    let mut gamma = create(
        &backend,
        &fixture,
        "Gamma",
        vec![ReviewSourceKind::IndexStaged],
    );
    gamma.archived_at_ms = Some(1);
    update(&backend, &gamma);

    let (mut scenario, screen) = open(&fixture, &backend, Entry::Home, 160, 40);
    assert_eq!(scenario.surface().route_name(), RouteName::Home);
    assert_eq!(
        rows(&scenario),
        [
            "Needs attention (1)",
            "Beta",
            "In progress (1)",
            "Alpha",
            "+Archived (1)"
        ]
    );
    for text in [
        "Code reviews",
        "Needs attention",
        "In progress",
        "Alpha",
        "Beta",
        "Preview",
    ] {
        assert!(screen.contains(text), "{text} missing:\n{}", screen.text());
    }
    assert!(
        !screen.contains("Gamma"),
        "archived reviews start collapsed"
    );
    // The first review row is selected and previewed.
    assert_eq!(
        scenario.surface().home().unwrap().selected(),
        Some(&RowKey::Review(
            listed(&backend, &fixture)
                .iter()
                .find(|workspace| workspace.title == "Beta")
                .unwrap()
                .id
                .clone()
        ))
    );
    // Unfold the archived group from its heading.
    scenario.key("shift+g");
    scenario.key("enter");
    let screen = settled(&mut scenario);
    assert!(screen.contains("Gamma"));
    // An archived review does not open; restoring it moves it back.
    scenario.key("shift+g");
    scenario.key("enter");
    assert_eq!(scenario.surface().route_name(), RouteName::Home);
    assert!(
        scenario
            .surface()
            .home()
            .unwrap()
            .status()
            .is_some_and(|status| status.contains("archived"))
    );
    scenario.key("shift+r");
    settled(&mut scenario);
    assert!(
        stored(&backend, &fixture, &gamma.id)
            .archived_at_ms
            .is_none()
    );
    assert_eq!(
        rows(&scenario),
        [
            "Needs attention (1)",
            "Beta",
            "In progress (2)",
            "Gamma",
            "Alpha"
        ]
    );
    assert!(
        scenario
            .surface()
            .home()
            .unwrap()
            .toasts()
            .iter()
            .any(|toast| toast.contains("restored"))
    );
}

#[test]
fn home_filter_matches_titles_and_sources_and_narrow_screens_degrade() {
    let fixture = fixture();
    let backend = Arc::new(LocalBackend::default());
    create(
        &backend,
        &fixture,
        "Parser cleanup",
        vec![ReviewSourceKind::LastCommit],
    );
    create(
        &backend,
        &fixture,
        "Release notes",
        vec![ReviewSourceKind::IndexStaged],
    );
    // Wide graphemes and combining marks in a title.
    create(
        &backend,
        &fixture,
        "漢字レビュー 🙂 e\u{301}",
        vec![ReviewSourceKind::LastCommit],
    );
    let (mut scenario, screen) = open(&fixture, &backend, Entry::Home, 140, 40);
    assert!(screen.contains("漢字レビュー"));
    scenario.key("/");
    scenario.paste("staged");
    let screen = settled(&mut scenario);
    assert_eq!(rows(&scenario), ["In progress (1)", "Release notes"]);
    assert!(screen.contains("1 of 3"));
    // Typing never reaches home actions.
    scenario.key("q");
    let screen = settled(&mut scenario);
    assert_eq!(scenario.surface().route_name(), RouteName::Home);
    assert_eq!(rows(&scenario), Vec::<String>::new());
    assert!(screen.contains("No matching reviews"));
    scenario.key("esc");
    settled(&mut scenario);
    assert_eq!(rows(&scenario).len(), 4);
    scenario.paste("漢字");
    assert_eq!(
        rows(&scenario).len(),
        4,
        "the filter is not focused any more"
    );

    // Narrow: the preview and secondary columns drop, and nothing is cut mid-glyph.
    for (width, height) in [(60, 30), (31, 12), (12, 5), (2, 3), (1, 1)] {
        scenario.resize(width, height);
        let screen = settled(&mut scenario);
        assert!(!screen.contains("Preview"), "{width}x{height}");
        if height >= 30 {
            assert!(
                screen.contains("Parser"),
                "{width}x{height}:\n{}",
                screen.text()
            );
        }
    }
}

#[test]
fn suggestions_follow_repository_state_and_the_provider() {
    let fixture = fixture();
    // Without a provider: uncommitted changes, the branch ahead of its base, and the wizard.
    let backend = Arc::new(LocalBackend::default());
    let (scenario, screen) = open(&fixture, &backend, Entry::Home, 160, 40);
    assert_eq!(
        scenario.surface().home().unwrap().suggestion_titles(),
        ["Uncommitted changes", "Branch main", "New review…"]
    );
    assert!(screen.contains("1 commit ahead of origin/main"));
    assert!(screen.contains("Start a review"));

    // With a provider: the stack of the current branch's pull request and review requests.
    let backend =
        Arc::new(LocalBackend::default().with_provider(FakeProvider::new(fixture.root())));
    // A review whose head moved since its recorded pass.
    let mut moved = create(
        &backend,
        &fixture,
        "Old pass",
        vec![ReviewSourceKind::LastCommit],
    );
    moved.reviewed_revisions = BTreeMap::from([(
        "source-1".to_owned(),
        ReviewedRevision {
            reviewed_base_oid: None,
            reviewed_head_oid: Some(rev_parse(&fixture, BASE_BRANCH)),
            recorded_at_ms: Some(1),
        },
    )]);
    update(&backend, &moved);
    let (mut scenario, screen) = open(&fixture, &backend, Entry::Home, 160, 40);
    let home = scenario.surface().home().unwrap();
    assert_eq!(
        home.suggestion_titles(),
        [
            "Uncommitted changes",
            "Branch main",
            "Stack of #100 (4 layers)",
            &format!("Review requested: #{FAKE_REQUESTED}"),
            "New commits: Old pass",
            "New review…",
        ]
    );
    assert!(home.stats(&moved.id).is_some_and(|stats| stats.new_commits));
    assert_eq!(rows(&scenario)[0], "Needs attention (1)");
    assert!(screen.contains("Stack of #100"));
    // Starting the review-requested suggestion (key 4) opens that pull request.
    assert_eq!(
        home.suggestions()[3].action,
        SuggestionAction::Start(Entry::Sources {
            title: Some(format!("#{FAKE_REQUESTED} Layer {FAKE_REQUESTED}")),
            sources: vec![ReviewSourceKind::PullRequest {
                provider: FAKE_PROVIDER.to_owned(),
                repository: FAKE_REPOSITORY.to_owned(),
                number: FAKE_REQUESTED,
            }],
        })
    );
    scenario.key("4");
    settled(&mut scenario);
    let review = scenario.surface().review().expect("review route");
    assert!(review.document().is_some_and(|doc| !doc.is_empty()));
    assert!(
        backend
            .provider()
            .unwrap()
            .calls()
            .contains(&format!("review_source.pull.fetch #{FAKE_REQUESTED}"))
    );

    // A failing provider (no credentials) keeps every local suggestion and says why.
    let backend = Arc::new(
        LocalBackend::default().with_provider(
            FakeProvider::new(fixture.root())
                .failing_pulls(REVIEW_SOURCE_ERROR_UNAUTHENTICATED, "token missing"),
        ),
    );
    let (scenario, screen) = open(&fixture, &backend, Entry::Home, 160, 40);
    assert_eq!(
        scenario.surface().home().unwrap().suggestion_titles(),
        ["Uncommitted changes", "Branch main", "New review…"]
    );
    assert!(screen.contains("not signed in"), "{}", screen.text());
}

#[test]
fn continue_latest_and_exit_return_home_keeping_state() {
    let fixture = fixture();
    let backend = Arc::new(LocalBackend::default());
    create(
        &backend,
        &fixture,
        "Older",
        vec![ReviewSourceKind::IndexStaged],
    );
    let latest = create(
        &backend,
        &fixture,
        "Latest",
        vec![ReviewSourceKind::LastCommit],
    );
    let (mut scenario, _) = open(&fixture, &backend, Entry::Home, 140, 40);
    // Home keeps its filter while a review is open.
    scenario.key("/");
    scenario.paste("e");
    scenario.key("enter");
    scenario.key("c");
    let screen = settled(&mut scenario);
    assert_eq!(scenario.surface().route_name(), RouteName::Review);
    assert!(screen.contains("Latest"));
    let review = scenario.surface().review().unwrap();
    assert_eq!(review.workspace().unwrap().id, latest.id);
    assert!(!review.is_transient());
    // Mark the first file viewed and the pass complete; leaving saves both.
    scenario.key("v");
    settled(&mut scenario);
    scenario.key("shift+m");
    assert!(
        scenario
            .surface()
            .review()
            .unwrap()
            .toasts()
            .contains(&"Review pass recorded")
    );
    scenario.key("q");
    let screen = settled(&mut scenario);
    assert_eq!(scenario.surface().route_name(), RouteName::Home);
    assert!(!closed(&scenario), "leaving a review returns home");
    assert!(screen.contains("Code reviews"));
    let saved = stored(&backend, &fixture, &latest.id);
    assert_eq!(saved.viewed_files.len(), 1);
    assert_eq!(
        saved
            .reviewed_revisions
            .get("source-1")
            .and_then(|revision| revision.reviewed_head_oid.clone()),
        Some(rev_parse(&fixture, HEAD_BRANCH))
    );
    // Home reloaded (the viewed count shows) and kept its filter.
    assert!(screen.contains("1/"), "{}", screen.text());
    assert!(screen.contains("2 of 2"), "{}", screen.text());
    // Reopening restores the review state.
    scenario.key("c");
    settled(&mut scenario);
    let doc = scenario.surface().review().unwrap().document().unwrap();
    assert_eq!(doc.viewed_progress().viewed, 1);
    assert_eq!(scenario.surface().opened_reviews(), 2);
    // `q` leaves the review; on home `esc` clears the filter and `q` closes the app.
    scenario.key("q");
    settled(&mut scenario);
    scenario.key("esc");
    settled(&mut scenario);
    assert_eq!(rows(&scenario).len(), 3);
    assert!(!closed(&scenario));
    scenario.key("q");
    assert!(closed(&scenario));
}

#[test]
fn rename_and_archive_update_the_store_and_the_table() {
    let fixture = fixture();
    let backend = Arc::new(LocalBackend::default());
    let review = create(
        &backend,
        &fixture,
        "Draft",
        vec![ReviewSourceKind::LastCommit],
    );
    let (mut scenario, _) = open(&fixture, &backend, Entry::Home, 140, 40);
    scenario.key("r");
    scenario.paste(" two");
    scenario.key("enter");
    let screen = settled(&mut scenario);
    assert_eq!(stored(&backend, &fixture, &review.id).title, "Draft two");
    assert!(screen.contains("Draft two"));
    // An empty title is refused.
    scenario.key("r");
    for _ in 0.."Draft two".len() {
        scenario.key("backspace");
    }
    scenario.key("enter");
    assert!(
        scenario
            .surface()
            .home()
            .unwrap()
            .status()
            .is_some_and(|status| status.contains("title"))
    );
    scenario.key("esc");
    scenario.key("x");
    settled(&mut scenario);
    assert!(
        stored(&backend, &fixture, &review.id)
            .archived_at_ms
            .is_some()
    );
    assert_eq!(rows(&scenario), ["+Archived (1)"]);
}

#[test]
fn entries_resolve_pull_requests_stacks_and_resume_before_opening() {
    let fixture = fixture();
    let backend =
        Arc::new(LocalBackend::default().with_provider(FakeProvider::new(fixture.root())));
    let pr = Entry::PullRequest {
        reference: format!("https://example.com/{FAKE_REPOSITORY}/pull/101"),
    };
    let (scenario, screen) = open(&fixture, &backend, pr, 140, 40);
    let review = scenario.surface().review().expect("review route");
    assert!(review.is_transient());
    assert!(screen.contains("#101 Layer 101"), "{}", screen.text());

    let (scenario, _) = open(
        &fixture,
        &backend,
        Entry::Stack { reference: None },
        140,
        40,
    );
    let workspace = scenario.surface().review().unwrap().workspace().unwrap();
    assert_eq!(
        workspace.sources[0].kind,
        ReviewSourceKind::Stack {
            provider: FAKE_PROVIDER.to_owned(),
            repository: FAKE_REPOSITORY.to_owned(),
            top: 103,
        }
    );

    let saved = create(
        &backend,
        &fixture,
        "Resumable",
        vec![ReviewSourceKind::LastCommit],
    );
    let (scenario, _) = open(
        &fixture,
        &backend,
        Entry::Resume { workspace_id: None },
        140,
        40,
    );
    assert_eq!(
        scenario.surface().review().unwrap().workspace().unwrap().id,
        saved.id
    );

    // `esc` while an entry resolves abandons it for home.
    let entry = Entry::Stack { reference: None };
    let mut scenario = Scenario::new(app(&fixture, &backend, entry), 140, 40);
    let screen = scenario.render();
    assert!(
        screen.contains("Finding the current branch"),
        "{}",
        screen.text()
    );
    scenario.key("esc");
    settled(&mut scenario);
    assert_eq!(scenario.surface().route_name(), RouteName::Home);

    // Without a provider the pull request entry explains what is missing; esc goes home.
    let backend = Arc::new(LocalBackend::default());
    let entry = Entry::PullRequest {
        reference: "7".to_owned(),
    };
    let (mut scenario, screen) = open(&fixture, &backend, entry, 140, 40);
    assert_eq!(scenario.surface().route_name(), RouteName::Opening);
    assert!(
        scenario
            .surface()
            .opening_error()
            .is_some_and(|error| error.contains("review-source provider"))
    );
    assert!(screen.contains("Could not open the review"));
    scenario.key("esc");
    settled(&mut scenario);
    assert_eq!(scenario.surface().route_name(), RouteName::Home);
}

#[test]
fn saving_a_transient_review_makes_it_durable_with_its_drafts() {
    let fixture = fixture();
    let backend = Arc::new(LocalBackend::default());
    let entry = Entry::Sources {
        title: None,
        sources: vec![ReviewSourceKind::LastCommit],
    };
    let (mut scenario, screen) = open(&fixture, &backend, entry, 140, 40);
    assert!(scenario.surface().review().unwrap().is_transient());
    assert!(screen.contains("Last commit"));
    // A review-level comment before saving.
    scenario.key("shift+c");
    scenario.paste("keep me");
    scenario.key("ctrl+s");
    settled(&mut scenario);
    scenario.key("v");
    settled(&mut scenario);
    scenario.key("ctrl+s");
    settled(&mut scenario);
    let review = scenario.surface().review().unwrap();
    assert!(!review.is_transient());
    assert!(review.toasts().contains(&"Review saved"));
    let id = review.workspace().unwrap().id.clone();
    assert_ne!(id, TRANSIENT_WORKSPACE_ID);
    let saved = stored(&backend, &fixture, &id);
    assert_eq!(saved.title, "Last commit");
    assert_eq!(saved.viewed_files.len(), 1);
    let drafts = |workspace: &ReviewWorkspace| {
        let scope = NoteScope::for_workspace(workspace);
        let response: ListDraftsResponse = backend.call_now(
            crate::v2::tui::notes::OP_DRAFT_LIST,
            &ListDraftsRequest {
                repo_path: scope.repo_path,
                target: scope.target,
                scope: scope.scope,
            },
        );
        response
            .drafts
            .into_iter()
            .map(|draft| draft.body)
            .collect::<Vec<_>>()
    };
    assert_eq!(drafts(&saved), ["keep me"]);
    let mut transient = saved;
    transient.id = TRANSIENT_WORKSPACE_ID.to_owned();
    assert_eq!(drafts(&transient), Vec::<String>::new(), "drafts moved");
    // Saving again says it is saved already; leaving lists it on home.
    scenario.key("ctrl+s");
    assert!(
        scenario
            .surface()
            .review()
            .unwrap()
            .status()
            .is_some_and(|status| status.contains("already saved"))
    );
    scenario.key("q");
    let screen = settled(&mut scenario);
    assert_eq!(rows(&scenario)[1], "Last commit");
    // Its preview counts its one draft in the singular.
    assert!(screen.contains("Preview"), "{}", screen.text());
    assert!(screen.contains("1 draft "), "{}", screen.text());
    assert!(!screen.contains("1 drafts"), "{}", screen.text());
}

/// Unsaved reviews keep their comments per target (spec 7.5): another target's unsaved review
/// does not show them, reopening the same target does, and saving moves them out of it.
#[test]
fn unsaved_reviews_keep_their_comments_per_target() {
    let fixture = fixture();
    let backend = Arc::new(LocalBackend::default());
    let last_commit = || Entry::Sources {
        title: None,
        sources: vec![ReviewSourceKind::LastCommit],
    };
    let branch = || Entry::Sources {
        title: None,
        sources: vec![ReviewSourceKind::BranchCompare {
            base_branch: BASE_BRANCH.to_owned(),
            head_branch: HEAD_BRANCH.to_owned(),
            merge_base: true,
        }],
    };
    let comment = |scenario: &mut Scenario<ReviewAppV2>, body: &str| {
        scenario.key("shift+c");
        scenario.paste(body);
        scenario.key("ctrl+s");
        settled(scenario)
    };
    let (mut scenario, _) = open(&fixture, &backend, last_commit(), 140, 40);
    let screen = comment(&mut scenario, "about the last commit");
    assert!(
        screen.contains("about the last commit"),
        "{}",
        screen.text()
    );

    // Another target's unsaved review starts without it.
    let (mut scenario, screen) = open(&fixture, &backend, branch(), 140, 40);
    assert!(scenario.surface().review().unwrap().is_transient());
    assert!(
        !screen.contains("about the last commit"),
        "{}",
        screen.text()
    );
    let screen = comment(&mut scenario, "about the branch");
    assert!(screen.contains("about the branch"), "{}", screen.text());

    // Reopening each target shows its own comments only.
    let (_, screen) = open(&fixture, &backend, last_commit(), 140, 40);
    assert!(
        screen.contains("about the last commit"),
        "{}",
        screen.text()
    );
    assert!(!screen.contains("about the branch"), "{}", screen.text());
    let (_, screen) = open(&fixture, &backend, branch(), 140, 40);
    assert!(screen.contains("about the branch"), "{}", screen.text());
    assert!(
        !screen.contains("about the last commit"),
        "{}",
        screen.text()
    );

    // Saving the last-commit review moves its comment into the saved review and out of the
    // unsaved scope; the branch review's comment stays where it was.
    let (mut scenario, _) = open(&fixture, &backend, last_commit(), 140, 40);
    scenario.key("ctrl+s");
    settled(&mut scenario);
    let saved = scenario
        .surface()
        .review()
        .unwrap()
        .workspace()
        .unwrap()
        .id
        .clone();
    assert_ne!(saved, TRANSIENT_WORKSPACE_ID);
    let resume = Entry::Resume {
        workspace_id: Some(saved),
    };
    let (_, screen) = open(&fixture, &backend, resume, 140, 40);
    assert!(
        screen.contains("about the last commit"),
        "{}",
        screen.text()
    );
    let (_, screen) = open(&fixture, &backend, last_commit(), 140, 40);
    assert!(
        !screen.contains("about the last commit"),
        "{}",
        screen.text()
    );
    let (_, screen) = open(&fixture, &backend, branch(), 140, 40);
    assert!(screen.contains("about the branch"), "{}", screen.text());
}

#[test]
fn home_clicks_start_suggestions_and_open_reviews_where_painted() {
    let fixture = fixture();
    let backend = Arc::new(LocalBackend::default());
    let review = create(
        &backend,
        &fixture,
        "Clickable",
        vec![ReviewSourceKind::LastCommit],
    );
    create(
        &backend,
        &fixture,
        "Other",
        vec![ReviewSourceKind::IndexStaged],
    );
    let (mut scenario, screen) = open(&fixture, &backend, Entry::Home, 140, 40);
    let point = screen.find("Branch main").expect("suggestion painted");
    scenario.click(point.x, point.y);
    let screen = settled(&mut scenario);
    assert_eq!(scenario.surface().route_name(), RouteName::Review);
    let workspace = scenario.surface().review().unwrap().workspace().unwrap();
    assert_eq!(
        workspace.sources[0].kind,
        ReviewSourceKind::CurrentBranch { base: None }
    );
    assert!(screen.contains(HEAD_BRANCH));
    scenario.key("q");
    let screen = settled(&mut scenario);
    // The first click selects a row, a press on the selected row opens it.
    let point = screen.find("Clickable").expect("row painted");
    scenario.click(point.x, point.y);
    settled(&mut scenario);
    assert_eq!(scenario.surface().route_name(), RouteName::Home);
    assert_eq!(
        scenario.surface().home().unwrap().selected(),
        Some(&RowKey::Review(review.id.clone()))
    );
    scenario.click(point.x, point.y);
    settled(&mut scenario);
    assert_eq!(
        scenario.surface().review().unwrap().workspace().unwrap().id,
        review.id
    );
}

#[test]
fn an_empty_home_invites_a_first_review() {
    let fixture = fixture();
    let backend = Arc::new(LocalBackend::default());
    let (mut scenario, screen) = open(&fixture, &backend, Entry::Home, 120, 36);
    assert!(
        screen.contains("Your next review starts here"),
        "{}",
        screen.text()
    );
    assert!(
        screen.contains("press n for a new review"),
        "{}",
        screen.text()
    );
    assert_eq!(rows(&scenario), Vec::<String>::new());
    scenario.key("n");
    settled(&mut scenario);
    assert_eq!(scenario.surface().route_name(), RouteName::NewReview);
    scenario.key("esc");
    settled(&mut scenario);
    assert_eq!(scenario.surface().route_name(), RouteName::Home);
}

/// A review written by the first review interface (before V2 state existed): path-only viewed
/// files, its own presentation state, and drafts whose anchors record neither head nor blob.
#[test]
fn a_review_written_before_v2_opens_with_its_viewed_files_and_drafts() {
    use crate::v2::document::{AnchorPlacement, LineKind, ReviewItem};
    use bcode_code_review_models::{SaveDraftRequest, SaveDraftResponse};
    let fixture = fixture();
    let backend = Arc::new(LocalBackend::default());
    let workspace = create(
        &backend,
        &fixture,
        "Before V2",
        vec![ReviewSourceKind::BranchCompare {
            base_branch: BASE_BRANCH.to_owned(),
            head_branch: HEAD_BRANCH.to_owned(),
            merge_base: true,
        }],
    );
    let resume = || Entry::Resume {
        workspace_id: Some(workspace.id.clone()),
    };
    // The anchor of the first added line, as the first interface recorded anchors.
    let mut anchor = {
        let (scenario, _) = open(&fixture, &backend, resume(), 140, 40);
        let doc = scenario.surface().review().unwrap().document().unwrap();
        let index = (0..doc.len())
            .find(|index| {
                matches!(doc.item(*index), Some(ReviewItem::Line(line)) if line.kind == LineKind::Added)
                    || matches!(doc.item(*index), Some(ReviewItem::LinePair(pair))
                        if pair.new.as_ref().is_some_and(|side| side.kind == LineKind::Added))
            })
            .expect("an added line");
        doc.anchor_for(&doc.item(index).unwrap().key())
            .expect("line anchor")
            .to_draft_anchor()
    };
    anchor.head_oid = None;
    anchor.blob_oid = None;
    let scope = NoteScope::for_workspace(&workspace);
    let _: SaveDraftResponse = backend.call_now(
        crate::v2::tui::notes::OP_DRAFT_SAVE,
        &SaveDraftRequest {
            repo_path: scope.repo_path,
            target: scope.target,
            scope: scope.scope,
            anchor: anchor.clone(),
            body: "written before V2".to_owned(),
        },
    );
    let mut legacy = serde_json::to_value(&workspace).expect("workspace encodes");
    legacy["viewed_files"] = serde_json::json!([anchor.file_path]);
    legacy["presentation_state"] = serde_json::json!({
        "schema_version": 1,
        "selected_path": anchor.file_path,
        "sidebar_mode": "files",
        "sidebar_visible": true,
        "thread_filter": "open",
        "show_resolved_threads": true,
        "collapsed_thread_keys": ["thread:1"],
    });
    let _: serde_json::Value = backend.call_now(
        OP_REVIEW_WORKSPACE_UPDATE,
        &serde_json::json!({
            "repo_path": fixture.root(),
            "workspace": legacy,
        }),
    );
    let (scenario, screen) = open(&fixture, &backend, resume(), 140, 40);
    assert_eq!(scenario.surface().route_name(), RouteName::Review);
    let doc = scenario.surface().review().unwrap().document().unwrap();
    assert_eq!(doc.viewed_progress().viewed, 1, "{}", screen.text());
    let placements: Vec<AnchorPlacement> = (0..doc.len())
        .filter_map(|index| match doc.item(index) {
            Some(ReviewItem::Thread(thread)) => Some(thread.placement),
            _ => None,
        })
        .collect();
    assert_eq!(placements, [AnchorPlacement::Line]);
    assert!(screen.contains("written before V2"), "{}", screen.text());
}
