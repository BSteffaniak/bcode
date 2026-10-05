//! Append-only epoch-qualified reduction chunks; stale epochs are never reused.
use super::{Database, DatabaseValue, Error, FilterableQuery, encode};
use switchy_database::{
    query::SortDirection,
    schema::{DataType, create_index, create_table},
};
const TABLE: &str = "workflow_parallel_chunks_v2";
pub(super) async fn install(db: &dyn Database) -> Result<(), Error> {
    create_table(TABLE)
        .column(super::super::column("identity", DataType::Text, false))
        .primary_key("identity")
        .column(super::super::column("job", DataType::Text, false))
        .column(super::super::column("epoch", DataType::Int, false))
        .column(super::super::column("ordinal", DataType::Int, false))
        .column(super::super::column("state_json", DataType::Text, false))
        .execute(db)
        .await?;
    create_index("idx_parallel_chunks_job_epoch")
        .table(TABLE)
        .columns(vec!["job", "epoch", "ordinal"])
        .execute(db)
        .await?;
    Ok(())
}
pub(super) async fn put(
    db: &dyn Database,
    job: &str,
    epoch: i64,
    ordinal: usize,
    state: &super::parallel::MemberState,
) -> Result<usize, Error> {
    let json = encode(state)?;
    let size = json.len();
    let identity = encode(&(job, epoch, ordinal))?;
    // A repeated waiting pass replaces only its own bounded chunk, never an entire accumulator.
    db.delete(TABLE)
        .where_eq("identity", identity.as_str())
        .execute(db)
        .await?;
    db.insert(TABLE)
        .value("identity", identity)
        .value("job", job)
        .value("epoch", epoch)
        .value(
            "ordinal",
            i64::try_from(ordinal).map_err(|_| Error::InvalidAdmission)?,
        )
        .value("state_json", json)
        .execute(db)
        .await?;
    Ok(size)
}
/// Final inline materialization is bounded by the explicit output byte budget, not graph size.
pub(super) async fn collect(
    db: &dyn Database,
    job: &str,
    epoch: i64,
    bytes: usize,
    budget: usize,
) -> Result<super::parallel::MemberState, Error> {
    if bytes > budget {
        return Err(Error::InvalidAdmission);
    }
    let mut state = super::parallel::MemberState::default();
    let mut after = None;
    let mut actual = 0_usize;
    loop {
        let mut query = db
            .select(TABLE)
            .columns(&["ordinal", "state_json"])
            .where_eq("job", job)
            .where_eq("epoch", epoch)
            .sort("ordinal", SortDirection::Asc)
            .limit(32);
        if let Some(after) = after {
            query = query.where_gt("ordinal", after);
        }
        let rows = query.execute(db).await?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            let Some(DatabaseValue::Int64(ordinal)) = row.get("ordinal") else {
                return Err(Error::MaintenanceRequired);
            };
            let Some(DatabaseValue::String(json)) = row.get("state_json") else {
                return Err(Error::MaintenanceRequired);
            };
            actual = actual
                .checked_add(json.len())
                .ok_or(Error::MaintenanceRequired)?;
            if actual > bytes {
                return Err(Error::MaintenanceRequired);
            }
            let part: super::parallel::MemberState =
                serde_json::from_str(&json).map_err(|_| Error::MaintenanceRequired)?;
            state.outcomes.extend(part.outcomes);
            state.pending.extend(part.pending);
            state.failed |= part.failed;
            state.records.extend(part.records);
            after = Some(ordinal);
        }
    }
    if actual != bytes {
        return Err(Error::MaintenanceRequired);
    }
    Ok(state)
}
