//! Review annotations and AI on the review screen: folding threads and hiding
//! resolved ones, AI follow-ups, structured commands, answers turned into suggestions and
//! drafts, the suggestion lifecycle, copying answers and the report, bulk viewed marks, the
//! file panel's hide-viewed filter, go-to-line from the palette, item action menus, host
//! completion in the composer, editing a review's sources, and the save offer when leaving an
//! unsaved review.

use super::app::{self as app_tests, settled};
use super::stable;
use crate::action::Action;
use crate::app::RouteName;
use crate::config::ReviewViewConfig;
use crate::entry::Entry;
use crate::notes::NoteScope;
use crate::testing::backend::TestBackend;
use crate::testing::fixture::{FixtureSpec, GitFixture};
use crate::testing::fixture_branch_compare;
use crate::testing::harness::Scenario;
use crate::{Focus, ReviewSurface};
use bcode_code_review::document::ReviewItem;
use bcode_code_review_models::{
    GetReviewWorkspaceRequest, GetReviewWorkspaceResponse, ListDraftsRequest, ListDraftsResponse,
    ListReviewAiExchangesRequest, ListReviewAiExchangesResponse, ListReviewSuggestionsRequest,
    ListReviewSuggestionsResponse, ReviewSourceKind, ReviewSuggestionStatus, ReviewWorkspace,
    UpdateReviewWorkspaceRequest,
};
use bcode_session_view::follow::SessionViewFollowUpdate;
use std::sync::Arc;
use std::time::Duration;

struct Setup {
    scenario: Scenario<ReviewSurface>,
    backend: Arc<TestBackend>,
    workspace: ReviewWorkspace,
}

fn setup(fixture: &GitFixture, width: u16, height: u16) -> Setup {
    let backend = Arc::new(TestBackend::default());
    let workspace = ReviewWorkspace {
        id: "parity-review".to_owned(),
        ..ReviewWorkspace::from_target(fixture.root().to_path_buf(), fixture_branch_compare())
    };
    let _ = backend.call_now(UpdateReviewWorkspaceRequest {
        repo_path: workspace.repo_root.clone(),
        workspace: workspace.clone(),
    });
    let surface = ReviewSurface::new(
        backend.services(),
        workspace.repo_root.clone(),
        workspace.sources.clone(),
        workspace.title.clone(),
        ReviewViewConfig::default(),
    )
    .with_scroll_motion(Duration::ZERO)
    .with_workspace(workspace.clone());
    let mut scenario = Scenario::new(surface, width, height);
    stable(&mut scenario);
    Setup {
        scenario,
        backend,
        workspace,
    }
}

fn scope(setup: &Setup) -> NoteScope {
    NoteScope::for_workspace(&setup.workspace)
}

fn drafts(setup: &Setup) -> Vec<String> {
    let scope = scope(setup);
    let response: ListDraftsResponse = setup.backend.call_now(ListDraftsRequest {
        repo_path: scope.repo_path,
        target: scope.target,
        scope: scope.scope,
    });
    response
        .drafts
        .into_iter()
        .map(|draft| draft.body)
        .collect()
}

