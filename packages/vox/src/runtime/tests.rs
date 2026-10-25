//! Behavioral tests use injected adapters only: no microphone, desktop, or network.
#[path = "initialization_tests.rs"]
mod initialization;
#[path = "lifecycle_tests.rs"]
mod lifecycle;
use super::*;
use crate::contracts::{Capture, RecognitionOptions, Transcript};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::ThreadId;

#[derive(Default)]
struct Counts {
    starts: AtomicUsize,
    finishes: AtomicUsize,
    drops: AtomicUsize,
    recognition: AtomicUsize,
    delivery: AtomicUsize,
    duration: AtomicUsize,
}

struct FakeAudio(Arc<Counts>, bool);
struct FakeCapture {
    counts: Arc<Counts>,
    creator: ThreadId,
    _not_send: Rc<()>,
    invalid: bool,
}

impl AudioSource for FakeAudio {
    fn start(&mut self, seconds: u32) -> Result<Box<dyn Capture>> {
        self.0.starts.fetch_add(1, Ordering::SeqCst);
        self.0.duration.store(
            usize::try_from(seconds).expect("fake duration fits usize"),
            Ordering::SeqCst,
        );
        Ok(Box::new(FakeCapture {
            counts: Arc::clone(&self.0),
            creator: thread::current().id(),
            _not_send: Rc::new(()),
            invalid: self.1,
        }))
    }
}

impl Capture for FakeCapture {
    fn finish(self: Box<Self>) -> Result<Recording> {
        assert_eq!(self.creator, thread::current().id());
        self.counts.finishes.fetch_add(1, Ordering::SeqCst);
        Ok(Recording {
            samples: vec![if self.invalid { f32::NAN } else { 0.5 }; 32],
            sample_rate: 16_000,
        })
    }
}

impl Drop for FakeCapture {
    fn drop(&mut self) {
        assert_eq!(self.creator, thread::current().id());
        self.counts.drops.fetch_add(1, Ordering::SeqCst);
    }
}

struct FakeRecognizer {
    counts: Arc<Counts>,
    entered: SyncSender<()>,
    release: Receiver<()>,
    block: bool,
}

impl Recognizer for FakeRecognizer {
    fn recognize(
        &mut self,
        _: &Recording,
        _: &RecognitionOptions,
        _: &Cancellation,
    ) -> Result<Transcript> {
        self.counts.recognition.fetch_add(1, Ordering::SeqCst);
        // Deliberately ignores cancellation to exercise stale-result fencing.
        let _ = self.entered.try_send(());
        if self.block {
            self.release
                .recv_timeout(Duration::from_secs(5))
                .map_err(|_| failure_error("fake release timed out"))?;
        }
        Ok(Transcript {
            text: "private final words".into(),
        })
    }
}

struct FakeGate(bool);
impl SpeechGate for FakeGate {
    fn has_speech(&mut self, _: &Recording) -> Result<bool> {
        Ok(self.0)
    }
}

struct FakeProcessor;
impl TextProcessor for FakeProcessor {
    fn process(&self, text: &str) -> Result<String> {
        Ok(format!("{text}!"))
    }
}

struct FakeSink(Arc<Counts>, bool);
impl TextSink for FakeSink {
    fn deliver(&mut self, _: &str, _: &Cancellation) -> Result<()> {
        self.0.delivery.fetch_add(1, Ordering::SeqCst);
        if self.1 {
            Err(failure_error("SECRET TRANSCRIPT AND CREDENTIAL"))
        } else {
            Ok(())
        }
    }
}

struct Fixture {
    handle: Handle,
    counts: Arc<Counts>,
    entered: Receiver<()>,
    release: SyncSender<()>,
}

#[derive(Clone, Copy, Default)]
enum SinkBehavior {
    #[default]
    Success,
    Failure,
}

fn fixture(
    mode: DeliveryMode,
    speech: bool,
    block: bool,
    sink: SinkBehavior,
    invalid_audio: bool,
) -> Fixture {
    fixture_with_limit(mode, speech, block, sink, invalid_audio, 120)
}

fn fixture_with_limit(
    mode: DeliveryMode,
    speech: bool,
    block: bool,
    sink: SinkBehavior,
    invalid_audio: bool,
    seconds: u32,
) -> Fixture {
    let counts = Arc::new(Counts::default());
    let (entered_tx, entered) = mpsc::sync_channel(1);
    let (release, release_rx) = mpsc::sync_channel(1);
    let config = Config {
        delivery: mode,
        max_recording_seconds: seconds,
        ..Config::default()
    };
    let runtime = Runtime::new(
        config,
        Box::new(FakeAudio(Arc::clone(&counts), invalid_audio)),
        Box::new(FakeRecognizer {
            counts: Arc::clone(&counts),
            entered: entered_tx,
            release: release_rx,
            block,
        }),
        Box::new(FakeGate(speech)),
        Box::new(FakeProcessor),
        Box::new(FakeSink(
            Arc::clone(&counts),
            matches!(sink, SinkBehavior::Failure),
        )),
    );
    Fixture {
        handle: runtime.into_handle().expect("spawn fake worker"),
        counts,
        entered,
        release,
    }
}

fn start_and_stop(handle: &Handle) {
    assert!(matches!(
        handle.action(Action::Start),
        Ok(Status::Recording { .. })
    ));
    handle.action(Action::Stop).expect("stop accepted");
}

