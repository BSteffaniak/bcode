//! Canonical output inspection routed through the owning representation.
use super::{
    RunLookup, ServerError, ServerState, lookup_at, open, representation, retained_store,
    unavailable, unsupported,
};

use switchy_database::Database as _;

#[cfg(test)]
mod tests;

/// Read one generation-fenced canonical inventory page through the owning store.
/// This verifies current facts, not semantic history or full capture applicability.
/// # Errors
/// Rejects invalid queries, unsupported representations, stale pins and damaged facts.
pub async fn inventory_page(
    state: &ServerState,
    query: &bcode_workflow::WorkflowInventoryQuery,
) -> Result<bcode_workflow::WorkflowInventoryPage, ServerError> {
    query.validate().map_err(|_| unavailable())?;
    let retained = retained_store(state).await?;
    match lookup_at(retained.path(), &query.run_id).await? {
        RunLookup::Available(_) => {}
        RunLookup::Legacy | RunLookup::UnsupportedRepresentation => return Err(unsupported()),
        RunLookup::Missing | RunLookup::Degraded => return Err(unavailable()),
    }
    let db = open(retained.path(), false)?;
    super::backpressure::retry(|| representation::inventory::page(&db, query))
        .await
        .map_err(|_| unavailable())
}

/// Read a bounded canonical output prefix without repair, migration or authority acquisition.
/// # Errors
/// Rejects unavailable, damaged or unsupported representations and invalid extension page limits.
pub async fn run_outputs(
    state: &ServerState,
    run_id: &str,
    limit: usize,
) -> Result<Vec<bcode_workflow::WorkflowOutputInspection>, ServerError> {
    let retained = retained_store(state).await?;
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
    let tx = db.begin_transaction().await.map_err(|_| unavailable())?;
    let values = super::backpressure::retry(|| {
        representation::graph::completion::outputs_in(&*tx, run_id, None, limit)
    })
    .await
    .map_err(|_| unavailable())?;
    let mut inspected = Vec::new();
    for output in values {
        inspected.push(
            representation::graph::completion::proven_output(&*tx, run_id, &output.output_id)
                .await
                .map_err(|_| unavailable())?
                .ok_or_else(unavailable)?,
        );
    }
    tx.commit().await.map_err(|_| unavailable())?;
    Ok(inspected)
}
