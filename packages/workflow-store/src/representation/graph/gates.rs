//! Explicit owner-fenced gate resolution; authorization belongs to the caller.
use super::{
    ACTIVATIONS, Database, DatabaseValue, Error, FilterableQuery, GRAPHS, NodeKind, RUNS,
    TransactionMode, Value, WorkflowExecutionAuthority, activation, decode, encode, schema, text,
};

/// Resolve an input gate or approved approval gate at its exact activation identity.
///
/// `None` is an approval grant only when the canonical node is Approval. Input gates require
/// a supplied value. This API never grants tool capability or resolves denial as success.
/// # Errors
/// Rejects stale revision/owner/identity, cancellation, wrong gate kind, conflicting duplicate,
/// invalid supplied/transformed output schemas or storage failures.
pub async fn resolve(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    run_id: &str,
    node_id: &str,
    id: &str,
    revision: u64,
    supplied: Option<&Value>,
) -> Result<bool, Error> {
    Ok(
        !resolve_observed(db, owner, run_id, node_id, id, revision, supplied)
            .await?
            .duplicate,
    )
}
/// Actual bounded transactional gate effects; pending propagation remains durable frontier work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateResolution {
    /// Identical decision previously committed.
    pub duplicate: bool,
    /// Explicit denial, not successful output.
    pub denied: bool,
    /// Activations created in this transaction only.
    pub activated: Vec<super::Activation>,
    /// Actual persisted root status, not inferred terminalization.
    pub run_status: super::super::runs::RunState,
}
fn duplicate_resolution(root: &switchy_database::Row) -> Result<GateResolution, Error> {
    Ok(GateResolution {
        duplicate: true,
        denied: false,
        activated: Vec::new(),
        run_status: super::super::runs::RunState::decode(&text(root, "status")?)?,
    })
}

/// Resolve a gate and report actual effects.
/// # Errors
/// Rejects invalid identity, schema, owner, revision, conflicting duplicate or storage failure.
pub async fn resolve_observed(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    run_id: &str,
    node_id: &str,
    id: &str,
    revision: u64,
    supplied: Option<&Value>,
) -> Result<GateResolution, Error> {
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await?;
    super::super::verified(&*tx).await?;
    let roots = tx
        .select(RUNS)
        .where_eq("run_id", run_id)
        .limit(1)
        .execute(&*tx)
        .await?;
    let root = roots.first().ok_or(Error::InvalidAdmission)?;
    if text(root, "authority_json")? != encode(owner)? {
        return Err(Error::AuthorityConflict);
    }
    let current = super::super::graph_pages::revision(&*tx, run_id).await?;
    let boundary = super::super::graph_edges::boundary(&*tx, run_id, current).await?;
    if current != revision {
        return Err(Error::InvalidAdmission);
    }
    let row = activation(&*tx, run_id, node_id)
        .await?
        .ok_or(Error::InvalidAdmission)?;
    if text(&row, "identity")? != id {
        return Err(Error::InvalidAdmission);
    }
    let node = super::super::graph_pages::node(&*tx, run_id, current, node_id)
        .await?
        .ok_or(Error::InvalidAdmission)?;
    let retained: Value = decode(&row, "input_json")?;
    let value = match (node.kind, supplied) {
        (NodeKind::Approval, None) => retained.clone(),
        (NodeKind::Input, Some(supplied)) => {
            if let Some(resolution) = node.configuration.get("input_resolution") {
                let resolution: bcode_workflow::WorkflowInputResolution =
                    serde_json::from_value(resolution.clone())
                        .map_err(|_| Error::InvalidAdmission)?;
                schema(&resolution.supplied.schema, supplied)?;
                resolution
                    .transform
                    .evaluate(&[
                        bcode_workflow::WorkflowTransformInput {
                            name: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_STATE,
                            value: &retained,
                        },
                        bcode_workflow::WorkflowTransformInput {
                            name: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_CURRENT,
                            value: supplied,
                        },
                    ])
                    .map_err(|_| Error::InvalidAdmission)?
            } else {
                supplied.clone()
            }
        }
        _ => return Err(Error::InvalidAdmission),
    };
    schema(&node.output.schema, &value)?;
    let encoded = encode(&value)?;
    if row.get("output_json") != Some(DatabaseValue::Null) {
        if text(&row, "output_json")? != encoded {
            return Err(Error::InvalidAdmission);
        }
        tx.commit().await?;
        return duplicate_resolution(root);
    }
    if text(root, "status")? != "paused" {
        return Err(Error::InvalidAdmission);
    }
    super::super::safety::ensure_admission_allowed(&*tx, run_id).await?;
    super::terminal_scan::invalidate(&*tx, run_id).await?;
    tx.update(ACTIVATIONS)
        .value("output_json", encoded.clone())
        .where_eq("identity", id)
        .execute(&*tx)
        .await?;
    let run: crate::NewWorkflowRun = decode(root, "admission_json")?;
    record_gate_resolution(&*tx, run_id, node_id, id).await?;
    super::completion::retain_control(&*tx, run_id, &node, id, &value).await?;
    super::frontier::enqueue(&*tx, run_id, current, node_id, &value).await?;
    let (_, activated) = super::frontier::drain_observed(&*tx, &run, 32).await?;
    if boundary.exits.iter().any(|exit| exit == node_id) {
        schema(&boundary.output.schema, &value)?;
        tx.update(GRAPHS)
            .value("terminal_json", encoded)
            .where_eq("run_id", run_id)
            .execute(&*tx)
            .await?;
    }
    tx.commit().await?;
    Ok(GateResolution {
        duplicate: false,
        denied: false,
        activated,
        run_status: super::super::runs::RunState::decode(&text(root, "status")?)?,
    })
}

