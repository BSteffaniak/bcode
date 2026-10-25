use super::*;
use crate::contracts::{RecognitionOptions, Recognizer};

#[test]
fn parses_actions_json_and_watch_in_either_global_flag_position() {
    for verb in ["start", "stop", "toggle", "cancel", "serve"] {
        assert!(Cli::try_parse_from(["vox", verb, "--json"]).is_ok());
        assert!(Cli::try_parse_from(["vox", "--json", verb]).is_ok());
    }
    let cli = Cli::try_parse_from(["vox", "status", "--watch", "--json"]).unwrap();
    assert!(cli.json);
    assert!(matches!(cli.command, Command::Status { watch: true }));
    assert!(Cli::try_parse_from(["vox", "start", "--watch"]).is_err());
}

#[test]
fn requires_explicit_commands_and_rejects_unknown_or_incomplete_arguments() {
    for args in [
        vec!["vox"],
        vec!["vox", "model", "install"],
        vec!["vox", "transcribe"],
        vec!["vox", "transcribe", "a.wav", "--backend", "automatic"],
        vec!["vox", "unknown"],
    ] {
        assert!(Cli::try_parse_from(args).is_err());
    }
    for args in [
        vec!["vox", "model", "list"],
        vec!["vox", "model", "install", "tiny.en"],
        vec!["vox", "model", "verify", "tiny.en"],
        vec!["vox", "model", "remove", "tiny.en"],
        vec!["vox", "doctor"],
        vec!["vox", "config", "example"],
        vec!["vox", "shortcuts", "hyprland"],
        vec!["vox", "shortcuts", "sway"],
    ] {
        assert!(Cli::try_parse_from(args).is_ok());
    }
}

#[test]
fn wav_overrides_are_explicit_and_always_select_stdout_delivery() {
    let cli = Cli::try_parse_from([
        "vox",
        "--config",
        "my.toml",
        "transcribe",
        "voice.wav",
        "--backend",
        "local",
        "--model",
        "local.gguf",
        "--language",
        "en",
        "--vocabulary",
        "Vox",
        "--vocabulary",
        "BMUX",
    ])
    .unwrap();
    assert_eq!(cli.config.as_deref(), Some(Path::new("my.toml")));
    let Command::Transcribe(args) = cli.command else {
        panic!("transcribe command")
    };
    let mut config = Config {
        backend: "cloud".into(),
        ..Config::default()
    };
    apply_transcribe_options(&mut config, &args).unwrap();
    assert_eq!(config.backend, "local");
    assert_eq!(config.model_path.as_deref(), Some(Path::new("local.gguf")));
    assert_eq!(config.recognition.language.as_deref(), Some("en"));
    assert_eq!(config.recognition.vocabulary, ["Vox", "BMUX"]);
    assert_eq!(config.delivery, crate::contracts::DeliveryMode::Stdout);
}

#[test]
fn cloud_does_not_accept_a_local_model_path_or_silently_select_local() {
    let cli =
        Cli::try_parse_from(["vox", "transcribe", "voice.wav", "--model", "local.gguf"]).unwrap();
    let Command::Transcribe(args) = cli.command else {
        panic!("transcribe command")
    };
    let mut config = Config {
        backend: "cloud".into(),
        ..Config::default()
    };
    assert!(apply_transcribe_options(&mut config, &args).is_err());
    assert_eq!(config.backend, "cloud");
}

#[test]
fn default_config_absence_is_allowed_but_explicit_absence_and_corruption_are_not() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.toml");
    assert!(load_config_at(None, Some(&path)).is_ok());
    assert!(load_config_at(Some(&path), None).is_err());
    std::fs::write(&path, "not TOML = [").unwrap();
    assert!(load_config_at(None, Some(&path)).is_err());
    std::fs::write(&path, "backend = 'cloud'\n").unwrap();
    assert_eq!(load_config_at(None, Some(&path)).unwrap().backend, "cloud");
}

#[test]
fn xdg_config_uses_only_absolute_nonempty_roots() {
    assert_eq!(
        default_config_path_from(Some("/xdg".into()), Some("/home/person".into())),
        Some(PathBuf::from("/xdg/vox/config.toml"))
    );
    assert_eq!(
        default_config_path_from(Some("relative".into()), Some("/home/person".into())),
        Some(PathBuf::from("/home/person/.config/vox/config.toml"))
    );
    assert_eq!(default_config_path_from(Some("".into()), None), None);
}

#[test]
fn generated_example_round_trips_the_frozen_config_schema() {
    let example = config_example().unwrap();
    let config: Config = toml::from_str(&example).unwrap();
    config.validate().unwrap();
    assert_eq!(config.backend, "local");
    assert_eq!(config.delivery, crate::contracts::DeliveryMode::Typing);
    assert!(config.cloud_url.is_none());
    assert!(config.model_path.is_none());
}

struct Gate(bool);
impl SpeechGate for Gate {
    fn has_speech(&mut self, _: &Recording) -> Result<bool> {
        Ok(self.0)
    }
}

struct Processor;
impl TextProcessor for Processor {
    fn process(&self, text: &str) -> Result<String> {
        Ok(format!("final:{text}"))
    }
}

struct FakeRecognizer {
    cancel_after_recognition: bool,
}
impl Recognizer for FakeRecognizer {
    fn recognize(
        &mut self,
        _: &Recording,
        _: &RecognitionOptions,
        cancel: &Cancellation,
    ) -> Result<Transcript> {
        if self.cancel_after_recognition {
            cancel.cancel();
        }
        Ok(Transcript {
            text: "hello".into(),
        })
    }
}

