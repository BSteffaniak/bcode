//! What the assistant is told about the screen: the view context of a stack step-through with
//! a comparison, of a single pull request and of home; nothing from a place the user left
//! survives moving layer, file, comparison or screen; the blocks a message carries.

use super::app::{create, fixture, settled};
use super::compare::reopened_after_a_restack;
use super::publish::resume;
use crate::app::RouteName;
use crate::assistant::context::{DETAIL_LABEL, VIEW_LABEL, ViewContext};
use crate::testing::backend::TestBackend;
use crate::testing::fake_provider::{FAKE_PROVIDER, FAKE_REPOSITORY, FakeProvider};
use std::sync::Arc;

fn review_of(context: &ViewContext) -> &crate::assistant::context::ReviewView {
    context.review.as_ref().expect("a review")
}

#[test]
fn a_stack_step_through_with_a_comparison_is_described_and_follows_every_move() {
    let (_fixture, backend, id, mut scenario) = reopened_after_a_restack(TestBackend::default());
    settled(&mut scenario);
    let context = scenario.surface().view_context();
    assert_eq!(context.screen, "review");
    let review = review_of(&context);
    assert_eq!(
        (
            review.id.as_deref(),
            review.kind.as_str(),
            review.sources.total
        ),
        (Some(id.as_str()), "stack_step_through", 3)
    );
    let stack = review.stack.as_ref().expect("stack");
    assert_eq!(stack.layers, vec![101, 102, 103]);
    assert_eq!(stack.view, "one_pr");
    let first = stack.current.expect("a current layer");
    let comparison = review.comparison.as_ref().expect("a comparison");
    assert_eq!(comparison.baseline, "last_looked");
    assert_eq!(comparison.view, "full");
    let states: Vec<(u64, &str)> = comparison
        .layers
        .iter()
        .map(|layer| (layer.number, layer.state.as_str()))
        .collect();
    assert_eq!(
        states,
        vec![
            (101, "changed"),
            (102, "rebased_only"),
            (103, "rebased_only")
        ]
    );
    let changed = &comparison.layers[0];
    assert!(
        changed.from.is_some(),
        "the baseline each layer resolves to"
    );
    assert!(
        changed
            .files
            .iter()
            .any(|file| file.path == "restacked.txt" && file.since == "new"),
        "{changed:?}"
    );
    let view = context.view_text();
    for expected in [
        "screen: review",
        "kind=stack_step_through",
        "stack: layers=#101,#102,#103",
        "view=one_pr",
        "comparison: baseline=last_looked  view=full",
        "#101 changed 1 file",
        "#102 rebased_only",
        "counts: changed=1 rebased=2 unchanged=0",
        "more: bcode.code-review.current_view",
    ] {
        assert!(view.contains(expected), "{expected:?} in:\n{view}");
    }
    let detail = context.detail_text();
    assert!(detail.contains("restacked.txt new"), "{detail}");

    // Next layer: the current layer and file follow; the previous ones are gone.
    scenario.key(".");
    settled(&mut scenario);
    let moved = scenario.surface().view_context();
    let stack = review_of(&moved).stack.as_ref().expect("stack");
    assert_ne!(stack.current, Some(first), "the layer moved");
    assert_ne!(moved.screen_generation, context.screen_generation);

    // Down into a file: the file and cursor describe where the cursor is now.
    scenario.keys(&["j", "j", "j"]);
    settled(&mut scenario);
    let in_file = scenario.surface().view_context();
    let file = review_of(&in_file).file.clone();
    if let Some(file) = &file {
        assert!(
            in_file
                .view_text()
                .contains(&format!("file: {}", file.path))
        );
    }

    // Only what changed: the comparison says so at once.
    scenario.key("shift+l");
    settled(&mut scenario);
    let only_changes = scenario.surface().view_context();
    let comparison = review_of(&only_changes)
        .comparison
        .as_ref()
        .expect("comparison");
    assert_eq!(comparison.view, "changes");
    assert!(only_changes.view_text().contains("view=changes"));

    // Home: nothing of the review remains.
    assert_home_keeps_nothing_of_the_review(&mut scenario);
    drop(backend);
}

