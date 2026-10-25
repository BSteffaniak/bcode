//! Deferred constructor and delivery-fence behavior; never opens real adapters.
use super::*;

fn wait_for_exit(handle: &Handle) {
    let shared = &handle.controller.shared;
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut state = shared.lock();
    while state.alive {
        assert!(Instant::now() < deadline, "worker did not exit");
        state = shared
            .changed
            .wait_timeout(state, deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0;
    }
    drop(state);
}

#[test]
fn slow_initialization_is_responsive_and_late_failure_after_cancel_is_discarded() {
    let (entered_tx, entered) = mpsc::sync_channel(1);
    let (release, release_rx) = mpsc::sync_channel(1);
    let handle = spawn_worker(
        move || {
            entered_tx.send(()).expect("signal initialization");
            release_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("release constructor");
            Err(failure_error("SECRET constructor diagnostic"))
        },
        true,
        None,
    )
    .expect("start deferred constructor");
    entered
        .recv_timeout(Duration::from_secs(2))
        .expect("constructor entered");
    assert_eq!(handle.status(), Status::Idle);
    assert!(matches!(handle.action(Action::Start), Err(Error::Busy)));
    assert!(matches!(handle.action(Action::Toggle), Err(Error::Busy)));
    assert_eq!(
        handle
            .action(Action::Cancel)
            .expect("cancel initialization"),
        Status::Idle
    );
    assert!(handle.action(Action::Start).is_err());
    release.send(()).expect("release");
    wait_for_exit(&handle);
    assert_eq!(handle.status(), Status::Idle);
    assert!(handle.take_final_text().is_none());
}

#[test]
fn deferred_success_accepts_work_only_after_construction_settles() {
    let (release, release_rx) = mpsc::sync_channel(1);
    let counts = Arc::new(Counts::default());
    let (recognition_tx, _) = mpsc::sync_channel(1);
    let (_, recognition_rx) = mpsc::sync_channel(1);
    let handle = spawn_worker(
        move || {
            release_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("release constructor");
            Ok(Runtime::new(
                Config {
                    delivery: DeliveryMode::Host,
                    ..Config::default()
                },
                Box::new(FakeAudio(Arc::clone(&counts), false)),
                Box::new(FakeRecognizer {
                    counts: Arc::clone(&counts),
                    entered: recognition_tx,
                    release: recognition_rx,
                    block: false,
                }),
                Box::new(FakeGate(true)),
                Box::new(FakeProcessor),
                Box::new(FakeSink(counts, false)),
            ))
        },
        true,
        None,
    )
    .expect("spawn constructor");
    assert!(matches!(handle.action(Action::Start), Err(Error::Busy)));
    release.send(()).expect("release");
    let shared = &handle.controller.shared;
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut state = shared.lock();
    while state.initializing {
        assert!(Instant::now() < deadline, "constructor did not settle");
        state = shared
            .changed
            .wait_timeout(state, deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0;
    }
    drop(state);
    start_and_stop(&handle);
    settled(&handle);
    assert_eq!(
        handle.take_final_text().expect("host text").text,
        "private final words!"
    );
}

#[test]
fn initialization_failure_is_redacted() {
    let handle = spawn_worker(
        || Err(failure_error("SECRET constructor diagnostic")),
        true,
        None,
    )
    .expect("start failing constructor");
    wait_for_exit(&handle);
    let Status::Failed { message } = handle.status() else {
        panic!("expected failure")
    };
    assert!(!message.contains("SECRET"));
}

struct BlockingSink {
    entered: SyncSender<()>,
    release: Receiver<()>,
    delivered: Arc<AtomicUsize>,
}

impl TextSink for BlockingSink {
    fn deliver(&mut self, _: &str, cancel: &Cancellation) -> Result<()> {
        self.entered.send(()).expect("signal delivery entry");
        self.release
            .recv_timeout(Duration::from_secs(3))
            .expect("release delivery");
        cancel.check()?;
        self.delivered.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[test]
fn cancellation_at_delivery_boundary_is_propagated_without_retry() {
    let counts = Arc::new(Counts::default());
    let (entered_tx, entered) = mpsc::sync_channel(1);
    let (release, release_rx) = mpsc::sync_channel(1);
    let (recognition_tx, _) = mpsc::sync_channel(1);
    let (_, recognition_rx) = mpsc::sync_channel(1);
    let delivered = Arc::new(AtomicUsize::new(0));
    let handle = Runtime::new(
        Config::default(),
        Box::new(FakeAudio(Arc::clone(&counts), false)),
        Box::new(FakeRecognizer {
            counts,
            entered: recognition_tx,
            release: recognition_rx,
            block: false,
        }),
        Box::new(FakeGate(true)),
        Box::new(FakeProcessor),
        Box::new(BlockingSink {
            entered: entered_tx,
            release: release_rx,
            delivered: Arc::clone(&delivered),
        }),
    )
    .into_handle()
    .expect("start injected runtime");
    start_and_stop(&handle);
    entered
        .recv_timeout(Duration::from_secs(2))
        .expect("delivery entered");
    assert!(matches!(handle.status(), Status::Delivering { .. }));
    handle.action(Action::Cancel).expect("cancel delivery");
    assert!(matches!(handle.action(Action::Start), Err(Error::Busy)));
    release.send(()).expect("release");
    settled(&handle);
    assert_eq!(delivered.load(Ordering::SeqCst), 0);
    assert_eq!(handle.status(), Status::Idle);
}
