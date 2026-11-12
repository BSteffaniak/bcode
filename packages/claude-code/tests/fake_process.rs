#![cfg(unix)]
//! Fake local processes only. These tests never invoke an installed Claude CLI.
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
use bcode_claude_code::{
    BillingRoute, CallbackResponse, ClaudeSession, ControlRequest, Error, Event, IdentityState,
    LaunchOptions, Limits, NativeAuthSupport, NativeEnvironment, SettingsInheritance, Settlement,
    SettlementSupport, auth_status,
};
use std::{collections::BTreeMap, time::Duration};

struct Fake {
    _directory: tempfile::TempDir,
    options: LaunchOptions,
}
impl Fake {
    fn new(case: &str) -> Self {
        let directory = tempfile::tempdir().expect("tempdir");
        // Never write an executable inode while parallel tests can spawn processes.
        // Only cwd/config/environment are per-test; the source fixture stays read-only.
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-claude.sh");
        assert!(path.is_file(), "checked-in fake CLI fixture");
        let mut env = BTreeMap::new();
        env.insert("FAKE_CASE".to_owned(), case.to_owned());
        let options = LaunchOptions {
            executable: path,
            expected_version: "2.1.308 (Claude Code)".to_owned(),
            cwd: directory.path().to_owned(),
            config_directory: directory.path().join("auth"),
            environment: NativeEnvironment(env),
            billing_route: BillingRoute::NativeAccount,
            settings: SettingsInheritance::ExplicitSources,
            setting_sources: vec![],
            resume_session_id: None,
            settlement_support: SettlementSupport::VerifiedSessionState,
            auth_support: NativeAuthSupport::VerifiedStatusAndLogin,
            limits: Limits {
                request_timeout: Duration::from_secs(2),
                ..Limits::default()
            },
        };
        Self {
            _directory: directory,
            options,
        }
    }
    async fn start(&self) -> ClaudeSession {
        let session = ClaudeSession::spawn(self.options.clone())
            .await
            .expect("spawn");
        session.client().initialize().await.expect("initialize");
        session
    }
}
#[tokio::test(flavor = "current_thread")]
async fn exact_version_mismatch_rejects_native_status_and_stream_launch() {
    let mut fake = Fake::new("ordinary");
    fake.options.expected_version = "fake-claude 2".into();
    assert!(matches!(
        auth_status(&fake.options).await,
        Err(Error::VersionMismatch)
    ));
    assert!(matches!(
        ClaudeSession::spawn(fake.options).await,
        Err(Error::VersionMismatch)
    ));
}

async fn receive(session: &mut ClaudeSession) -> Result<Event, Error> {
    tokio::time::timeout(Duration::from_secs(4), session.next_event())
        .await
        .expect("bounded receive")
}
async fn settled(session: &mut ClaudeSession) {
    for _ in 0..24 {
        if matches!(
            receive(session).await.expect("event"),
            Event::Settlement(Settlement::Settled)
        ) {
            return;
        }
    }
    panic!("no authoritative settlement marker");
}

