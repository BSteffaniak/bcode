//! Offline native-process fixture checks, not proof of host lifecycle acceptance.
//! See fixtures/harness/README.md for the managed-session integration obligations.
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, sync_channel};
use std::time::{Duration, Instant};

#[path = "fixtures/harness/python.rs"]
mod fixture_python;

use serde_json::{Value, json};

const DEADLINE: Duration = Duration::from_secs(8);
const MAX_FRAME: u64 = 65_537;

struct NativeFake {
    child: Child,
    input: ChildStdin,
    frames: Receiver<Vec<u8>>,
}

impl NativeFake {
    fn start(scenario: &str, oracle: &Path) -> Self {
        let python = fixture_python::python();
        let child = Command::new(python)
            .arg(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/harness/fake_native.py"),
            )
            .args(["--scenario", scenario, "--oracle"])
            .arg(oracle)
            .env_clear()
            .env("HOME", oracle)
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .current_dir(oracle)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start the offline Python fixture (no native agent)");
        Self::from_child(child)
    }

    fn claude(scenario: &str, oracle: &Path) -> Self {
        std::fs::write(oracle.join("scenario"), scenario).unwrap();
        let python = fixture_python::python();
        let child = Command::new(python)
            .arg(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/harness/claude_native.py"),
            )
            .arg("--session-id=33333333-3333-4333-8333-333333333333")
            .env_clear()
            .env("CLAUDE_CONFIG_DIR", oracle)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        Self::from_child(child)
    }

    fn from_child(mut child: Child) -> Self {
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        // Drain concurrently; bounded frames and channel capacity prevent the
        // test infrastructure from hiding reader backpressure behind a deadlock.
        let (sender, frames) = sync_channel(128);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(output);
            for _ in 0..128 {
                let mut frame = Vec::new();
                let read = (&mut reader).take(MAX_FRAME).read_until(b'\n', &mut frame);
                match read {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if frame.len() >= usize::try_from(MAX_FRAME).unwrap()
                            || sender.send(frame).is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });
        Self {
            child,
            input,
            frames,
        }
    }

    fn send(&mut self, value: &Value) {
        serde_json::to_writer(&mut self.input, value).unwrap();
        self.input.write_all(b"\n").unwrap();
        self.input.flush().unwrap();
    }

    fn frame(&self) -> Vec<u8> {
        self.frames
            .recv_timeout(DEADLINE)
            .expect("bounded native frame")
    }

    fn event(&self) -> Value {
        serde_json::from_slice(&self.frame()).expect("valid native fixture JSON")
    }

    fn exit(&mut self) -> (i32, String) {
        let deadline = Instant::now() + DEADLINE;
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "offline fixture deadline exceeded"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        let mut error = String::new();
        self.child
            .stderr
            .take()
            .unwrap()
            .take(MAX_FRAME)
            .read_to_string(&mut error)
            .unwrap();
        (status.code().expect("fixture exits normally"), error)
    }
}

