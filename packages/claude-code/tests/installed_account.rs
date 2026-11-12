//! Explicit bounded subscription compatibility probe. No credential contents are read or exported.
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
use bcode_claude_code::{
    AuthMethod, BillingRoute, CallbackRequest, CallbackResponse, ClaudeSession, Event,
    LaunchOptions, Limits, NativeAuthSupport, NativeEnvironment, SettingsInheritance,
    SettlementSupport, SubmissionUuid, auth_status,
};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fmt::Write, time::Duration};

const PROBE_COMMAND: &str = "test -z \"${BCODE_CLAUDE_ISOLATION_SENTINEL-}\" && cat probe.txt";
const EXPECTED_OUTPUT: &str = "SYNTHETIC_NATIVE_TOOL_OK";

#[derive(Default)]
struct ToolEvidence {
    call_id: Option<String>,
    succeeded: bool,
    invalid: bool,
}
impl ToolEvidence {
    fn observe(&mut self, value: &serde_json::Value) {
        if value.get("type").and_then(serde_json::Value::as_str) == Some("assistant")
            && let Some(blocks) = value
                .pointer("/message/content")
                .and_then(serde_json::Value::as_array)
        {
            for block in blocks {
                if block.get("type").and_then(serde_json::Value::as_str) != Some("tool_use") {
                    continue;
                }
                if self.call_id.is_some()
                    || block.get("name").and_then(serde_json::Value::as_str) != Some("Bash")
                    || block
                        .pointer("/input/command")
                        .and_then(serde_json::Value::as_str)
                        != Some(PROBE_COMMAND)
                {
                    self.invalid = true;
                    continue;
                }
                self.call_id = block
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
                self.invalid |= self.call_id.is_none();
            }
        }
        if value.get("type").and_then(serde_json::Value::as_str) != Some("user") {
            return;
        }
        let Some(blocks) = value
            .pointer("/message/content")
            .and_then(serde_json::Value::as_array)
        else {
            return;
        };
        for block in blocks {
            if block.get("type").and_then(serde_json::Value::as_str) != Some("tool_result") {
                continue;
            }
            let correlated = self.call_id.as_deref().is_some_and(|id| {
                block.get("tool_use_id").and_then(serde_json::Value::as_str) == Some(id)
            });
            let successful = correlated
                && block.get("is_error").and_then(serde_json::Value::as_bool) == Some(false)
                && value
                    .pointer("/tool_use_result/exitCode")
                    .and_then(serde_json::Value::as_i64)
                    == Some(0)
                && value
                    .pointer("/tool_use_result/interrupted")
                    .and_then(serde_json::Value::as_bool)
                    == Some(false)
                && value
                    .pointer("/tool_use_result/stdout")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|text| text.trim() == EXPECTED_OUTPUT)
                && value
                    .pointer("/tool_use_result/stderr")
                    .and_then(serde_json::Value::as_str)
                    == Some("");
            self.invalid |= !successful || self.succeeded;
            self.succeeded |= successful;
        }
    }

    const fn successful(&self) -> bool {
        self.succeeded && !self.invalid
    }
}

#[test]
fn failed_isolation_tool_cannot_pass_with_successful_model_final() {
    use serde_json::json;
    for (exit, error, stdout, id, accepted) in [
        (0, false, EXPECTED_OUTPUT, "call", true),
        (1, true, "", "call", false),
        (1, false, EXPECTED_OUTPUT, "call", false),
        (0, false, EXPECTED_OUTPUT, "other", false),
        (0, false, "wrong output", "call", false),
    ] {
        let mut evidence = ToolEvidence::default();
        evidence.observe(&json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"call","name":"Bash","input":{"command":PROBE_COMMAND}}]}}));
        evidence.observe(&json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":id,"is_error":error,"content":stdout}]},"tool_use_result":{"stdout":stdout,"stderr":"","exitCode":exit,"interrupted":false}}));
        evidence.observe(&json!({"type":"result","is_error":false,"result":EXPECTED_OUTPUT}));
        assert_eq!(evidence.successful(), accepted);
    }
}

