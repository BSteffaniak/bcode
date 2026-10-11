//! Exact owner-facing reads and fenced execution-session links.
pub use super::retry::{next_retry_attempt, schedule_retry};
use super::{
    Database, DatabaseValue, DispatchReceipt, Error, PreparedAttempt, WorkflowExecutionAuthority,
    attempt, authority, decode, encode, run, text,
};
use crate::{PreparedActivationDispatch, WorkflowExecutionSessionLink};
use switchy_database::{
    TransactionMode,
    query::{FilterableQuery, SortDirection},
    schema::{DataType, create_table},
};
const LINKS: &str = "workflow_execution_sessions_v2";
pub(super) async fn install(db: &dyn Database) -> Result<(), Error> {
    create_table(LINKS)
        .column(super::super::column(
            "dispatch_identity",
            DataType::Text,
            false,
        ))
        .primary_key("dispatch_identity")
        .column(super::super::column("link_json", DataType::Text, false))
        .execute(db)
        .await?;
    Ok(())
}
/// A bounded recovery candidate; ownership still requires trusted resolver evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoverableRun {
    pub run_id: String,
    pub authority: WorkflowExecutionAuthority,
    pub recovery_required: bool,
}
/// Discover bounded nonterminal owned runs by stable run-ID keyset.
/// # Errors
/// Rejects unsupported representations, invalid bounds or malformed owner records.
pub async fn recoverable_runs(
    db: &dyn Database,
    after: Option<&str>,
    limit: u16,
) -> Result<(Vec<RecoverableRun>, Option<String>), Error> {
    if !(1..=100).contains(&limit) {
        return Err(Error::InvalidAdmission);
    }
    let tx = db.begin_transaction().await?;
    super::super::verified(&*tx).await?;
    let mut query = tx
        .select(super::RUNS)
        .columns(&["run_id", "authority_json"])
        .where_eq("status", "paused")
        .where_not_eq("authority_json", DatabaseValue::Null)
        .sort("run_id", SortDirection::Asc)
        .limit(usize::from(limit) + 1);
    if let Some(after) = after {
        crate::validate_id("run cursor", after).map_err(|_| Error::InvalidAdmission)?;
        query = query.where_gt("run_id", after);
    }
    let mut rows = query.execute(&*tx).await?;
    let more = rows.len() > usize::from(limit);
    rows.truncate(usize::from(limit));
    let cursor = if more {
        rows.last().map(|r| text(r, "run_id")).transpose()?
    } else {
        None
    };
    let mut result = Vec::with_capacity(rows.len());
    for row in rows {
        let run_id = text(&row, "run_id")?;
        let authority: WorkflowExecutionAuthority = decode(&row, "authority_json")?;
        let recovery_required = super::super::safety::recovery_required(&*tx, &run_id).await?;
        result.push(RecoverableRun {
            run_id,
            authority,
            recovery_required,
        });
    }
    tx.commit().await?;
    Ok((result, cursor))
}
/// Discover exact unresolved preparations/receipts under recovery owner's durable fence.
///
/// Receiptless items remain ambiguous; returning them never authorizes redispatch.
/// # Errors
/// Rejects stale authority, missing recovery barrier, invalid bounds or malformed attempt state.
pub async fn recovery_attempts(
    db: &dyn Database,
    run_id: &str,
    owner: &WorkflowExecutionAuthority,
    after: Option<&str>,
    limit: u16,
) -> Result<(Vec<super::PendingAttempt>, Option<String>), Error> {
    if !(1..=100).contains(&limit) {
        return Err(Error::InvalidAdmission);
    }
    let tx = db.begin_transaction().await?;
    super::super::verified(&*tx).await?;
    authority(&run(&*tx, run_id).await?, owner)?;
    if !super::super::safety::recovery_required(&*tx, run_id).await? {
        return Err(Error::AuthorityConflict);
    }
    let mut query = tx
        .select(super::TABLE)
        .where_eq("run_id", run_id)
        .where_eq("outcome_json", DatabaseValue::Null)
        .sort("dispatch_identity", SortDirection::Asc)
        .limit(usize::from(limit) + 1);
    if let Some(after) = after {
        crate::validate_id("dispatch cursor", after).map_err(|_| Error::InvalidAdmission)?;
        query = query.where_gt("dispatch_identity", after);
    }
    let mut rows = query.execute(&*tx).await?;
    let more = rows.len() > usize::from(limit);
    rows.truncate(usize::from(limit));
    let cursor = if more {
        rows.last()
            .map(|r| text(r, "dispatch_identity"))
            .transpose()?
    } else {
        None
    };
    let result = rows
        .iter()
        .map(|row| {
            let dispatch_identity = text(row, "dispatch_identity")?;
            let prepared = checked_prepared(row, run_id, &dispatch_identity)?;
            let receipt = if row.get("receipt_json") == Some(DatabaseValue::Null) {
                None
            } else {
                let receipt: DispatchReceipt = decode(row, "receipt_json")?;
                if receipt.run_id != prepared.run_id
                    || receipt.node_id != prepared.node_id
                    || receipt.activation_id != prepared.activation_id
                    || receipt.attempt != prepared.attempt
                    || receipt.dispatch_identity != dispatch_identity
                {
                    return Err(Error::MaintenanceRequired);
                }
                Some(receipt)
            };
            Ok(super::PendingAttempt {
                dispatch_identity,
                prepared,
                receipt,
            })
        })
        .collect::<Result<Vec<_>, Error>>()?;
    tx.commit().await?;
    Ok((result, cursor))
}

