//! Canonical adapter contract tests. These fixtures never invoke native programs.
use bcode_harness_models::*;
use bcode_session_models::{
    SessionId,
    external_harness::{ExternalHarnessOwner, ExternalHarnessRequest, ExternalHarnessResponse},
};

fn request() -> HarnessRequest {
    let session_id = SessionId::new().to_string();
    HarnessRequest {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: "open".into(),
        instance: HarnessInstance {
            instance_id: "instance".into(),
            plugin_id: "fixture".into(),
            executable: "/not/invoked".into(),
            config_namespace: "/fixture".into(),
            auth: HarnessAuthBinding {
                provider_id: "fixture".into(),
                profile: "native".into(),
                credential_namespace: "fixture".into(),
                custody: HarnessAuthCustody::NativeManaged,
            },
        },
        kind: HarnessRequestKind::Open(HarnessOpenRequest {
            open_id: "open".into(),
            session_id,
            generation: 1,
            working_directory: "/workspace".into(),
            resume: None,
            parent_assistance: None,
        }),
    }
}

#[test]
fn canonical_delivery_contract_retains_full_open_and_rejects_auth() {
    let mut input = request();
    let retained = ExternalHarnessRequest::from_request(&input).unwrap();
    assert_eq!(
        serde_json::to_value(retained.request()).unwrap(),
        serde_json::to_value(&input).unwrap()
    );
    input.kind = HarnessRequestKind::AuthStatus { verify: false };
    assert!(ExternalHarnessRequest::from_request(&input).is_err());
    input.kind = HarnessRequestKind::Inspect;
    assert!(ExternalHarnessRequest::from_request(&input).is_err());
}

#[test]
fn recorded_result_preserves_correlation_without_native_redispatch() {
    let input = request();
    let HarnessRequestKind::Open(open) = &input.kind else {
        panic!("fixture")
    };
    let response = HarnessServiceResponse {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: input.request_id.clone(),
        operation: HarnessOperation::Open,
        kind: HarnessServiceResponseKind::Opened(HarnessSessionBinding {
            schema_version: HARNESS_SCHEMA_VERSION,
            session_id: open.session_id.clone(),
            instance_id: input.instance.instance_id.clone(),
            native_session_id: "native-observed".into(),
            native_version: "fixture-v1".into(),
            generation: 1,
        }),
    };
    let retained = ExternalHarnessResponse::from_response(&input, &response).unwrap();
    retained.response().validate_for(&input).unwrap();
    assert_eq!(
        serde_json::to_value(retained.response()).unwrap(),
        serde_json::to_value(response).unwrap()
    );
}

#[test]
fn generation_is_not_a_substitute_for_full_owner_identity() {
    let owner = ExternalHarnessOwner {
        session_id: SessionId::new(),
        generation: 1,
        artifact_id: "artifact".into(),
        daemon_id: "daemon-a".into(),
    };
    owner.validate().unwrap();
    let mut other = owner.clone();
    other.daemon_id = "daemon-b".into();
    assert_ne!(owner, other);
    other = owner.clone();
    other.artifact_id = "different-artifact".into();
    assert_ne!(owner, other);
}

#[tokio::test]
async fn prepared_confirmation_question_and_lifecycle_require_exact_live_cursor() {
    Box::pin(check_prepared_observation_cursor()).await;
}

async fn record_prepared_submission(
    sessions: &bcode_session::SessionManager,
    owner: &ExternalHarnessOwner,
    instance: &HarnessInstance,
    prepared: &HarnessPreparedSession,
) {
    let submission = HarnessRequest {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: "submit".into(),
        instance: instance.clone(),
        kind: HarnessRequestKind::Submit {
            target: HarnessSessionTarget::Prepared(prepared.clone()),
            submission: HarnessSubmission {
                submission_id: "submit".into(),
                attempt_id: "attempt".into(),
                session_id: owner.session_id.to_string(),
                generation: owner.generation,
                text: "fixture-only".into(),
                placement: HarnessPlacement::NextTurn,
            },
        },
    };
    sessions
        .claim_external_harness_request(owner, &submission)
        .await
        .unwrap();
    sessions
        .complete_external_harness_request(
            owner,
            &submission,
            &HarnessServiceResponse {
                schema_version: HARNESS_SCHEMA_VERSION,
                request_id: "submit".into(),
                operation: HarnessOperation::Submit,
                kind: HarnessServiceResponseKind::Receipt(HarnessReceipt {
                    submission_id: "submit".into(),
                    attempt_id: "attempt".into(),
                    session_id: owner.session_id.to_string(),
                    generation: owner.generation,
                    state: HarnessDeliveryState::NativeAccepted,
                    native_request_id: None,
                }),
            },
        )
        .await
        .unwrap();
}

