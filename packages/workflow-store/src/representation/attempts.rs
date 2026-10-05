//! Durable attempt ledger. No operation here enables scheduler dispatch.
use super::{Database, DatabaseValue, RUNS, RepresentationError as Error, TransactionMode, column};
use crate::{DispatchReceipt, PreparedAttempt, WorkflowExecutionAuthority};
use serde::{Deserialize, Serialize};
use switchy_database::{
    Row,
    query::{FilterableQuery, SortDirection},
    schema::{DataType, create_index, create_table},
};
const TABLE: &str = "workflow_attempts_v2";
/// Shared read-only fresh-leaf admission facts.
pub mod admission;
pub mod owner_boundary;
mod retry;

/// Stable persisted attempt outcome; neither implies graph completion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TerminalOutcome {
    /// Operation owner confirmed success; graph output settlement is still separate.
    Succeeded,
    /// Operation owner confirmed failure.
    Failed,
    /// Operation owner confirmed cancellation (a signal alone is insufficient).
    Cancelled,
}

/// Bounded receipt/intent observation for recovery, not authority to repeat effects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAttempt {
    /// Stable key and exclusive pagination cursor.
    pub dispatch_identity: String,
    /// Exact prepared intent.
    pub prepared: PreparedAttempt,
    /// Durable receipt if attachment committed.
    pub receipt: Option<DispatchReceipt>,
}

pub(super) async fn install(db: &dyn Database) -> Result<(), Error> {
    owner_boundary::install(db).await?;
    retry::install(db).await?;
    create_table(TABLE)
        .column(column("dispatch_identity", DataType::Text, false))
        .primary_key("dispatch_identity")
        .column(column("run_id", DataType::Text, false))
        .column(column("prepared_json", DataType::Text, false))
        .column(column("node_id", DataType::Text, false))
        .column(column("activation_id", DataType::Text, false))
        .column(column("attempt_number", DataType::BigInt, false))
        .unique_columns(["run_id", "node_id", "activation_id", "attempt_number"])
        .column(column("receipt_json", DataType::Text, true))
        .column(column("outcome_json", DataType::Text, true))
        .column(column("terminal_at_ms", DataType::Int, true))
        .execute(db)
        .await?;
    create_index("idx_workflow_attempts_v2_pending")
        .table(TABLE)
        .columns(vec!["run_id", "outcome_json", "dispatch_identity"])
        .execute(db)
        .await?;
    Ok(())
}

fn encode<T: Serialize>(value: &T) -> Result<String, Error> {
    let json = serde_json::to_string(value).map_err(|_| Error::InvalidAdmission)?;
    if json.len() > super::MAX_ADMISSION_BYTES {
        return Err(Error::InvalidAdmission);
    }
    Ok(json)
}
fn text(row: &Row, key: &str) -> Result<String, Error> {
    match row.get(key) {
        Some(DatabaseValue::String(s)) => Ok(s),
        _ => Err(Error::MaintenanceRequired),
    }
}
fn integer(row: &Row, key: &str) -> Result<i64, Error> {
    match row.get(key) {
        Some(DatabaseValue::Int64(n)) if n >= 0 => Ok(n),
        _ => Err(Error::MaintenanceRequired),
    }
}
fn decode<T: serde::de::DeserializeOwned>(row: &Row, key: &str) -> Result<T, Error> {
    serde_json::from_str(&text(row, key)?).map_err(|_| Error::MaintenanceRequired)
}
async fn run(db: &dyn Database, id: &str) -> Result<Row, Error> {
    db.select(RUNS)
        .where_eq("run_id", id)
        .limit(1)
        .execute(db)
        .await?
        .into_iter()
        .next()
        .ok_or(Error::InvalidAdmission)
}
fn authority(row: &Row, owner: &WorkflowExecutionAuthority) -> Result<(), Error> {
    if text(row, "authority_json")? != encode(owner)? {
        return Err(Error::AuthorityConflict);
    }
    Ok(())
}
async fn attempt(db: &dyn Database, id: &str) -> Result<Option<Row>, Error> {
    Ok(db
        .select(TABLE)
        .where_eq("dispatch_identity", id)
        .limit(1)
        .execute(db)
        .await?
        .into_iter()
        .next())
}

