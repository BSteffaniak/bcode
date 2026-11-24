//! Offline integration tests against the deterministic fake ACP server fixture.
#![cfg(unix)]

use bcode_antigravity_harness_plugin::auth;
use bcode_antigravity_harness_plugin::session::AcpSession;
use bcode_antigravity_harness_plugin::transport::{
    AcpError, Launch, probe_version, run_bounded_cli,
};
use bcode_harness_models::{
    HARNESS_SCHEMA_VERSION, HarnessActivity, HarnessAuthBinding, HarnessAuthCustody,
    HarnessCompleteness, HarnessConnectivity, HarnessExecutionEvent, HarnessObservation,
    HarnessObservationKind, HarnessPermissionMode, HarnessPlacement, HarnessResponse,
    HarnessResponseValue, HarnessSessionBinding, HarnessSessionSetting, HarnessSubmission,
    HarnessTerminalDisposition,
};
use bcode_provider_auth_models::managed::AuthReadiness;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn fake_agy_script(dir: &Path) -> PathBuf {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/acp_server.py");
    let wrapper = dir.join("agy");
    let body = format!("#!/bin/sh\nexec python3 {:?} \"$@\"\n", fixture);
    std::fs::write(&wrapper, body).expect("write wrapper");
    let mut perms = std::fs::metadata(&wrapper).expect("meta").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&wrapper, perms).expect("chmod");
    wrapper
}

fn test_launch(dir: &Path) -> Launch {
    let exe = fake_agy_script(dir);
    let gemini_dir = dir.join("gemini");
    std::fs::create_dir_all(&gemini_dir).expect("mkdir gemini");
    Launch {
        executable: exe,
        gemini_dir,
        working_directory: dir.to_path_buf(),
        timeout: Duration::from_secs(10),
    }
}

fn initial_binding() -> HarnessSessionBinding {
    HarnessSessionBinding {
        schema_version: HARNESS_SCHEMA_VERSION,
        session_id: "bcode-sess-1".into(),
        instance_id: "antigravity".into(),
        native_session_id: "open-1".into(),
        native_version: "unverified-acp-v1".into(),
        generation: 1,
    }
}

fn drain_until<F>(session: &mut AcpSession, mut pred: F) -> Vec<HarnessObservation>
where
    F: FnMut(&[HarnessObservation]) -> bool,
{
    let mut all = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let (batch, _) = session.observe(16).expect("observe");
        all.extend(batch);
        if pred(&all) {
            return all;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    all
}

#[test]
fn version_models_and_auth_readiness_work_against_fake_acp() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let launch = test_launch(tmp.path());

    assert_eq!(probe_version(&launch).expect("version"), "1.5.15");
    let models_out = run_bounded_cli(&launch, &["models"]).expect("models");
    assert!(models_out.contains("gemini-4-neon-high\tGemini 4 Neon (High)"));

    let auth_binding = HarnessAuthBinding {
        provider_id: "bcode.antigravity".into(),
        profile: "native".into(),
        credential_namespace: launch.gemini_dir.to_string_lossy().into_owned(),
        custody: HarnessAuthCustody::NativeManaged,
    };
    let ready = auth::inspect_status(&launch, &auth_binding).expect("ready status");
    assert_eq!(ready.readiness, AuthReadiness::Ready);

    // Mark namespace unauthenticated and confirm non-mutating LoginRequired detection.
    std::fs::write(launch.gemini_dir.join("UNAUTH"), b"1").expect("mark unauth");
    let unauth = auth::inspect_status(&launch, &auth_binding).expect("unauth status");
    assert_eq!(unauth.readiness, AuthReadiness::LoginRequired);
}

