//! Canonical output inspection routed through the owning representation.
use super::{
    RunLookup, ServerError, ServerState, lookup_at, open, representation, retained_store,
    unavailable, unsupported,
};

#[cfg(test)]
mod tests;

/// Read a bounded canonical output prefix without repair, migration or authority acquisition.
/// # Errors
/// Rejects unavailable, damaged or unsupported representations and invalid extension page limits.
pub async fn run_outputs(
    state: &ServerState,
    run_id: &str,
    limit: usize,
) -> Result<Vec<bcode_workflow::WorkflowOutputInspection>, ServerError> {
    let retained = retained_store(state)?;
    match lookup_at(retained.path(), run_id).await? {
        RunLookup::Legacy | RunLookup::Missing => {
            return crate::workflow_operations::run_outputs(state, run_id, limit)
                .map_err(Into::into);
        }
        RunLookup::Available(_) => {}
        RunLookup::UnsupportedRepresentation => return Err(unsupported()),
        RunLookup::Degraded => return Err(unavailable()),
    }
    let limit = u16::try_from(limit)
        .ok()
        .filter(|value| (1..=100).contains(value))
        .ok_or_else(unavailable)?;
    let db = open(retained.path(), false)?;
    super::backpressure::retry(|| {
        representation::graph::completion::outputs(&db, run_id, None, limit)
    })
    .await
    .map_err(|_| unavailable())?
    .into_iter()
    .map(inspect)
    .collect()
}

fn inspect(
    output: bcode_workflow_store::ValidatedOutput,
) -> Result<bcode_workflow::WorkflowOutputInspection, ServerError> {
    let checksum_sha256 = bcode_workflow::workflow_canonical_value_sha256(&output.value)
        .map_err(|_| unavailable())?;
    Ok(bcode_workflow::WorkflowOutputInspection {
        version: bcode_workflow::WORKFLOW_OUTPUT_INSPECTION_VERSION,
        checksum_sha256,
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
}
