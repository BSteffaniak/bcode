//! Final-recording speech gates, independent of capture and recognition.
//!
//! Without a configured VAD model, [`gate`] is an **energy baseline, not a
//! speech classifier**. It rejects
//! digital silence, DC, low-level noise and sparse impulses in full windows. Audible noise,
//! music or another speaker can pass; quiet speech below its fixed floor can
//! be rejected. A `true` result means only "worth trying recognition", not
//! proof of speech. No input is trimmed, so short utterances and trailing
//! partial windows remain available to the recognizer.
//!
//! [`digital_silence_gate`] provides the least destructive alternative: reject
//! only empty/exactly zero audio. [`silero_gate`] performs model-based VAD, but
//! requires a separately installed Silero GGUF model and `local-inference`.
//! Set `Config::vad_model_path` to select Silero through [`gate`], or call
//! [`silero_gate`] explicitly. Neither constructor falls back on model failure.
//! None of these gates downloads models, logs audio,
//! invokes recognition, or falls back to a cloud service.

use crate::config::Config;
use crate::contracts::{Error, Recording, Result, SpeechGate};
use std::path::Path;

#[cfg(feature = "local-inference")]
mod silero;
#[cfg(test)]
mod tests;

// About -54 dBFS. This is an explicit fixed noise floor, not calibrated VAD.
const ENERGY_RMS_FLOOR: f64 = 0.002;
const WINDOWS_PER_SECOND: u32 = 100;

/// Construct the configured Silero gate, or the model-free energy baseline.
///
/// When `Config::vad_model_path` is set, delegates to [`silero_gate`] without
/// fallback. Otherwise see the module documentation for the energy baseline's
/// false-positive/false-negative limitations.
///
/// # Errors
/// Returns an error for invalid configuration or any [`silero_gate`] construction
/// failure when a VAD model is configured. Each call to `has_speech` rejects
/// invalid PCM/rates and recordings exceeding the configured limit; the Silero
/// gate additionally requires 16 kHz PCM and reports inference failures.
pub fn gate(config: &Config) -> Result<Box<dyn SpeechGate>> {
    if let Some(path) = &config.vad_model_path {
        return silero_gate(config, path);
    }
    Ok(Box::new(EnergyGate {
        limits: InputLimits::new(config)?,
    }))
}

/// Construct a gate that rejects only empty or exact digital silence.
///
/// This deliberately admits every nonzero finite recording, including noise
/// and DC. Unlike the energy baseline it has no quiet-speech threshold.
///
/// # Errors
/// Returns an error for invalid configuration. Input errors are reported by
/// `has_speech`, identically to [`gate`].
pub fn digital_silence_gate(config: &Config) -> Result<Box<dyn SpeechGate>> {
    Ok(Box::new(DigitalSilenceGate {
        limits: InputLimits::new(config)?,
    }))
}

/// Construct a CPU Silero VAD gate from an explicitly installed GGUF model.
///
/// This is separate from the ASR model at `Config::model_path`. No model is
/// discovered or downloaded implicitly, and failures never select the energy
/// baseline instead. Input must be mono 16 kHz PCM; resampling belongs to the
/// capture/audio adapter. Model decisions are probabilistic, not guarantees.
///
/// # Errors
/// Returns `Unsupported` without `local-inference`, for a non-Silero model, or
/// for unsupported model input geometry. Invalid configuration, inaccessible
/// model files, model loading and session failures also return safe errors.
/// At inference time invalid input and model failures return errors.
pub fn silero_gate(config: &Config, model_path: &Path) -> Result<Box<dyn SpeechGate>> {
    let limits = InputLimits::new(config)?;
    #[cfg(feature = "local-inference")]
    {
        silero::load(limits, model_path)
    }
    #[cfg(not(feature = "local-inference"))]
    {
        let _ = (limits, model_path);
        Err(Error::Unsupported(
            "Silero VAD requires the local-inference feature and a separate VAD model".into(),
        ))
    }
}

#[derive(Clone, Copy)]
struct InputLimits {
    max_seconds: u32,
}

impl InputLimits {
    fn new(config: &Config) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            max_seconds: config.max_recording_seconds,
        })
    }

    fn validate(self, recording: &Recording) -> Result<()> {
        if !(8_000..=192_000).contains(&recording.sample_rate) {
            return Err(Error::Invalid(
                "speech gating requires a sample rate from 8000 through 192000 Hz".into(),
            ));
        }
        let max_samples = u64::from(recording.sample_rate) * u64::from(self.max_seconds);
        let sample_count = u64::try_from(recording.samples.len())
            .map_err(|_| Error::Invalid("recording exceeds the speech gate limit".into()))?;
        if sample_count > max_samples {
            return Err(Error::Invalid(
                "recording exceeds the configured duration limit".into(),
            ));
        }
        // Validate the entire buffer before any early acceptance. In particular,
        // a speech-like prefix must not conceal a NaN or infinity in its tail.
        if recording
            .samples
            .iter()
            .any(|sample| !sample.is_finite() || !(-1.0..=1.0).contains(sample))
        {
            return Err(Error::Invalid(
                "speech gating requires finite normalized PCM in [-1, 1]".into(),
            ));
        }
        Ok(())
    }
}

struct DigitalSilenceGate {
    limits: InputLimits,
}

impl SpeechGate for DigitalSilenceGate {
    fn has_speech(&mut self, recording: &Recording) -> Result<bool> {
        self.limits.validate(recording)?;
        Ok(recording.samples.iter().any(|sample| *sample != 0.0))
    }
}

struct EnergyGate {
    limits: InputLimits,
}

impl SpeechGate for EnergyGate {
    fn has_speech(&mut self, recording: &Recording) -> Result<bool> {
        self.limits.validate(recording)?;
        let window_samples = usize::try_from(recording.sample_rate.div_ceil(WINDOWS_PER_SECOND))
            .map_err(|_| Error::Invalid("unsupported speech gate sample rate".into()))?;
        Ok(recording
            .samples
            .chunks(window_samples)
            .any(window_has_energy))
    }
}

fn window_has_energy(samples: &[f32]) -> bool {
    let (sum, count) = samples.iter().fold((0.0, 0.0), |(sum, count), sample| {
        (sum + f64::from(*sample), count + 1.0)
    });
    if samples.is_empty() {
        return false;
    }
    let mean = sum / count;
    let energy = samples
        .iter()
        .map(|sample| (f64::from(*sample) - mean).powi(2))
        .sum::<f64>()
        / count;
    if energy < ENERGY_RMS_FLOOR * ENERGY_RMS_FLOOR {
        return false;
    }
    // Require activity over at least 10% of this window, not just a click.
    // Relative to the window RMS rather than its mean, so a large impulse's
    // DC correction does not make the rest of its silent window look active.
    // No minimum utterance duration: even the final partial window is scored.
    let activity_floor = energy.sqrt() * 0.5;
    let active_samples = samples
        .iter()
        .filter(|sample| (f64::from(**sample) - mean).abs() >= activity_floor)
        .count();
    active_samples >= samples.len().div_ceil(10)
}
