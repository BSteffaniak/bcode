//! Switching how a series shows, in a stack of pull requests and in selected commits: views
//! change in place on the file being read, the rail stays with the review, and progress,
//! guides, drafts, passes and the assistant follow every group rather than only the one shown.

use super::app::{create, fixture, settled, stored, update};
use super::assistant_actions::active_session;
use super::publish::{layer, resume};
use crate::app::ReviewApp;
use crate::assistant::actions;
use crate::host::ClientToolReply;
use crate::services::Operation;
use crate::testing::backend::TestBackend;
use crate::testing::fake_provider::{FAKE_PROVIDER, FAKE_REPOSITORY, FakeProvider};
use crate::testing::fixture::{GitFixture, STACK_SHARED_FILE};
use crate::testing::harness::Scenario;
use bcode_code_review::document::ReviewItem;
use bcode_code_review_models::{
    ListReviewGuidesRequest, ReviewGroupKey, ReviewSeriesView, ReviewSourceKind,
    ReviewSourcesTokenRequest, ReviewWorkspace,
};
use std::sync::Arc;

fn commit(repo: &GitFixture, path: &str, content: &str) -> String {
    std::fs::write(repo.root().join(path), content).unwrap();
    repo.run(&["add", "--", path]);
    repo.run(&["commit", "-qm", content.trim(), "--", path]);
    repo.git(&["rev-parse", "HEAD"]).trim().to_owned()
}

fn key(app: &mut Scenario<ReviewApp>, key: &str) {
    app.key(key);
    settled(app);
}

/// The cursor's file path and the selector of the group it is in.
fn at(app: &Scenario<ReviewApp>) -> Option<(String, String)> {
    let review = app.surface().review()?;
    let cursor = review.cursor()?;
    let path = cursor.file()?.path.clone();
    let group = review.document()?.group_of_key(cursor)?.selector();
    Some((path, group))
}

/// The selector of the group the cursor is in (on a file or its banner).
fn group_at(app: &Scenario<ReviewApp>) -> Option<String> {
    let review = app.surface().review()?;
    Some(
        review
            .document()?
            .group_of_key(review.cursor()?)?
            .selector(),
    )
}

/// Step file by file (`n`, then `p`) until the cursor is on `path` in a group `accept`s.
#[track_caller]
fn to_file(app: &mut Scenario<ReviewApp>, path: &str, accept: impl Fn(&str) -> bool) {
    for step in ["n", "p"] {
        for _ in 0..40 {
            if at(app).is_some_and(|(file, group)| file == path && accept(&group)) {
                return;
            }
            key(app, step);
        }
    }
    panic!("never reached {path}; at {:?}", at(app));
}

fn series_view(app: &Scenario<ReviewApp>) -> Option<ReviewSeriesView> {
    app.surface().view_context().review?.series_view
}

fn files(app: &Scenario<ReviewApp>) -> (usize, usize) {
    app.surface().view_context().review.expect("review").files
}

fn status(app: &Scenario<ReviewApp>) -> String {
    app.surface()
        .review()
        .and_then(|review| review.status())
        .unwrap_or_default()
        .to_owned()
}

/// Thread cards the document shows.
fn thread_cards(app: &Scenario<ReviewApp>) -> usize {
    let doc = app.surface().review().unwrap().document().unwrap();
    (0..doc.len())
        .filter(|&index| matches!(doc.item(index), Some(ReviewItem::Thread(_))))
        .count()
}

fn opens(backend: &TestBackend) -> usize {
    backend.calls(<ReviewSourcesTokenRequest as Operation>::NAME)
}

/// Three contiguous commits: `a.txt`, then `b.txt`, then `a.txt` again.
struct Chain {
    repo: GitFixture,
    backend: Arc<TestBackend>,
    workspace: ReviewWorkspace,
    members: [ReviewGroupKey; 3],
    combined: ReviewGroupKey,
}

fn chain(revs: impl FnOnce(&GitFixture) -> Vec<String>) -> Chain {
    let repo = fixture();
    let revs = revs(&repo);
    let backend = Arc::new(TestBackend::default());
    let workspace = create(
        &backend,
        &repo,
        "Selected commits",
        vec![ReviewSourceKind::Commits { revs: revs.clone() }],
    );
    let source_id = workspace.sources[0].id.clone();
    let member = |index: usize| ReviewGroupKey::Member {
        source_id: source_id.clone(),
        member: revs[index.min(revs.len() - 1)].clone(),
    };
    Chain {
        members: [member(0), member(1), member(2)],
        combined: ReviewGroupKey::Combined {
            source_id: source_id.clone(),
        },
        repo,
        backend,
        workspace,
    }
}

