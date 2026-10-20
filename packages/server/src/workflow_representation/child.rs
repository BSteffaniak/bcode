use super::representation::hierarchy::ChildResult;
use super::{
    ServerError, ServerState, open, recovery, representation, retained_store, unavailable,
    unsupported,
};
use bcode_workflow_store::AttemptObservation as Observation;

pub async fn observe(
    state: &ServerState,
    request: &bcode_workflow_store::AttemptReconciliationRequest,
) -> Result<Option<bcode_workflow_store::AttemptObservation>, ServerError> {
    if !recovery::is_represented(state, &request.run_id).await? {
        return Ok(None);
    }
    let authority = super::owner::local_authority(state, &request.run_id)
        .await?
        .ok_or_else(unsupported)?;
    let retained = retained_store(state).await?;
    let db = open(retained.path(), false)?;
    let node = super::backpressure::retry(|| {
        representation::graph::execution_node(&db, &request.run_id, &request.node_id)
    })
    .await
    .map_err(|_| unavailable())?;
    let result = super::backpressure::retry(|| {
        representation::hierarchy::child_result(
            &db,
            &authority.authority,
            &request.run_id,
            &request.node_id,
            &request.activation_id,
            request.attempt,
        )
    })
    .await
    .map_err(|_| unavailable())?;
    Ok(Some(match result {
        None | Some(ChildResult::Pending) => Observation::Running,
        Some(ChildResult::Completed(value)) => Observation::Succeeded {
            output: bcode_workflow_store::ValidatedOutput {
                output_id: format!("{}:child-output", request.activation_id),
                run_id: request.run_id.clone(),
                node_id: request.node_id.clone(),
                activation_id: request.activation_id.clone(),
                schema_id: node.output.type_name,
                schema_version: 1,
                value,
                artifact_reference: None,
                created_at_ms: crate::current_unix_millis(),
            },
        },
        Some(ChildResult::Failed) => Observation::Failed {
            message: "child workflow failed".into(),
        },
        Some(ChildResult::Cancelled) => Observation::Cancelled,
    }))
}

pub async fn dispatch(
    state: &std::sync::Arc<ServerState>,
    request: &bcode_workflow_store::PreparedActivationDispatch,
) -> Result<Option<serde_json::Value>, ServerError> {
    if !recovery::is_represented(state, &request.activation.run_id).await? {
        return Ok(None);
    }
    let guard = super::owner::local_authority(state, &request.activation.run_id)
        .await?
        .ok_or_else(unsupported)?;
    let retained = retained_store(state).await?;
    let db = open(retained.path(), true)?;
    let context = super::backpressure::retry(|| {
        representation::hierarchy::parent_context(&db, &guard.authority, &request.activation.run_id)
    })
    .await
    .map_err(|_| unavailable())?;
    let config: bcode_workflow::WorkflowCallConfiguration =
        serde_json::from_value(request.activation.node.configuration.clone())
            .map_err(|_| unsupported())?;
    let binding =
        representation::admission_projection::package_binding(&db, &request.activation.run_id)
            .await
            .map_err(|_| unsupported())?;
    let identity = representation::hierarchy::resolve_call(&db, binding.as_ref(), &config.target)
        .await
        .map_err(|_| unsupported())?;
    let stored = retained
        .definition(&identity.definition_id, identity.definition_version)?
        .ok_or_else(unavailable)?;
    let definition: bcode_workflow::WorkflowDefinition =
        serde_json::from_str(&stored.definition_json).map_err(|_| unavailable())?;
    crate::workflow_operations::validate_workflow_definition_for_production(state, &definition)?;
    let child_id = bcode_workflow_store::workflow_child_run_id(
        &context.root_run_id,
        &request.activation.run_id,
        &request.activation.activation_id,
        request.attempt,
        &config.target,
    );
    let now = crate::current_unix_millis();
    let mut child = context.admission;
    child.run_id = child_id.clone();
    child.definition_id = identity.definition_id.clone();
    child.definition_version = identity.definition_version;
    child.input = request.activation.input.clone();
    child.created_at_ms = now;
    child.binding = None;
    child.authored_provenance = None;
    child.execution_authority = Some(guard.authority.clone());
    child.limits = context.effective_limits;
    let link = bcode_workflow_store::WorkflowRunLink {
        version: bcode_workflow_store::WORKFLOW_RUN_LINK_VERSION,
        root_run_id: context.root_run_id,
        parent_run_id: request.activation.run_id.clone(),
        parent_node_id: request.activation.node_id.clone(),
        parent_activation_id: request.activation.activation_id.clone(),
        parent_attempt: request.attempt,
        child_run_id: child_id.clone(),
        target: config.target,
        depth: u32::try_from(context.depth.checked_add(1).ok_or_else(unsupported)?)
            .map_err(|_| unsupported())?,
        created_at_ms: now,
    };
    super::backpressure::retry(|| {
        representation::hierarchy::admit_child(&db, &guard.authority, &link, &child, &definition)
    })
    .await
    .map_err(|_| unavailable())?;
    if let Some(sender) = state.workflow_driver_sender.get() {
        let _ = sender.try_send(child_id.clone());
    }
    Ok(Some(
        serde_json::json!({"owner":"bcode.server.workflow-child/v1", "child_run_id":child_id}),
    ))
}

/// Read the canonical parent only; discovery grants no execution authority.
pub async fn parent_run(state: &ServerState, run_id: &str) -> Result<Option<String>, ServerError> {
    let retained = retained_store(state).await?;
    if !recovery::is_represented(state, run_id).await? {
        return Ok(retained
            .parent_run_link(run_id)?
            .map(|link| link.parent_run_id));
    }
    let guard = super::owner::local_authority(state, run_id)
        .await?
        .ok_or_else(unsupported)?;
    let db = open(retained.path(), false)?;
    let context = super::backpressure::retry(|| {
        representation::hierarchy::parent_context(&db, &guard.authority, run_id)
    })
    .await
    .map_err(|_| unavailable())?;
    Ok(context.parent_link.map(|link| link.parent_run_id))
}
