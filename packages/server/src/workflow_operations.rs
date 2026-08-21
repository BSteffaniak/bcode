//! Transport-neutral application operations for workflow inspection.

use super::ServerState;

/// Return bounded workflow run summaries without transport framing.
pub fn list_runs(
    state: &ServerState,
    limit: usize,
) -> Result<Vec<bcode_workflow_store::WorkflowRunSummary>, bcode_workflow_store::WorkflowStoreError>
{
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .list_runs(limit)
}

/// Return checksum-verified bounded workflow outputs without transport framing.
pub fn run_outputs(
    state: &ServerState,
    run_id: &str,
    limit: usize,
) -> Result<Vec<bcode_ipc::WorkflowOutputInspection>, bcode_workflow_store::WorkflowStoreError> {
    let store = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let checksums = store
        .output_summaries(run_id, limit)?
        .into_iter()
        .map(|output| (output.output_id, output.checksum_sha256))
        .collect::<std::collections::BTreeMap<_, _>>();
    Ok(store
        .validated_outputs(run_id, limit)?
        .into_iter()
        .map(|output| bcode_ipc::WorkflowOutputInspection {
            version: bcode_ipc::WORKFLOW_OUTPUT_INSPECTION_VERSION,
            checksum_sha256: checksums
                .get(&output.output_id)
                .cloned()
                .expect("validated output has a matching bounded summary"),
            output_id: output.output_id,
            run_id: output.run_id,
            node_id: output.node_id,
            activation_id: output.activation_id,
            schema_id: output.schema_id,
            schema_version: output.schema_version,
            value: output.value,
            artifact_reference: output.artifact_reference,
            created_at_ms: output.created_at_ms,
        })
        .collect())
}

/// Return one bounded workflow run inspection without transport framing.
pub async fn inspect_run(
    state: &ServerState,
    run_id: &str,
    limit: usize,
) -> Result<bcode_ipc::WorkflowRunInspection, super::ServerError> {
    super::workflow_run_inspection(state, run_id, limit).await
}

/// Return bounded workflow definitions without transport framing.
pub fn list_definitions(
    state: &ServerState,
    limit: usize,
) -> Result<
    Vec<bcode_workflow_store::StoredWorkflowDefinition>,
    bcode_workflow_store::WorkflowStoreError,
> {
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .list_definitions(limit)
}

/// Return one versioned workflow definition without transport framing.
pub fn describe_definition(
    state: &ServerState,
    definition_id: &str,
    version: u32,
) -> Result<
    Option<bcode_workflow_store::StoredWorkflowDefinition>,
    bcode_workflow_store::WorkflowStoreError,
> {
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .definition(definition_id, version)
}
