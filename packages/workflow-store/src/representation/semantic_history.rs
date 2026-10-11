//! Transactional attempt history with immutable old-writer coverage obligations.
//!
//! This scope excludes every other workflow semantic source.
use super::{Database, DatabaseValue, RepresentationError as Error};
use bcode_workflow::{WorkflowSemanticHistoryEvent, WorkflowSemanticHistoryPayload};
use sha2::{Digest, Sha256};
use switchy_database::{
    DatabaseTransaction, Row,
    query::{FilterableQuery, SortDirection},
};
mod checkpoint_guards;
mod checkpoints;
mod classification;
mod installation;
/// Independent run admission and committed status history.
pub mod lifecycle;
#[cfg(test)]
mod tests;

const CONTRACT: &str = "workflow_attempt_history_contract_v1";
const ORIGINS: &str = "workflow_attempt_history_origins_v1";
const OBLIGATIONS: &str = "workflow_attempt_history_obligations_v1";
const EVENTS: &str = "workflow_attempt_history_events_v1";
const ACKS: &str = "workflow_attempt_history_acks_v1";
const SOURCE: &str = "workflow_attempts_v2";
const FIELDS: &[&str] = &[
    "dispatch_identity",
    "run_id",
    "prepared_json",
    "node_id",
    "activation_id",
    "attempt_number",
    "receipt_json",
    "outcome_json",
    "terminal_at_ms",
];
pub(super) async fn install_in(db: &dyn Database) -> Result<(), Error> {
    installation::install_in(db).await
}

pub(super) struct Transition {
    boundary: i64,
    before: Option<Row>,
    baseline: checkpoints::Baseline,
}

async fn high_water(db: &dyn Database, table: &str, run: Option<&str>) -> Result<i64, Error> {
    let mut query = db
        .select(table)
        .columns(&["sequence"])
        .sort("sequence", SortDirection::Desc)
        .limit(1);
    if let Some(run) = run {
        query = query.where_eq("run_id", run);
    }
    let rows = query.execute(db).await?;
    rows.first().map_or(Ok(0), |row| integer(row, "sequence"))
}

/// Capture a source transition only inside the caller's owned Immediate transaction.
/// The caller must not commit, yield ownership or acknowledge earlier obligations.
pub(super) async fn begin(
    tx: &dyn DatabaseTransaction,
    identity: &str,
    run: &str,
) -> Result<Option<Transition>, Error> {
    if tx.get_table_info(CONTRACT).await?.is_none() {
        return Ok(None);
    }
    installation::verify(tx).await?;
    let before = tx
        .select(SOURCE)
        .where_eq("dispatch_identity", identity)
        .limit(1)
        .execute(tx)
        .await?
        .into_iter()
        .next();
    Ok(Some(Transition {
        baseline: checkpoints::baseline(tx, run).await?,
        boundary: high_water(tx, OBLIGATIONS, None).await?,
        before,
    }))
}

fn integer(row: &Row, key: &str) -> Result<i64, Error> {
    match row.get(key) {
        Some(DatabaseValue::Int64(value)) => Ok(value),
        _ => Err(Error::MaintenanceRequired),
    }
}
fn text(row: &Row, key: &str) -> Result<String, Error> {
    match row.get(key) {
        Some(DatabaseValue::String(value)) => Ok(value),
        _ => Err(Error::MaintenanceRequired),
    }
}

