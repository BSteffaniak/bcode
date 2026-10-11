//! Bounded observation, association-only detachment and fenced terminalization.
use super::{
    Database, DatabaseValue, RUNS, RepresentationError as Error, RunRepresentation, TransactionMode,
};
use crate::WorkflowExecutionAuthority;
/// Bounded mixed-representation catalog.
pub mod catalog;
#[cfg(test)]
mod tests;
use switchy_database::{
    Row,
    query::{FilterableQuery, SortDirection},
};

/// Stable run state projected from canonical representation fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    /// Admitted but not enabled for production dispatch.
    Paused,
    /// Canonical successful graph and all attempts settled.
    Completed,
    /// Owner-confirmed failure after all attempts settled.
    Failed,
    /// Cancellation intent with all attempts settled, or confirmed operation cancellation.
    Cancelled,
}
impl RunState {
    pub(super) fn decode(value: &str) -> Result<Self, Error> {
        match value {
            "paused" => Ok(Self::Paused),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            _ => Err(Error::MaintenanceRequired),
        }
    }
    const fn name(self) -> &'static str {
        match self {
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Bounded canonical observation; no authority is inferred from this view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunObservation {
    /// Exact durable identity.
    pub run_id: String,
    /// Current association; original admission session is historical provenance only.
    pub parent_session_id: Option<String>,
    /// Stable execution state.
    pub state: RunState,
}
/// Per-identity compatibility result. Unsupported data is never read as legacy state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunLookup {
    /// No canonical identity in either known representation.
    Missing,
    /// Identity belongs to legacy adapter; this API does not open it as v2.
    Legacy,
    /// Exact new-format observation.
    Available(RunObservation),
    /// Representation contract is unknown; preserve its data.
    UnsupportedRepresentation,
    /// This specific row is malformed or ambiguous.
    Degraded,
}
/// Bounded page with explicit extension-local compatibility status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPage {
    /// Each row is isolated; malformed data does not discard other rows.
    pub entries: Vec<(String, RunLookup)>,
    /// Exclusive last examined identity; presence does not guarantee another page exists.
    pub next_after: Option<String>,
    /// False means new-format rows could not safely be interpreted.
    pub representation_supported: bool,
}
fn text(row: &Row, field: &str) -> Result<String, Error> {
    match row.get(field) {
        Some(DatabaseValue::String(value)) if value.len() <= crate::MAX_ID_BYTES => Ok(value),
        _ => Err(Error::MaintenanceRequired),
    }
}
fn observation(row: &Row) -> Result<RunObservation, Error> {
    Ok(RunObservation {
        run_id: text(row, "run_id")?,
        parent_session_id: match row.get("association_session_id") {
            Some(DatabaseValue::Null) => None,
            Some(DatabaseValue::String(id)) if id.len() <= crate::MAX_ID_BYTES => Some(id),
            _ => return Err(Error::MaintenanceRequired),
        },
        state: RunState::decode(&text(row, "status")?)?,
    })
}
async fn supported(db: &dyn Database) -> Result<bool, Error> {
    super::compatible(db).await?;
    let rows = db
        .select(super::CONTRACT)
        .columns(&["version"])
        .where_eq("representation", 2_i64)
        .limit(1)
        .execute(db)
        .await?;
    Ok(rows.first().and_then(|row| row.get("version"))
        == Some(DatabaseValue::Int64(super::EXECUTION_VERSION)))
}

/// Lookup an exact run without falling back across representation errors.
/// # Errors
/// Returns storage/guard failures; unknown extension versions are returned explicitly.
pub async fn lookup(db: &dyn Database, run_id: &str) -> Result<RunLookup, Error> {
    crate::validate_id("run_id", run_id).map_err(|_| Error::InvalidAdmission)?;
    let tx = db.begin_transaction().await?;
    match lookup_in(&*tx, run_id).await {
        Ok(result) => {
            tx.commit().await?;
            Ok(result)
        }
        Err(error) => {
            tx.rollback().await?;
            Err(error)
        }
    }
}

