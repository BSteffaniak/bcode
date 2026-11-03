//! Payload-ingress resource and lifecycle boundaries, without scheduling sleeps.
use super::*;
use crate::tests::{correlation, payload_request, request};

#[tokio::test]
async fn unresolved_prior_payload_boundary_is_busy_and_resumes_after_publication() {
    let hub = XrayHub::default();
    let observation = hub.observe(request("s", true)).unwrap();
    let reference = hub
        .capture_payload(&correlation("s"), "text/plain", b"fixture")
        .unwrap()
        .unwrap();
    let request = payload_request(&observation, &reference);
    let (command, boundary) = {
        let mut state = hub.state().unwrap();
        let boundary = XrayHub::payload_boundary(&state).unwrap();
        let mut context = Context::from_waker(&hub.inner.ingress_budget.waker);
        let Poll::Ready(Some(command)) = state.payload_ingress.poll_recv(&mut context) else {
            panic!("fixture published payload");
        };
        drop(state);
        (command, boundary)
    };
    // Hold the prior command at the publication boundary. It is not absent merely
    // because the bounded consumer pass currently cannot resolve that prefix.
    let mut prior = Some(boundary);
    assert_eq!(
        hub.read_payload_at(&request, &mut prior),
        Err(XrayError::Busy)
    );
    let notified = hub.inner.ingress_budget.notify.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    let sender = hub.inner.payload_sender.read().unwrap().clone();
    sender
        .try_send(command)
        .unwrap_or_else(|_| panic!("fixture capacity"));
    std::future::poll_fn(|cx| {
        assert!(notified.as_mut().poll(cx).is_ready());
        Poll::Ready(())
    })
    .await;
    assert_eq!(
        hub.read_payload_at(&request, &mut prior).unwrap().bytes,
        b"fix"
    );
}

