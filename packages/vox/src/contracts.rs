//! Portable dictation contracts. Backend/framework types never cross this boundary.
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
/// Domain result.
pub type Result<T> = std::result::Result<T, Error>;
/// Explicit errors; messages must not contain audio, transcripts, or credentials.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Invalid user configuration or input.
    #[error("invalid input: {0}")]
    Invalid(String),
    /// Required capability not available.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// Another operation owns the resource.
    #[error("Vox is busy")]
    Busy,
    /// Operation cancelled.
    #[error("cancelled")]
    Cancelled,
    /// OS IO failure.
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// Backend failed, with safe context.
    #[error("backend failure: {0}")]
    Backend(String),
}
/// Thread-safe cancellation signal.
#[derive(Clone, Default, Debug)]
pub struct Cancellation(Arc<AtomicBool>);
impl Cancellation {
    /// New independent signal.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    /// Cancel all copies.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    /// Whether cancellation was requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
    /// Fail when cancelled.
    ///
    /// # Errors
    /// Returns [`Error::Cancelled`] if cancellation was requested.
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
}
/// Owned mono floating-point recording.
#[derive(Clone, Debug)]
pub struct Recording {
    /// Samples in [-1,1].
    pub samples: Vec<f32>,
    /// Frames per second.
    pub sample_rate: u32,
}
/// Recording handle owned by one operation. Capture adapters need not be Send.
pub trait Capture {
    /// Stop recording and finalize buffered audio.
    ///
    /// # Errors
    /// Returns capture device, buffering, or invalid recording errors.
    fn finish(self: Box<Self>) -> Result<Recording>;
}
/// Microphone factory; create and finish on the runtime worker.
pub trait AudioSource: Send {
    /// Start capture with a bounded duration.
    ///
    /// # Errors
    /// Returns invalid duration, unavailable device, or capture initialization errors.
    fn start(&mut self, max_seconds: u32) -> Result<Box<dyn Capture>>;
}
/// Recognition settings for one finalized recording.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RecognitionOptions {
    /// Optional language hint.
    pub language: Option<String>,
    /// Priority vocabulary terms.
    pub vocabulary: Vec<String>,
}
/// Final text from recognition.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Transcript {
    /// Plain final text; never automatically logged.
    pub text: String,
}
/// Speech recognizer, called only on its dedicated worker.
pub trait Recognizer: Send {
    /// Recognize or cancel without delivering partial output.
    ///
    /// # Errors
    /// Returns invalid input, unavailable model/backend, inference, or cancellation errors.
    fn recognize(
        &mut self,
        recording: &Recording,
        options: &RecognitionOptions,
        cancel: &Cancellation,
    ) -> Result<Transcript>;
}
/// Speech gate returns whether a recording contains speech.
pub trait SpeechGate: Send {
    /// Reject silence before inference.
    ///
    /// # Errors
    /// Returns invalid audio or gate backend errors.
    fn has_speech(&mut self, recording: &Recording) -> Result<bool>;
}
/// Final text transformation, independent of delivery.
pub trait TextProcessor: Send {
    /// Apply configured deterministic edits.
    ///
    /// # Errors
    /// Returns invalid configuration or input exceeding processing bounds.
    fn process(&self, text: &str) -> Result<String>;
}
/// Explicit delivery policy; no automatic clipboard fallback.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryMode {
    /// Type Unicode into current focus.
    #[default]
    Typing,
    /// Explicit clipboard-based paste.
    Clipboard,
    /// Write final text to stdout.
    Stdout,
    /// Return text to an embedding frontend.
    Host,
}
/// Delivery is attempted once. Failure may mean partial delivery: never auto-retry.
pub trait TextSink: Send {
    /// Deliver literal text using this sink's policy.
    ///
    /// # Errors
    /// Returns unsafe input, cancellation, unsupported backend, or transport errors.
    /// Failure may follow partial delivery; callers must not retry automatically.
    fn deliver(&mut self, text: &str, cancel: &Cancellation) -> Result<()>;
}
/// Generic control action.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Start recording.
    Start,
    /// Stop and transcribe.
    Stop,
    /// Toggle start/stop.
    Toggle,
    /// Cancel current work.
    Cancel,
}
/// Portable runtime state.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Status {
    /// No operation.
    Idle,
    /// Capturing.
    Recording {
        /// Unique operation number.
        operation: u64,
    },
    /// Processing.
    Transcribing {
        /// Unique operation number.
        operation: u64,
    },
    /// Delivering final output.
    Delivering {
        /// Unique operation number.
        operation: u64,
    },
    /// Safe diagnostic only.
    Failed {
        /// Redacted actionable message.
        message: String,
    },
}