/// Inspect exact operator-retry facts under current ownership without scheduling or dispatching.
///
/// Returned next-attempt identity is not a retry grant. Caller must authorize and persist
/// the retry transition before returning a successful public retry result.
/// # Errors
/// Rejects stale authority, nonlatest/nonfailed/ambiguous attempts, exhausted policy or retired activation.
pub async fn retry_facts(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
    failed_attempt: u32,
) -> Result<bcode_workflow::WorkflowNodeRetryResult, Error> {
    let tx = db.begin_transaction().await?;
    let result =
        retry_facts_in(&*tx, owner, run_id, node_id, activation_id, failed_attempt).await?;
    tx.commit().await?;
    Ok(result)
}

pub(super) async fn retry_facts_in(
    tx: &dyn switchy_database::DatabaseTransaction,
    owner: &WorkflowExecutionAuthority,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
    failed_attempt: u32,
) -> Result<bcode_workflow::WorkflowNodeRetryResult, Error> {
    super::super::verified(tx).await?;
    let root = run(tx, run_id).await?;
    authority(&root, owner)?;
    super::super::safety::ensure_admission_allowed(tx, run_id).await?;
    if !matches!(text(&root, "status")?.as_str(), "paused" | "failed") {
        return Err(Error::InvalidAdmission);
    }
    let rows = tx
        .select(super::TABLE)
        .where_eq("run_id", run_id)
        .where_eq("node_id", node_id)
        .where_eq("activation_id", activation_id)
        .sort("attempt_number", SortDirection::Desc)
        .limit(1)
        .execute(tx)
        .await?;
    let row = rows.first().ok_or(Error::InvalidAdmission)?;
    let identity = text(row, "dispatch_identity")?;
    let previous = checked_prepared(row, run_id, &identity)?;
    if previous.attempt != failed_attempt
        || row.get("outcome_json")
            != Some(DatabaseValue::String(encode(
                &super::TerminalOutcome::Failed,
            )?))
    {
        return Err(Error::InvalidAdmission);
    }
    let receipt: DispatchReceipt = decode(row, "receipt_json")?;
    if receipt.dispatch_identity != identity
        || receipt.run_id != run_id
        || receipt.node_id != node_id
        || receipt.activation_id != activation_id
        || receipt.attempt != failed_attempt
    {
        return Err(Error::MaintenanceRequired);
    }
    let limits = super::super::admission_projection::limits(&root)?;
    let limits = super::super::allowance::effective_limits(tx, run_id, &limits).await?;
    let lineage = tx
        .select("workflow_lineage_v2")
        .columns(&["root_run_id"])
        .where_eq("run_id", run_id)
        .limit(1)
        .execute(tx)
        .await?;
    let root_id = text(
        lineage.first().ok_or(Error::MaintenanceRequired)?,
        "root_run_id",
    )?;
    let root_row = run(tx, &root_id).await?;
    let root_baseline = super::super::admission_projection::limits(&root_row)?;
    let root_limits =
        super::super::allowance::effective_limits(tx, &root_id, &root_baseline).await?;
    let counters = tx
        .select("workflow_root_allowances_v2")
        .columns(&["consumed"])
        .where_eq("root_run_id", root_id)
        .limit(1)
        .execute(tx)
        .await?;
    if super::integer(
        counters.first().ok_or(Error::MaintenanceRequired)?,
        "consumed",
    )?
    .cast_unsigned()
        >= root_limits.node_execution_cap
    {
        return Err(Error::CapacityUnavailable);
    }
    let next = failed_attempt
        .checked_add(1)
        .ok_or(Error::CapacityUnavailable)?;
    if failed_attempt > limits.retry_cap
        || super::integer(&root, "consumed")?.cast_unsigned() >= limits.node_execution_cap
    {
        return Err(Error::CapacityUnavailable);
    }
    if !super::super::graph::is_ready_activation(tx, run_id, node_id, activation_id).await? {
        return Err(Error::InvalidAdmission);
    }
    Ok(bcode_workflow::WorkflowNodeRetryResult {
        run_id: run_id.into(),
        node_id: node_id.into(),
        activation_id: activation_id.into(),
        previous_attempt: failed_attempt,
        next_attempt: next,
    })
}