fn contiguous() -> Chain {
    chain(|repo| {
        vec![
            commit(repo, "a.txt", "a1\n"),
            commit(repo, "b.txt", "b1\n"),
            commit(repo, "a.txt", "a2\n"),
        ]
    })
}

#[test]
fn switching_views_of_a_loaded_series_stays_on_the_file_without_reopening() {
    let chain = contiguous();
    let [first, _, third] = &chain.members;
    let mut app = resume(&chain.repo, &chain.backend, &chain.workspace.id);
    assert_eq!(series_view(&app), Some(ReviewSeriesView::Combined));
    let opened = opens(&chain.backend);
    to_file(&mut app, "a.txt", |group| {
        group == chain.combined.selector()
    });

    for (view, group) in [
        (ReviewSeriesView::OneMember, first),
        (ReviewSeriesView::AllMembers, first),
        (ReviewSeriesView::Combined, &chain.combined),
        (ReviewSeriesView::OneMember, first),
    ] {
        key(&mut app, "shift+o");
        assert_eq!(series_view(&app), Some(view));
        assert_eq!(
            at(&app),
            Some(("a.txt".to_owned(), group.selector())),
            "{view:?}"
        );
        assert!(status(&app).contains("(shift+o: "), "{}", status(&app));
    }
    // Presses accumulate: two steps from the first member land on the third.
    app.key(".");
    app.key(".");
    settled(&mut app);
    assert_eq!(group_at(&app), Some(third.selector()));
    key(&mut app, ",");
    key(&mut app, ".");
    assert_eq!(
        opens(&chain.backend),
        opened,
        "switching between loaded representations opens nothing again"
    );
}

#[test]
fn a_combined_diff_being_computed_counts_as_shown_for_the_next_press() {
    let chain = chain(|repo| {
        let first = commit(repo, "a.txt", "a1\n");
        commit(repo, "skipped.txt", "not selected\n");
        let second = commit(repo, "a.txt", "a2\n");
        vec![first, second]
    });
    let mut app = resume(&chain.repo, &chain.backend, &chain.workspace.id);
    assert_eq!(series_view(&app), Some(ReviewSeriesView::AllMembers));
    // The first press asks for the combined diff, which is computed; the second moves on from
    // it rather than asking for it again.
    app.key("shift+o");
    app.key("shift+o");
    settled(&mut app);
    assert_eq!(series_view(&app), Some(ReviewSeriesView::OneMember));
    // Once computed, the combined diff shows.
    key(&mut app, "shift+o");
    key(&mut app, "shift+o");
    assert_eq!(series_view(&app), Some(ReviewSeriesView::Combined));
    to_file(&mut app, "a.txt", |group| {
        group == chain.combined.selector()
    });
}

#[test]
fn shift_o_says_why_a_review_without_a_series_has_one_view() {
    let repo = fixture();
    let only = commit(&repo, "a.txt", "only\n");
    let backend = Arc::new(TestBackend::default());
    let workspace = create(
        &backend,
        &repo,
        "One commit",
        vec![ReviewSourceKind::Commits { revs: vec![only] }],
    );
    let mut app = resume(&repo, &backend, &workspace.id);
    key(&mut app, "shift+o");
    assert!(
        status(&app).contains("other ways to show"),
        "{}",
        status(&app)
    );
}

#[test]
fn an_assistant_drafted_comment_stays_with_its_member() {
    let chain = contiguous();
    let [first, second, _] = &chain.members;
    let mut app = resume(&chain.repo, &chain.backend, &chain.workspace.id);
    key(&mut app, "shift+o");
    to_file(&mut app, "a.txt", |group| group == first.selector());
    app.surface_mut()
        .review_mut()
        .unwrap()
        .draft_comment_at_cursor("Only the first edit.".to_owned())
        .expect("drafted");
    settled(&mut app);
    assert_eq!(thread_cards(&app), 1, "the draft shows on its member");
    key(&mut app, ".");
    assert_eq!(group_at(&app), Some(second.selector()));
    assert_eq!(thread_cards(&app), 0, "and on no other member");
}