fn probe_options() -> (tempfile::TempDir, LaunchOptions) {
    assert_eq!(
        std::env::var("BCODE_CLAUDE_LIVE_PROBE").as_deref(),
        Ok("authorized")
    );
    let executable = std::path::PathBuf::from(
        std::env::var_os("BCODE_CLAUDE_TEST_EXECUTABLE").expect("explicit artifact"),
    );
    let namespace = std::path::PathBuf::from(
        std::env::var_os("BCODE_CLAUDE_TEST_NATIVE_NAMESPACE").expect("explicit native namespace"),
    );
    assert!(namespace.is_absolute());
    let expected = std::env::var("BCODE_CLAUDE_TEST_SHA256").expect("explicit digest");
    let mut digest = String::new();
    for byte in Sha256::digest(std::fs::read(&executable).expect("executable only")) {
        write!(digest, "{byte:02x}").unwrap();
    }
    assert_eq!(digest, expected);
    let workspace = tempfile::tempdir().unwrap();
    // Synthetic settings must not affect the explicitly empty source selection.
    std::fs::create_dir(workspace.path().join(".claude")).unwrap();
    std::fs::write(
        workspace.path().join(".claude/settings.json"),
        r#"{"env":{"BCODE_CLAUDE_ISOLATION_SENTINEL":"project-settings-leaked"}}"#,
    )
    .unwrap();
    std::fs::write(
        workspace.path().join("probe.txt"),
        "SYNTHETIC_NATIVE_TOOL_OK\n",
    )
    .unwrap();
    let mut environment = BTreeMap::new();
    for key in ["HOME", "USER", "PATH", "TMPDIR", "LANG", "LC_ALL"] {
        if let Ok(value) = std::env::var(key) {
            environment.insert(key.into(), value);
        }
    }
    environment.insert("ANTHROPIC_MODEL".into(), "haiku".into());
    let options = LaunchOptions {
        executable,
        expected_version: std::env::var("BCODE_CLAUDE_TEST_VERSION").expect("exact version"),
        cwd: workspace.path().into(),
        config_directory: namespace,
        environment: NativeEnvironment(environment),
        billing_route: BillingRoute::NativeAccount,
        settings: SettingsInheritance::ExplicitSources,
        setting_sources: Vec::new(),
        resume_session_id: None,
        settlement_support: SettlementSupport::Unknown,
        auth_support: NativeAuthSupport::VerifiedStatus,
        limits: Limits {
            request_timeout: Duration::from_secs(20),
            ..Limits::default()
        },
    };
    (workspace, options)
}

#[tokio::test]
#[ignore = "explicit subscription authorization, native namespace and pinned executable required"]
async fn installed_account_stream_and_tool_compatibility() {
    let (_workspace, options) = probe_options();
    let status = auth_status(&options).await.expect("sanitized status");
    assert!(status.logged_in, "native subscription unavailable");
    assert_eq!(status.method, AuthMethod::ClaudeAccount);
    let mut session = ClaudeSession::spawn(options)
        .await
        .expect("native transport");
    let result = tokio::time::timeout(Duration::from_secs(90), async {
        let initialization = session.client().initialize().await.expect("initialize");
        eprintln!("native_probe initialized=true session_state_declared={}", initialization.pointer("/capabilities/sdk_session_state_events").and_then(serde_json::Value::as_bool) == Some(true));
        let uuid = SubmissionUuid::new("2b700001-1000-4000-8000-000000000001".into()).unwrap();
        session.client().submit_with_uuid(
            "Synthetic compatibility test. Run exactly one Bash command: test -z \"${BCODE_CLAUDE_ISOLATION_SENTINEL-}\" && cat probe.txt . Do not use any other tool, read any other file, access network, or modify files. Then reply exactly SYNTHETIC_NATIVE_TOOL_OK.",
            None, uuid,
        ).await.expect("one authorized submission");
        let mut chunks = 0;
        let mut tool_seen = false;
        let mut evidence = ToolEvidence::default();
        let mut result_seen = false;
        let mut state_seen = false;
        for _ in 0..256 {
            match session.next_event().await.expect("bounded stream") {
                Event::Callback { request_id, request } => {
                    let response = match &request {
                        CallbackRequest::CanUseTool { tool_name, input, .. } if tool_name == "Bash" && input.get("command").and_then(serde_json::Value::as_str) == Some("test -z \"${BCODE_CLAUDE_ISOLATION_SENTINEL-}\" && cat probe.txt") => {
                            tool_seen = true;
                            CallbackResponse::Allow { updated_input: input.clone() }
                        }
                        _ => CallbackResponse::Deny { message: "Only the exact synthetic probe command is authorized".into(), interrupt: true },
                    };
                    session.client().respond(&request_id, response).await.expect("permission response");
                }
                Event::Message(message) => {
                    let value = match &message {
                        bcode_claude_code::NativeMessage::System(value)
                        | bcode_claude_code::NativeMessage::Assistant(value)
                        | bcode_claude_code::NativeMessage::User(value)
                        | bcode_claude_code::NativeMessage::Result(value)
                        | bcode_claude_code::NativeMessage::Stream(value)
                        | bcode_claude_code::NativeMessage::Notice(value) => value,
                    };
                    evidence.observe(value);
                    match value.get("type").and_then(serde_json::Value::as_str) {
                        Some("stream_event") => chunks += 1,
                        Some("system") if value.get("subtype").and_then(serde_json::Value::as_str) == Some("session_state_changed") => state_seen = true,
                        Some("result") => {
                            if value.get("is_error").and_then(serde_json::Value::as_bool) == Some(true) {
                                let text = value.get("result").and_then(serde_json::Value::as_str).unwrap_or_default().to_ascii_lowercase();
                                eprintln!("native_probe native_error=true limit_related={} reset_indication={}", text.contains("limit"), text.contains("resets"));
                            }
                            assert_eq!(value.get("is_error").and_then(serde_json::Value::as_bool), Some(false), "native result error");
                            assert!(value.get("result").and_then(serde_json::Value::as_str).is_some_and(|s| s.contains("SYNTHETIC_NATIVE_TOOL_OK")), "synthetic result");
                            result_seen = true;
                            break;
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        eprintln!("native_probe result={result_seen} stream_chunks={chunks} permission_tool={tool_seen} state_event={state_seen}");
        assert!(result_seen && tool_seen && chunks > 0 && evidence.successful(), "required correlated successful native tool evidence");
    }).await;
    session.close().await;
    result.expect("90-second live probe budget");
}