/// Classify one exact identity within the caller's snapshot without starting a transaction.
///
/// Does not commit, roll back, repair, migrate or acquire execution authority.
/// # Errors
/// Rejects invalid identities and storage failures; compatibility/degradation remain explicit.
pub async fn lookup_in(db: &dyn Database, run_id: &str) -> Result<RunLookup, Error> {
    crate::validate_id("run_id", run_id).map_err(|_| Error::InvalidAdmission)?;
    let legacy = !db
        .select("workflow_runs")
        .columns(&["run_id"])
        .where_eq("run_id", run_id)
        .limit(1)
        .execute(db)
        .await?
        .is_empty();
    let extension = db.table_exists(RUNS).await?;
    let newer = extension
        && !db
            .select(RUNS)
            .columns(&["run_id"])
            .where_eq("run_id", run_id)
            .limit(1)
            .execute(db)
            .await?
            .is_empty();
    if legacy {
        return Ok(if newer {
            RunLookup::Degraded
        } else {
            RunLookup::Legacy
        });
    }
    if !newer {
        return Ok(RunLookup::Missing);
    }
    if !supported(db).await? {
        return Ok(RunLookup::UnsupportedRepresentation);
    }
    super::verified(db).await?;
    let result = match super::identity_in(db, run_id).await {
        Ok(None) => RunLookup::Missing,
        Ok(Some(RunRepresentation::Legacy)) => RunLookup::Legacy,
        Ok(Some(RunRepresentation::Version2)) => {
            let rows = db
                .select(RUNS)
                .columns(&["run_id", "association_session_id", "status"])
                .where_eq("run_id", run_id)
                .limit(1)
                .execute(db)
                .await?;
            rows.first()
                .and_then(|row| observation(row).ok())
                .map_or(RunLookup::Degraded, RunLookup::Available)
        }
        Err(Error::MaintenanceRequired) => RunLookup::Degraded,
        Err(error) => return Err(error),
    };
    Ok(result)
}

/// Read a bounded v2-only catalog; callers merge legacy pages through its own adapter.
/// # Errors
/// Rejects invalid page bounds or database/guard failures. Unknown contracts are page-local.
pub async fn catalog(db: &dyn Database, after: Option<&str>, limit: u16) -> Result<RunPage, Error> {
    if limit == 0 || limit > 100 {
        return Err(Error::InvalidAdmission);
    }
    let tx = db.begin_transaction().await?;
    if !supported(&*tx).await? {
        return Ok(RunPage {
            entries: vec![],
            next_after: None,
            representation_supported: false,
        });
    }
    super::verified(&*tx).await?;
    let mut query = tx
        .select(RUNS)
        .columns(&["run_id", "association_session_id", "status"])
        .sort("run_id", SortDirection::Asc)
        .limit(usize::from(limit));
    if let Some(after) = after {
        query = query.where_gt("run_id", after);
    }
    let rows = query.execute(&*tx).await?;
    let mut entries = Vec::new();
    for row in rows {
        let id = text(&row, "run_id")?;
        let value = match super::identity_in(&*tx, &id).await {
            Ok(Some(RunRepresentation::Version2)) => {
                observation(&row).map_or(RunLookup::Degraded, RunLookup::Available)
            }
            Ok(_) | Err(Error::MaintenanceRequired) => RunLookup::Degraded,
            Err(error) => return Err(error),
        };
        entries.push((id, value));
    }
    let next_after = entries.last().map(|(id, _)| id.clone());
    tx.commit().await?;
    Ok(RunPage {
        entries,
        next_after,
        representation_supported: true,
    })
}

/// Remove only a known-version session association after caller authorization.
///
/// No execution-owner acquisition, verification or contact is required. Exact expected association
/// prevents detaching a different session. Original admission JSON remains immutable provenance.
/// # Errors
/// Rejects unsupported representation, wrong association or persistence failure; never falls back.
pub async fn detach(
    db: &dyn Database,
    run_id: &str,
    expected_session: &str,
) -> Result<bool, Error> {
    crate::validate_id("session", expected_session).map_err(|_| Error::InvalidAdmission)?;
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await?;
    // Association capability is independently versioned from executable semantics.
    super::compatible(&*tx).await?;
    let contract = tx
        .select(super::CONTRACT)
        .columns(&["version"])
        .where_eq("representation", 0_i64)
        .limit(1)
        .execute(&*tx)
        .await?;
    if contract.first().and_then(|row| row.get("version")) != Some(DatabaseValue::Int64(1)) {
        return Err(Error::MaintenanceRequired);
    }
    if super::identity_in(&*tx, run_id).await? != Some(RunRepresentation::Version2) {
        return Err(Error::InvalidAdmission);
    }
    let rows = tx
        .select(RUNS)
        .columns(&["association_session_id"])
        .where_eq("run_id", run_id)
        .limit(1)
        .execute(&*tx)
        .await?;
    let association = rows.first().and_then(|r| r.get("association_session_id"));
    let changed = match association {
        Some(DatabaseValue::Null) => false,
        Some(DatabaseValue::String(id)) if id == expected_session => {
            tx.update(RUNS)
                .value("association_session_id", DatabaseValue::Null)
                .value("updated_at_ms", timestamp()?)
                .where_eq("run_id", run_id)
                .execute(&*tx)
                .await?;
            true
        }
        _ => return Err(Error::InvalidAdmission),
    };
    tx.commit().await?;
    Ok(changed)
}

