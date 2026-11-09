//! Opt-in real-Codex CLI/daemon acceptance. Uses the installed `codex` executable with native
//! account custody in its existing `CODEX_HOME` and private Bcode state.
//!
//! `BCODE_NATIVE_CODEX_HOME` selects the native namespace (default `$CODEX_HOME`, then
//! `~/.codex`). Credentials are never copied into test state.
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![cfg(all(unix, feature = "app", feature = "static-bundled-codex-harness-plugin"))]

use serde_json::{Value, json};
use std::{
    io::{Read as _, Seek as _},
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const SESSION: &str = "5c4f0a1e-2b8d-4c7e-9f61-3a2d8e0b7c15";
const INSTANCE: &str = "live-codex";

struct LiveCli {
    root: tempfile::TempDir,
    instance: Value,
}

/// The installed `codex` on PATH (profile spelling kept) and its exact top-level version.
fn installed_codex() -> (PathBuf, String) {
    let path = std::env::var_os("PATH").expect("PATH required");
    let executable = std::env::split_paths(&path)
        .map(|directory| directory.join("codex"))
        .find(|candidate| candidate.is_absolute() && candidate.is_file())
        .expect("installed codex on PATH required");
    let output = Command::new(&executable)
        .arg("--version")
        .stderr(Stdio::null())
        .output()
        .expect("native version probe failed");
    assert!(output.status.success(), "native version probe rejected");
    let version = String::from_utf8(output.stdout)
        .unwrap()
        .trim()
        .strip_prefix("codex-cli ")
        .expect("unexpected native version output")
        .to_owned();
    (executable, version)
}

impl LiveCli {
    fn new() -> Self {
        let (executable, version) = installed_codex();
        let root = tempfile::tempdir().unwrap();
        let namespace = std::env::var_os("BCODE_NATIVE_CODEX_HOME")
            .or_else(|| std::env::var_os("CODEX_HOME"))
            .map_or_else(
                || PathBuf::from(std::env::var_os("HOME").unwrap()).join(".codex"),
                PathBuf::from,
            );
        assert!(namespace.is_absolute() && namespace.is_dir());
        let quoted_home = serde_json::to_string(&namespace).unwrap();
        let quoted_executable = serde_json::to_string(&executable).unwrap();
        std::fs::write(
            root.path().join("live.toml"),
            format!(
                "[plugins]\ndefault = \"none\"\nenabled = [\"bcode.codex-harness\"]\n\
                 [plugins.config.\"bcode.codex-harness\".instances.{INSTANCE}]\n\
                 executable = {quoted_executable}\ncodex_home = {quoted_home}\n\
                 exact_version = {version:?}\n"
            ),
        )
        .unwrap();
        let instance = json!({"instance_id":INSTANCE,"plugin_id":"bcode.codex-harness",
            "executable":executable,"config_namespace":namespace,"auth":{
                "provider_id":"bcode.codex","profile":"native",
                "credential_namespace":namespace,"custody":"native_managed"}});
        Self { root, instance }
    }

    fn run(&self, args: &[&str]) -> String {
        let mut out = tempfile::tempfile().unwrap();
        let mut err = tempfile::tempfile().unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_bcode"));
        command.env_clear();
        for key in [
            "HOME",
            "USER",
            "PATH",
            "LANG",
            "SSL_CERT_FILE",
            "NIX_SSL_CERT_FILE",
        ] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        let mut child = command
            .env("RUST_LOG", "bcode_server::harness=warn")
            .env("BCODE_CONFIG", self.root.path().join("live.toml"))
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_DATA_HOME", self.root.path().join("data"))
            .arg("--state-root")
            .arg(self.root.path().join("state"))
            .args(args)
            .current_dir(self.root.path())
            .stdin(Stdio::null())
            .stdout(Stdio::from(out.try_clone().unwrap()))
            .stderr(Stdio::from(err.try_clone().unwrap()))
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(90);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline
                || out.metadata().unwrap().len() > 1_048_576
                || err.metadata().unwrap().len() > 1_048_576
            {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("live CLI exceeded time/output bound");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        if !status.success() {
            err.rewind().unwrap();
            let mut diagnostic = String::new();
            err.read_to_string(&mut diagnostic).unwrap();
            panic!(
                "live CLI failed: {args:?}; status={status}; stderr={}; test_root={}",
                diagnostic.lines().last().unwrap_or_default(),
                self.root.path().display()
            );
        }
        out.rewind().unwrap();
        let mut text = String::new();
        out.read_to_string(&mut text).unwrap();
        text
    }

    fn call(&self, command: &str, request_id: &str, kind: &Value) -> Value {
        let path = self.root.path().join("request.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({
            "session_id":SESSION,"parent_session_id":null,"assistance_destination_session_id":null,
            "harness":{"schema_version":1,"request_id":request_id,
                "instance":self.instance,"kind":kind}}))
            .unwrap(),
        )
        .unwrap();
        let response: Value = serde_json::from_str(&self.run(&[
            "harness",
            command,
            "--request",
            path.to_str().unwrap(),
        ]))
        .unwrap();
        assert_ne!(
            response["kind"]["kind"], "failure",
            "live harness returned failure: {response}"
        );
        response["kind"]["response"].clone()
    }

    fn application(&self, command: &str, request: &Value) -> Value {
        let path = self.root.path().join("application.json");
        std::fs::write(&path, serde_json::to_vec(request).unwrap()).unwrap();
        serde_json::from_str(&self.run(&["harness", command, "--request", path.to_str().unwrap()]))
            .unwrap()
    }
}

