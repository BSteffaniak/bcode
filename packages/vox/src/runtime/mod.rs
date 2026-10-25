//! Bounded single-operation dictation runtime.
//!
//! Service integration: `spawn(config)` returns a cloneable, `Send + Sync` `Handle`.
//! `Handle::action` accepts control without waiting for capture or inference;
//! `Handle::status` reads a redacted snapshot. `Handle::take_final_text` consumes
//! the one retained host result. There is deliberately no transcript history.
//!
//! The worker alone creates, finishes, and drops capture handles (which need not
//! be `Send`). Control is shared state, not an unbounded command queue. Cancel
//! publishes `Idle` immediately, but a cancelled operation keeps its exclusive
//! reservation until its backend returns. A backend ignoring cancellation cannot
//! cause parallel inference or an accumulation of abandoned worker threads.
//! Cancellation cannot undo bytes already delivered by an external sink. The
//! delivery transition is the commit boundary: cancellation observed before it
//! prevents sink entry; cancellation after it is cooperative via the sink token.
//! No state mutex is held across sink calls, so shutdown cannot block on delivery.

use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::contracts::{
    Action, AudioSource, Cancellation, DeliveryMode, Error, Recognizer, Recording, Result,
    SpeechGate, Status, TextProcessor, TextSink,
};

const MAX_TEXT_BYTES: usize = 1024 * 1024;
const MAX_SAMPLE_RATE: u32 = 192_000;

// Host results are retained by the runtime, not sent through an external sink.
// Fail closed if a future refactor accidentally routes host text here.
struct HostOnlySink;

impl TextSink for HostOnlySink {
    fn deliver(&mut self, _: &str, _: &Cancellation) -> Result<()> {
        Err(Error::Unsupported(
            "host output must be consumed through take_final_text".into(),
        ))
    }
}

/// Assemble only explicitly configured adapters; never fall back to cloud or clipboard.
///
/// # Errors
/// Returns invalid configuration or an unavailable configured adapter.
pub fn build(config: &Config) -> Result<Runtime> {
    config.validate()?;
    let mut runtime = Runtime::new(
        config.clone(),
        crate::audio::source(config)?,
        crate::recognition::recognizer(config)?,
        crate::speech::gate(config)?,
        crate::text::processor(config)?,
        if config.delivery == DeliveryMode::Host {
            Box::new(HostOnlySink)
        } else {
            crate::delivery::sink(config)?
        },
    );
    runtime.real_pipeline = true;
    Ok(runtime)
}

/// Reserve cooperative per-user capture ownership and initialize on the worker.
///
/// No microphone is opened until a start action is accepted.
/// During initialization status remains `Idle`, start/toggle returns `Busy`,
/// and cancel/shutdown are responsive. Initialization failure becomes `Failed`.
/// Cancelling initialization retires the handle (create another to retry) and
/// discards late constructor output; its singleton stays held until it returns.
///
/// # Errors
/// Returns invalid configuration, singleton contention (`Busy`), an unavailable
/// secure runtime directory, or failure to create the dedicated thread.
pub fn spawn(config: Config) -> Result<Handle> {
    config.validate()?;
    // Acquire before loading models or creating any real adapters. The worker
    // retains ownership through initialization and non-cooperative shutdown.
    let reservation = singleton::reserve()?;
    spawn_worker(move || build(&config), true, Some(reservation))
}

mod singleton;

