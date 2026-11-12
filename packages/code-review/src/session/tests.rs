//! Progressive opening over the fixture's stack (#100 to #103) served by the fake provider.

use super::*;
use crate::provider::{JsonReviewSource, ReviewSourceProvider};
use crate::testing::fake_provider::{FAKE_PROVIDER, FAKE_PULLS, FAKE_REPOSITORY, FakeProvider};
use crate::testing::fixture::{FixtureSpec, GitFixture};
use bcode_code_review_models::{
    OP_REVIEW_SOURCE_MANIFEST, OP_REVIEW_SOURCE_PULLS_GET, OP_REVIEW_SOURCE_STACK_RESOLVE,
    ReviewSourceProviderManifest,
};
use std::sync::Arc;

fn registry(fake: &Arc<FakeProvider>) -> ProviderRegistry {
    let manifest: ReviewSourceProviderManifest = serde_json::from_slice(
        &fake
            .answer(OP_REVIEW_SOURCE_MANIFEST, b"{}")
            .expect("manifest"),
    )
    .expect("manifest");
    let served = Arc::clone(fake);
    let provider: Arc<dyn ReviewSourceProvider> = Arc::new(JsonReviewSource::new(
        manifest,
        Box::new(move |operation, payload| served.answer(operation, &payload)),
    ));
    ProviderRegistry::new().with_source(provider)
}

fn layer_sources() -> Vec<ReviewSource> {
    FAKE_PULLS
        .iter()
        .map(|number| ReviewSource {
            id: format!("layer-{number}"),
            kind: ReviewSourceKind::StackLayer {
                provider: FAKE_PROVIDER.into(),
                repository: FAKE_REPOSITORY.into(),
                number: *number,
            },
            label: format!("#{number}"),
            included: true,
        })
        .collect()
}

fn ids(sources: &[OpenedSource]) -> Vec<&str> {
    sources
        .iter()
        .map(|opened| opened.source.source_id.as_str())
        .collect()
}

fn open_all(
    data: &ReviewData,
    registry: &ProviderRegistry,
    request: &OpenRequest,
) -> Vec<OpenEvent> {
    let mut events = Vec::new();
    open(data, registry, request, |event| {
        events.push(event);
        std::ops::ControlFlow::Continue(())
    })
    .expect("open");
    events
}