#[tokio::test(flavor = "current_thread")]
async fn uuid_wire_submission_emits_reply_and_result_evidence_before_raw_frames() {
    use bcode_claude_code::{AttributionStage, NativeMessage, SubmissionUuid};
    let fake = Fake::new("attribution");
    let mut session = fake.start().await;
    let client = session.client();
    let uuid = SubmissionUuid::new("00000000-0000-4000-8000-000000000001".into()).unwrap();
    client
        .submit_with_uuid("hello", None, uuid.clone())
        .await
        .unwrap();
    let mut stages = Vec::new();
    let mut saw_notice = false;
    let mut saw_settlement = false;
    for _ in 0..16 {
        match receive(&mut session).await.unwrap() {
            Event::Attribution(evidence) => {
                assert_eq!(evidence.primary, uuid);
                assert_eq!(evidence.consumed, vec![uuid.clone()]);
                stages.push(evidence.stage);
            }
            Event::Message(NativeMessage::Assistant(_)) => {
                assert_eq!(stages, [AttributionStage::Reply]);
            }
            Event::Message(NativeMessage::Result(_)) => assert_eq!(stages.len(), 2),
            Event::Message(NativeMessage::Notice(_)) => saw_notice = true,
            Event::Settlement(Settlement::Settled) => {
                saw_settlement = true;
                break;
            }
            _ => {}
        }
    }
    assert!(saw_settlement && saw_notice);
    assert_eq!(
        stages,
        [
            AttributionStage::Reply,
            AttributionStage::Result {
                is_error: false,
                queued_turn_count: Some(0)
            }
        ]
    );
    assert!(client.submit_with_uuid("hello", None, uuid).await.is_err());
    assert!(client.submit("legacy", None).await.is_err());
    session.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn result_then_idle_settles_but_interrupt_ack_is_not_settlement() {
    let fake = Fake::new("normal");
    let mut session = fake.start().await;
    let client = session.client();
    assert_eq!(client.settlement(), Settlement::Running);
    client.interrupt().await.expect("ack");
    assert_eq!(client.settlement(), Settlement::Running);
    client.submit("hello", None).await.expect("written");
    settled(&mut session).await;
    session.close().await;
    assert_eq!(client.settlement(), Settlement::Uncertain);
}
#[tokio::test(flavor = "current_thread")]
async fn background_task_keeps_result_and_idle_from_settling() {
    let fake = Fake::new("background");
    let mut session = fake.start().await;
    session
        .client()
        .submit("start", None)
        .await
        .expect("submit");
    for _ in 0..3 {
        let _ = receive(&mut session).await.expect("message");
    }
    assert_ne!(session.settlement(), Settlement::Settled);
    session.close().await;
}
#[tokio::test(flavor = "current_thread")]
async fn task_completion_followup_result_and_idle_settle() {
    let fake = Fake::new("followup");
    let mut session = fake.start().await;
    session
        .client()
        .submit("start", None)
        .await
        .expect("submit");
    settled(&mut session).await;
    session.close().await;
}
#[tokio::test(flavor = "current_thread")]
async fn callbacks_are_concurrent_and_cancelled_ids_cannot_be_answered() {
    let fake = Fake::new("callbacks");
    let mut session = fake.start().await;
    let client = session.client();
    client.submit("ask", None).await.expect("submit");
    assert!(
        matches!(receive(&mut session).await.expect("callback"), Event::Callback { request_id, .. } if request_id == "a")
    );
    assert!(
        matches!(receive(&mut session).await.expect("callback"), Event::Callback { request_id, .. } if request_id == "b")
    );
    assert!(
        matches!(receive(&mut session).await.expect("cancel"), Event::CallbackCancelled { request_id } if request_id == "a")
    );
    assert!(matches!(
        client
            .respond(
                "a",
                CallbackResponse::Deny {
                    message: "no".into(),
                    interrupt: false
                }
            )
            .await,
        Err(Error::StaleCallback)
    ));
    client
        .respond(
            "b",
            CallbackResponse::Hook {
                output: serde_json::json!({}),
            },
        )
        .await
        .expect("independent response");
    assert!(matches!(
        client.respond("b", CallbackResponse::Error).await,
        Err(Error::StaleCallback)
    ));
    session.close().await;
}
#[tokio::test(flavor = "current_thread")]
async fn unknown_state_fails_closed() {
    let fake = Fake::new("unknown");
    let mut session = fake.start().await;
    session
        .client()
        .submit("start", None)
        .await
        .expect("submit");
    assert!(matches!(receive(&mut session).await, Err(Error::Protocol)));
    assert_eq!(session.settlement(), Settlement::Uncertain);
    session.close().await;
}
#[tokio::test(flavor = "current_thread")]
async fn slow_consumer_overflow_is_explicit_not_silent_loss() {
    let mut fake = Fake::new("overflow");
    fake.options.limits.queued_events = 1;
    let mut session = fake.start().await;
    let _ = session.client().submit("start", None).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut capacity = false;
    for _ in 0..4 {
        if matches!(receive(&mut session).await, Err(Error::Capacity)) {
            capacity = true;
            break;
        }
    }
    assert!(capacity);
    assert_eq!(session.settlement(), Settlement::Uncertain);
    session.close().await;
}
#[tokio::test(flavor = "current_thread")]
async fn unterminated_oversize_frame_is_bounded() {
    let mut fake = Fake::new("oversized");
    fake.options.limits.frame_bytes = 512;
    let mut session = fake.start().await;
    let _ = session.client().submit("start", None).await;
    assert!(matches!(receive(&mut session).await, Err(Error::Capacity)));
    session.close().await;
}
#[tokio::test(flavor = "current_thread")]
async fn stderr_is_drained_while_stdout_and_control_remain_live() {
    let fake = Fake::new("stderr");
    let mut session = fake.start().await;
    session
        .client()
        .submit("start", None)
        .await
        .expect("submit");
    settled(&mut session).await;
    session.close().await;
}
#[tokio::test(flavor = "current_thread")]
async fn explicit_resume_and_auth_share_config_namespace() {
    let mut fake = Fake::new("resume");
    fake.options.resume_session_id = Some("native-123".into());
    let status = auth_status(&fake.options).await.expect("local auth status");
    assert_eq!(status.config_directory, fake.options.config_directory);
    let mut session = fake.start().await;
    session
        .client()
        .submit("resume", Some("native-123"))
        .await
        .expect("submit");
    settled(&mut session).await;
    session.close().await;
}
#[tokio::test(flavor = "current_thread")]
async fn ambiguous_process_exit_never_becomes_success() {
    let fake = Fake::new("eof");
    let mut session = fake.start().await;
    let _ = session.client().submit("start", None).await;
    for _ in 0..4 {
        if receive(&mut session).await.is_err() {
            break;
        }
    }
    assert_eq!(session.settlement(), Settlement::Uncertain);
    session.close().await;
}
#[tokio::test(flavor = "current_thread")]
async fn unverified_mode_never_settles_and_unintended_api_key_is_rejected() {
    let mut fake = Fake::new("normal");
    fake.options.settlement_support = SettlementSupport::Unknown;
    let mut session = fake.start().await;
    session
        .client()
        .submit("start", None)
        .await
        .expect("submit");
    let _ = receive(&mut session).await.expect("result");
    let _ = receive(&mut session).await.expect("idle");
    assert_eq!(session.settlement(), Settlement::Unsupported);
    session.close().await;
    fake.options
        .environment
        .0
        .insert("ANTHROPIC_API_KEY".into(), "not-a-real-key".into());
    assert!(matches!(
        ClaudeSession::spawn(fake.options).await,
        Err(Error::InvalidConfiguration(_))
    ));
}
#[tokio::test(flavor = "current_thread")]
async fn abandoned_request_does_not_block_other_requests_or_retry_itself() {
    let fake = Fake::new("normal");
    let session = fake.start().await;
    let client = session.client();
    let ticket = client
        .request(ControlRequest::SetModel {
            model: "fake-model".into(),
        })
        .await
        .expect("request");
    ticket.cancel();
    // The abandoned request was rust_2; no automatic replay creates rust_3.
    assert_eq!(
        client
            .request(ControlRequest::Interrupt)
            .await
            .expect("request")
            .request_id(),
        "rust_3"
    );
    session.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn new_session_requires_native_confirmation_of_requested_uuid() {
    let fake = Fake::new("identity_confirm");
    let uuid = "550e8400-e29b-41d4-a716-446655440000";
    let mut session = ClaudeSession::spawn_new(fake.options, uuid)
        .await
        .expect("spawn new");
    let client = session.client();
    client
        .initialize()
        .await
        .expect("control ready without native identity");
    assert!(client.is_control_ready());
    assert_eq!(
        client.identity_state(),
        IdentityState::Prepared {
            requested_session_id: uuid.into()
        }
    );
    assert!(matches!(
        client
            .submit("wrong association", Some("other-native"))
            .await,
        Err(Error::SessionIdentity)
    ));
    client
        .submit("actual authorized task", Some(uuid))
        .await
        .expect("first delivery allowed");
    let identity = client
        .wait_session_identity()
        .await
        .expect("first native init");
    assert_eq!(identity.native_session_id, uuid);
    assert!(
        matches!(client.identity_state(), IdentityState::Confirmed { native_session_id } if native_session_id == uuid)
    );
    assert!(matches!(
        receive(&mut session).await.expect("init observation"),
        Event::Message(bcode_claude_code::NativeMessage::System(_))
    ));
    session.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn create_and_resume_mismatches_fail_on_first_native_init() {
    for resume in [false, true] {
        let mut fake = Fake::new("identity_mismatch");
        let mut session = if resume {
            fake.options.resume_session_id = Some("native-123".into());
            ClaudeSession::spawn(fake.options)
                .await
                .expect("resume process")
        } else {
            ClaudeSession::spawn_new(fake.options, "550e8400-e29b-41d4-a716-446655440000")
                .await
                .expect("create process")
        };
        let client = session.client();
        client
            .initialize()
            .await
            .expect("control readiness is not identity");
        assert!(matches!(
            client.identity_state(),
            IdentityState::Prepared { .. }
        ));
        let _ = client.submit("actual task", None).await;
        assert!(matches!(
            receive(&mut session).await,
            Err(Error::SessionIdentity)
        ));
        assert_eq!(client.identity_state(), IdentityState::Uncertain);
        assert!(client.submit("must not replay", None).await.is_err());
        session.close().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn missing_native_identity_cannot_be_replaced_by_requested_uuid() {
    let mut fake = Fake::new("identity_missing");
    fake.options.limits.request_timeout = Duration::from_millis(100);
    let session = ClaudeSession::spawn_new(fake.options, "550e8400-e29b-41d4-a716-446655440000")
        .await
        .expect("create process");
    assert!(matches!(
        session.client().initialize_session().await,
        Err(Error::SessionIdentity)
    ));
    session.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn sdk_host_idle_before_result_is_valid_settlement_order() {
    let fake = Fake::new("idle_first");
    let mut session = fake.start().await;
    session
        .client()
        .submit("actual task", None)
        .await
        .expect("submit");
    settled(&mut session).await;
    session.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn creation_uuid_and_resume_are_mutually_exclusive() {
    let mut fake = Fake::new("normal");
    fake.options.resume_session_id = Some("native-123".into());
    assert!(matches!(
        ClaudeSession::spawn_new(fake.options, "550e8400-e29b-41d4-a716-446655440000").await,
        Err(Error::InvalidConfiguration(_))
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn version_drift_launches_with_probed_version_but_unparseable_output_fails_closed() {
    let mut fake = Fake::new("normal");
    fake.options
        .environment
        .0
        .insert("FAKE_VERSION".into(), "2.9.0 (Claude Code)".into());
    let session = fake.start().await;
    assert_eq!(session.client().native_version(), "2.9.0 (Claude Code)");
    assert!(!bcode_claude_code::is_tested_version(
        session.client().native_version()
    ));
    session.close().await;
    for output in [
        "fake-claude 1",
        "wrapper 2.1.308 (Claude Code)",
        "2.1 (Claude Code)",
    ] {
        let mut fake = Fake::new("normal");
        fake.options
            .environment
            .0
            .insert("FAKE_VERSION".into(), output.into());
        assert!(matches!(
            ClaudeSession::spawn(fake.options).await,
            Err(Error::VersionMismatch)
        ));
    }
    // Output exactly equal to the configured expected value still launches, untested.
    let mut fake = Fake::new("normal");
    fake.options.expected_version = "fake-claude 1".into();
    fake.options
        .environment
        .0
        .insert("FAKE_VERSION".into(), "fake-claude 1".into());
    let session = fake.start().await;
    assert_eq!(session.client().native_version(), "fake-claude 1");
    assert!(!bcode_claude_code::is_tested_version(
        session.client().native_version()
    ));
    session.close().await;
    let fake = Fake::new("normal");
    let session = fake.start().await;
    assert!(bcode_claude_code::is_tested_version(
        session.client().native_version()
    ));
    session.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn model_and_permission_mode_switch_between_turns_after_native_acknowledgment() {
    use bcode_claude_code::{PermissionMode, available_models};
    let fake = Fake::new("configure");
    let mut session = ClaudeSession::spawn(fake.options.clone())
        .await
        .expect("spawn");
    let client = session.client();
    let initialize = client.initialize().await.expect("initialize");
    let models = available_models(&initialize).expect("advertised models");
    assert_eq!(models.len(), 2);
    client.submit("first turn", None).await.expect("submit");
    // An unadvertised key never reaches the native process.
    assert!(matches!(
        client.select_model(&models, "unadvertised").await,
        Err(Error::InvalidConfiguration(_))
    ));
    settled(&mut session).await;
    assert!(client.can_submit_next_turn());
    let selected = client
        .select_model(&models, "sonnet")
        .await
        .expect("switch");
    assert_eq!(selected.requested, "sonnet");
    assert_eq!(selected.effective.as_deref(), Some("claude-sonnet-4-6"));
    // Native rejection is reported without terminating the session.
    assert!(matches!(
        client.select_model(&models, "claude-opus-4-1").await,
        Err(Error::Protocol)
    ));
    assert!(client.is_control_ready());
    for mode in [
        PermissionMode::Plan,
        PermissionMode::AcceptEdits,
        PermissionMode::Default,
    ] {
        client.set_permission_mode(mode).await.expect("mode");
    }
    client.submit("second turn", None).await.expect("submit");
    settled(&mut session).await;
    session.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn model_switch_is_rejected_while_a_foreground_turn_is_active() {
    use bcode_claude_code::available_models;
    let fake = Fake::new("background");
    let session = ClaudeSession::spawn(fake.options.clone())
        .await
        .expect("spawn");
    let client = session.client();
    let initialize = client.initialize().await.expect("initialize");
    let models = available_models(&initialize).expect("advertised models");
    client.submit("start", None).await.expect("submit");
    assert!(!client.can_submit_next_turn());
    assert!(matches!(
        client.select_model(&models, "sonnet").await,
        Err(Error::InvalidConfiguration(_))
    ));
    session.close().await;
}
