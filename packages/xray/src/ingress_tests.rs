//! Deterministic admission/barrier coverage; private handles are pause points.
use super::*;
use crate::tests::{event, request};

#[tokio::test]
async fn cross_observation_pump_notifies_sleeping_finalizer_after_settlement() {
    let hub = XrayHub::default();
    let mut first = hub.observe(request("first", false)).unwrap();
    let mut second = hub.observe(request("second", false)).unwrap();
    let event = event("first", "pending", false);
    let mut publication = hub.admit_event(&event).unwrap();
    let sender = publication.sender();
    let bytes = hub
        .inner
        .ingress_budget
        .bytes(1, hub.inner.config.max_ingress_bytes)
        .unwrap();
    let command = publication.command(event, bytes);
    publication.published();
    drop(publication);
    // Hold the pending command outside the queue: publisher retirement has already
    // happened and the first finalizer must await command settlement, not a writer.
    let completion = first.finalize_complete();
    tokio::pin!(completion);
    std::future::poll_fn(|cx| {
        assert!(completion.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    sender
        .try_send(command)
        .unwrap_or_else(|_| panic!("fixture ingress has capacity"));
    // Register after publication to distinguish settlement's wake from channel
    // publication's wake. The second observation now drains the first's command.
    let notify = Arc::clone(&hub.inner.ingress_budget.notify);
    let settled = notify.notified();
    tokio::pin!(settled);
    settled.as_mut().enable();
    assert_eq!(second.recv().await.unwrap().event.name, "ready");
    let receive = second.recv();
    tokio::pin!(receive);
    std::future::poll_fn(|cx| {
        assert!(receive.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    std::future::poll_fn(|cx| {
        assert!(settled.as_mut().poll(cx).is_ready());
        assert!(completion.as_mut().poll(cx).is_ready());
        Poll::Ready(())
    })
    .await;
}

#[tokio::test]
async fn paused_admission_does_not_exclude_another_writer_or_require_state() {
    let hub = XrayHub::default();
    let mut observation = hub.observe(request("s", false)).unwrap();
    let first = event("s", "first", false);
    let mut paused = hub.admit_event(&first).unwrap();
    let state = hub.state().unwrap();
    // The ordinary state lock is deliberately held throughout B's emit.
    hub.emit(event("s", "second", false));
    drop(state);
    hub.publish_event(&mut paused, first);
    drop(paused);
    let receipt = observation.finalize().unwrap();
    assert_eq!(receipt.hub_contention_drops, 0);
    assert_eq!(observation.recv().await.unwrap().event.name, "ready");
    assert_eq!(observation.recv().await.unwrap().event.name, "second");
    assert_eq!(observation.recv().await.unwrap().event.name, "first");
}

#[tokio::test]
async fn cancelled_completion_keeps_cutoff_and_resumes_same_receipt() {
    let hub = XrayHub::default();
    let mut observation = hub.observe(request("s", false)).unwrap();
    let event = event("s", "admitted", false);
    let mut paused = hub.admit_event(&event).unwrap();
    {
        let future = observation.finalize_complete();
        tokio::pin!(future);
        std::future::poll_fn(|cx| {
            assert!(future.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
    }
    assert!(!hub.capture_active("s"));
    hub.publish_event(&mut paused, event);
    drop(paused);
    let receipt = observation.finalize_complete().await.unwrap();
    assert_eq!(receipt.final_sequence, 3);
    assert_eq!(observation.finalize().unwrap(), receipt);
}

#[tokio::test]
async fn admission_snapshot_excludes_new_attachment_and_survives_cutoff() {
    let hub = XrayHub::default();
    let mut old = hub.observe(request("s", false)).unwrap();
    let event = event("s", "old_generation", false);
    let mut paused = hub.admit_event(&event).unwrap();
    let mut new = hub.observe(request("s", false)).unwrap();
    assert_eq!(old.finalize(), Err(XrayError::FinalizePending));
    hub.publish_event(&mut paused, event);
    drop(paused);
    assert_eq!(old.finalize().unwrap().final_sequence, 3);
    assert_eq!(new.finalize().unwrap().final_sequence, 2);
    assert_eq!(old.recv().await.unwrap().event.name, "ready");
    assert_eq!(old.recv().await.unwrap().event.name, "old_generation");
}

#[tokio::test]
async fn ingress_full_is_uncertainty_but_delivery_full_is_exact_gap() {
    let hub = XrayHub::new(XrayConfig {
        ingress_capacity: 2,
        queue_capacity: 1,
        ..XrayConfig::default()
    });
    let mut observation = hub.observe(request("s", false)).unwrap();
    let mut other = hub.observe(request("other", false)).unwrap();
    hub.emit(event("s", "one", false));
    hub.emit(event("s", "two", false));
    hub.emit(event("s", "presequence", false));
    let receipt = observation.finalize().unwrap();
    assert_eq!(receipt.hub_contention_drops, 1);
    assert_eq!(other.finalize().unwrap().hub_contention_drops, 1);
    assert_eq!(receipt.dropped_events, 1);
    assert_eq!(receipt.final_sequence, 4);
    assert_eq!(observation.recv().await.unwrap().event.name, "ready");
    assert_eq!(observation.recv().await.unwrap().event.name, "one");
    let gap = observation.recv().await.unwrap();
    assert_eq!(gap.event.name, "gap");
    assert_eq!(gap.event.data["first_sequence"], 3);
    assert_eq!(gap.event.data["last_sequence"], 3);
}

#[test]
fn abandoned_publication_releases_barrier_and_accounts_loss() {
    let hub = XrayHub::default();
    let mut observation = hub.observe(request("s", false)).unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _paused = hub.admit_event(&event("s", "unwind", false)).unwrap();
        panic!("fixture abandoned publisher");
    }));
    assert!(result.is_err());
    let receipt = observation.finalize().unwrap();
    assert_eq!(receipt.hub_contention_drops, 1);
    assert_eq!(receipt.final_sequence, 2);
}

#[test]
fn inflight_and_serialized_byte_limits_are_real_resource_limits() {
    let hub = XrayHub::new(XrayConfig {
        max_inflight_operations: 2,
        ..XrayConfig::default()
    });
    let mut observation = hub.observe(request("s", false)).unwrap();
    let first = hub.admit_event(&event("s", "one", false)).unwrap();
    let second = hub.admit_event(&event("s", "two", false)).unwrap();
    assert!(hub.admit_event(&event("s", "three", false)).is_none());
    drop(first);
    drop(second);
    assert_eq!(observation.finalize().unwrap().hub_contention_drops, 3);
    let budget = ingress::Budget::new();
    let bytes = budget.bytes(8, 8).unwrap();
    assert!(budget.bytes(1, 8).is_none());
    drop(bytes);
    assert!(budget.bytes(8, 8).is_some());
}

#[tokio::test]
async fn pre_expiry_admission_survives_later_expiry_before_publication() {
    let hub = XrayHub::default();
    let mut observation = hub.observe(request("s", false)).unwrap();
    let event = event("s", "before_expiry", false);
    let mut paused = hub.admit_event(&event).unwrap();
    // Cross expiry after the captured admission instant, without sleeping. Leave
    // the hint stale so consumer authority, not hint freshness, decides retention.
    hub.state()
        .unwrap()
        .observers
        .get_mut(observation.id())
        .unwrap()
        .deadline = Some(Instant::now());
    hub.publish_event(&mut paused, event);
    drop(paused);
    let receipt = observation.finalize().unwrap();
    assert_eq!(receipt.final_sequence, 3);
    assert_eq!(receipt.dropped_events, 0);
    assert_eq!(observation.recv().await.unwrap().event.name, "ready");
    assert_eq!(
        observation.recv().await.unwrap().event.name,
        "before_expiry"
    );
}

#[test]
fn paused_global_loss_reporter_prevents_frozen_cutoff_until_accounted() {
    let hub = XrayHub::default();
    let mut observation = hub.observe(request("s", false)).unwrap();
    // Same guard/order as correlation_active: register before the failed read,
    // then pause after observing contention but before recording its uncertainty.
    let reporter = hub.inner.ingress_budget.reporter();
    let lifecycle = hub.inner.activity.write().unwrap();
    assert!(hub.inner.activity.try_read().is_err());
    drop(lifecycle);
    assert_eq!(observation.finalize(), Err(XrayError::FinalizePending));
    // A completion retry must not require a lifecycle write lock or rebuild.
    let lifecycle = hub.inner.activity.read().unwrap();
    assert_eq!(observation.finalize(), Err(XrayError::FinalizePending));
    drop(lifecycle);
    hub.record_loss(LossCause::ActivityHint, 1);
    drop(reporter);
    let receipt = observation.finalize().unwrap();
    assert_eq!(receipt.hub_contention_drops, 1);
    assert_eq!(receipt.dropped_events, 0);
    hub.report_external_loss(2);
    assert_eq!(observation.finalize().unwrap(), receipt);
}

#[test]
fn last_detach_retires_old_channel_even_with_paused_publisher() {
    let hub = XrayHub::default();
    let observation = hub.observe(request("s", false)).unwrap();
    let event = event("s", "detached", false);
    let mut paused = hub.admit_event(&event).unwrap();
    hub.emit(event.clone());
    drop(observation);
    let mut replacement = hub.observe(request("s", false)).unwrap();
    hub.publish_event(&mut paused, event);
    drop(paused);
    assert_eq!(replacement.finalize().unwrap().final_sequence, 2);
}