#[tokio::test]
async fn another_observation_settles_payload_and_wakes_pending_cutoff() {
    let hub = XrayHub::default();
    let mut observation = hub.observe(request("s", true)).unwrap();
    let other = hub.observe(request("other", true)).unwrap();
    hub.capture_payload(&correlation("s"), "text/plain", b"fixture")
        .unwrap()
        .unwrap();
    let command = {
        let mut state = hub.state().unwrap();
        let mut context = Context::from_waker(&hub.inner.ingress_budget.waker);
        let Poll::Ready(Some(command)) = state.payload_ingress.poll_recv(&mut context) else {
            panic!("fixture payload");
        };
        drop(state);
        command
    };
    let completion = observation.finalize_complete();
    tokio::pin!(completion);
    std::future::poll_fn(|cx| {
        assert!(completion.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    let sender = hub.inner.payload_sender.read().unwrap().clone();
    sender
        .try_send(command)
        .unwrap_or_else(|_| panic!("fixture capacity"));
    let settled = hub.inner.ingress_budget.notify.notified();
    tokio::pin!(settled);
    settled.as_mut().enable();
    let request = XrayReadPayloadRequest {
        schema_version: XRAY_SCHEMA_VERSION,
        observation_id: other.id().into(),
        session_id: "other".into(),
        payload_id: "unknown".into(),
        offset: 0,
        length: 1,
    };
    assert_eq!(
        hub.read_payload(&request),
        Err(XrayError::PayloadUnavailable)
    );
    std::future::poll_fn(|cx| {
        assert!(settled.as_mut().poll(cx).is_ready());
        assert!(completion.as_mut().poll(cx).is_ready());
        Poll::Ready(())
    })
    .await;
}

#[test]
fn paused_payload_preparation_does_not_exclude_parallel_capture() {
    let hub = XrayHub::default();
    let observation = hub.observe(request("s", true)).unwrap();
    let prepared = hub
        .prepare_payload(&correlation("s"), "text/plain", b"first")
        .unwrap()
        .unwrap();
    let other = hub.clone();
    let second = std::thread::spawn(move || {
        other
            .capture_payload(&correlation("s"), "text/plain", b"second")
            .unwrap()
            .unwrap()
    })
    .join()
    .unwrap();
    let first = hub
        .commit_payload(prepared, &correlation("s"))
        .unwrap()
        .unwrap();
    assert_eq!(
        hub.read_payload(&payload_request(&observation, &first))
            .unwrap()
            .bytes,
        b"fir"
    );
    assert_eq!(
        hub.read_payload(&payload_request(&observation, &second))
            .unwrap()
            .bytes,
        b"sec"
    );
}

#[test]
fn near_exhausted_payload_progress_rejects_before_publication_and_preserves_reads() {
    let hub = XrayHub::default();
    let mut observation = hub.observe(request("s", true)).unwrap();
    hub.state().unwrap().payload_consumed = u64::MAX - 1;
    hub.inner
        .payload_issued
        .read()
        .unwrap()
        .store(u64::MAX - 1, Ordering::Release);
    let reference = hub
        .capture_payload(&correlation("s"), "text/plain", b"last")
        .unwrap()
        .unwrap();
    assert_eq!(
        hub.capture_payload(&correlation("s"), "text/plain", b"exhausted"),
        Err(XrayError::PayloadProgressExhausted)
    );
    assert_eq!(
        hub.read_payload(&payload_request(&observation, &reference))
            .unwrap()
            .bytes,
        b"las"
    );
    assert_eq!(hub.state().unwrap().payload_consumed, u64::MAX);
    assert_eq!(hub.inner.payload_budget.used_bytes(), 0);
    let receipt = observation.finalize().unwrap();
    assert_eq!(receipt.hub_contention_drops, 1);
    assert_eq!(receipt.final_sequence, 2);
    // Retiring the last observation creates a fresh, independent generation.
    drop(observation);
    let replacement = hub.observe(request("s", true)).unwrap();
    let reference = hub
        .capture_payload(&correlation("s"), "text/plain", b"new")
        .unwrap()
        .unwrap();
    assert_eq!(
        hub.read_payload(&payload_request(&replacement, &reference))
            .unwrap()
            .bytes,
        b"new"
    );
}

#[test]
fn overflowed_payload_boundary_is_an_explicit_error_not_a_successful_reference() {
    let hub = XrayHub::default();
    let observation = hub.observe(request("s", true)).unwrap();
    let reference = hub
        .capture_payload(&correlation("s"), "text/plain", b"queued")
        .unwrap()
        .unwrap();
    // Deliberately inconsistent seeded state must fail closed, not wrap or freeze.
    hub.state().unwrap().payload_consumed = u64::MAX;
    assert_eq!(
        hub.read_payload(&payload_request(&observation, &reference)),
        Err(XrayError::PayloadProgressExhausted)
    );
}

#[test]
fn finite_read_boundary_does_not_chase_later_unrelated_payloads() {
    let hub = XrayHub::default();
    let observation = hub.observe(request("s", true)).unwrap();
    let _other = hub.observe(request("other", true)).unwrap();
    let reference = hub
        .capture_payload(&correlation("s"), "text/plain", b"target")
        .unwrap()
        .unwrap();
    let boundary = XrayHub::payload_boundary(&hub.state().unwrap()).unwrap();
    // Every later publication is outside the fixed read prefix, even if the
    // unrelated producer continues filling all available queue capacity.
    for _ in 0..32 {
        hub.capture_payload(&correlation("other"), "text/plain", b"later")
            .unwrap()
            .unwrap();
    }
    let mut prior = Some(boundary);
    assert_eq!(
        hub.read_payload_at(&payload_request(&observation, &reference), &mut prior)
            .unwrap()
            .bytes,
        b"tar"
    );
    let state = hub.state().unwrap();
    assert_eq!(state.payload_consumed, boundary);
    assert_eq!(state.payload_ingress.len(), 32);
    drop(state);
    let mut foreign = payload_request(&observation, &reference);
    foreign.session_id = "foreign".into();
    assert_eq!(hub.read_payload(&foreign), Err(XrayError::CaptureInactive));
    assert_eq!(hub.state().unwrap().payload_consumed, boundary);
    let mut unknown = payload_request(&observation, &reference);
    unknown.payload_id = "unknown".into();
    assert_eq!(
        hub.read_payload(&unknown),
        Err(XrayError::PayloadUnavailable)
    );
}

#[test]
fn payload_capture_and_immediate_read_do_not_require_producer_consumer_lock() {
    let hub = XrayHub::default();
    let observation = hub.observe(request("s", true)).unwrap();
    let state = hub.state().unwrap();
    let first = hub
        .capture_payload(&correlation("s"), "text/plain", b"first")
        .unwrap()
        .unwrap();
    let second = hub
        .capture_payload(&correlation("s"), "text/plain", b"second")
        .unwrap()
        .unwrap();
    drop(state);
    assert_eq!(
        hub.read_payload(&payload_request(&observation, &first))
            .unwrap()
            .bytes,
        b"fir"
    );
    assert_eq!(
        hub.read_payload(&payload_request(&observation, &second))
            .unwrap()
            .bytes,
        b"sec"
    );
    assert_eq!(hub.inner.payload_budget.used_bytes(), 0);
}

#[test]
fn payload_preparation_reserves_count_including_zero_bytes_and_unwind_releases() {
    let hub = XrayHub::new(XrayConfig {
        max_pending_payloads: 2,
        ..XrayConfig::default()
    });
    let _observation = hub.observe(request("s", true)).unwrap();
    let first = hub
        .prepare_payload(&correlation("s"), "text/plain", b"")
        .unwrap()
        .unwrap();
    let second = hub
        .prepare_payload(&correlation("s"), "text/plain", b"")
        .unwrap()
        .unwrap();
    assert!(matches!(
        hub.prepare_payload(&correlation("s"), "text/plain", b""),
        Err(XrayError::Busy)
    ));
    drop(first);
    drop(second);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _prepared = hub
            .prepare_payload(&correlation("s"), "text/plain", b"charged")
            .unwrap()
            .unwrap();
        assert_eq!(hub.inner.payload_budget.used_bytes(), 7);
        panic!("fixture preparation unwind");
    }));
    assert!(result.is_err());
    assert_eq!(hub.inner.payload_budget.used_bytes(), 0);
    assert!(
        hub.prepare_payload(&correlation("s"), "text/plain", b"ok")
            .unwrap()
            .is_some()
    );
}

