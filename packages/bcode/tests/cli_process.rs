#![cfg(feature = "app")]
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use std::fmt::Write as _;
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
    run_cli_at_root(root.path(), arguments, stdout, stdin)
}

fn isolated_cli(root: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bcode"));
    command
        .env_clear()
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("BCODE_STATE_DIR", root.join("bcode-state"))
        .current_dir(root);
    command
}

fn run_cli_at_root(
    root: &std::path::Path,
    arguments: &[&str],
    stdout: Stdio,
    stdin: Stdio,
) -> Output {
    let mut child = isolated_cli(root)
        .args(arguments)
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

struct ForegroundDaemon(std::process::Child);

impl Drop for ForegroundDaemon {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

fn graph_cli_json(root: &std::path::Path, arguments: &[&str]) -> serde_json::Value {
    let output = run_cli_at_root(root, arguments, Stdio::piped(), Stdio::null());
    assert!(
        output.status.success(),
        "{arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("one complete JSON result")
}

fn start_graph_test_daemon(root: &tempfile::TempDir) -> ForegroundDaemon {
    start_graph_test_daemon_with_setup(root, |_| {})
}

fn start_graph_test_daemon_with_setup(
    root: &tempfile::TempDir,
    setup: impl FnOnce(&std::path::Path),
) -> ForegroundDaemon {
    let library = std::fs::canonicalize(
        std::env::var_os("BCODE_DEFAULT_AGENTS_PLUGIN_TEST_LIBRARY")
            .expect("set BCODE_DEFAULT_AGENTS_PLUGIN_TEST_LIBRARY"),
    )
    .unwrap();
    let plugins = root.path().join(".bcode/plugins/default-agents");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("bcode-plugin.toml"),
        include_str!("../../../plugins/default-agents-plugin/bcode-plugin.toml"),
    )
    .unwrap();
    std::fs::copy(
        library,
        plugins.join("libbcode_default_agents_plugin.dylib"),
    )
    .unwrap();
    std::fs::write(
        root.path().join("bcode.toml"),
        "[plugins]\ndefault = \"none\"\nenabled = [\"bcode.default-agents\"]\n",
    )
    .unwrap();
    setup(&plugins);
    let log = std::fs::File::create(root.path().join("daemon.log")).unwrap();
    let mut daemon = ForegroundDaemon(
        isolated_cli(root.path())
            .args(["server", "run"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(
            daemon.0.try_wait().unwrap().is_none(),
            "daemon exited: {}",
            std::fs::read_to_string(root.path().join("daemon.log")).unwrap()
        );
        if run_cli_at_root(
            root.path(),
            &["server", "status"],
            Stdio::piped(),
            Stdio::null(),
        )
        .status
        .success()
        {
            break;
        }
        assert!(Instant::now() < deadline, "daemon startup timed out");
        std::thread::sleep(Duration::from_millis(50));
    }
    daemon
}

fn prepare_graph_test_run(root: &tempfile::TempDir) {
    let mut source: serde_json::Value = serde_json::from_str(include_str!(
        "../../../fixtures/workflows/source-defined-input.workflow.json"
    ))
    .unwrap();
    let mut second = source["definition"]["nodes"]["await_input"].clone();
    second["id"] = serde_json::json!("second_input");
    source["definition"]["nodes"]["second_input"] = second;
    source["definition"]["exits"] = serde_json::json!(["second_input"]);
    source["definition"]["edges"] = serde_json::json!([
        {"from": "await_input", "to": "second_input"}
    ]);
    std::fs::write(
        root.path().join("workflow.json"),
        serde_json::to_vec(&source).unwrap(),
    )
    .unwrap();
    std::fs::write(
        root.path().join("input.json"),
        r#"{"message":"graph test"}"#,
    )
    .unwrap();
    let session = graph_cli_json(root.path(), &["session", "create", "graph-test", "--json"]);
    let session_id = session["id"].as_str().unwrap();
    graph_cli_json(
        root.path(),
        &[
            "workflow",
            "author",
            "create",
            "workflow.json",
            "--draft-id",
            "draft",
        ],
    );
    graph_cli_json(
        root.path(),
        &[
            "workflow",
            "author",
            "publish",
            "--workflow-id",
            "example/source-defined-input",
            "--draft-id",
            "draft",
            "--expected-generation",
            "1",
            "--activate",
        ],
    );
    graph_cli_json(
        root.path(),
        &[
            "workflow",
            "start",
            "--parent-session-id",
            session_id,
            "--run-id",
            "graph-test",
            "--input",
            "input.json",
            "active",
            "--workflow-id",
            "example/source-defined-input",
        ],
    );
}

#[test]
#[ignore = "requires BCODE_DEFAULT_AGENTS_PLUGIN_TEST_LIBRARY pointing to the built default-agents plugin"]
fn workflow_graph_cli_pages_real_daemon_and_rejects_stale_revision() {
    let root = tempfile::tempdir().unwrap();
    let daemon = start_graph_test_daemon(&root);
    prepare_graph_test_run(&root);
    let first = graph_cli_json(
        root.path(),
        &[
            "workflow",
            "inspect-run-graph",
            "--run-id",
            "graph-test",
            "--expected-revision",
            "1",
            "--limit",
            "1",
        ],
    );
    assert_eq!(first["revision"], 1);
    assert_eq!(first["nodes"].as_array().unwrap().len(), 1);
    let node = first["nodes"][0]["node"]["id"].as_str().unwrap();
    assert_eq!(first["edges"].as_array().unwrap().len(), 1);
    let edge = first["edges"][0]["edge_id"].as_u64().unwrap().to_string();
    let next = graph_cli_json(
        root.path(),
        &[
            "workflow",
            "inspect-run-graph",
            "--run-id",
            "graph-test",
            "--expected-revision",
            "1",
            "--after-node-id",
            node,
            "--after-edge-id",
            &edge,
            "--limit",
            "1",
        ],
    );
    assert_ne!(next["nodes"][0]["node"]["id"], node);
    assert!(next["edges"].as_array().unwrap().is_empty());
    assert_eq!(next["edges_complete"], true);
    assert_eq!(next["revision"], 1);
    let stale = run_cli_at_root(
        root.path(),
        &[
            "workflow",
            "inspect-run-graph",
            "--run-id",
            "graph-test",
            "--expected-revision",
            "2",
        ],
        Stdio::piped(),
        Stdio::null(),
    );
    assert_eq!(stale.status.code(), Some(1));
    assert!(stale.stdout.is_empty());
    assert!(String::from_utf8_lossy(&stale.stderr).contains("workflow_unavailable"));
    drop(daemon);
}

#[test]
#[ignore = "requires BCODE_DEFAULT_AGENTS_PLUGIN_TEST_LIBRARY pointing to the built default-agents plugin"]
fn auth_pool_daemon_status_returns_json_from_real_host() {
    let root = tempfile::tempdir().unwrap();
    let daemon = start_graph_test_daemon(&root);
    let pools = graph_cli_json(root.path(), &["auth", "pool", "daemon-status"]);
    assert!(pools.is_array());
    drop(daemon);
}

#[test]
#[ignore = "requires BCODE_DEFAULT_AGENTS_PLUGIN_TEST_LIBRARY pointing to the built default-agents plugin"]
fn auth_pool_preference_failure_is_secret_safe_through_real_host() {
    let root = tempfile::tempdir().unwrap();
    let daemon = start_graph_test_daemon(&root);
    let output = run_cli_at_root(
        root.path(),
        &[
            "session",
            "set-auth-pool",
            "private-pool-marker",
            "--profile",
            "private-profile-marker",
            "--json",
        ],
        Stdio::piped(),
        Stdio::null(),
    );
    drop(daemon);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("auth_pool_preference"));
    assert!(error.contains("could not be saved"));
    assert!(!error.contains("private-pool-marker"));
    assert!(!error.contains("private-profile-marker"));
}

#[test]
#[ignore = "requires BCODE_DEFAULT_AGENTS_PLUGIN_TEST_LIBRARY pointing to the built default-agents plugin"]
fn auth_pool_preference_set_and_clear_preserves_configuration() {
    let root = tempfile::tempdir().unwrap();
    let daemon = start_graph_test_daemon(&root);
    let path = root.path().join("bcode.toml");
    let config = format!(
        "{}\n[auth.profiles.first]\nbackend = \"sshenv\"\nscheme = \"chatgpt\"\n[auth.profiles.second]\nbackend = \"sshenv\"\nscheme = \"chatgpt\"\n[auth.pools.test]\nprofiles = [\"first\", \"second\"]\nstrategy = \"failover\"\n",
        std::fs::read_to_string(&path).unwrap()
    );
    std::fs::write(&path, &config).unwrap();
    let set = graph_cli_json(
        root.path(),
        &[
            "session",
            "set-auth-pool",
            "test",
            "--profile",
            "second",
            "--json",
        ],
    );
    assert_eq!(set["status"], "auth_pool_preference_set");
    let pools = graph_cli_json(root.path(), &["auth", "pool", "daemon-status"]);
    let pool = pools
        .as_array()
        .unwrap()
        .iter()
        .find(|pool| pool["pool"] == "test")
        .unwrap();
    assert_eq!(pool["preferred_profile"], "second");
    assert_eq!(pool["preference_source"], "interactive_state");
    graph_cli_json(
        root.path(),
        &["session", "set-auth-pool", "test", "--clear", "--json"],
    );
    let pools = graph_cli_json(root.path(), &["auth", "pool", "daemon-status"]);
    let pool = pools
        .as_array()
        .unwrap()
        .iter()
        .find(|pool| pool["pool"] == "test")
        .unwrap();
    assert_eq!(pool["preferred_profile"], "first");
    assert_eq!(pool["preference_source"], "pool_order");
    assert_eq!(std::fs::read_to_string(path).unwrap(), config);
    drop(daemon);
}

#[test]
fn auth_pool_daemon_status_reports_connection_failure_without_json() {
    let output = run_cli_with_state(&["auth", "pool", "daemon-status"], true);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(!output.stderr.is_empty());
}

#[test]
#[ignore = "requires BCODE_DEFAULT_AGENTS_PLUGIN_TEST_LIBRARY pointing to the built default-agents plugin"]
fn plugin_contributions_returns_daemon_schema_without_invocation() {
    let root = tempfile::tempdir().unwrap();
    let daemon = start_graph_test_daemon(&root);
    let value = graph_cli_json(root.path(), &["plugin", "contributions"]);
    assert!(value["commands"].is_array());
    assert!(value["command_contributions"].is_array());
    assert!(value["config_extensions"].is_array());
    assert_eq!(value.as_object().unwrap().len(), 3);
    drop(daemon);
}

#[test]
#[ignore = "requires BCODE_DEFAULT_AGENTS_PLUGIN_TEST_LIBRARY pointing to the built default-agents plugin"]
fn ralph_status_reads_live_daemon_and_missing_run_fails() {
    let root = tempfile::tempdir().unwrap();
    let daemon = start_graph_test_daemon(&root);
    let repo = root.path().to_str().unwrap();
    let value = graph_cli_json(root.path(), &["ralph", "status", "--repo-root", repo]);
    assert_eq!(value, serde_json::json!({"loop_summary": null}));
    let missing = run_cli_at_root(
        root.path(),
        &["ralph", "run-status", "--repo-root", repo],
        Stdio::piped(),
        Stdio::piped(),
    );
    assert_eq!(missing.status.code(), Some(1));
    assert!(missing.stdout.is_empty());
    assert!(!missing.stderr.is_empty());
    drop(daemon);
}

#[test]
#[ignore = "requires BCODE_DEFAULT_AGENTS_PLUGIN_TEST_LIBRARY pointing to the built default-agents plugin"]
fn composer_draft_cli_reads_sets_and_clears_live_daemon() {
    let root = tempfile::tempdir().unwrap();
    let daemon = start_graph_test_daemon(&root);
    let directory = root.path().to_str().unwrap();
    let base = [
        "session",
        "composer-draft",
        "--launch-working-directory",
        directory,
    ];
    assert_eq!(graph_cli_json(root.path(), &base), serde_json::Value::Null);
    let path = root.path().join("draft.txt");
    let text = "draft with Unicode λ\nand a trailing newline\n";
    std::fs::write(&path, text).unwrap();
    let mut set = base.to_vec();
    set.extend(["--set-file", path.to_str().unwrap()]);
    assert_eq!(graph_cli_json(root.path(), &set), serde_json::Value::Null);
    assert_eq!(graph_cli_json(root.path(), &base), serde_json::json!(text));
    let session = graph_cli_json(root.path(), &["session", "create", "draft-test", "--json"]);
    let session_scope = [
        "session",
        "composer-draft",
        "--session-id",
        session["id"].as_str().unwrap(),
    ];
    assert_eq!(
        graph_cli_json(root.path(), &session_scope),
        serde_json::Value::Null
    );
    std::fs::write(&path, "session-only draft").unwrap();
    let mut session_set = session_scope.to_vec();
    session_set.extend(["--set-file", path.to_str().unwrap()]);
    assert_eq!(
        graph_cli_json(root.path(), &session_set),
        serde_json::Value::Null
    );
    assert_eq!(
        graph_cli_json(root.path(), &session_scope),
        serde_json::json!("session-only draft")
    );
    assert_eq!(graph_cli_json(root.path(), &base), serde_json::json!(text));
    let mut session_clear = session_scope.to_vec();
    session_clear.push("--clear");
    assert_eq!(
        graph_cli_json(root.path(), &session_clear),
        serde_json::Value::Null
    );
    assert_eq!(
        graph_cli_json(root.path(), &session_scope),
        serde_json::Value::Null
    );
    assert_eq!(graph_cli_json(root.path(), &base), serde_json::json!(text));
    let mut clear = base.to_vec();
    clear.push("--clear");
    assert_eq!(graph_cli_json(root.path(), &clear), serde_json::Value::Null);
    assert_eq!(graph_cli_json(root.path(), &base), serde_json::Value::Null);
    drop(daemon);
}

#[test]
#[ignore = "requires BCODE_DEFAULT_AGENTS_PLUGIN_TEST_LIBRARY pointing to the built default-agents plugin"]
fn derivation_cli_executes_and_observes_live_daemon() {
    let root = tempfile::tempdir().unwrap();
    let daemon = start_graph_test_daemon(&root);
    let session = graph_cli_json(
        root.path(),
        &["session", "create", "derivation-source", "--json"],
    );
    let id = session["id"].as_str().unwrap();
    let source = graph_cli_json(root.path(), &["session", "derivation-snapshot", id]);
    let generation = source["generation"].to_string();
    let page = graph_cli_json(
        root.path(),
        &[
            "session",
            "derivation-prompts",
            id,
            "--generation",
            &generation,
            "--limit",
            "1",
        ],
    );
    assert_eq!(page["generation"], source["generation"]);
    assert_eq!(page["candidates"], serde_json::json!([]));
    let operation_id = "00000000-0000-0000-0000-000000000042";
    let request = serde_json::json!({
        "version": source["version"], "operation_id": operation_id,
        "idempotency_key": "cli-derivation-test", "source": source,
        "cutoff_sequence": 0, "destination_name": "derived-test",
        "initial_draft": "derived draft",
        "lineage": {"producer": "cli-test", "operation_kind": "test"}
    });
    let path = root.path().join("derive.json");
    std::fs::write(&path, serde_json::to_vec(&request).unwrap()).unwrap();
    let args = ["session", "derive", "--request", path.to_str().unwrap()];
    let outcome = graph_cli_json(root.path(), &args);
    assert_eq!(outcome["status"], "succeeded");
    assert_ne!(outcome["session"]["id"], session["id"]);
    assert_eq!(graph_cli_json(root.path(), &args), outcome);
    let status = graph_cli_json(root.path(), &["session", "derivation-status", operation_id]);
    assert_eq!(status["outcome"], outcome);
    assert_eq!(
        graph_cli_json(root.path(), &["session", "cancel-derivation", operation_id]),
        false
    );
    assert_eq!(
        graph_cli_json(
            root.path(),
            &[
                "session",
                "composer-draft",
                "--session-id",
                outcome["session"]["id"].as_str().unwrap()
            ]
        ),
        "derived draft"
    );
    drop(daemon);
}

#[test]
fn derivation_commands_validate_identity_and_reach_daemon() {
    for command in [
        "derivation-snapshot",
        "derivation-status",
        "cancel-derivation",
    ] {
        let invalid = run_cli_with_state(&["session", command, "not-an-id"], true);
        assert_eq!(invalid.status.code(), Some(2));
        assert!(invalid.stdout.is_empty());
        let failed = run_cli_with_state(
            &["session", command, "00000000-0000-0000-0000-000000000001"],
            true,
        );
        assert_eq!(failed.status.code(), Some(1));
        assert!(failed.stdout.is_empty());
        assert!(!failed.stderr.is_empty());
    }
}

#[test]
fn composer_draft_requires_one_scope_and_reports_daemon_failure() {
    let missing = run_cli_with_state(&["session", "composer-draft"], true);
    assert_eq!(missing.status.code(), Some(2));
    assert!(missing.stdout.is_empty());
    let conflicting = run_cli_with_state(
        &[
            "session",
            "composer-draft",
            "--session-id",
            "00000000-0000-0000-0000-000000000001",
            "--launch-working-directory",
            "/unused",
        ],
        true,
    );
    assert_eq!(conflicting.status.code(), Some(2));
    assert!(conflicting.stdout.is_empty());
    let failed = run_cli_with_state(
        &[
            "session",
            "composer-draft",
            "--launch-working-directory",
            "/unused",
        ],
        true,
    );
    assert_eq!(failed.status.code(), Some(1));
    assert!(failed.stdout.is_empty());
    assert!(!failed.stderr.is_empty());
}

#[test]
fn ralph_status_requires_identity_and_reports_daemon_failure() {
    let invalid = run_cli_with_state(&["ralph", "status"], true);
    assert_eq!(invalid.status.code(), Some(2));
    assert!(invalid.stdout.is_empty());
    for action in ["status", "run-status"] {
        let failed = run_cli_with_state(&["ralph", action, "--repo-root", "/unused"], true);
        assert_eq!(failed.status.code(), Some(1));
        assert!(failed.stdout.is_empty());
        assert!(!failed.stderr.is_empty());
    }
}

#[test]
fn plugin_contributions_rejects_local_roots_and_reports_daemon_failure() {
    let invalid = run_cli_with_state(&["plugin", "contributions", "--root", "unused"], true);
    assert_eq!(invalid.status.code(), Some(2));
    assert!(invalid.stdout.is_empty());
    let failed = run_cli_with_state(&["plugin", "contributions"], true);
    assert_eq!(failed.status.code(), Some(1));
    assert!(failed.stdout.is_empty());
    assert!(!failed.stderr.is_empty());
}

#[test]
#[ignore = "requires BCODE_DEFAULT_AGENTS_PLUGIN_TEST_LIBRARY pointing to the built default-agents plugin"]
fn workflow_launch_detail_reads_source_through_daemon() {
    let root = tempfile::tempdir().unwrap();
    let daemon = start_graph_test_daemon(&root);
    let source = root.path().join("source.workflow.json");
    std::fs::write(
        &source,
        include_str!("../../../fixtures/workflows/source-defined-input.workflow.json"),
    )
    .unwrap();
    let request = serde_json::json!({
        "version": 1, "workspace": root.path(),
        "source": {"source_kind":"explicit_source", "source_path": source, "source_format":"json"}
    });
    std::fs::write(
        root.path().join("request.json"),
        serde_json::to_vec(&request).unwrap(),
    )
    .unwrap();
    let detail = graph_cli_json(
        root.path(),
        &["workflow", "launch-detail", "--request", "request.json"],
    );
    assert_eq!(detail["version"], 1);
    assert!(detail.is_object());
    drop(daemon);
}

#[test]
#[ignore = "requires BCODE_DEFAULT_AGENTS_PLUGIN_TEST_LIBRARY pointing to the built default-agents plugin"]
fn workflow_package_cli_applies_and_publishes_exact_lock() {
    let root = tempfile::tempdir().unwrap();
    let daemon = start_graph_test_daemon(&root);
    std::fs::write(
        root.path().join("member.json"),
        include_str!("../../../fixtures/workflows/source-defined-input.workflow.json"),
    )
    .unwrap();
    std::fs::write(
        root.path().join("package.json"),
        serde_json::to_vec(&serde_json::json!({
            "version": 3, "package_id": "cli/package", "exports": {"main": "member"},
            "members": [{"member_id": "member", "source_name": "member.json"}]
        }))
        .unwrap(),
    )
    .unwrap();
    let applied = graph_cli_json(
        root.path(),
        &["workflow", "package", "apply", "package.json"],
    );
    assert_eq!(applied[0]["outcome"], "applied");
    std::fs::write(
        root.path().join("lock.json"),
        serde_json::to_vec(&applied[0]["lock"]).unwrap(),
    )
    .unwrap();
    let duplicate = run_cli_at_root(
        root.path(),
        &["workflow", "package", "apply", "package.json"],
        Stdio::piped(),
        Stdio::null(),
    );
    assert_eq!(duplicate.status.code(), Some(1));
    let published = graph_cli_json(
        root.path(),
        &[
            "workflow",
            "package",
            "publish",
            "--lock",
            "lock.json",
            "--expected-generation",
            "member=1",
        ],
    );
    assert_eq!(published["outcome"], "published");
    assert_eq!(
        published["lock"]["members"][0]["published_revision"]["revision"],
        1
    );
    drop(daemon);
}

#[test]
#[ignore = "requires BCODE_DEFAULT_AGENTS_PLUGIN_TEST_LIBRARY pointing to the built default-agents plugin"]
fn workflow_template_cli_instantiates_and_starts_external_document() {
    use sha2::Digest as _;
    let root = tempfile::tempdir().unwrap();
    let daemon = start_graph_test_daemon_with_setup(&root, |plugins| {
        let mut document: serde_json::Value = serde_json::from_str(include_str!(
            "../../../fixtures/workflows/source-defined-input.workflow.json"
        ))
        .unwrap();
        document["configuration_schema"] = document["definition"]["input"].clone();
        document["configuration_defaults"] = serde_json::json!({"message":"template acceptance"});
        let source = serde_json::to_string(&document).unwrap();
        std::fs::write(plugins.join("template.json"), &source).unwrap();
        let manifest = plugins.join("bcode-plugin.toml");
        let mut contents = std::fs::read_to_string(&manifest).unwrap();
        write!(contents, "\n[[workflow_templates]]\ncontribution_version = 1\ntemplate_id = \"cli-input\"\ntemplate_version = 1\ntitle = \"CLI input\"\ndescription = \"CLI acceptance\"\n[workflow_templates.document_source]\npath = \"template.json\"\nsha256 = \"{:x}\"\n", sha2::Sha256::digest(source.as_bytes())).unwrap();
        std::fs::write(manifest, contents).unwrap();
    });
    let request = serde_json::json!({"owner_plugin_id":"bcode.default-agents", "template_id":"cli-input", "template_version":1, "workflow_id":"cli/template", "draft_id":"cli-draft"});
    std::fs::write(
        root.path().join("instantiate.json"),
        serde_json::to_vec(&request).unwrap(),
    )
    .unwrap();
    let instantiated = graph_cli_json(
        root.path(),
        &[
            "workflow",
            "template",
            "instantiate",
            "--request",
            "instantiate.json",
        ],
    );
    assert_eq!(instantiated["workflow"]["workflow_id"], "cli/template");
    assert_eq!(instantiated["draft"]["generation"], 1);
    let session = graph_cli_json(
        root.path(),
        &["session", "create", "template-parent", "--json"],
    );
    let request = serde_json::json!({"owner_plugin_id":"bcode.default-agents", "template_id":"cli-input", "template_version":1, "parent_session_id":session["id"], "configuration":{"message":"template acceptance"}, "run_id":"cli-template-run"});
    std::fs::write(
        root.path().join("start.json"),
        serde_json::to_vec(&request).unwrap(),
    )
    .unwrap();
    let started = graph_cli_json(
        root.path(),
        &["workflow", "template", "start", "--request", "start.json"],
    );
    assert_eq!(started["run"]["run_id"], "cli-template-run");
    graph_cli_json(
        root.path(),
        &["workflow", "cancel-run", "--run-id", "cli-template-run"],
    );
    drop(daemon);
}

#[test]
fn workflow_template_instantiation_rejects_invalid_request_before_startup() {
    let output = run_cli_with_fixture(
        &[
            "workflow",
            "template",
            "instantiate",
            "--request",
            "request.json",
        ],
        true,
        Stdio::piped(),
        |root| {
            std::fs::write(root.join("request.json"), r#"{"private":"do-not-echo"}"#).unwrap();
        },
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("invalid workflow template instantiation request"));
    assert!(!error.contains("do-not-echo"));
}

#[test]
#[ignore = "requires BCODE_DEFAULT_AGENTS_PLUGIN_TEST_LIBRARY pointing to the built default-agents plugin"]
fn workflow_template_start_missing_template_returns_no_admission() {
    let root = tempfile::tempdir().unwrap();
    let daemon = start_graph_test_daemon(&root);
    let session = graph_cli_json(
        root.path(),
        &["session", "create", "template-parent", "--json"],
    );
    std::fs::write(
        root.path().join("request.json"),
        serde_json::to_vec(&serde_json::json!({
            "owner_plugin_id":"missing.plugin", "template_id":"missing", "template_version":1,
            "parent_session_id":session["id"], "configuration":{}, "run_id":"missing-template-run"
        }))
        .unwrap(),
    )
    .unwrap();
    let output = run_cli_at_root(
        root.path(),
        &["workflow", "template", "start", "--request", "request.json"],
        Stdio::piped(),
        Stdio::null(),
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("workflow_unavailable: workflow state is unavailable"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    drop(daemon);
}

#[test]
fn workflow_template_start_rejects_invalid_request_before_startup() {
    let output = run_cli_with_fixture(
        &["workflow", "template", "start", "--request", "request.json"],
        true,
        Stdio::piped(),
        |root| {
            std::fs::write(root.join("request.json"), r#"{"private":"do-not-echo"}"#).unwrap();
        },
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("invalid workflow template start request"));
    assert!(!error.contains("do-not-echo"));
}

#[test]
fn workflow_launch_detail_rejects_future_version_before_dispatch() {
    let output = run_cli_with_fixture(
        &["workflow", "launch-detail", "--request", "request.json"],
        true,
        Stdio::piped(),
        |root| {
            std::fs::write(root.join("request.json"), serde_json::to_vec(&serde_json::json!({
                "version": 999, "workspace": root,
                "source": {"source_kind":"explicit_source", "source_path":"source.json", "source_format":"json"}
            })).unwrap()).unwrap();
        },
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("workflow launch detail request"));
}

#[test]
#[ignore = "requires BCODE_DEFAULT_AGENTS_PLUGIN_TEST_LIBRARY pointing to the built default-agents plugin"]
fn workflow_draft_edit_updates_and_rejects_stale_generation() {
    let root = tempfile::tempdir().unwrap();
    let daemon = start_graph_test_daemon(&root);
    let source: serde_json::Value = serde_json::from_str(include_str!(
        "../../../fixtures/workflows/source-defined-input.workflow.json"
    ))
    .unwrap();
    std::fs::write(
        root.path().join("source.json"),
        serde_json::to_vec(&source).unwrap(),
    )
    .unwrap();
    graph_cli_json(
        root.path(),
        &[
            "workflow",
            "author",
            "create",
            "source.json",
            "--draft-id",
            "edit-draft",
        ],
    );
    let mut node = source["definition"]["nodes"]["await_input"].clone();
    node["name"] = serde_json::json!("Updated input");
    let request = serde_json::json!({
        "workflow_id":"example/source-defined-input", "draft_id":"edit-draft",
        "batch":{"version":1,"expected_generation":1,"edits":[{"operation":"update_node","node":node}]},
        "producer":{"kind":"human","producer_id":"cli-test"}
    });
    std::fs::write(
        root.path().join("edit.json"),
        serde_json::to_vec(&request).unwrap(),
    )
    .unwrap();
    let args = ["workflow", "author", "edit", "--request", "edit.json"];
    let updated = graph_cli_json(root.path(), &args);
    assert_eq!(updated["updated"]["generation"], 2);
    assert_eq!(
        updated["updated"]["document"]["definition"]["nodes"]["await_input"]["name"],
        "Updated input"
    );
    let stale = run_cli_at_root(root.path(), &args, Stdio::piped(), Stdio::null());
    assert_eq!(stale.status.code(), Some(1));
    let conflict: serde_json::Value = serde_json::from_slice(&stale.stdout).unwrap();
    assert_eq!(conflict["conflict"]["current_generation"], 2);
    assert_eq!(conflict["conflict"]["expected_generation"], 1);
    drop(daemon);
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

#[cfg(unix)]
#[test]
fn model_ignore_mutations_preserve_unreadable_state_targets() {
    for missing in [false, true] {
        let fixture = tempfile::tempdir().expect("state target fixture");
        let target = fixture.path().join("ignores.toml");
        if !missing {
            std::fs::write(&target, b"invalid [toml").unwrap();
        }
        for command in ["ignore", "unignore"] {
            for json in [false, true] {
                let mut arguments =
                    vec!["model", command, "example", "--provider", "custom-provider"];
                if json {
                    arguments.push("--json");
                }
                let output = run_cli_with_fixture(&arguments, false, Stdio::piped(), |root| {
                    let state = root.join("bcode-state");
                    std::fs::create_dir_all(&state).unwrap();
                    std::os::unix::fs::symlink(&target, state.join("model-ignores.toml")).unwrap();
                });
                assert_eq!(
                    output.status.code(),
                    Some(1),
                    "{arguments:?}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert!(
                    output.stdout.is_empty(),
                    "unexpected receipt: {arguments:?}"
                );
                if missing {
                    assert!(!target.exists(), "must not create dangling symlink target");
                } else {
                    assert_eq!(std::fs::read(&target).unwrap(), b"invalid [toml");
                }
            }
        }
    }
}

#[test]
fn model_ignore_diagnostics_do_not_disclose_state_contents() {
    for contents in [
        "secret-token-123 = [",
        "[providers.first]\nmodels = [\"secret-token-123\", 42]",
    ] {
        for command in ["ignored", "ignore", "unignore"] {
            for json in [false, true] {
                let mut arguments = vec!["model", command];
                if command != "ignored" {
                    arguments.extend(["example", "--provider", "custom-provider"]);
                }
                if json {
                    arguments.push("--json");
                }
                let output = run_cli_with_fixture(&arguments, false, Stdio::piped(), |root| {
                    let state = root.join("bcode-state");
                    std::fs::create_dir_all(&state).unwrap();
                    std::fs::write(state.join("model-ignores.toml"), contents).unwrap();
                });
                let stderr = String::from_utf8_lossy(&output.stderr);
                assert_eq!(output.status.code(), Some(1), "{arguments:?}: {stderr}");
                assert!(
                    output.stdout.is_empty(),
                    "unexpected receipt: {arguments:?}"
                );
                assert!(
                    stderr.contains("invalid TOML or ignore-rule schema"),
                    "{stderr}"
                );
                assert!(!stderr.contains("secret-token-123"), "{stderr}");
            }
        }
    }
}

#[test]
fn workflow_package_parse_failures_are_secret_safe_before_daemon_access() {
    for (extension, source) in [
        ("json", r#"{"version":"secret-marker-123"}"#),
        ("yaml", "version: secret-marker-123\n"),
        ("toml", "version = \"secret-marker-123\"\n"),
    ] {
        let fixture = tempfile::tempdir().expect("manifest fixture");
        let manifest = fixture.path().join(format!("package.{extension}"));
        std::fs::write(&manifest, source).unwrap();
        for command in ["validate", "preview", "apply"] {
            let output = run_cli_with_state(
                &["workflow", "package", command, manifest.to_str().unwrap()],
                true,
            );
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(output.status.code(), Some(2), "{command}: {stderr}");
            assert!(output.stdout.is_empty());
            assert!(stderr.contains("invalid workflow package"), "{stderr}");
            assert!(!stderr.contains("secret-marker-123"), "{stderr}");
            assert_eq!(std::fs::read_to_string(&manifest).unwrap(), source);
        }
    }
}

#[test]
fn workflow_input_stdin_rejects_invalid_and_oversized_payloads() {
    use std::io::{Seek as _, Write as _};

    for (contents, diagnostic) in [
        (
            b"{\"secret\":\"workflow-secret-marker\"} {}".to_vec(),
            "trailing characters",
        ),
        (
            vec![b' '; bcode_workflow::MAX_WORKFLOW_AUTHORING_DOCUMENT_BYTES + 1],
            "workflow JSON exceeds",
        ),
    ] {
        let mut input = tempfile::tempfile().expect("stdin fixture");
        input.write_all(&contents).unwrap();
        input.rewind().unwrap();
        let output = run_cli_with_stdio(
            &[
                "workflow",
                "provide-input",
                "--run-id",
                "test-run",
                "--node-id",
                "test-node",
                "--activation-id",
                "test-activation",
                "--value",
                "-",
            ],
            true,
            Stdio::piped(),
            Stdio::from(input),
            |_| {},
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{stderr}");
        assert!(output.stdout.is_empty());
        assert!(stderr.contains(diagnostic), "{stderr}");
        assert!(!stderr.contains("workflow-secret-marker"), "{stderr}");
    }
}

#[test]
fn workflow_input_stdin_accepts_valid_json_through_exact_limit() {
    use std::io::{Seek as _, Write as _};

    for length in [4, bcode_workflow::MAX_WORKFLOW_AUTHORING_DOCUMENT_BYTES] {
        let mut bytes = b"null".to_vec();
        bytes.resize(length, b' ');
        let mut input = tempfile::tempfile().expect("stdin fixture");
        input.write_all(&bytes).unwrap();
        input.rewind().unwrap();
        let output = run_cli_with_stdio(
            &[
                "workflow",
                "provide-input",
                "--run-id",
                "test-run",
                "--node-id",
                "test-node",
                "--activation-id",
                "test-activation",
                "--value",
                "-",
            ],
            true,
            Stdio::piped(),
            Stdio::from(input),
            |_| {},
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{stderr}");
        assert!(output.stdout.is_empty());
        assert!(!stderr.contains("workflow JSON exceeds"), "{stderr}");
        assert!(!stderr.is_empty());
    }
}

#[test]
fn workflow_catalog_view_validates_typed_query_before_daemon_access() {
    for query in [
        r#"{"limit":"private-marker"}"#,
        r#"{"limit":7,"unknown":"private-marker"}"#,
    ] {
        let output = run_cli_with_state(&["workflow", "catalog-view", "--query", query], true);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{stderr}");
        assert!(output.stdout.is_empty());
        assert!(
            stderr.contains("invalid workflow catalog query"),
            "{stderr}"
        );
        assert!(!stderr.contains("private-marker"), "{stderr}");
    }
    let output = run_cli_with_state(
        &["workflow", "catalog-view", "--query", r#"{"limit":7}"#],
        true,
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(output.stdout.is_empty());
    assert!(!stderr.is_empty());
}

#[test]
fn workflow_run_view_reaches_daemon_boundary() {
    let output = run_cli_with_state(
        &[
            "workflow", "run-view", "--run-id", "test-run", "--limit", "7",
        ],
        true,
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(output.stdout.is_empty());
    assert!(!stderr.is_empty());
    assert!(!stderr.contains("unrecognized subcommand"), "{stderr}");
}

#[test]
fn workflow_approval_requires_exactly_one_decision() {
    let arguments = [
        "workflow",
        "resolve-approval",
        "--run-id",
        "test-run",
        "--node-id",
        "test-node",
        "--activation-id",
        "test-activation",
    ];
    assert_usage_error(&arguments, "required arguments were not provided");
    let mut conflicting = arguments.to_vec();
    conflicting.extend(["--approve", "--deny"]);
    assert_usage_error(&conflicting, "cannot be used with");
    for decision in ["--approve", "--deny"] {
        let mut selected = arguments.to_vec();
        selected.push(decision);
        let output = run_cli_with_state(&selected, true);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn workflow_mutation_approval_listing_reaches_application_boundary() {
    for run in [None, Some("test-run")] {
        let mut arguments = vec!["workflow", "mutation-approvals", "--limit", "5"];
        if let Some(run) = run {
            arguments.extend(["--run-id", run]);
        }
        let output = run_cli_with_state(&arguments, true);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
}

#[test]
fn workflow_mutation_approval_resolution_requires_explicit_decision() {
    let arguments = [
        "workflow",
        "resolve-mutation-approval",
        "--approval-id",
        "test-approval",
    ];
    assert_usage_error(&arguments, "required arguments were not provided");
    let mut conflicting = arguments.to_vec();
    conflicting.extend(["--approve", "--deny"]);
    assert_usage_error(&conflicting, "cannot be used with");
    for decision in ["--approve", "--deny"] {
        let mut selected = arguments.to_vec();
        selected.push(decision);
        let output = run_cli_with_state(&selected, true);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn workflow_events_accepts_page_cursor_and_rejects_invalid_sequence() {
    let arguments = ["workflow", "events", "--run-id", "test-run", "--limit", "5"];
    for cursor in [None, Some("0"), Some("18446744073709551615")] {
        let mut page = arguments.to_vec();
        if let Some(cursor) = cursor {
            page.extend(["--after-sequence", cursor]);
        }
        let output = run_cli_with_state(&page, true);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty());
    }
    let mut invalid = arguments.to_vec();
    invalid.extend(["--after-sequence", "18446744073709551616"]);
    assert_usage_error(&invalid, "invalid value");
}

#[test]
fn workflow_attempts_requires_complete_keyset_cursor() {
    let arguments = [
        "workflow", "attempts", "--run-id", "test-run", "--limit", "5",
    ];
    for partial in [
        ["--after-prepared-at-ms", "10"],
        ["--after-dispatch-identity", "dispatch"],
    ] {
        let mut page = arguments.to_vec();
        page.extend(partial);
        assert_usage_error(&page, "required arguments were not provided");
    }
    for cursor in [false, true] {
        let mut page = arguments.to_vec();
        if cursor {
            page.extend([
                "--after-prepared-at-ms",
                "10",
                "--after-dispatch-identity",
                "dispatch",
            ]);
        }
        let output = run_cli_with_state(&page, true);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn workflow_run_controls_require_identity_and_do_not_claim_success_on_failure() {
    for command in ["cancel-run", "pause-run", "resume-run", "run-status"] {
        assert_usage_error(
            &["workflow", command],
            "required arguments were not provided",
        );
        let output = run_cli_with_state(&["workflow", command, "--run-id", "run-1"], true);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{command}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty(), "{command}");
    }
}

#[test]
fn workflow_definition_queries_require_exact_version_and_report_failure() {
    assert_usage_error(
        &[
            "workflow",
            "describe-definition",
            "--definition-id",
            "definition-1",
        ],
        "required arguments were not provided",
    );
    for arguments in [
        vec!["workflow", "definitions", "--limit", "5"],
        vec![
            "workflow",
            "describe-definition",
            "--definition-id",
            "definition-1",
            "--version",
            "3",
        ],
    ] {
        let output = run_cli_with_state(&arguments, true);
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn workflow_doctor_requires_identity_and_reports_daemon_failure() {
    assert_usage_error(
        &["workflow", "doctor"],
        "required arguments were not provided",
    );
    let output = run_cli_with_state(
        &["workflow", "doctor", "--run-id", "run-1", "--limit", "5"],
        true,
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
}

#[test]
fn workflow_runs_does_not_emit_a_list_on_daemon_failure() {
    let output = run_cli_with_state(&["workflow", "runs", "--limit", "5"], true);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
}

#[test]
fn workflow_retry_zero_attempt_is_usage_error() {
    let output = run_cli_with_state(
        &[
            "workflow",
            "retry-node",
            "--run-id",
            "run",
            "--node-id",
            "node",
            "--activation-id",
            "activation",
            "--failed-attempt",
            "0",
        ],
        true,
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid value"));
}

#[test]
fn workflow_retry_node_does_not_emit_success_on_daemon_failure() {
    assert_usage_error(
        &["workflow", "retry-node"],
        "required arguments were not provided",
    );
    let output = run_cli_with_state(
        &[
            "workflow",
            "retry-node",
            "--run-id",
            "test-run",
            "--node-id",
            "test-node",
            "--activation-id",
            "test-activation",
            "--failed-attempt",
            "1",
        ],
        true,
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
}

#[test]
fn workflow_waits_requires_run_and_reaches_application_boundary() {
    assert_usage_error(
        &["workflow", "waits"],
        "required arguments were not provided",
    );
    let output = run_cli_with_state(
        &["workflow", "waits", "--run-id", "test-run", "--limit", "5"],
        true,
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
}

#[test]
fn ignored_models_json_filters_provider_rules() {
    for provider in [None, Some("first"), Some("missing")] {
        let mut arguments = vec!["model", "ignored", "--json"];
        if let Some(provider) = provider {
            arguments.extend(["--provider", provider]);
        }
        let output = run_cli_with_fixture(&arguments, false, Stdio::piped(), |root| {
            let state = root.join("bcode-state");
            std::fs::create_dir_all(&state).unwrap();
            std::fs::write(state.join("model-ignores.toml"), "[providers.first]\nmodels = [\"one\"]\npatterns = [\"test-*\"]\n[providers.second]\nmodels = [\"two\"]\n").unwrap();
        });
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            value.as_object().unwrap().len(),
            match provider {
                None => 2,
                Some("first") => 1,
                _ => 0,
            }
        );
        if provider != Some("missing") {
            assert_eq!(value["first"]["models"], serde_json::json!(["one"]));
            assert_eq!(value["first"]["patterns"], serde_json::json!(["test-*"]));
        }
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn model_ignore_mutations_return_json_receipts() {
    for (command, operation) in [("ignore", "model_ignored"), ("unignore", "model_unignored")] {
        let arguments = [
            "model",
            command,
            "example",
            "--provider",
            "custom-provider",
            "--json",
        ];
        let output = run_cli(&arguments);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(receipt["operation"], operation);
        assert_eq!(receipt["provider"], "custom-provider");
        assert_eq!(receipt["model_id"], "example");
        assert!(
            receipt["state_path"]
                .as_str()
                .unwrap()
                .ends_with("model-ignores.toml")
        );
        assert!(output.stderr.is_empty());
        let failed = run_cli_with_state(&arguments, true);
        assert_eq!(failed.status.code(), Some(1));
        assert!(failed.stdout.is_empty());
    }
}

#[test]
fn explicit_model_ignore_provider_does_not_require_a_configured_default() {
    for command in ["ignore", "unignore"] {
        let output = run_cli(&["model", command, "example", "--provider", "custom-provider"]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(0), "{command}: {stderr}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("example"), "{stdout}");
        assert!(stdout.contains("custom-provider"), "{stdout}");
        assert!(output.stderr.is_empty(), "{stderr}");
    }
}

#[test]
fn model_inspection_commands_report_daemon_failure_without_results() {
    for command in ["list", "status", "diagnostics", "capabilities", "validate"] {
        for json in [false, true] {
            let mut arguments = vec!["model", command];
            if json {
                arguments.push("--json");
            }
            let output = run_cli_with_state(&arguments, true);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(output.status.code(), Some(1), "{arguments:?}: {stderr}");
            assert!(output.stdout.is_empty(), "unexpected result: {arguments:?}");
            assert!(stderr.starts_with("error:"), "{arguments:?}: {stderr}");
            assert!(!stderr.contains("panicked"), "{arguments:?}: {stderr}");
        }
    }
}

#[test]
fn model_set_alias_accepts_machine_output_and_reports_daemon_failure() {
    for prefix in [vec!["model", "set"], vec!["session", "set-model"]] {
        for json in [false, true] {
            let mut arguments = prefix.clone();
            arguments.extend([
                "00000000-0000-0000-0000-000000000001",
                "example",
                "--provider",
                "provider",
            ]);
            if json {
                arguments.push("--json");
            }
            let output = run_cli_with_state(&arguments, true);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(output.status.code(), Some(1), "{arguments:?}: {stderr}");
            assert!(
                output.stdout.is_empty(),
                "unexpected success receipt: {arguments:?}"
            );
            assert!(stderr.starts_with("error:"), "{stderr}");
            assert!(!stderr.contains("panicked"), "{stderr}");
        }
    }
}

#[test]
fn permission_commands_report_daemon_failure_without_success_receipts() {
    for arguments in [
        vec!["permission", "status"],
        vec!["permission", "list"],
        vec!["permission", "approve", "pending"],
        vec!["permission", "approve", "pending", "--remember"],
        vec!["permission", "deny", "pending"],
        vec!["permission", "resolve-batch", "batch", "--approve"],
        vec!["permission", "resolve-batch", "batch", "--deny"],
        vec![
            "permission",
            "add",
            "--agent",
            "build",
            "--category",
            "read",
            "--pattern",
            "*",
            "--action",
            "ask",
        ],
    ] {
        for json in [false, true] {
            let mut args = arguments.clone();
            if json {
                args.push("--json");
            }
            let output = run_cli_with_state(&args, true);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(output.status.code(), Some(1), "{args:?}: {stderr}");
            assert!(
                output.stdout.is_empty(),
                "unexpected success receipt: {args:?}"
            );
            assert!(stderr.starts_with("error:"), "{args:?}: {stderr}");
            assert!(!stderr.contains("panicked"), "{args:?}: {stderr}");
        }
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
fn scoped_catalog_commands_report_daemon_failure_without_success_output() {
    for arguments in [
        vec!["session", "create", "named", "--cwd", "workspace", "--json"],
        vec![
            "session",
            "list",
            "--cwd",
            "workspace",
            "--with-status",
            "--json",
        ],
        vec![
            "session",
            "refresh",
            "--cwd",
            "workspace",
            "--source",
            "native",
            "--json",
        ],
        vec!["session", "refresh", "--source", "native"],
    ] {
        let output = run_cli_with_fixture(&arguments, true, Stdio::piped(), |root| {
            std::fs::create_dir(root.join("workspace")).expect("workspace fixture");
        });
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{arguments:?}: {stderr}");
        assert!(
            output.stdout.is_empty(),
            "unexpected success output: {arguments:?}"
        );
        assert!(stderr.starts_with("error:"), "{stderr}");
        assert!(!stderr.contains("panicked"), "{stderr}");
    }
}

#[test]
fn catalog_status_requires_json_before_daemon_access() {
    let output = run_cli_with_state(&["session", "list", "--with-status"], true);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(output.stdout.is_empty());
    assert_eq!(
        stderr,
        "error: one or more required arguments were not provided\n"
    );
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
fn invocation_input_process_rejects_invalid_envelopes_before_daemon_access() {
    for json in [false, true] {
        for (contents, diagnostic) in [
            (
                br#"{"schema_version":"private-sentinel"}"#.to_vec(),
                "invalid invocation input envelope",
            ),
            (b"{} {}".to_vec(), "trailing characters"),
            (
                vec![b' '; 256 * 1024 + 1],
                "interaction JSON exceeds 262144 bytes",
            ),
        ] {
            let mut arguments = vec![
                "interaction",
                "input",
                "00000000-0000-4000-8000-000000000001",
                "--payload",
                "payload.json",
            ];
            if json {
                arguments.push("--json");
            }
            let output = run_cli_with_fixture(&arguments, true, Stdio::piped(), |root| {
                std::fs::write(root.join("payload.json"), contents).unwrap();
            });
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(output.status.code(), Some(2), "{stderr}");
            assert!(output.stdout.is_empty());
            assert!(stderr.contains(diagnostic), "{stderr}");
            assert!(!stderr.contains("private-sentinel"), "{stderr}");
            assert!(!stderr.contains("panicked"), "{stderr}");
        }
    }
}

#[test]
fn invocation_input_process_stdin_preserves_bounds_and_envelope_validation() {
    use std::io::{Seek as _, Write as _};
    let mut valid = br#"{
        "invocation_id":"call", "input_id":"input", "producer_id":"plugin",
        "schema":"plugin.input", "schema_version":1, "payload":"private-sentinel"
    }"#
    .to_vec();
    valid.resize(256 * 1024, b' ');
    for (contents, code, diagnostic) in [
        (valid, 1, "error:"),
        (
            vec![b' '; 256 * 1024 + 1],
            2,
            "interaction JSON exceeds 262144 bytes",
        ),
        (
            br#"{"schema_version":"private-sentinel"}"#.to_vec(),
            2,
            "invalid invocation input envelope",
        ),
    ] {
        let mut input = tempfile::tempfile().unwrap();
        input.write_all(&contents).unwrap();
        input.rewind().unwrap();
        let output = run_cli_with_stdio(
            &[
                "interaction",
                "input",
                "00000000-0000-4000-8000-000000000001",
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
        assert!(stderr.contains(diagnostic), "{stderr}");
        assert!(!stderr.contains("private-sentinel"), "{stderr}");
        assert!(!stderr.contains("panicked"), "{stderr}");
    }
}

#[test]
fn invocation_input_process_reports_daemon_failure_without_acceptance() {
    for json in [false, true] {
        let mut arguments = vec![
            "interaction",
            "input",
            "00000000-0000-4000-8000-000000000001",
            "--payload",
            "payload.json",
        ];
        if json {
            arguments.push("--json");
        }
        let output = run_cli_with_fixture(&arguments, true, Stdio::piped(), |root| {
            std::fs::write(
                root.join("payload.json"),
                br#"{
                "invocation_id":"call", "input_id":"input", "producer_id":"plugin",
                "schema":"plugin.input", "schema_version":1, "payload":"private-sentinel"
            }"#,
            )
            .unwrap();
        });
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{stderr}");
        assert!(output.stdout.is_empty());
        assert!(stderr.starts_with("error:"), "{stderr}");
        assert!(
            !stderr.contains("invalid invocation input envelope"),
            "{stderr}"
        );
        assert!(!stderr.contains("private-sentinel"), "{stderr}");
        assert!(!stderr.contains("panicked"), "{stderr}");
    }
}

#[test]
fn artifact_range_process_enforces_bounds_before_daemon_access() {
    for length in ["0", "1048577", "4294967295"] {
        let output = run_cli_with_state(
            &[
                "session",
                "artifact-range",
                "00000000-0000-4000-8000-000000000001",
                "artifact",
                "reference",
                "--length",
                length,
            ],
            true,
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{stderr}");
        assert!(output.stdout.is_empty());
        assert!(stderr.contains("invalid value"), "{stderr}");
        assert!(!stderr.contains("panicked"), "{stderr}");
    }
    for length in ["1", "1048576"] {
        let output = run_cli_with_state(
            &[
                "session",
                "artifact-range",
                "00000000-0000-4000-8000-000000000001",
                "artifact",
                "reference",
                "--offset",
                "18446744073709551615",
                "--length",
                length,
            ],
            true,
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{stderr}");
        assert!(output.stdout.is_empty());
        assert!(!stderr.contains("panicked"), "{stderr}");
    }
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
abi_version = 4
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
                manifest.replace("abi_version = 4", "abi_version = 65535"),
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

fn metrics_plugin_fixture(root: &std::path::Path) {
    let library = std::env::var_os("BCODE_METRICS_PLUGIN_TEST_LIBRARY")
        .expect("set BCODE_METRICS_PLUGIN_TEST_LIBRARY to the built metrics plugin");
    let library = std::fs::canonicalize(library).expect("built metrics plugin library");
    let plugin_root = root.join("test-plugins");
    std::fs::create_dir(&plugin_root).unwrap();
    std::fs::write(
        root.join("bcode.toml"),
        "[plugins]\nenabled = [\"bcode.metrics\"]\n",
    )
    .unwrap();
    let manifest = include_str!("../../../plugins/metrics-plugin/bcode-plugin.toml");
    std::fs::write(plugin_root.join("bcode-plugin.toml"), manifest).unwrap();
    std::fs::copy(library, plugin_root.join("libbcode_metrics_plugin.dylib")).unwrap();
}

#[test]
#[ignore = "requires BCODE_METRICS_PLUGIN_TEST_LIBRARY pointing to a built metrics plugin"]
fn plugin_service_errors_return_failure_with_response_envelope() {
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
            let output = run_cli_with_fixture(&args, false, Stdio::piped(), metrics_plugin_fixture);
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
#[ignore = "requires BCODE_METRICS_PLUGIN_TEST_LIBRARY pointing to a built metrics plugin"]
fn plugin_service_success_preserves_payload_and_zero_exit() {
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
                "invoke",
                "--root",
                "test-plugins",
                r#"{"command_id":"metrics.open_dashboard"}"#,
            ]);
            if json {
                args.push("--json");
            }
            let output = run_cli_with_fixture(&args, false, Stdio::piped(), metrics_plugin_fixture);
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(output.stderr.is_empty());
            let payload = if json {
                let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                assert!(envelope["error"].is_null());
                serde_json::from_value::<Vec<u8>>(envelope["payload"].clone()).unwrap()
            } else {
                output.stdout
            };
            let response: serde_json::Value = serde_json::from_slice(&payload).unwrap();
            assert_eq!(response["success"], true);
            assert_eq!(response["message"], "Opening metrics dashboard");
            assert_eq!(response["effects"].as_array().unwrap().len(), 1);

            let (reader, writer) = std::io::pipe().expect("output pipe");
            drop(reader);
            let output =
                run_cli_with_fixture(&args, false, Stdio::from(writer), metrics_plugin_fixture);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(output.status.code(), Some(1), "{stderr}");
            assert!(stderr.starts_with("error: I/O error:"), "{stderr}");
            assert!(!stderr.contains("panicked"), "{stderr}");
        }
    }
}

#[test]
fn plugin_service_help_discloses_failure_and_retry_semantics() {
    for operation in ["invoke", "call"] {
        let output = run_cli(&["plugin", operation, "--help"]);
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        let text = String::from_utf8(output.stdout).unwrap();
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            text.contains("service error is printed in the response envelope"),
            "{text}"
        );
        assert!(text.contains("exits with status 1"), "{text}");
        assert!(
            text.contains("does not imply rollback or that retrying is safe"),
            "{text}"
        );
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
fn invalid_prompt_sources_fail_before_daemon_access_without_disclosure() {
    for stdin in [false, true] {
        for (bytes, diagnostic) in [
            (Vec::new(), "prompt input must not be empty"),
            (
                b"private-prompt-marker\xff".to_vec(),
                "prompt input must be valid UTF-8",
            ),
            (
                vec![b'x'; 1024 * 1024 + 1],
                "prompt input exceeds 1048576 bytes",
            ),
        ] {
            let mut args = vec!["send", "00000000-0000-0000-0000-000000000001", "--json"];
            let input = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(input.path(), &bytes).unwrap();
            let output = if stdin {
                args.push("--stdin");
                run_cli_with_stdio(
                    &args,
                    true,
                    Stdio::piped(),
                    Stdio::from(input.reopen().unwrap()),
                    |_| {},
                )
            } else {
                args.extend(["--file", "prompt.txt"]);
                run_cli_with_fixture(&args, true, Stdio::piped(), |root| {
                    std::fs::write(root.join("prompt.txt"), &bytes).unwrap();
                })
            };
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(output.status.code(), Some(2), "{stderr}");
            assert!(output.stdout.is_empty());
            assert!(stderr.contains(diagnostic), "{stderr}");
            assert!(!stderr.contains("private-prompt-marker"), "{stderr}");
            assert!(!stderr.contains("panicked"), "{stderr}");
        }
    }
}

#[test]
fn follow_up_rejects_ignored_turn_admission_options() {
    for extra in [
        vec!["--producer", "test.producer"],
        vec!["--idempotency-key", "retry-key"],
        vec!["--background"],
    ] {
        let mut args = vec![
            "send",
            "00000000-0000-0000-0000-000000000001",
            "hello",
            "--follow-up",
            "--json",
        ];
        args.extend(extra);
        let output = run_cli_with_state(&args, true);
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("cannot be used with"), "{stderr}");
    }
    // The default producer must not conflict unless supplied explicitly.
    let output = run_cli_with_state(
        &[
            "send",
            "00000000-0000-0000-0000-000000000001",
            "hello",
            "--follow-up",
            "--json",
        ],
        true,
    );
    assert_eq!(output.status.code(), Some(1), "{:?}", output.stderr);
    assert!(output.stdout.is_empty());
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
