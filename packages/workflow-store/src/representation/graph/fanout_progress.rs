//! Incremental wait-all fan-out reduction over exact ordered virtual members.
use super::{Database, DatabaseValue, Error, FilterableQuery, encode};
use switchy_database::schema::{DataType, create_table};
const JOBS: &str = "workflow_fanout_reduction_v2";
pub(super) async fn install(db: &dyn Database) -> Result<(), Error> {
    let mut table = create_table(JOBS)
        .column(super::super::column("identity", DataType::Text, false))
        .primary_key("identity");
    for field in ["epoch", "next_member", "state_bytes"] {
        table = table.column(super::super::column(field, DataType::Int, false));
    }
    table
        .column(super::super::column("complete", DataType::Bool, false))
        .execute(db)
        .await?;
    Ok(())
}
/// Whether a wait-all reduction has resumable local work rather than only external waiting.
/// # Errors
/// Rejects malformed or unsupported durable state and storage failures.
pub async fn pending(db: &dyn Database, run: &str, controller: &str) -> Result<bool, Error> {
    let tx = db.begin_transaction().await?;
    let version = super::parallel_progress::scan_version_in(&*tx, run).await?;
    let key = encode(&(run, controller, version.revision))?;
    let rows = tx
        .select(JOBS)
        .where_eq("identity", key)
        .limit(1)
        .execute(&*tx)
        .await?;
    let result = rows.first().is_some_and(|r| {
        r.get("epoch")
            != Some(DatabaseValue::Int64(
                i64::try_from(version.epoch).unwrap_or(i64::MAX),
            ))
            || r.get("complete") == Some(DatabaseValue::Int64(0))
            || r.get("complete") == Some(DatabaseValue::Bool(false))
    });
    tx.commit().await?;
    Ok(result)
}
fn integer(row: &switchy_database::Row, key: &str) -> Result<usize, Error> {
    match row.get(key) {
        Some(DatabaseValue::Int64(v)) => usize::try_from(v).map_err(|_| Error::MaintenanceRequired),
        _ => Err(Error::MaintenanceRequired),
    }
}
pub(super) async fn reduce(
    db: &dyn switchy_database::DatabaseTransaction,
    run: &str,
    id: &str,
    count: usize,
    policy: super::parallel_progress::OutputPolicy,
) -> Result<Option<super::parallel::MemberState>, Error> {
    let version = super::parallel_progress::scan_version_in(db, run).await?;
    let epoch = i64::try_from(version.epoch).map_err(|_| Error::MaintenanceRequired)?;
    let key = encode(&(run, id, version.revision))?;
    let rows = db
        .select(JOBS)
        .where_eq("identity", key.as_str())
        .limit(1)
        .execute(db)
        .await?;
    let (mut next, mut bytes) = if let Some(row) = rows
        .first()
        .filter(|r| r.get("epoch") == Some(DatabaseValue::Int64(epoch)))
    {
        (integer(row, "next_member")?, integer(row, "state_bytes")?)
    } else {
        (0, 0)
    };
    if next > count || bytes > policy.max_inline_bytes {
        return Err(Error::InvalidAdmission);
    }
    let start = next;
    let mut chunk = super::parallel::MemberState::default();
    if next < count {
        let after = next
            .checked_sub(1)
            .map(u32::try_from)
            .transpose()
            .map_err(|_| Error::InvalidAdmission)?;
        let names = super::fanout::members(db, run, id, after, 32).await?;
        if names.is_empty() {
            return Err(Error::MaintenanceRequired);
        }
        for name in names {
            let Some(mut part) = super::parallel::members_state(db, run, &[name.as_str()]).await?
            else {
                return Err(Error::MaintenanceRequired);
            };
            if let Some(value) = part.outcomes.remove(&name) {
                chunk.outcomes.insert(format!("{next:010}"), value);
            }
            chunk.failed |= part.failed;
            chunk.pending.extend(part.pending);
            chunk.records.extend(part.records);
            next += 1;
        }
        bytes = bytes
            .checked_add(super::parallel_chunks::put(db, &key, epoch, start, &chunk).await?)
            .ok_or(Error::InvalidAdmission)?;
    }
    if bytes > policy.max_inline_bytes {
        return Err(Error::InvalidAdmission);
    }
    db.delete(JOBS)
        .where_eq("identity", key.as_str())
        .execute(db)
        .await?;
    db.insert(JOBS)
        .value("identity", key.as_str())
        .value("epoch", epoch)
        .value(
            "next_member",
            i64::try_from(next).map_err(|_| Error::InvalidAdmission)?,
        )
        .value(
            "state_bytes",
            i64::try_from(bytes).map_err(|_| Error::InvalidAdmission)?,
        )
        .value("complete", next == count)
        .execute(db)
        .await?;
    if next < count {
        return Ok(None);
    }
    Ok(Some(
        super::parallel_chunks::collect(db, &key, epoch, bytes, policy.max_inline_bytes).await?,
    ))
}
