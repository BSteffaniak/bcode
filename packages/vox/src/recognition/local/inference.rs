//! Safe transcribe-cpp adapter. Native errors and logs never escape this module.

use std::{
    sync::{Once, mpsc},
    thread,
    time::Duration,
};

use transcribe_cpp::{
    Backend, CancelToken, Capabilities, Feature, Model, ModelOptions, Role, RunOptions, Session,
    TimestampKind,
};

use crate::{
    Error, Result,
    config::Config,
    contracts::{Cancellation, RecognitionOptions, Recognizer, Recording, Transcript},
};

const SAMPLE_RATE: u32 = 16_000;
const CANCEL_POLL: Duration = Duration::from_millis(5);
static SILENCE_NATIVE_LOGS: Once = Once::new();

pub(super) fn recognizer(config: &Config) -> Result<Box<dyn Recognizer>> {
    config.validate()?;
    let backend = compute_backend(&config.compute)?;
    let path = config.model_path.as_ref().ok_or_else(|| {
        Error::Invalid("local recognition requires an installed model_path".into())
    })?;
    // The native sink is process-global. Never route native output (which may
    // include prompts or vocabulary) into a host logger or default stderr.
    SILENCE_NATIVE_LOGS.call_once(transcribe_cpp::disable_logging);
    let model = Model::load_with(
        path,
        &ModelOptions {
            backend,
            ..ModelOptions::default()
        },
    )
    .map_err(|error| native_error(&error))?;
    if !model.roles().contains(Role::Asr) {
        return Err(Error::Unsupported(
            "local model does not support transcription".into(),
        ));
    }
    let capabilities = model.capabilities().map_err(|error| native_error(&error))?;
    if capabilities.native_sample_rate != 16_000 {
        return Err(Error::Unsupported(
            "local model requires an unsupported sample rate".into(),
        ));
    }
    if !model.supports(Feature::Cancellation) {
        return Err(Error::Unsupported(
            "local model does not support cooperative cancellation".into(),
        ));
    }
    // Backend requests are strict in the native API; additionally fail closed
    // if an explicit GPU request nevertheless reports a different backend.
    let expected = match backend {
        Backend::Cpu => Some("cpu"),
        Backend::Vulkan => Some("vulkan"),
        Backend::Metal => Some("metal"),
        _ => None,
    };
    if expected.is_some_and(|value| model.backend() != value) {
        return Err(Error::Unsupported(
            "requested local compute backend was not selected".into(),
        ));
    }
    let vocabulary = model.supports(Feature::Vocabulary);
    validate_options(&capabilities, vocabulary, &config.recognition)?;
    let session = model.session().map_err(|error| native_error(&error))?;
    let limits = session.limits().map_err(|error| native_error(&error))?;
    Ok(Box::new(LocalRecognizer {
        session,
        capabilities,
        vocabulary,
        max_audio_ms: limits.effective_max_audio_ms,
        max_recording_seconds: config.max_recording_seconds,
    }))
}

fn compute_backend(value: &str) -> Result<Backend> {
    match value {
        "cpu" => Ok(Backend::Cpu),
        "cpu-accel" => Ok(Backend::CpuAccel),
        "auto" => Ok(Backend::Auto),
        "vulkan" if cfg!(feature = "vulkan") => Ok(Backend::Vulkan),
        "metal" if cfg!(all(feature = "metal", target_os = "macos")) => Ok(Backend::Metal),
        "vulkan" | "metal" | "cuda" | "rocm" => Err(Error::Unsupported(
            "requested compute backend is not enabled in this Vox build".into(),
        )),
        _ => Err(Error::Invalid(
            "compute must be cpu, cpu-accel, auto, vulkan, or metal".into(),
        )),
    }
}

struct LocalRecognizer {
    // Session retains the model internally; neither weights nor compute state
    // are reloaded between utterances. &mut self serializes this session.
    session: Session,
    capabilities: Capabilities,
    vocabulary: bool,
    max_audio_ms: i64,
    max_recording_seconds: u32,
}

impl Recognizer for LocalRecognizer {
    fn recognize(
        &mut self,
        recording: &Recording,
        options: &RecognitionOptions,
        cancel: &Cancellation,
    ) -> Result<Transcript> {
        cancel.check()?;
        validate_options(&self.capabilities, self.vocabulary, options)?;
        validate_recording(
            recording,
            self.max_recording_seconds,
            &[self.capabilities.max_audio_ms, self.max_audio_ms],
        )?;
        let run_options = RunOptions {
            timestamps: TimestampKind::None,
            language: options.language.clone(),
            vocabulary: options.vocabulary.clone(),
            ..RunOptions::default()
        };
        // No streaming callback is installed. Partial native transcripts are
        // dropped, including successful-looking results after cancellation.
        with_cancellation(cancel, |token| {
            self.session.set_cancel_token(token);
            let result = self.session.run(&recording.samples, &run_options);
            let aborted = self.session.was_aborted();
            let truncated = self.session.was_truncated();
            self.session.clear_cancel_token();
            final_transcript(result, aborted, truncated)
        })
    }
}