/// Read exact persisted authority; absence grants no authority.
/// # Errors
/// Rejects unsupported or malformed state and database failure.
pub async fn execution_authority(
    db: &dyn Database,
    id: &str,
) -> Result<Option<WorkflowExecutionAuthority>, Error> {
    let tx = db.begin_transaction().await?;
    super::super::verified(&*tx).await?;
    let row = run(&*tx, id).await?;
    let result = match row.get("authority_json") {
        Some(DatabaseValue::Null) => None,
        _ => Some(decode(&row, "authority_json")?),
    };
    tx.commit().await?;
    Ok(result)
}
/// Verify exact current authority without claiming liveness.
/// # Errors
/// Rejects stale ownership and unsupported or malformed state.
pub async fn verify(
    db: &dyn Database,
    id: &str,
    owner: &WorkflowExecutionAuthority,
) -> Result<(), Error> {
    let tx = db.begin_transaction().await?;
    super::super::verified(&*tx).await?;
    authority(&run(&*tx, id).await?, owner)?;
    tx.commit().await?;
    Ok(())
}
/// Read an exact persisted preparation, including settled attempts. Never authorizes replay.
/// # Errors
/// Rejects wrong run, corrupt normalized identity, oversized payload or unsupported state.
pub async fn prepared(
    db: &dyn Database,
    run_id: &str,
    dispatch_identity: &str,
) -> Result<Option<PreparedAttempt>, Error> {
    crate::validate_id("run_id", run_id).map_err(|_| Error::InvalidAdmission)?;
    crate::validate_id("dispatch_identity", dispatch_identity)
        .map_err(|_| Error::InvalidAdmission)?;
    let tx = db.begin_transaction().await?;
    let result = prepared_in(&*tx, run_id, dispatch_identity).await?;
    tx.commit().await?;
    Ok(result)
}

