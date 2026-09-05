#![cfg(feature = "app")]
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use std::io::Read as _;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn capture_output(mut pipe: impl std::io::Read) -> std::io::Result<(Vec<u8>, bool)> {
    let mut retained = Vec::new();
    let mut truncated = false;
    let mut buffer = [0; 4096];
    loop {
        let count = pipe.read(&mut buffer)?;
        if count == 0 {
            return Ok((retained, truncated));
        }
        let keep = count.min(65_536_usize.saturating_sub(retained.len()));
        truncated |= keep != count;
        retained.extend_from_slice(&buffer[..keep]);
    }
}

fn run_cli(arguments: &[&str]) -> Output {
    run_cli_with_state(arguments, false)
}

fn run_cli_with_state(arguments: &[&str], blocked_state: bool) -> Output {
    run_cli_with_output(arguments, blocked_state, Stdio::piped())
}

fn run_cli_with_output(arguments: &[&str], blocked_state: bool, stdout: Stdio) -> Output {
    let root = tempfile::tempdir().expect("isolated CLI directory");
    if blocked_state {
        std::fs::write(root.path().join("bcode-state"), b"not a directory")
            .expect("blocked state fixture");
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_bcode"))
        .args(arguments)
        .env_clear()
        .env("HOME", root.path())
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("XDG_STATE_HOME", root.path().join("state"))
        .env("BCODE_STATE_DIR", root.path().join("bcode-state"))
        .current_dir(root.path())
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(Stdio::piped())
        .spawn()
        .expect("CLI subprocess");
    // Drain concurrently so output cannot fill a pipe and stall process completion.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take().expect("stderr pipe");
    let drain = |pipe: Box<dyn std::io::Read + Send>| {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let _ = sender.send(capture_output(pipe));
        });
        receiver
    };
    let stdout = stdout.map(|pipe| drain(Box::new(pipe)));
    let stderr = drain(Box::new(stderr));
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll CLI") {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().expect("kill timed-out CLI");
            child.wait().expect("reap timed-out CLI");
            panic!("CLI exceeded 30 seconds: {arguments:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let collect = |receiver: std::sync::mpsc::Receiver<std::io::Result<(Vec<u8>, bool)>>| {
        let (bytes, truncated) = receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("CLI output pipe did not close within deadline")
            .expect("read CLI output");
        assert!(!truncated, "CLI output exceeded 64 KiB capture limit");
        bytes
    };
    Output {
        status,
        stdout: stdout.map_or_else(Vec::new, collect),
        stderr: collect(stderr),
    }
}

#[test]
fn output_capture_reports_truncation_and_drains_remaining_bytes() {
    for size in [0, 65_536, 65_537, 100_000] {
        let mut input = std::io::repeat(b'x').take(size);
        let (bytes, truncated) = capture_output(&mut input).expect("capture output");
        assert_eq!(bytes.len() as u64, size.min(65_536));
        assert_eq!(truncated, size > 65_536);
        assert_eq!(input.limit(), 0);
    }
}

#[test]
fn complete_backfill_rejects_ignored_cursor_before_startup() {
    for command in ["search-backfill", "search-backfill-start"] {
        assert_usage_error(
            &[
                "session",
                command,
                "--cursor",
                "1:00000000-0000-0000-0000-000000000001",
                "--json",
            ],
            "--cursor is unsupported for complete backfill",
        );
    }
}

#[test]
fn complete_backfill_help_describes_cursor_and_slice_limits() {
    for command in ["search-backfill", "search-backfill-start"] {
        let output = run_cli(&["session", command, "--help"]);
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        let text = String::from_utf8(output.stdout).expect("help UTF-8");
        assert!(text.contains("Unsupported legacy option"), "{text}");
        assert!(text.contains("not a total operation timeout"), "{text}");
    }
}