fn suggestions(setup: &Setup) -> Vec<(String, ReviewSuggestionStatus)> {
    let scope = scope(setup);
    let response: ListReviewSuggestionsResponse =
        setup.backend.call_now(ListReviewSuggestionsRequest {
            repo_path: scope.repo_path,
            target: scope.target,
            scope: scope.scope,
        });
    let mut out: Vec<_> = response
        .suggestions
        .into_iter()
        .map(|suggestion| (suggestion.suggestion_id, suggestion.status))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn exchanges(setup: &Setup) -> Vec<bcode_code_review_models::ReviewAiExchange> {
    let scope = scope(setup);
    let response: ListReviewAiExchangesResponse =
        setup.backend.call_now(ListReviewAiExchangesRequest {
            repo_path: scope.repo_path,
            target: scope.target,
            scope: scope.scope,
        });
    response.exchanges
}

fn stored(setup: &Setup) -> ReviewWorkspace {
    let response: GetReviewWorkspaceResponse = setup.backend.call_now(GetReviewWorkspaceRequest {
        repo_path: setup.workspace.repo_root.clone(),
        workspace_id: setup.workspace.id.clone(),
    });
    response.workspace.expect("stored workspace")
}

fn index_where(
    scenario: &Scenario<ReviewSurface>,
    predicate: impl Fn(&ReviewItem) -> bool,
) -> Option<usize> {
    let doc = scenario.surface().document()?;
    (0..doc.len()).find(|index| doc.item(*index).is_some_and(&predicate))
}

fn first_added_line(scenario: &Scenario<ReviewSurface>) -> usize {
    use bcode_code_review::document::LineKind;
    index_where(
        scenario,
        |item| matches!(item, ReviewItem::Line(line) if line.kind == LineKind::Added),
    )
    .expect("an added line")
}

/// Move the cursor to item `index` with a counted motion.
fn move_to(scenario: &mut Scenario<ReviewSurface>, index: usize) {
    let doc = scenario.surface().document().expect("document");
    let current = scenario
        .surface()
        .cursor()
        .and_then(|key| doc.index_of(key))
        .unwrap_or(0);
    let count = super::stops_between(doc, current, index);
    let key = if index >= current { "j" } else { "k" };
    if count > 0 {
        for digit in count.to_string().chars() {
            scenario.key(&digit.to_string());
        }
        scenario.key(key);
    }
    stable(scenario);
}

fn cursor_item(scenario: &Scenario<ReviewSurface>) -> ReviewItem {
    let surface = scenario.surface();
    let doc = surface.document().expect("document");
    let index = doc
        .index_of(surface.cursor().expect("cursor"))
        .expect("cursor index");
    doc.item(index).expect("cursor item").clone()
}

fn run_palette(scenario: &mut Scenario<ReviewSurface>, query: &str) {
    scenario.key(":");
    scenario.paste(query);
    scenario.key("enter");
    stable(scenario);
}

/// Answer the first question sent to `session` with `text`.
fn answer(setup: &mut Setup, session: &str, text: &str) {
    let mut snapshot = bcode_session_view_models::SessionViewSnapshot::empty();
    snapshot.connection_status = bcode_session_view_models::SessionConnectionViewStatus::Attached;
    let (_, prompt) = setup
        .backend
        .messages()
        .into_iter()
        .find(|(id, _)| id == session)
        .expect("a question was sent");
    snapshot
        .transcript
        .items
        .push(bcode_session_view_models::TranscriptViewItem {
            id: bcode_session_view_models::TranscriptViewItemId::new("user:1"),
            revision: 1,
            sequence: Some(1),
            timestamp_ms: Some(1),
            output_location: None,
            streaming: false,
            kind: bcode_session_view_models::TranscriptViewItemKind::UserMessage {
                message: bcode_session_view_models::ChatMessageView::markdown(prompt),
            },
        });
    snapshot
        .transcript
        .items
        .push(bcode_session_view_models::TranscriptViewItem {
            id: bcode_session_view_models::TranscriptViewItemId::new("assistant:1"),
            revision: 1,
            sequence: Some(1),
            timestamp_ms: Some(1),
            output_location: None,
            streaming: false,
            kind: bcode_session_view_models::TranscriptViewItemKind::AssistantMessage {
                message: bcode_session_view_models::ChatMessageView::markdown(text),
            },
        });
    setup
        .scenario
        .host()
        .session_view(session)
        .expect("subscribed")
        .try_send(SessionViewFollowUpdate::Snapshot(Box::new(snapshot)))
        .unwrap();
    stable(&mut setup.scenario);
}

#[test]
fn threads_fold_resolved_threads_hide_and_both_persist() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let mut setup = setup(&fixture, 120, 40);
    let line = first_added_line(&setup.scenario);
    move_to(&mut setup.scenario, line);
    setup.scenario.key("c");
    setup.scenario.paste("first body line\n\nsecond paragraph");
    setup.scenario.key("ctrl+s");
    stable(&mut setup.scenario);
    setup.scenario.key(")");
    let screen = stable(&mut setup.scenario);
    assert!(matches!(
        cursor_item(&setup.scenario),
        ReviewItem::Thread(_)
    ));
    assert!(screen.contains("second paragraph"), "{}", screen.text());
    // `z` folds the thread to its header; `z` again unfolds it.
    setup.scenario.key("z");
    let screen = stable(&mut setup.scenario);
    assert!(!screen.contains("second paragraph"), "{}", screen.text());
    assert!(screen.contains("1 comment"), "{}", screen.text());
    let key = setup.scenario.surface().cursor().cloned().unwrap();
    let header = setup.scenario.surface().item_area(&key).expect("painted");
    assert_eq!(header.height, 1, "a collapsed thread is its header");
    // A click on the header unfolds it again.
    setup.scenario.click(header.x + header.width / 2, header.y);
    let screen = stable(&mut setup.scenario);
    assert!(screen.contains("second paragraph"), "{}", screen.text());
    // `shift+z` collapses every thread; again expands them.
    setup.scenario.key("shift+z");
    let screen = stable(&mut setup.scenario);
    assert!(!screen.contains("second paragraph"), "{}", screen.text());
    setup.scenario.key("shift+z");
    let screen = stable(&mut setup.scenario);
    assert!(screen.contains("second paragraph"), "{}", screen.text());
    // Resolving, then hiding resolved threads removes the thread from the document.
    setup.scenario.key("z");
    setup.scenario.key("shift+r");
    stable(&mut setup.scenario);
    setup.scenario.key("shift+h");
    stable(&mut setup.scenario);
    let has_thread = |scenario: &Scenario<ReviewSurface>| {
        index_where(scenario, |item| matches!(item, ReviewItem::Thread(_))).is_some()
    };
    assert!(!has_thread(&setup.scenario));
    // The resolved filter persists with the workspace, and resolving it yourself marked the
    // thread viewed (which replaces the fold by hand, and folds it by itself).
    setup.scenario.key("q");
    stable(&mut setup.scenario);
    let stored = stored(&setup);
    let presentation = stored.presentation.clone().expect("presentation");
    assert!(presentation.hide_resolved_threads);
    assert_eq!(stored.viewed_threads.len(), 1);
    assert!(presentation.thread_folds.is_empty() && presentation.collapsed_threads.is_empty());
    // Reopening restores them; showing resolved threads brings it back, still collapsed.
    let mut reopened = setup.workspace.clone();
    reopened.presentation = Some(presentation);
    reopened.viewed_threads = stored.viewed_threads;
    let surface = ReviewSurface::new(
        setup.backend.services(),
        reopened.repo_root.clone(),
        reopened.sources.clone(),
        reopened.title.clone(),
        ReviewViewConfig::default(),
    )
    .with_scroll_motion(Duration::ZERO)
    .with_workspace(reopened);
    let mut scenario = Scenario::new(surface, 120, 40);
    stable(&mut scenario);
    assert!(!has_thread(&scenario));
    scenario.key("shift+h");
    let screen = stable(&mut scenario);
    assert!(has_thread(&scenario));
    assert!(!screen.contains("second paragraph"), "{}", screen.text());
}