/// Read one exact prepared attempt in the caller's snapshot.
/// # Errors
/// Rejects invalid identities, unsupported state and inconsistent persisted identity.
pub async fn prepared_in(
    tx: &dyn switchy_database::DatabaseTransaction,
    run_id: &str,
    dispatch_identity: &str,
) -> Result<Option<PreparedAttempt>, Error> {
    crate::validate_id("run_id", run_id).map_err(|_| Error::InvalidAdmission)?;
    crate::validate_id("dispatch_identity", dispatch_identity)
        .map_err(|_| Error::InvalidAdmission)?;
    super::super::verified(tx).await?;
    attempt(tx, dispatch_identity)
        .await?
        .map(|row| checked_prepared(&row, run_id, dispatch_identity))
        .transpose()
}
fn checked_prepared(
    row: &switchy_database::Row,
    run_id: &str,
    identity: &str,
) -> Result<PreparedAttempt, Error> {
    let json = text(row, "prepared_json")?;
    if json.len() > super::super::MAX_ADMISSION_BYTES {
        return Err(Error::MaintenanceRequired);
    }
    let prepared: PreparedAttempt =
        serde_json::from_str(&json).map_err(|_| Error::MaintenanceRequired)?;
    if prepared.run_id != run_id
        || text(row, "run_id")? != run_id
        || prepared.dispatch_identity() != identity
        || text(row, "node_id")? != prepared.node_id
        || text(row, "activation_id")? != prepared.activation_id
        || super::integer(row, "attempt_number")? != i64::from(prepared.attempt)
    {
        return Err(Error::InvalidAdmission);
    }
    Ok(prepared)
}
/// Read the exact latest persisted attempt for an activation; None means no attempt exists.
///
/// This reports history, not permission to retry or redispatch ambiguous work.
/// # Errors
/// Rejects invalid identities, unsupported state or inconsistent persisted identity.
pub async fn latest_prepared(
    db: &dyn Database,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
) -> Result<Option<PreparedAttempt>, Error> {
    for id in [run_id, node_id, activation_id] {
        crate::validate_id("attempt identity", id).map_err(|_| Error::InvalidAdmission)?;
    }
    let tx = db.begin_transaction().await?;
    super::super::verified(&*tx).await?;
    let rows = tx
        .select(super::TABLE)
        .where_eq("run_id", run_id)
        .where_eq("node_id", node_id)
        .where_eq("activation_id", activation_id)
        .sort("attempt_number", SortDirection::Desc)
        .limit(1)
        .execute(&*tx)
        .await?;
    let result = rows
        .first()
        .map(|row| checked_prepared(row, run_id, &text(row, "dispatch_identity")?))
        .transpose()?;
    tx.commit().await?;
    Ok(result)
}

/// Read an exact canonical terminal outcome without inferring success from missing state.
///
/// `None` means the existing attempt is nonterminal. A missing attempt is an error.
/// This observation grants no authority to dispatch, retry, or count progress.
/// # Errors
/// Rejects absent attempts, unsupported state, malformed outcomes or inconsistent receipt identities.
pub async fn terminal_outcome(
    db: &dyn Database,
    run_id: &str,
    dispatch_identity: &str,
) -> Result<Option<super::TerminalOutcome>, Error> {
    crate::validate_id("run_id", run_id).map_err(|_| Error::InvalidAdmission)?;
    crate::validate_id("dispatch_identity", dispatch_identity)
        .map_err(|_| Error::InvalidAdmission)?;
    let tx = db.begin_transaction().await?;
    match terminal_outcome_in(&*tx, run_id, dispatch_identity).await {
        Ok(outcome) => {
            tx.commit().await?;
            Ok(outcome)
        }
        Err(error) => {
            tx.rollback().await?;
            Err(error)
        }
    }
}

/// Read the exact terminal outcome inside the caller's snapshot, without a nested transaction.
///
/// Preserves receipt, identity, timestamp and compatibility validation; never commits,
/// rolls back, repairs or acquires execution authority.
/// # Errors
/// Rejects invalid/missing attempts, inconsistent receipts, unsupported storage and database failures.
pub async fn terminal_outcome_in(
    db: &dyn Database,
    run_id: &str,
    dispatch_identity: &str,
) -> Result<Option<super::TerminalOutcome>, Error> {
    Ok(attempt_observation_in(db, run_id, dispatch_identity)
        .await?
        .outcome)
}

/// Verified canonical attempt ledger transitions without exposing them publicly as payloads.
pub struct AttemptLedgerObservation {
    /// Verified terminal outcome.
    pub outcome: Option<super::TerminalOutcome>,
    /// Verified admission receipt.
    pub receipt: Option<DispatchReceipt>,
    /// Canonical terminal timestamp.
    pub terminal_at_ms: Option<u64>,
}

