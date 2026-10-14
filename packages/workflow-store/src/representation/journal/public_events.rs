//! Timed public facts appended by semantic producers in their source transactions.
use super::{Database, DatabaseValue, Error};
use bcode_workflow::{
    WorkflowHistoryEvent, WorkflowLifecycleHistoryPayload, WorkflowSemanticHistoryPayload,
};
use serde::{Deserialize, Serialize};
use switchy_database::{
    DatabaseTransaction,
    query::{FilterableQuery, SortDirection},
};

/// Guarded control/output source commitments.
pub mod control;
#[cfg(test)]
mod control_tests;
/// Verified version-one exclusion of legacy permission-storage collections.
pub mod exclusions;
mod installation;
/// Independently opted-in, guarded repeat emission facts; not complete workflow history.
pub mod repeat;
#[cfg(test)]
mod tests;
const CONTRACT: &str = "workflow_public_events_contract_v1";
const ORIGINS: &str = "workflow_public_events_origins_v1";
const EVENTS: &str = "workflow_public_events_v1";

/// Closed producer facts. Never private receipts, messages or request-only context.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Fact {
    /// Exact guarded attempt transition.
    Attempt {
        dispatch_identity: String,
        attempt: bcode_workflow::WorkflowHistoryAttemptCorrelation,
        payload: WorkflowSemanticHistoryPayload,
    },
    /// Exact guarded run transition.
    Lifecycle {
        payload: WorkflowLifecycleHistoryPayload,
    },
    /// Exact control producer result, with no supplied value or private evidence.
    Control {
        node_id: String,
        activation_id: String,
        operation: control::Operation,
        outcome: control::Outcome,
    },
}

/// Portable committed diagnostic page shared with application consumers.
pub use bcode_workflow::WorkflowCommittedHistoryPage as Page;

pub(super) async fn install(db: &dyn Database) -> Result<(), Error> {
    installation::install(db).await?;
    control::install(db).await
}
pub(super) async fn enable(tx: &dyn DatabaseTransaction, run: &str) -> Result<(), Error> {
    installation::verify(tx).await?;
    control::enable(tx, run).await?;
    tx.insert(ORIGINS)
        .value("run_id", run)
        .value("version", 1_i64)
        .execute(tx)
        .await?;
    Ok(())
}

/// Append one producer-defined fact atomically with its actual source transition.
/// No-op for non-opted-in runs. Uses the producer's recorded clock, never read-time clock.
/// # Errors
/// Rejects unsupported/damaged schema, malformed correlation, oversized facts or database errors.
pub async fn append(
    tx: &dyn DatabaseTransaction,
    run: &str,
    identity: &str,
    time: u64,
    fact: Fact,
) -> Result<(), Error> {
    append_in(tx, run, identity, time, fact).await
}

async fn append_in(
    tx: &dyn Database,
    run: &str,
    identity: &str,
    time: u64,
    fact: Fact,
) -> Result<(), Error> {
    if tx.get_table_info(CONTRACT).await?.is_none() {
        return Ok(());
    }
    installation::verify(tx).await?;
    if tx
        .select(ORIGINS)
        .where_eq("run_id", run)
        .limit(1)
        .execute(tx)
        .await?
        .is_empty()
    {
        return Ok(());
    }
    if identity.len() != 64
        || !identity
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(Error::InvalidAdmission);
    }
    if let Fact::Attempt {
        dispatch_identity,
        attempt,
        ..
    } = &fact
        && (attempt.run_id != run || dispatch_identity.len() != 64)
    {
        return Err(Error::MaintenanceRequired);
    }
    let json = serde_json::to_string(&fact).map_err(|_| Error::InvalidAdmission)?;
    append_encoded(tx, run, identity, time, "committed_fact_v1", &json).await
}

async fn append_encoded(
    tx: &dyn Database,
    run: &str,
    identity: &str,
    time: u64,
    event_type: &str,
    json: &str,
) -> Result<(), Error> {
    if json.len() > 8192 {
        return Err(Error::InvalidAdmission);
    }
    let existing = tx
        .select(EVENTS)
        .where_eq("identity", identity)
        .limit(1)
        .execute(tx)
        .await?;
    let time = i64::try_from(time).map_err(|_| Error::InvalidAdmission)?;
    if let Some(row) = existing.first() {
        if row.get("event_type") != Some(DatabaseValue::String(event_type.into())) {
            return Err(Error::MaintenanceRequired);
        }
        if row.get("payload") != Some(DatabaseValue::String(json.into()))
            || row.get("run_id") != Some(DatabaseValue::String(run.into()))
            || row.get("recorded_at_ms") != Some(DatabaseValue::Int64(time))
        {
            return Err(Error::MaintenanceRequired);
        }
        return Ok(());
    }
    tx.insert(EVENTS)
        .value("identity", identity)
        .value("run_id", run)
        .value("payload", json)
        .value("event_type", event_type)
        .value("recorded_at_ms", time)
        .execute(tx)
        .await?;
    Ok(())
}