impl Drop for NativeFake {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

fn wait_for_gate(oracle: &Path, gate: &str) {
    let deadline = Instant::now() + DEADLINE;
    while !oracle.join(format!("{gate}.waiting")).exists() {
        assert!(Instant::now() < deadline, "fixture never reached its gate");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn release(oracle: &Path, gate: &str) {
    wait_for_gate(oracle, gate);
    std::fs::write(oracle.join(format!("{gate}.release")), b"release").unwrap();
}

#[test]
fn claude_initialize_has_no_identity_and_first_user_causes_native_confirmation() {
    let root = tempfile::tempdir().unwrap();
    let mut native = NativeFake::claude("question", root.path());
    native.send(&json!({"type":"control_request","request_id":"init-1",
        "request":{"subtype":"initialize","hooks":null}}));
    let initialized = native.event();
    assert_eq!(initialized["type"], "control_response");
    assert_eq!(initialized["response"]["request_id"], "init-1");
    assert_eq!(initialized["response"]["response"], json!({}));
    wait_for_gate(root.path(), "initialized");
    assert!(native.frames.try_recv().is_err());
    assert!(!root.path().join("submit.effect").exists());
    assert!(!root.path().join("binding-confirmed.emitted").exists());
    native.send(&json!({"type":"user","message":{"role":"user","content":"fixture-only"}}));
    let confirmed = native.event();
    assert_eq!(confirmed["type"], "system");
    assert_eq!(confirmed["subtype"], "init");
    assert_eq!(
        confirmed["session_id"],
        "33333333-3333-4333-8333-333333333333"
    );
    assert!(root.path().join("submit.effect").exists());
}

#[test]
fn claude_dummy_prompt_cannot_manufacture_confirmation() {
    let root = tempfile::tempdir().unwrap();
    let mut native = NativeFake::claude("question", root.path());
    native.send(&json!({"type":"control_request","request_id":"init-1",
        "request":{"subtype":"initialize"}}));
    assert_eq!(native.event()["response"]["response"], json!({}));
    native.send(&json!({"type":"user","message":{"role":"user","content":""}}));
    assert_eq!(native.exit().0, 65);
    assert!(root.path().join("unexpected-user.rejected").exists());
    assert!(!root.path().join("submit.effect").exists());
    assert!(!root.path().join("binding-confirmed.emitted").exists());
}

#[test]
fn pi_agent_end_is_observable_before_authoritative_settlement() {
    let root = tempfile::tempdir().unwrap();
    let mut native = NativeFake::start("pi_turn", root.path());
    native.send(&json!({"id":"retained-1","type":"prompt","message":"fixture-only"}));
    let receipt = native.event();
    assert_eq!(receipt["id"], "retained-1");
    assert_eq!(receipt["data"]["disposition"], "started");
    assert_eq!(native.event()["type"], "agent_start");
    assert_eq!(native.event()["type"], "agent_end");
    wait_for_gate(root.path(), "pi-settlement");
    assert!(native.frames.try_recv().is_err());
    release(root.path(), "pi-settlement");
    assert_eq!(
        native.event(),
        json!({"type":"agent_settled","aborted":false})
    );
    assert_eq!(native.exit(), (0, String::new()));
}

#[test]
fn pi_handled_prompt_does_not_invent_a_turn_or_settlement_event() {
    let root = tempfile::tempdir().unwrap();
    let mut native = NativeFake::start("pi_handled_without_turn", root.path());
    native.send(&json!({"id":"handled-1","type":"prompt","message":"fixture extension"}));
    assert_eq!(native.event()["data"]["disposition"], "handled");
    assert_eq!(native.exit(), (0, String::new()));
    assert!(native.frames.recv_timeout(DEADLINE).is_err());
    assert!(!root.path().join("pi-prompt.effect").exists());
}

#[test]
fn lost_native_ack_oracle_detects_even_a_differently_identified_replay() {
    for (scenario, command, effect, expected_exit) in [
        ("pi_lost_ack", "prompt", "pi-prompt", 81),
        ("pi_abort_lost_ack", "abort", "pi-abort", 82),
    ] {
        let root = tempfile::tempdir().unwrap();
        let mut first = NativeFake::start(scenario, root.path());
        first.send(&json!({"id":"retained-before-crash","type":command,"message":"fixture-only"}));
        assert_eq!(first.exit(), (expected_exit, String::new()));
        assert!(root.path().join(format!("{effect}.effect")).exists());
        // Deliberately reproduce the forbidden behavior to prove the oracle
        // catches it. The host acceptance test must never perform this replay.
        let mut replay = NativeFake::start(scenario, root.path());
        replay.send(
            &json!({"id":"new-id-is-not-new-authority","type":command,"message":"fixture-only"}),
        );
        let (code, diagnostic) = replay.exit();
        assert_eq!(code, 73);
        assert_eq!(diagnostic, "duplicate native effect attempted\n");
        assert!(root.path().join(format!("{effect}.duplicate")).exists());
    }
}

#[test]
fn stale_response_can_precede_current_response_on_the_same_native_stream() {
    let root = tempfile::tempdir().unwrap();
    let mut native = NativeFake::start("pi_stale_response", root.path());
    native.send(&json!({"id":"current-owner-request","type":"get_state"}));
    let stale = native.event();
    let current = native.event();
    assert_eq!(stale["id"], "previous-owner-request");
    assert_eq!(stale["data"]["isStreaming"], false);
    assert_eq!(current["id"], "current-owner-request");
    assert_eq!(current["data"]["isStreaming"], true);
    assert_eq!(native.exit(), (0, String::new()));
}

#[test]
fn pi_question_uses_native_request_correlation_not_a_prompt() {
    let root = tempfile::tempdir().unwrap();
    let mut native = NativeFake::start("pi_question", root.path());
    native.send(&json!({"id":"submit-1","type":"prompt","message":"fixture-only"}));
    assert_eq!(native.event()["command"], "prompt");
    let question = native.event();
    assert_eq!(question["method"], "select");
    native.send(&json!({"type":"extension_ui_response","id":question["id"],"value":"safe"}));
    assert_eq!(native.event()["type"], "agent_end");
    assert_eq!(native.event()["type"], "agent_settled");
    assert_eq!(native.exit(), (0, String::new()));
    assert!(root.path().join("question-answer.effect").exists());
}

#[test]
fn claude_foreground_result_does_not_hide_background_continuation() {
    let root = tempfile::tempdir().unwrap();
    let mut native = NativeFake::start("claude_result_before_background", root.path());
    native.send(&json!({"type":"user","message":{"role":"user","content":"fixture-only"}}));
    assert_eq!(native.event()["state"], "running");
    assert_eq!(native.event()["subtype"], "task_started");
    assert_eq!(native.event()["result"], "foreground result");
    wait_for_gate(root.path(), "claude-background");
    assert!(native.frames.try_recv().is_err());
    release(root.path(), "claude-background");
    assert_eq!(native.event()["subtype"], "task_notification");
    assert_eq!(native.event()["result"], "background continuation result");
    assert_eq!(native.event()["state"], "idle");
    assert_eq!(native.exit(), (0, String::new()));
}

#[test]
fn codex_turn_delivery_can_lose_ack_without_losing_its_native_effect() {
    let root = tempfile::tempdir().unwrap();
    let mut native = NativeFake::start("codex_lost_turn_ack", root.path());
    native.send(&json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"offline-fixture","version":"1.0.0"}}}));
    assert_eq!(native.event()["id"], 1);
    native.send(&json!({"method":"initialized"}));
    native.send(
        &json!({"id":2,"method":"turn/start","params":{"threadId":"fixture-thread","input":[]}}),
    );
    assert_eq!(native.exit(), (83, String::new()));
    assert!(root.path().join("codex-turn.effect").exists());
    assert!(native.frames.recv_timeout(DEADLINE).is_err());
}

