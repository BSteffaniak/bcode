//! Opt-in real-Claude CLI/daemon acceptance. Uses native account custody and private Bcode state.
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![cfg(all(unix, feature = "app", feature = "static-bundled-claude-code-plugin"))]

use serde_json::{Value, json};
use std::{
    io::{Read as _, Seek as _},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const NATIVE: &str = "/nix/store/yzhsl1i8v8gypvdx2bigpysj58la5i8w-claude-code-2.1.308/bin/claude";
const SESSION: &str = "8a681342-579e-4d00-a93f-3d35da81b8d1";

struct LiveCli {
    root: tempfile::TempDir,
    instance: Value,
}

impl LiveCli {
    fn new() -> Self {
        verify_native();
        let root = tempfile::tempdir().unwrap();
        let namespace = std::env::var_os("BCODE_NATIVE_CLAUDE_CONFIG_DIR")
            .or_else(|| std::env::var_os("CLAUDE_CONFIG_DIR"))
            .map_or_else(
                || PathBuf::from(std::env::var_os("HOME").unwrap()).join(".claude"),
                PathBuf::from,
            );
        assert!(namespace.is_absolute() && namespace.is_dir());
        let quoted = serde_json::to_string(&namespace).unwrap();
        std::fs::write(
            root.path().join("live.toml"),
            format!(
                "[plugins]\ndefault = \"none\"\nenabled = [\"bcode.claude-code\"]\n\
             [plugins.config.\"bcode.claude-code\".instances.live-claude]\n\
             executable = {NATIVE:?}\nconfig_directory = {quoted}\n\
             exact_version = \"2.1.308 (Claude Code)\"\nverified_session_state = true\n\
             verified_native_auth_status = true\nsetting_sources = []\n"
            ),
        )
        .unwrap();
        let instance = json!({"instance_id":"live-claude","plugin_id":"bcode.claude-code",
            "executable":NATIVE,"config_namespace":namespace,"auth":{
                "provider_id":"claude-code","profile":"native-live",
                "credential_namespace":namespace,"custody":"native_managed"}});
        Self { root, instance }
    }

    fn run(&self, args: &[&str]) -> String {
        let mut out = tempfile::tempfile().unwrap();
        let mut err = tempfile::tempfile().unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_bcode"));
        command.env_clear();
        for key in ["HOME", "PATH", "LANG", "SSL_CERT_FILE", "NIX_SSL_CERT_FILE"] {
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
            let code = [
                "managed_session_conflict",
                "managed_session_storage_unavailable",
                "managed_session_stale_owner",
            ]
            .into_iter()
            .find(|code| diagnostic.contains(code))
            .unwrap_or("unclassified");
            panic!(
                "live CLI failed: {args:?}; status={status}; code={code}; test_root={}",
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
            "live harness returned failure"
        );
        response["kind"]["response"].clone()
    }

    fn application(&self, command: &str, request: &Value) -> Value {
        let path = self.root.path().join("application.json");
        std::fs::write(&path, serde_json::to_vec(request).unwrap()).unwrap();
        serde_json::from_str(&self.run(&["harness", command, "--request", path.to_str().unwrap()]))
            .unwrap()
    }

    fn history(&self) -> Vec<Value> {
        let mut observations = Vec::new();
        let mut after: Option<String> = None;
        for _ in 0..256 {
            let mut args = vec!["harness", "history", SESSION, "--limit", "128"];
            if let Some(cursor) = &after {
                args.extend(["--after", cursor]);
            }
            let page: Value = serde_json::from_str(&self.run(&args)).unwrap();
            observations.extend(page["observations"].as_array().unwrap().iter().cloned());
            let next = page["next_after"].as_str().map(str::to_owned);
            if next.is_none()
                || (next == after && page["observations"].as_array().unwrap().is_empty())
            {
                return observations;
            }
            assert_ne!(next, after, "nonempty history cursor did not advance");
            after = next;
        }
        panic!("live history exceeded bounded page budget");
    }
}

impl Drop for LiveCli {
    fn drop(&mut self) {
        // Stop only the daemon with this test's exact artifact and private state root.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.run(&["server", "stop", "--force", "--yes"])
        }));
        if std::thread::panicking() {
            for entry in std::fs::read_dir(self.root.path().join("state/logs")).unwrap() {
                let log = std::fs::read_to_string(entry.unwrap().path()).unwrap();
                for line in log.lines().filter(|line| {
                    line.contains("stage failed") || line.contains("commit rejected")
                }) {
                    eprintln!("{line}");
                }
            }
        }
        if result.is_err() {
            eprintln!("isolated live daemon cleanup failed");
        }
    }
}

fn verify_native() {
    for (path, expected) in [
        (
            PathBuf::from(NATIVE),
            "49ca742c79d844aea24b25190c765994bbcb7d63ddf4726e4c048e89946494b0",
        ),
        (
            Path::new(NATIVE).with_file_name(".claude-wrapped"),
            "0696f3eba9cc930f29dab7e6d4102b79b72d58c163c14415cfe72460575e355b",
        ),
    ] {
        let output = Command::new("sha256sum").arg(path).output().unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout)
                .unwrap()
                .split_whitespace()
                .next(),
            Some(expected)
        );
    }
}