/// Collected terminal attempts remain failures, but their exact activations need no success output.
/// Exclude any activation with a newer retry, including an unresolved retry.
pub(super) fn collected_terminal_activations(
    db: &dyn Database,
    run: &str,
) -> Box<dyn switchy_database::query::List> {
    Box::new(
        db.select("workflow_attempts_v2")
            .columns(&["activation_id"])
            .where_eq("run_id", run)
            .where_in(
                "outcome_json",
                vec![
                    DatabaseValue::String("\"Failed\"".into()),
                    DatabaseValue::String("\"Cancelled\"".into()),
                ],
            )
            .where_in(
                "dispatch_identity",
                Box::new(
                    db.select("workflow_collected_attempts_v2")
                        .columns(&["dispatch_identity"])
                        .where_eq("run_id", run),
                ) as Box<dyn switchy_database::query::List>,
            )
            .where_not_in(
                "activation_id",
                Box::new(
                    db.select("workflow_attempts_v2")
                        .columns(&["activation_id"])
                        .where_eq("run_id", run)
                        .where_not_in(
                            "dispatch_identity",
                            Box::new(
                                db.select("workflow_collected_attempts_v2")
                                    .columns(&["dispatch_identity"])
                                    .where_eq("run_id", run),
                            ) as Box<dyn switchy_database::query::List>,
                        ),
                ) as Box<dyn switchy_database::query::List>,
            ),
    )
}

async fn has_attempt(db: &dyn Database, run: &str, outcome: DatabaseValue) -> Result<bool, Error> {
    Ok(!db
        .select("workflow_attempts_v2")
        .columns(&["dispatch_identity"])
        .where_eq("run_id", run)
        .where_eq("outcome_json", outcome)
        .where_not_in(
            "dispatch_identity",
            Box::new(
                db.select("workflow_collected_attempts_v2")
                    .columns(&["dispatch_identity"])
                    .where_eq("run_id", run),
            ) as Box<dyn switchy_database::query::List>,
        )
        .limit(1)
        .execute(db)
        .await?
        .is_empty())
}
/// Derive and commit a stable terminal state from canonical graph and attempt outcomes.
///
/// Pending intents/receipts and retained leases block terminalization. Cancellation intent takes
/// precedence over failure and success, but never proves pending operations had no effects.
/// # Errors
/// Rejects stale authority, unknown outcomes, inconsistent state, incompatible storage or failure.
pub async fn terminalize(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    run_id: &str,
) -> Result<Option<RunState>, Error> {
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await?;
    super::verified(&*tx).await?;
    let root = terminal_owner(&*tx, run_id, owner).await?;
    let previous = RunState::decode(&text(&root, "status")?)?;
    if previous != RunState::Paused {
        tx.commit().await?;
        return Ok(Some(previous));
    }
    if super::graph::frontier::pending(&*tx, run_id).await? {
        return Ok(None);
    }
    if has_attempt(&*tx, run_id, DatabaseValue::Null).await? {
        // Preparation and settlement invalidate proofs in their canonical transactions.
        // Observing unresolved work must not advance the controller outcome epoch.
        tx.commit().await?;
        return Ok(None);
    }
    if root.get("active_attempts") != Some(DatabaseValue::Int64(0)) {
        return Err(Error::MaintenanceRequired);
    }
    validate_terminal_storage(&*tx, run_id).await?;
    let outcome = |name: &str| DatabaseValue::String(format!("\"{name}\""));
    let cancelled = !tx
        .select("workflow_cancellation_v2")
        .columns(&["run_id"])
        .where_eq("run_id", run_id)
        .limit(1)
        .execute(&*tx)
        .await?
        .is_empty();
    if !cancelled
        && !tx
            .select("workflow_retry_grants_v2")
            .columns(&["run_id"])
            .where_eq("run_id", run_id)
            .where_eq("consumed", 0_i64)
            .limit(1)
            .execute(&*tx)
            .await?
            .is_empty()
    {
        tx.commit().await?;
        return Ok(None);
    }
    if !cancelled && !super::graph::terminal_scan::step(&*tx, run_id, 32).await? {
        tx.commit().await?;
        return Ok(None);
    }
    let next = if cancelled || has_attempt(&*tx, run_id, outcome("Cancelled")).await? {
        Some(RunState::Cancelled)
    } else if has_attempt(&*tx, run_id, outcome("Failed")).await?
        || !tx
            .select("workflow_control_failures_v2")
            .columns(&["identity"])
            .where_eq("run_id", run_id)
            .limit(1)
            .execute(&*tx)
            .await?
            .is_empty()
    {
        Some(RunState::Failed)
    } else {
        let graph = tx
            .select("workflow_graphs_v2")
            .columns(&["terminal_json"])
            .where_eq("run_id", run_id)
            .limit(1)
            .execute(&*tx)
            .await?;
        match graph.first().and_then(|r| r.get("terminal_json")) {
            Some(DatabaseValue::String(_)) => Some(RunState::Completed),
            Some(DatabaseValue::Null) => None,
            _ => return Err(Error::MaintenanceRequired),
        }
    };
    if let Some(next) = next {
        let history = super::semantic_history::lifecycle::begin(&*tx, run_id).await?;
        let recorded_at = timestamp()?;
        tx.update(RUNS)
            .value("status", next.name())
            .value("updated_at_ms", recorded_at)
            .where_eq("run_id", run_id)
            .execute(&*tx)
            .await?;
        super::semantic_history::lifecycle::finish(
            &*tx,
            history,
            next.name(),
            u64::try_from(recorded_at).map_err(|_| Error::MaintenanceRequired)?,
        )
        .await?;
    }
    tx.commit().await?;
    Ok(next)
}