#[test]
fn open_streaming_configure_permission_cancel_and_resume_lifecycle() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let launch = test_launch(tmp.path());

    let mut session = AcpSession::open_with_model(
        &launch,
        initial_binding(),
        false,
        Some("gemini-4-neon-medium"),
    )
    .expect("open with selected model");
    assert_eq!(session.binding().native_version, "1.5.15");
    assert_eq!(session.model(), Some("gemini-4-neon-medium"));

    // Mid-session mode switch
    session
        .configure(&HarnessSessionSetting::PermissionMode {
            mode: HarnessPermissionMode::Plan,
        })
        .expect("set_mode plan");

    // First turn with streaming thought + text + usage + SubmissionTerminal
    let sub1 = HarnessSubmission {
        submission_id: "sub-1".into(),
        attempt_id: "att-1".into(),
        session_id: "bcode-sess-1".into(),
        generation: 1,
        text: "hello world".into(),
        placement: HarnessPlacement::NextTurn,
    };
    let receipt = session.submit(&sub1).expect("submit 1");
    assert!(receipt.native_request_id.is_some());
    // Idempotent retry returns exact receipt
    assert_eq!(session.submit(&sub1).expect("retry 1"), receipt);

    let obs = drain_until(&mut session, |all| {
        all.iter().any(|o| {
            matches!(
                &o.kind,
                HarnessObservationKind::Execution {
                    event: HarnessExecutionEvent::SubmissionTerminal { .. }
                }
            )
        })
    });
    assert!(obs.iter().any(|o| matches!(
        &o.kind,
        HarnessObservationKind::Reasoning { text, .. } if text == "thinking..."
    )));
    assert!(obs.iter().any(|o| matches!(
        &o.kind,
        HarnessObservationKind::Text { text, .. } if text == "Echo(gemini-4-neon-medium,plan): hello world"
    )));
    assert!(obs.iter().any(|o| matches!(
        &o.kind,
        HarnessObservationKind::Usage {
            input_tokens: 100,
            output_tokens: 20,
            ..
        }
    )));
    assert!(obs.iter().any(|o| matches!(
        &o.kind,
        HarnessObservationKind::Execution {
            event: HarnessExecutionEvent::SubmissionTerminal {
                disposition: HarnessTerminalDisposition::Completed,
                ..
            }
        }
    )));

    // Second turn: permission callback -> approve -> tool result + DONE_APPROVED
    let sub2 = HarnessSubmission {
        submission_id: "sub-2".into(),
        attempt_id: "att-2".into(),
        session_id: "bcode-sess-1".into(),
        generation: 1,
        text: "TRIGGER_PERMISSION".into(),
        placement: HarnessPlacement::NextTurn,
    };
    session.submit(&sub2).expect("submit 2");
    let perm_obs = drain_until(&mut session, |all| {
        all.iter()
            .any(|o| matches!(&o.kind, HarnessObservationKind::Interaction { .. }))
    });
    let request_id = perm_obs
        .iter()
        .find_map(|o| match &o.kind {
            HarnessObservationKind::Interaction { interaction } => {
                Some(interaction.request_id.clone())
            }
            _ => None,
        })
        .expect("permission interaction");

    // Busy while permission pending
    let sub_busy = HarnessSubmission {
        submission_id: "sub-busy".into(),
        attempt_id: "att-busy".into(),
        session_id: "bcode-sess-1".into(),
        generation: 1,
        text: "should fail".into(),
        placement: HarnessPlacement::NextTurn,
    };
    assert_eq!(session.submit(&sub_busy), Err(AcpError::Rejected));

    session
        .respond(&HarnessResponse {
            response_id: "resp-1".into(),
            request_id,
            session_id: "bcode-sess-1".into(),
            generation: 1,
            value: HarnessResponseValue::Approval { approved: true },
        })
        .expect("approve permission");

    let after_perm = drain_until(&mut session, |all| {
        all.iter().any(|o| {
            matches!(
                &o.kind,
                HarnessObservationKind::Execution {
                    event: HarnessExecutionEvent::SubmissionTerminal { .. }
                }
            )
        })
    });
    assert!(after_perm.iter().any(|o| matches!(
        &o.kind,
        HarnessObservationKind::ToolResult {
            is_error: false,
            ..
        }
    )));
    assert!(after_perm.iter().any(|o| matches!(
        &o.kind,
        HarnessObservationKind::Text { text, .. } if text == "DONE_APPROVED"
    )));

    // Third turn: interrupt cancels pending permission and emits InteractionWithdrawn + Interrupted
    let sub3 = HarnessSubmission {
        submission_id: "sub-3".into(),
        attempt_id: "att-3".into(),
        session_id: "bcode-sess-1".into(),
        generation: 1,
        text: "TRIGGER_CANCEL".into(),
        placement: HarnessPlacement::NextTurn,
    };
    session.submit(&sub3).expect("submit 3");
    let _ = drain_until(&mut session, |all| {
        all.iter()
            .any(|o| matches!(&o.kind, HarnessObservationKind::Interaction { .. }))
    });
    session.interrupt_active(1).expect("interrupt");
    let cancel_obs = drain_until(&mut session, |all| {
        all.iter().any(|o| {
            matches!(
                &o.kind,
                HarnessObservationKind::Execution {
                    event: HarnessExecutionEvent::SubmissionTerminal {
                        disposition: HarnessTerminalDisposition::Interrupted,
                        ..
                    }
                }
            )
        })
    });
    assert!(
        cancel_obs
            .iter()
            .any(|o| matches!(&o.kind, HarnessObservationKind::InteractionWithdrawn { .. }))
    );

    // Close and verify physical reap, then resume exact sessionId in generation 2
    let mut resume_binding = session.binding().clone();
    let reaped = session.reap_flag();
    session.send_close();
    drop(session);
    let reap_deadline = Instant::now() + Duration::from_secs(3);
    while !reaped.load(std::sync::atomic::Ordering::SeqCst) && Instant::now() < reap_deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(reaped.load(std::sync::atomic::Ordering::SeqCst));

    resume_binding.generation = 2;
    let resumed =
        AcpSession::open_with_model(&launch, resume_binding.clone(), true, None).expect("resume");
    assert_eq!(
        resumed.binding().native_session_id,
        resume_binding.native_session_id
    );
    assert_eq!(resumed.binding().generation, 2);
}

