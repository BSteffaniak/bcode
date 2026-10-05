//! Explicit durable parallel failure reconciliation; cancellation is intent, never proof.
use super::{
    Database, DatabaseValue, Error, FilterableQuery, SortDirection, TransactionMode, Value,
    WorkflowExecutionAuthority, activation, decode, encode, output, schema, text,
};
use bcode_workflow::{NodeKind, ParallelFailurePolicy};
use switchy_database::schema::{DataType, create_index, create_table};
#[cfg(test)]
mod tests;
const CANCEL: &str = "workflow_parallel_cancellations_v2";
const COLLECTED: &str = "workflow_collected_attempts_v2";
pub(super) async fn install(db: &dyn Database) -> Result<(), Error> {
    create_table(CANCEL)
        .column(super::super::column("activation_id", DataType::Text, false))
        .primary_key("activation_id")
        .column(super::super::column("run_id", DataType::Text, false))
        .execute(db)
        .await?;
    create_index("idx_workflow_parallel_cancel_run")
        .table(CANCEL)
        .columns(vec!["run_id", "activation_id"])
        .execute(db)
        .await?;
    create_table(COLLECTED)
        .column(super::super::column(
            "dispatch_identity",
            DataType::Text,
            false,
        ))
        .primary_key("dispatch_identity")
        .column(super::super::column("run_id", DataType::Text, false))
        .execute(db)
        .await?;
    Ok(())
}
/// Reconcile one exact join after observing member outcomes; never dispatch or signal work.
/// # Errors
/// Rejects stale authority/revision, unsupported members, invalid schema or durable inconsistencies.
pub async fn settle(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    run: &str,
    join: &str,
    revision: u64,
) -> Result<bool, Error> {
    settle_with_policy(
        db,
        owner,
        run,
        join,
        revision,
        super::parallel_progress::OutputPolicy::default(),
    )
    .await
}
/// Reconcile a parallel controller with explicit inline accumulator payload policy.
/// # Errors
/// Propagates settlement errors and rejects payloads exceeding the caller's budget.
pub async fn settle_with_policy(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    run: &str,
    join: &str,
    revision: u64,
    output_policy: super::parallel_progress::OutputPolicy,
) -> Result<bool, Error> {
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await?;
    super::super::verified(&*tx).await?;
    let root = owned_root(&*tx, run, owner).await?;
    let current = super::super::graph_pages::revision(&*tx, run).await?;
    let boundary = super::super::graph_edges::boundary(&*tx, run, current).await?;
    if current != revision {
        return Err(Error::InvalidAdmission);
    }
    let node = super::super::graph_pages::node(&*tx, run, current, join)
        .await?
        .ok_or(Error::InvalidAdmission)?;
    if node.kind != NodeKind::Parallel {
        return Err(Error::InvalidAdmission);
    }
    if output(&*tx, run, join).await?.is_some() {
        return Ok(false);
    }
    if text(&root, "status")? != "paused" {
        return Err(Error::InvalidAdmission);
    }
    let policy: ParallelFailurePolicy = node
        .configuration
        .get("failure_policy")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|_| Error::InvalidAdmission)?
        .unwrap_or_default();
    let members =
        bcode_workflow::parallel_join_member_ids(&node).map_err(|_| Error::InvalidAdmission)?;
    let Some(MemberState {
        outcomes,
        pending,
        failed,
        records,
    }) = super::parallel_progress::reduce(&*tx, run, join, revision, &members, output_policy)
        .await?
    else {
        tx.commit().await?;
        return Ok(false);
    };
    if failed && policy == ParallelFailurePolicy::FailFast {
        cancel_pending(&*tx, run, &pending).await?;
    }
    if !pending.is_empty() {
        tx.commit().await?;
        return Ok(false);
    }
    if failed && policy != ParallelFailurePolicy::CollectOutcomes {
        super::control_state::fail(&*tx, run, &format!("parallel:{join}")).await?;
        tx.commit().await?;
        return Ok(true);
    }
    let value = if policy == ParallelFailurePolicy::CollectOutcomes {
        Value::Object(outcomes)
    } else {
        super::join_input(&*tx, run, &node)
            .await?
            .ok_or(Error::MaintenanceRequired)?
    };
    if encode(&value)?.len() > output_policy.max_inline_bytes {
        return Err(Error::InvalidAdmission);
    }
    schema(&node.input.schema, &value)?;
    schema(&node.output.schema, &value)?;
    if activation(&*tx, run, join).await?.is_none() {
        super::insert_activation(&*tx, run, join, &value).await?;
    }
    let id = super::activation_id(&*tx, run, join)
        .await?
        .ok_or(Error::MaintenanceRequired)?;
    super::terminal_scan::invalidate(&*tx, run).await?;
    tx.update(super::ACTIVATIONS)
        .value("output_json", encode(&value)?)
        .where_eq("identity", id.as_str())
        .execute(&*tx)
        .await?;
    super::completion::retain_control(&*tx, run, &node, &id, &value).await?;
    record_collected(&*tx, run, records).await?;
    let request: crate::NewWorkflowRun = decode(&root, "admission_json")?;
    super::propagate_paged(&*tx, &request, current, join, &value).await?;
    if boundary.exits.iter().any(|exit| exit == join) {
        schema(&boundary.output.schema, &value)?;
        tx.update(super::GRAPHS)
            .value("terminal_json", encode(&value)?)
            .where_eq("run_id", run)
            .execute(&*tx)
            .await?;
    }
    tx.commit().await?;
    Ok(true)
}
/// Bounded committed cancellation intents; caller signals only after this read transaction ends.
/// # Errors
/// Rejects invalid bounds or unknown durable contract. A listed intent never implies terminality.
pub async fn cancellations(
    db: &dyn Database,
    run: &str,
    after: Option<&str>,
    limit: u16,
) -> Result<Vec<String>, Error> {
    if limit == 0 || limit > 100 {
        return Err(Error::InvalidAdmission);
    }
    let tx = db.begin_transaction().await?;
    super::super::verified(&*tx).await?;
    let mut query = tx
        .select(CANCEL)
        .columns(&["activation_id"])
        .where_eq("run_id", run)
        .sort("activation_id", SortDirection::Asc)
        .limit(usize::from(limit));
    if let Some(after) = after {
        query = query.where_gt("activation_id", after);
    }
    let result = query
        .execute(&*tx)
        .await?
        .iter()
        .map(|row| text(row, "activation_id"))
        .collect::<Result<Vec<_>, _>>()?;
    tx.commit().await?;
    Ok(result)
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub(super) struct MemberState {
    pub(super) outcomes: serde_json::Map<String, Value>,
    pub(super) pending: Vec<String>,
    pub(super) failed: bool,
    pub(super) records: Vec<String>,
}
pub(super) async fn members_state(
    db: &dyn Database,
    run: &str,
    members: &[&str],
) -> Result<Option<MemberState>, Error> {
    let mut outcomes = serde_json::Map::new();
    let mut pending = Vec::new();
    let mut failed = false;
    let mut records = Vec::new();
    for &member in members {
        let Some(row) = activation(db, run, member).await? else {
            return Ok(None);
        };
        let id = text(&row, "identity")?;
        if let Some(value) = output(db, run, member).await? {
            outcomes.insert(
                member.into(),
                serde_json::json!({"status":"completed","value":value}),
            );
            continue;
        }
        if row.get("skipped") == Some(DatabaseValue::Int64(1)) {
            outcomes.insert(member.into(), serde_json::json!({"status":"skipped"}));
            continue;
        }
        let attempts = db
            .select("workflow_attempts_v2")
            .where_eq("run_id", run)
            .where_eq("activation_id", id.as_str())
            .sort("attempt_number", SortDirection::Desc)
            .limit(1)
            .execute(db)
            .await?;
        let Some(attempt) = attempts.first() else {
            pending.push(id);
            continue;
        };
        let state = match attempt.get("outcome_json") {
            Some(DatabaseValue::String(state)) if state == "\"Failed\"" => "failed",
            Some(DatabaseValue::String(state)) if state == "\"Cancelled\"" => "cancelled",
            Some(DatabaseValue::Null) => {
                pending.push(id);
                continue;
            }
            Some(DatabaseValue::String(state)) if state == "\"Succeeded\"" => {
                pending.push(id);
                continue;
            }
            _ => return Err(Error::MaintenanceRequired),
        };
        failed = true;
        records.push(text(attempt, "dispatch_identity")?);
        outcomes.insert(member.into(), serde_json::json!({"status":state}));
    }
    Ok(Some(MemberState {
        outcomes,
        pending,
        failed,
        records,
    }))
}

async fn cancel_pending(
    db: &dyn switchy_database::DatabaseTransaction,
    run: &str,
    pending: &[String],
) -> Result<(), Error> {
    for id in pending {
        super::cancellation::cancel_in(db, run, id).await?;
    }
    Ok(())
}

async fn record_collected(db: &dyn Database, run: &str, records: Vec<String>) -> Result<(), Error> {
    for record in records {
        if db
            .select(COLLECTED)
            .where_eq("dispatch_identity", record.as_str())
            .limit(1)
            .execute(db)
            .await?
            .is_empty()
        {
            db.insert(COLLECTED)
                .value("dispatch_identity", record)
                .value("run_id", run)
                .execute(db)
                .await?;
        }
    }
    Ok(())
}

async fn owned_root(
    db: &dyn Database,
    run: &str,
    owner: &WorkflowExecutionAuthority,
) -> Result<switchy_database::Row, Error> {
    let roots = db
        .select(super::RUNS)
        .where_eq("run_id", run)
        .limit(1)
        .execute(db)
        .await?;
    let root = roots.into_iter().next().ok_or(Error::InvalidAdmission)?;
    if text(&root, "authority_json")? != encode(owner)? {
        return Err(Error::AuthorityConflict);
    }
    Ok(root)
}