/// Read only the independently versioned canonical association.
/// # Errors
/// Rejects unknown association capability, duplicate identity or malformed association; no owner needed.
pub async fn association(db: &dyn Database, run_id: &str) -> Result<Option<String>, Error> {
    let tx = db.begin_transaction().await?;
    super::compatible(&*tx).await?;
    let contract = tx
        .select(super::CONTRACT)
        .columns(&["version"])
        .where_eq("representation", 0_i64)
        .limit(1)
        .execute(&*tx)
        .await?;
    if contract.first().and_then(|row| row.get("version")) != Some(DatabaseValue::Int64(1)) {
        return Err(Error::MaintenanceRequired);
    }
    if super::identity_in(&*tx, run_id).await? != Some(RunRepresentation::Version2) {
        return Err(Error::InvalidAdmission);
    }
    let rows = tx
        .select(RUNS)
        .columns(&["association_session_id"])
        .where_eq("run_id", run_id)
        .limit(1)
        .execute(&*tx)
        .await?;
    let value = match rows
        .first()
        .and_then(|row| row.get("association_session_id"))
    {
        Some(DatabaseValue::Null) => None,
        Some(DatabaseValue::String(id)) if id.len() <= crate::MAX_ID_BYTES => Some(id),
        _ => return Err(Error::MaintenanceRequired),
    };
    tx.commit().await?;
    Ok(value)
}

/// Project a known representation into the normalized run-summary contract.
///
/// The current association overrides historical admission provenance. Output references remain
/// absent until the representation provides a normalized output-inspection identity contract.
/// # Errors
/// Rejects unknown versions/statuses, malformed admission or ambiguous identity; never falls back.
pub async fn summary(
    db: &dyn Database,
    run_id: &str,
) -> Result<Option<bcode_workflow::WorkflowRunSummary>, Error> {
    let tx = db.begin_transaction().await?;
    let result = summary_in(&*tx, run_id).await?;
    tx.commit().await?;
    Ok(result)
}

