use super::*;

fn capabilities() -> Capabilities {
    Capabilities {
        native_sample_rate: 16_000,
        languages: vec!["en".into()],
        translate_target_languages: Vec::new(),
        max_timestamp_kind: TimestampKind::None,
        supports_language_detect: false,
        supports_translate: false,
        supports_streaming: false,
        supports_spec_decode: false,
        max_audio_ms: 0,
    }
}

fn recording(samples: Vec<f32>) -> Recording {
    Recording {
        samples,
        sample_rate: SAMPLE_RATE,
    }
}

#[test]
fn cpu_is_strict_and_auto_requires_explicit_selection() {
    assert_eq!(
        compute_backend(&Config::default().compute).unwrap(),
        Backend::Cpu
    );
    assert_eq!(compute_backend("auto").unwrap(), Backend::Auto);
    assert_eq!(compute_backend("cpu-accel").unwrap(), Backend::CpuAccel);
    assert!(matches!(compute_backend("bogus"), Err(Error::Invalid(_))));
    assert!(matches!(
        compute_backend("cuda"),
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        compute_backend("rocm"),
        Err(Error::Unsupported(_))
    ));
    if !cfg!(feature = "vulkan") {
        assert!(matches!(
            compute_backend("vulkan"),
            Err(Error::Unsupported(_))
        ));
    }
    if !cfg!(all(feature = "metal", target_os = "macos")) {
        assert!(matches!(
            compute_backend("metal"),
            Err(Error::Unsupported(_))
        ));
    }
}