#[test]
#[allow(clippy::too_many_lines)]
fn ai_answers_follow_up_copy_suggest_draft_and_decide_suggestions() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let mut setup = setup(&fixture, 140, 50);
    let line = first_added_line(&setup.scenario);
    move_to(&mut setup.scenario, line);
    // A structured command from the AI menu.
    setup.scenario.key("shift+a");
    stable(&mut setup.scenario);
    let menu = setup.scenario.surface().overlay().expect("AI menu");
    assert_eq!(
        menu.shown_actions(),
        [
            "ai_explain",
            "ai_risks",
            "ai_suggest_comment",
            "ai_tests",
            "ai_summarize",
            "ask"
        ]
    );
    setup.scenario.key("enter");
    stable(&mut setup.scenario);
    let sessions = setup.backend.sessions();
    assert_eq!(sessions.len(), 1);
    let session = sessions[0].0.clone();
    let origin = &setup.backend.session_origins()[0];
    assert_eq!(origin.app_id, "bcode.code-review");
    assert_eq!(
        origin.context.get("purpose").map(String::as_str),
        Some("question")
    );
    assert!(
        setup.backend.messages()[0]
            .1
            .contains("Explain what the selected change does")
    );
    setup
        .scenario
        .surface_mut()
        .session_navigation_finished(session.parse().unwrap(), Ok(()));
    answer(&mut setup, &session, "Consider **renaming** this.");
    // The answer's actions: copy, expand, suggest, draft.
    let agent = index_where(&setup.scenario, |item| {
        matches!(item, ReviewItem::AgentThread(_))
    })
    .expect("AI answer");
    move_to(&mut setup.scenario, agent);
    setup.scenario.key("enter");
    stable(&mut setup.scenario);
    let actions: Vec<String> = setup
        .scenario
        .surface()
        .overlay()
        .expect("item menu")
        .shown_actions()
        .into_iter()
        .map(str::to_owned)
        .collect();
    for expected in [
        "open_session",
        "copy_answer",
        "suggest_comment",
        "draft_answer",
    ] {
        assert!(
            actions.iter().any(|action| action == expected),
            "{actions:?}"
        );
    }
    assert!(!actions.iter().any(|action| action == "accept_suggestion"));
    setup.scenario.key("esc");
    setup.scenario.key("shift+y");
    stable(&mut setup.scenario);
    assert_eq!(
        setup.scenario.host().clipboard(),
        ["Consider **renaming** this."]
    );
    setup.scenario.key("m");
    stable(&mut setup.scenario);
    assert_eq!(drafts(&setup), ["Consider **renaming** this."]);
    setup.scenario.key("m");
    stable(&mut setup.scenario);
    assert_eq!(drafts(&setup).len(), 1, "drafting twice is refused");
    // Suggest, then accept: a suggestion becomes a draft.
    let agent = index_where(&setup.scenario, |item| {
        matches!(item, ReviewItem::AgentThread(_))
    })
    .unwrap();
    move_to(&mut setup.scenario, agent);
    setup.scenario.key("s");
    let screen = stable(&mut setup.scenario);
    assert!(
        screen.contains("suggested"),
        "{:?}\n{}",
        setup.scenario.surface().status(),
        screen.text()
    );
    assert!(
        screen.contains("Accept"),
        "{:?} {:?}\n{}",
        setup.scenario.surface().toasts(),
        suggestions(&setup),
        screen.text()
    );
    assert_eq!(suggestions(&setup).len(), 1);
    let agent = index_where(&setup.scenario, |item| {
        matches!(item, ReviewItem::AgentThread(_))
    })
    .unwrap();
    move_to(&mut setup.scenario, agent);
    setup.scenario.key(";");
    stable(&mut setup.scenario);
    assert_eq!(suggestions(&setup)[0].1, ReviewSuggestionStatus::Accepted);
    assert_eq!(drafts(&setup).len(), 2);
    // Suggest again, then reject from the palette.
    let agent = index_where(&setup.scenario, |item| {
        matches!(item, ReviewItem::AgentThread(_))
    })
    .unwrap();
    move_to(&mut setup.scenario, agent);
    setup.scenario.key("s");
    stable(&mut setup.scenario);
    run_palette(&mut setup.scenario, "reject the suggestion");
    let decided: Vec<_> = suggestions(&setup)
        .into_iter()
        .map(|(_, status)| status)
        .collect();
    assert_eq!(
        decided,
        [
            ReviewSuggestionStatus::Accepted,
            ReviewSuggestionStatus::Rejected
        ]
    );
    // Refining asks again in the same linked session (a follow-up), as does `a` on the answer.
    let agent = index_where(&setup.scenario, |item| {
        matches!(item, ReviewItem::AgentThread(_))
    })
    .unwrap();
    move_to(&mut setup.scenario, agent);
    setup.scenario.key("s");
    stable(&mut setup.scenario);
    run_palette(&mut setup.scenario, "refine the suggestion");
    assert!(
        suggestions(&setup)
            .iter()
            .any(|(_, status)| *status == ReviewSuggestionStatus::Refining)
    );
    let agent = index_where(&setup.scenario, |item| {
        matches!(item, ReviewItem::AgentThread(_))
    })
    .unwrap();
    move_to(&mut setup.scenario, agent);
    setup.scenario.key("a");
    assert_eq!(setup.scenario.surface().focus(), Focus::Composer);
    setup.scenario.paste("And the tests?");
    setup.scenario.key("enter");
    stable(&mut setup.scenario);
    assert_eq!(setup.backend.sessions().len(), 1, "no second session");
    let messages = setup.backend.messages();
    assert_eq!(messages.len(), 3);
    assert!(messages.iter().all(|(id, _)| *id == session));
    assert!(
        messages[1]
            .1
            .contains("Refine this suggested review comment")
    );
    assert!(messages[2].1.contains("And the tests?"));
    assert!(
        exchanges(&setup)
            .iter()
            .all(|exchange| exchange.session_id.as_deref() == Some(session.as_str()))
    );
    // `z` on an answer folds it, as it does a thread.
    let agent = index_where(&setup.scenario, |item| {
        matches!(item, ReviewItem::AgentThread(_))
    })
    .unwrap();
    move_to(&mut setup.scenario, agent);
    setup.scenario.key("z");
    stable(&mut setup.scenario);
    assert_eq!(setup.scenario.surface().status(), Some("answer collapsed"));
    setup.scenario.key("z");
    stable(&mut setup.scenario);
    // Its More button shows the whole answer (and Less the preview again).
    let screen = setup.scenario.frame();
    let more = screen.find("More").expect("More");
    setup.scenario.click(more.x, more.y);
    stable(&mut setup.scenario);
    assert_eq!(
        setup.scenario.surface().status(),
        Some("showing the whole answer")
    );
    // `shift+d` on an answer deletes its exchange.
    let before = exchanges(&setup).len();
    setup.scenario.key("shift+d");
    stable(&mut setup.scenario);
    assert_eq!(exchanges(&setup).len(), before - 1);
}

