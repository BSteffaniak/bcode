//! Normal application controls routed through canonical representation and owner fencing.
use super::{
    ServerError, ServerState, open, recovery, representation, retained_store, unavailable,
    unsupported,
};

pub async fn pause(
    state: &ServerState,
    run_id: &str,
    paused: bool,
) -> Result<Option<bool>, ServerError> {
    if !recovery::is_represented(state, run_id).await? {
        return Ok(None);
    }
    let gate = crate::workflow_drive_gate(state, run_id);
    let guard = gate.lock().await;
    let authority = super::owner::local_authority(state, run_id)
        .await?
        .ok_or_else(unsupported)?;
    let retained = retained_store(state).await?;
    let db = open(retained.path(), true)?;
    let expected_revision = representation::graph_pages::revision(&db, run_id)
        .await
        .map_err(|_| unavailable())?;
    let wake = if paused {
        None
    } else {
        Some(
            state
                .workflow_driver_sender
                .get()
                .ok_or_else(unsupported)?
                .try_reserve()
                .map_err(|_| unsupported())?,
        )
    };
    let changed = representation::controls::set_pause(
        &db,
        &authority.authority,
        &representation::controls::PauseRequest {
            version: 1,
            run_id: run_id.into(),
            expected_revision,
            paused,
        },
    )
    .await
    .map_err(|_| unavailable())?;
    drop(db);
    drop(guard);
    if let Some(wake) = wake {
        wake.send(run_id.to_owned());
    }
    Ok(Some(changed))
}

pub async fn resolve_gate(
    state: &std::sync::Arc<ServerState>,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
    decision: representation::controls::GateDecision,
) -> Result<Option<bcode_workflow::WaitingResolutionResult>, ServerError> {
    if !recovery::is_represented(state, run_id).await? {
        return Ok(None);
    }
    let gate = crate::workflow_drive_gate(state, run_id);
    let guard = gate.lock().await;
    let authority = super::owner::local_authority(state, run_id)
        .await?
        .ok_or_else(unsupported)?;
    let retained = retained_store(state).await?;
    let db = open(retained.path(), true)?;
    let expected_revision = representation::graph_pages::revision(&db, run_id)
        .await
        .map_err(|_| unavailable())?;
    let result = representation::controls::resolve_gate(
        &db,
        &authority.authority,
        &representation::controls::GateRequest {
            version: 1,
            run_id: run_id.into(),
            node_id: node_id.into(),
            activation_id: activation_id.into(),
            expected_revision,
            decision,
        },
    )
    .await
    .map_err(|_| unavailable())?;
    let result = bcode_workflow::WaitingResolutionResult {
        run_id: run_id.into(),
        node_id: node_id.into(),
        activation_id: activation_id.into(),
        outcome: if result.denied { "denied" } else { "accepted" }.into(),
        activated: result
            .activated
            .into_iter()
            .map(|activation| bcode_workflow::NewActivation {
                run_id: run_id.into(),
                node_id: activation.node_id,
                activation_id: activation.activation_id,
                dependency_generation: activation.generation,
                input: Some(activation.input),
                created_at_ms: activation.created_at_ms,
            })
            .collect(),
        run_status: match result.run_status {
            representation::runs::RunState::Paused => bcode_workflow::RunStatus::Paused,
            representation::runs::RunState::Completed => bcode_workflow::RunStatus::Completed,
            representation::runs::RunState::Failed => bcode_workflow::RunStatus::Failed,
            representation::runs::RunState::Cancelled => bcode_workflow::RunStatus::Cancelled,
        },
    };
    drop(db);
    drop(guard);
    crate::drive_workflow_run(state, run_id).await?;
    Ok(Some(result))
}

/// Increase only the explicit execution allowance, with an atomic changed result.
pub async fn increase_allowance(
    state: &std::sync::Arc<ServerState>,
    run_id: &str,
    expected: u64,
    target: u64,
) -> Result<Option<bool>, ServerError> {
    if !recovery::is_represented(state, run_id).await? {
        return Ok(None);
    }
    let guard = super::owner::local_authority(state, run_id)
        .await?
        .ok_or_else(unsupported)?;
    state.start_workflow_driver().await;
    let permit = state
        .workflow_driver_sender
        .get()
        .ok_or_else(unsupported)?
        .try_reserve()
        .map_err(|_| unsupported())?;
    let retained = retained_store(state).await?;
    let db = open(retained.path(), true)?;
    let now = crate::current_unix_millis();
    let changed = super::backpressure::queued(retained.path(), || {
        representation::allowance::increase_execution_allowance(
            &db,
            run_id,
            &guard.authority,
            expected,
            target,
            now,
        )
    })
    .await
    .map_err(super::backpressure::normalize)?;
    permit.send(run_id.to_owned());
    Ok(Some(changed))
}

/// Schedule an exact operator-authorized retry, never replay a previous dispatch.
pub async fn retry_node(
    state: &std::sync::Arc<ServerState>,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
    failed_attempt: u32,
) -> Result<Option<bcode_workflow::WorkflowNodeRetryResult>, ServerError> {
    if !recovery::is_represented(state, run_id).await? {
        return Ok(None);
    }
    let gate = crate::workflow_drive_gate(state, run_id);
    let _exclusive = gate.lock().await;
    let guard = super::owner::local_authority(state, run_id)
        .await?
        .ok_or_else(unsupported)?;
    state.start_workflow_driver().await;
    let permit = state
        .workflow_driver_sender
        .get()
        .ok_or_else(unsupported)?
        .try_reserve()
        .map_err(|_| unsupported())?;
    let retained = retained_store(state).await?;
    let db = open(retained.path(), true)?;
    let result = super::backpressure::queued(retained.path(), || {
        representation::attempts::owner_boundary::schedule_retry(
            &db,
            &guard.authority,
            run_id,
            node_id,
            activation_id,
            failed_attempt,
        )
    })
    .await
    .map_err(super::backpressure::normalize)?;
    permit.send(run_id.into());
    Ok(Some(result))
}