/// Read canonical stage facts inside the caller's snapshot.
/// # Errors
/// Rejects inconsistent identities, receipts, timestamps and unsupported storage.
pub async fn attempt_observation_in(
    db: &dyn Database,
    run_id: &str,
    dispatch_identity: &str,
) -> Result<AttemptLedgerObservation, Error> {
    crate::validate_id("run_id", run_id).map_err(|_| Error::InvalidAdmission)?;
    crate::validate_id("dispatch_identity", dispatch_identity)
        .map_err(|_| Error::InvalidAdmission)?;
    super::super::verified(db).await?;
    run(db, run_id).await?;
    let row = attempt(db, dispatch_identity)
        .await?
        .ok_or(Error::InvalidAdmission)?;
    let prepared = checked_prepared(&row, run_id, dispatch_identity)?;
    let receipt = match row.get("receipt_json") {
        Some(DatabaseValue::Null) => None,
        Some(DatabaseValue::String(json)) if json.len() <= super::super::MAX_ADMISSION_BYTES => {
            let receipt: DispatchReceipt =
                serde_json::from_str(&json).map_err(|_| Error::MaintenanceRequired)?;
            if receipt.dispatch_identity != dispatch_identity
                || receipt.run_id != run_id
                || receipt.node_id != prepared.node_id
                || receipt.activation_id != prepared.activation_id
                || receipt.attempt != prepared.attempt
                || receipt.admitted_at_ms < prepared.prepared_at_ms
            {
                return Err(Error::MaintenanceRequired);
            }
            Some(receipt)
        }
        _ => return Err(Error::MaintenanceRequired),
    };
    let outcome = match row.get("outcome_json") {
        Some(DatabaseValue::Null) if row.get("terminal_at_ms") == Some(DatabaseValue::Null) => None,
        Some(DatabaseValue::String(json)) if json.len() <= super::super::MAX_ADMISSION_BYTES => {
            let receipt = receipt.as_ref().ok_or(Error::MaintenanceRequired)?;
            if super::integer(&row, "terminal_at_ms")?.cast_unsigned() < receipt.admitted_at_ms {
                return Err(Error::MaintenanceRequired);
            }
            Some(serde_json::from_str(&json).map_err(|_| Error::MaintenanceRequired)?)
        }
        _ => return Err(Error::MaintenanceRequired),
    };
    let terminal_at_ms = if outcome.is_some() {
        Some(super::integer(&row, "terminal_at_ms")?.cast_unsigned())
    } else {
        None
    };
    Ok(AttemptLedgerObservation {
        outcome,
        receipt,
        terminal_at_ms,
    })
}

/// Read an exact committed intent, including settled attempts, without authorizing replay.
/// # Errors
/// Rejects wrong run, malformed identity/payload, oversized intent or unsupported representation.
pub async fn dispatch_intent(
    db: &dyn Database,
    run_id: &str,
    dispatch_identity: &str,
) -> Result<serde_json::Value, Error> {
    crate::validate_id("run_id", run_id).map_err(|_| Error::InvalidAdmission)?;
    crate::validate_id("dispatch_identity", dispatch_identity)
        .map_err(|_| Error::InvalidAdmission)?;
    let tx = db.begin_transaction().await?;
    super::super::verified(&*tx).await?;
    run(&*tx, run_id).await?;
    let row = attempt(&*tx, dispatch_identity)
        .await?
        .ok_or(Error::InvalidAdmission)?;
    let json = text(&row, "prepared_json")?;
    if json.len() > super::super::MAX_ADMISSION_BYTES {
        return Err(Error::MaintenanceRequired);
    }
    let prepared: PreparedAttempt =
        serde_json::from_str(&json).map_err(|_| Error::MaintenanceRequired)?;
    if prepared.run_id != run_id
        || text(&row, "run_id")? != run_id
        || prepared.dispatch_identity() != dispatch_identity
    {
        return Err(Error::InvalidAdmission);
    }
    tx.commit().await?;
    Ok(prepared.intent)
}

