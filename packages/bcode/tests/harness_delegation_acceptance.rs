//! Offline delegation acceptance: an ordinary Bcode parent session, driven by the scripted
//! fake model provider, delegates to the fake native Claude executable through the simple
//! `harness.delegate` / `harness.result` / `harness.continue` tool shapes.
//!
//! No real model provider, native agent, account or network is used.
#![cfg(all(
    unix,
    feature = "app",
    feature = "static-bundled-claude-code-plugin",
    feature = "static-bundled-harness-plugin",
    feature = "static-bundled-fake-provider-plugin",
    feature = "static-bundled-default-agents-plugin"
))]
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

#[path = "fixtures/harness/python.rs"]
mod fixture_python;

use serde_json::{Value, json};
use std::fmt::Write as _;
use std::io::{Read as _, Seek as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const OUTPUT_BUDGET: u64 = 4 * 1024 * 1024;

struct Fixture {
    root: tempfile::TempDir,
    config: PathBuf,
    native: PathBuf,
}

struct Reply {
    ok: bool,
    stdout: String,
    stderr: String,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let native = root.path().join("native");
        std::fs::create_dir(&native).unwrap();
        std::fs::write(native.join("scenario"), "multi_turn").unwrap();
        std::fs::write(native.join("application-idle-seconds"), "120").unwrap();
        let executable = root.path().join("offline-claude");
        let python = fixture_python::python();
        let interpreter = python.to_str().expect("UTF-8 Python path");
        assert!(!interpreter.chars().any(char::is_whitespace) && interpreter.len() < 240);
        let source = include_str!("fixtures/harness/claude_native.py");
        let (_, body) = source.split_once('\n').unwrap();
        std::fs::write(&executable, format!("#!{interpreter}\n{body}")).unwrap();
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let quoted = |path: &Path| serde_json::to_string(path.to_str().unwrap()).unwrap();
        let (executable, native_quoted) = (quoted(&executable), quoted(&native));
        // The parent uses only the offline scripted fake provider. The generic harness
        // catalog names the fake native executable as the default delegation instance.
        let config = format!(
            r#"[plugins]
default = "none"
enabled = ["bcode.claude-code", "bcode.harness", "bcode.fake-provider", "bcode.default-agents"]

[model]
provider_plugin_id = "bcode.fake-provider"
model_id = "fake-echo"

[model.prompt_cache]
mode = "off"

[model.settings]
fake_prompt_tool_directives = "true"

[agent.build.permission.tools]
"harness.delegate" = "allow"
"harness.continue" = "allow"

[plugins.config."bcode.claude-code".instances.offline-claude]
executable = {executable}
config_directory = {native_quoted}
exact_version = "2.1.308 (Claude Code)"
verified_session_state = false
verified_native_auth = true
setting_sources = []

[plugins.config."bcode.harness"]
enabled = true
default_instance = "offline-claude"

[plugins.config."bcode.harness".instances.offline-claude]
enabled = true

[plugins.config."bcode.harness".instances.offline-claude.instance]
instance_id = "offline-claude"
plugin_id = "bcode.claude-code"
executable = {executable}
config_namespace = {native_quoted}

[plugins.config."bcode.harness".instances.offline-claude.instance.auth]
provider_id = "claude-code"
profile = "offline-fixture"
credential_namespace = {native_quoted}
custody = "native_managed"
"#
        );
        let path = root.path().join("bcode.toml");
        std::fs::write(&path, config).unwrap();
        Self {
            root,
            config: path,
            native,
        }
    }

    fn run(&self, args: &[&str]) -> Reply {
        let mut out = tempfile::tempfile().unwrap();
        let mut err = tempfile::tempfile().unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_bcode"))
            .env_clear()
            .env("HOME", self.root.path())
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_DATA_HOME", self.root.path().join("data"))
            .env("PATH", "/usr/bin:/bin")
            .env("BCODE_CONFIG", &self.config)
            .env("BCODE_DISABLE_REMOTE_MODEL_CATALOG", "1")
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
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("bounded CLI operation timed out: {:?}", args.first());
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        out.rewind().unwrap();
        err.rewind().unwrap();
        let mut stdout = String::new();
        let mut stderr = String::new();
        out.take(OUTPUT_BUDGET).read_to_string(&mut stdout).unwrap();
        err.take(OUTPUT_BUDGET).read_to_string(&mut stderr).unwrap();
        Reply {
            ok: status.success(),
            stdout,
            stderr,
        }
    }

    fn json(&self, args: &[&str]) -> Value {
        let reply = self.run(args);
        assert!(
            reply.ok,
            "{args:?} failed: {}{}",
            reply.stdout, reply.stderr
        );
        serde_json::from_str(&reply.stdout).unwrap()
    }

    /// Run one parent turn whose scripted fake model calls exactly `tool` with `arguments`.
    fn tool_turn(&self, parent: &str, tool: &str, arguments: &Value) -> Value {
        let prompt = self.root.path().join("prompt.txt");
        std::fs::write(&prompt, format!("tool-call {tool} {arguments}")).unwrap();
        let reply = self.run(&[
            "send",
            parent,
            "--file",
            prompt.to_str().unwrap(),
            "--wait",
            "--timeout-secs",
            "60",
            "--json",
        ]);
        if !reply.ok {
            let history = self.run(&["session", "history", parent, "--limit", "50", "--json"]);
            let mut logs = String::new();
            collect_logs(self.root.path(), &mut logs, 0);
            panic!(
                "parent turn calling {tool} failed: {}{}\nhistory: {}\nlogs: {logs}",
                reply.stdout, reply.stderr, history.stdout
            );
        }
        self.last_tool_output(parent, tool)
    }

    /// Latest JSON tool output recorded in the parent's canonical history for `tool`.
    fn last_tool_output(&self, parent: &str, tool: &str) -> Value {
        let history = self.json(&["session", "history", parent, "--limit", "1000", "--json"]);
        let mut found = None;
        collect_outputs(&history, tool, &mut found);
        found.unwrap_or_else(|| panic!("no {tool} output in parent history: {history}"))
    }

    fn poll_until_settled(&self, parent: &str, child: &str, after: Option<&str>) -> Value {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let mut arguments = json!({"session_id": child});
            if let Some(after) = after {
                arguments["after"] = json!(after);
            }
            let summary = self.tool_turn(parent, "harness.result", &arguments);
            assert_eq!(summary["session_id"], child, "{summary}");
            assert_ne!(summary["activity"], "failed", "{summary}");
            if summary["activity"] == "settled" {
                return summary;
            }
            if Instant::now() >= deadline {
                let mut logs = String::new();
                collect_logs(self.root.path(), &mut logs, 0);
                let history = self.run(&["harness", "history", child, "--limit", "32"]);
                panic!(
                    "delegated child never settled; last summary={summary}\nchild history: {}\nlogs: {logs}\nnative: {:?}",
                    history.stdout,
                    std::fs::read_dir(&self.native)
                        .unwrap()
                        .flatten()
                        .map(|entry| entry.file_name())
                        .collect::<Vec<_>>()
                );
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let child = Command::new(env!("CARGO_BIN_EXE_bcode"))
            .env_clear()
            .env("HOME", self.root.path())
            .env("BCODE_CONFIG", &self.config)
            .arg("--state-root")
            .arg(self.root.path().join("state"))
            .args(["server", "stop", "--force", "--yes"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let Ok(mut child) = child else {
            return;
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                }
            }
        }
    }
}

/// Bounded diagnostic tails of daemon log files under the private fixture root.
fn collect_logs(directory: &Path, logs: &mut String, depth: usize) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && depth < 6 {
            collect_logs(&path, logs, depth + 1);
        } else if path.extension().is_some_and(|extension| extension == "log")
            && let Ok(text) = std::fs::read_to_string(&path)
        {
            let start = text.len().saturating_sub(16 * 1024);
            let start = (start..text.len())
                .find(|index| text.is_char_boundary(*index))
                .unwrap_or(text.len());
            let _ = write!(logs, "\n== {}\n{}", path.display(), &text[start..]);
        }
    }
}

