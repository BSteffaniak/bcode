//! Behavior of existing PR workspaces as the controller switches series views.

use super::app::{create, fixture, settled, update};
use super::publish::{layer, resume};
use crate::app::ReviewApp;
use crate::publish::Step;
use crate::surface::Modal;
use crate::testing::backend::TestBackend;
use crate::testing::fake_provider::{FAKE_PROVIDER, FAKE_REPOSITORY, FakeProvider};
use crate::testing::fake_publisher::FakePublisher;
use crate::testing::fixture::GitFixture;
use crate::testing::harness::Scenario;
use bcode_code_review::document::ReviewItem;
use bcode_code_review_models::{ReviewSourceKind, ReviewWorkspace};
use std::collections::BTreeSet;
use std::sync::Arc;

fn stack() -> (GitFixture, Arc<TestBackend>, ReviewWorkspace) {
    let fixture = fixture();
    let backend = Arc::new(
        TestBackend::default()
            .with_provider(FakeProvider::new(fixture.root()))
            .with_publisher(FakePublisher::default()),
    );
    let mut sources: Vec<_> = (100..=103).map(layer).collect();
    sources.push(ReviewSourceKind::Stack {
        provider: FAKE_PROVIDER.to_owned(),
        repository: FAKE_REPOSITORY.to_owned(),
        top: 103,
    });
    let workspace = create(&backend, &fixture, "Existing PR stack", sources);
    (fixture, backend, workspace)
}

fn visible_sources(scenario: &Scenario<ReviewApp>) -> BTreeSet<String> {
    let doc = scenario.surface().review().unwrap().document().unwrap();
    (0..doc.len())
        .filter_map(|index| match doc.item(index)? {
            ReviewItem::SourceHeader(header) => Some(header.source_id.clone()),
            item => item.file().map(|file| file.source_id.clone()),
        })
        .collect()
}

#[test]
fn reopening_honors_both_generations_of_stack_preferences() {
    let (fixture, backend, mut workspace) = stack();
    let layers = ["source-1", "source-2", "source-3", "source-4"];
    for (preference, expected) in [
        // These snapshots predate per-series preferences. Schema 1 is the
        // supported presentation format; schema 2 is intentionally ignored.
        (
            r#"{"schema_version":1,"all_stack_layers":true}"#,
            &layers[..],
        ),
        (
            r#"{"schema_version":1,"all_stack_layers":true,"stack_view":"whole"}"#,
            &["source-5"][..],
        ),
    ] {
        workspace.presentation = Some(serde_json::from_str(preference).unwrap());
        update(&backend, &workspace);
        let scenario = resume(&fixture, &backend, &workspace.id);
        assert_eq!(
            visible_sources(&scenario),
            expected.iter().copied().map(str::to_owned).collect(),
            "{preference}"
        );
    }
}

#[test]
fn discovering_layers_keeps_an_explicit_legacy_all_layers_choice() {
    let (fixture, backend, mut workspace) = stack();
    workspace
        .sources
        .retain(|s| matches!(s.kind, ReviewSourceKind::Stack { .. }));
    workspace.presentation =
        Some(serde_json::from_str(r#"{"schema_version":1,"all_stack_layers":true}"#).unwrap());
    update(&backend, &workspace);
    let scenario = resume(&fixture, &backend, &workspace.id);
    let review = scenario.surface().review().unwrap();
    let layers: BTreeSet<_> = review
        .workspace()
        .unwrap()
        .sources
        .iter()
        .filter(|s| matches!(s.kind, ReviewSourceKind::StackLayer { .. }))
        .map(|s| s.id.clone())
        .collect();
    assert_eq!(layers.len(), 4);
    assert_eq!(visible_sources(&scenario), layers);
    let (tabs, selected) = review.rail();
    assert!(tabs[selected.expect("selected layer")].starts_with('#'));
    assert!(tabs.last().unwrap().starts_with("Whole stack #100–#103"));
}

#[test]
fn stepping_and_changing_views_preserve_the_layer_draft_and_its_publish_target() {
    let (fixture, backend, mut workspace) = stack();
    workspace.presentation =
        Some(serde_json::from_str(r#"{"schema_version":1,"stack_view":"one_layer"}"#).unwrap());
    update(&backend, &workspace);
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    scenario.key(".");
    settled(&mut scenario);
    scenario.keys(&["j", "j", "j"]);
    settled(&mut scenario);
    let cursor = scenario
        .surface()
        .review()
        .unwrap()
        .cursor()
        .unwrap()
        .clone();
    assert_eq!(cursor.file().unwrap().source_id, "source-2");
    for key in [".", ","] {
        scenario.key(key);
        settled(&mut scenario);
    }
    assert_eq!(scenario.surface().review().unwrap().cursor(), Some(&cursor));

    let body = "Please clarify this change in PR 101";
    scenario.key("c");
    scenario.paste(body);
    scenario.key("ctrl+s");
    settled(&mut scenario);
    for (expected, status) in [
        (
            &["source-1", "source-2", "source-3", "source-4"][..],
            "every pull request of the stack (shift+o: the whole stack as one diff)",
        ),
        (
            &["source-5"][..],
            "the whole stack as one diff (shift+o: one pull request at a time)",
        ),
        (
            &["source-2"][..],
            "one pull request at a time (shift+o: every pull request of the stack)",
        ),
    ] {
        scenario.key("shift+o");
        assert_eq!(scenario.surface().review().unwrap().status(), Some(status));
        settled(&mut scenario);
        assert_eq!(
            visible_sources(&scenario),
            expected.iter().copied().map(str::to_owned).collect()
        );
    }

    scenario.key("shift+p");
    scenario.key("enter");
    settled(&mut scenario);
    scenario.keys(&["enter", "enter"]);
    settled(&mut scenario);
    let Some(Modal::Publish(flow)) = scenario.surface().review().unwrap().modal() else {
        panic!("expected publish preview");
    };
    assert_eq!(flow.step(), &Step::Preview);
    let [prepared] = flow.prepared() else {
        panic!("expected one bundle, with no duplicate from the alternate view");
    };
    let target = prepared.bundle.pull_request.as_ref().unwrap();
    assert_eq!(target.provider, FAKE_PROVIDER);
    assert_eq!(target.repository, FAKE_REPOSITORY);
    assert_eq!(target.number, 101);
    let [thread] = prepared.bundle.threads.as_slice() else {
        panic!("expected exactly the saved draft");
    };
    assert_eq!(thread.anchor.source_id.as_deref(), Some("source-2"));
    assert_eq!(thread.anchor.file_path, cursor.file().unwrap().path);
    let [comment] = thread.comments.as_slice() else {
        panic!("expected one comment");
    };
    assert_eq!(comment.body, body);
    assert_eq!(backend.publisher().unwrap().submitted().len(), 0);
}
