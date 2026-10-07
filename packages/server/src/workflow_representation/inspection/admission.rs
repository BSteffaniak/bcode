//! Bounded optional admission diagnostics; absence never hides the rest of inspection.
use bcode_workflow::{
    WorkflowAdmissionCategory, WorkflowAdmissionObservation, WorkflowAdmissionPhase,
};
use bcode_workflow_store::representation::admission_observation::{
    self, Category, Observation, Phase,
};

fn project(value: Observation) -> WorkflowAdmissionObservation {
    WorkflowAdmissionObservation {
        run_id: value.run_id,
        node_id: value.node_id,
        activation_id: value.activation_id,
        attempt: value.attempt,
        dispatch_identity: value.dispatch_identity,
        phase: match value.phase {
            Phase::OwnerDispatch => WorkflowAdmissionPhase::OwnerDispatch,
            Phase::ReceiptPersistence => WorkflowAdmissionPhase::ReceiptPersistence,
        },
        category: match value.category {
            Category::OwnerAccessDeferred => WorkflowAdmissionCategory::OwnerAccessDeferred,
            Category::OwnerError => WorkflowAdmissionCategory::OwnerError,
            Category::ReceiptPersistenceUnavailable => {
                WorkflowAdmissionCategory::ReceiptPersistenceUnavailable
            }
        },
        observed_at_ms: value.observed_at_ms,
    }
}

/// Return optional diagnostics plus explicit completeness. Never installs or repairs state.
pub async fn inspect(
    db: &dyn switchy_database::Database,
    run_id: &str,
    bound: u16,
) -> (Vec<WorkflowAdmissionObservation>, Option<bool>) {
    let limit = usize::from(bound).clamp(1, 100);
    let Ok(values) = admission_observation::inspect(db, run_id, None, limit).await else {
        return (Vec::new(), None);
    };
    let complete = if values.len() == limit {
        let after = admission_observation::cursor(values.last().expect("positive limit"));
        let Ok(extra) = admission_observation::inspect(db, run_id, Some(&after), 1).await else {
            return (Vec::new(), None);
        };
        extra.is_empty()
    } else {
        true
    };
    (values.into_iter().map(project).collect(), Some(complete))
}

#[cfg(test)]
mod tests;
