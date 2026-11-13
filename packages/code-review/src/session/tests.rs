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

/// The sources of the `Ready` (or, with `pending`, `Pending`) events, in order.
fn reported(events: &[OpenEvent], pending: bool) -> Vec<&OpenedSource> {
    events
        .iter()
        .filter_map(|event| match event {
            OpenEvent::Ready(opened) if !pending => Some(opened),
            OpenEvent::Pending(opened) if pending => Some(opened),
            _ => None,
        })
        .collect()
}

fn ids<'a>(sources: &[&'a OpenedSource]) -> Vec<&'a str> {
    sources
        .iter()
        .map(|opened| opened.source.source_id.as_str())
        .collect()
}

fn failures(events: &[OpenEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match event {
            OpenEvent::Failed(diagnostic) => Some(diagnostic.source_id.as_str()),
            _ => None,
        })
        .collect()
}

fn layer_count(events: &[OpenEvent]) -> Vec<usize> {
    events
        .iter()
        .filter_map(|event| match event {
            OpenEvent::Stack(news) => Some(news.stack.layers.len()),
            _ => None,
        })
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
            cancel: crate::provider::Cancel::default(),
        },
    );
    // The stack, every other layer pending, then the asked-for layer listed.
    let [
        OpenEvent::Stage(OpenStage::AskingProviders),
        OpenEvent::Stack(news),
        OpenEvent::Pending(_),
        OpenEvent::Pending(_),
        OpenEvent::Pending(_),
        OpenEvent::Stage(OpenStage::OpeningFirst),
        OpenEvent::Ready(first),
        OpenEvent::Stage(OpenStage::Confirming),
        OpenEvent::Stage(OpenStage::Listing { done: 0, total: 3 }),
        ..,
        OpenEvent::Stage(OpenStage::Listing { done: 3, total: 3 }),
        OpenEvent::Done,
    ] = events.as_slice()
    else {
        panic!("unexpected events: {events:#?}");
    };
    assert_eq!(news.stack.layers.len(), 4);
    assert_eq!(first.source.source_id, "layer-102");
    assert_ne!(first.entries.len(), 0);
    let pending = reported(&events, true);
    assert_eq!(ids(&pending), ["layer-100", "layer-101", "layer-103"]);
    assert!(pending.iter().all(|pending| pending.entries.is_empty()));
    // Then the others, nearest to the first first, with the heads the placeholders had.
    let ready = reported(&events, false);
    assert_eq!(
        ids(&ready),
        ["layer-102", "layer-101", "layer-103", "layer-100"]
    );
    assert_eq!(failures(&events), Vec::<&str>::new());
    for placeholder in pending {
        let opened = ready
            .iter()
            .find(|opened| opened.source.source_id == placeholder.source.source_id)
            .expect("opened");
        assert_eq!(placeholder.source.head, opened.source.head);
        let layers = |opened: &OpenedSource| {
            opened.stack.as_ref().map(|stack| {
                stack
                    .layers
                    .iter()
                    .map(|layer| layer.number)
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(layers(placeholder), layers(opened));
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
        cancel: crate::provider::Cancel::default(),
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
    assert_eq!(reported(&events, false).len(), 4);
    assert_eq!(events.last(), Some(&OpenEvent::Done));

    // A review of one source is ready in one step, with nothing pending.
    let one = OpenRequest {
        sources: layer_sources().into_iter().take(1).collect(),
        ..request
    };
    let events = open_all(&data, &registry, &one);
    assert_eq!(ids(&reported(&events, false)), ["layer-100"]);
    assert_eq!(reported(&events, true).len(), 0);
    assert_eq!(events.last(), Some(&OpenEvent::Done));
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
        cancel: crate::provider::Cancel::default(),
    };
    let count = |operation: &str| {
        fake.calls()
            .iter()
            .filter(|call| call.starts_with(operation))
            .count()
    };
    let calls = || {
        (
            count(OP_REVIEW_SOURCE_STACK_RESOLVE),
            count(OP_REVIEW_SOURCE_PULLS_GET),
        )
    };
    // The first open resolves the stack; with no freshness at all, even that is confirmed when
    // the rest is asked about.
    open_all(&data, &registry, &request);
    assert_eq!(calls(), (1, 1));

    // Reopening shows the remembered stack at once, then confirms it with one request.
    let events = open_all(&data, &registry, &request);
    assert_eq!(calls(), (1, 2));
    assert_eq!(reported(&events, false).len(), 4);
    // Nothing moved, so the first layer was not listed again.
    assert!(events.contains(&OpenEvent::Stage(OpenStage::Listing { done: 0, total: 3 })));

    // The top layer was closed: confirming shows the stack changed, so it is resolved again
    // (not confirmed twice) and the closed layer's source fails while the others open.
    fake.close(103);
    let events = open_all(&data, &registry, &request);
    // The stack once for the open layers, and once more for the closed layer's own source, which
    // is no longer in it.
    assert_eq!(calls(), (3, 3));
    let mut ready = ids(&reported(&events, false));
    ready.sort_unstable();
    assert_eq!(ready, ["layer-100", "layer-101", "layer-102"]);
    assert_eq!(failures(&events), ["layer-103"]);
    // The remembered stack, then the confirmed one.
    assert_eq!(layer_count(&events), [4, 3]);
}

#[test]
fn a_layer_opened_on_top_of_a_remembered_stack_is_found_when_confirming() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let fake = Arc::new(FakeProvider::new(fixture.root()));
    let registry = registry(&fake).with_stack_freshness(std::time::Duration::ZERO);
    let data = ReviewData::default();
    // The stack has three layers when it is first opened.
    fake.close(103);
    let request = OpenRequest {
        repo_path: fixture.root().to_path_buf(),
        sources: layer_sources().into_iter().take(3).collect(),
        first: Some("layer-100".into()),
        max_layers: None,
        cancel: crate::provider::Cancel::default(),
    };
    let events = open_all(&data, &registry, &request);
    assert_eq!(layer_count(&events), [3]);
    // #103 is opened on top of it; the next open shows the remembered three layers at once, and
    // confirming them sees #103 on top, so the stack is resolved again with it.
    fake.reopen(103);
    let events = open_all(&data, &registry, &request);
    assert_eq!(layer_count(&events), [3, 4]);
    let resolves = fake
        .calls()
        .iter()
        .filter(|call| call.starts_with(OP_REVIEW_SOURCE_STACK_RESOLVE))
        .count();
    assert_eq!(resolves, 2);
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
        cancel: crate::provider::Cancel::default(),
    };
    // The review closes as soon as its first source is ready.
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

#[test]
fn a_cancelled_open_stops_and_its_provider_sees_it() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let fake = Arc::new(FakeProvider::new(fixture.root()));
    let registry = registry(&fake).with_stack_freshness(std::time::Duration::ZERO);
    let data = ReviewData::default();
    let request = OpenRequest {
        repo_path: fixture.root().to_path_buf(),
        sources: layer_sources(),
        first: Some("layer-102".into()),
        max_layers: None,
        cancel: crate::provider::Cancel::default(),
    };
    // Cancelled before it starts: the provider is told, and nothing is listed.
    request.cancel.cancel();
    let events = open_all(&data, &registry, &request);
    assert_eq!(
        events,
        [OpenEvent::Stage(OpenStage::AskingProviders)],
        "{events:#?}"
    );
    assert!(
        fake.calls()
            .contains(&format!("{OP_REVIEW_SOURCE_STACK_RESOLVE} cancelled")),
        "{:?}",
        fake.calls()
    );

    // Cancelled once the stack is shown: the stack is not confirmed, and the open stops there.
    open_all(
        &data,
        &registry,
        &OpenRequest {
            cancel: crate::provider::Cancel::default(),
            ..request.clone()
        },
    );
    let confirmations = |fake: &FakeProvider| {
        fake.calls()
            .iter()
            .filter(|call| call.starts_with(OP_REVIEW_SOURCE_PULLS_GET))
            .count()
    };
    let before = confirmations(&fake);
    let cancel = crate::provider::Cancel::default();
    let request = OpenRequest {
        cancel: cancel.clone(),
        ..request
    };
    let mut events = Vec::new();
    open(&data, &registry, &request, |event| {
        if matches!(event, OpenEvent::Ready(_)) {
            cancel.cancel();
        }
        events.push(event);
        std::ops::ControlFlow::Continue(())
    })
    .expect("open");
    assert!(
        matches!(events.last(), Some(OpenEvent::Ready(_))),
        "{events:#?}"
    );
    assert_eq!(confirmations(&fake), before);
}
