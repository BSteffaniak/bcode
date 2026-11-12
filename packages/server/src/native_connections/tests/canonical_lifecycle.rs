//! Canonical managed-session admission/runtime, using synthetic native I/O only.
use super::*;
use bcode_harness_models::*;
use bcode_ipc::harness::{MANAGED_SESSION_VERSION, ManagedSessionCall, ManagedSessionOperation};
use bcode_session_models::{ClientId, SessionId};

fn state(root: &std::path::Path) -> Arc<ServerState> {
    let mut state = managed_open::catalog_state(root);
    state.sessions = bcode_session::SessionManager::persistent_with_metrics_and_lease_owner(
        root.join("sessions"),
        bcode_metrics::MetricsRegistry::disabled(),
        bcode_session::lease::SessionLeaseOwnerContext {
            build_fingerprint: Some(state.daemon_status.build_fingerprint.clone()),
            daemon_instance_id: Some(state.daemon_status.instance_id.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    state.daemon_status.artifact_id = Some(bcode_ipc::ArtifactId::current());
    let state = Arc::new(state);
    crate::harness::install_canonical_backend(&state).unwrap();
    state
}
async fn dispatch(
    state: &Arc<ServerState>,
    client: ClientId,
    operation: ManagedSessionOperation,
    request: &HarnessRequest,
) -> Result<HarnessServiceResponse, bcode_ipc::ErrorResponse> {
    let call = ManagedSessionCall {
        session_id: None,
        harness: serde_json::from_value(serde_json::to_value(request).unwrap()).unwrap(),
        parent_session_id: None,
        assistance_destination_session_id: None,
    };
    let response = crate::harness_dispatch::dispatch(
        state,
        crate::harness_dispatch::ManagedCaller::Client(client),
        bcode_ipc::ManagedSessionRequest {
            schema_version: MANAGED_SESSION_VERSION,
            operation,
            request_json: serde_json::to_string(&call).unwrap(),
        },
    )
    .await?;
    Ok(serde_json::from_str(&response.response_json).unwrap())
}

#[tokio::test]
async fn published_native_connection_enters_canonical_managed_start_and_fences_stale_owner() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    pipeline::approve(&state).await;
    assert!(matches!(
        execute(&state, pipeline::prepare(1)).await,
        NativeConnectionResponse::Record { .. }
    ));
    let catalog = crate::plugin_operations::call_service(
        &state,
        "bcode.harness.configuration/v1",
        "catalog".into(),
        b"{}".to_vec(),
    )
    .await
    .unwrap();
    let catalog: serde_json::Value = serde_json::from_slice(&catalog.payload).unwrap();
    let instance: HarnessInstance =
        serde_json::from_value(catalog["instances"]["fixture-instance"].clone()).unwrap();
    let client = ClientId::new();
    let session = SessionId::new();
    let opening = HarnessRequest {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: "canonical-open".into(),
        instance,
        kind: HarnessRequestKind::Open(HarnessOpenRequest {
            open_id: "canonical-open".into(),
            session_id: session.to_string(),
            generation: 1,
            working_directory: root.path().to_str().unwrap().into(),
            resume: None,
            parent_assistance: None,
        }),
    };
    assert!(
        dispatch(&state, client, ManagedSessionOperation::Start, &opening)
            .await
            .is_err(),
        "no authenticated client context means no creation"
    );
    assert!(state.sessions.session_summary(session).await.is_err());
    state
        .set_client_runtime_context(
            client,
            Some(bcode_ipc::ClientRuntimeContext {
                working_directory: Some(root.path().to_path_buf()),
                ..Default::default()
            }),
        )
        .await;
    let opened = dispatch(&state, client, ManagedSessionOperation::Start, &opening)
        .await
        .unwrap();
    opened.validate_for(&opening).unwrap();
    let HarnessServiceResponseKind::Opened(binding) = opened.kind else {
        panic!("canonical Open must confirm native binding");
    };
    let snapshot = state
        .sessions
        .session_external_harness_state(session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        snapshot.target,
        Some(HarnessSessionTarget::Confirmed(binding.clone()))
    );
    assert_eq!(snapshot.instance, opening.instance);
    assert_eq!(snapshot.owner.daemon_id, state.daemon_status.instance_id);
    assert!(
        state
            .sessions
            .read_external_harness_retained_request(session, &opening)
            .await
            .unwrap()
            .is_some(),
        "Open delivery/result are canonical, not only a plugin response"
    );
    Box::pin(interaction_and_controls(&state, client, &opening, binding)).await;
    drop(state);
}

async fn interaction_and_controls(
    state: &Arc<ServerState>,
    client: ClientId,
    opening: &HarnessRequest,
    binding: HarnessSessionBinding,
) {
    let session: SessionId = binding.session_id.parse().unwrap();
    let target = HarnessSessionTarget::Confirmed(binding.clone());
    let respond = HarnessRequest {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: "approval-response".into(),
        instance: opening.instance.clone(),
        kind: HarnessRequestKind::Respond {
            target: target.clone(),
            response: HarnessResponse {
                response_id: "approval-response".into(),
                request_id: "synthetic-approval".into(),
                session_id: session.to_string(),
                generation: 1,
                value: HarnessResponseValue::Approval { approved: false },
            },
        },
    };
    assert!(
        dispatch(state, client, ManagedSessionOperation::Respond, &respond)
            .await
            .is_err(),
        "unobserved native approval grants no response authority"
    );
    let observe = HarnessRequest {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: "observe-approval".into(),
        instance: opening.instance.clone(),
        kind: HarnessRequestKind::Observe(HarnessObserveRequest {
            target: target.clone(),
            retention: None,
            after: None,
            limit: 16,
            wait_millis: 0,
        }),
    };
    let observed = dispatch(state, client, ManagedSessionOperation::Inspect, &observe)
        .await
        .unwrap();
    assert!(matches!(
        observed.kind,
        HarnessServiceResponseKind::Observations(_)
    ));
    let response = dispatch(state, client, ManagedSessionOperation::Respond, &respond)
        .await
        .unwrap();
    assert!(matches!(
        response.kind,
        HarnessServiceResponseKind::Acknowledged { .. }
    ));
    assert!(
        state
            .sessions
            .read_external_harness_retained_request(session, &respond)
            .await
            .unwrap()
            .is_some()
    );
    let cancel = HarnessRequest {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: "canonical-interrupt".into(),
        instance: opening.instance.clone(),
        kind: HarnessRequestKind::Interrupt(HarnessControlRequest {
            target: target.clone(),
            mutation_id: "canonical-interrupt".into(),
        }),
    };
    let cancelled = dispatch(state, client, ManagedSessionOperation::Interrupt, &cancel)
        .await
        .unwrap();
    assert!(matches!(
        cancelled.kind,
        HarnessServiceResponseKind::Acknowledged { .. }
    ));
    assert!(
        state
            .sessions
            .read_external_harness_retained_request(session, &cancel)
            .await
            .unwrap()
            .is_some()
    );
    recovery_and_fencing(state, client, opening, binding).await;
}

async fn recovery_and_fencing(
    state: &Arc<ServerState>,
    client: ClientId,
    opening: &HarnessRequest,
    binding: HarnessSessionBinding,
) {
    let session: SessionId = binding.session_id.parse().unwrap();
    let recovery = bcode_ipc::harness::ManagedSessionRecoveryRequest {
        session_id: session,
        instance_id: "fixture-instance".into(),
        expected_generation: 1,
        recovery_id: "no-release-proof".into(),
    };
    assert!(
        crate::harness_dispatch::dispatch(
            state,
            crate::harness_dispatch::ManagedCaller::Client(client),
            bcode_ipc::ManagedSessionRequest {
                schema_version: MANAGED_SESSION_VERSION,
                operation: ManagedSessionOperation::Recover,
                request_json: serde_json::to_string(&recovery).unwrap()
            }
        )
        .await
        .is_err(),
        "interrupt acknowledgement is neither transport-release nor recovery authority"
    );
    let mut foreign_generation = binding.clone();
    foreign_generation.generation += 1;
    let interrupt = HarnessRequest {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: "stale-interrupt".into(),
        instance: opening.instance.clone(),
        kind: HarnessRequestKind::Interrupt(HarnessControlRequest {
            target: HarnessSessionTarget::Confirmed(foreign_generation),
            mutation_id: "stale-interrupt".into(),
        }),
    };
    assert!(
        dispatch(
            state,
            client,
            ManagedSessionOperation::Interrupt,
            &interrupt
        )
        .await
        .is_err()
    );
    assert!(
        state
            .sessions
            .read_external_harness_retained_request(session, &interrupt)
            .await
            .unwrap()
            .is_none(),
        "stale input must not be claimed or delivered"
    );
    assert_eq!(
        state
            .sessions
            .session_external_harness_state(session)
            .await
            .unwrap()
            .unwrap()
            .target,
        Some(HarnessSessionTarget::Confirmed(binding))
    );
}