/// Read a durable receipt without authorizing replay.
/// # Errors
/// Rejects malformed receipt or database failure.
pub async fn receipt(db: &dyn Database, id: &str) -> Result<Option<DispatchReceipt>, Error> {
    let tx = db.begin_transaction().await?;
    let result = receipt_in(&*tx, id).await?;
    tx.commit().await?;
    Ok(result)
}

/// Read an immutable dispatch receipt within the caller's snapshot.
/// # Errors
/// Rejects unsupported storage, malformed receipts and query failures.
pub async fn receipt_in(
    tx: &dyn switchy_database::DatabaseTransaction,
    id: &str,
) -> Result<Option<DispatchReceipt>, Error> {
    super::super::verified(tx).await?;
    match attempt(tx, id).await? {
        Some(row) if row.get("receipt_json") != Some(DatabaseValue::Null) => {
            Ok(Some(decode(&row, "receipt_json")?))
        }
        _ => Ok(None),
    }
}
/// Recheck exact persisted intent, owner, activation and cancellation before external admission.
/// Existing receipts are returned for deduplication, never as permission to repeat an effect.
/// # Errors
/// Rejects stale ownership, changed intent/graph, cancellation, terminal work or malformed state.
pub async fn check_owner_admission(
    db: &dyn Database,
    request: &PreparedActivationDispatch,
    owner: &WorkflowExecutionAuthority,
) -> Result<Option<serde_json::Value>, Error> {
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await?;
    super::super::verified(&*tx).await?;
    let root = run(&*tx, &request.activation.run_id).await?;
    authority(&root, owner)?;
    let row = attempt(&*tx, &request.dispatch_identity)
        .await?
        .ok_or(Error::InvalidAdmission)?;
    let prepared: PreparedAttempt = decode(&row, "prepared_json")?;
    if prepared.dispatch_identity() != request.dispatch_identity
        || prepared.run_id != request.activation.run_id
        || prepared.node_id != request.activation.node_id
        || prepared.activation_id != request.activation.activation_id
        || prepared.attempt != request.attempt
        || prepared.intent != request.intent
    {
        return Err(Error::InvalidAdmission);
    }
    if row.get("receipt_json") != Some(DatabaseValue::Null) {
        let receipt: DispatchReceipt = decode(&row, "receipt_json")?;
        tx.commit().await?;
        return Ok(Some(receipt.receipt));
    }
    if text(&root, "status")? != "paused" || row.get("outcome_json") != Some(DatabaseValue::Null) {
        return Err(Error::InvalidAdmission);
    }
    super::super::safety::ensure_admission_allowed(&*tx, &prepared.run_id).await?;
    super::ensure_activation_admission(&*tx, &prepared.run_id, &prepared.activation_id).await?;
    if !super::super::graph::is_ready_activation(
        &*tx,
        &prepared.run_id,
        &prepared.node_id,
        &prepared.activation_id,
    )
    .await?
    {
        return Err(Error::InvalidAdmission);
    }
    let node =
        super::super::graph::execution_node(&*tx, &prepared.run_id, &prepared.node_id).await?;
    if node != request.activation.node {
        return Err(Error::InvalidAdmission);
    }
    tx.commit().await?;
    Ok(None)
}
/// Read the exact execution-session relationship, not the run's detachable association.
/// # Errors
/// Rejects malformed link and database failure.
pub async fn execution_session_link(
    db: &dyn Database,
    run_id: &str,
    node: &str,
    activation: &str,
    number: u32,
) -> Result<Option<WorkflowExecutionSessionLink>, Error> {
    crate::validate_execution_session_link_identity(run_id, node, activation, number)
        .map_err(|_| Error::InvalidAdmission)?;
    let tx = db.begin_transaction().await?;
    let result = execution_session_link_in(&*tx, run_id, node, activation, number).await?;
    tx.commit().await?;
    Ok(result)
}