/// Validate one retained row and project only reviewed public facts.
fn decode_event(row: &switchy_database::Row, run: &str) -> Result<WorkflowHistoryEvent, Error> {
    let Some(DatabaseValue::String(payload)) = row.get("payload") else {
        return Err(Error::MaintenanceRequired);
    };
    if payload.len() > 8192 {
        return Err(Error::MaintenanceRequired);
    }
    let Some(DatabaseValue::String(operation_id)) = row.get("identity") else {
        return Err(Error::MaintenanceRequired);
    };
    if operation_id.len() != 64
        || !operation_id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(Error::MaintenanceRequired);
    }
    if row.get("run_id") != Some(DatabaseValue::String(run.into())) {
        return Err(Error::MaintenanceRequired);
    }
    let event_type = super::text(row, "event_type")?;
    let public_payload = if event_type == "committed_fact_v1" {
        let fact: Fact = serde_json::from_str(&payload).map_err(|_| Error::MaintenanceRequired)?;
        serde_json::json!({"operation_id":operation_id,"fact":fact})
    } else if event_type == "repeat_emission_v1" {
        let summary: bcode_workflow::WorkflowRepeatOutcomeSummary =
            serde_json::from_str(&payload).map_err(|_| Error::MaintenanceRequired)?;
        repeat::validate_retained(
            &summary,
            run,
            &operation_id,
            super::integer(row, "recorded_at_ms")?,
        )?;
        serde_json::json!({"operation_id":operation_id,"repeat_emission":summary})
    } else {
        serde_json::json!({"unavailable":"unreviewed_event_payload"})
    };
    Ok(WorkflowHistoryEvent {
        event_seq: super::integer(row, "sequence")?,
        run_id: run.into(),
        event_type,
        payload: public_payload,
        created_at_ms: super::integer(row, "recorded_at_ms")?,
    })
}

/// Bounded immutable pin scan. Historical absence remains unsupported, not synthesized.
/// # Errors
/// Rejects invalid versions/limits/future pins or malformed retained events.
pub async fn page(
    db: &dyn Database,
    version: u32,
    run: &str,
    after: u64,
    through: Option<u64>,
    limit: u16,
) -> Result<Page, Error> {
    if version != 1 || !(1..=100).contains(&limit) {
        return Err(Error::InvalidAdmission);
    }
    let tx = db.begin_transaction().await?;
    let result = page_in(&*tx, version, run, after, through, limit).await?;
    tx.commit().await?;
    Ok(result)
}

async fn page_in(
    tx: &dyn Database,
    version: u32,
    run: &str,
    after: u64,
    through: Option<u64>,
    limit: u16,
) -> Result<Page, Error> {
    if version != 1 || !(1..=100).contains(&limit) {
        return Err(Error::InvalidAdmission);
    }
    installation::verify(tx).await?;
    let origins = tx
        .select(ORIGINS)
        .where_eq("run_id", run)
        .limit(1)
        .execute(tx)
        .await?;
    let from_admission = if let Some(row) = origins.first() {
        if super::integer(row, "version")? != 1 {
            return Err(Error::MaintenanceRequired);
        }
        true
    } else {
        false
    };
    let attempt_coverage = super::super::semantic_history::coverage(tx, run).await?;
    let lifecycle_coverage = super::super::semantic_history::lifecycle::coverage(tx, run).await?;
    let control_coverage = control::coverage(tx, run).await?;
    let latest = tx
        .select(EVENTS)
        .where_eq("run_id", run)
        .sort("sequence", SortDirection::Desc)
        .limit(1)
        .execute(tx)
        .await?;
    let high = latest
        .first()
        .map(|r| super::integer(r, "sequence"))
        .transpose()?
        .unwrap_or(0);
    let through = through.unwrap_or(high);
    if through > high || after > through {
        return Err(Error::InvalidAdmission);
    }
    let rows = tx
        .select(EVENTS)
        .where_eq("run_id", run)
        .where_gt(
            "sequence",
            i64::try_from(after).map_err(|_| Error::InvalidAdmission)?,
        )
        .where_lte(
            "sequence",
            i64::try_from(through).map_err(|_| Error::InvalidAdmission)?,
        )
        .sort("sequence", SortDirection::Asc)
        .limit(usize::from(limit) + 1)
        .execute(tx)
        .await?;
    if !from_admission && !rows.is_empty() {
        return Err(Error::MaintenanceRequired);
    }
    let more = rows.len() > usize::from(limit);
    let events = rows
        .iter()
        .take(usize::from(limit))
        .map(|row| decode_event(row, run))
        .collect::<Result<Vec<_>, Error>>()?;
    let next_after_sequence = if more {
        events.last().map(|e| e.event_seq)
    } else {
        None
    };
    Ok(Page {
        version: 1,
        run_id: run.into(),
        from_admission,
        scopes: vec![
            bcode_workflow::WorkflowCommittedHistoryScope::AttemptsV1,
            bcode_workflow::WorkflowCommittedHistoryScope::RunLifecycleV1,
            bcode_workflow::WorkflowCommittedHistoryScope::ControlSourcesV1,
        ],
        attempt_coverage,
        lifecycle_coverage,
        control_coverage,
        through_sequence: through,
        next_after_sequence,
        events,
    })
}