/// Claim initial ledger authority once; this cannot replace or hand off an existing owner.
///
/// # Errors
/// Rejects invalid authority, stale/conflicting ownership, incompatible storage or database failure.
pub async fn claim_authority(
    db: &dyn Database,
    run_id: &str,
    owner: &WorkflowExecutionAuthority,
) -> Result<(), Error> {
    for id in [
        &owner.target_artifact_id,
        &owner.daemon_instance_id,
        &owner.fencing_token,
    ] {
        crate::validate_id("authority", id).map_err(|_| Error::InvalidAdmission)?;
    }
    if owner.generation == 0 {
        return Err(Error::InvalidAdmission);
    }
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await?;
    super::verified(&*tx).await?;
    let row = run(&*tx, run_id).await?;
    match row.get("authority_json") {
        Some(DatabaseValue::Null) => {
            tx.update(RUNS)
                .value("authority_json", encode(owner)?)
                .where_eq("run_id", run_id)
                .execute(&*tx)
                .await?;
        }
        _ => authority(&row, owner)?,
    }
    tx.commit().await?;
    Ok(())
}

/// Reserve an initial or explicitly requested retry attempt atomically.
///
/// Resource claims, cancellation intent, allowance and concurrency use the same transaction.
/// Retries require the exact latest failed, receipt-backed terminal attempt and configured allowance.
/// This persists intent, not permission to invoke an external operation.
/// # Errors
/// Rejects unsupported activation/resource semantics, expired policy, exhausted capacity,
/// stale authority, conflicting duplicate or storage failure.
pub async fn prepare(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    prepared: &PreparedAttempt,
) -> Result<String, Error> {
    crate::validate_prepared_attempt(prepared).map_err(|_| Error::InvalidAdmission)?;
    let json = encode(prepared)?;
    let id = prepared.dispatch_identity();
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await?;
    super::verified(&*tx).await?;
    let row = run(&*tx, &prepared.run_id).await?;
    authority(&row, owner)?;
    super::safety::ensure_admission_allowed(&*tx, &prepared.run_id).await?;
    ensure_activation_admission(&*tx, &prepared.run_id, &prepared.activation_id).await?;
    if let Some(existing) = attempt(&*tx, &id).await? {
        if text(&existing, "prepared_json")? != json {
            return Err(Error::InvalidAdmission);
        }
        tx.commit().await?;
        return Ok(id);
    }
    admission::ensure_fresh(&*tx, prepared).await?;
    if text(&row, "status")? != "paused" {
        return Err(Error::InvalidAdmission);
    }
    let baseline = super::admission_projection::limits(&row)?;
    let limits = super::allowance::effective_limits(&*tx, &prepared.run_id, &baseline).await?;
    let node = super::graph::execution_node(&*tx, &prepared.run_id, &prepared.node_id).await?;
    if !super::graph::is_ready_activation(
        &*tx,
        &prepared.run_id,
        &prepared.node_id,
        &prepared.activation_id,
    )
    .await?
        || !matches!(
            node.kind,
            bcode_workflow::NodeKind::Task
                | bcode_workflow::NodeKind::Agent
                | bcode_workflow::NodeKind::PluginBlock
                | bcode_workflow::NodeKind::WorkflowCall
        )
    {
        return Err(Error::UnsupportedLifecycle);
    }
    validate_retry(&*tx, prepared, limits.retry_cap).await?;
    retry::consume(&*tx, prepared).await?;
    let consumed = integer(&row, "consumed")?;
    let active = integer(&row, "active_attempts")?;
    if limits
        .deadline_at_ms
        .is_some_and(|deadline| prepared.prepared_at_ms >= deadline)
        || u64::try_from(consumed).map_err(|_| Error::MaintenanceRequired)?
            >= limits.node_execution_cap
        || limits
            .concurrency_cap
            .is_some_and(|cap| active >= i64::from(cap))
    {
        return Err(Error::CapacityUnavailable);
    }
    super::graph::fanout::reserve(
        &*tx,
        &prepared.run_id,
        &prepared.node_id,
        &prepared.activation_id,
    )
    .await?;
    super::safety::reserve(&*tx, &prepared.run_id, &id, &node.resources).await?;
    super::hierarchy::consume_attempt(&*tx, &prepared.run_id).await?;
    tx.insert(TABLE)
        .value("dispatch_identity", id.as_str())
        .value("run_id", prepared.run_id.as_str())
        .value("prepared_json", json)
        .value("node_id", prepared.node_id.as_str())
        .value("activation_id", prepared.activation_id.as_str())
        .value("attempt_number", i64::from(prepared.attempt))
        .value("receipt_json", DatabaseValue::Null)
        .value("outcome_json", DatabaseValue::Null)
        .value("terminal_at_ms", DatabaseValue::Null)
        .execute(&*tx)
        .await?;
    super::graph::terminal_scan::invalidate(&*tx, &prepared.run_id).await?;
    tx.update(RUNS)
        .value(
            "consumed",
            consumed.checked_add(1).ok_or(Error::CapacityUnavailable)?,
        )
        .value(
            "active_attempts",
            active.checked_add(1).ok_or(Error::CapacityUnavailable)?,
        )
        .where_eq("run_id", prepared.run_id.as_str())
        .execute(&*tx)
        .await?;
    tx.commit().await?;
    Ok(id)
}

