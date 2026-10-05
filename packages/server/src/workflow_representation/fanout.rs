//! Bounded fan-out materialization through canonical graph operations.
use super::{ServerError, representation};

pub(super) async fn advance(
    path: &std::path::Path,
    db: &dyn switchy_database::Database,
    authority: &bcode_workflow_store::WorkflowExecutionAuthority,
    run_id: &str,
    revision: u64,
    activation: &representation::graph::Activation,
) -> Result<bool, ServerError> {
    let created = super::backpressure::queued(path, || {
        representation::graph::fanout::materialize(
            db,
            authority,
            run_id,
            &activation.node_id,
            &activation.activation_id,
            revision,
            32,
        )
    })
    .await
    .map_err(super::backpressure::normalize)?;
    let settled = super::backpressure::queued(path, || {
        representation::graph::fanout::settle(
            db,
            authority,
            run_id,
            &activation.node_id,
            &activation.activation_id,
            revision,
        )
    })
    .await
    .map_err(super::backpressure::normalize)?;
    let pending = super::backpressure::retry(|| {
        representation::graph::fanout_progress::pending(db, run_id, &activation.activation_id)
    })
    .await
    .map_err(super::backpressure::normalize)?;
    Ok(created > 0 || settled || pending)
}
