//! Incremental parallel-controller observation, independent of activation readiness.
use super::{ServerError, representation};

#[derive(Default)]
pub(super) struct Scan {
    after: Option<String>,
    revision: Option<representation::graph::parallel_progress::ScanVersion>,
    reduction_pending: bool,
    finished: bool,
}
impl Scan {
    pub(super) async fn advance(
        &mut self,
        path: &std::path::Path,
        db: &dyn switchy_database::Database,
        authority: &bcode_workflow_store::WorkflowExecutionAuthority,
        run_id: &str,
    ) -> Result<(bool, bool), ServerError> {
        let version = super::backpressure::retry(|| {
            representation::graph::parallel_progress::scan_version(db, run_id)
        })
        .await
        .map_err(super::backpressure::normalize)?;
        let revision = version.revision;
        if self.revision != Some(version) {
            self.after = None;
            self.revision = Some(version);
            self.reduction_pending = false;
            self.finished = false;
        }
        if self.finished {
            return Ok((false, false));
        }
        let nodes = super::backpressure::retry(|| {
            representation::graph_pages::nodes(db, run_id, revision, self.after.as_deref(), 32)
        })
        .await
        .map_err(super::backpressure::normalize)?;
        let next = nodes.last().map(|node| node.id.clone());
        let mut progressed = false;
        let mut pending = false;
        for node in nodes {
            if node.kind != bcode_workflow::NodeKind::Parallel {
                continue;
            }
            progressed |= super::backpressure::queued(path, || {
                representation::graph::parallel::settle(db, authority, run_id, &node.id, revision)
            })
            .await
            .map_err(super::backpressure::normalize)?;
            pending |= super::backpressure::retry(|| {
                representation::graph::parallel_progress::pending(db, run_id, &node.id, revision)
            })
            .await
            .map_err(super::backpressure::normalize)?;
        }
        self.reduction_pending |= pending;
        let scan_pending = next.is_some();
        let reduction_pending = self.reduction_pending;
        if !scan_pending {
            self.finished = !reduction_pending;
            self.reduction_pending = false;
        }
        self.after = next;
        Ok((progressed, scan_pending || reduction_pending))
    }
}

pub(super) async fn signal_cancellations(
    state: &super::ServerState,
    db: &dyn switchy_database::Database,
    authority: &bcode_workflow_store::WorkflowExecutionAuthority,
    run_id: &str,
    cursor: &mut Option<String>,
) -> Result<bool, ServerError> {
    use representation::attempts::owner_boundary;
    let (receipts, next) = super::backpressure::retry(|| {
        owner_boundary::parallel_cancellation_receipts(db, run_id, authority, cursor.as_deref(), 32)
    })
    .await
    .map_err(super::backpressure::normalize)?;
    for receipt in receipts {
        super::backpressure::retry(|| owner_boundary::verify(db, run_id, authority))
            .await
            .map_err(super::backpressure::normalize)?;
        crate::signal_workflow_attempt_cancellation(
            state,
            &bcode_workflow_store::ActiveAttemptCancellation {
                run_id: receipt.run_id,
                node_id: receipt.node_id,
                activation_id: receipt.activation_id,
                attempt: receipt.attempt,
                dispatch_identity: receipt.dispatch_identity,
                receipt: Some(receipt.receipt),
            },
        )
        .await?;
    }
    *cursor = next;
    Ok(cursor.is_some())
}
