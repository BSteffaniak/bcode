//! Independent run admission/status history; attempts v1 remains unchanged.
use super::{Database, Error, checkpoints, high_water, text};
use bcode_workflow::{
    WORKFLOW_LIFECYCLE_HISTORY_VERSION, WorkflowLifecycleHistoryCursor,
    WorkflowLifecycleHistoryEvent, WorkflowLifecycleHistoryPage, WorkflowLifecycleHistoryPayload,
    WorkflowLifecycleHistoryScope,
};
use sha2::{Digest, Sha256};
use switchy_database::{
    DatabaseTransaction,
    query::{FilterableQuery, SortDirection},
};
mod installation;
mod source;
#[cfg(test)]
mod tests;
const EVENTS: &str = "workflow_lifecycle_history_events_v1";
const ACKS: &str = "workflow_lifecycle_history_acks_v1";
const CHECKPOINTS: &str = "workflow_lifecycle_history_checkpoints_v1";
const TABLES: checkpoints::Tables = checkpoints::Tables {
    events: EVENTS,
    acknowledgements: ACKS,
    checkpoints: CHECKPOINTS,
    obligations: source::OBLIGATIONS,
    origins: source::ORIGINS,
    initial_checkpoint: false,
};
pub(in crate::representation) async fn install_in(db: &dyn Database) -> Result<(), Error> {
    installation::install_in(db).await
}
pub(in crate::representation) struct Transition {
    source: source::Transition,
    baseline: checkpoints::Baseline,
}
pub(in crate::representation) async fn begin(
    tx: &dyn DatabaseTransaction,
    run: &str,
) -> Result<Transition, Error> {
    installation::verify(tx).await?;
    let baseline = checkpoints::baseline_for(tx, run, TABLES).await?;
    let fresh = source::water(tx, Some(run)).await? == 0
        && tx
            .select(super::super::RUNS)
            .columns(&["run_id"])
            .where_eq("run_id", run)
            .limit(1)
            .execute(tx)
            .await?
            .is_empty();
    Ok(Transition {
        baseline: if fresh {
            checkpoints::admission(run)
        } else {
            baseline
        },
        source: source::begin(tx, run).await?,
    })
}
pub(in crate::representation) async fn finish(
    tx: &dyn DatabaseTransaction,
    transition: Transition,
    status: &str,
    time: u64,
) -> Result<(), Error> {
    let Some(proof) = source::prove(tx, transition.source, status, time).await? else {
        return Ok(());
    };
    let baseline = transition.baseline;
    let normalize = |value| match value {
        super::super::runs::RunState::Paused => bcode_workflow::RunStatus::Paused,
        super::super::runs::RunState::Completed => bcode_workflow::RunStatus::Completed,
        super::super::runs::RunState::Failed => bcode_workflow::RunStatus::Failed,
        super::super::runs::RunState::Cancelled => bcode_workflow::RunStatus::Cancelled,
    };
    let payload = proof.previous.map_or_else(
        || WorkflowLifecycleHistoryPayload::RunAdmitted {
            status: normalize(proof.status),
        },
        |previous| WorkflowLifecycleHistoryPayload::RunStatusChanged {
            previous: normalize(previous),
            status: normalize(proof.status),
        },
    );
    let sequence = high_water(tx, EVENTS, Some(&proof.run))
        .await?
        .checked_add(1)
        .ok_or(Error::MaintenanceRequired)?;
    let operation_id = hex::encode(Sha256::digest(
        serde_json::to_vec(&(&proof.run, proof.obligation, &payload))
            .map_err(|_| Error::InvalidAdmission)?,
    ));
    let event = WorkflowLifecycleHistoryEvent {
        run_id: proof.run,
        sequence: u64::try_from(sequence).map_err(|_| Error::MaintenanceRequired)?,
        operation_id,
        recorded_at_ms: proof.recorded_at_ms,
        payload,
    };
    super::super::journal::public_events::append(
        tx,
        &event.run_id,
        &event.operation_id,
        event.recorded_at_ms,
        super::super::journal::public_events::Fact::Lifecycle {
            payload: event.payload.clone(),
        },
    )
    .await?;
    tx.insert(EVENTS)
        .value("identity", event.operation_id.as_str())
        .value("run_id", event.run_id.as_str())
        .value("sequence", sequence)
        .value(
            "event_json",
            serde_json::to_string(&event).map_err(|_| Error::InvalidAdmission)?,
        )
        .execute(tx)
        .await?;
    tx.insert(ACKS)
        .value("obligation", proof.obligation)
        .value("event_identity", event.operation_id.as_str())
        .execute(tx)
        .await?;
    checkpoints::advance_for(
        tx,
        baseline,
        proof.obligation,
        sequence,
        &event.operation_id,
        TABLES,
    )
    .await
}
/// Read bounded, pinned run admission/status facts, never full workflow history.
/// # Errors
/// Rejects invalid cursors, unavailable installation or inconsistent proof state.
pub(in crate::representation) async fn coverage(
    db: &dyn Database,
    run: &str,
) -> Result<bcode_workflow::WorkflowSemanticHistoryCoverage, Error> {
    installation::verify(db).await?;
    let baseline = checkpoints::baseline_for(db, run, TABLES).await?;
    Ok(checkpoints::coverage_of(&baseline))
}

/// Read bounded, pinned run admission/status facts, never full workflow history.
///
/// # Errors
/// Rejects invalid cursors, unavailable installation or inconsistent proof state.
pub async fn page(
    db: &dyn Database,
    run: &str,
    cursor: Option<&WorkflowLifecycleHistoryCursor>,
    limit: u16,
) -> Result<WorkflowLifecycleHistoryPage, Error> {
    crate::validate_id("run_id", run).map_err(|_| Error::InvalidAdmission)?;
    if !(1..=100).contains(&limit) {
        return Err(Error::InvalidAdmission);
    }
    let tx = db.begin_transaction().await?;
    installation::verify(&*tx).await?;
    let baseline = checkpoints::baseline_for(&*tx, run, TABLES).await?;
    let coverage = checkpoints::coverage_of(&baseline);
    let boundary = if let Some(cursor) = cursor {
        if cursor.version != WORKFLOW_LIFECYCLE_HISTORY_VERSION
            || cursor.run_id != run
            || cursor.scope != WorkflowLifecycleHistoryScope::RunLifecycleV1
            || cursor.after_sequence > cursor.through_sequence
        {
            return Err(Error::InvalidAdmission);
        }
        if cursor.coverage != coverage {
            return Err(Error::MaintenanceRequired);
        }
        cursor.clone()
    } else {
        WorkflowLifecycleHistoryCursor {
            version: WORKFLOW_LIFECYCLE_HISTORY_VERSION,
            run_id: run.into(),
            scope: WorkflowLifecycleHistoryScope::RunLifecycleV1,
            coverage,
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
    let events = rows
        .iter()
        .take(usize::from(limit))
        .map(|row| {
            serde_json::from_str::<WorkflowLifecycleHistoryEvent>(&text(row, "event_json")?)
                .map_err(|_| Error::MaintenanceRequired)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let next_cursor = if rows.len() > usize::from(limit) {
        let mut next = boundary.clone();
        next.after_sequence = events.last().ok_or(Error::MaintenanceRequired)?.sequence;
        Some(next)
    } else {
        None
    };
    let page = WorkflowLifecycleHistoryPage {
        cursor: boundary,
        events,
        next_cursor,
    };
    page.validate(run, cursor)
        .map_err(|_| Error::MaintenanceRequired)?;
    tx.commit().await?;
    Ok(page)
}
