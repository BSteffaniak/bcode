//! Capture and controller lifecycle tests using the parent module's fake adapters.
use super::*;

#[test]
fn cancelled_capture_is_dropped_on_its_worker_without_finalizing() {
    let f = fixture(
        DeliveryMode::Host,
        true,
        false,
        SinkBehavior::Success,
        false,
    );
    f.handle.action(Action::Start).expect("start");
    // Wait for the fake source to create its !Send capture, not for the deadline.
    let deadline = Instant::now() + Duration::from_secs(2);
    while f.counts.starts.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline, "capture did not start");
        thread::yield_now();
    }
    f.handle.action(Action::Cancel).expect("cancel capture");
    settled(&f.handle);
    assert_eq!(f.handle.status(), Status::Idle);
    assert_eq!(f.counts.finishes.load(Ordering::SeqCst), 0);
    assert_eq!(f.counts.drops.load(Ordering::SeqCst), 1);
    assert_eq!(f.counts.recognition.load(Ordering::SeqCst), 0);
}

#[test]
fn dropping_last_controller_cancels_inflight_work() {
    let f = fixture(
        DeliveryMode::Typing,
        true,
        true,
        SinkBehavior::Success,
        false,
    );
    start_and_stop(&f.handle);
    f.entered
        .recv_timeout(Duration::from_secs(2))
        .expect("inference entered");
    let shared = Arc::clone(&f.handle.controller.shared);
    drop(f.handle);
    assert!(shared.lock().shutdown);
    f.release.send(()).expect("release inference");
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut state = shared.lock();
    while state.alive {
        assert!(Instant::now() < deadline, "worker did not exit");
        state = shared
            .changed
            .wait_timeout(state, deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0;
    }
    assert!(state.final_text.is_none());
    drop(state);
    assert_eq!(f.counts.delivery.load(Ordering::SeqCst), 0);
}
