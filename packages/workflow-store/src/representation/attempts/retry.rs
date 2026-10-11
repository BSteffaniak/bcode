//! Explicit durable operator retry grants; preparation never invents permission.
use super::{
    DataType, Database, Error, FilterableQuery, PreparedAttempt, RUNS, TransactionMode,
    WorkflowExecutionAuthority, authority, column, create_table, integer, owner_boundary, run,
};
const GRANTS: &str = "workflow_retry_grants_v2";

pub(super) async fn install(db: &dyn Database) -> Result<(), Error> {
    create_table(GRANTS)
        .column(column("run_id", DataType::Text, false))
        .column(column("node_id", DataType::Text, false))
        .column(column("activation_id", DataType::Text, false))
        .column(column("failed_attempt", DataType::BigInt, false))
        .column(column("consumed", DataType::BigInt, false))
        .unique_columns(["run_id", "node_id", "activation_id", "failed_attempt"])
        .execute(db)
        .await?;
    Ok(())
}

/// Persist an exact retry grant and reopen a failed run under current execution authority.
/// # Errors
/// Rejects stale authority, ambiguous outcomes, retired activations and exhausted policy.
pub async fn schedule_retry(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
    failed_attempt: u32,
) -> Result<bcode_workflow::WorkflowNodeRetryResult, Error> {
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await?;
    super::super::verified(&*tx).await?;
    authority(&run(&*tx, run_id).await?, owner)?;
    let existing = tx
        .select(GRANTS)
        .where_eq("run_id", run_id)
        .where_eq("node_id", node_id)
        .where_eq("activation_id", activation_id)
        .where_eq("failed_attempt", i64::from(failed_attempt))
        .limit(1)
        .execute(&*tx)
        .await?;
    if !existing.is_empty() {
        return Ok(bcode_workflow::WorkflowNodeRetryResult {
            run_id: run_id.into(),
            node_id: node_id.into(),
            activation_id: activation_id.into(),
            previous_attempt: failed_attempt,
            next_attempt: failed_attempt
                .checked_add(1)
                .ok_or(Error::MaintenanceRequired)?,
        });
    }
    let result =
        owner_boundary::retry_facts_in(&*tx, owner, run_id, node_id, activation_id, failed_attempt)
            .await?;
    tx.insert(GRANTS)
        .value("run_id", run_id)
        .value("node_id", node_id)
        .value("activation_id", activation_id)
        .value("failed_attempt", i64::from(failed_attempt))
        .value("consumed", 0_i64)
        .execute(&*tx)
        .await?;
    let history = super::super::semantic_history::lifecycle::begin(&*tx, run_id).await?;
    let recorded_at = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| Error::InvalidAdmission)?
            .as_millis(),
    )
    .map_err(|_| Error::InvalidAdmission)?;
    tx.update(RUNS)
        .value("status", "paused")
        .value("updated_at_ms", recorded_at)
        .where_eq("run_id", run_id)
        .execute(&*tx)
        .await?;
    super::super::semantic_history::lifecycle::finish(
        &*tx,
        history,
        "paused",
        u64::try_from(recorded_at).map_err(|_| Error::InvalidAdmission)?,
    )
    .await?;
    super::super::graph::terminal_scan::invalidate(&*tx, run_id).await?;
    tx.commit().await?;
    Ok(result)
}

/// Read an outstanding exact retry grant before applying terminal-outcome short circuits.
/// # Errors
/// Rejects stale ownership, unsupported schema and malformed grant state.
pub async fn next_retry_attempt(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
) -> Result<Option<u32>, Error> {
    let tx = db.begin_transaction().await?;
    super::super::verified(&*tx).await?;
    authority(&run(&*tx, run_id).await?, owner)?;
    let rows = tx
        .select(GRANTS)
        .where_eq("run_id", run_id)
        .where_eq("node_id", node_id)
        .where_eq("activation_id", activation_id)
        .where_eq("consumed", 0_i64)
        .limit(2)
        .execute(&*tx)
        .await?;
    if rows.len() > 1 {
        return Err(Error::MaintenanceRequired);
    }
    let result = rows
        .first()
        .map(|row| {
            u32::try_from(integer(row, "failed_attempt")?)
                .ok()
                .and_then(|n| n.checked_add(1))
                .ok_or(Error::MaintenanceRequired)
        })
        .transpose()?;
    tx.commit().await?;
    Ok(result)
}

pub(super) async fn consume(db: &dyn Database, prepared: &PreparedAttempt) -> Result<(), Error> {
    if prepared.attempt == 1 {
        return Ok(());
    }
    let rows = db
        .select(GRANTS)
        .where_eq("run_id", prepared.run_id.as_str())
        .where_eq("node_id", prepared.node_id.as_str())
        .where_eq("activation_id", prepared.activation_id.as_str())
        .where_eq("failed_attempt", i64::from(prepared.attempt - 1))
        .where_eq("consumed", 0_i64)
        .limit(1)
        .execute(db)
        .await?;
    if rows.is_empty() {
        return Err(Error::InvalidAdmission);
    }
    db.update(GRANTS)
        .value("consumed", 1_i64)
        .where_eq("run_id", prepared.run_id.as_str())
        .where_eq("node_id", prepared.node_id.as_str())
        .where_eq("activation_id", prepared.activation_id.as_str())
        .where_eq("failed_attempt", i64::from(prepared.attempt - 1))
        .execute(db)
        .await?;
    Ok(())
}