impl Drop for LiveCli {
    fn drop(&mut self) {
        // Stop only the daemon with this test's exact artifact and private state root.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.run(&["server", "stop", "--force", "--yes"])
        }));
        if result.is_err() {
            eprintln!("isolated live daemon cleanup failed");
        }
    }
}

/// Submit one next-turn input against `target` at `generation`; returns the attempt id.
fn submit(cli: &LiveCli, target: &Value, generation: u64, serial: u8, text: &str) -> String {
    let submission = format!("codex-journey-submission-{serial}");
    let attempt = format!("00000000-0000-4000-a000-{serial:012}");
    let receipt = cli.call(
        "send",
        &submission,
        &json!({"operation":"submit","request":{
        "target":target,"submission":{"submission_id":submission,"attempt_id":attempt,
        "session_id":SESSION,"generation":generation,"text":text,
        "placement":{"kind":"next_turn"}}}}),
    );
    assert_eq!(receipt["submission_id"], submission);
    assert_eq!(receipt["state"], "native_accepted", "{receipt}");
    attempt
}

/// What one observed native turn produced.
#[derive(Default)]
struct Turn {
    text: String,
    tools: Vec<String>,
    tool_results: usize,
    approvals: u8,
    model: Option<String>,
    output: Option<String>,
}

/// Observe until the exact attempt's foreground terminal arrives, approving native requests.
fn drive_approving(
    cli: &LiveCli,
    target: &Value,
    after: &mut Option<String>,
    attempt: &str,
) -> Turn {
    let deadline = Instant::now() + Duration::from_secs(240);
    let mut turn = Turn::default();
    let mut terminal = false;
    loop {
        assert!(
            Instant::now() < deadline,
            "live journey turn did not finish"
        );
        let page = cli.call(
            "observe",
            "codex-journey-observe",
            &json!({"operation":"observe","request":{
            "target":target,"after":after,"limit":64,"wait_millis":5000}}),
        );
        // Empty pages intentionally reset the transport cursor. Never retain an older cursor.
        *after = page["next_after"].as_str().map(str::to_owned);
        for observation in page["observations"].as_array().unwrap() {
            let kind = &observation["kind"];
            match kind["kind"].as_str().unwrap() {
                "text" => {
                    assert!(kind["segment_id"].is_string(), "segmented text: {kind}");
                    turn.text.push_str(kind["text"].as_str().unwrap());
                }
                "tool" => turn.tools.push(kind["name"].as_str().unwrap().to_owned()),
                "tool_result" => turn.tool_results += 1,
                "usage" => turn.model = kind["model"].as_str().map(str::to_owned),
                "diagnostic" => panic!("native diagnostic: {kind}"),
                "interaction" if kind["interaction"]["kind"] == "approval" => {
                    turn.approvals += 1;
                    let interaction = &kind["interaction"];
                    assert!(interaction["tool_name"].is_string(), "{interaction}");
                    let response_id =
                        format!("codex-journey-approval-{attempt}-{}", turn.approvals);
                    cli.call(
                        "respond",
                        &response_id,
                        &json!({"operation":"respond","request":{"target":target,
                        "response":{"response_id":response_id,
                            "request_id":interaction["request_id"],
                            "session_id":interaction["session_id"],
                            "generation":interaction["generation"],
                            "value":{"kind":"approval","approved":true}}}}),
                    );
                }
                "execution"
                    if kind["event"]["event"] == "submission_terminal"
                        && kind["event"]["attempt"]["attempt_id"] == attempt =>
                {
                    assert_eq!(kind["event"]["disposition"], "completed");
                    turn.output = kind["event"]["output"].as_str().map(str::to_owned);
                    terminal = true;
                }
                _ => {}
            }
        }
        if terminal {
            return turn;
        }
    }
}