fn validate_options(
    caps: &Capabilities,
    vocabulary: bool,
    options: &RecognitionOptions,
) -> Result<()> {
    if let Some(language) = &options.language {
        if language.is_empty()
            || language
                .chars()
                .any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(Error::Invalid(
                "language must be a nonempty language code".into(),
            ));
        }
        if !caps.languages.is_empty() && !caps.languages.contains(language) {
            return Err(Error::Unsupported(
                "local model does not support the requested language".into(),
            ));
        }
    }
    if !options.vocabulary.is_empty() && !vocabulary {
        // Upstream may otherwise silently ignore vocabulary with a warning.
        return Err(Error::Unsupported(
            "local model does not support priority vocabulary".into(),
        ));
    }
    if options
        .vocabulary
        .iter()
        .any(|word| word.trim().is_empty() || word.contains('\0'))
    {
        return Err(Error::Invalid(
            "vocabulary terms must be nonempty and contain no NUL".into(),
        ));
    }
    Ok(())
}

fn validate_recording(recording: &Recording, max_seconds: u32, limits_ms: &[i64]) -> Result<()> {
    if recording.sample_rate != SAMPLE_RATE {
        return Err(Error::Invalid(
            "local recognition requires 16000 Hz mono audio".into(),
        ));
    }
    if recording.samples.is_empty() {
        return Err(Error::Invalid(
            "local recognition requires nonempty audio".into(),
        ));
    }
    let count = u64::try_from(recording.samples.len())
        .map_err(|_| Error::Invalid("recording exceeds local input limits".into()))?;
    if count > u64::from(max_seconds) * u64::from(SAMPLE_RATE)
        || count > 2_147_483_647
        || limits_ms.iter().any(|limit| {
            u64::try_from(*limit)
                .ok()
                .filter(|ms| *ms > 0)
                .is_some_and(|ms| count > ms.saturating_mul(16))
        })
    {
        return Err(Error::Invalid(
            "recording exceeds configured or model duration limits".into(),
        ));
    }
    if recording
        .samples
        .iter()
        .any(|sample| !sample.is_finite() || !(-1.0..=1.0).contains(sample))
    {
        return Err(Error::Invalid(
            "recording samples must be finite and within [-1, 1]".into(),
        ));
    }
    Ok(())
}

/// Poll the portable token on a scoped bridge thread, never on native callbacks.
/// Completion wakes the bridge immediately. Disconnect also ends it on unwind.
fn with_cancellation<T>(
    cancel: &Cancellation,
    run: impl FnOnce(&CancelToken) -> Result<T>,
) -> Result<T> {
    cancel.check()?;
    let token = CancelToken::new();
    thread::scope(|scope| {
        let (done, completion) = mpsc::channel();
        let bridge_cancel = cancel.clone();
        let bridge_token = token.clone();
        let bridge = thread::Builder::new()
            .name("vox-local-cancel".into())
            .spawn_scoped(scope, move || {
                loop {
                    if bridge_cancel.is_cancelled() {
                        bridge_token.cancel();
                        break;
                    }
                    match completion.recv_timeout(CANCEL_POLL) {
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
            .map_err(|_| Error::Backend("could not start local cancellation bridge".into()))?;
        // Check again after thread creation so a pre-inference cancellation
        // never has to wait for the bridge's first scheduling opportunity.
        if cancel.is_cancelled() {
            token.cancel();
        }
        let result = if token.is_cancelled() {
            Err(Error::Cancelled)
        } else {
            run(&token)
        };
        let _ = done.send(());
        bridge
            .join()
            .map_err(|_| Error::Backend("local cancellation bridge failed".into()))?;
        cancel.check()?;
        result
    })
}

fn final_transcript(
    result: transcribe_cpp::Result<transcribe_cpp::Transcript>,
    aborted: bool,
    truncated: bool,
) -> Result<Transcript> {
    if aborted {
        return Err(Error::Cancelled);
    }
    if truncated {
        return Err(Error::Backend(
            "local decode incomplete; no transcript delivered".into(),
        ));
    }
    result
        .map(|value| Transcript { text: value.text })
        .map_err(|error| native_error(&error))
}

fn native_error(error: &transcribe_cpp::Error) -> Error {
    use transcribe_cpp::Error as Native;
    // Never format a native error: it may own a partial transcript, vocabulary,
    // prompt, model path, or other caller-supplied text.
    match error {
        Native::Aborted { .. } => Error::Cancelled,
        Native::Busy(_) => Error::Busy,
        Native::OutputTruncated { .. } | Native::OutputRepetition { .. } => {
            Error::Backend("local decode incomplete; no transcript delivered".into())
        }
        Native::NotImplemented(_) | Native::Unsupported(_) | Native::UnsupportedRole(_) => {
            Error::Unsupported("local model does not support the recognition request".into())
        }
        Native::InvalidArgument(_)
        | Native::Nul(_)
        | Native::InputTooLong(_)
        | Native::InputTooShort(_) => {
            Error::Invalid("local recognition input is invalid or outside model limits".into())
        }
        Native::ModelFileNotFound(_) => {
            Error::Invalid("configured local model file was not found".into())
        }
        Native::VersionMismatch(_) | Native::BadStructSize(_) => {
            Error::Unsupported("local inference library is incompatible with this build".into())
        }
        _ => Error::Backend(
            "local inference failed; check installed model and compute configuration".into(),
        ),
    }
}

#[cfg(test)]
mod tests;
