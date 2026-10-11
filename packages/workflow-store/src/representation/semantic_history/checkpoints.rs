//! Append-only run checkpoints. Coverage is an induction over owned transitions,
//! never a history-wide count or acknowledgement anti-join.
use super::{
    ACKS, Database, DatabaseValue, EVENTS, Error, OBLIGATIONS, ORIGINS, Row, high_water, integer,
    text,
};
use bcode_workflow::WorkflowSemanticHistoryCoverage as Coverage;
use switchy_database::{
    DatabaseTransaction,
    query::{FilterableQuery, SortDirection},
};
pub(super) const TABLE: &str = "workflow_attempt_history_checkpoints_v2";

/// Concrete durable tables for one implemented semantic source.
#[derive(Clone, Copy)]
pub(super) struct Tables {
    pub checkpoints: &'static str,
    pub obligations: &'static str,
    pub origins: &'static str,
    pub events: &'static str,
    pub acknowledgements: &'static str,
    pub initial_checkpoint: bool,
}
pub(super) const ATTEMPTS: Tables = Tables {
    checkpoints: TABLE,
    obligations: OBLIGATIONS,
    origins: ORIGINS,
    events: EVENTS,
    acknowledgements: ACKS,
    initial_checkpoint: true,
};

pub(super) async fn verify_index(
    db: &dyn Database,
    table: &str,
    name: &str,
    columns: &[&str],
    unique: bool,
) -> Result<(), Error> {
    let info = db
        .get_table_info(table)
        .await?
        .ok_or(Error::MaintenanceRequired)?;
    let index = info.indexes.get(name).ok_or(Error::MaintenanceRequired)?;
    if index.unique != unique
        || index
            .columns
            .iter()
            .map(String::as_str)
            .ne(columns.iter().copied())
    {
        return Err(Error::MaintenanceRequired);
    }
    Ok(())
}

pub(super) struct Baseline {
    pub run: String,
    verified: i64,
    gap: bool,
    sequence: i64,
}

pub(super) async fn obligation_water_for(
    db: &dyn Database,
    run: &str,
    tables: Tables,
) -> Result<i64, Error> {
    let mut water = 0;
    for field in ["old_run_id", "new_run_id"] {
        let rows = db
            .select(tables.obligations)
            .columns(&["sequence"])
            .where_eq(field, run)
            .sort("sequence", SortDirection::Desc)
            .limit(1)
            .execute(db)
            .await?;
        if let Some(row) = rows.first() {
            water = water.max(integer(row, "sequence")?);
        }
    }
    Ok(water)
}
async fn latest(db: &dyn Database, run: &str, tables: Tables) -> Result<Option<Row>, Error> {
    Ok(db
        .select(tables.checkpoints)
        .where_eq("run_id", run)
        .sort("sequence", SortDirection::Desc)
        .limit(1)
        .execute(db)
        .await?
        .into_iter()
        .next())
}
pub(super) async fn baseline(db: &dyn Database, run: &str) -> Result<Baseline, Error> {
    baseline_for(db, run, ATTEMPTS).await
}
pub(super) async fn baseline_for(
    db: &dyn Database,
    run: &str,
    tables: Tables,
) -> Result<Baseline, Error> {
    let water = obligation_water_for(db, run, tables).await?;
    let event_water = high_water(db, tables.events, Some(run)).await?;
    let origin = !db
        .select(tables.origins)
        .where_eq("run_id", run)
        .limit(1)
        .execute(db)
        .await?
        .is_empty();
    let Some(row) = latest(db, run, tables).await? else {
        if (origin && tables.initial_checkpoint) || event_water != 0 {
            return Err(Error::MaintenanceRequired);
        }
        return Ok(Baseline {
            run: run.into(),
            verified: 0,
            gap: true,
            sequence: 0,
        });
    };
    let sequence = integer(&row, "sequence")?;
    let verified = integer(&row, "last_verified")?;
    let stored_gap = integer(&row, "gap")?;
    if sequence < 0
        || sequence != event_water
        || verified < 0
        || verified > water
        || !matches!(stored_gap, 0 | 1)
    {
        return Err(Error::MaintenanceRequired);
    }
    if sequence == 0 {
        if !origin
            || verified != 0
            || stored_gap != 0
            || !text(&row, "event_identity")?.is_empty()
            || integer(&row, "obligation")? != 0
        {
            return Err(Error::MaintenanceRequired);
        }
    } else {
        let identity = text(&row, "event_identity")?;
        let event = db
            .select(tables.events)
            .where_eq("identity", identity.as_str())
            .limit(1)
            .execute(db)
            .await?;
        let ack = db
            .select(tables.acknowledgements)
            .where_eq("obligation", integer(&row, "obligation")?)
            .limit(1)
            .execute(db)
            .await?;
        if event.first().is_none_or(|event| {
            event.get("run_id") != Some(DatabaseValue::String(run.into()))
                || event.get("sequence") != Some(DatabaseValue::Int64(sequence))
        }) || ack
            .first()
            .is_none_or(|ack| ack.get("event_identity") != Some(DatabaseValue::String(identity)))
        {
            return Err(Error::MaintenanceRequired);
        }
    }
    Ok(Baseline {
        run: run.into(),
        verified,
        gap: stored_gap == 1 || !origin || water != verified,
        sequence,
    })
}
pub(super) fn admission(run: &str) -> Baseline {
    Baseline {
        run: run.into(),
        verified: 0,
        gap: false,
        sequence: 0,
    }
}
pub(super) const fn coverage_of(baseline: &Baseline) -> Coverage {
    if baseline.gap {
        Coverage::HistoricalGap
    } else {
        Coverage::CompleteFromAdmission
    }
}
pub(super) async fn coverage(db: &dyn Database, run: &str) -> Result<Coverage, Error> {
    Ok(if baseline(db, run).await?.gap {
        Coverage::HistoricalGap
    } else {
        Coverage::CompleteFromAdmission
    })
}
pub(super) async fn advance(
    tx: &dyn DatabaseTransaction,
    before: Baseline,
    obligation: i64,
    sequence: i64,
    identity: &str,
) -> Result<(), Error> {
    advance_for(tx, before, obligation, sequence, identity, ATTEMPTS).await
}
pub(super) async fn advance_for(
    tx: &dyn DatabaseTransaction,
    before: Baseline,
    obligation: i64,
    sequence: i64,
    identity: &str,
    tables: Tables,
) -> Result<(), Error> {
    if sequence
        != before
            .sequence
            .checked_add(1)
            .ok_or(Error::MaintenanceRequired)?
        || obligation_water_for(tx, &before.run, tables).await? != obligation
    {
        return Err(Error::MaintenanceRequired);
    }
    tx.insert(tables.checkpoints)
        .value("run_id", before.run)
        .value("sequence", sequence)
        .value(
            "last_verified",
            if before.gap {
                before.verified
            } else {
                obligation
            },
        )
        .value("gap", i64::from(before.gap))
        .value("obligation", obligation)
        .value("event_identity", identity)
        .execute(tx)
        .await?;
    Ok(())
}