#[test]
fn the_first_source_is_ready_before_the_rest_with_the_stack_pending() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let fake = Arc::new(FakeProvider::new(fixture.root()));
    let registry = registry(&fake);
    let data = ReviewData::default();
    let events = open_all(
        &data,
        &registry,
        &OpenRequest {
            repo_path: fixture.root().to_path_buf(),
            sources: layer_sources(),
            first: Some("layer-102".into()),
            max_layers: None,
        },
    );
    let [
        OpenEvent::Stage(OpenStage::AskingProviders),
        OpenEvent::Stage(OpenStage::OpeningFirst),
        OpenEvent::Ready(ready),
        OpenEvent::Stage(OpenStage::AskingProviders),
        OpenEvent::Stage(OpenStage::Listing { done: 0, total: 3 }),
        ..,
        OpenEvent::Rest(rest),
    ] = events.as_slice()
    else {
        panic!("unexpected events: {events:#?}");
    };
    assert!(events.contains(&OpenEvent::Stage(OpenStage::Listing { done: 3, total: 3 })));
    // The asked-for layer first, listed; every other layer pending with its stack, no files.
    assert_eq!(ids(&ready.ready), ["layer-102"]);
    assert_ne!(ready.ready[0].entries.len(), 0);
    assert_eq!(ids(&ready.pending), ["layer-100", "layer-101", "layer-103"]);
    assert!(
        ready
            .pending
            .iter()
            .all(|pending| pending.entries.is_empty())
    );
    // Then every layer in workspace order, listed, with the same heads the placeholders had.
    assert_eq!(
        ids(&rest.ready),
        ["layer-100", "layer-101", "layer-102", "layer-103"]
    );
    assert!(rest.pending.is_empty() && rest.diagnostics.is_empty());
    for pending in &ready.pending {
        let opened = rest
            .ready
            .iter()
            .find(|opened| opened.source.source_id == pending.source.source_id)
            .expect("opened");
        assert_eq!(pending.source.head, opened.source.head);
        let layers = |opened: &OpenedSource| {
            opened.stack.as_ref().map(|stack| {
                stack
                    .layers
                    .iter()
                    .map(|layer| layer.number)
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(layers(pending), layers(opened));
    }
}

#[test]
fn a_recently_resolved_stack_is_not_resolved_again_and_one_source_is_ready_at_once() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let fake = Arc::new(FakeProvider::new(fixture.root()));
    let registry = registry(&fake);
    let data = ReviewData::default();
    let request = OpenRequest {
        repo_path: fixture.root().to_path_buf(),
        sources: layer_sources(),
        first: None,
        max_layers: None,
    };
    let resolves = |fake: &FakeProvider| {
        fake.calls()
            .iter()
            .filter(|call| call.starts_with(OP_REVIEW_SOURCE_STACK_RESOLVE))
            .count()
    };
    open_all(&data, &registry, &request);
    assert_eq!(resolves(&fake), 1);
    // Opening again (for example right after the wizard resolved it) reuses the stack.
    let events = open_all(&data, &registry, &request);
    assert_eq!(resolves(&fake), 1);
    assert!(matches!(events.last(), Some(OpenEvent::Rest(_))));

    // A review of one source is ready in one step.
    let one = OpenRequest {
        sources: layer_sources().into_iter().take(1).collect(),
        ..request
    };
    let events = open_all(&data, &registry, &one);
    assert!(
        matches!(events.as_slice(), [.., OpenEvent::Ready(ready)] if ready.pending.is_empty()),
        "{events:#?}"
    );
}

#[test]
fn a_remembered_stack_is_confirmed_in_one_request_and_resolved_again_when_it_changed() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let fake = Arc::new(FakeProvider::new(fixture.root()));
    // Every remembered stack is past its freshness, so each open confirms it.
    let registry = registry(&fake).with_stack_freshness(std::time::Duration::ZERO);
    let data = ReviewData::default();
    let request = OpenRequest {
        repo_path: fixture.root().to_path_buf(),
        sources: layer_sources(),
        first: Some("layer-101".into()),
        max_layers: None,
    };
    let count = |operation: &str| {
        fake.calls()
            .iter()
            .filter(|call| call.starts_with(operation))
            .count()
    };
    // The first open resolves the stack; with no freshness at all, even that is confirmed when
    // the rest is asked about.
    open_all(&data, &registry, &request);
    assert_eq!(
        (
            count(OP_REVIEW_SOURCE_STACK_RESOLVE),
            count(OP_REVIEW_SOURCE_PULLS_GET)
        ),
        (1, 1)
    );

    // Reopening shows the remembered stack at once, then confirms it with one request.
    let events = open_all(&data, &registry, &request);
    assert_eq!(
        (
            count(OP_REVIEW_SOURCE_STACK_RESOLVE),
            count(OP_REVIEW_SOURCE_PULLS_GET)
        ),
        (1, 2)
    );
    let Some(OpenEvent::Rest(rest)) = events.last() else {
        panic!("{events:#?}");
    };
    assert_eq!(rest.ready.len(), 4);
    // Nothing moved, so the first layer was not listed again.
    assert!(events.contains(&OpenEvent::Stage(OpenStage::Listing { done: 0, total: 3 })));

    // The top layer was closed: confirming shows the stack changed, so it is resolved again
    // (not confirmed twice) and the closed layer's source fails while the others open.
    fake.close(103);
    let events = open_all(&data, &registry, &request);
    assert_eq!(
        (
            count(OP_REVIEW_SOURCE_STACK_RESOLVE),
            count(OP_REVIEW_SOURCE_PULLS_GET)
        ),
        // The stack once for the open layers, and once more for the closed layer's own source,
        // which is no longer in it.
        (3, 3)
    );
    let Some(OpenEvent::Rest(rest)) = events.last() else {
        panic!("{events:#?}");
    };
    assert_eq!(ids(&rest.ready), ["layer-100", "layer-101", "layer-102"]);
    assert_eq!(rest.diagnostics.len(), 1, "{:#?}", rest.diagnostics);
}

#[test]
fn a_stopped_open_reports_nothing_more_and_asks_providers_nothing_more() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let fake = Arc::new(FakeProvider::new(fixture.root()));
    let registry = registry(&fake);
    let data = ReviewData::default();
    let request = OpenRequest {
        repo_path: fixture.root().to_path_buf(),
        sources: layer_sources(),
        first: Some("layer-102".into()),
        max_layers: None,
    };
    // The review closes as soon as its first frame is ready.
    let mut events = Vec::new();
    open(&data, &registry, &request, |event| {
        let ready = matches!(event, OpenEvent::Ready(_));
        events.push(event);
        if ready {
            std::ops::ControlFlow::Break(())
        } else {
            std::ops::ControlFlow::Continue(())
        }
    })
    .expect("open");
    assert!(
        matches!(events.last(), Some(OpenEvent::Ready(_))),
        "{events:#?}"
    );
    // Nothing was asked after the first frame: the other layers were never fetched.
    let fetches = fake
        .calls()
        .iter()
        .filter(|call| call.starts_with("review_source.pull.fetch"))
        .count();
    assert_eq!(fetches, 3, "{:?}", fake.calls());
}