#[test]
fn bulk_viewed_marks_hide_viewed_files_goto_line_and_the_report() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let mut setup = setup(&fixture, 160, 40);
    let total = setup
        .scenario
        .surface()
        .document()
        .unwrap()
        .viewed_progress()
        .total;
    run_palette(&mut setup.scenario, "mark every file viewed");
    let progress = setup
        .scenario
        .surface()
        .document()
        .unwrap()
        .viewed_progress();
    assert_eq!(progress.viewed, total);
    // Hiding viewed files empties the file panel; marking every file unviewed shows them all.
    setup.scenario.key("h");
    stable(&mut setup.scenario);
    let tree = setup.scenario.surface().file_tree().unwrap();
    assert_eq!(tree.items().len(), 0);
    run_palette(&mut setup.scenario, "mark every file unviewed");
    let tree = setup.scenario.surface().file_tree().unwrap();
    assert!(tree.items().len() >= total);
    assert_eq!(
        setup
            .scenario
            .surface()
            .document()
            .unwrap()
            .viewed_progress()
            .viewed,
        0
    );
    // Go to a line of the cursor's file from the palette.
    let line = first_added_line(&setup.scenario);
    move_to(&mut setup.scenario, line);
    let ReviewItem::Line(target) = cursor_item(&setup.scenario) else {
        panic!("a line");
    };
    let number = target.new_line.unwrap();
    // Leave the line for its hunk header (same file).
    setup.scenario.key("{");
    stable(&mut setup.scenario);
    assert!(matches!(
        cursor_item(&setup.scenario),
        ReviewItem::HunkHeader(_)
    ));
    setup.scenario.key(":");
    setup.scenario.paste(&number.to_string());
    let overlay = setup.scenario.surface().overlay().unwrap();
    assert_eq!(
        overlay.shown_actions(),
        [format!("goto_line:{number}").as_str()]
    );
    setup.scenario.key("enter");
    stable(&mut setup.scenario);
    let ReviewItem::Line(landed) = cursor_item(&setup.scenario) else {
        panic!("the cursor is on a line");
    };
    assert_eq!(landed.new_line, Some(number));
    // The report.
    setup.scenario.key("c");
    setup.scenario.paste("Report me");
    setup.scenario.key("ctrl+s");
    stable(&mut setup.scenario);
    run_palette(&mut setup.scenario, "copy a markdown review report");
    let clipboard = setup.scenario.host().clipboard();
    let report = clipboard.last().expect("report copied");
    assert!(
        report.starts_with(&format!("# {}", setup.workspace.title)),
        "{report}"
    );
    assert!(
        report.contains("## Threads (1 open, 0 resolved)"),
        "{report}"
    );
    assert!(report.contains("Report me"), "{report}");
    assert!(report.contains("## Unviewed files"), "{report}");
}