#[test]
fn a_completed_pass_keeps_members_it_did_not_cover_and_drops_removed_sources() {
    let chain = contiguous();
    let [first, second, _] = &chain.members;
    let mut workspace = chain.workspace.clone();
    let gone = bcode_code_review_models::ReviewedRevision {
        reviewed_base_oid: None,
        reviewed_head_oid: Some("0".repeat(40)),
        recorded_at_ms: None,
    };
    workspace
        .reviewed_revisions
        .insert("removed-source".to_owned(), gone);
    update(&chain.backend, &workspace);
    let mut app = resume(&chain.repo, &chain.backend, &workspace.id);
    key(&mut app, "shift+o");
    assert_eq!(series_view(&app), Some(ReviewSeriesView::OneMember));
    key(&mut app, "shift+m");
    let passes = stored(&chain.backend, &chain.repo, &workspace.id).reviewed_revisions;
    assert!(passes.contains_key(&first.selector()), "{passes:?}");
    assert!(!passes.contains_key("removed-source"), "{passes:?}");
    key(&mut app, ".");
    key(&mut app, "shift+m");
    let passes = stored(&chain.backend, &chain.repo, &workspace.id).reviewed_revisions;
    assert!(passes.contains_key(&second.selector()), "{passes:?}");
    assert!(
        passes.contains_key(&first.selector()),
        "a member the pass did not show keeps its entry: {passes:?}"
    );
    assert!(
        !passes.contains_key(&chain.combined.selector()),
        "{passes:?}"
    );
}

/// Call the app's tool `tool` as the panel's conversation and let the app answer.
fn call(app: &mut Scenario<ReviewApp>, tool: &str, arguments: &str) -> ClientToolReply {
    let session = active_session(app);
    let mut answer = app.host().call_client_tool(&session, tool, arguments);
    for _ in 0..40 {
        settled(app);
        if let Ok(reply) = answer.try_recv() {
            return reply;
        }
    }
    panic!("{tool} was not answered");
}

fn refused(reply: ClientToolReply) -> String {
    match reply {
        ClientToolReply::Failed { message, .. } => message,
        ClientToolReply::Completed(text) => panic!("answered: {text}"),
    }
}

