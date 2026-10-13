//! Snapshot-backed canonical unresolved gate waits; incompatible state remains unavailable.
use super::representation;
use bcode_workflow::{WaitingActivation, WorkflowWaitKind};
#[cfg(test)]
mod tests;

pub(super) async fn inspect(
    tx: &dyn switchy_database::DatabaseTransaction,
    run: &str,
    revision: u64,
    bound: u16,
) -> (Vec<WaitingActivation>, Option<bool>) {
    let Ok(page) =
        representation::graph::gate_waits::page_in(tx, run, Some(revision), None, bound).await
    else {
        return (Vec::new(), None);
    };
    let complete = page.next_node_id.is_none();
    let waits = page
        .gates
        .into_iter()
        .map(|gate| WaitingActivation {
            run_id: run.into(),
            node_id: gate.node_id,
            activation_id: gate.activation_id,
            kind: match gate.kind {
                representation::graph::gate_waits::GateKind::Input => WorkflowWaitKind::Input,
                representation::graph::gate_waits::GateKind::Approval => WorkflowWaitKind::Approval,
            },
            input: Some(gate.input),
            requested_at_ms: gate.requested_at_ms,
        })
        .collect();
    (waits, Some(complete))
}