fn settled(handle: &Handle) {
    let shared = &handle.controller.shared;
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut state = shared.lock();
    while state.active.is_some() {
        assert!(Instant::now() < deadline, "fake operation did not settle");
        state = shared
            .changed
            .wait_timeout(state, deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0;
    }
    drop(state);
}

#[test]
fn handle_is_send_and_sync() {
    fn assert_traits<T: Send + Sync + Clone>() {}
    assert_traits::<Handle>();
}

#[test]
fn host_result_is_final_processed_take_once_and_never_delivered() {
    let f = fixture(
        DeliveryMode::Host,
        true,
        false,
        SinkBehavior::Success,
        false,
    );
    start_and_stop(&f.handle);
    settled(&f.handle);
    assert_eq!(f.handle.status(), Status::Idle);
    let final_text = f.handle.take_final_text().expect("host result");
    assert_eq!(final_text.operation, 1);
    assert_eq!(final_text.text, "private final words!");
    assert!(f.handle.take_final_text().is_none());
    assert_eq!(f.counts.delivery.load(Ordering::SeqCst), 0);
    assert_eq!(f.counts.finishes.load(Ordering::SeqCst), 1);
    assert_eq!(f.counts.drops.load(Ordering::SeqCst), 1);
}

#[test]
fn cancellation_is_responsive_during_noncooperative_inference_and_fences_late_text() {
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
    assert!(matches!(
        f.handle.status(),
        Status::Transcribing { operation: 1 }
    ));
    assert!(matches!(f.handle.action(Action::Start), Err(Error::Busy)));
    assert_eq!(
        f.handle.action(Action::Cancel).expect("cancel"),
        Status::Idle
    );
    assert_eq!(f.handle.status(), Status::Idle);
    assert!(matches!(f.handle.action(Action::Start), Err(Error::Busy)));
    f.release.send(()).expect("release inference");
    settled(&f.handle);
    assert_eq!(f.handle.status(), Status::Idle);
    assert!(f.handle.take_final_text().is_none());
    assert_eq!(f.counts.delivery.load(Ordering::SeqCst), 0);
    assert!(matches!(
        f.handle.action(Action::Start),
        Ok(Status::Recording { operation: 2 })
    ));
    f.handle
        .action(Action::Cancel)
        .expect("cancel second generation");
    settled(&f.handle);
}

#[test]
fn silence_and_invalid_audio_never_reach_inference_or_delivery() {
    for (speech, invalid) in [(false, false), (true, true)] {
        let f = fixture(
            DeliveryMode::Typing,
            speech,
            false,
            SinkBehavior::Success,
            invalid,
        );
        start_and_stop(&f.handle);
        settled(&f.handle);
        assert_eq!(f.counts.recognition.load(Ordering::SeqCst), 0);
        assert_eq!(f.counts.delivery.load(Ordering::SeqCst), 0);
        assert_eq!(matches!(f.handle.status(), Status::Failed { .. }), invalid);
    }
}

#[test]
fn failed_delivery_is_attempted_once_and_errors_are_redacted() {
    let f = fixture(
        DeliveryMode::Typing,
        true,
        false,
        SinkBehavior::Failure,
        false,
    );
    start_and_stop(&f.handle);
    settled(&f.handle);
    let Status::Failed { message } = f.handle.status() else {
        panic!("expected failed status")
    };
    assert!(!message.contains("SECRET"));
    assert!(!message.contains("private"));
    assert_eq!(f.counts.delivery.load(Ordering::SeqCst), 1);
    f.handle.action(Action::Stop).expect("idempotent stop");
    assert_eq!(f.counts.delivery.load(Ordering::SeqCst), 1);
    assert!(f.handle.take_final_text().is_none());
}

#[test]
fn deadline_stops_capture_without_a_stop_action() {
    let f = fixture_with_limit(
        DeliveryMode::Host,
        true,
        false,
        SinkBehavior::Success,
        false,
        1,
    );
    f.handle.action(Action::Start).expect("start");
    settled(&f.handle);
    assert_eq!(f.counts.duration.load(Ordering::SeqCst), 1);
    assert_eq!(f.counts.finishes.load(Ordering::SeqCst), 1);
    assert!(f.handle.take_final_text().is_some());
}

#[test]
fn shutdown_during_inference_is_nonblocking_and_late_result_is_discarded() {
    let f = fixture(DeliveryMode::Host, true, true, SinkBehavior::Success, false);
    start_and_stop(&f.handle);
    f.entered
        .recv_timeout(Duration::from_secs(2))
        .expect("inference entered");
    let clone = f.handle.clone();
    clone.shutdown();
    assert_eq!(f.handle.status(), Status::Idle);
    assert!(f.handle.action(Action::Start).is_err());
    f.release.send(()).expect("release inference");
    settled(&f.handle);
    assert!(f.handle.take_final_text().is_none());
}

#[test]
fn new_operation_discards_unconsumed_host_output_and_toggle_stops() {
    let f = fixture(
        DeliveryMode::Host,
        true,
        false,
        SinkBehavior::Success,
        false,
    );
    start_and_stop(&f.handle);
    settled(&f.handle);
    assert!(matches!(
        f.handle.action(Action::Toggle),
        Ok(Status::Recording { operation: 2 })
    ));
    assert!(f.handle.take_final_text().is_none());
    f.handle.action(Action::Toggle).expect("toggle stop");
    settled(&f.handle);
    assert_eq!(
        f.handle.take_final_text().expect("second output").operation,
        2
    );
}