/// Verify exact OLD/NEW facts, then append an event and its immutable acknowledgement.
/// No count, timestamp or current-state inference establishes correspondence.
pub(super) async fn finish(
    tx: &dyn DatabaseTransaction,
    transition: Option<Transition>,
    expected: Row,
    payload: WorkflowSemanticHistoryPayload,
    recorded_at_ms: u64,
) -> Result<(), Error> {
    let Some(transition) = transition else {
        return Ok(());
    };
    let obligations = tx
        .select(OBLIGATIONS)
        .where_gt("sequence", transition.boundary)
        .limit(2)
        .execute(tx)
        .await?;
    let [obligation] = obligations.as_slice() else {
        return Err(Error::MaintenanceRequired);
    };
    let operation = if transition.before.is_some() {
        "update"
    } else {
        "insert"
    };
    if text(obligation, "operation")? != operation {
        return Err(Error::MaintenanceRequired);
    }
    for field in FIELDS {
        for (prefix, source) in [
            ("old", transition.before.as_ref()),
            ("new", Some(&expected)),
        ] {
            let value = source
                .and_then(|row| row.get(field))
                .unwrap_or(DatabaseValue::Null);
            if obligation.get(&format!("{prefix}_{field}")) != Some(value) {
                return Err(Error::MaintenanceRequired);
            }
            if field.ends_with("json")
                || matches!(
                    *field,
                    "dispatch_identity" | "run_id" | "node_id" | "activation_id"
                )
            {
                let status = integer(obligation, &format!("{prefix}_{field}_status"))?;
                if status > 1 {
                    return Err(Error::MaintenanceRequired);
                }
            }
        }
    }
    let run_id = text(&expected, "run_id")?;
    if transition.baseline.run != run_id {
        return Err(Error::MaintenanceRequired);
    }
    let dispatch_identity = text(&expected, "dispatch_identity")?;
    let prepared: crate::PreparedAttempt = serde_json::from_str(&text(&expected, "prepared_json")?)
        .map_err(|_| Error::MaintenanceRequired)?;
    classification::validate(
        transition.before.as_ref(),
        &expected,
        &prepared,
        &payload,
        recorded_at_ms,
    )?;
    if prepared.run_id != run_id || prepared.dispatch_identity() != dispatch_identity {
        return Err(Error::MaintenanceRequired);
    }
    let sequence = high_water(tx, EVENTS, Some(&run_id))
        .await?
        .checked_add(1)
        .ok_or(Error::MaintenanceRequired)?;
    let operation_id = hex::encode(Sha256::digest(
        serde_json::to_vec(&(&dispatch_identity, &payload)).map_err(|_| Error::InvalidAdmission)?,
    ));
    let event = WorkflowSemanticHistoryEvent {
        run_id: run_id.clone(),
        sequence: u64::try_from(sequence).map_err(|_| Error::MaintenanceRequired)?,
        operation_id,
        recorded_at_ms,
        attempt: bcode_workflow::WorkflowHistoryAttemptCorrelation {
            run_id: run_id.clone(),
            node_id: prepared.node_id,
            activation_id: prepared.activation_id,
            attempt: prepared.attempt,
        },
        dispatch_identity,
        payload,
    };
    append_acknowledged_event(
        tx,
        &event,
        integer(obligation, "sequence")?,
        transition.baseline,
    )
    .await
}

/// Append the event, exact acknowledgement and checkpoint in the source transaction.
async fn append_acknowledged_event(
    tx: &dyn DatabaseTransaction,
    event: &WorkflowSemanticHistoryEvent,
    obligation: i64,
    baseline: checkpoints::Baseline,
) -> Result<(), Error> {
    let sequence = i64::try_from(event.sequence).map_err(|_| Error::MaintenanceRequired)?;
    tx.insert(EVENTS)
        .value("run_id", event.run_id.as_str())
        .value("sequence", sequence)
        .value("identity", event.operation_id.as_str())
        .value(
            "event_json",
            serde_json::to_string(&event).map_err(|_| Error::InvalidAdmission)?,
        )
        .execute(tx)
        .await?;
    tx.insert(ACKS)
        .value("obligation", obligation)
        .value("event_identity", event.operation_id.as_str())
        .execute(tx)
        .await?;
    checkpoints::advance(tx, baseline, obligation, sequence, &event.operation_id).await?;
    Ok(())
}

pub(super) async fn prepared(
    tx: &dyn DatabaseTransaction,
    transition: Option<Transition>,
    prepared: &crate::PreparedAttempt,
) -> Result<(), Error> {
    finish(
        tx,
        transition,
        source_row(prepared, None, None, None)?,
        WorkflowSemanticHistoryPayload::AttemptPrepared {
            side_effect: prepared.side_effect,
        },
        prepared.prepared_at_ms,
    )
    .await
}

