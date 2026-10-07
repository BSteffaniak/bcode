//! Representation-aware plugin preparation gating; plugin semantics stay in the existing owner.
use super::{RunLookup, ServerState, lookup_at, retained_store};
use bcode_workflow_store::WorkflowStoreError;

pub async fn check_supported(
    state: &ServerState,
    run_id: &str,
    block: &bcode_workflow::WorkflowBlockDefinition,
) -> Result<bool, WorkflowStoreError> {
    let retained = retained_store(state).await.map_err(|_| unavailable())?;
    match lookup_at(retained.path(), run_id)
        .await
        .map_err(|_| unavailable())?
    {
        RunLookup::Legacy => Ok(false),
        RunLookup::Available(_)
            if block.effect == bcode_workflow::WorkflowBlockEffect::ReadOnly
                && block.authorization.capability
                    <= bcode_workflow::WorkflowToolCapability::ReadOnly
                && !block.authorization.explicit_grant_required
                && !block.preparation_required =>
        {
            Ok(true)
        }
        _ => Err(unavailable()),
    }
}

fn unavailable() -> WorkflowStoreError {
    WorkflowStoreError::InvalidData(
        "workflow plugin authorization is unsupported for this representation".into(),
    )
}
