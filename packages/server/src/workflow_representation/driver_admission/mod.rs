//! Secret-safe admission failure recording; does not reconcile or replay admission.
use super::super::{ServerError, ServerState, open, retained_store};
use bcode_workflow_store::representation::admission_observation::{
    self, Category, Observation, Phase,
};
use bcode_workflow_store::{
    PreparedActivationDispatch, WorkflowExecutionAuthority, WorkflowStoreError,
};

pub const fn owner_category(error: &WorkflowStoreError) -> Category {
    if matches!(error, WorkflowStoreError::OwnerAccessDeferred) {
        Category::OwnerAccessDeferred
    } else {
        Category::OwnerError
    }
}

pub async fn record(
    state: &ServerState,
    authority: &WorkflowExecutionAuthority,
    dispatch: &PreparedActivationDispatch,
    phase: Phase,
    category: Category,
) -> Result<(), ServerError> {
    let retained = retained_store(state).await?;
    let db = open(retained.path(), true)?;
    let observation = Observation {
        version: admission_observation::VERSION,
        run_id: dispatch.activation.run_id.clone(),
        node_id: dispatch.activation.node_id.clone(),
        activation_id: dispatch.activation.activation_id.clone(),
        attempt: dispatch.attempt,
        dispatch_identity: dispatch.dispatch_identity.clone(),
        phase,
        category,
        observed_at_ms: crate::current_unix_millis(),
    };
    admission_observation::record(&db, authority, &observation)
        .await
        .map_err(super::super::backpressure::normalize)
}

#[cfg(test)]
mod tests;
