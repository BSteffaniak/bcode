//! Exact persisted activation metadata for bounded public observation.
use super::{Database, DatabaseValue, RepresentationError as Error};
use switchy_database::query::FilterableQuery;
/// Read one activation's canonical metadata without disclosing its input.
/// # Errors
/// Rejects absent, malformed or inconsistent durable activation rows.
pub async fn metadata(db: &dyn Database, run: &str, id: &str) -> Result<(u64, u64, usize), Error> {
    let rows = db
        .select("workflow_activations_v2")
        .columns(&["generation", "created_at_ms", "input_json"])
        .where_eq("run_id", run)
        .where_eq("identity", id)
        .limit(1)
        .execute(db)
        .await?;
    let row = rows.first().ok_or(Error::MaintenanceRequired)?;
    let integer = |field| match row.get(field) {
        Some(DatabaseValue::Int64(value)) => {
            u64::try_from(value).map_err(|_| Error::MaintenanceRequired)
        }
        _ => Err(Error::MaintenanceRequired),
    };
    let size = match row.get("input_json") {
        Some(DatabaseValue::String(value)) => value.len(),
        _ => return Err(Error::MaintenanceRequired),
    };
    Ok((integer("generation")?, integer("created_at_ms")?, size))
}