pub(super) async fn ensure_activation_admission(
    db: &dyn Database,
    run_id: &str,
    activation_id: &str,
) -> Result<(), Error> {
    if !db
        .select("workflow_parallel_cancellations_v2")
        .columns(&["activation_id"])
        .where_eq("run_id", run_id)
        .where_eq("activation_id", activation_id)
        .limit(1)
        .execute(db)
        .await?
        .is_empty()
    {
        return Err(Error::CapacityUnavailable);
    }
    Ok(())
}

async fn validate_retry(
    db: &dyn Database,
    prepared: &PreparedAttempt,
    retry_cap: u32,
) -> Result<(), Error> {
    let latest = db
        .select(TABLE)
        .where_eq("run_id", prepared.run_id.as_str())
        .where_eq("node_id", prepared.node_id.as_str())
        .where_eq("activation_id", prepared.activation_id.as_str())
        .sort("attempt_number", SortDirection::Desc)
        .limit(1)
        .execute(db)
        .await?;
    let Some(row) = latest.first() else {
        return if prepared.attempt == 1 {
            Ok(())
        } else {
            Err(Error::InvalidAdmission)
        };
    };
    let previous: PreparedAttempt = decode(row, "prepared_json")?;
    if previous.run_id != prepared.run_id
        || previous.node_id != prepared.node_id
        || previous.activation_id != prepared.activation_id
        || integer(row, "attempt_number")? != i64::from(previous.attempt)
        || text(row, "dispatch_identity")? != previous.dispatch_identity()
    {
        return Err(Error::MaintenanceRequired);
    }
    if previous.attempt.checked_add(1) != Some(prepared.attempt) || prepared.attempt - 1 > retry_cap
    {
        return Err(Error::CapacityUnavailable);
    }
    if row.get("outcome_json") != Some(DatabaseValue::String(encode(&TerminalOutcome::Failed)?)) {
        return Err(Error::InvalidAdmission);
    }
    let receipt: DispatchReceipt = decode(row, "receipt_json")?;
    if receipt.run_id != prepared.run_id
        || receipt.node_id != prepared.node_id
        || receipt.activation_id != prepared.activation_id
        || receipt.attempt != previous.attempt
        || receipt.dispatch_identity != previous.dispatch_identity()
        || u64::try_from(integer(row, "terminal_at_ms")?).map_err(|_| Error::MaintenanceRequired)?
            > prepared.prepared_at_ms
    {
        return Err(Error::InvalidAdmission);
    }
    Ok(())
}