async fn prepared_observation_fixture() -> (
    tempfile::TempDir,
    bcode_session::SessionManager,
    ExternalHarnessOwner,
    HarnessRequest,
    HarnessPreparedSession,
) {
    use bcode_session::{SessionManager, lease::SessionLeaseOwnerContext};
    let root = tempfile::tempdir().unwrap();
    let sessions = SessionManager::persistent_with_metrics_and_lease_owner(
        root.path(),
        bcode_metrics::MetricsRegistry::disabled(),
        SessionLeaseOwnerContext {
            build_fingerprint: Some("artifact".into()),
            daemon_instance_id: Some("daemon".into()),
            ..SessionLeaseOwnerContext::default()
        },
    )
    .unwrap();
    let session = sessions
        .create_session(None, root.path().into())
        .await
        .unwrap();
    let owner = ExternalHarnessOwner {
        session_id: session.id,
        generation: 1,
        artifact_id: "artifact".into(),
        daemon_id: "daemon".into(),
    };
    let mut opening = request();
    let HarnessRequestKind::Open(open) = &mut opening.kind else {
        panic!("fixture")
    };
    open.session_id = session.id.to_string();
    open.working_directory = root.path().to_str().unwrap().into();
    let capability = HarnessCapability {
        support: HarnessSupportLevel::Supported,
        constraints: vec![],
    };
    let capabilities = HarnessCapabilities {
        resume: capability.clone(),
        steering: capability.clone(),
        approval: capability.clone(),
        questions: capability.clone(),
        assistance: capability.clone(),
        interrupt: capability.clone(),
        authoritative_settlement: capability.clone(),
        background_continuation: capability.clone(),
        authentication: capability,
    };
    sessions
        .prepare_external_harness_open(&owner, &opening, capabilities, None)
        .await
        .unwrap();
    sessions
        .claim_external_harness_request(&owner, &opening)
        .await
        .unwrap();
    let prepared = HarnessPreparedSession {
        open_id: opening.request_id.clone(),
        session_id: session.id.to_string(),
        instance_id: opening.instance.instance_id.clone(),
        generation: 1,
        native_version: "fixture-v1".into(),
        requested_native_session_id: None,
    };
    sessions
        .complete_external_harness_request(
            &owner,
            &opening,
            &HarnessServiceResponse {
                schema_version: HARNESS_SCHEMA_VERSION,
                request_id: opening.request_id.clone(),
                operation: HarnessOperation::Open,
                kind: HarnessServiceResponseKind::Prepared(prepared.clone()),
            },
        )
        .await
        .unwrap();
    Box::pin(record_prepared_submission(
        &sessions,
        &owner,
        &opening.instance,
        &prepared,
    ))
    .await;
    (root, sessions, owner, opening, prepared)
}

async fn check_prepared_observation_cursor() {
    let (root, sessions, owner, opening, prepared) = Box::pin(prepared_observation_fixture()).await;
    let binding = HarnessSessionBinding {
        schema_version: HARNESS_SCHEMA_VERSION,
        session_id: owner.session_id.to_string(),
        instance_id: prepared.instance_id.clone(),
        generation: 1,
        native_version: prepared.native_version.clone(),
        native_session_id: "native".into(),
    };
    let first = confirmation_question_page(&owner, &prepared, &binding);
    sessions
        .commit_external_harness_page(&owner, None, &first)
        .await
        .unwrap();
    sessions
        .commit_external_harness_page(&owner, None, &first)
        .await
        .unwrap();
    let current = sessions.external_harness_state(&owner).await.unwrap();
    assert_eq!(
        current.target,
        Some(HarnessSessionTarget::Confirmed(binding.clone()))
    );
    acknowledge_answer_and_interrupt(&sessions, &owner, &opening.instance, binding).await;
    assert_settlement_cursor(&sessions, &owner, &first).await;
    sessions
        .release_idle_session_resources(owner.session_id)
        .await
        .unwrap();
    drop(sessions);
    drop(root);
}

