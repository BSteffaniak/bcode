//! Behavioral application-boundary recovery and bounded hydration coverage.
use bcode_client::BcodeClient;
use bcode_ipc::{Request, Response, ResponsePayload};
use bcode_session_models::*;
use bcode_session_view_models::{SessionViewAction, SessionViewActionOutcome};

use crate::{SessionView, execute_session_view_action, hydrate_submission_queue};

fn record() -> SubmissionRecord {
    let id = SubmissionId("oldest-outside-window".to_owned());
    SubmissionRecord {
        receipt: SubmissionReceipt {
            submission_id: id.clone(),
            accepted_event_sequence: 7,
        },
        submission: PromptSubmission {
            id,
            client_id: ClientId::new(),
            text: "not resident".to_owned(),
            admission: TurnAdmissionMetadata::default(),
            requested_placement: SubmissionPlacement::FollowUp,
            target: SubmissionTarget::OwnTurn {
                turn_id: TurnId("original-target".to_owned()),
            },
            execution_descriptor: None,
        },
        status: SubmissionStatus::Pending,
        context_observation: None,
        recovery_authorization: None,
    }
}

#[tokio::test]
async fn queue_hydration_and_targeted_actions_use_only_bounded_application_operations() {
    let session_id = SessionId::new();
    let pending = record();
    let confirmation = SubmissionResumeConfirmation {
        version: 1,
        submission_id: pending.receipt.submission_id.clone(),
        expected_accepted_sequence: 7,
        descriptor: SubmissionExecutionDescriptor {
            version: 2,
            provider: "displayed-provider".to_owned(),
            model: "displayed-model".to_owned(),
            model_profile: Some("displayed-profile".to_owned()),
            auth_profile: None,
            reasoning: None,
            config_identity: "digest".to_owned(),
            recovery_recipe: None,
        },
    };
    let expected_confirmation = confirmation.clone();
    let expected_record = pending.clone();
    let socket = tempfile::tempdir().unwrap();
    let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket.path().join("queue.sock"));
    let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
    let responder = tokio::spawn(respond_to_recovery_scenario(
        listener,
        session_id,
        pending,
        confirmation,
    ));
    let client = BcodeClient::new(endpoint);
    let exercise = async {
        let mut view = SessionView::new();
        hydrate_submission_queue(&client, session_id, &mut view)
            .await
            .unwrap();
        assert!(view.snapshot().transcript.items.is_empty());
        assert!(view.snapshot().runtime.queue_recovery().unwrap().can_resume);
        let prepared = execute_session_view_action(
            &client,
            SessionViewAction::PrepareSubmissionResume {
                session_id,
                submission_id: expected_record.receipt.submission_id.clone(),
                expected_accepted_sequence: 7,
            },
        )
        .await
        .unwrap();
        let SessionViewActionOutcome::SubmissionResumePrepared { confirmation } = prepared else {
            panic!("prepare result");
        };
        assert_eq!(confirmation, expected_confirmation);
        let resumed = execute_session_view_action(
            &client,
            SessionViewAction::ResumeSubmission {
                session_id,
                confirmation,
                operation_id: "resume-operation".to_owned(),
            },
        )
        .await
        .unwrap();
        assert!(
            matches!(resumed, SessionViewActionOutcome::SubmissionRecovered { result } if !result.duplicate && result.record == expected_record)
        );
        let cancelled = execute_session_view_action(
            &client,
            SessionViewAction::CancelPendingSubmission {
                session_id,
                submission_id: expected_record.receipt.submission_id,
                expected_accepted_sequence: 7,
                operation_id: "cancel-operation".to_owned(),
            },
        )
        .await
        .unwrap();
        assert!(
            matches!(cancelled, SessionViewActionOutcome::SubmissionRecovered { result } if result.duplicate && matches!(result.record.status, SubmissionStatus::Cancelled { .. }))
        );
        hydrate_submission_queue(&client, session_id, &mut view)
            .await
            .unwrap();
        assert!(view.snapshot().runtime.queue_recovery().is_none());
        responder.await.unwrap();
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), exercise)
        .await
        .unwrap();
}

