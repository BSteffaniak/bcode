//! Explicitly authorized subscription compatibility probes. Not run by default.
//! These exercise the Rust long-lived session API, not a replacement Python transport.
//! Coordinator opt-in (subscription spending): run only the exact ignored
//! `installed_native_long_lived_two_exact_uuid_submissions` test with `--nocapture`.
//! Set BCODE_NATIVE_CLAUDE_CONFIG_DIR only to the existing native credential namespace.
//! This does not verify native turn identity, background settlement, or full harness integration.
use bcode_claude_code::{
    AttributionStage, AuthMethod, BillingRoute, CallbackResponse, ClaudeSession, Event,
    IdentityState, LaunchOptions, Limits, NativeAuthSupport, NativeEnvironment, NativeMessage,
    SettingsInheritance, Settlement, SettlementSupport, SubmissionUuid,
};
use std::{collections::BTreeMap, path::PathBuf, time::Duration};

const EXECUTABLE: &str =
    "/nix/store/yzhsl1i8v8gypvdx2bigpysj58la5i8w-claude-code-2.1.308/bin/claude";
const EXECUTABLE_SHA256: &str = "49ca742c79d844aea24b25190c765994bbcb7d63ddf4726e4c048e89946494b0";
const WRAPPED_SHA256: &str = "0696f3eba9cc930f29dab7e6d4102b79b72d58c163c14415cfe72460575e355b";

fn verify_artifacts() {
    for (path, expected) in [
        (PathBuf::from(EXECUTABLE), EXECUTABLE_SHA256),
        (
            PathBuf::from(EXECUTABLE).with_file_name(".claude-wrapped"),
            WRAPPED_SHA256,
        ),
    ] {
        let output = std::process::Command::new("sha256sum")
            .env_clear()
            .env("PATH", std::env::var_os("PATH").expect("sha256sum PATH"))
            .arg(&path)
            .output()
            .expect("sha256sum required for exact native probe");
        assert!(output.status.success(), "native artifact hashing failed");
        let stdout = String::from_utf8(output.stdout).expect("hash output UTF-8");
        assert_eq!(
            stdout.split_whitespace().next(),
            Some(expected),
            "native artifact identity mismatch"
        );
    }
    eprintln!(
        "native_library version=2.1.308 executable_sha256={EXECUTABLE_SHA256} wrapped_sha256={WRAPPED_SHA256}"
    );
}

fn options(cwd: PathBuf) -> LaunchOptions {
    let home = std::env::var("HOME").expect("existing native login HOME required");
    let config_directory = std::env::var_os("BCODE_NATIVE_CLAUDE_CONFIG_DIR")
        .or_else(|| std::env::var_os("CLAUDE_CONFIG_DIR"))
        .map_or_else(|| PathBuf::from(&home).join(".claude"), PathBuf::from);
    let mut environment = BTreeMap::new();
    for key in ["HOME", "PATH", "LANG", "SSL_CERT_FILE", "NIX_SSL_CERT_FILE"] {
        if let Ok(value) = std::env::var(key) {
            environment.insert(key.into(), value);
        }
    }
    // No ambient API keys, OAuth overrides, alternate endpoints or cloud routes.
    environment.insert("ANTHROPIC_MODEL".into(), "haiku".into());
    LaunchOptions {
        executable: PathBuf::from(EXECUTABLE),
        expected_version: "2.1.308 (Claude Code)".into(),
        cwd,
        config_directory,
        environment: NativeEnvironment(environment),
        billing_route: BillingRoute::NativeAccount,
        settings: SettingsInheritance::ExplicitSources,
        setting_sources: Vec::new(),
        resume_session_id: None,
        settlement_support: SettlementSupport::VerifiedSessionState,
        auth_support: NativeAuthSupport::VerifiedStatusAndLogin,
        limits: Limits {
            request_timeout: Duration::from_secs(30),
            ..Limits::default()
        },
    }
}

fn fresh_uuid() -> String {
    let ticks = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    format!(
        "{:08x}-{:04x}-4000-8000-{:012x}",
        std::process::id(),
        (ticks >> 48) & 0xffff,
        ticks & 0xffff_ffff_ffff
    )
}