pub(super) fn source_row(
    prepared: &crate::PreparedAttempt,
    receipt: Option<String>,
    outcome: Option<String>,
    terminal: Option<i64>,
) -> Result<Row, Error> {
    Ok(Row {
        columns: vec![
            (
                "dispatch_identity".into(),
                prepared.dispatch_identity().into(),
            ),
            ("run_id".into(), prepared.run_id.clone().into()),
            (
                "prepared_json".into(),
                serde_json::to_string(prepared)
                    .map_err(|_| Error::InvalidAdmission)?
                    .into(),
            ),
            ("node_id".into(), prepared.node_id.clone().into()),
            (
                "activation_id".into(),
                prepared.activation_id.clone().into(),
            ),
            (
                "attempt_number".into(),
                DatabaseValue::Int64(i64::from(prepared.attempt)),
            ),
            (
                "receipt_json".into(),
                receipt.map_or(DatabaseValue::Null, DatabaseValue::String),
            ),
            (
                "outcome_json".into(),
                outcome.map_or(DatabaseValue::Null, DatabaseValue::String),
            ),
            (
                "terminal_at_ms".into(),
                terminal.map_or(DatabaseValue::Null, DatabaseValue::Int64),
            ),
        ],
    })
}

async fn coverage(
    db: &dyn Database,
    run: &str,
) -> Result<bcode_workflow::WorkflowSemanticHistoryCoverage, Error> {
    checkpoints::coverage(db, run).await
}

/// Read bounded, pinned `AttemptsV1` events. Other semantic sources are not certified.
/// # Errors
/// Rejects invalid cursors, unknown installations and inconsistent retained facts.
pub async fn page(
    db: &dyn Database,
    run: &str,
    cursor: Option<&bcode_workflow::WorkflowSemanticHistoryCursor>,
    limit: u16,
) -> Result<bcode_workflow::WorkflowSemanticHistoryPage, Error> {
    use bcode_workflow::{
        WorkflowSemanticHistoryCursor, WorkflowSemanticHistoryPage, WorkflowSemanticHistoryScope,
    };
    crate::validate_id("run_id", run).map_err(|_| Error::InvalidAdmission)?;
    if !(1..=100).contains(&limit) {
        return Err(Error::InvalidAdmission);
    }
    let tx = db.begin_transaction().await?;
    installation::verify(&*tx).await?;
    let boundary = if let Some(cursor) = cursor {
        if cursor.version != bcode_workflow::WORKFLOW_SEMANTIC_HISTORY_VERSION
            || cursor.run_id != run
            || cursor.scope != WorkflowSemanticHistoryScope::AttemptsV1
            || cursor.after_sequence > cursor.through_sequence
        {
            return Err(Error::InvalidAdmission);
        }
        if cursor.coverage != coverage(&*tx, run).await? {
            return Err(Error::MaintenanceRequired);
        }
        cursor.clone()
    } else {
        WorkflowSemanticHistoryCursor {
            version: bcode_workflow::WORKFLOW_SEMANTIC_HISTORY_VERSION,
            run_id: run.into(),
            scope: WorkflowSemanticHistoryScope::AttemptsV1,
            coverage: coverage(&*tx, run).await?,
            through_sequence: u64::try_from(high_water(&*tx, EVENTS, Some(run)).await?)
                .map_err(|_| Error::MaintenanceRequired)?,
            after_sequence: 0,
        }
    };
    let rows = tx
        .select(EVENTS)
        .where_eq("run_id", run)
        .where_gt(
            "sequence",
            i64::try_from(boundary.after_sequence).map_err(|_| Error::InvalidAdmission)?,
        )
        .where_lte(
            "sequence",
            i64::try_from(boundary.through_sequence).map_err(|_| Error::InvalidAdmission)?,
        )
        .sort("sequence", SortDirection::Asc)
        .limit(usize::from(limit) + 1)
        .execute(&*tx)
        .await?;
    let has_more = rows.len() > usize::from(limit);
    let events = rows
        .iter()
        .take(usize::from(limit))
        .map(|row| {
            serde_json::from_str::<WorkflowSemanticHistoryEvent>(&text(row, "event_json")?)
                .map_err(|_| Error::MaintenanceRequired)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let next_cursor = if has_more {
        let mut next = boundary.clone();
        next.after_sequence = events.last().ok_or(Error::MaintenanceRequired)?.sequence;
        Some(next)
    } else {
        None
    };
    let page = WorkflowSemanticHistoryPage {
        cursor: boundary,
        events,
        next_cursor,
    };
    page.validate(run, cursor)
        .map_err(|_| Error::MaintenanceRequired)?;
    tx.commit().await?;
    Ok(page)
}
