//! Exact run-lifecycle source obligations, independent of attempt accounting.
use super::super::super::{Database, DatabaseValue, RUNS, RepresentationError as Error};
use switchy_database::{
    DatabaseTransaction, Row,
    query::{FilterableQuery, SortDirection},
};
mod installation;
pub(super) async fn install_in(db: &dyn Database) -> Result<(), Error> {
    installation::install_in(db).await
}
pub(super) async fn verify(db: &dyn Database) -> Result<(), Error> {
    installation::verify(db).await
}
pub(super) const CONTRACT: &str = "workflow_lifecycle_history_contract_v1";
pub(super) const OBLIGATIONS: &str = "workflow_lifecycle_history_obligations_v1";
pub(super) const ORIGINS: &str = "workflow_lifecycle_history_origins_v1";
const FIELDS: &[&str] = &["run_id", "status", "updated_at_ms"];

/// Captured before the source mutation in the caller's owned Immediate transaction.
pub(super) struct Transition {
    boundary: i64,
    before: Option<Row>,
    run: String,
}

pub(super) async fn water(db: &dyn Database, run: Option<&str>) -> Result<i64, Error> {
    let mut highest = 0;
    for field in ["old_run_id", "new_run_id"] {
        let mut query = db
            .select(OBLIGATIONS)
            .columns(&["sequence"])
            .sort("sequence", SortDirection::Desc)
            .limit(1);
        if let Some(run) = run {
            query = query.where_eq(field, run);
        }
        let rows = query.execute(db).await?;
        if let Some(row) = rows.first() {
            highest = highest.max(integer(row, "sequence")?);
        }
        if run.is_none() {
            break;
        }
    }
    Ok(highest)
}

pub(super) async fn begin(tx: &dyn DatabaseTransaction, run: &str) -> Result<Transition, Error> {
    verify(tx).await?;
    let before = tx
        .select(RUNS)
        .columns(FIELDS)
        .where_eq("run_id", run)
        .limit(1)
        .execute(tx)
        .await?
        .into_iter()
        .next();
    Ok(Transition {
        boundary: water(tx, None).await?,
        before,
        run: run.into(),
    })
}

/// Proof returned only for one exact observed transition; not an acknowledgement.
pub(super) struct Proof {
    pub obligation: i64,
    pub run: String,
    pub previous: Option<super::super::super::runs::RunState>,
    pub status: super::super::super::runs::RunState,
    pub recorded_at_ms: u64,
}

/// Check exact OLD/NEW facts before constructing any normalized public event.
/// Identity changes, deletion and malformed states remain unacknowledged gaps.
pub(super) async fn prove(
    tx: &dyn DatabaseTransaction,
    transition: Transition,
    status: &str,
    recorded_at_ms: u64,
) -> Result<Option<Proof>, Error> {
    let obligations = tx
        .select(OBLIGATIONS)
        .where_gt("sequence", transition.boundary)
        .limit(2)
        .execute(tx)
        .await?;
    let old_status = transition
        .before
        .as_ref()
        .map(|row| text(row, "status"))
        .transpose()?;
    if old_status.as_deref() == Some(status) {
        return if obligations.is_empty() {
            Ok(None)
        } else {
            Err(Error::MaintenanceRequired)
        };
    }
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
    let expected = Row {
        columns: vec![
            (
                "run_id".into(),
                DatabaseValue::String(transition.run.clone()),
            ),
            ("status".into(), DatabaseValue::String(status.into())),
            (
                "updated_at_ms".into(),
                DatabaseValue::Int64(
                    i64::try_from(recorded_at_ms).map_err(|_| Error::InvalidAdmission)?,
                ),
            ),
        ],
    };
    for field in FIELDS {
        for (prefix, source) in [
            ("old", transition.before.as_ref()),
            ("new", Some(&expected)),
        ] {
            let value = source
                .and_then(|row| row.get(field))
                .unwrap_or(DatabaseValue::Null);
            if obligation.get(&format!("{prefix}_{field}")) != Some(value)
                || !matches!(
                    integer(obligation, &format!("{prefix}_{field}_status"))?,
                    0 | 1
                )
            {
                return Err(Error::MaintenanceRequired);
            }
        }
    }
    if transition
        .before
        .as_ref()
        .is_some_and(|row| row.get("run_id") != Some(DatabaseValue::String(transition.run.clone())))
    {
        return Err(Error::MaintenanceRequired);
    }
    let previous = old_status
        .as_deref()
        .map(super::super::super::runs::RunState::decode)
        .transpose()?;
    let status = super::super::super::runs::RunState::decode(status)?;
    Ok(Some(Proof {
        obligation: integer(obligation, "sequence")?,
        run: transition.run,
        previous,
        status,
        recorded_at_ms,
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