fn spawn_worker<F>(
    factory: F,
    initializing: bool,
    reservation: Option<singleton::Reservation>,
) -> Result<Handle>
where
    F: FnOnce() -> Result<Runtime> + Send + 'static,
{
    let shared = Arc::new(Shared::default());
    shared.lock().initializing = initializing;
    let worker_state = Arc::clone(&shared);
    let _worker = thread::Builder::new().name("vox-runtime".into()).spawn(move || {
        let _reservation = reservation;
        // Do not change process-wide panic hooks. Adapters must never panic with
        // sensitive payloads; caught payloads are not copied to public status.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if let Ok(runtime) = factory() {
                let mut state = worker_state.lock();
                state.initializing = false;
                let shutdown = state.shutdown;
                drop(state);
                worker_state.changed.notify_all();
                if shutdown { return; }
                runtime.run(&worker_state);
            } else {
                let mut state = worker_state.lock();
                if !state.shutdown {
                    state.status = failure("runtime initialization failed; check configured capabilities and model");
                }
            }
        }));
        let mut state = worker_state.lock();
        state.initializing = false;
        state.alive = false;
        if let Some(active) = state.active.take() { active.cancel.cancel(); }
        if outcome.is_err() && !state.shutdown {
            state.status = failure("dictation worker stopped unexpectedly");
        }
        drop(state);
        worker_state.changed.notify_all();
    }).map_err(Error::Io)?;
    Ok(Handle {
        controller: Arc::new(Controller { shared }),
    })
}

/// Owned adapters, moved together onto one dedicated worker by `into_handle`.
pub struct Runtime {
    config: Config,
    audio: Box<dyn AudioSource>,
    recognizer: Box<dyn Recognizer>,
    gate: Box<dyn SpeechGate>,
    processor: Box<dyn TextProcessor>,
    sink: Box<dyn TextSink>,
    real_pipeline: bool,
}

impl Runtime {
    /// Assemble injected adapters without opening capture or executing inference.
    ///
    /// Validation occurs in `into_handle`, including for injected configurations.
    /// Injected adapters are intentionally not subject to the machine singleton;
    /// real plugin/standalone callers should use `spawn` or `build`.
    #[must_use]
    pub fn new(
        config: Config,
        audio: Box<dyn AudioSource>,
        recognizer: Box<dyn Recognizer>,
        gate: Box<dyn SpeechGate>,
        processor: Box<dyn TextProcessor>,
        sink: Box<dyn TextSink>,
    ) -> Self {
        Self {
            config,
            audio,
            recognizer,
            gate,
            processor,
            sink,
            real_pipeline: false,
        }
    }

    /// Start the dedicated worker, initially idle. Dropping the last handle
    /// requests cancellation and shutdown without waiting on a blocking backend.
    ///
    /// # Errors
    /// Returns invalid configuration, real-pipeline singleton contention, or
    /// thread creation failure. Unlike `spawn`, eager `build` plus `into_handle`
    /// performs adapter initialization on the caller; frontends should use `spawn`.
    pub fn into_handle(self) -> Result<Handle> {
        self.config.validate()?;
        let reservation = if self.real_pipeline {
            Some(singleton::reserve()?)
        } else {
            None
        };
        spawn_worker(move || Ok(self), false, reservation)
    }

    fn run(mut self, shared: &Shared) {
        loop {
            let Some((operation, cancel, started)) = shared.next() else {
                return;
            };
            let outcome = self.record_and_deliver(shared, operation, &cancel, started);
            shared.complete(operation, outcome);
        }
    }