fn confirmation_question_page(
    owner: &ExternalHarnessOwner,
    prepared: &HarnessPreparedSession,
    binding: &HarnessSessionBinding,
) -> HarnessObservationPage {
    let status = HarnessExecutionStatus {
        connectivity: HarnessConnectivity::Connected,
        activity: HarnessActivity::Running,
        attention: HarnessAttention::None,
        continuation: HarnessContinuation::Available,
        completeness: HarnessCompleteness::Complete,
    };
    let event = |id: &str, kind| HarnessObservation {
        observation_id: id.into(),
        session_id: owner.session_id.to_string(),
        generation: 1,
        native_correlations: vec![],
        completeness: HarnessCompleteness::Complete,
        kind,
    };
    HarnessObservationPage {
        observations: vec![
            event(
                "confirmation",
                HarnessObservationKind::BindingConfirmed {
                    open_id: prepared.open_id.clone(),
                    binding: binding.clone(),
                },
            ),
            event(
                "question",
                HarnessObservationKind::Interaction {
                    interaction: HarnessInteraction {
                        request_id: "native-question".into(),
                        session_id: owner.session_id.to_string(),
                        generation: 1,
                        kind: HarnessInteractionKind::Question,
                        prompt: "Choose".into(),
                        scope: "native question".into(),
                        choices: vec![],
                    },
                },
            ),
            event(
                "running",
                HarnessObservationKind::Status {
                    status: status.clone(),
                },
            ),
        ],
        next_after: Some("running".into()),
        completeness: HarnessCompleteness::Complete,
        status,
    }
}

async fn acknowledge_answer_and_interrupt(
    sessions: &bcode_session::SessionManager,
    owner: &ExternalHarnessOwner,
    instance: &HarnessInstance,
    binding: HarnessSessionBinding,
) {
    let answer = HarnessRequest {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: "answer".into(),
        instance: instance.clone(),
        kind: HarnessRequestKind::Respond {
            target: HarnessSessionTarget::Confirmed(binding.clone()),
            response: HarnessResponse {
                response_id: "answer".into(),
                request_id: "native-question".into(),
                session_id: owner.session_id.to_string(),
                generation: 1,
                value: HarnessResponseValue::Answer {
                    choice_id: None,
                    text: "safe".into(),
                },
            },
        },
    };
    let interrupt = HarnessRequest {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: "interrupt".into(),
        instance: instance.clone(),
        kind: HarnessRequestKind::Interrupt(HarnessControlRequest {
            target: HarnessSessionTarget::Confirmed(binding),
            mutation_id: "interrupt".into(),
        }),
    };
    for request in [&answer, &interrupt] {
        sessions
            .claim_external_harness_request(owner, request)
            .await
            .unwrap();
        sessions
            .complete_external_harness_request(
                owner,
                request,
                &HarnessServiceResponse {
                    schema_version: HARNESS_SCHEMA_VERSION,
                    request_id: request.request_id.clone(),
                    operation: request.kind.operation(),
                    kind: HarnessServiceResponseKind::Acknowledged {
                        mutation_id: request.request_id.clone(),
                    },
                },
            )
            .await
            .unwrap();
    }
}

async fn assert_settlement_cursor(
    sessions: &bcode_session::SessionManager,
    owner: &ExternalHarnessOwner,
    first: &HarnessObservationPage,
) {
    let event = |id: &str, kind| HarnessObservation {
        observation_id: id.into(),
        session_id: owner.session_id.to_string(),
        generation: 1,
        native_correlations: vec![],
        completeness: HarnessCompleteness::Complete,
        kind,
    };
    let mut repeated_prefix = first.clone();
    repeated_prefix.observations.push(event(
        "settled",
        HarnessObservationKind::Settlement {
            native_turn_id: "turn".into(),
        },
    ));
    repeated_prefix.next_after = Some("settled".into());
    assert!(
        sessions
            .commit_external_harness_page(owner, None, &repeated_prefix)
            .await
            .is_err()
    );
    let next = HarnessObservationPage {
        observations: vec![event(
            "settled",
            HarnessObservationKind::Settlement {
                native_turn_id: "turn".into(),
            },
        )],
        next_after: Some("settled".into()),
        completeness: HarnessCompleteness::Complete,
        status: first.status.clone(),
    };
    sessions
        .commit_external_harness_page(owner, first.next_after.as_deref(), &next)
        .await
        .unwrap();
}
