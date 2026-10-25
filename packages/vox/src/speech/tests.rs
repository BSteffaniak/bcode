use super::{digital_silence_gate, gate, silero_gate};
use crate::config::Config;
use crate::contracts::{Error, Recording, SpeechGate};

fn recording(samples: Vec<f32>, sample_rate: u32) -> Recording {
    Recording {
        samples,
        sample_rate,
    }
}

// Deterministic, oscillating synthetic signal; this is NOT a speech fixture.
fn alternating(length: usize, amplitude: f32) -> Vec<f32> {
    (0..length)
        .map(|index| {
            if index % 2 == 0 {
                amplitude
            } else {
                -amplitude
            }
        })
        .collect()
}

fn baseline() -> Box<dyn SpeechGate> {
    gate(&Config::default()).unwrap()
}

#[test]
fn empty_and_digital_silence_are_rejected() {
    for rate in [8_000, 16_000, 44_100, 48_000, 192_000] {
        let mut gate = baseline();
        assert!(!gate.has_speech(&recording(vec![], rate)).unwrap());
        assert!(!gate.has_speech(&recording(vec![0.0; 1000], rate)).unwrap());
        assert!(!gate.has_speech(&recording(vec![-0.0; 1000], rate)).unwrap());
    }
}

#[test]
fn low_level_noise_and_dc_do_not_trigger_recognition() {
    let mut gate = baseline();
    assert!(
        !gate
            .has_speech(&recording(alternating(16_000, 0.001), 16_000))
            .unwrap()
    );
    for dc in [-1.0, -0.1, 0.1, 1.0] {
        assert!(
            !gate
                .has_speech(&recording(vec![dc; 16_000], 16_000))
                .unwrap()
        );
    }
}

#[test]
fn an_isolated_full_scale_click_is_rejected() {
    for position in [0, 79, 159, 160, 8000, 15_999] {
        let mut samples = vec![0.0; 16_000];
        samples[position] = 1.0;
        assert!(!baseline().has_speech(&recording(samples, 16_000)).unwrap());
    }
}

#[test]
fn short_activity_is_not_diluted_by_long_silence_or_dropped_at_the_tail() {
    let short = alternating(80, 0.02); // 5 ms, shorter than a scoring window.
    assert!(
        baseline()
            .has_speech(&recording(short.clone(), 16_000))
            .unwrap()
    );
    let mut tail = vec![0.0; 16_000];
    tail.extend(short.clone());
    assert!(baseline().has_speech(&recording(tail, 16_000)).unwrap());
    let mut prefix = short;
    prefix.resize(16_000, 0.0);
    assert!(baseline().has_speech(&recording(prefix, 16_000)).unwrap());
}

#[test]
fn baseline_honestly_admits_audible_non_speech_noise() {
    // Energy is not speech: loud alternating interference passes this gate.
    assert!(
        baseline()
            .has_speech(&recording(alternating(1600, 0.2), 16_000))
            .unwrap()
    );
}

#[test]
fn digital_gate_preserves_nonzero_quiet_audio_but_has_no_noise_discrimination() {
    let mut gate = digital_silence_gate(&Config::default()).unwrap();
    assert!(!gate.has_speech(&recording(vec![0.0; 160], 16_000)).unwrap());
    assert!(!gate.has_speech(&recording(vec![], 16_000)).unwrap());
    assert!(
        gate.has_speech(&recording(vec![f32::MIN_POSITIVE], 16_000))
            .unwrap()
    );
    assert!(gate.has_speech(&recording(vec![0.1; 160], 16_000)).unwrap());
}

#[test]
fn invalid_pcm_anywhere_is_an_error_even_after_an_active_prefix() {
    for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1.01, -1.01] {
        for mut gate in [
            baseline(),
            digital_silence_gate(&Config::default()).unwrap(),
        ] {
            let mut samples = alternating(160, 0.1);
            samples.push(value);
            assert!(matches!(
                gate.has_speech(&recording(samples, 16_000)),
                Err(Error::Invalid(_))
            ));
        }
    }
}

#[test]
fn invalid_rates_and_overlong_recordings_are_errors() {
    let config = Config {
        max_recording_seconds: 1,
        ..Config::default()
    };
    for mut gate in [
        gate(&config).unwrap(),
        digital_silence_gate(&config).unwrap(),
    ] {
        for rate in [0, 7999, 192_001, u32::MAX] {
            assert!(matches!(
                gate.has_speech(&recording(vec![], rate)),
                Err(Error::Invalid(_))
            ));
        }
        assert!(!gate.has_speech(&recording(vec![0.0; 8000], 8000)).unwrap());
        assert!(matches!(
            gate.has_speech(&recording(vec![0.0; 8001], 8000)),
            Err(Error::Invalid(_))
        ));
    }
}

#[test]
fn configured_silero_failure_never_selects_the_energy_baseline() {
    let directory = tempfile::tempdir().unwrap();
    let config = Config {
        vad_model_path: Some(directory.path().join("missing.gguf")),
        ..Config::default()
    };
    assert!(gate(&config).is_err());
}

#[test]
fn construction_rejects_invalid_configuration() {
    let config = Config {
        max_recording_seconds: 0,
        ..Config::default()
    };
    assert!(gate(&config).is_err());
    assert!(digital_silence_gate(&config).is_err());
    assert!(silero_gate(&config, std::path::Path::new("unused.gguf")).is_err());
}

#[test]
fn gating_does_not_trim_or_normalize_the_original_recording() {
    let input = recording(alternating(81, 0.1), 16_000);
    let original = input.samples.clone();
    assert!(baseline().has_speech(&input).unwrap());
    assert_eq!(input.samples, original);
    assert_eq!(input.sample_rate, 16_000);
}

#[test]
fn separate_calls_have_no_noise_floor_or_speech_history() {
    let mut gate = baseline();
    assert!(
        gate.has_speech(&recording(alternating(160, 0.02), 16_000))
            .unwrap()
    );
    assert!(!gate.has_speech(&recording(vec![0.0; 160], 16_000)).unwrap());
    assert!(
        gate.has_speech(&recording(alternating(160, 0.02), 16_000))
            .unwrap()
    );
}

#[cfg(not(feature = "local-inference"))]
#[test]
fn explicit_silero_request_never_falls_back_without_native_support() {
    assert!(matches!(
        silero_gate(&Config::default(), std::path::Path::new("unused.gguf")),
        Err(Error::Unsupported(_))
    ));
}

#[cfg(feature = "local-inference")]
#[test]
fn explicit_silero_request_never_falls_back_on_an_invalid_model() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("missing.gguf");
    assert!(matches!(
        silero_gate(&Config::default(), &missing),
        Err(Error::Invalid(_))
    ));
    let invalid = directory.path().join("invalid.gguf");
    std::fs::write(&invalid, b"not a model").unwrap();
    assert!(matches!(
        silero_gate(&Config::default(), &invalid),
        Err(Error::Backend(_))
    ));
}