    fn record_and_deliver(
        &mut self,
        shared: &Shared,
        operation: u64,
        cancel: &Cancellation,
        started: Instant,
    ) -> Result<Option<String>> {
        cancel.check()?;
        let capture = self
            .audio
            .start(self.config.max_recording_seconds)
            .map_err(|_| failure_error("audio capture could not start"))?;
        // The timeout includes audio startup, not just the wait after it.
        let deadline = started + Duration::from_secs(u64::from(self.config.max_recording_seconds));
        {
            let mut state = shared.lock();
            loop {
                cancel.check()?;
                let active = state.active.as_ref().ok_or(Error::Cancelled)?;
                if active.operation != operation || state.shutdown {
                    return Err(Error::Cancelled);
                }
                if active.stop || Instant::now() >= deadline {
                    break;
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                state = shared
                    .changed
                    .wait_timeout(state, remaining)
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0;
            }
        }
        // Cancellation above drops capture on this same worker without finishing it.
        let recording = capture
            .finish()
            .map_err(|_| failure_error("audio capture could not finish"))?;
        cancel.check()?;
        shared.transition(operation, Status::Transcribing { operation })?;
        validate_recording(&recording, self.config.max_recording_seconds)?;
        if recording.samples.is_empty() {
            return Ok(None);
        }
        let speech = self
            .gate
            .has_speech(&recording)
            .map_err(|_| failure_error("speech detection failed"))?;
        cancel.check()?;
        if !speech {
            return Ok(None);
        }
        let transcript = self
            .recognizer
            .recognize(&recording, &self.config.recognition, cancel)
            .map_err(|_| failure_error("transcription failed"))?;
        cancel.check()?;
        validate_text(&transcript.text)?;
        let text = self
            .processor
            .process(&transcript.text)
            .map_err(|_| failure_error("text processing failed"))?;
        cancel.check()?;
        validate_text(&text)?;
        if text.trim().is_empty() {
            return Ok(None);
        }
        shared.transition(operation, Status::Delivering { operation })?;
        cancel.check()?;
        if self.config.delivery == DeliveryMode::Host {
            return Ok(Some(text));
        }
        // Exactly one attempt; error may mean partial delivery. Never retry.
        self.sink.deliver(&text, cancel).map_err(|_| {
            failure_error("text delivery failed; output may be partial; not retried")
        })?;
        cancel.check()?;
        Ok(None)
    }
}

fn validate_recording(recording: &Recording, seconds: u32) -> Result<()> {
    let limit = u64::from(recording.sample_rate) * u64::from(seconds);
    if recording.sample_rate == 0
        || recording.sample_rate > MAX_SAMPLE_RATE
        || u64::try_from(recording.samples.len()).unwrap_or(u64::MAX) > limit
        || recording
            .samples
            .iter()
            .any(|sample| !sample.is_finite() || sample.abs() > 1.0)
    {
        return Err(failure_error("capture returned invalid or oversized audio"));
    }
    Ok(())
}

fn validate_text(text: &str) -> Result<()> {
    if text.len() > MAX_TEXT_BYTES {
        return Err(failure_error("final text exceeds runtime size limit"));
    }
    Ok(())
}

fn failure(message: &str) -> Status {
    Status::Failed {
        message: message.into(),
    }
}

fn failure_error(message: &str) -> Error {
    Error::Backend(message.into())
}

/// A final host-only result. Intentionally does not implement `Debug` or `Display`.
pub struct FinalText {
    /// Operation that produced this final result.
    pub operation: u64,
    /// Final processed text; the host must not log it implicitly.
    pub text: String,
}

/// Cloneable, thread-safe controller; methods never call backend code.
#[derive(Clone)]
pub struct Handle {
    controller: Arc<Controller>,
}

impl Handle {
    /// Accept an action and return its immediate state, not eventual completion.
    ///
    /// Stop/toggle request finalization asynchronously. Cancellation immediately
    /// masks all later results, including failures, from the cancelled generation.
    /// A new start discards any unconsumed host result.
    ///
    /// # Errors
    /// Returns `Busy` until existing work (even cancelled work) releases its
    /// reservation, or a safe error when the worker is unavailable or IDs exhaust.
    pub fn action(&self, action: Action) -> Result<Status> {
        let shared = &self.controller.shared;
        let mut state = shared.lock();
        if !state.alive || state.shutdown {
            return Err(failure_error("dictation worker is unavailable"));
        }
        let action = if action == Action::Toggle {
            if matches!(state.status, Status::Recording { .. }) {
                Action::Stop
            } else {
                Action::Start
            }
        } else {
            action
        };
        match action {
            Action::Start => {
                if state.initializing || state.active.is_some() {
                    return Err(Error::Busy);
                }
                let operation = state
                    .generation
                    .checked_add(1)
                    .ok_or_else(|| failure_error("operation identifier exhausted"))?;
                state.generation = operation;
                state.final_text = None;
                state.active = Some(Active {
                    operation,
                    cancel: Cancellation::new(),
                    stop: false,
                    claimed: false,
                    started: Instant::now(),
                });
                state.status = Status::Recording { operation };
            }
            Action::Stop => {
                if let Some(active) = state.active.as_mut() {
                    // Repeated stop is idempotent, including during inference.
                    active.stop = true;
                }
            }
            Action::Cancel => {
                // Construction cannot be interrupted through the frozen adapter
                // API. Cancel initialization by retiring this handle: a late
                // success/failure must not reopen or overwrite its Idle state.
                if state.initializing {
                    state.shutdown = true;
                }
                if let Some(active) = &state.active {
                    active.cancel.cancel();
                }
                state.final_text = None;
                state.status = Status::Idle;
            }
            Action::Toggle => unreachable!("toggle is normalized above"),
        }
        let status = state.status.clone();
        drop(state);
        shared.changed.notify_all();
        Ok(status)
    }