#[test]
fn editing_sources_keeps_ids_and_excluding_needs_one_source() {
    let fixture = app_tests::fixture();
    let backend = Arc::new(TestBackend::default());
    let workspace = app_tests::create(
        &backend,
        &fixture,
        "Sources",
        vec![ReviewSourceKind::LastCommit],
    );
    let entry = Entry::Workspace {
        workspace: Box::new(workspace.clone()),
    };
    let (mut scenario, _) = app_tests::open(&fixture, &backend, entry, 140, 40);
    scenario.key(":");
    scenario.paste("edit the review's sources");
    scenario.key("enter");
    let screen = settled(&mut scenario);
    assert_eq!(scenario.surface().route_name(), RouteName::NewReview);
    let wizard = scenario.surface().wizard().unwrap();
    assert!(wizard.is_editing());
    assert_eq!(wizard.source_labels(), ["Last commit"]);
    assert!(screen.contains("Edit sources"), "{}", screen.text());
    assert!(screen.contains("Save changes"), "{}", screen.text());
    // Excluding the only source is refused.
    scenario.key("end");
    scenario.key("tab");
    settled(&mut scenario);
    assert_eq!(scenario.surface().wizard().unwrap().inclusion(), [false]);
    scenario.key("home");
    scenario.key("enter");
    settled(&mut scenario);
    assert_eq!(scenario.surface().route_name(), RouteName::NewReview);
    // Include it again, add a second source, and save: ids are kept and new ones are unique.
    scenario.key("end");
    scenario.key("tab");
    assert_eq!(scenario.surface().wizard().unwrap().inclusion(), [true]);
    scenario.key("home");
    scenario.key("down");
    scenario.key("enter");
    settled(&mut scenario);
    scenario.key("enter");
    settled(&mut scenario);
    scenario.key("down");
    scenario.key("enter");
    settled(&mut scenario);
    assert_eq!(
        scenario.surface().wizard().unwrap().source_labels(),
        ["Last commit", "Staged changes"]
    );
    // The same source is not added twice.
    scenario.key("home");
    scenario.key("down");
    scenario.key("enter");
    settled(&mut scenario);
    scenario.key("enter");
    settled(&mut scenario);
    scenario.key("down");
    scenario.key("enter");
    settled(&mut scenario);
    let wizard = scenario.surface().wizard().unwrap();
    assert_eq!(wizard.source_labels(), ["Last commit", "Staged changes"]);
    assert_eq!(wizard.status(), Some("that source is already included"));
    scenario.key("home");
    scenario.key("enter");
    settled(&mut scenario);
    assert_eq!(scenario.surface().route_name(), RouteName::Review);
    let saved = app_tests::stored(&backend, &fixture, &workspace.id);
    let ids: Vec<&str> = saved
        .sources
        .iter()
        .map(|source| source.id.as_str())
        .collect();
    assert_eq!(ids, ["source-1", "source-2"]);
    assert_eq!(saved.sources[1].kind, ReviewSourceKind::IndexStaged);
    // `esc` in the editor returns to the review unchanged.
    scenario.key(":");
    scenario.paste("edit the review's sources");
    scenario.key("enter");
    settled(&mut scenario);
    scenario.key("esc");
    settled(&mut scenario);
    assert_eq!(scenario.surface().route_name(), RouteName::Review);
    assert_eq!(
        app_tests::stored(&backend, &fixture, &workspace.id)
            .sources
            .len(),
        2
    );
}

