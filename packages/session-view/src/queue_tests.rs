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

fn private_queue_ipc_fixture() -> (
    tempfile::TempDir,
    bcode_ipc::IpcEndpoint,
    bcode_ipc::LocalIpcListener,
) {
    // Keep the Unix socket path short and resolve aliases such as macOS /tmp.
    let socket = tempfile::tempdir_in("/tmp").unwrap();
    // Let strict IPC create its own private parent instead of trusting tempfile's mode.
    let endpoint = bcode_ipc::IpcEndpoint::unix_socket(
        socket
            .path()
            .canonicalize()
            .unwrap()
            .join("ipc")
            .join("queue.sock"),
    );
    let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
    (socket, endpoint, listener)
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
    let (_socket, endpoint, listener) = private_queue_ipc_fixture();
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
        assert_eq!(
            view.snapshot().transcript.items,
            [] as [bcode_session_view_models::TranscriptViewItem; 0]
        );
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
        let ended = execute_session_view_action(
            &client,
            SessionViewAction::EndInterruptedTurn {
                session_id,
                turn_id: "inspected-interrupted-turn".into(),
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            ended,
            SessionViewActionOutcome::Cancelled { cancelled: true }
        ));
        responder.await.unwrap();
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), exercise)
        .await
        .unwrap();
}

fn managed_composer_target(
    session_id: SessionId,
) -> bcode_ipc::harness::ManagedSessionTargetSnapshot {
    use bcode_harness_models::{
        HarnessAuthBinding, HarnessAuthCustody, HarnessInstance, HarnessPreparedSession,
        HarnessSessionTarget,
    };
    use bcode_ipc::harness::ManagedSessionTargetSnapshot;
    ManagedSessionTargetSnapshot {
        session_id,
        instance: HarnessInstance {
            instance_id: "native".into(),
            plugin_id: "native.plugin".into(),
            executable: "/native".into(),
            config_namespace: "isolated".into(),
            auth: HarnessAuthBinding {
                provider_id: "native".into(),
                profile: "work".into(),
                credential_namespace: "isolated".into(),
                custody: HarnessAuthCustody::NativeManaged,
            },
        },
        target: HarnessSessionTarget::Prepared(HarnessPreparedSession {
            open_id: "open".into(),
            session_id: session_id.to_string(),
            instance_id: "native".into(),
            generation: 3,
            native_version: "1".into(),
            requested_native_session_id: None,
        }),
    }
}

fn managed_composer_delivery_response(
    request: &bcode_ipc::harness::ManagedSessionRequest,
    expected: &bcode_ipc::harness::ManagedSessionTargetSnapshot,
    exact: &mut Option<String>,
    wrong_generation: bool,
) -> String {
    use bcode_harness_models::{
        HarnessDeliveryState, HarnessReceipt, HarnessRequestKind, HarnessServiceResponse,
        HarnessServiceResponseKind,
    };
    use bcode_ipc::harness::ManagedSessionOperation;
    let session_id = expected.session_id;
    assert_eq!(request.operation, ManagedSessionOperation::Followup);
    let call = request.decode_call().unwrap();
    assert_eq!(call.session_id, Some(session_id));
    assert!(call.parent_session_id.is_none());
    let HarnessRequestKind::Submit { target, submission } = &call.harness.kind else {
        panic!("submit only");
    };
    assert_eq!(target, &expected.target);
    assert_eq!(submission.submission_id, "composer-submit");
    assert_eq!(submission.attempt_id, "composer-attempt");
    assert_eq!(submission.generation, 3);
    assert_eq!(submission.text, "ordinary native input");
    assert_eq!(
        submission.placement,
        bcode_harness_models::HarnessPlacement::NextTurn
    );
    if let Some(exact) = exact.as_ref() {
        assert_eq!(&request.request_json, exact);
    } else {
        *exact = Some(request.request_json.clone());
    }
    serde_json::to_string(&HarnessServiceResponse {
        schema_version: 1,
        request_id: call.harness.request_id.clone(),
        operation: bcode_harness_models::HarnessOperation::Submit,
        kind: HarnessServiceResponseKind::Receipt(HarnessReceipt {
            submission_id: submission.submission_id.clone(),
            attempt_id: submission.attempt_id.clone(),
            session_id: submission.session_id.clone(),
            generation: if wrong_generation { 4 } else { 3 },
            state: HarnessDeliveryState::Uncertain,
            native_request_id: None,
        }),
    })
    .unwrap()
}