async fn start(cwd: PathBuf, session_uuid: &str, without_tools: bool) -> ClaudeSession {
    verify_artifacts();
    let options = options(cwd);
    let auth = bcode_claude_code::auth_status(&options)
        .await
        .expect("subscription status");
    assert!(auth.logged_in);
    assert_eq!(auth.method, AuthMethod::ClaudeAccount);
    let session = if without_tools {
        ClaudeSession::spawn_new_without_tools(options, session_uuid).await
    } else {
        ClaudeSession::spawn_new(options, session_uuid).await
    }
    .expect("exact native spawn");
    session
        .client()
        .initialize()
        .await
        .expect("initialize live transport");
    session
}

async fn next(session: &mut ClaudeSession) -> Event {
    tokio::time::timeout(Duration::from_secs(60), session.next_event())
        .await
        .expect("bounded native observation")
        .expect("native protocol event")
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "spends existing Claude subscription; explicit native compatibility authorization required"]
async fn installed_native_long_lived_two_exact_uuid_submissions() {
    let directory = tempfile::tempdir().unwrap();
    let session_uuid = fresh_uuid();
    let mut session = start(directory.path().to_path_buf(), &session_uuid, true).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    for serial in 1..=2 {
        let uuid = SubmissionUuid::new(fresh_uuid()).unwrap();
        session
            .client()
            .submit_with_uuid(
                "Reply with exactly LIBRARY_OK. Do not use tools.",
                Some(&session_uuid),
                uuid.clone(),
            )
            .await
            .expect("write exact attempt");
        let mut reply = false;
        let mut result = false;
        let mut settled = false;
        let mut expected_text = false;
        let mut running = false;
        let mut idle = false;
        let mut sdk_state_flag_observed = false;
        for _ in 0..128 {
            match tokio::time::timeout_at(deadline, next(&mut session))
                .await
                .expect("native no-tools test total deadline")
            {
                Event::Attribution(evidence) => {
                    assert_eq!(evidence.primary, uuid);
                    assert_eq!(evidence.consumed, vec![uuid.clone()]);
                    assert!(!evidence.resumed);
                    match evidence.stage {
                        AttributionStage::Reply => reply = true,
                        AttributionStage::Result {
                            is_error,
                            queued_turn_count,
                        } => {
                            assert!(!is_error);
                            assert_eq!(queued_turn_count, Some(0));
                            result = true;
                        }
                    }
                }
                Event::Callback { request_id, .. } => {
                    session
                        .client()
                        .respond(&request_id, CallbackResponse::Error)
                        .await
                        .unwrap();
                    panic!("unexpected tool or hook callback in no-tools probe");
                }
                Event::Settlement(Settlement::Settled) => {
                    settled = true;
                    break;
                }
                Event::Message(NativeMessage::Assistant(value)) => {
                    if let Some(blocks) = value
                        .pointer("/message/content")
                        .and_then(serde_json::Value::as_array)
                    {
                        assert!(
                            !blocks.iter().any(|block| block
                                .get("type")
                                .and_then(serde_json::Value::as_str)
                                == Some("tool_use")),
                            "ordinary prompt unexpectedly invoked native tools"
                        );
                    }
                }
                Event::Message(NativeMessage::Result(value)) => {
                    expected_text = value
                        .get("result")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|text| text.trim() == "LIBRARY_OK");
                }
                Event::Message(NativeMessage::System(value))
                    if value.get("subtype").and_then(serde_json::Value::as_str)
                        == Some("session_state_changed") =>
                {
                    sdk_state_flag_observed |= value
                        .get("sdk_host_only")
                        .and_then(serde_json::Value::as_bool)
                        == Some(true);
                    match value.get("state").and_then(serde_json::Value::as_str) {
                        Some("running") => running = true,
                        Some("idle") => idle = true,
                        _ => {}
                    }
                }
                Event::Message(NativeMessage::System(value))
                    if value.get("subtype").and_then(serde_json::Value::as_str) == Some("init") =>
                {
                    assert_eq!(
                        value
                            .get("apiKeySource")
                            .and_then(serde_json::Value::as_str),
                        Some("none")
                    );
                    assert!(
                        value
                            .get("tools")
                            .and_then(serde_json::Value::as_array)
                            .is_some_and(Vec::is_empty),
                        "native tools were not disabled"
                    );
                }
                _ => {}
            }
        }
        assert!(
            reply
                && result
                && settled
                && expected_text
                && running
                && idle
                && sdk_state_flag_observed
        );
        assert_eq!(session.client().settlement(), Settlement::Settled);
        eprintln!(
            "native_library turn={serial} reply_correlated=true result_correlated=true expected_text=true running=true idle=true settled=true"
        );
        assert!(matches!(
            session.client().identity_state(),
            IdentityState::Confirmed { .. }
        ));
    }
    session.close().await;
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "spends existing Claude subscription and executes one isolated background sleep"]
async fn installed_native_background_shell_never_settles_on_false_idle() {
    let directory = tempfile::tempdir().unwrap();
    let session_uuid = fresh_uuid();
    let mut session = start(directory.path().to_path_buf(), &session_uuid, false).await;
    session.client().submit_with_uuid(
        "Protocol test. Make exactly one Bash call with command sleep 3 and run_in_background true. Do not read/write files or execute anything else. Immediately reply LAUNCHED; let the native background notification arrive later.",
        Some(&session_uuid),
        SubmissionUuid::new(fresh_uuid()).unwrap(),
    ).await.unwrap();
    let mut saw_intent = false;
    let mut completed = false;
    let mut results = 0;
    let mut allowed = false;
    let mut snapshot_live = false;
    let mut snapshot_empty_after_live = false;
    let mut result_background_id = false;
    for _ in 0..256 {
        match next(&mut session).await {
            Event::Callback {
                request_id,
                request,
            } => {
                let response = match request {
                    bcode_claude_code::CallbackRequest::CanUseTool {
                        tool_name, input, ..
                    } if !allowed
                        && tool_name == "Bash"
                        && input.get("command").and_then(serde_json::Value::as_str)
                            == Some("sleep 3")
                        && input
                            .get("run_in_background")
                            .and_then(serde_json::Value::as_bool)
                            == Some(true) =>
                    {
                        allowed = true;
                        CallbackResponse::Allow {
                            updated_input: input,
                        }
                    }
                    _ => CallbackResponse::Error,
                };
                session
                    .client()
                    .respond(&request_id, response)
                    .await
                    .unwrap();
            }
            Event::Message(NativeMessage::Assistant(value)) => {
                if let Some(blocks) = value
                    .pointer("/message/content")
                    .and_then(serde_json::Value::as_array)
                {
                    for block in blocks {
                        if block.get("type").and_then(serde_json::Value::as_str) == Some("tool_use")
                        {
                            assert_eq!(
                                block.get("name").and_then(serde_json::Value::as_str),
                                Some("Bash")
                            );
                            assert_eq!(
                                block
                                    .pointer("/input/command")
                                    .and_then(serde_json::Value::as_str),
                                Some("sleep 3")
                            );
                            assert_eq!(
                                block
                                    .pointer("/input/run_in_background")
                                    .and_then(serde_json::Value::as_bool),
                                Some(true)
                            );
                            saw_intent = true;
                        }
                    }
                }
            }
            Event::Message(NativeMessage::User(value)) => {
                result_background_id |= value
                    .pointer("/tool_use_result/backgroundTaskId")
                    .or_else(|| value.pointer("/tool_use_result/background_task_id"))
                    .and_then(serde_json::Value::as_str)
                    .is_some();
            }
            Event::Message(NativeMessage::System(value)) => {
                if value.get("subtype").and_then(serde_json::Value::as_str)
                    == Some("background_tasks_changed")
                {
                    let tasks = value
                        .get("tasks")
                        .and_then(serde_json::Value::as_array)
                        .expect("native snapshot array");
                    snapshot_live |= !tasks.is_empty();
                    snapshot_empty_after_live |= snapshot_live && tasks.is_empty();
                }
                if value.get("subtype").and_then(serde_json::Value::as_str)
                    == Some("task_notification")
                    && value.get("status").and_then(serde_json::Value::as_str) == Some("completed")
                {
                    completed = true;
                }
                if value.get("subtype").and_then(serde_json::Value::as_str)
                    == Some("session_state_changed")
                    && value.get("state").and_then(serde_json::Value::as_str) == Some("idle")
                    && completed
                    && results >= 2
                {
                    break;
                }
            }
            Event::Message(NativeMessage::Result(_)) => results += 1,
            Event::Settlement(Settlement::Settled) => panic!("background shell falsely settled"),
            _ => {}
        }
    }
    assert!(saw_intent && completed && results >= 2);
    assert!(snapshot_live && snapshot_empty_after_live && result_background_id);
    eprintln!(
        "native_library background_intent=true tool_result_id=true snapshot_live=true snapshot_empty_after_live=true completion_notification=true results={results} settlement=unsupported"
    );
    assert_eq!(session.client().settlement(), Settlement::Unsupported);
    session.close().await;
}
