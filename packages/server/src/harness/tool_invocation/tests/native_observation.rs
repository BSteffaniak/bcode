//! Canonical approval and native observation preceding the real host-tool queue.
use super::*;

pub(super) async fn observe_native_read(
    state: &Arc<ServerState>,
    caller: &ManagedCaller,
    call: &ManagedSessionCall,
) {
    let session = call.session_id.unwrap();
    let snapshot = state
        .sessions
        .session_external_harness_state(session)
        .await
        .unwrap()
        .unwrap();
    let owner = snapshot.owner;
    let directory = state
        .sessions
        .session_working_directory(session)
        .await
        .unwrap();
    let opening = opening_request(session, &call.harness.instance, &directory);
    complete(
        state,
        &owner,
        &opening,
        HarnessServiceResponseKind::Opened(HarnessSessionBinding {
            schema_version: HARNESS_SCHEMA_VERSION,
            session_id: session.to_string(),
            instance_id: opening.instance.instance_id.clone(),
            native_session_id: "fixture-native".into(),
            native_version: "fixture".into(),
            generation: owner.generation,
        }),
    )
    .await;
    let selected = state.sessions.external_harness_state(&owner).await.unwrap();
    let target = selected.target.unwrap();
    let mut submission =
        serde_json::from_value::<HarnessRequest>(serde_json::to_value(&call.harness).unwrap())
            .unwrap();
    submission.request_id = "fixture-submit".into();
    submission.kind = HarnessRequestKind::Submit {
        target: target.clone(),
        submission: HarnessSubmission {
            submission_id: "fixture-submit".into(),
            attempt_id: "fixture-attempt".into(),
            session_id: session.to_string(),
            generation: owner.generation,
            text: "offline fixture".into(),
            placement: HarnessPlacement::NextTurn,
        },
    };
    complete(
        state,
        &owner,
        &submission,
        HarnessServiceResponseKind::Receipt(HarnessReceipt {
            submission_id: "fixture-submit".into(),
            attempt_id: "fixture-attempt".into(),
            session_id: session.to_string(),
            generation: owner.generation,
            state: HarnessDeliveryState::NativeAccepted,
            native_request_id: None,
        }),
    )
    .await;
    Box::pin(approve_and_observe(state, caller, call, &owner, target)).await;
}

async fn approve_and_observe(
    state: &Arc<ServerState>,
    caller: &ManagedCaller,
    call: &ManagedSessionCall,
    owner: &ExternalHarnessOwner,
    target: HarnessSessionTarget,
) {
    let session = owner.session_id;
    let page = approval_page(owner);
    state
        .sessions
        .commit_external_harness_page(owner, None, &page)
        .await
        .unwrap();
    let mut response =
        serde_json::from_value::<HarnessRequest>(serde_json::to_value(&call.harness).unwrap())
            .unwrap();
    response.request_id = "fixture-response".into();
    response.kind = HarnessRequestKind::Respond {
        target,
        response: HarnessResponse {
            response_id: response.request_id.clone(),
            request_id: "native-approval".into(),
            session_id: session.to_string(),
            generation: owner.generation,
            value: HarnessResponseValue::Approval { approved: true },
        },
    };
    // Use ordinary authenticated dispatch: the fixture acknowledges native delivery only.
    crate::harness_dispatch::dispatch(
        state,
        caller.clone(),
        bcode_ipc::ManagedSessionRequest {
            schema_version: bcode_ipc::harness::MANAGED_SESSION_VERSION,
            operation: bcode_ipc::harness::ManagedSessionOperation::Respond,
            request_json: serde_json::to_string(&ManagedSessionCall {
                session_id: Some(session),
                harness: response,
                parent_session_id: None,
                assistance_destination_session_id: None,
            })
            .unwrap(),
        },
    )
    .await
    .unwrap();
    let mut observed = page.clone();
    observed.observations[0].observation_id = "fixture-tool-observed".into();
    observed.observations[0].kind = HarnessObservationKind::Tool {
        native_tool_id: "native-shaped-file-read".into(),
        name: "mcp__bcode-tools__filesystem_read".into(),
        status: "observed".into(),
    };
    observed.next_after = Some("fixture-tool-observed".into());
    state
        .sessions
        .commit_external_harness_page(owner, page.next_after.as_deref(), &observed)
        .await
        .unwrap();
}

async fn complete(
    state: &Arc<ServerState>,
    owner: &ExternalHarnessOwner,
    request: &HarnessRequest,
    kind: HarnessServiceResponseKind,
) {
    state
        .sessions
        .claim_external_harness_request(owner, request)
        .await
        .unwrap();
    state
        .sessions
        .complete_external_harness_request(
            owner,
            request,
            &HarnessServiceResponse {
                schema_version: HARNESS_SCHEMA_VERSION,
                request_id: request.request_id.clone(),
                operation: request.kind.operation(),
                kind,
            },
        )
        .await
        .unwrap();
}

pub(super) fn opening_request(
    session: SessionId,
    instance: &HarnessInstance,
    directory: &std::path::Path,
) -> HarnessRequest {
    HarnessRequest {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: "fixture-open".into(),
        instance: instance.clone(),
        kind: HarnessRequestKind::Open(HarnessOpenRequest {
            open_id: "fixture-open".into(),
            session_id: session.to_string(),
            generation: 1,
            working_directory: directory.to_str().unwrap().into(),
            resume: None,
            parent_assistance: None,
        }),
    }
}

fn approval_page(owner: &ExternalHarnessOwner) -> HarnessObservationPage {
    let session = owner.session_id;
    let status = HarnessExecutionStatus {
        connectivity: HarnessConnectivity::Connected,
        activity: HarnessActivity::Running,
        attention: HarnessAttention::None,
        continuation: HarnessContinuation::Available,
        completeness: HarnessCompleteness::Complete,
    };
    HarnessObservationPage {
        retention: None,
        hydration_requests: vec![],
        inventory_page: None,
        observations: vec![HarnessObservation {
            observation_id: "fixture-approval".into(),
            session_id: session.to_string(),
            generation: owner.generation,
            native_correlations: vec![],
            completeness: HarnessCompleteness::Complete,
            kind: HarnessObservationKind::Interaction {
                interaction: HarnessInteraction {
                    request_id: "native-approval".into(),
                    session_id: session.to_string(),
                    generation: owner.generation,
                    kind: HarnessInteractionKind::Approval,
                    prompt: "Allow native wrapper?".into(),
                    scope: "mcp__bcode-tools__filesystem_read".into(),
                    choices: vec![],
                },
            },
        }],
        next_after: Some("fixture-approval".into()),
        completeness: HarnessCompleteness::Complete,
        status,
    }
}