#[test]
fn leaving_an_unsaved_review_with_comments_offers_saving_once() {
    let fixture = app_tests::fixture();
    let backend = Arc::new(TestBackend::default());
    let entry = Entry::Sources {
        title: None,
        sources: vec![ReviewSourceKind::LastCommit],
    };
    let (mut scenario, _) = app_tests::open(&fixture, &backend, entry, 140, 40);
    // Without annotations, `q` leaves at once.
    scenario.key("q");
    settled(&mut scenario);
    assert_eq!(scenario.surface().route_name(), RouteName::Home);
    let entry = Entry::Sources {
        title: None,
        sources: vec![ReviewSourceKind::LastCommit],
    };
    let (mut scenario, _) = app_tests::open(&fixture, &backend, entry, 140, 40);
    scenario.key("shift+c");
    scenario.paste("unsaved note");
    scenario.key("ctrl+s");
    settled(&mut scenario);
    scenario.key("q");
    settled(&mut scenario);
    let review = scenario.surface().review().expect("still in the review");
    assert!(
        review
            .status()
            .is_some_and(|status| status.contains("not saved") && status.contains("ctrl+s")),
        "{:?}",
        review.status()
    );
    // Any other key disarms; the next exit asks again, and a second one leaves.
    scenario.key("j");
    scenario.key("q");
    settled(&mut scenario);
    assert_eq!(scenario.surface().route_name(), RouteName::Review);
    scenario.key("q");
    settled(&mut scenario);
    assert_eq!(scenario.surface().route_name(), RouteName::Home);
    assert!(
        !scenario
            .actions()
            .iter()
            .any(|action| matches!(action, Action::Close { .. }))
    );
}

