//! Saved series opened from their canonical sources, through the app and its in-process backend.

use super::app::{create, fixture, settled};
use super::publish::{layer, resume};
use crate::app::ReviewApp;
use crate::assistant::context::ReviewView;
use crate::testing::backend::TestBackend;
use crate::testing::fake_provider::FakeProvider;
use crate::testing::fixture::GitFixture;
use crate::testing::harness::Scenario;
use bcode_code_review_models::{ReviewGroupKey, ReviewSourceKind};
use std::sync::Arc;

const PATH: &str = "series.txt";

fn commit(repo: &GitFixture, path: &str, content: &str) -> String {
    std::fs::write(repo.root().join(path), content).unwrap();
    repo.run(&["add", "--", path]);
    repo.run(&["commit", "-qm", content.trim(), "--", path]);
    repo.git(&["rev-parse", "HEAD"]).trim().to_owned()
}

fn view(app: &Scenario<ReviewApp>) -> ReviewView {
    app.surface().view_context().review.expect("review context")
}

fn key(app: &mut Scenario<ReviewApp>, key: &str) {
    app.key(key);
    settled(app);
}

// A source banner can be the initial cursor stop. Drive the cursor onto its file before
// commenting, marking, or asking for the current file; never construct an anchor by hand.
fn focus_file(app: &mut Scenario<ReviewApp>) {
    for _ in 0..16 {
        if view(app).file.is_some() {
            return;
        }
        key(app, "j");
    }
    panic!("no current file in the displayed group");
}

fn assert_file(app: &mut Scenario<ReviewApp>, group: &ReviewGroupKey, viewed: usize) {
    focus_file(app);
    let review = view(app);
    assert_eq!(review.kind, "commits");
    assert!(review.stack.is_none() && review.pull_request.is_none());
    assert_eq!(review.files, (1, viewed));
    let file = review.file.unwrap();
    assert_eq!(file.path, PATH);
    // The assistant must have a usable selector in its current-file context. It may retain
    // the canonical source_id alongside that selector, rather than overloading the field.
    let fields = serde_json::to_value(file).unwrap();
    let selector = group.selector();
    assert!(
        fields
            .as_object()
            .unwrap()
            .values()
            .any(|value| { value.as_str() == Some(selector.as_str()) }),
        "missing active selector {selector} in {fields}"
    );
}

// Check actual document cards as well as the assistant's description. Filtering an
// assistant response alone must not make a misplaced inline thread pass this scenario.
#[track_caller]
fn assert_threads(app: &Scenario<ReviewApp>, expected: usize) {
    let doc = app.surface().review().unwrap().document().unwrap();
    let cards = (0..doc.len())
        .filter(|&index| {
            matches!(
                doc.item(index),
                Some(bcode_code_review::document::ReviewItem::Thread(_))
            )
        })
        .count();
    assert_eq!(cards, expected, "visible thread cards");
    let threads = view(app).threads;
    assert_eq!(
        threads.on_file.total, expected,
        "current file's thread list"
    );
    assert_eq!(threads.open, expected, "current file's open threads");
}