#[test]
fn the_assistant_names_series_views_as_its_tools_do_and_asks_which_group_a_path_means() {
    let chain = contiguous();
    let [first, _, third] = &chain.members;
    let mut app = resume(&chain.repo, &chain.backend, &chain.workspace.id);
    let both = refused(call(
        &mut app,
        actions::SET_COMPARISON,
        r#"{"series_view":"all_members","stack_view":"all_prs"}"#,
    ));
    assert!(both.contains("pass one grouping setting"), "{both}");
    let changed = call(
        &mut app,
        actions::SET_COMPARISON,
        r#"{"series_view":"all_members"}"#,
    );
    assert!(
        matches!(changed, ClientToolReply::Completed(_)),
        "{changed:?}"
    );
    assert_eq!(series_view(&app), Some(ReviewSeriesView::AllMembers));
    let text = app.surface().view_context().view_text();
    assert!(text.contains("series: all_members"), "{text}");
    assert!(!text.contains("AllMembers"), "{text}");

    // a.txt is in the first and the third member: name one.
    let several = refused(call(&mut app, actions::GO_TO, r#"{"path":"a.txt"}"#));
    assert!(
        several.contains(&first.selector()) && several.contains(&third.selector()),
        "{several}"
    );
    let source = third.selector();
    let moved = call(
        &mut app,
        actions::GO_TO,
        &serde_json::json!({"path": "a.txt", "source": source}).to_string(),
    );
    assert!(matches!(moved, ClientToolReply::Completed(_)), "{moved:?}");
    assert_eq!(at(&app), Some(("a.txt".to_owned(), third.selector())));
}

/// A stack of four pull requests with its whole-stack source, then one more commit, opened one
/// pull request at a time.
fn stack_and_commit() -> (GitFixture, Arc<TestBackend>, ReviewWorkspace) {
    let repo = fixture();
    let backend = Arc::new(TestBackend::default().with_provider(FakeProvider::new(repo.root())));
    let mut sources: Vec<_> = (100..=103).map(layer).collect();
    sources.push(ReviewSourceKind::Stack {
        provider: FAKE_PROVIDER.to_owned(),
        repository: FAKE_REPOSITORY.to_owned(),
        top: 103,
    });
    let rev = commit(&repo, "elsewhere.txt", "another change\n");
    sources.push(ReviewSourceKind::Commit { rev });
    let mut workspace = create(&backend, &repo, "Stack and more", sources);
    workspace.presentation =
        Some(serde_json::from_str(r#"{"schema_version":1,"stack_view":"one_layer"}"#).unwrap());
    update(&backend, &workspace);
    (repo, backend, workspace)
}

#[test]
fn the_rail_stays_while_reading_another_source_of_a_stack_review() {
    let (repo, backend, workspace) = stack_and_commit();
    let mut app = resume(&repo, &backend, &workspace.id);
    let (tabs, _) = app.surface().review().unwrap().rail();
    assert_eq!(tabs.len(), 5, "{tabs:?}");
    to_file(&mut app, "elsewhere.txt", |group| group == "source-6");
    let (tabs, selected) = app.surface().review().unwrap().rail();
    assert_eq!(tabs.len(), 5, "{tabs:?}");
    assert!(selected.is_some());
    assert!(app.surface().view_context().review.unwrap().stack.is_some());
    key(&mut app, "shift+o");
    assert!(
        status(&app).starts_with("every pull request of the stack"),
        "{}",
        status(&app)
    );
}

#[test]
fn changing_stack_views_stays_on_the_file_being_read() {
    let (repo, backend, workspace) = stack_and_commit();
    let mut app = resume(&repo, &backend, &workspace.id);
    key(&mut app, ".");
    let layer = group_at(&app).expect("a layer");
    // Leave the layer on one file, then read another in the whole stack.
    to_file(&mut app, "stack/layer_1.rs", |group| group == layer);
    key(&mut app, "shift+o");
    key(&mut app, "shift+o");
    assert_eq!(series_view(&app), Some(ReviewSeriesView::Combined));
    assert_eq!(
        at(&app).map(|(path, _)| path).as_deref(),
        Some("stack/layer_1.rs")
    );
    to_file(&mut app, STACK_SHARED_FILE, |group| group == "source-5");
    key(&mut app, "shift+o");
    assert_eq!(series_view(&app), Some(ReviewSeriesView::OneMember));
    assert_eq!(at(&app), Some((STACK_SHARED_FILE.to_owned(), layer)));
}

#[test]
fn stepping_and_changing_stack_views_neither_rereads_the_guide_nor_narrows_progress() {
    let (repo, backend, workspace) = stack_and_commit();
    let mut app = resume(&repo, &backend, &workspace.id);
    let reads = || backend.calls(<ListReviewGuidesRequest as Operation>::NAME);
    let before = reads();
    assert!(before > 0, "the guide was read once");
    for press in [".", ",", "shift+o", "shift+o", "shift+o"] {
        key(&mut app, press);
    }
    assert_eq!(reads(), before, "every layer's pins stay the same");

    // One pull request at a time still marks every file of the review.
    assert_eq!(series_view(&app), Some(ReviewSeriesView::OneMember));
    key(&mut app, ":");
    app.paste("Mark every file viewed");
    key(&mut app, "enter");
    let (total, viewed) = files(&app);
    assert!(total > 0);
    assert_eq!(viewed, total, "every layer and source counts");
}

#[test]
fn an_unsaved_sparse_selection_computes_its_combined_diff_when_asked() {
    let repo = fixture();
    let first = commit(&repo, "a.txt", "a1\n");
    commit(&repo, "skipped.txt", "not selected\n");
    let second = commit(&repo, "a.txt", "a2\n");
    let backend = Arc::new(TestBackend::default());
    let (mut app, _) = super::app::open(
        &repo,
        &backend,
        crate::entry::Entry::Sources {
            title: None,
            sources: vec![ReviewSourceKind::Commits {
                revs: vec![first, second],
            }],
        },
        160,
        48,
    );
    assert_eq!(series_view(&app), Some(ReviewSeriesView::AllMembers));
    key(&mut app, "shift+o");
    assert_eq!(series_view(&app), Some(ReviewSeriesView::Combined));
    let doc = app.surface().review().unwrap().document().unwrap();
    assert!(
        doc.groups()
            .any(|g| matches!(g.key, ReviewGroupKey::Combined { .. }) && !g.alternate),
        "the combined diff shows"
    );
}