fn submission(id: &str, text: &str) -> HarnessSubmission {
    HarnessSubmission {
        submission_id: id.into(),
        attempt_id: format!("att-{id}"),
        session_id: "bcode-sess-1".into(),
        generation: 1,
        text: text.into(),
        placement: HarnessPlacement::NextTurn,
    }
}

fn is_disconnect_status(observation: &HarnessObservation) -> bool {
    matches!(
        &observation.kind,
        HarnessObservationKind::Status { status }
            if status.connectivity == HarnessConnectivity::Disconnected
    )
}

/// Observe until the disconnect status arrives, recording every page's completeness.
fn drain_until_disconnect(
    session: &mut AcpSession,
) -> (Vec<HarnessObservation>, Vec<HarnessCompleteness>) {
    let mut all = Vec::new();
    let mut completeness = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let (batch, page) = session.observe(8).expect("observe");
        completeness.push(page);
        all.extend(batch);
        if all.iter().any(is_disconnect_status) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    (all, completeness)
}

#[test]
fn process_loss_mid_turn_reports_a_gap_and_no_invented_outcome() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let launch = test_launch(tmp.path());
    let mut session =
        AcpSession::open_with_model(&launch, initial_binding(), false, None).expect("open");
    session
        .submit(&submission("crash", "TRIGGER_CRASH"))
        .expect("submit");

    let (observations, completeness) = drain_until_disconnect(&mut session);
    assert!(observations.iter().any(is_disconnect_status));
    assert!(completeness.contains(&HarnessCompleteness::Gap));
    assert!(!observations.iter().any(|o| matches!(
        &o.kind,
        HarnessObservationKind::Execution {
            event: HarnessExecutionEvent::SubmissionTerminal { .. }
        }
    )));
    let status = session.status();
    assert_eq!(status.connectivity, HarnessConnectivity::Disconnected);
    assert_eq!(status.activity, HarnessActivity::Unknown);
}

#[test]
fn output_received_before_exit_is_normalized_before_the_disconnect() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let launch = test_launch(tmp.path());
    let mut session =
        AcpSession::open_with_model(&launch, initial_binding(), false, None).expect("open");
    session
        .submit(&submission("burst", "TRIGGER_BURST_EXIT"))
        .expect("submit");

    let (observations, _) = drain_until_disconnect(&mut session);
    let terminal = observations
        .iter()
        .position(|o| {
            matches!(
                &o.kind,
                HarnessObservationKind::Execution {
                    event: HarnessExecutionEvent::SubmissionTerminal {
                        disposition: HarnessTerminalDisposition::Completed,
                        ..
                    }
                }
            )
        })
        .expect("terminal outcome survives the exit");
    let disconnect = observations
        .iter()
        .position(is_disconnect_status)
        .expect("disconnect");
    assert!(terminal < disconnect);
    let expected = "x".repeat(200);
    assert!(observations.iter().any(|o| matches!(
        &o.kind,
        HarnessObservationKind::Text { text, .. } if *text == expected
    )));
}

#[test]
fn acp_tool_kind_names_native_tools_without_an_explicit_name() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let launch = test_launch(tmp.path());
    let mut session =
        AcpSession::open_with_model(&launch, initial_binding(), false, None).expect("open");
    session
        .submit(&submission("perm", "TRIGGER_PERMISSION"))
        .expect("submit");
    let observations = drain_until(&mut session, |all| {
        all.iter()
            .any(|o| matches!(&o.kind, HarnessObservationKind::Interaction { .. }))
    });
    assert!(observations.iter().any(|o| matches!(
        &o.kind,
        HarnessObservationKind::Tool { name, .. } if name == "execute"
    )));
    assert!(observations.iter().any(|o| matches!(
        &o.kind,
        HarnessObservationKind::Interaction { interaction }
            if interaction.tool_name.as_deref() == Some("execute")
    )));
}