/// The interactive journey on the real native executor through the daemon application:
/// selected-model open, a turn with a native approval, a later turn after observed completion,
/// a mid-session model switch, a graceful daemon restart, recovery of the same native thread,
/// and context kept across it.
#[test]
#[ignore = "uses the installed Codex CLI and its native account; explicit live-run authorization required"]
fn native_codex_journey_approves_switches_model_and_resumes_after_restart() {
    let cli = LiveCli::new();
    cli.run(&["server", "start"]);
    let catalog = cli.application(
        "models",
        &json!({"request_id":"codex-catalog", "instances":[cli.instance], "provider_plugin_id":null}),
    );
    let models = catalog["models"].as_array().unwrap();
    assert!(!models.is_empty(), "no native-advertised models: {catalog}");
    // Prefer stable, widely available keys; any advertised model works natively.
    let pick = |preferred: &str, exclude: Option<&str>| {
        models
            .iter()
            .find(|entry| entry["native_model_id"] == preferred)
            .or_else(|| {
                models
                    .iter()
                    .find(|entry| entry["native_model_id"].as_str() != exclude)
            })
            .cloned()
    };
    let model = pick("gpt-5.5", None).unwrap();
    let selection = json!({"model_id":model["model"]["model_id"],
        "route":model["route"], "account_reference":model["account_reference"]});
    let started = cli.application(
        "start-selected",
        &json!({
            "selection":selection,"instance":cli.instance,
            "open":{"open_id":"codex-journey-open","session_id":SESSION,"generation":1,
                "working_directory":cli.root.path(),"resume":null},"parent_session_id":null,
        }),
    );
    assert_eq!(started["response"]["kind"]["kind"], "selected", "{started}");
    let target = started["response"]["kind"]["response"]["target"].clone();
    assert_eq!(target["state"], "confirmed");
    let mut after = None;

    let attempt = submit(
        &cli,
        &target,
        1,
        1,
        "Remember the code word MANGO. Create a file named notes.txt containing hi using a \
         shell command, then reply with DONE.",
    );
    let first = drive_approving(&cli, &target, &mut after, &attempt);
    assert!(first.approvals >= 1, "native approval was requested");
    assert!(!first.tools.is_empty() && first.tool_results >= 1);
    assert!(first.output.is_some(), "foreground output");
    assert!(cli.root.path().join("notes.txt").exists());

    // A later NextTurn is admitted once the previous completion was observed.
    let attempt = submit(&cli, &target, 1, 2, "Reply with the single word OK.");
    let second = drive_approving(&cli, &target, &mut after, &attempt);
    assert_ne!(second.text, "");

    let switched_to = pick("gpt-5.6-luna", model["native_model_id"].as_str())
        .and_then(|entry| entry["native_model_id"].as_str().map(str::to_owned))
        .filter(|id| Some(id.as_str()) != model["native_model_id"].as_str())
        .unwrap_or_default();
    if !switched_to.is_empty() {
        let switched = cli.call(
            "configure",
            "codex-journey-model",
            &json!({"operation":"configure","request":{"target":target,
                "mutation_id":"codex-journey-model",
                "setting":{"kind":"model","model_id":switched_to}}}),
        );
        assert_eq!(switched["mutation_id"], "codex-journey-model");
        let attempt = submit(&cli, &target, 1, 3, "Reply with the single word OK.");
        let third = drive_approving(&cli, &target, &mut after, &attempt);
        assert_eq!(third.model.as_deref(), Some(switched_to.as_str()));
    }

    let binding = restart_and_recover(&cli, &target);
    let resumed = json!({"state":"confirmed","session":binding});
    let mut resumed_after = None;
    let attempt = submit(
        &cli,
        &resumed,
        2,
        4,
        "What code word did I ask you to remember? Answer with the word only.",
    );
    let resumed_turn = drive_approving(&cli, &resumed, &mut resumed_after, &attempt);
    assert!(
        resumed_turn.text.contains("MANGO"),
        "resumed context: {}",
        resumed_turn.text
    );
}

/// Gracefully restart the daemon and recover the same native thread at generation 2.
fn restart_and_recover(cli: &LiveCli, target: &Value) -> Value {
    cli.run(&["server", "stop"]);
    cli.run(&["server", "start"]);
    let recovered: Value = serde_json::from_str(&cli.run(&[
        "harness",
        "recover",
        SESSION,
        "--instance",
        INSTANCE,
        "--generation",
        "1",
        "--recovery-id",
        "codex-journey-recover",
    ]))
    .unwrap();
    assert_eq!(recovered["kind"]["kind"], "opened", "{recovered}");
    let binding = recovered["kind"]["response"].clone();
    assert_eq!(binding["generation"], 2);
    assert_eq!(
        binding["native_session_id"], target["session"]["native_session_id"],
        "recovery resumes the exact native thread"
    );
    binding
}