async fn record_gate_resolution(
    db: &dyn Database,
    run: &str,
    node: &str,
    activation: &str,
) -> Result<(), Error> {
    use super::super::journal::public_events::control::{Commitment, Operation, Outcome, record};
    record(
        db,
        Commitment {
            run,
            node,
            activation,
            operation: Operation::GateResolved,
            outcome: Outcome::Succeeded,
            time: u64::try_from(super::super::runs::timestamp()?)
                .map_err(|_| Error::InvalidAdmission)?,
            source: None,
        },
    )
    .await
}

/// Persist an explicit approval denial without claiming outstanding attempts had no effects.
/// # Errors
/// Rejects wrong activation/kind, stale authority/revision, conflicting prior grant or storage failure.
pub async fn deny(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    run_id: &str,
    node_id: &str,
    id: &str,
    revision: u64,
) -> Result<(), Error> {
    deny_observed(db, owner, run_id, node_id, id, revision).await?;
    Ok(())
}
/// Deny exactly one approval and report its durable result.
/// # Errors
/// Rejects wrong kind, stale identity/authority/revision, prior grant or storage failure.
pub async fn deny_observed(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    run_id: &str,
    node_id: &str,
    id: &str,
    revision: u64,
) -> Result<GateResolution, Error> {
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await?;
    super::super::verified(&*tx).await?;
    let roots = tx
        .select(RUNS)
        .where_eq("run_id", run_id)
        .limit(1)
        .execute(&*tx)
        .await?;
    let root = roots.first().ok_or(Error::InvalidAdmission)?;
    if text(root, "authority_json")? != encode(owner)? {
        return Err(Error::AuthorityConflict);
    }
    let current = super::super::graph_pages::revision(&*tx, run_id).await?;
    if current != revision
        || super::super::graph_pages::node(&*tx, run_id, current, node_id)
            .await?
            .is_none_or(|node| node.kind != NodeKind::Approval)
    {
        return Err(Error::InvalidAdmission);
    }
    let row = activation(&*tx, run_id, node_id)
        .await?
        .ok_or(Error::InvalidAdmission)?;
    if text(&row, "identity")? != id || row.get("output_json") != Some(DatabaseValue::Null) {
        return Err(Error::InvalidAdmission);
    }
    let duplicate = !tx
        .select("workflow_control_failures_v2")
        .where_eq("identity", id)
        .limit(1)
        .execute(&*tx)
        .await?
        .is_empty();
    if !duplicate && text(root, "status")? != "paused" {
        return Err(Error::InvalidAdmission);
    }
    if !duplicate {
        use super::super::journal::public_events::control::{
            Commitment, Operation, Outcome, record,
        };
        record(
            &*tx,
            Commitment {
                run: run_id,
                node: node_id,
                activation: id,
                operation: Operation::GateDenied,
                outcome: Outcome::Failed,
                time: u64::try_from(super::super::runs::timestamp()?)
                    .map_err(|_| Error::InvalidAdmission)?,
                source: Some(("workflow_control_failures_v2", id, 1)),
            },
        )
        .await?;
    }
    super::control_state::fail(&*tx, run_id, id).await?;
    tx.commit().await?;
    Ok(GateResolution {
        duplicate,
        denied: true,
        activated: Vec::new(),
        run_status: super::super::runs::RunState::decode(&text(root, "status")?)?,
    })
}