    /// Return an immediate redacted state snapshot without waiting on inference.
    #[must_use]
    pub fn status(&self) -> Status {
        self.controller.shared.lock().status.clone()
    }

    /// Consume the sole final host result. Other delivery modes never populate it.
    #[expect(
        clippy::must_use_candidate,
        reason = "Option is already must-use; this method consumes retained output rather than observing it"
    )]
    pub fn take_final_text(&self) -> Option<FinalText> {
        self.controller.shared.lock().final_text.take()
    }

    /// Request shutdown and cancel pending work for every clone. This never joins
    /// the worker; a non-cooperative backend may outlive this call.
    pub fn shutdown(&self) {
        self.controller.shared.shutdown();
    }
}

struct Controller {
    shared: Arc<Shared>,
}

impl Drop for Controller {
    fn drop(&mut self) {
        self.shared.shutdown();
    }
}

struct Active {
    operation: u64,
    cancel: Cancellation,
    stop: bool,
    claimed: bool,
    started: Instant,
}

struct State {
    status: Status,
    generation: u64,
    active: Option<Active>,
    final_text: Option<FinalText>,
    alive: bool,
    initializing: bool,
    shutdown: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            status: Status::Idle,
            generation: 0,
            active: None,
            final_text: None,
            alive: true,
            initializing: false,
            shutdown: false,
        }
    }
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn shutdown(&self) {
        let mut state = self.lock();
        state.shutdown = true;
        if let Some(active) = &state.active {
            active.cancel.cancel();
        }
        state.final_text = None;
        state.status = Status::Idle;
        drop(state);
        self.changed.notify_all();
    }

    fn next(&self) -> Option<(u64, Cancellation, Instant)> {
        let mut state = self.lock();
        loop {
            if state.shutdown {
                return None;
            }
            if let Some(active) = state.active.as_mut()
                && !active.claimed
            {
                active.claimed = true;
                return Some((active.operation, active.cancel.clone(), active.started));
            }
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn transition(&self, operation: u64, status: Status) -> Result<()> {
        let mut state = self.lock();
        let active = state.active.as_ref().ok_or(Error::Cancelled)?;
        if active.operation != operation || active.cancel.is_cancelled() || state.shutdown {
            return Err(Error::Cancelled);
        }
        state.status = status;
        drop(state);
        self.changed.notify_all();
        Ok(())
    }

    fn complete(&self, operation: u64, outcome: Result<Option<String>>) {
        let mut state = self.lock();
        let Some(active) = state.active.as_ref() else {
            return;
        };
        if active.operation != operation {
            return;
        }
        let cancelled = active.cancel.is_cancelled() || state.shutdown;
        state.active = None;
        if cancelled {
            // Preserve the cancellation terminal state, never publish stale text/error.
            state.status = Status::Idle;
        } else {
            match outcome {
                Ok(text) => {
                    state.final_text = text.map(|text| FinalText { operation, text });
                    state.status = Status::Idle;
                }
                Err(Error::Backend(message)) => {
                    // Only runtime-authored messages reach this point: every
                    // adapter error is replaced at the call boundary above.
                    state.status = Status::Failed { message };
                }
                Err(_) => {
                    state.status = failure(
                        "dictation failed; check configured audio, model, and delivery capabilities",
                    );
                }
            }
        }
        drop(state);
        self.changed.notify_all();
    }
}

#[cfg(test)]
mod tests;
