//! Opt-in Switchy/Simvar acceptance on the real selection. Inputs are prepared outside the
//! app, never discovered from a user's store or inferred from the implementation under test:
//!
//! * `BCODE_SERIES_REPO`: disposable `MoosicBox` clone containing the objects.
//! * `BCODE_SERIES_SOURCES`: JSON of the existing workspace's `sources` array (including its
//!   original source id, label and ordered list of 61 full commit OIDs).
//! * `BCODE_SERIES_EXPECTED_PATHS`: independent UTF-8 manifest, one combined display path per
//!   line, including only paths affected by the selection, not by skipped-only changes.
//! * `BCODE_SERIES_REVIEW_ID`: the original saved review id (value, not a database path).

use super::app::settled;
use crate::app::{ReviewApp, RouteName};
use crate::config::ReviewViewConfig;
use crate::entry::{Entry, TRANSIENT_WORKSPACE_ID};
use crate::testing::backend::TestBackend;
use crate::testing::harness::Scenario;
use bcode_code_review::document::ReviewDocument;
use bcode_code_review::viewed::ViewedState;
use bcode_code_review_models::{
    ReviewGroupKey, ReviewSource, ReviewSourceKind, ReviewTarget, ReviewWorkspace,
    UpdateReviewWorkspaceRequest,
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn input_path(name: &str) -> PathBuf {
    std::env::var_os(name)
        .unwrap_or_else(|| panic!("missing {name}"))
        .into()
}

fn document(app: &Scenario<ReviewApp>) -> &ReviewDocument {
    app.surface()
        .review()
        .unwrap()
        .document()
        .expect("loaded review")
}

fn open(repo: &Path, backend: &TestBackend, id: &str) -> Scenario<ReviewApp> {
    let review = ReviewApp::new(
        backend.services(),
        repo.to_owned(),
        ReviewViewConfig::default(),
        Entry::Resume {
            workspace_id: Some(id.to_owned()),
        },
    )
    .with_scroll_motion(Duration::ZERO);
    let mut app = Scenario::with_shared_host(review, backend.host(), 160, 48);
    settled(&mut app);
    app
}

fn key(app: &mut Scenario<ReviewApp>, key: &str) {
    app.key(key);
    settled(app);
}

fn active(app: &Scenario<ReviewApp>) -> Vec<ReviewGroupKey> {
    document(app)
        .groups()
        .filter(|g| !g.hidden && !g.alternate)
        .map(|g| g.key.clone())
        .collect()
}

fn focus_file(app: &mut Scenario<ReviewApp>) -> String {
    for _ in 0..16 {
        if let Some(file) = app.surface().view_context().review.unwrap().file {
            return file.path;
        }
        key(app, "j");
    }
    panic!("could not reach a file from the group header");
}

fn assert_only_group(app: &Scenario<ReviewApp>, expected: &ReviewGroupKey) {
    assert_eq!(active(app).as_slice(), std::slice::from_ref(expected));
}

fn member_keys(sources: &[ReviewSource]) -> (Vec<ReviewGroupKey>, ReviewGroupKey) {
    let [source] = sources else {
        panic!("expected the single existing Commits source")
    };
    assert!(source.included);
    let ReviewSourceKind::Commits { revs } = &source.kind else {
        panic!("expected Commits")
    };
    assert_eq!(revs.len(), 61);
    assert_eq!(revs.iter().collect::<BTreeSet<_>>().len(), 61);
    assert!(
        revs.iter()
            .all(|oid| oid.len() == 40 && oid.bytes().all(|b| b.is_ascii_hexdigit()))
    );
    let members: Vec<_> = revs
        .iter()
        .map(|oid| ReviewGroupKey::Member {
            source_id: source.id.clone(),
            member: oid.clone(),
        })
        .collect();
    let combined = ReviewGroupKey::Combined {
        source_id: source.id.clone(),
    };
    (members, combined)
}

#[test]
#[ignore = "requires a disposable MoosicBox clone and the externally prepared Switchy/Simvar selection"]
fn switchy_simvar_saved_selection_round_trips_through_the_app() {
    let repo = input_path("BCODE_SERIES_REPO").canonicalize().unwrap();
    let sources: Vec<ReviewSource> =
        serde_json::from_slice(&std::fs::read(input_path("BCODE_SERIES_SOURCES")).unwrap())
            .expect("workspace sources array");
    let id = std::env::var("BCODE_SERIES_REVIEW_ID").expect("original workspace id");
    assert!(!id.trim().is_empty() && id != TRANSIENT_WORKSPACE_ID);
    let (members, combined) = member_keys(&sources);
    let manifest = std::fs::read_to_string(input_path("BCODE_SERIES_EXPECTED_PATHS")).unwrap();
    let expected: BTreeSet<_> = manifest.lines().collect();
    assert_eq!(manifest.lines().count(), 116);
    assert_eq!(expected.len(), 116);
    assert!(!expected.contains(""));

    // Seed only the declarative input under the real id; no user's annotations, preferences
    // or database are imported. Update without a base inserts into the empty temporary store.
    let backend = TestBackend::default();
    let mut workspace = ReviewWorkspace::from_target(repo.clone(), ReviewTarget::LastCommit);
    workspace.id.clone_from(&id);
    workspace.sources.clone_from(&sources);
    workspace.title = "Switchy/Simvar".to_owned();
    let saved = backend.call_now(UpdateReviewWorkspaceRequest {
        repo_path: repo.clone(),
        workspace,
        base: None,
    });
    assert_eq!(saved.workspace.id, id);
    let mut app = open(&repo, &backend, &id);
    assert_eq!(active(&app), members, "all 61 selected commits, in order");
    key(&mut app, "shift+o");
    assert_only_group(&app, &combined);
    let doc = document(&app);
    let group = doc.groups().find(|g| g.key == &combined).unwrap();
    let paths: BTreeSet<_> = doc
        .group_files(group.index)
        .map(|f| f.path.as_str())
        .collect();
    assert_eq!(paths, expected, "a skipped-only path must never appear");
    assert_eq!(group.file_count, 116);
    assert_eq!(app.surface().view_context().review.unwrap().files, (116, 0));
    let marked = focus_file(&mut app);
    key(&mut app, "v");
    assert_eq!(app.surface().view_context().review.unwrap().files, (116, 1));
    key(&mut app, "shift+o");
    assert_only_group(&app, &members[0]);
    key(&mut app, ".");
    assert_only_group(&app, &members[1]);
    key(&mut app, "q");
    assert_eq!(app.surface().route_name(), RouteName::Home);
    drop(app);

    let mut app = open(&repo, &backend, &id);
    let saved = app.surface().review().unwrap().workspace().unwrap();
    assert_eq!(saved.id, id);
    assert_eq!(saved.sources, sources);
    assert_only_group(&app, &members[1]);
    key(&mut app, "shift+o");
    assert_eq!(active(&app), members);
    key(&mut app, "shift+o");
    assert_only_group(&app, &combined);
    let doc = document(&app);
    let group = doc.groups().find(|g| g.key == &combined).unwrap();
    let viewed: Vec<_> = doc
        .group_files(group.index)
        .filter(|f| doc.viewed_state(f).is_some_and(ViewedState::is_viewed))
        .map(|f| f.path.as_str())
        .collect();
    assert_eq!(viewed, [marked.as_str()]);
}