#[test]
fn model_is_required_without_downloading_or_falling_back() {
    assert!(matches!(
        recognizer(&Config::default()),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn missing_model_has_a_safe_explicit_error() {
    let directory = tempfile::tempdir().unwrap();
    let config = Config {
        model_path: Some(directory.path().join("private-model-path-do-not-disclose")),
        ..Config::default()
    };
    let Err(error) = recognizer(&config) else {
        panic!("missing model unexpectedly loaded");
    };
    assert!(matches!(error, Error::Invalid(_)));
    assert!(
        !error
            .to_string()
            .contains("private-model-path-do-not-disclose")
    );
}

#[test]
fn vocabulary_is_not_silently_ignored() {
    let options = RecognitionOptions {
        language: Some("en".into()),
        vocabulary: vec!["Vox".into()],
    };
    assert!(matches!(
        validate_options(&capabilities(), false, &options),
        Err(Error::Unsupported(_))
    ));
    assert!(validate_options(&capabilities(), true, &options).is_ok());
}

#[test]
fn language_gating_uses_advertised_capabilities() {
    let mut options = RecognitionOptions {
        language: Some("fr".into()),
        vocabulary: Vec::new(),
    };
    assert!(matches!(
        validate_options(&capabilities(), true, &options),
        Err(Error::Unsupported(_))
    ));
    // Empty language metadata means language-agnostic, not "supports none".
    let mut caps = capabilities();
    caps.languages.clear();
    assert!(validate_options(&caps, true, &options).is_ok());
    options.language = None;
    assert!(validate_options(&capabilities(), true, &options).is_ok());
    for value in ["", "en us", "en\0", "\nen"] {
        options.language = Some(value.into());
        assert!(matches!(
            validate_options(&caps, true, &options),
            Err(Error::Invalid(_))
        ));
    }
}

#[test]
fn malformed_vocabulary_never_reaches_native_code() {
    for value in ["", "  ", "term\0suffix"] {
        let options = RecognitionOptions {
            language: None,
            vocabulary: vec![value.into()],
        };
        assert!(matches!(
            validate_options(&capabilities(), true, &options),
            Err(Error::Invalid(_))
        ));
    }
}

#[test]
fn only_nonempty_finite_bounded_16khz_audio_is_accepted() {
    assert!(validate_recording(&recording(vec![-1.0, 0.0, 1.0]), 1, &[0]).is_ok());
    for samples in [
        vec![],
        vec![f32::NAN],
        vec![f32::INFINITY],
        vec![-1.01],
        vec![1.01],
    ] {
        assert!(matches!(
            validate_recording(&recording(samples), 1, &[0]),
            Err(Error::Invalid(_))
        ));
    }
    let invalid_rate = Recording {
        samples: vec![0.1],
        sample_rate: 48_000,
    };
    assert!(matches!(
        validate_recording(&invalid_rate, 1, &[0]),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn duration_limits_reject_instead_of_truncating_audio() {
    let exact = recording(vec![0.1; 16_000]);
    assert!(validate_recording(&exact, 1, &[0, 1000]).is_ok());
    assert!(matches!(
        validate_recording(&exact, 1, &[999]),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        validate_recording(&exact, 1, &[1000, 999]),
        Err(Error::Invalid(_))
    ));
    let too_long = recording(vec![0.1; 16_001]);
    assert!(matches!(
        validate_recording(&too_long, 1, &[0]),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        validate_recording(&too_long, 2, &[1000]),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn already_cancelled_does_not_start_work() {
    let cancel = Cancellation::new();
    cancel.cancel();
    let result: Result<()> = with_cancellation(&cancel, |_| panic!("cancelled work was started"));
    assert!(matches!(result, Err(Error::Cancelled)));
}

#[test]
fn bridge_propagates_cross_thread_cancellation_during_work() {
    let cancel = Cancellation::new();
    let (started, ready) = mpsc::channel();
    thread::scope(|scope| {
        let producer_cancel = cancel.clone();
        scope.spawn(move || {
            ready.recv_timeout(Duration::from_secs(2)).unwrap();
            producer_cancel.cancel();
        });
        let result = with_cancellation(&cancel, |token| {
            started.send(()).unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while !token.is_cancelled() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "native cancellation not forwarded"
                );
                thread::yield_now();
            }
            // Even a backend that returns successful partial text after an
            // abort cannot cause the portable wrapper to return a transcript.
            Ok(Transcript {
                text: "must not escape".into(),
            })
        });
        assert!(matches!(result, Err(Error::Cancelled)));
    });
}

#[test]
fn cancellation_at_completion_discards_success_and_next_run_is_independent() {
    let cancel = Cancellation::new();
    let result = with_cancellation(&cancel, |_| {
        cancel.cancel();
        Ok(Transcript {
            text: "must not escape".into(),
        })
    });
    assert!(matches!(result, Err(Error::Cancelled)));
    let next = Cancellation::new();
    let result = with_cancellation(&next, |token| {
        assert!(!token.is_cancelled());
        Ok(Transcript {
            text: "complete".into(),
        })
    })
    .unwrap();
    assert_eq!(result.text, "complete");
}

#[test]
fn successful_final_text_is_preserved_verbatim() {
    let result = final_transcript(
        Ok(transcribe_cpp::Transcript {
            text: "  Héllo 世界!\n".into(),
            ..transcribe_cpp::Transcript::default()
        }),
        false,
        false,
    )
    .unwrap();
    assert_eq!(result.text, "  Héllo 世界!\n");
}

#[test]
fn native_partial_results_never_become_portable_transcripts() {
    let partial = || {
        Some(Box::new(transcribe_cpp::Transcript {
            text: "private partial text".into(),
            ..transcribe_cpp::Transcript::default()
        }))
    };
    let aborted = final_transcript(
        Err(transcribe_cpp::Error::Aborted {
            message: "private diagnostic".into(),
            partial: partial(),
        }),
        false,
        false,
    );
    assert!(matches!(aborted, Err(Error::Cancelled)));
    for native in [
        transcribe_cpp::Error::OutputTruncated {
            message: "private diagnostic".into(),
            partial: partial(),
        },
        transcribe_cpp::Error::OutputRepetition {
            message: "private diagnostic".into(),
            partial: partial(),
        },
    ] {
        let error = final_transcript(Err(native), false, false).unwrap_err();
        assert!(matches!(error, Error::Backend(_)));
        assert!(!format!("{error:?}").contains("private"));
    }
    for (aborted, truncated) in [(true, false), (false, true), (true, true)] {
        let result = final_transcript(
            Ok(transcribe_cpp::Transcript::default()),
            aborted,
            truncated,
        );
        assert!(result.is_err());
    }
}

#[test]
fn backend_diagnostics_are_redacted_for_every_mapped_error_category() {
    use transcribe_cpp::Error as Native;
    for error in [
        Native::ModelLoad("private".into()),
        Native::Backend("private".into()),
        Native::Unsupported("private".into()),
        Native::InvalidArgument("private".into()),
        Native::ModelFileNotFound("private".into()),
        Native::OutOfMemory("private".into()),
        Native::VersionMismatch("private".into()),
        Native::Busy("private".into()),
        Native::Other("private".into()),
    ] {
        assert!(!format!("{:?}", native_error(&error)).contains("private"));
    }
}

/// Explicit opt-in fixture test: no microphone, downloads, or cloud requests.
/// Supply an installed model and a mono 16-bit 16 kHz speech WAV plus its exact
/// expected text. The same dedicated recognizer handles both utterances.
#[test]
#[ignore = "requires VOX_TEST_MODEL, VOX_TEST_WAV and VOX_TEST_TEXT local fixtures"]
fn installed_model_transcribes_two_utterances_with_one_resident_session() {
    let config = Config {
        model_path: Some(
            std::env::var_os("VOX_TEST_MODEL")
                .expect("local model fixture required")
                .into(),
        ),
        ..Config::default()
    };
    let wave = std::env::var_os("VOX_TEST_WAV").expect("local WAV fixture required");
    let expected = std::env::var("VOX_TEST_TEXT").expect("expected fixture text required");
    let mut reader = hound::WavReader::open(wave).unwrap();
    let spec = reader.spec();
    assert_eq!(spec.channels, 1);
    assert_eq!(spec.sample_rate, SAMPLE_RATE);
    assert_eq!(spec.bits_per_sample, 16);
    assert_eq!(spec.sample_format, hound::SampleFormat::Int);
    let audio = recording(
        reader
            .samples::<i16>()
            .map(|sample| f32::from(sample.unwrap()) / 32_768.0)
            .collect(),
    );
    let mut recognizer = recognizer(&config).unwrap();
    let first = recognizer
        .recognize(&audio, &config.recognition, &Cancellation::new())
        .unwrap();
    let second = recognizer
        .recognize(&audio, &config.recognition, &Cancellation::new())
        .unwrap();
    // Do not include transcript contents in assertion diagnostics.
    assert!(
        first.text.trim() == expected.trim(),
        "first fixture transcript mismatch"
    );
    assert!(
        second.text.trim() == expected.trim(),
        "second fixture transcript mismatch"
    );
}