/// Read an exact execution-session link inside the caller's read snapshot.
/// # Errors
/// Rejects invalid identity, unsupported representation, malformed storage or query failures.
pub async fn execution_session_link_in(
    tx: &dyn switchy_database::DatabaseTransaction,
    run_id: &str,
    node: &str,
    activation: &str,
    number: u32,
) -> Result<Option<WorkflowExecutionSessionLink>, Error> {
    crate::validate_execution_session_link_identity(run_id, node, activation, number)
        .map_err(|_| Error::InvalidAdmission)?;
    super::super::verified(tx).await?;
    let id = crate::dispatch_identity(run_id, node, activation, number);
    let rows = tx
        .select(LINKS)
        .where_eq("dispatch_identity", id)
        .limit(1)
        .execute(tx)
        .await?;
    let result = rows
        .first()
        .map(|row| decode(row, "link_json"))
        .transpose()?;
    Ok(result)
}
/// Persist an exact execution-session link under current authority in one transaction.
/// Detaching the parent association does not rewrite historical execution-session identity.
/// # Errors
/// Rejects conflicting duplicates, stale authority, cancellation, wrong workspace or missing attempt.
pub async fn persist_execution_session_link(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    link: &WorkflowExecutionSessionLink,
) -> Result<(), Error> {
    crate::validate_execution_session_link(link).map_err(|_| Error::InvalidAdmission)?;
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await?;
    super::super::verified(&*tx).await?;
    let root = run(&*tx, &link.run_id).await?;
    authority(&root, owner)?;
    let id = crate::dispatch_identity(
        &link.run_id,
        &link.node_id,
        &link.activation_id,
        link.attempt,
    );
    let encoded = encode(link)?;
    let rows = tx
        .select(LINKS)
        .where_eq("dispatch_identity", id.as_str())
        .limit(1)
        .execute(&*tx)
        .await?;
    if let Some(row) = rows.first() {
        if text(row, "link_json")? != encoded {
            return Err(Error::InvalidAdmission);
        }
        tx.commit().await?;
        return Ok(());
    }
    super::super::safety::ensure_admission_allowed(&*tx, &link.run_id).await?;
    let admission: crate::NewWorkflowRun = decode(&root, "admission_json")?;
    let row = attempt(&*tx, &id).await?.ok_or(Error::InvalidAdmission)?;
    let prepared: PreparedAttempt = decode(&row, "prepared_json")?;
    if text(&root, "status")? != "paused"
        || admission.workspace_snapshot != link.workspace_snapshot
        || row.get("outcome_json") != Some(DatabaseValue::Null)
        || prepared.dispatch_identity() != id
        || link.created_at_ms < prepared.prepared_at_ms
    {
        return Err(Error::InvalidAdmission);
    }
    tx.insert(LINKS)
        .value("dispatch_identity", id)
        .value("link_json", encoded)
        .execute(&*tx)
        .await?;
    tx.commit().await?;
    Ok(())
}
async fn activation_cancellation_intended(
    db: &dyn Database,
    run_id: &str,
    activation_id: &str,
) -> Result<bool, Error> {
    Ok(!db
        .select("workflow_parallel_cancellations_v2")
        .columns(&["activation_id"])
        .where_eq("run_id", run_id)
        .where_eq("activation_id", activation_id)
        .limit(1)
        .execute(db)
        .await?
        .is_empty())
}
/// Observe whole-run or exact parallel-member cancellation intent for one persisted dispatch.
/// # Errors
/// Rejects stale authority, missing/mismatched attempts and unsupported durable state.
pub async fn cancellation_intended(
    db: &dyn Database,
    run_id: &str,
    dispatch_identity: &str,
    owner: &WorkflowExecutionAuthority,
) -> Result<bool, Error> {
    let tx = db.begin_transaction().await?;
    super::super::verified(&*tx).await?;
    authority(&run(&*tx, run_id).await?, owner)?;
    let row = attempt(&*tx, dispatch_identity)
        .await?
        .ok_or(Error::InvalidAdmission)?;
    let prepared = checked_prepared(&row, run_id, dispatch_identity)?;
    let intended = super::super::safety::cancellation_requested_at(&*tx, run_id)
        .await?
        .is_some()
        || activation_cancellation_intended(&*tx, run_id, &prepared.activation_id).await?;
    tx.commit().await?;
    Ok(intended)
}
/// Discover only receipt-backed unsettled dispatches targeted by parallel-member cancellation.
///
/// Scans a bounded dispatch page; an empty result with a cursor is not exhaustion.
/// Neither widens cancellation to the run nor changes outcomes or resource reservations.
/// # Errors
/// Rejects stale authority, invalid bounds/cursors, inconsistent receipts or database failures.
pub async fn parallel_cancellation_receipts(
    db: &dyn Database,
    run_id: &str,
    owner: &WorkflowExecutionAuthority,
    after: Option<&str>,
    limit: usize,
) -> Result<(Vec<DispatchReceipt>, Option<String>), Error> {
    if !(1..=128).contains(&limit) {
        return Err(Error::InvalidAdmission);
    }
    if let Some(after) = after {
        crate::validate_id("dispatch cursor", after).map_err(|_| Error::InvalidAdmission)?;
    }
    let tx = db.begin_transaction().await?;
    super::super::verified(&*tx).await?;
    authority(&run(&*tx, run_id).await?, owner)?;
    let mut query = tx
        .select(super::TABLE)
        .where_eq("run_id", run_id)
        .where_eq("outcome_json", DatabaseValue::Null)
        .sort("dispatch_identity", SortDirection::Asc)
        .limit(limit + 1);
    if let Some(after) = after {
        query = query.where_gt("dispatch_identity", after);
    }
    let mut rows = query.execute(&*tx).await?;
    let more = rows.len() > limit;
    rows.truncate(limit);
    let cursor = if more {
        rows.last()
            .map(|r| text(r, "dispatch_identity"))
            .transpose()?
    } else {
        None
    };
    let mut receipts = Vec::new();
    for row in rows {
        let identity = text(&row, "dispatch_identity")?;
        let prepared = checked_prepared(&row, run_id, &identity)?;
        if row.get("receipt_json") == Some(DatabaseValue::Null)
            || !activation_cancellation_intended(&*tx, run_id, &prepared.activation_id).await?
        {
            continue;
        }
        let receipt: DispatchReceipt = decode(&row, "receipt_json")?;
        if receipt.run_id != run_id
            || receipt.node_id != prepared.node_id
            || receipt.activation_id != prepared.activation_id
            || receipt.attempt != prepared.attempt
            || receipt.dispatch_identity != identity
        {
            return Err(Error::MaintenanceRequired);
        }
        receipts.push(receipt);
    }
    tx.commit().await?;
    Ok((receipts, cursor))
}

