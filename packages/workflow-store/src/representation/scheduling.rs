//! Bounded read-only scheduling observations from one database snapshot; not dispatch authority.
use super::{Database, DatabaseValue, FilterableQuery, RUNS, RepresentationError as Error};
use switchy_database::query::SortDirection;
#[cfg(test)]
mod tests;

/// Persisted activation state, without guessing readiness or blocker reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationState {
    /// Canonically skipped.
    Skipped,
    /// Canonical output exists.
    OutputCommitted,
    /// Neither skipped nor output committed. Attempt and gate state may explain why.
    Unresolved,
}
/// Latest attempt for this exact activation, with no provider-private payloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptObservation {
    /// Persisted attempt number.
    pub attempt: u32,
    /// Exact dispatch identity.
    pub dispatch_identity: String,
    /// Durable receipt exists; absence does not prove no side effects occurred.
    pub receipt_present: bool,
    /// Canonical terminal outcome, absent while unresolved.
    pub terminal: Option<super::attempts::TerminalOutcome>,
}
/// One current activation and its latest exact attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationObservation {
    /// Canonical node identity, including virtual members.
    pub node_id: String,
    /// Canonical activation identity.
    pub activation_id: String,
    /// Only persisted activation facts, not inferred execution status.
    pub status: ActivationState,
    /// None means no attempt exists, not unknown or failed.
    pub latest_attempt: Option<AttemptObservation>,
}
/// Same-snapshot local-run capacity and paged observations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulingSnapshot {
    /// Scope of both concurrency cap and occupied count; excludes descendants.
    pub run_id: String,
    /// Current canonical graph revision.
    pub revision: u64,
    /// Actual persisted run status.
    pub run_status: super::runs::RunState,
    /// Local run concurrency cap; None explicitly means uncapped.
    pub concurrency_cap: Option<u32>,
    /// Local admitted nonterminal attempts, including ambiguous preparations.
    pub occupied: u64,
    /// Explicit user pause, separate from run status.
    pub user_paused: bool,
    /// At most the requested number of current activations.
    pub activations: Vec<ActivationObservation>,
    /// Exclusive last returned identity only when lookahead proves another row exists.
    pub next_activation_id: Option<String>,
}
fn text(row: &switchy_database::Row, field: &str) -> Result<String, Error> {
    match row.get(field) {
        Some(DatabaseValue::String(v)) => Ok(v),
        _ => Err(Error::MaintenanceRequired),
    }
}
fn integer(row: &switchy_database::Row, field: &str) -> Result<u64, Error> {
    match row.get(field) {
        Some(DatabaseValue::Int64(v)) => u64::try_from(v).map_err(|_| Error::MaintenanceRequired),
        _ => Err(Error::MaintenanceRequired),
    }
}
/// Read a bounded page under one snapshot without acquiring execution authority.
/// Revision pins topology, not an event stream: later arrivals require a fresh scan.
/// # Errors
/// Rejects unsupported storage, missing run, malformed state, stale revision, invalid bounds
/// or an unpinned continuation. Performs no repair, installation or writes.
pub async fn snapshot(
    db: &dyn Database,
    run: &str,
    expected_revision: Option<u64>,
    after_activation_id: Option<&str>,
    limit: u16,
) -> Result<SchedulingSnapshot, Error> {
    if !(1..=100).contains(&limit) || (after_activation_id.is_some() && expected_revision.is_none())
    {
        return Err(Error::InvalidAdmission);
    }
    let tx = db.begin_transaction().await?;
    let result = snapshot_in(&*tx, run, expected_revision, after_activation_id, limit).await?;
    tx.commit().await?;
    Ok(result)
}
/// Observe scheduling facts within an existing read snapshot; never opens a nested transaction.
/// # Errors
/// Same validation and storage errors as `snapshot`.
pub async fn snapshot_in(
    tx: &dyn switchy_database::DatabaseTransaction,
    run: &str,
    expected_revision: Option<u64>,
    after_activation_id: Option<&str>,
    limit: u16,
) -> Result<SchedulingSnapshot, Error> {
    if !(1..=100).contains(&limit) || (after_activation_id.is_some() && expected_revision.is_none())
    {
        return Err(Error::InvalidAdmission);
    }
    super::verified(tx).await?;
    let revision = super::graph_pages::revision(tx, run).await?;
    if expected_revision.is_some_and(|v| v != revision) {
        return Err(Error::InvalidAdmission);
    }
    let roots = tx
        .select(RUNS)
        .where_eq("run_id", run)
        .limit(1)
        .execute(tx)
        .await?;
    let root = roots.first().ok_or(Error::InvalidAdmission)?;
    super::admission_projection::package_binding(tx, run).await?;
    let admission: crate::NewWorkflowRun = serde_json::from_str(&text(root, "admission_json")?)
        .map_err(|_| Error::MaintenanceRequired)?;
    let mut query = tx
        .select("workflow_activations_v2")
        .columns(&["identity", "node_id", "skipped", "output_json"])
        .where_eq("run_id", run)
        .where_eq("current", true)
        .sort("identity", SortDirection::Asc)
        .limit(usize::from(limit) + 1);
    if let Some(after) = after_activation_id {
        query = query.where_gt("identity", after);
    }
    let mut rows = query.execute(tx).await?;
    let more = rows.len() > usize::from(limit);
    rows.truncate(usize::from(limit));
    let mut activations = Vec::with_capacity(rows.len());
    for row in rows {
        let activation_id = text(&row, "identity")?;
        let node_id = text(&row, "node_id")?;
        let skipped = match row.get("skipped") {
            Some(DatabaseValue::Bool(v)) => v,
            Some(DatabaseValue::Int64(0)) => false,
            Some(DatabaseValue::Int64(1)) => true,
            _ => return Err(Error::MaintenanceRequired),
        };
        let status = if skipped {
            ActivationState::Skipped
        } else {
            match row.get("output_json") {
                Some(DatabaseValue::Null) => ActivationState::Unresolved,
                Some(DatabaseValue::String(_)) => ActivationState::OutputCommitted,
                _ => return Err(Error::MaintenanceRequired),
            }
        };
        let latest_attempt = latest(tx, run, &node_id, &activation_id).await?;
        activations.push(ActivationObservation {
            node_id,
            activation_id,
            status,
            latest_attempt,
        });
    }
    let next_activation_id = if more {
        activations.last().map(|v| v.activation_id.clone())
    } else {
        None
    };
    let result = SchedulingSnapshot {
        run_id: run.into(),
        revision,
        run_status: super::runs::RunState::decode(&text(root, "status")?)?,
        concurrency_cap: admission.limits.concurrency_cap,
        occupied: integer(root, "active_attempts")?,
        user_paused: super::controls::user_paused(tx, run).await?,
        activations,
        next_activation_id,
    };
    Ok(result)
}
async fn latest(
    db: &dyn Database,
    run: &str,
    node: &str,
    activation: &str,
) -> Result<Option<AttemptObservation>, Error> {
    let rows = db
        .select("workflow_attempts_v2")
        .columns(&[
            "attempt_number",
            "dispatch_identity",
            "receipt_json",
            "outcome_json",
        ])
        .where_eq("run_id", run)
        .where_eq("node_id", node)
        .where_eq("activation_id", activation)
        .sort("attempt_number", SortDirection::Desc)
        .limit(1)
        .execute(db)
        .await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let receipt_present = match row.get("receipt_json") {
        Some(DatabaseValue::Null) => false,
        Some(DatabaseValue::String(_)) => true,
        _ => return Err(Error::MaintenanceRequired),
    };
    let terminal = match row.get("outcome_json") {
        Some(DatabaseValue::Null) => None,
        Some(DatabaseValue::String(v)) => {
            Some(serde_json::from_str(&v).map_err(|_| Error::MaintenanceRequired)?)
        }
        _ => return Err(Error::MaintenanceRequired),
    };
    if terminal.is_some() && !receipt_present {
        return Err(Error::MaintenanceRequired);
    }
    Ok(Some(AttemptObservation {
        attempt: u32::try_from(integer(row, "attempt_number")?)
            .map_err(|_| Error::MaintenanceRequired)?,
        dispatch_identity: text(row, "dispatch_identity")?,
        receipt_present,
        terminal,
    }))
}