fn recording() -> Recording {
    Recording {
        samples: vec![0.5; 160],
        sample_rate: 16_000,
    }
}

#[test]
fn silence_skips_recognizer_construction_and_text_processing() {
    let transcript = transcribe_recording(
        &recording(),
        &Config::default(),
        &mut Gate(false),
        &Processor,
        &Cancellation::new(),
        || panic!("silence must not construct a recognizer"),
    )
    .unwrap();
    assert!(transcript.text.is_empty());
}

#[test]
fn final_transcript_passes_through_processor_without_desktop_delivery() {
    let transcript = transcribe_recording(
        &recording(),
        &Config::default(),
        &mut Gate(true),
        &Processor,
        &Cancellation::new(),
        || {
            Ok(Box::new(FakeRecognizer {
                cancel_after_recognition: false,
            }))
        },
    )
    .unwrap();
    assert_eq!(transcript.text, "final:hello");
}

#[test]
fn cancelled_or_oversize_input_never_constructs_recognizer() {
    let cancel = Cancellation::new();
    cancel.cancel();
    assert!(matches!(
        transcribe_recording(
            &recording(),
            &Config::default(),
            &mut Gate(true),
            &Processor,
            &cancel,
            || panic!("cancelled")
        ),
        Err(Error::Cancelled)
    ));
    let input = Recording {
        samples: vec![0.5; 121],
        sample_rate: 1,
    };
    assert!(
        transcribe_recording(
            &input,
            &Config::default(),
            &mut Gate(true),
            &Processor,
            &Cancellation::new(),
            || panic!("oversize")
        )
        .is_err()
    );
}

#[test]
fn cancellation_after_recognition_discards_final_text() {
    assert!(matches!(
        transcribe_recording(
            &recording(),
            &Config::default(),
            &mut Gate(true),
            &Processor,
            &Cancellation::new(),
            || Ok(Box::new(FakeRecognizer {
                cancel_after_recognition: true
            }))
        ),
        Err(Error::Cancelled)
    ));
}

#[test]
fn recognition_failure_is_returned_without_retry_or_fallback() {
    let mut calls = 0;
    let result = transcribe_recording(
        &recording(),
        &Config::default(),
        &mut Gate(true),
        &Processor,
        &Cancellation::new(),
        || {
            calls += 1;
            Err(Error::Backend("not available".into()))
        },
    );
    assert!(matches!(result, Err(Error::Backend(_))));
    assert_eq!(calls, 1);
}

#[test]
fn public_diagnostics_do_not_echo_untrusted_error_context() {
    for error in [
        Error::Invalid("private transcript or key".into()),
        Error::Backend("private transcript or key".into()),
        Error::Unsupported("private transcript or key".into()),
        Error::Io(io::Error::other("private transcript or key")),
    ] {
        assert!(!diagnostic(&error).contains("private transcript or key"));
    }
}

#[test]
fn doctor_distinguishes_metadata_from_unperformed_live_checks() {
    let temp = tempfile::tempdir().unwrap();
    let model = temp.path().join("model.gguf");
    let mut config = Config {
        model_path: Some(model.clone()),
        ..Config::default()
    };
    assert_eq!(doctor_report(&config).model, "missing");
    std::fs::write(model, b"not a valid model").unwrap();
    let report = doctor_report(&config);
    assert_eq!(
        report.model,
        "file exists; integrity and compatibility not verified"
    );
    assert!(report.live_checks.starts_with("not performed:"));
    assert!(
        report
            .global_shortcut_portal
            .starts_with("not implemented;")
    );
    config.model_path = None;
    assert_eq!(doctor_report(&config).model, "not configured");
}

#[test]
fn doctor_json_preserves_flat_compiled_capability_fields() {
    let report = doctor_report(&Config::default());
    let value = serde_json::to_value(&report).unwrap();
    assert_eq!(
        value["native_audio_compiled"],
        cfg!(feature = "native-audio")
    );
    assert_eq!(
        value["local_inference_compiled"],
        cfg!(feature = "local-inference")
    );
    assert_eq!(
        value["wayland_adapter_compiled"],
        cfg!(all(target_os = "linux", feature = "wayland"))
    );
    assert_eq!(
        value["x11_adapter_compiled"],
        cfg!(all(target_os = "linux", feature = "x11"))
    );
    assert!(value.get("audio").is_none());
    assert!(value.get("desktop").is_none());
}

#[test]
fn status_serialization_redacts_failures_but_preserves_operation_identity() {
    let status = safe_status(Status::Failed {
        message: "private transcript or key".into(),
    });
    let encoded = serde_json::to_string(&status).unwrap();
    assert!(!encoded.contains("private transcript or key"));
    assert!(matches!(status, Status::Failed { .. }));
    let recording = Status::Recording { operation: 42 };
    assert_eq!(safe_status(recording.clone()), recording);
}

#[test]
fn json_output_is_one_round_trippable_line_with_escaped_transcript() {
    let transcript = Transcript {
        text: "one\ntwo\"three".into(),
    };
    let mut output = Vec::new();
    write_json(&mut output, &transcript).unwrap();
    let encoded = std::str::from_utf8(&output).unwrap();
    assert!(encoded.ends_with('\n'));
    let mut lines = encoded.lines();
    assert!(lines.next().is_some());
    assert!(lines.next().is_none());
    let decoded: Transcript = serde_json::from_slice(&output).unwrap();
    assert_eq!(decoded.text, transcript.text);
}