fn recovery_test_hello() -> ResponsePayload {
    ResponsePayload::Hello {
        protocol_version: bcode_ipc::ProtocolVersion::current(),
        client_id: ClientId::new(),
        daemon: bcode_ipc::DaemonStatus {
            namespace: bcode_ipc::daemon_namespace(),
            protocol_version: u32::from(bcode_ipc::ProtocolVersion::current().0),
            artifact_id: Some(bcode_ipc::ArtifactId::current()),
            executable_digest: Some(
                bcode_daemon_lifecycle::current_executable_identity()
                    .unwrap()
                    .1,
            ),
            build_fingerprint: bcode_ipc::BUILD_FINGERPRINT.to_owned(),
            storage_writer_epoch: Some(CURRENT_SESSION_STORAGE_WRITER_EPOCH),
            session_event_schema_version: Some(CURRENT_SESSION_EVENT_SCHEMA_VERSION),
            state_location_id: Some(bcode_ipc::state_location_id()),
            ..bcode_ipc::DaemonStatus::default()
        },
    }
}

async fn respond_to_recovery_scenario(
    listener: bcode_ipc::LocalIpcListener,
    session_id: SessionId,
    pending: SubmissionRecord,
    confirmation: SubmissionResumeConfirmation,
) {
    let mut stage = 0;
    while stage < 5 {
        let mut stream = listener.accept().await.unwrap();
        loop {
            let Ok(envelope) = bcode_ipc::recv_envelope(&mut stream).await else {
                break;
            };
            let request = bcode_ipc::decode_request(&envelope.payload).unwrap();
            let payload = match request {
                Request::Hello { .. } => recovery_test_hello(),
                Request::SubmissionQueueStatus {
                    session_id: requested,
                } => {
                    assert_eq!(requested, session_id);
                    assert!(stage == 0 || stage == 4);
                    stage += 1;
                    ResponsePayload::SubmissionQueueStatus {
                        status: if stage == 1 {
                            SubmissionQueueStatus {
                                oldest: Some(pending.clone()),
                                blocker: Some(SubmissionQueueBlocker::MissingContext),
                            }
                        } else {
                            SubmissionQueueStatus {
                                oldest: None,
                                blocker: None,
                            }
                        },
                    }
                }
                Request::PrepareSubmissionResume {
                    session_id: requested,
                    submission_id,
                    expected_accepted_sequence,
                } => {
                    assert_eq!(stage, 1);
                    assert_eq!(requested, session_id);
                    assert_eq!(submission_id, pending.receipt.submission_id);
                    assert_eq!(expected_accepted_sequence, 7);
                    stage += 1;
                    ResponsePayload::SubmissionResumePrepared {
                        confirmation: confirmation.clone(),
                    }
                }
                Request::ResumeSubmission {
                    session_id: requested,
                    confirmation: received,
                    operation_id,
                } => {
                    assert_eq!(stage, 2);
                    assert_eq!(requested, session_id);
                    assert_eq!(received, confirmation);
                    assert_eq!(operation_id, "resume-operation");
                    stage += 1;
                    ResponsePayload::SubmissionRecovered {
                        result: SubmissionRecoveryResult {
                            record: pending.clone(),
                            duplicate: false,
                        },
                    }
                }
                Request::CancelPendingSubmission {
                    session_id: requested,
                    submission_id,
                    expected_accepted_sequence,
                    operation_id,
                } => {
                    assert_eq!(stage, 3);
                    assert_eq!(requested, session_id);
                    assert_eq!(submission_id, pending.receipt.submission_id);
                    assert_eq!(expected_accepted_sequence, 7);
                    assert_eq!(operation_id, "cancel-operation");
                    stage += 1;
                    let mut cancelled = pending.clone();
                    cancelled.status = SubmissionStatus::Cancelled { event_sequence: 10 };
                    ResponsePayload::SubmissionRecovered {
                        result: SubmissionRecoveryResult {
                            record: cancelled,
                            duplicate: true,
                        },
                    }
                }
                other => panic!(
                    "recovery must not scan history, change settings, or grant permissions: {other:?}"
                ),
            };
            let reply =
                bcode_ipc::response_envelope(envelope.request_id, &Response::Ok(payload)).unwrap();
            bcode_ipc::send_envelope(&mut stream, &reply).await.unwrap();
            if stage == 5 {
                break;
            }
        }
    }
}
