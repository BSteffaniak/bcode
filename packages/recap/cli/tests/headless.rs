#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
//! Real executable regressions: stdin is null and both output streams are redirected.

use std::{
    io::{Read as _, Seek as _},
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

fn run(arguments: &[&str]) -> Output {
    let root = tempfile::tempdir().unwrap();
    let stdout = tempfile::tempfile().unwrap();
    let stderr = tempfile::tempfile().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_recap"))
        .args(arguments)
        .current_dir(root.path())
        .env_clear()
        .env("HOME", root.path())
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

#[cfg(not(feature = "tui"))]
#[test]
fn headless_build_excludes_interactive_command() {
    let help = run(&["--help"]);
    assert!(!String::from_utf8_lossy(&help.stdout).contains("  tui"));
    assert_eq!(run(&["tui"]).status.code(), Some(2));
}