#[test]
fn same_path_marks_and_drafts_follow_the_member_across_views_and_reopen() {
    let repo = fixture();
    let first = commit(&repo, PATH, "first\n");
    let second = commit(&repo, PATH, "second\n");
    let backend = Arc::new(TestBackend::default());
    let source = ReviewSourceKind::Commits {
        revs: vec![first.clone(), second.clone()],
    };
    let workspace = create(&backend, &repo, "Two edits", vec![source]);
    let id = &workspace.id;
    let source_id = workspace.sources[0].id.clone();
    let member = |member| ReviewGroupKey::Member {
        source_id: source_id.clone(),
        member,
    };
    let combined = ReviewGroupKey::Combined {
        source_id: source_id.clone(),
    };
    let mut app = resume(&repo, &backend, id);
    assert_file(&mut app, &combined, 0);
    key(&mut app, "shift+o");
    assert_file(&mut app, &member(first.clone()), 0);
    app.key("c");
    app.paste("Only the first edit needs a change.");
    key(&mut app, "ctrl+s");
    assert_threads(&app, 1);
    key(&mut app, "v");
    key(&mut app, ".");
    assert_file(&mut app, &member(second.clone()), 0);
    assert_threads(&app, 0);
    key(&mut app, "shift+o");
    assert_eq!(view(&app).files, (2, 1));
    key(&mut app, "shift+o");
    assert_file(&mut app, &combined, 0);
    assert_threads(&app, 0);
    key(&mut app, "q");
    drop(app);

    let mut reopened = resume(&repo, &backend, id);
    assert_file(&mut reopened, &combined, 0);
    let saved = reopened.surface().review().unwrap().workspace().unwrap();
    assert_eq!(&saved.id, id);
    assert_eq!(saved.sources, workspace.sources);
    key(&mut reopened, "shift+o");
    assert_file(&mut reopened, &member(second), 0);
    key(&mut reopened, ",");
    assert_file(&mut reopened, &member(first), 1);
    // The viewed file reopens folded. Its thread is still on this member, but a
    // folded file correctly has no inline card. Open it before checking the card.
    assert_eq!(view(&reopened).threads.on_file.total, 1);
    let chevron = reopened.frame().find("▸").expect("folded file chevron");
    reopened.click(chevron.x, chevron.y);
    settled(&mut reopened);
    assert_threads(&reopened, 1);
}

#[test]
fn sparse_series_opens_as_members_and_combines_only_selected_changes() {
    let repo = fixture();
    let first = commit(&repo, PATH, "first\n");
    commit(&repo, "omitted.txt", "not selected\n");
    let second = commit(&repo, PATH, "second\n");
    let backend = Arc::new(TestBackend::default());
    let workspace = create(
        &backend,
        &repo,
        "Sparse",
        vec![ReviewSourceKind::Commits {
            revs: vec![first, second],
        }],
    );
    let mut app = resume(&repo, &backend, &workspace.id);
    assert_eq!(view(&app).files, (2, 0));
    key(&mut app, "shift+o");
    assert_file(
        &mut app,
        &ReviewGroupKey::Combined {
            source_id: workspace.sources[0].id.clone(),
        },
        0,
    );
}

#[test]
fn a_single_selected_commit_has_no_alternate_document() {
    let repo = fixture();
    let only = commit(&repo, PATH, "only\n");
    let backend = Arc::new(TestBackend::default());
    let workspace = create(
        &backend,
        &repo,
        "One",
        vec![ReviewSourceKind::Commits { revs: vec![only] }],
    );
    let mut app = resume(&repo, &backend, &workspace.id);
    focus_file(&mut app);
    let before = view(&app);
    key(&mut app, "shift+o");
    key(&mut app, ".");
    let after = view(&app);
    assert_eq!(after.file, before.file);
    assert_eq!(after.files, (1, 0));
    assert!(after.stack.is_none());
}

#[test]
fn legacy_layer_only_workspace_keeps_pr_ids_and_stack_context() {
    let repo = fixture();
    let provider = FakeProvider::new(repo.root());
    provider.close(100);
    let backend = Arc::new(TestBackend::default().with_provider(provider));
    let workspace = create(
        &backend,
        &repo,
        "Before series",
        vec![layer(101), layer(102), layer(103)],
    );
    let mut app = resume(&repo, &backend, &workspace.id);
    for mode in ["one_pr", "all_prs", "whole", "one_pr"] {
        let current = view(&app);
        assert_eq!(current.id.as_ref(), Some(&workspace.id));
        let stack = current.stack.expect("forge-backed stack context");
        assert_eq!(stack.layers, [101, 102, 103]);
        assert_eq!(stack.view, mode);
        for (original, number) in workspace.sources.iter().zip([101, 102, 103]) {
            assert!(
                current
                    .sources
                    .items
                    .iter()
                    .any(|source| { source.id == original.id && source.number == Some(number) })
            );
        }
        key(&mut app, "shift+o");
    }
}