/// Attach the operation owner's exact receipt without reopening terminal attempts.
/// # Errors
/// Rejects stale authority, missing intent, mismatched receipt, conflicting duplicate or storage failure.
pub async fn attach_receipt(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    receipt: &DispatchReceipt,
) -> Result<(), Error> {
    crate::validate_dispatch_receipt(receipt).map_err(|_| Error::InvalidAdmission)?;
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await?;
    super::verified(&*tx).await?;
    authority(&run(&*tx, &receipt.run_id).await?, owner)?;
    let row = attempt(&*tx, &receipt.dispatch_identity)
        .await?
        .ok_or(Error::InvalidAdmission)?;
    let prepared: PreparedAttempt = decode(&row, "prepared_json")?;
    if receipt.admitted_at_ms < prepared.prepared_at_ms {
        return Err(Error::InvalidAdmission);
    }
    if receipt.dispatch_identity != prepared.dispatch_identity()
        || receipt.run_id != prepared.run_id
        || receipt.node_id != prepared.node_id
        || receipt.activation_id != prepared.activation_id
        || receipt.attempt != prepared.attempt
    {
        return Err(Error::InvalidAdmission);
    }
    let json = encode(receipt)?;
    if row.get("receipt_json") == Some(DatabaseValue::String(json.clone())) {
        tx.commit().await?;
        return Ok(());
    }
    if row.get("receipt_json") != Some(DatabaseValue::Null)
        || row.get("outcome_json") != Some(DatabaseValue::Null)
    {
        return Err(Error::InvalidAdmission);
    }
    tx.update(TABLE)
        .value("receipt_json", json)
        .where_eq("dispatch_identity", receipt.dispatch_identity.as_str())
        .execute(&*tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Record an owner-confirmed terminal result and release concurrency, not total allowance.
///
/// Graph/output settlement and run terminalization remain unavailable.
/// # Errors
/// Rejects stale authority, missing receipt, conflicting duplicate, inconsistent counters or database failure.
pub async fn settle(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    run_id: &str,
    identity: &str,
    outcome: TerminalOutcome,
    terminal_at_ms: u64,
) -> Result<(), Error> {
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await?;
    settle_in(&*tx, owner, run_id, identity, outcome, terminal_at_ms).await?;
    tx.commit().await?;
    Ok(())
}

/// Settle within the caller's immediate transaction; never commits or rolls back it.
/// # Errors
/// Rejects stale authority, missing receipts, conflicting outcomes and inconsistent counters.
pub async fn settle_in(
    tx: &dyn switchy_database::DatabaseTransaction,
    owner: &WorkflowExecutionAuthority,
    run_id: &str,
    identity: &str,
    outcome: TerminalOutcome,
    terminal_at_ms: u64,
) -> Result<(), Error> {
    super::verified(tx).await?;
    let root = run(tx, run_id).await?;
    authority(&root, owner)?;
    let row = attempt(tx, identity)
        .await?
        .ok_or(Error::InvalidAdmission)?;
    if text(&row, "run_id")? != run_id || row.get("receipt_json") == Some(DatabaseValue::Null) {
        return Err(Error::InvalidAdmission);
    }
    let receipt: DispatchReceipt = decode(&row, "receipt_json")?;
    let prepared: PreparedAttempt = decode(&row, "prepared_json")?;
    if prepared.run_id != run_id
        || prepared.dispatch_identity() != identity
        || receipt.dispatch_identity != identity
        || receipt.run_id != run_id
        || receipt.node_id != prepared.node_id
        || receipt.activation_id != prepared.activation_id
        || receipt.attempt != prepared.attempt
    {
        return Err(Error::MaintenanceRequired);
    }
    if terminal_at_ms < receipt.admitted_at_ms {
        return Err(Error::InvalidAdmission);
    }
    let json = encode(&outcome)?;
    let timestamp = i64::try_from(terminal_at_ms).map_err(|_| Error::InvalidAdmission)?;
    if row.get("outcome_json") == Some(DatabaseValue::String(json.clone())) {
        // The first terminal observation owns its timestamp. Later delivery is a no-op.
        integer(&row, "terminal_at_ms")?;
        return Ok(());
    }
    if row.get("outcome_json") != Some(DatabaseValue::Null) {
        return Err(Error::InvalidAdmission);
    }
    let active = integer(&root, "active_attempts")?
        .checked_sub(1)
        .filter(|n| *n >= 0)
        .ok_or(Error::MaintenanceRequired)?;
    tx.update(TABLE)
        .value("outcome_json", json)
        .value("terminal_at_ms", timestamp)
        .where_eq("dispatch_identity", identity)
        .execute(tx)
        .await?;
    super::graph::terminal_scan::invalidate(tx, run_id).await?;
    super::graph::fanout::release(tx, run_id, &prepared.node_id, &prepared.activation_id).await?;
    super::safety::release(tx, run_id, identity).await?;
    tx.update(RUNS)
        .value("active_attempts", active)
        .where_eq("run_id", run_id)
        .execute(tx)
        .await?;
    Ok(())
}

/// Discover a bounded page of unsettled intents, retaining attached receipts.
///
/// Continue with the last dispatch identity; rescan for later arrivals behind that cursor.
/// # Errors
/// Rejects invalid bounds, unknown representations, malformed rows or database failure.
pub async fn pending(
    db: &dyn Database,
    run_id: &str,
    after: Option<&str>,
    limit: u16,
) -> Result<Vec<PendingAttempt>, Error> {
    if limit == 0 || limit > 100 {
        return Err(Error::InvalidAdmission);
    }
    let tx = db.begin_transaction().await?;
    super::verified(&*tx).await?;
    run(&*tx, run_id).await?;
    let mut query = tx
        .select(TABLE)
        .where_eq("run_id", run_id)
        .where_eq("outcome_json", DatabaseValue::Null)
        .sort("dispatch_identity", SortDirection::Asc)
        .limit(usize::from(limit));
    if let Some(after) = after {
        query = query.where_gt("dispatch_identity", after);
    }
    let mut result = Vec::new();
    for row in query.execute(&*tx).await? {
        result.push(PendingAttempt {
            dispatch_identity: text(&row, "dispatch_identity")?,
            prepared: decode(&row, "prepared_json")?,
            receipt: if row.get("receipt_json") == Some(DatabaseValue::Null) {
                None
            } else {
                Some(decode(&row, "receipt_json")?)
            },
        });
    }
    tx.commit().await?;
    Ok(result)
}

#[cfg(test)]
mod tests;
