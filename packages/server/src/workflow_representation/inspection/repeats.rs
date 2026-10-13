//! Actual repeat records remain visible without pretending that source history is complete.
use super::representation;

pub(super) async fn inspect(
    tx: &dyn switchy_database::DatabaseTransaction,
    run: &str,
    bound: u16,
) -> Vec<bcode_workflow::WorkflowRepeatOutcomeSummary> {
    representation::graph::repeat_outcome::page_in(tx, run, None, bound)
        .await
        .map_or_else(|_| Vec::new(), |(records, _)| records)
}
