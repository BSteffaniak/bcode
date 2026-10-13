//! Current canonical resource lease inspection; no historical lease coverage is claimed.
use super::representation;

pub(super) async fn inspect(
    tx: &dyn switchy_database::DatabaseTransaction,
    run: &str,
    bound: u16,
) -> (Vec<bcode_workflow::WorkflowResourceLease>, Option<bool>) {
    match representation::safety::lease_observation::page_in(tx, run, bound).await {
        Ok(page) => (page.leases, Some(page.complete)),
        Err(_) => (Vec::new(), None),
    }
}
