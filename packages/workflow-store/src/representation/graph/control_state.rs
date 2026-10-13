//! Canonical repeat counters and control failure facts.
use super::{Database, DatabaseValue, Error, FilterableQuery};
use switchy_database::schema::{DataType, create_table};
/// Immutable observed repeat emission outcomes.
pub mod repeat_outcome;
const CYCLES: &str = "workflow_repeat_cycles_v2";
const FAILURES: &str = "workflow_control_failures_v2";
pub(super) async fn install(db: &dyn Database) -> Result<(), Error> {
    repeat_outcome::install(db).await?;
    for table in [CYCLES, FAILURES] {
        create_table(table)
            .column(super::super::column("identity", DataType::Text, false))
            .primary_key("identity")
            .column(super::super::column("run_id", DataType::Text, false))
            .column(super::super::column("value", DataType::Int, false))
            .execute(db)
            .await?;
    }
    Ok(())
}
fn key(run: &str, node: &str) -> Result<String, Error> {
    super::encode(&(run, node))
}
pub(super) async fn completed(db: &dyn Database, run: &str, node: &str) -> Result<u64, Error> {
    let rows = db
        .select(CYCLES)
        .where_eq("identity", key(run, node)?)
        .limit(1)
        .execute(db)
        .await?;
    match rows.first().and_then(|r| r.get("value")) {
        None => Ok(0),
        Some(DatabaseValue::Int64(n)) if n >= 0 => {
            u64::try_from(n).map_err(|_| Error::MaintenanceRequired)
        }
        _ => Err(Error::MaintenanceRequired),
    }
}
pub(super) async fn record(
    db: &dyn Database,
    run: &str,
    node: &str,
    value: u64,
) -> Result<(), Error> {
    let id = key(run, node)?;
    let count = i64::try_from(value).map_err(|_| Error::InvalidAdmission)?;
    if db
        .select(CYCLES)
        .where_eq("identity", id.as_str())
        .limit(1)
        .execute(db)
        .await?
        .is_empty()
    {
        db.insert(CYCLES)
            .value("identity", id)
            .value("run_id", run)
            .value("value", count)
            .execute(db)
            .await?;
    } else {
        db.update(CYCLES)
            .value("value", count)
            .where_eq("identity", id)
            .execute(db)
            .await?;
    }
    Ok(())
}
pub(super) async fn fail(db: &dyn Database, run: &str, activation: &str) -> Result<(), Error> {
    if db
        .select(FAILURES)
        .where_eq("identity", activation)
        .limit(1)
        .execute(db)
        .await?
        .is_empty()
    {
        db.insert(FAILURES)
            .value("identity", activation)
            .value("run_id", run)
            .value("value", 1_i64)
            .execute(db)
            .await?;
    }
    Ok(())
}