/// Read a bounded canonical summary inside the caller's verified read snapshot.
///
/// Does not begin or commit a transaction.
/// # Errors
/// Rejects unsupported representation, inconsistent storage or query failures.
pub async fn summary_in(
    tx: &dyn switchy_database::DatabaseTransaction,
    run_id: &str,
) -> Result<Option<bcode_workflow::WorkflowRunSummary>, Error> {
    super::verified(tx).await?;
    match super::identity_in(tx, run_id).await? {
        None => return Ok(None),
        Some(RunRepresentation::Legacy) => return Err(Error::UnsupportedStore),
        Some(RunRepresentation::Version2) => {}
    }
    let rows = tx
        .select(RUNS)
        .where_eq("run_id", run_id)
        .limit(1)
        .execute(tx)
        .await?;
    let row = rows.first().ok_or(Error::MaintenanceRequired)?;
    let observed = observation(row)?;
    let Some(DatabaseValue::String(json)) = row.get("admission_json") else {
        return Err(Error::MaintenanceRequired);
    };
    if json.len() > super::MAX_ADMISSION_BYTES {
        return Err(Error::MaintenanceRequired);
    }
    let request: crate::NewWorkflowRun =
        serde_json::from_str(&json).map_err(|_| Error::MaintenanceRequired)?;
    let cancellation = tx
        .select("workflow_cancellation_v2")
        .columns(&["requested_at_ms"])
        .where_eq("run_id", run_id)
        .limit(1)
        .execute(tx)
        .await?;
    let cancellation_requested_at_ms = match cancellation
        .first()
        .and_then(|row| row.get("requested_at_ms"))
    {
        None => None,
        Some(DatabaseValue::Int64(n)) => {
            Some(u64::try_from(n).map_err(|_| Error::MaintenanceRequired)?)
        }
        _ => return Err(Error::MaintenanceRequired),
    };
    let terminal = if observed.state == RunState::Completed {
        super::graph::completion::terminal_in(tx, run_id).await?
    } else {
        None
    };
    let result = bcode_workflow::WorkflowRunSummary {
        run_id: request.run_id,
        definition_id: request.definition_id,
        definition_version: request.definition_version,
        workspace_snapshot: request.workspace_snapshot,
        parent_session_generation: if observed.parent_session_id.is_some() {
            request.parent_session_generation
        } else {
            None
        },
        parent_session_id: observed.parent_session_id,
        binding: request.binding,
        authored_provenance: request.authored_provenance,
        terminal_output_id: terminal.as_ref().map(|output| output.output_id.clone()),
        terminal_output_checksum_sha256: terminal
            .as_ref()
            .map(|output| {
                bcode_workflow::workflow_canonical_value_sha256(&output.value)
                    .map_err(|_| Error::MaintenanceRequired)
            })
            .transpose()?,
        authorization_profile: request.authorization_profile,
        authorization_ceiling: request.authorization_ceiling,
        status: match observed.state {
            RunState::Paused => bcode_workflow::RunStatus::Paused,
            RunState::Completed => bcode_workflow::RunStatus::Completed,
            RunState::Failed => bcode_workflow::RunStatus::Failed,
            RunState::Cancelled => bcode_workflow::RunStatus::Cancelled,
        },
        cancellation_requested_at_ms,
        created_at_ms: request.created_at_ms,
        updated_at_ms: match row.get("updated_at_ms") {
            Some(DatabaseValue::Int64(n)) => {
                u64::try_from(n).map_err(|_| Error::MaintenanceRequired)?
            }
            _ => return Err(Error::MaintenanceRequired),
        },
    };
    Ok(Some(result))
}

pub(super) fn timestamp() -> Result<i64, Error> {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| Error::InvalidAdmission)?
            .as_millis(),
    )
    .map_err(|_| Error::InvalidAdmission)
}

async fn terminal_owner(
    db: &dyn Database,
    run_id: &str,
    owner: &WorkflowExecutionAuthority,
) -> Result<Row, Error> {
    let rows = db
        .select(RUNS)
        .columns(&["authority_json", "status", "active_attempts"])
        .where_eq("run_id", run_id)
        .limit(1)
        .execute(db)
        .await?;
    let root = rows.into_iter().next().ok_or(Error::InvalidAdmission)?;
    let owner = serde_json::to_string(owner).map_err(|_| Error::InvalidAdmission)?;
    if root.get("authority_json") != Some(DatabaseValue::String(owner)) {
        return Err(Error::AuthorityConflict);
    }
    Ok(root)
}

async fn validate_terminal_storage(db: &dyn Database, run_id: &str) -> Result<(), Error> {
    if !db
        .select("workflow_resource_leases_v2")
        .columns(&["dispatch_identity"])
        .where_eq("run_id", run_id)
        .limit(1)
        .execute(db)
        .await?
        .is_empty()
    {
        return Err(Error::MaintenanceRequired);
    }
    let outcome = |name: &str| DatabaseValue::String(format!("\"{name}\""));
    if !db
        .select("workflow_attempts_v2")
        .columns(&["dispatch_identity"])
        .where_eq("run_id", run_id)
        .where_not_in(
            "outcome_json",
            vec![
                outcome("Succeeded"),
                outcome("Failed"),
                outcome("Cancelled"),
            ],
        )
        .limit(1)
        .execute(db)
        .await?
        .is_empty()
    {
        return Err(Error::MaintenanceRequired);
    }
    Ok(())
}