fn assert_usage_error(arguments: &[&str], diagnostic: &str) {
    let output = run_cli(arguments);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(output.stdout.is_empty(), "unexpected machine output");
    assert!(stderr.contains(diagnostic), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
}

#[test]
fn conflicting_history_cursors_fail_before_daemon_access() {
    for command in ["history", "inspect"] {
        let mut arguments = vec!["session", command, "00000000-0000-0000-0000-000000000001"];
        if command == "inspect" {
            arguments.push("terminal-outcomes");
        }
        arguments.extend(["--after", "1", "--before", "2", "--json"]);
        assert_usage_error(
            &arguments,
            "session history accepts only one of --after or --before",
        );
    }
}

#[test]
fn unusable_state_returns_runtime_error_without_machine_output() {
    let output = run_cli_with_state(&["session", "list", "--json"], true);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(output.stdout.is_empty(), "unexpected machine output");
    assert!(stderr.starts_with("error:"), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
}

#[test]
fn model_queries_report_daemon_access_failure_without_json() {
    for command in ["list", "status"] {
        let output = run_cli_with_state(&["model", command, "--json"], true);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{command}: {stderr}");
        assert!(output.stdout.is_empty(), "unexpected machine output");
        assert!(stderr.starts_with("error:"), "{stderr}");
        assert!(!stderr.contains("panicked"), "{stderr}");
    }
}

#[test]
fn server_queries_report_daemon_access_failure_without_json() {
    for arguments in [
        vec!["server", "metrics", "--json"],
        vec!["server", "metrics", "--report"],
        vec!["server", "diagnose", "--json"],
    ] {
        let output = run_cli_with_state(&arguments, true);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{arguments:?}: {stderr}");
        assert!(output.stdout.is_empty(), "unexpected machine output");
        assert!(stderr.starts_with("error:"), "{stderr}");
        assert!(!stderr.contains("panicked"), "{stderr}");
    }
}

#[test]
fn import_queries_accept_json_and_fail_without_machine_output() {
    for command in ["sources", "discover"] {
        let output = run_cli_with_state(&["session", "import", command, "--json"], true);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{command}: {stderr}");
        assert!(output.stdout.is_empty(), "unexpected machine output");
        assert!(stderr.starts_with("error:"), "{stderr}");
        assert!(!stderr.contains("panicked"), "{stderr}");
    }
}

#[test]
fn state_locations_closed_pipe_returns_io_error_without_panic() {
    let (reader, writer) = std::io::pipe().expect("output pipe");
    drop(reader);
    let output = run_cli_with_output(
        &["state", "locations", "--json"],
        false,
        Stdio::from(writer),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.starts_with("error: I/O error:"), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
}

#[test]
fn state_locations_emits_one_json_document_without_diagnostics() {
    let output = run_cli(&["state", "locations", "--json"]);
    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(output.stderr.is_empty(), "unexpected diagnostics");
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("one complete JSON document");
    let locations = value["locations"].as_array().expect("location array");
    assert_eq!(locations.len(), 1);
    let location = &locations[0];
    assert_eq!(location["primary"], true);
    assert_eq!(location["available"], false);
    let root = std::path::Path::new(location["root"].as_str().expect("root path"));
    assert!(root.is_absolute());
    assert!(root.ends_with("bcode-state"));
    assert_eq!(
        std::path::Path::new(location["sessions_root"].as_str().expect("sessions path")),
        root.join("sessions")
    );
}

#[test]
fn missing_interaction_payload_is_io_failure_not_interrupt() {
    let output = run_cli(&[
        "interaction",
        "respond",
        "test-exchange",
        "--payload",
        "missing-payload.json",
        "--json",
    ]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(output.stdout.is_empty(), "unexpected machine output");
    assert!(stderr.starts_with("error: I/O error:"), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
}

#[test]
fn empty_interaction_stdin_is_json_usage_failure() {
    assert_usage_error(
        &[
            "interaction",
            "respond",
            "test-exchange",
            "--payload",
            "-",
            "--json",
        ],
        "EOF while parsing a value",
    );
}

#[test]
fn invalid_session_id_is_a_process_usage_error() {
    assert_usage_error(
        &["session", "delete", "not-a-session-id", "--yes", "--json"],
        "invalid value",
    );
}

#[test]
fn deletion_without_confirmation_fails_before_daemon_access() {
    assert_usage_error(
        &[
            "session",
            "delete",
            "00000000-0000-0000-0000-000000000001",
            "--json",
        ],
        "session deletion requires --yes",
    );
}

#[test]
fn unknown_option_is_a_process_usage_error() {
    assert_usage_error(
        &["session", "list", "--json", "--not-a-bcode-option"],
        "unexpected argument",
    );
}