async fn serve_managed_composer_scenario(
    listener: bcode_ipc::LocalIpcListener,
    expected: bcode_ipc::harness::ManagedSessionTargetSnapshot,
) {
    use bcode_ipc::harness::{ManagedSessionOperation, ManagedSessionResponse};
    let session_id = expected.session_id;
    let mut stage = 0;
    let mut exact = None;
    while stage < 6 {
        let mut stream = listener.accept().await.unwrap();
        loop {
            let Ok(envelope) = bcode_ipc::recv_envelope(&mut stream).await else {
                break;
            };
            let payload = match bcode_ipc::decode_request(&envelope.payload).unwrap() {
                Request::Hello { .. } => recovery_test_hello(),
                Request::ManagedSession { request } => {
                    let response_json = match stage {
                        0..=2 => {
                            assert_eq!(request.operation, ManagedSessionOperation::Binding);
                            let query: serde_json::Value =
                                serde_json::from_str(&request.request_json).unwrap();
                            assert_eq!(query["session_id"], session_id.to_string());
                            serde_json::to_string(&Some(&expected)).unwrap()
                        }
                        3..=5 => managed_composer_delivery_response(
                            &request,
                            &expected,
                            &mut exact,
                            stage == 5,
                        ),
                        _ => unreachable!(),
                    };
                    stage += 1;
                    ResponsePayload::ManagedSession {
                        response: ManagedSessionResponse {
                            schema_version: 1,
                            response_json,
                        },
                    }
                }
                other => panic!(
                    "managed composer must not change model selections or dispatch model work: {other:?}"
                ),
            };
            bcode_ipc::send_envelope(
                &mut stream,
                &bcode_ipc::response_envelope(envelope.request_id, &Response::Ok(payload)).unwrap(),
            )
            .await
            .unwrap();
            if stage == 6 {
                break;
            }
        }
    }
}