/// Walk history in order, keeping the last string that decodes as this tool's JSON output.
fn collect_outputs(value: &Value, tool: &str, found: &mut Option<Value>) {
    match value {
        Value::String(text) => {
            if let Ok(decoded @ Value::Object(_)) = serde_json::from_str::<Value>(text) {
                // Result summaries carry `activity`; mutation outcomes carry `status`.
                let matches = if tool == "harness.result" {
                    decoded.get("activity").is_some()
                } else {
                    decoded.get("activity").is_none()
                        && decoded.get("status").is_some()
                        && decoded.get("session_id").is_some()
                };
                if matches {
                    *found = Some(decoded);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_outputs(item, tool, found);
            }
        }
        Value::Object(map) => {
            for item in map.values() {
                collect_outputs(item, tool, found);
            }
        }
        _ => {}
    }
}

#[test]
fn parent_session_delegates_polls_and_continues_through_simple_tool_shapes() {
    let fixture = Fixture::new();
    let started = fixture.run(&["server", "start"]);
    assert!(started.ok, "daemon start failed: {}", started.stderr);
    let parent = fixture.json(&["session", "create", "delegating parent", "--json"]);
    let parent = parent["id"].as_str().unwrap().to_owned();

    // The model supplies only a prompt; the plugin resolves the default instance.
    let delegated = fixture.tool_turn(&parent, "harness.delegate", &json!({"prompt": "turn-1"}));
    assert_eq!(delegated["status"], "submitted", "{delegated}");
    assert_eq!(delegated["instance_id"], "offline-claude");
    let child = delegated["session_id"].as_str().unwrap().to_owned();

    // The child is an ordinary canonical child of the calling session.
    let children = fixture.json(&["session", "children", &parent, "--limit", "4", "--json"]);
    let items = children["children"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{children}");
    assert_eq!(items[0]["id"], child);
    assert_eq!(items[0]["parent_id"], parent);

    let first = fixture.poll_until_settled(&parent, &child, None);
    assert!(
        first["latest_result"]
            .as_str()
            .is_some_and(|text| text.contains("reply turn-1")),
        "{first}"
    );
    assert_eq!(first["pending"], json!([]));
    assert!(fixture.native.join("turn-1.effect").exists());
    let cursor = first["cursor"].as_str().unwrap().to_owned();

    let continued = fixture.tool_turn(
        &parent,
        "harness.continue",
        &json!({"session_id": child, "prompt": "turn-2"}),
    );
    assert_eq!(continued["status"], "submitted", "{continued}");
    let second = fixture.poll_until_settled(&parent, &child, Some(&cursor));
    assert!(
        second["latest_result"]
            .as_str()
            .is_some_and(|text| text.contains("reply turn-2")),
        "{second}"
    );
    assert!(fixture.native.join("turn-2.effect").exists());

    // One native process served both turns; no input was replayed or rejected.
    assert!(fixture.native.join("launch-0").exists());
    assert!(!fixture.native.join("launch-1").exists());
    assert!(!fixture.native.join("turn-1.duplicate").exists());
    assert!(!fixture.native.join("turn-2.duplicate").exists());
    assert!(!fixture.native.join("unexpected-user.rejected").exists());
}