#[test]
fn saving_a_transient_review_moves_its_ai_questions() {
    let fixture = app_tests::fixture();
    let backend = Arc::new(TestBackend::default());
    let entry = Entry::Sources {
        title: None,
        sources: vec![ReviewSourceKind::LastCommit],
    };
    let (mut scenario, _) = app_tests::open(&fixture, &backend, entry, 140, 40);
    scenario.key("shift+a");
    scenario.key("down");
    scenario.key("enter");
    settled(&mut scenario);
    let session = backend.sessions()[0].0.clone();
    if scenario.surface().route_name() == RouteName::Review {
        scenario
            .surface_mut()
            .session_navigation_finished(session.parse().unwrap(), Ok(()));
    }
    settled(&mut scenario);
    scenario.key("ctrl+s");
    settled(&mut scenario);
    let review = scenario.surface().review().unwrap();
    assert!(!review.is_transient());
    let saved = review.workspace().unwrap().clone();
    let listed = |workspace: &ReviewWorkspace| {
        let scope = NoteScope::for_workspace(workspace);
        let response: ListReviewAiExchangesResponse =
            backend.call_now(ListReviewAiExchangesRequest {
                repo_path: scope.repo_path,
                target: scope.target,
                scope: scope.scope,
            });
        response.exchanges
    };
    let moved = listed(&saved);
    assert_eq!(moved.len(), 1);
    assert!(moved[0].question.contains("risks"));
    assert_eq!(moved[0].session_id.as_deref(), Some(session.as_str()));
    let mut transient = saved;
    transient.id = crate::entry::TRANSIENT_WORKSPACE_ID.to_owned();
    assert!(listed(&transient).is_empty(), "the question moved");
}

/// Index of the first item after `from` that satisfies `predicate`.
fn index_after(
    scenario: &Scenario<ReviewSurface>,
    from: usize,
    predicate: impl Fn(&ReviewItem) -> bool,
) -> Option<usize> {
    let doc = scenario.surface().document()?;
    (from + 1..doc.len()).find(|index| doc.item(*index).is_some_and(&predicate))
}

fn cursor_index(scenario: &Scenario<ReviewSurface>) -> usize {
    let surface = scenario.surface();
    let doc = surface.document().expect("document");
    doc.index_of(surface.cursor().expect("cursor"))
        .expect("cursor index")
}