#[test]
fn codex_expected_turn_mismatch_is_an_error_not_a_queued_prompt() {
    let root = tempfile::tempdir().unwrap();
    let mut native = NativeFake::start("codex_expected_turn_mismatch", root.path());
    native.send(&json!({"id":7,"method":"turn/steer","params":{"threadId":"fixture-thread","expectedTurnId":"stale-turn","input":[]}}));
    let reply = native.event();
    assert_eq!(reply["id"], 7);
    assert_eq!(reply["error"]["code"], -32602);
    assert!(reply.get("result").is_none());
    assert_eq!(native.exit(), (0, String::new()));
    assert!(!root.path().join("codex-turn.effect").exists());
}

#[test]
fn malformed_and_truncated_native_frames_are_not_successful_responses() {
    for (scenario, code) in [("malformed_frame", 84), ("partial_frame_then_exit", 85)] {
        let root = tempfile::tempdir().unwrap();
        let mut native = NativeFake::start(scenario, root.path());
        native.send(&json!({"type":"get_state","id":"state-1"}));
        assert!(serde_json::from_slice::<Value>(&native.frame()).is_err());
        assert_eq!(native.exit(), (code, String::new()));
    }
}

#[test]
fn fixture_mismatch_diagnostics_never_echo_sensitive_input() {
    let root = tempfile::tempdir().unwrap();
    let mut native = NativeFake::start("pi_question", root.path());
    native.send(&json!({"type":"auth_continue","secret":"fixture-secret-DO-NOT-PERSIST"}));
    let (code, diagnostic) = native.exit();
    assert_eq!(code, 65);
    assert_eq!(diagnostic, "fake native protocol mismatch\n");
    assert!(!root.path().join("question-answer.effect").exists());
}
