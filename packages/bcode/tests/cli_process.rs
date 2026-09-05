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
    run_cli_with_fixture(arguments, blocked_state, stdout, |_| {})
}

fn run_cli_with_fixture(
    arguments: &[&str],
    blocked_state: bool,
    stdout: Stdio,
    setup: impl FnOnce(&std::path::Path),
) -> Output {
    run_cli_with_stdio(arguments, blocked_state, stdout, Stdio::null(), setup)
}

fn run_cli_with_stdio(
    arguments: &[&str],
    blocked_state: bool,
    stdout: Stdio,
    stdin: Stdio,
    setup: impl FnOnce(&std::path::Path),
) -> Output {
    let root = tempfile::tempdir().expect("isolated CLI directory");
    setup(root.path());
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
        .stdin(stdin)
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
fn interaction_help_distinguishes_input_and_resolution_limits() {
    let output = run_cli(&["interaction", "respond", "--help"]);
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let text = String::from_utf8(output.stdout).expect("help UTF-8");
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(text.contains("Input is capped at 256 KiB"), "{text}");
    assert!(text.contains("encoded resolution to 64 KiB"), "{text}");
    assert!(text.contains("including envelope and escaping"), "{text}");
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
fn worktree_commands_report_daemon_failure_without_json() {
    for arguments in [
        vec!["worktree", "list", "--json"],
        vec!["worktree", "create", "test-worktree", "--json"],
        vec!["worktree", "remove", "test-worktree", "--yes", "--json"],
    ] {
        let output = run_cli_with_state(&arguments, true);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{arguments:?}: {stderr}");
        assert!(output.stdout.is_empty(), "unexpected result: {arguments:?}");
        assert!(stderr.starts_with("error:"), "{stderr}");
        assert!(!stderr.contains("panicked"), "{stderr}");
        assert!(!stderr.contains("requires --yes"), "{stderr}");
    }
}

#[test]
fn worktree_removal_requires_confirmation_before_daemon_access() {
    let fixture = tempfile::tempdir().expect("worktree fixture");
    let marker = fixture.path().join("keep.txt");
    std::fs::write(&marker, b"must remain").expect("marker");
    for json in [true, false] {
        let mut args = vec![
            "worktree",
            "remove",
            fixture.path().to_str().expect("fixture UTF-8"),
            "--force",
        ];
        if json {
            args.push("--json");
        }
        let output = run_cli_with_state(&args, true);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{stderr}");
        assert!(
            stderr.contains("worktree removal requires --yes"),
            "{stderr}"
        );
        assert!(output.stdout.is_empty());
        assert!(!stderr.contains("panicked"), "{stderr}");
        assert_eq!(
            std::fs::read(&marker).expect("marker retained"),
            b"must remain"
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

fn auth_security_fixture(root: &std::path::Path) {
    let auth = root.join("bcode-state/auth");
    std::fs::create_dir_all(&auth).expect("auth fixture directory");
    let registry = serde_json::json!({"profiles": {"test-profile": {
        "provider_id": "xai", "owner_plugin_id": "bcode.xai",
        "backend": "sshenv", "scheme": "api_key",
        "storage_profile": "test-profile", "vault": auth.join("missing-vault"),
        "device_seal": "required"
    }}});
    std::fs::write(
        auth.join("subscriptions.json"),
        serde_json::to_vec(&registry).unwrap(),
    )
    .expect("auth metadata fixture");
}

#[test]
fn auth_security_emits_compact_status_and_handles_closed_pipe() {
    let args = ["auth", "security", "--profile", "test-profile"];
    let output = run_cli_with_fixture(&args, false, Stdio::piped(), auth_security_fixture);
    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(output.stderr.is_empty());
    let text = String::from_utf8(output.stdout).expect("security UTF-8");
    assert_eq!(text.lines().count(), 1);
    let value: serde_json::Value = serde_json::from_str(&text).expect("security JSON");
    assert_eq!(value["profile"], "test-profile");
    assert_eq!(value["vault_exists"], false);
    assert!(!text.contains("api_key"));
    let (reader, writer) = std::io::pipe().expect("output pipe");
    drop(reader);
    let output = run_cli_with_fixture(&args, false, Stdio::from(writer), auth_security_fixture);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(stderr.starts_with("error: I/O error:"), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
}

#[test]
fn auth_security_rejects_provider_and_backend_mismatches_without_json() {
    for (option, value, diagnostic) in [
        (
            "--provider",
            "other-provider",
            "does not belong to provider",
        ),
        (
            "--require-backend",
            "required-test-backend",
            "required 'required-test-backend'",
        ),
    ] {
        let output = run_cli_with_fixture(
            &[
                "auth",
                "security",
                "--profile",
                "test-profile",
                option,
                value,
            ],
            false,
            Stdio::piped(),
            auth_security_fixture,
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{stderr}");
        assert!(output.stdout.is_empty(), "unexpected security result");
        assert!(stderr.contains(diagnostic), "{stderr}");
        assert!(!stderr.contains("panicked"), "{stderr}");
    }
}

#[test]
fn auth_security_missing_profile_fails_without_machine_output() {
    let output = run_cli(&["auth", "security", "--profile", "missing-test-profile"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(output.stdout.is_empty(), "unexpected security result");
    assert!(
        stderr.contains("not declared or registered in runtime state"),
        "{stderr}"
    );
    assert!(!stderr.contains("panicked"), "{stderr}");
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
fn invalid_interaction_files_fail_before_daemon_access() {
    for (contents, diagnostic) in [
        (b"{invalid".to_vec(), "key must be a string"),
        (b"{} {}".to_vec(), "trailing characters"),
        (vec![b'\"', 0xff, b'\"'], "invalid unicode code point"),
        (
            vec![b' '; 256 * 1024 + 1],
            "interaction JSON exceeds 262144 bytes",
        ),
    ] {
        let output = run_cli_with_fixture(
            &[
                "interaction",
                "respond",
                "test-exchange",
                "--payload",
                "payload.json",
                "--json",
            ],
            true,
            Stdio::piped(),
            |root| std::fs::write(root.join("payload.json"), contents).expect("payload fixture"),
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{stderr}");
        assert!(output.stdout.is_empty(), "unexpected machine output");
        assert!(stderr.contains(diagnostic), "{stderr}");
        assert!(!stderr.contains("panicked"), "{stderr}");
    }
}

#[test]
fn exact_limit_interaction_json_reaches_daemon_access() {
    let mut contents = b"{}".to_vec();
    contents.resize(256 * 1024, b' ');
    let output = run_cli_with_fixture(
        &[
            "interaction",
            "respond",
            "test-exchange",
            "--payload",
            "payload.json",
            "--json",
        ],
        true,
        Stdio::piped(),
        |root| std::fs::write(root.join("payload.json"), contents).expect("payload fixture"),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(output.stdout.is_empty());
    assert!(stderr.starts_with("error:"), "{stderr}");
    assert!(!stderr.contains("interaction JSON exceeds"), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
}

#[test]
fn interaction_stdin_enforces_the_same_byte_limit_as_files() {
    use std::io::{Seek as _, Write as _};
    for (size, code) in [(256 * 1024, 1), (256 * 1024 + 1, 2)] {
        let mut contents = b"{}".to_vec();
        contents.resize(size, b' ');
        let mut input = tempfile::tempfile().expect("stdin fixture");
        input.write_all(&contents).expect("write stdin");
        input.rewind().expect("rewind stdin");
        let output = run_cli_with_stdio(
            &[
                "interaction",
                "respond",
                "test-exchange",
                "--payload",
                "-",
                "--json",
            ],
            true,
            Stdio::piped(),
            Stdio::from(input),
            |_| {},
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(code), "{stderr}");
        assert!(output.stdout.is_empty());
        assert_eq!(
            stderr.contains("interaction JSON exceeds 262144 bytes"),
            code == 2,
            "{stderr}"
        );
        assert!(!stderr.contains("panicked"), "{stderr}");
    }
}

#[test]
fn malformed_interaction_stdin_fails_before_daemon_access_without_payload_disclosure() {
    use std::io::{Seek as _, Write as _};
    let sentinel = "private-interaction-value";
    for (contents, diagnostic) in [
        (
            format!("{{\"secret\":\"{sentinel}\"}} {{}}").into_bytes(),
            "trailing characters",
        ),
        (
            format!("{{\"secret\":\"{sentinel}\",invalid}}").into_bytes(),
            "key must be a string",
        ),
        (
            [
                format!("{{\"secret\":\"{sentinel}").into_bytes(),
                vec![0xff, b'\"', b'}'],
            ]
            .concat(),
            "invalid unicode code point",
        ),
    ] {
        let mut input = tempfile::tempfile().expect("stdin fixture");
        input.write_all(&contents).expect("write stdin");
        input.rewind().expect("rewind stdin");
        let output = run_cli_with_stdio(
            &[
                "interaction",
                "respond",
                "test-exchange",
                "--payload",
                "-",
                "--json",
            ],
            true,
            Stdio::piped(),
            Stdio::from(input),
            |_| {},
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{stderr}");
        assert!(output.stdout.is_empty(), "unexpected machine output");
        assert!(stderr.contains(diagnostic), "{stderr}");
        assert!(!stderr.contains(sentinel), "payload leaked: {stderr}");
        assert!(!stderr.contains("panicked"), "{stderr}");
    }
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
fn empty_plugin_commands_handle_closed_stdout_without_panicking() {
    for (command, json) in ["list", "services", "check"]
        .into_iter()
        .flat_map(|command| [false, true].map(|json| (command, json)))
    {
        let mut arguments = vec!["plugin", command, "--root", "empty-plugins"];
        if json {
            arguments.push("--json");
        }
        let setup = |root: &std::path::Path| {
            std::fs::create_dir(root.join("empty-plugins")).expect("empty plugin root");
        };
        let output = run_cli_with_fixture(&arguments, false, Stdio::piped(), setup);
        assert!(output.status.success(), "{:?}", output.stderr);
        assert!(output.stderr.is_empty());
        assert_eq!(
            output.stdout,
            if json {
                "[]\n"
            } else if command == "services" {
                "no plugin services discovered\n"
            } else {
                "no plugins discovered\n"
            }
            .as_bytes()
        );
        let (reader, writer) = std::io::pipe().expect("output pipe");
        drop(reader);
        let output = run_cli_with_fixture(&arguments, false, Stdio::from(writer), setup);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{stderr}");
        assert!(stderr.starts_with("error: I/O error:"), "{stderr}");
        assert!(!stderr.contains("panicked"), "{stderr}");
    }
}

fn plugin_discovery_fixture(root: &std::path::Path) {
    let plugins = root.join("test-plugins");
    std::fs::write(
        root.join("bcode.toml"),
        "[plugins]\nenabled = [\"test.discovery\"]\n",
    )
    .unwrap();
    std::fs::create_dir(&plugins).unwrap();
    std::fs::write(
        plugins.join("bcode-plugin.toml"),
        r#"
id = "test.discovery"
name = "Discovery Fixture"
version = "0.0.1"
[[services]]
class = "service"
interface_id = "test.named/v1"
name = "Named Service"
description = "Service metadata"
[[services]]
class = "service"
interface_id = "test.unnamed/v1"
[runtime]
type = "native"
abi_version = 3
library = "deliberately-absent.dylib"
"#,
    )
    .unwrap();
}

#[test]
fn plugin_list_exposes_manifest_identity_without_loading_native_code() {
    for json in [false, true] {
        let mut args = vec!["plugin", "list", "--root", "test-plugins"];
        if json {
            args.push("--json");
        }
        let output = run_cli_with_fixture(&args, false, Stdio::piped(), plugin_discovery_fixture);
        assert!(output.status.success(), "{:?}", output.stderr);
        assert!(output.stderr.is_empty());
        if json {
            let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(
                value,
                serde_json::json!([{
                    "plugin_id": "test.discovery", "version": "0.0.1",
                    "name": "Discovery Fixture", "manifest_path": "test-plugins/bcode-plugin.toml"
                }])
            );
        } else {
            assert_eq!(
                output.stdout,
                b"test.discovery\t0.0.1\tDiscovery Fixture\ttest-plugins/bcode-plugin.toml\n"
            );
        }
        let (reader, writer) = std::io::pipe().unwrap();
        drop(reader);
        let output =
            run_cli_with_fixture(&args, false, Stdio::from(writer), plugin_discovery_fixture);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{stderr}");
        assert!(stderr.starts_with("error: I/O error:"), "{stderr}");
        assert!(!stderr.contains("panicked"), "{stderr}");
    }
}

#[test]
fn plugin_services_exposes_manifest_metadata_without_loading_native_code() {
    let setup = plugin_discovery_fixture;
    for json in [false, true] {
        let mut args = vec!["plugin", "services", "--root", "test-plugins"];
        if json {
            args.push("--json");
        }
        let output = run_cli_with_fixture(&args, false, Stdio::piped(), setup);
        assert!(output.status.success(), "{:?}", output.stderr);
        assert!(output.stderr.is_empty());
        if json {
            let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(
                value,
                serde_json::json!([
                    {"plugin_id": "test.discovery", "interface_id": "test.named/v1", "name": "Named Service", "description": "Service metadata"},
                    {"plugin_id": "test.discovery", "interface_id": "test.unnamed/v1", "name": null, "description": null}
                ])
            );
        } else {
            assert_eq!(output.stdout, b"test.named/v1\ttest.discovery\tNamed Service\ntest.unnamed/v1\ttest.discovery\t<unnamed>\n");
        }
        let (reader, writer) = std::io::pipe().unwrap();
        drop(reader);
        let output = run_cli_with_fixture(&args, false, Stdio::from(writer), setup);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{stderr}");
        assert!(stderr.starts_with("error: I/O error:"), "{stderr}");
        assert!(!stderr.contains("panicked"), "{stderr}");
    }
}

#[test]
fn disabled_plugin_with_missing_library_does_not_break_local_operations() {
    for command in ["list", "services", "check", "publish"] {
        let mut args = vec!["plugin", command, "--root", "test-plugins", "--json"];
        if command == "publish" {
            args.push("test.topic");
        }
        let output = run_cli_with_fixture(&args, false, Stdio::piped(), |root| {
            plugin_discovery_fixture(root);
            std::fs::write(
                root.join("bcode.toml"),
                "[plugins]\nenabled = [\"test.discovery\"]\ndisabled = [\"test.discovery\"]\n",
            )
            .unwrap();
        });
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{args:?}: {stderr}");
        assert!(output.stderr.is_empty(), "{stderr}");
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            value,
            if command == "publish" {
                serde_json::json!({"delivered": 0})
            } else {
                serde_json::json!([])
            }
        );
    }
}

#[test]
fn disabled_plugin_cannot_be_invoked_by_id_or_interface() {
    for operation in [
        vec!["invoke", "test.discovery", "test.named/v1", "run"],
        vec!["call", "test.named/v1", "run"],
    ] {
        for json in [false, true] {
            let mut args = vec!["plugin"];
            args.extend(operation.iter().copied());
            args.extend(["--root", "test-plugins"]);
            if json {
                args.push("--json");
            }
            let output = run_cli_with_fixture(&args, false, Stdio::piped(), |root| {
                plugin_discovery_fixture(root);
                std::fs::write(
                    root.join("bcode.toml"),
                    "[plugins]\nenabled = [\"test.discovery\"]\ndisabled = [\"test.discovery\"]\n",
                )
                .unwrap();
            });
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(output.status.code(), Some(1), "{args:?}: {stderr}");
            assert!(
                output.stdout.is_empty(),
                "unexpected success output: {args:?}"
            );
            let expected = if operation[0] == "invoke" {
                "error: plugin error: plugin is not loaded: test.discovery\n"
            } else {
                "error: plugin error: no loaded plugin declares service interface 'test.named/v1'\n"
            };
            assert_eq!(stderr, expected, "{args:?}");
        }
    }
}

#[test]
fn plugin_execution_rejects_missing_native_library_without_success_output() {
    for operation in [
        vec!["check"],
        vec!["invoke", "test.discovery", "test.named/v1", "run"],
        vec!["call", "test.named/v1", "run"],
        vec!["publish", "test.topic"],
    ] {
        for json in [false, true] {
            let mut args = vec!["plugin"];
            args.extend(operation.iter().copied());
            args.extend(["--root", "test-plugins"]);
            if json {
                args.push("--json");
            }
            let output =
                run_cli_with_fixture(&args, false, Stdio::piped(), plugin_discovery_fixture);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(output.status.code(), Some(1), "{args:?}: {stderr}");
            assert!(
                output.stdout.is_empty(),
                "unexpected success output: {args:?}"
            );
            assert!(
                stderr.starts_with("error: plugin error: failed to load native library"),
                "{args:?}: {stderr}"
            );
            assert!(stderr.contains("deliberately-absent.dylib"), "{stderr}");
            assert!(!stderr.contains("panicked"), "{stderr}");
        }
    }
}

#[test]
fn plugin_check_rejects_future_abi_before_native_loading() {
    for json in [false, true] {
        let mut args = vec!["plugin", "check", "--root", "test-plugins"];
        if json {
            args.push("--json");
        }
        let output = run_cli_with_fixture(&args, false, Stdio::piped(), |root| {
            plugin_discovery_fixture(root);
            let path = root.join("test-plugins/bcode-plugin.toml");
            let manifest = std::fs::read_to_string(&path).unwrap();
            std::fs::write(
                path,
                manifest.replace("abi_version = 3", "abi_version = 65535"),
            )
            .unwrap();
        });
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{stderr}");
        assert!(output.stdout.is_empty());
        assert!(
            stderr.contains("uses unsupported ABI version 65535"),
            "{stderr}"
        );
        assert!(
            !stderr.contains("failed to load native library"),
            "{stderr}"
        );
        assert!(!stderr.contains("panicked"), "{stderr}");
    }
}

#[test]
#[ignore = "requires BCODE_METRICS_PLUGIN_TEST_LIBRARY pointing to a built metrics plugin"]
fn plugin_service_errors_return_failure_with_response_envelope() {
    let library = std::env::var_os("BCODE_METRICS_PLUGIN_TEST_LIBRARY")
        .expect("set BCODE_METRICS_PLUGIN_TEST_LIBRARY to the built metrics plugin");
    let library = std::fs::canonicalize(library).expect("built metrics plugin library");
    for invoke in [false, true] {
        for json in [false, true] {
            let mut args = vec!["plugin"];
            if invoke {
                args.extend(["invoke", "bcode.metrics"]);
            } else {
                args.push("call");
            }
            args.extend([
                "bcode.command/v1",
                "unsupported-test-operation",
                "--root",
                "test-plugins",
            ]);
            if json {
                args.push("--json");
            }
            let output = run_cli_with_fixture(&args, false, Stdio::piped(), |root| {
                let plugin_root = root.join("test-plugins");
                std::fs::create_dir(&plugin_root).unwrap();
                std::fs::write(
                    root.join("bcode.toml"),
                    "[plugins]\nenabled = [\"bcode.metrics\"]\n",
                )
                .unwrap();
                let manifest = include_str!("../../../plugins/metrics-plugin/bcode-plugin.toml");
                std::fs::write(plugin_root.join("bcode-plugin.toml"), manifest).unwrap();
                std::fs::copy(&library, plugin_root.join("libbcode_metrics_plugin.dylib")).unwrap();
            });
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(output.status.code(), Some(1), "{stderr}");
            assert!(stderr.contains("unsupported_operation"), "{stderr}");
            assert!(!stderr.contains("panicked"), "{stderr}");
            if json {
                let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(value["error"]["code"], "unsupported_operation");
                assert_eq!(value["error"]["message"], "unsupported command operation");
                assert_eq!(value["payload"], serde_json::json!([]));
            } else {
                assert_eq!(
                    output.stdout,
                    b"ERROR\tunsupported_operation\tunsupported command operation\n"
                );
            }
        }
    }
}

#[test]
fn plugin_check_help_discloses_native_code_execution() {
    let output = run_cli(&["plugin", "check", "--help"]);
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let text = String::from_utf8(output.stdout).unwrap();
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(text.contains("activation/deactivation callbacks"), "{text}");
    assert!(text.contains("This executes plugin code"), "{text}");
    assert!(text.contains("manifest-only discovery"), "{text}");
}

#[test]
fn plugin_daemon_operations_reject_ignored_local_roots() {
    for operation in [
        vec!["services"],
        vec!["invoke", "test-plugin", "test/v1", "run"],
        vec!["call", "test/v1", "run"],
        vec!["publish", "test.topic"],
    ] {
        let mut arguments = vec!["plugin"];
        arguments.extend(operation);
        arguments.extend(["--root", "unused-plugins", "--daemon", "--json"]);
        let output = run_cli_with_state(&arguments, true);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{stderr}");
        assert!(output.stdout.is_empty());
        assert!(stderr.contains("cannot be used with"), "{stderr}");
        assert!(!stderr.contains("panicked"), "{stderr}");
    }
}

#[test]
fn plugin_publish_receipt_handles_closed_stdout_without_panicking() {
    for json in [false, true] {
        let mut arguments = vec!["plugin", "publish", "--root", "empty-plugins", "test.topic"];
        if json {
            arguments.push("--json");
        }
        let setup = |root: &std::path::Path| {
            std::fs::create_dir(root.join("empty-plugins")).expect("empty plugin root");
        };
        let output = run_cli_with_fixture(&arguments, false, Stdio::piped(), setup);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        assert!(output.stderr.is_empty(), "{stderr}");
        let expected = if json {
            "{\n  \"delivered\": 0\n}\n"
        } else {
            "delivered\t0\n"
        };
        assert_eq!(output.stdout, expected.as_bytes());

        let (reader, writer) = std::io::pipe().expect("output pipe");
        drop(reader);
        let output = run_cli_with_fixture(&arguments, false, Stdio::from(writer), setup);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{stderr}");
        assert!(stderr.starts_with("error: I/O error:"), "{stderr}");
        assert!(!stderr.contains("panicked"), "{stderr}");
    }
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