#[test]
fn payload_byte_budget_is_independent_of_count_and_retained_cache() {
    let hub = XrayHub::new(XrayConfig {
        max_payload_bytes: 4,
        max_pending_payload_bytes: 4,
        ..XrayConfig::default()
    });
    let observation = hub.observe(request("s", true)).unwrap();
    let prepared = hub
        .prepare_payload(&correlation("s"), "text/plain", b"four")
        .unwrap()
        .unwrap();
    assert!(matches!(
        hub.prepare_payload(&correlation("s"), "text/plain", b"x"),
        Err(XrayError::Busy)
    ));
    let reference = hub
        .commit_payload(prepared, &correlation("s"))
        .unwrap()
        .unwrap();
    assert_eq!(hub.inner.payload_budget.used_bytes(), 4);
    assert_eq!(
        hub.read_payload(&payload_request(&observation, &reference))
            .unwrap()
            .bytes,
        b"fou"
    );
    assert_eq!(hub.inner.payload_budget.used_bytes(), 0);
}

#[test]
fn preparation_never_retargets_new_observation_after_cutoff_or_detach() {
    let hub = XrayHub::default();
    let mut old = hub.observe(request("s", true)).unwrap();
    let prepared = hub
        .prepare_payload(&correlation("s"), "text/plain", b"old")
        .unwrap()
        .unwrap();
    let mut new = hub.observe(request("s", true)).unwrap();
    assert_eq!(old.finalize(), Err(XrayError::FinalizePending));
    assert_eq!(
        hub.commit_payload(prepared, &correlation("s")).unwrap(),
        None
    );
    old.finalize().unwrap();
    new.finalize().unwrap();
    assert!(hub.state().unwrap().observers[new.id()].payloads.is_empty());
    drop(old);
    drop(new);
    let observation = hub.observe(request("s", true)).unwrap();
    hub.capture_payload(&correlation("s"), "text/plain", b"queued")
        .unwrap()
        .unwrap();
    assert_eq!(hub.inner.payload_budget.used_bytes(), 6);
    drop(observation);
    assert_eq!(hub.inner.payload_budget.used_bytes(), 0);
}

#[tokio::test]
async fn finite_payload_read_and_cutoff_transfer_preexpiry_capture() {
    let hub = XrayHub::default();
    let mut observation = hub.observe(request("s", true)).unwrap();
    let reference = hub
        .capture_payload(&correlation("s"), "text/plain", b"fixture")
        .unwrap()
        .unwrap();
    {
        let mut state = hub.state().unwrap();
        state.observers.get_mut(observation.id()).unwrap().deadline = Some(Instant::now());
        hub.refresh_activity(&state);
        drop(state);
    }
    observation.finalize_complete().await.unwrap();
    assert_eq!(
        hub.read_payload_complete(&payload_request(&observation, &reference))
            .await
            .unwrap()
            .bytes,
        b"fix"
    );
    assert_eq!(hub.inner.payload_budget.used_bytes(), 0);
}
