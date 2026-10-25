//! Optional real Silero VAD. The session owns its native model lifetime.
use super::InputLimits;
use crate::contracts::{Error, Recording, Result, SpeechGate};
use std::path::Path;
use transcribe_cpp::{Backend, Model, ModelOptions, VadOptions, VadSession};

pub(super) fn load(limits: InputLimits, model_path: &Path) -> Result<Box<dyn SpeechGate>> {
    if !model_path.is_file() {
        return Err(Error::Invalid(
            "Silero VAD requires an existing, separately installed model file".into(),
        ));
    }
    let model = Model::load_with(
        model_path,
        &ModelOptions {
            backend: Backend::Cpu,
            ..ModelOptions::default()
        },
    )
    .map_err(|_| Error::Backend("could not load the Silero VAD model".into()))?;
    if model.arch() != "silero_vad" {
        return Err(Error::Unsupported(
            "speech gating requires a Silero VAD model, not an ASR model".into(),
        ));
    }
    let info = model
        .vad_info()
        .map_err(|_| Error::Backend("could not read Silero VAD input requirements".into()))?;
    if info.sample_rate != 16_000 || info.frame_samples != 512 {
        return Err(Error::Unsupported(
            "unsupported Silero VAD input geometry".into(),
        ));
    }
    let session = model
        .vad_session()
        .map_err(|_| Error::Backend("could not create the Silero VAD session".into()))?;
    Ok(Box::new(SileroGate { limits, session }))
}

struct SileroGate {
    limits: InputLimits,
    session: VadSession,
}

impl SpeechGate for SileroGate {
    fn has_speech(&mut self, recording: &Recording) -> Result<bool> {
        self.limits.validate(recording)?;
        if recording.sample_rate != 16_000 {
            return Err(Error::Invalid("Silero VAD requires 16000 Hz PCM".into()));
        }
        if !recording.samples.iter().any(|sample| *sample != 0.0) {
            return Ok(false);
        }
        // Do not apply Silero's default 250 ms segment deletion to short words.
        // run() starts from fresh state and scores the final partial frame too.
        let result = self
            .session
            .run(
                &recording.samples,
                &VadOptions {
                    min_speech_ms: 0,
                    ..VadOptions::default()
                },
            )
            .map_err(|_| Error::Backend("Silero VAD inference failed".into()))?;
        decide(&result.probs, recording.samples.len())
    }
}

fn decide(probabilities: &[f32], sample_count: usize) -> Result<bool> {
    if probabilities.len() != sample_count.div_ceil(512)
        || probabilities
            .iter()
            .any(|probability| !probability.is_finite() || !(0.0..=1.0).contains(probability))
    {
        return Err(Error::Backend(
            "Silero VAD returned invalid probabilities".into(),
        ));
    }
    // One positive frame is enough, including for clips below 32 ms. This
    // favors short-word recall; it does not make model predictions infallible.
    Ok(probabilities.iter().any(|probability| *probability >= 0.5))
}

#[cfg(test)]
mod tests {
    use super::decide;

    #[test]
    fn silence_and_noise_predictions_do_not_open_the_gate() {
        assert!(!decide(&[0.001, 0.03, 0.49], 1536).unwrap());
    }

    #[test]
    fn one_positive_short_frame_is_preserved() {
        assert!(decide(&[0.75], 160).unwrap());
        assert!(decide(&[0.01, 0.5], 513).unwrap());
    }

    #[test]
    fn invalid_model_output_is_an_error_not_a_fallback() {
        for probabilities in [
            vec![],
            vec![f32::NAN],
            vec![f32::INFINITY],
            vec![1.1],
            vec![-0.1],
        ] {
            assert!(decide(&probabilities, 160).is_err());
        }
    }
}