#[test]
#[allow(clippy::too_many_lines)]
fn motions_reach_open_threads_answers_and_attention_and_answers_reconnect() {
    use bcode_code_review::document::LineKind;
    use bcode_code_review_models::{ReviewThreadKind, ReviewThreadSeverity};
    let added =
        |item: &ReviewItem| matches!(item, ReviewItem::Line(line) if line.kind == LineKind::Added);
    let open = |item: &ReviewItem| matches!(item, ReviewItem::Thread(thread) if !thread.resolved);
    let is_answer = |item: &ReviewItem| matches!(item, ReviewItem::AgentThread(_));
    let fixture = GitFixture::generate(FixtureSpec::small());
    let mut setup = setup(&fixture, 160, 40);
    // A resolved thread, an open one (kind and severity set in the composer), then a question.
    let line = first_added_line(&setup.scenario);
    move_to(&mut setup.scenario, line);
    setup.scenario.key("c");
    setup.scenario.paste("resolved note");
    setup.scenario.key("ctrl+s");
    stable(&mut setup.scenario);
    setup.scenario.key(")");
    stable(&mut setup.scenario);
    setup.scenario.key("shift+r");
    stable(&mut setup.scenario);
    let line = index_after(&setup.scenario, cursor_index(&setup.scenario), added).unwrap();
    move_to(&mut setup.scenario, line);
    setup.scenario.key("c");
    setup.scenario.paste("open note");
    setup.scenario.keys(&["alt+k", "alt+s"]);
    setup.scenario.key("ctrl+s");
    stable(&mut setup.scenario);
    setup.scenario.key(")");
    stable(&mut setup.scenario);
    let thread = cursor_index(&setup.scenario);
    let line = index_after(&setup.scenario, thread, added).unwrap();
    move_to(&mut setup.scenario, line);
    setup.scenario.key("a");
    setup.scenario.paste("Why?");
    setup.scenario.key("enter");
    stable(&mut setup.scenario);
    let session = setup.backend.sessions()[0].0.clone();
    setup
        .scenario
        .surface_mut()
        .session_navigation_finished(session.parse().unwrap(), Ok(()));
    stable(&mut setup.scenario);

    // Open-thread motions skip the resolved thread.
    setup.scenario.key("g");
    run_palette(&mut setup.scenario, "open_thread_next");
    let ReviewItem::Thread(found) = cursor_item(&setup.scenario) else {
        panic!("an open thread");
    };
    assert!(!found.resolved);
    assert_eq!(
        (found.kind, found.severity),
        (ReviewThreadKind::Question, ReviewThreadSeverity::Nit),
        "alt+k and alt+s in the composer"
    );
    run_palette(&mut setup.scenario, "ai_thread_next");
    assert!(is_answer(&cursor_item(&setup.scenario)));
    run_palette(&mut setup.scenario, "open_thread_previous");
    assert!(open(&cursor_item(&setup.scenario)));
    setup.scenario.key("shift+g");
    stable(&mut setup.scenario);
    run_palette(&mut setup.scenario, "ai_thread_previous");
    assert!(is_answer(&cursor_item(&setup.scenario)));

    // Attention: unviewed files first, then open threads, then nothing.
    setup.scenario.key("g");
    run_palette(&mut setup.scenario, "attention_next");
    assert!(matches!(
        cursor_item(&setup.scenario),
        ReviewItem::FileHeader(header) if !header.viewed
    ));
    run_palette(&mut setup.scenario, "mark every file viewed");
    setup.scenario.key("g");
    run_palette(&mut setup.scenario, "attention_next");
    assert!(open(&cursor_item(&setup.scenario)));
    setup.scenario.key("shift+g");
    stable(&mut setup.scenario);
    run_palette(&mut setup.scenario, "attention_next");
    assert!(
        setup
            .scenario
            .surface()
            .status()
            .is_some_and(|status| status.starts_with("nothing needs attention")),
        "{:?}",
        setup.scenario.surface().status()
    );

    // A stopped answer stream reconnects with `retry`.
    let at = index_where(&setup.scenario, is_answer).unwrap();
    move_to(&mut setup.scenario, at);
    let stopped = setup.scenario.host().session_view(&session).unwrap();
    stopped
        .try_send(SessionViewFollowUpdate::Disconnected {
            message: "daemon went away".to_owned(),
            code: None,
        })
        .unwrap();
    let screen = stable(&mut setup.scenario);
    assert!(screen.contains("stream stopped"), "{}", screen.text());
    run_palette(&mut setup.scenario, "retry");
    let screen = stable(&mut setup.scenario);
    assert!(!screen.contains("stream stopped"), "{}", screen.text());
    answer(&mut setup, &session, "Because **reasons**.");
    let screen = stable(&mut setup.scenario);
    assert!(screen.contains("reasons"), "{}", screen.text());

    // Showing the whole answer is kept with the review.
    let at = index_where(&setup.scenario, is_answer).unwrap();
    move_to(&mut setup.scenario, at);
    let screen = setup.scenario.frame();
    let more = screen.find("More").expect("More");
    setup.scenario.click(more.x, more.y);
    stable(&mut setup.scenario);
    setup.scenario.key("q");
    stable(&mut setup.scenario);
    let presentation = stored(&setup).presentation.expect("presentation");
    assert_eq!(presentation.expanded_answers.len(), 1);
}