fn observe_turn(cli: &LiveCli, target: &mut Value, after: &mut Option<String>, serial: u8) {
    let submission = format!("live-submission-{serial}");
    let attempt = format!("00000000-0000-4000-8000-{serial:012}");
    let expected = format!("BCODE_HOST_LIVE_OK_{serial}");
    let receipt = cli.call("send", &submission, &json!({"operation":"submit","request":{
        "target":target,"submission":{"submission_id":submission,"attempt_id":attempt,
        "session_id":SESSION,"generation":1,"text":format!("Reply with exactly {expected}. Do not use tools."),
        "placement":{"kind":"next_turn"}}}}));
    assert_eq!(receipt["submission_id"], submission);
    let deadline = Instant::now() + Duration::from_secs(180);
    let (mut text, mut result, mut terminal) = (false, false, false);
    loop {
        assert!(
            Instant::now() < deadline,
            "live turn did not complete authoritatively"
        );
        let page = cli.call(
            "observe",
            "live-observe",
            &json!({"operation":"observe","request":{
            "target":target,"after":after,"limit":64,"wait_millis":0}}),
        );
        // Empty pages intentionally reset the transport cursor. Never retain an older cursor.
        *after = page["next_after"].as_str().map(str::to_owned);
        for observation in page["observations"].as_array().unwrap() {
            let kind = &observation["kind"];
            match kind["kind"].as_str().unwrap() {
                "binding_confirmed" => {
                    *target = json!({"state":"confirmed","session":kind["binding"]});
                }
                "text" => text |= kind["text"] == expected,
                "submission_result_observed" => {
                    assert_eq!(kind["submission_id"], submission);
                    assert_eq!(kind["attempt_id"], attempt);
                    assert_eq!(kind["is_error"], false);
                    result = true;
                }
                "execution" if kind["event"]["event"] == "submission_terminal" => {
                    assert_eq!(kind["event"]["attempt"]["submission_id"], submission);
                    assert_eq!(kind["event"]["attempt"]["attempt_id"], attempt);
                    assert_eq!(kind["event"]["disposition"], "completed");
                    assert_eq!(kind["event"]["output"], expected);
                    terminal = true;
                }
                _ => {}
            }
        }
        if text && result && terminal && page["status"]["activity"] == "settled" {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    eprintln!("live host turn={serial} correlated_result=true completed=true settled=true");
}

#[test]
#[ignore = "uses existing native Claude subscription; explicit live-run authorization required"]
fn native_discovered_model_selected_before_two_turns() {
    let cli = LiveCli::new();
    let artifact = cli.run(&["artifact-id"]);
    eprintln!("selected-model test artifact: {}", artifact.trim());
    cli.run(&["server", "start"]);
    assert!(cli.run(&["server", "status"]).contains(artifact.trim()));
    let catalog = cli.application("models", &json!({
        "request_id":"live-model-catalog", "instances":[cli.instance], "provider_plugin_id":null,
    }));
    assert_eq!(catalog["request_id"], "live-model-catalog");
    assert!(
        catalog["issues"].as_array().unwrap().is_empty(),
        "native discovery was incomplete"
    );
    let models = catalog["models"].as_array().unwrap();
    assert!(!models.is_empty(), "no native-advertised models");
    let model = models
        .iter()
        .find(|entry| entry["native_model_id"] == "haiku")
        .unwrap_or(&models[0]);
    assert_eq!(model["route"]["kind"], "managed_harness");
    let selection = json!({"model_id":model["model"]["model_id"],
        "route":model["route"], "account_reference":model["account_reference"]});
    let started = cli.application(
        "start-selected",
        &json!({
            "selection":selection,"instance":cli.instance,
            "open":{"open_id":"live-selected-open","session_id":SESSION,"generation":1,
                "working_directory":cli.root.path(),"resume":null},"parent_session_id":null,
        }),
    );
    assert_eq!(started["selection"]["requested"], selection);
    assert_eq!(started["response"]["kind"]["kind"], "selected");
    assert_eq!(
        started["response"]["kind"]["response"]["evidence"]["requested_model_id"],
        model["native_model_id"]
    );
    let mut target = started["response"]["kind"]["response"]["target"].clone();
    assert_eq!(target["state"], "prepared");
    let mut after = None;
    observe_turn(&cli, &mut target, &mut after, 1);
    observe_turn(&cli, &mut target, &mut after, 2);
    assert!(
        cli.history()
            .iter()
            .any(|observation| observation["kind"]["kind"] == "result"
                && observation["kind"]["text"] == "BCODE_HOST_LIVE_OK_2")
    );
}

#[test]
#[ignore = "uses existing native Claude subscription; explicit live-run authorization required"]
fn native_cli_two_turns_and_offline_canonical_history() {
    let cli = LiveCli::new();
    let artifact = cli.run(&["artifact-id"]);
    eprintln!("test artifact: {}", artifact.trim());
    cli.run(&["server", "start"]);
    let status = cli.run(&["server", "status"]);
    assert!(
        status.contains(artifact.trim()),
        "client/daemon artifact mismatch"
    );
    let prepared = cli.call(
        "start",
        "live-open",
        &json!({"operation":"open","request":{
        "open_id":"live-open","session_id":SESSION,"generation":1,
        "working_directory":cli.root.path(),"resume":null}}),
    );
    assert!(prepared.get("native_session_id").is_none());
    let mut target = json!({"state":"prepared","session":prepared});
    let mut after = None;
    observe_turn(&cli, &mut target, &mut after, 1);
    observe_turn(&cli, &mut target, &mut after, 2);
    let before = cli.history();
    assert!(
        before
            .iter()
            .any(|observation| observation["kind"]["kind"] == "result"
                && observation["kind"]["text"] == "BCODE_HOST_LIVE_OK_2")
    );
    cli.run(&["server", "stop", "--force", "--yes"]);
    std::fs::write(
        cli.root.path().join("live.toml"),
        "[plugins]\ndefault = \"none\"\nenabled = []\n",
    )
    .unwrap();
    let offline = cli.history();
    assert_eq!(
        before, offline,
        "history must survive daemon restart with the adapter disabled"
    );
}