/// Go home and check no field of the review survives.
fn assert_home_keeps_nothing_of_the_review(
    scenario: &mut crate::testing::harness::Scenario<crate::app::ReviewApp>,
) {
    scenario.key("q");
    settled(scenario);
    assert_eq!(scenario.surface().route_name(), RouteName::Home);
    let home = scenario.surface().view_context();
    assert_eq!(home.screen, "home");
    assert!(home.review.is_none());
    let view = home.view_text();
    for gone in [
        "review:",
        "stack:",
        "comparison:",
        "file:",
        "cursor:",
        "#101",
    ] {
        assert!(!view.contains(gone), "{gone:?} left over in:\n{view}");
    }
    assert!(view.contains("reviews: needs_attention="), "{view}");
    assert_eq!(home.detail_text(), "", "home has no detail block");
}

#[test]
fn a_single_pull_request_is_described_as_one() {
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
    let context = scenario.surface().view_context();
    let review = review_of(&context);
    assert_eq!(review.kind, "pull_request");
    assert!(review.stack.is_none(), "no stack for one pull request");
    assert_eq!(review.sources.items[0].number, Some(101));
    assert!(
        review.sources.items[0].head.is_some(),
        "the head it resolved to"
    );
    let view = context.view_text();
    assert!(view.contains("kind=pull_request"), "{view}");
    assert!(view.contains("sources: 1  #101"), "{view}");
    assert!(!view.contains("stack:"), "{view}");
}

#[test]
fn every_message_carries_a_stored_view_block_and_a_request_only_detail_block() {
    let mut setup = super::assistant::setup(160, 40, true);
    // Select a range of lines, then ask.
    for _ in 0..60 {
        let on_line = matches!(
            setup.app().review().and_then(|review| review.cursor()),
            Some(
                bcode_code_review::document::ItemKey::Line { .. }
                    | bcode_code_review::document::ItemKey::LinePair { .. }
            )
        );
        if on_line {
            break;
        }
        setup.scenario.key("j");
        setup.settle();
    }
    setup.scenario.keys(&["shift+v", "j"]);
    setup.settle();
    let review = setup.app().review().expect("review");
    assert!(review.selection().is_some(), "a range is selected");
    setup.scenario.key("ctrl+k");
    setup.send("What does this do?");
    let (_, request) = setup.backend.host().turns().last().cloned().expect("turn");
    assert_eq!(request.text, "What does this do?");
    let labels: Vec<(&str, bool)> = request
        .context
        .iter()
        .map(|block| (block.label.as_str(), block.request_only))
        .collect();
    assert_eq!(labels, vec![(VIEW_LABEL, false), (DETAIL_LABEL, true)]);
    let view = &request.context[0].text;
    assert!(view.contains("screen: review"), "{view}");
    assert!(view.contains("cursor: "), "{view}");
    assert!(view.contains("selection="), "{view}");
    let detail = &request.context[1].text;
    assert!(detail.starts_with("selection: "), "{detail}");

    // Detached: the screen and review only, and no detail block.
    setup.scenario.key("ctrl+x");
    setup.send("And in general?");
    let (_, request) = setup.backend.host().turns().last().cloned().expect("turn");
    assert_eq!(request.context.len(), 1);
    assert!(request.context[0].text.contains("context: detached"));
    assert!(!request.context[0].text.contains("cursor:"));
}

#[test]
fn a_message_whose_request_only_context_was_lost_is_sent_again_once() {
    let mut setup = super::assistant::setup(160, 40, true);
    setup.scenario.key("ctrl+k");
    let session = setup.send("First");
    setup
        .backend
        .host()
        .fail_next_submit(crate::host::REQUEST_ONLY_CONTEXT_UNAVAILABLE);
    let again = setup.send("Second");
    assert_eq!(again, session);
    let texts: Vec<String> = setup
        .backend
        .host()
        .turns()
        .into_iter()
        .map(|(_, request)| request.text)
        .collect();
    assert_eq!(texts, vec!["First".to_owned(), "Second".to_owned()]);
    assert_eq!(setup.app().assistant().notice(), None);
    // A second loss in a row is reported, not retried forever.
    for _ in 0..2 {
        setup
            .backend
            .host()
            .fail_next_submit(crate::host::REQUEST_ONLY_CONTEXT_UNAVAILABLE);
    }
    setup.scenario.paste("Third");
    setup.scenario.key("enter");
    setup.settle();
    assert_eq!(setup.backend.host().turns().len(), 2);
    assert!(
        setup
            .app()
            .assistant()
            .notice()
            .is_some_and(|notice| notice.contains("could not send")),
        "{:?}",
        setup.app().assistant().notice()
    );
}
