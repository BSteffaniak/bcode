//! Actual repeat records with independent source coverage and bounded exhaustion.
use super::representation;

pub(super) async fn inspect(
    tx: &dyn switchy_database::DatabaseTransaction,
    run: &str,
    bound: u16,
) -> (
    Vec<bcode_workflow::WorkflowRepeatOutcomeSummary>,
    Option<bool>,
) {
    let coverage = representation::journal::public_events::repeat::coverage(tx, run).await;
    match representation::graph::repeat_outcome::page_in(tx, run, None, bound).await {
        Ok((records, next)) => {
            let complete = coverage.ok().map(|coverage| {
                coverage == bcode_workflow::WorkflowSemanticHistoryCoverage::CompleteFromAdmission
                    && next.is_none()
            });
            (records, complete)
        }
        Err(_) => (Vec::new(), None),
    }
}