/// Page receipt-backed outstanding targets after a durable cancellation request.
/// # Errors
/// Rejects stale authority, absent cancellation intent, invalid bounds or malformed receipts.
pub async fn cancellation_receipts(
    db: &dyn Database,
    run_id: &str,
    owner: &WorkflowExecutionAuthority,
    after: Option<&str>,
    limit: usize,
) -> Result<(Vec<DispatchReceipt>, Option<String>), Error> {
    if !(1..=128).contains(&limit) {
        return Err(Error::InvalidAdmission);
    }
    let tx = db.begin_transaction().await?;
    super::super::verified(&*tx).await?;
    authority(&run(&*tx, run_id).await?, owner)?;
    if super::super::safety::cancellation_requested_at(&*tx, run_id)
        .await?
        .is_none()
    {
        return Err(Error::InvalidAdmission);
    }
    let mut query = tx
        .select(super::TABLE)
        .where_eq("run_id", run_id)
        .where_eq("outcome_json", DatabaseValue::Null)
        .sort("dispatch_identity", SortDirection::Asc)
        .limit(limit + 1);
    if let Some(after) = after {
        query = query.where_gt("dispatch_identity", after);
    }
    let mut rows = query.execute(&*tx).await?;
    let more = rows.len() > limit;
    rows.truncate(limit);
    let cursor = if more {
        rows.last()
            .map(|row| text(row, "dispatch_identity"))
            .transpose()?
    } else {
        None
    };
    let receipts = rows
        .iter()
        .filter(|row| row.get("receipt_json") != Some(DatabaseValue::Null))
        .map(|row| decode(row, "receipt_json"))
        .collect::<Result<Vec<_>, _>>()?;
    tx.commit().await?;
    Ok((receipts, cursor))
}
