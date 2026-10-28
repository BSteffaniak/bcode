#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
//! Real executable regressions: stdin is null and both output streams are redirected.

use std::{
    io::{Read as _, Seek as _},
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

#[cfg(unix)]
#[test]
fn acquisition_failure_exports_safe_report_from_actual_binary() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::tempdir().unwrap();
    let tool = root.path().join("yt-dlp");
    std::fs::write(&tool, "#!/bin/sh\ncase \"$*\" in\n*--version*) echo 2026.06.09;;\n*--dump-single-json*) echo '{\"id\":\"fJwhaC_Fn-8\",\"duration\":827}';;\n*) echo 'ERROR: unable to download video data: HTTP Error 403: Forbidden PRIVATE_TOKEN https://signed.invalid/?token=SECRET' >&2; exit 1;;\nesac\n").unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o700)).unwrap();
    let report = root.path().join("diagnostic.json");
    let output = run_at(
        &[
            "--diagnostic-report",
            report.to_str().unwrap(),
            "doctor",
            "--source",
            "https://youtu.be/fJwhaC_Fn-8",
            "--yt-dlp",
            tool.to_str().unwrap(),
            "--ffmpeg",
            "/usr/bin/true",
            "--ffprobe",
            "/usr/bin/true",
        ],
        root.path(),
    );
    assert_eq!(output.status.code(), Some(1));
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("HTTP 403 Forbidden"), "{error}");
    assert!(error.contains("No model request was made"));
    let bytes = std::fs::read(&report).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["inference_started"], false);
    assert_eq!(value["committed"], false);
    assert!(
        value["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["event"]["http_status"] == 403)
    );
    for text in [error.as_ref(), std::str::from_utf8(&bytes).unwrap()] {
        assert!(!text.contains("PRIVATE_TOKEN"));
        assert!(!text.contains("signed.invalid"));
        assert!(!text.contains("SECRET"));
    }
    let prior = bytes;
    let _ = run_at(
        &[
            "--diagnostic-report",
            report.to_str().unwrap(),
            "export",
            "missing",
        ],
        root.path(),
    );
    assert_eq!(
        prior,
        std::fs::read(&report).unwrap(),
        "reports must never clobber existing evidence"
    );
}

fn run(arguments: &[&str]) -> Output {
    let root = tempfile::tempdir().unwrap();
    run_at(arguments, root.path())
}

fn run_at(arguments: &[&str], root: &std::path::Path) -> Output {
    let stdout = tempfile::tempfile().unwrap();
    let stderr = tempfile::tempfile().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_recap"))
        .args(arguments)
        .current_dir(root)
        .env_clear()
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join(".config"))
        .env("XDG_STATE_HOME", root.join(".state"))
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout.try_clone().unwrap()))
        .stderr(Stdio::from(stderr.try_clone().unwrap()))
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("redirected command hung: {arguments:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut stdout = stdout;
    let mut stderr = stderr;
    stdout.rewind().unwrap();
    stderr.rewind().unwrap();
    let mut out = Vec::new();
    let mut err = Vec::new();
    stdout.read_to_end(&mut out).unwrap();
    stderr.read_to_end(&mut err).unwrap();
    let output = Output {
        status,
        stdout: out,
        stderr: err,
    };
    assert!(
        !output.stdout.contains(&0x1b),
        "stdout contains terminal controls"
    );
    assert!(
        !output.stderr.contains(&0x1b),
        "stderr contains terminal controls"
    );
    output
}

#[test]
fn bare_redirected_invocation_is_successful_help() {
    let output = run(&[]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("Usage:"));
    for command in ["create", "refine", "export", "inspect", "doctor"] {
        assert!(text.contains(command), "help missing {command}");
    }
}

#[cfg(feature = "tui")]
#[test]
fn explicit_tui_requires_terminals_with_headless_remedy() {
    let output = run(&["tui"]);
    assert_eq!(output.status.code(), Some(1));
    let text = String::from_utf8(output.stderr).unwrap().to_lowercase();
    assert!(text.contains("terminal"));
    assert!(text.contains("headless"));
    assert!(text.contains("create"));
}

#[test]
fn headless_subcommands_keep_help_and_parser_exit_semantics() {
    for command in ["create", "refine", "export", "inspect", "doctor"] {
        let output = run(&[command, "--help"]);
        assert!(output.status.success(), "{command} --help failed");
        assert!(String::from_utf8(output.stdout).unwrap().contains("Usage:"));
    }
    for arguments in [
        vec!["--unknown-option"],
        vec!["inspect", "../outside"],
        vec!["--max-document-bytes", "0", "inspect", "missing"],
        vec!["create"],
        vec!["refine"],
    ] {
        assert_eq!(run(&arguments).status.code(), Some(2), "{arguments:?}");
    }
}

#[test]
fn headless_operational_failures_remain_nonzero_without_entering_tui() {
    for arguments in [
        vec!["inspect", "missing"],
        vec!["export", "missing"],
        vec![
            "create",
            "--source",
            "absent.mp4",
            "--model-config",
            "absent.toml",
        ],
        vec![
            "refine",
            "missing",
            "--focus",
            "Inspect source",
            "--model-config",
            "absent.toml",
        ],
        vec!["doctor", "--ffmpeg", "/recap-acceptance-unavailable/ffmpeg"],
    ] {
        let output = run(&arguments);
        assert_eq!(output.status.code(), Some(1), "{arguments:?}");
        assert!(!output.stderr.is_empty(), "failure must be actionable");
    }
}

#[test]
fn missing_optional_model_config_is_an_operational_error_not_parser_rejection() {
    for arguments in [
        vec!["create", "--source", "absent.mp4"],
        vec!["refine", "missing", "--focus", "Inspect source"],
    ] {
        let root = tempfile::tempdir().unwrap();
        let output = run_at(&arguments, root.path());
        assert_eq!(output.status.code(), Some(1), "{arguments:?}");
        assert!(!output.stderr.is_empty(), "failure must be actionable");
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("required arguments"),
            "model configuration is optional at the parser boundary"
        );
        assert!(!root.path().join(".recap").exists());
    }
}

#[test]
fn malformed_explicit_override_is_secret_safe_and_never_creates_a_store() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("malformed.toml");
    // Deliberately invalid, with a sentinel that must never appear in diagnostics.
    let sentinel = "acceptance-secret-must-not-be-printed";
    let contents = format!("api_key = '{sentinel}'\n[model\n");
    std::fs::write(&config, &contents).unwrap();
    for arguments in [
        vec![
            "create",
            "--source",
            "absent.mp4",
            "--model-config",
            config.to_str().unwrap(),
        ],
        vec![
            "refine",
            "missing",
            "--focus",
            "Inspect source",
            "--model-config",
            config.to_str().unwrap(),
        ],
    ] {
        let output = run_at(&arguments, root.path());
        assert_eq!(output.status.code(), Some(1), "{arguments:?}");
        assert!(!output.stderr.is_empty());
        for stream in [&output.stdout, &output.stderr] {
            assert!(!String::from_utf8_lossy(stream).contains(sentinel));
        }
        assert!(!root.path().join(".recap").exists());
        assert_eq!(std::fs::read_to_string(&config).unwrap(), contents);
    }
}

#[cfg(not(feature = "tui"))]
#[test]
fn headless_build_excludes_interactive_command() {
    let help = run(&["--help"]);
    assert!(!String::from_utf8_lossy(&help.stdout).contains("  tui"));
    assert_eq!(run(&["tui"]).status.code(), Some(2));
}
