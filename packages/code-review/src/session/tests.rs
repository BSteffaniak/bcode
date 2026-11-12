//! Progressive opening over the fixture's stack (#100 to #103) served by the fake provider.

use super::*;
use crate::provider::{JsonReviewSource, ReviewSourceProvider};
use crate::testing::fake_provider::{FAKE_PROVIDER, FAKE_PULLS, FAKE_REPOSITORY, FakeProvider};
use crate::testing::fixture::{FixtureSpec, GitFixture};
use bcode_code_review_models::{
    OP_REVIEW_SOURCE_MANIFEST, OP_REVIEW_SOURCE_STACK_RESOLVE, ReviewSourceProviderManifest,
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
    open(data, registry, request, |event| events.push(event)).expect("open");
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