#[tokio::test]
async fn managed_composer_shared_actions_capture_then_dispatch_exact_uncertain_attempt() {
    use bcode_harness_models::HarnessDeliveryState;
    let (_socket, endpoint, listener) = private_queue_ipc_fixture();
    let session_id = SessionId::new();
    let backend = tokio::spawn(serve_managed_composer_scenario(
        listener,
        managed_composer_target(session_id),
    ));
    let client = BcodeClient::new(endpoint);
    // Older callers without a captured native envelope fail closed instead of invoking a model.
    assert!(
        execute_session_view_action(
            &client,
            SessionViewAction::SubmitMessage {
                submission_id: SubmissionId("legacy".into()),
                session_id: Some(session_id),
                launch_working_directory: None,
                text: "must not execute".into(),
                placement: PromptPlacement::FollowUp,
                execution: Box::default(),
            }
        )
        .await
        .is_err()
    );
    assert!(
        execute_session_view_action(
            &client,
            SessionViewAction::PrepareComposerSubmission {
                session_id,
                submission_id: "missing-turn".into(),
                attempt_id: "missing-turn-attempt".into(),
                text: "must not steer".into(),
                placement: PromptPlacement::Steering,
                expected_native_turn_id: None,
            }
        )
        .await
        .is_err()
    );
    let prepared = execute_session_view_action(
        &client,
        SessionViewAction::PrepareComposerSubmission {
            session_id,
            submission_id: "composer-submit".into(),
            attempt_id: "composer-attempt".into(),
            text: "ordinary native input".into(),
            placement: PromptPlacement::FollowUp,
            expected_native_turn_id: None,
        },
    )
    .await
    .unwrap();
    let SessionViewActionOutcome::SubmissionPrepared {
        managed: Some(retained),
    } = prepared
    else {
        panic!("captured target required");
    };
    for _ in 0..2 {
        let outcome = execute_session_view_action(
            &client,
            SessionViewAction::SubmitManagedMessage {
                request: retained.clone(),
            },
        )
        .await
        .unwrap();
        let SessionViewActionOutcome::ManagedSubmissionRecorded { response } = outcome else {
            panic!("native receipt, not model acceptance");
        };
        let ExternalHarnessResponseKind::Receipt(receipt) = response.kind else {
            panic!("receipt required");
        };
        assert_eq!(receipt.state, HarnessDeliveryState::Uncertain);
        assert_eq!(receipt.attempt_id, "composer-attempt");
    }
    assert!(
        execute_session_view_action(
            &client,
            SessionViewAction::SubmitManagedMessage { request: retained }
        )
        .await
        .is_err(),
        "foreign-generation receipt must fail, never become model acceptance"
    );
    tokio::time::timeout(std::time::Duration::from_secs(10), backend)
        .await
        .unwrap()
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

fn interrupted_cancel_response(
    stage: usize,
    requested: SessionId,
    session_id: SessionId,
    expected_turn_id: Option<&str>,
    clear_queue: bool,
) {
    assert_eq!(stage, 5);
    assert_eq!(requested, session_id);
    assert_eq!(expected_turn_id, Some("inspected-interrupted-turn"));
    assert!(!clear_queue);
}

fn queue_response(stage: usize, pending: &SubmissionRecord) -> ResponsePayload {
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

async fn respond_to_recovery_scenario(
    listener: bcode_ipc::LocalIpcListener,
    session_id: SessionId,
    pending: SubmissionRecord,
    confirmation: SubmissionResumeConfirmation,
) {
    let stage = std::sync::Arc::new(std::sync::Mutex::new(0));
    let mut connections = tokio::task::JoinSet::new();
    loop {
        let mut stream = tokio::select! {
            stream = listener.accept() => stream.unwrap(),
            completed = connections.join_next(), if !connections.is_empty() => {
                if completed.unwrap().unwrap() { return; }
                continue;
            }
        };
        let stage = stage.clone();
        let pending = pending.clone();
        let confirmation = confirmation.clone();
        connections.spawn(async move {
            loop {
                let Ok(envelope) = bcode_ipc::recv_envelope(&mut stream).await else {
                    break;
                };
                let request = bcode_ipc::decode_request(&envelope.payload).unwrap();
                let (payload, complete) = {
                    let mut stage_guard = stage.lock().unwrap();
                    let result = recovery_scenario_response(
                        request,
                        session_id,
                        &pending,
                        &confirmation,
                        &mut stage_guard,
                    );
                    drop(stage_guard);
                    result
                };
                let reply =
                    bcode_ipc::response_envelope(envelope.request_id, &Response::Ok(payload))
                        .unwrap();
                bcode_ipc::send_envelope(&mut stream, &reply).await.unwrap();
                if complete {
                    return true;
                }
            }
            false
        });
    }
}

fn recovery_scenario_response(
    request: Request,
    session_id: SessionId,
    pending: &SubmissionRecord,
    confirmation: &SubmissionResumeConfirmation,
    stage: &mut usize,
) -> (ResponsePayload, bool) {
    let payload = match request {
        Request::Hello { .. } => recovery_test_hello(),
        Request::SubmissionQueueStatus {
            session_id: requested,
        } => {
            assert_eq!(requested, session_id);
            assert!(*stage == 0 || *stage == 4);
            *stage += 1;
            queue_response(*stage, pending)
        }
        Request::PrepareSubmissionResume {
            session_id: requested,
            submission_id,
            expected_accepted_sequence,
        } => {
            assert_eq!(*stage, 1);
            assert_eq!(requested, session_id);
            assert_eq!(submission_id, pending.receipt.submission_id);
            assert_eq!(expected_accepted_sequence, 7);
            *stage += 1;
            ResponsePayload::SubmissionResumePrepared {
                confirmation: confirmation.clone(),
            }
        }
        Request::ResumeSubmission {
            session_id: requested,
            confirmation: received,
            operation_id,
        } => {
            assert_eq!(*stage, 2);
            assert_eq!(requested, session_id);
            assert_eq!(&received, confirmation);
            assert_eq!(operation_id, "resume-operation");
            *stage += 1;
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
            assert_eq!(*stage, 3);
            assert_eq!(requested, session_id);
            assert_eq!(submission_id, pending.receipt.submission_id);
            assert_eq!(expected_accepted_sequence, 7);
            assert_eq!(operation_id, "cancel-operation");
            *stage += 1;
            let mut cancelled = pending.clone();
            cancelled.status = SubmissionStatus::Cancelled { event_sequence: 10 };
            ResponsePayload::SubmissionRecovered {
                result: SubmissionRecoveryResult {
                    record: cancelled,
                    duplicate: true,
                },
            }
        }
        Request::CancelSessionTurn {
            session_id: requested,
            expected_turn_id,
            clear_queue,
        } => {
            interrupted_cancel_response(
                *stage,
                requested,
                session_id,
                expected_turn_id.as_deref(),
                clear_queue,
            );
            *stage += 1;
            ResponsePayload::TurnCancellationRequested { cancelled: true }
        }
        other => panic!(
            "recovery must not scan history, change settings, or grant permissions: {other:?}"
        ),
    };
    (payload, *stage == 6)
}
