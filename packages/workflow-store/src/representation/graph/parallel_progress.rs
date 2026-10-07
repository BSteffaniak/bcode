//! Epoch-fenced incremental parallel member reduction.
use super::{Database, DatabaseValue, Error, FilterableQuery, encode};
use switchy_database::schema::{DataType, create_table};
const EPOCH: &str = "workflow_parallel_epoch_v2";
#[cfg(test)]
mod tests;
const JOBS: &str = "workflow_parallel_progress_v2";
/// Explicit inline outcome budget; limits result size, never graph lifetime/member count.
#[derive(Debug, Clone, Copy)]
pub struct OutputPolicy {
    /// Maximum inline serialized accumulator bytes.
    pub max_inline_bytes: usize,
}
impl Default for OutputPolicy {
    fn default() -> Self {
        Self {
            max_inline_bytes: super::super::MAX_ADMISSION_BYTES,
        }
    }
}
pub(super) async fn install(db: &dyn Database) -> Result<(), Error> {
    super::parallel_chunks::install(db).await?;
    create_table(EPOCH)
        .column(super::super::column("run_id", DataType::Text, false))
        .primary_key("run_id")
        .column(super::super::column("epoch", DataType::Int, false))
        .execute(db)
        .await?;
    create_table(JOBS)
        .column(super::super::column("identity", DataType::Text, false))
        .primary_key("identity")
        .column(super::super::column("epoch", DataType::Int, false))
        .column(super::super::column("next_member", DataType::Int, false))
        .column(super::super::column("state_bytes", DataType::Int, false))
        .column(super::super::column("complete", DataType::Bool, false))
        .execute(db)
        .await?;
    Ok(())
}
/// Invalidate partial reductions in the same transaction as new canonical attempts/outcomes.
/// # Errors
/// Rejects epoch overflow or malformed durable state.
pub async fn invalidate(db: &dyn Database, run: &str) -> Result<(), Error> {
    let rows = db
        .select(EPOCH)
        .where_eq("run_id", run)
        .limit(1)
        .execute(db)
        .await?;
    if rows.is_empty() {
        db.insert(EPOCH)
            .value("run_id", run)
            .value("epoch", 1_i64)
            .execute(db)
            .await?;
    } else {
        let next = epoch(db, run)
            .await?
            .checked_add(1)
            .ok_or(Error::MaintenanceRequired)?;
        db.update(EPOCH)
            .value("epoch", next)
            .where_eq("run_id", run)
            .execute(db)
            .await?;
    }
    Ok(())
}
/// Same-snapshot invalidation token for bounded controller scans; not execution authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanVersion {
    /// Canonical graph revision.
    pub revision: u64,
    /// Run-local outcome/invalidation epoch. Restart a finished scan when this changes.
    pub epoch: u64,
}
/// Observe graph revision and controller epoch atomically without modifying storage.
/// The token is a comparison key, not a durable replay cursor or permission to dispatch.
/// # Errors
/// Rejects missing runs, unsupported storage, malformed counters and database failures.
pub async fn scan_version(db: &dyn Database, run: &str) -> Result<ScanVersion, Error> {
    let tx = db.begin_transaction().await?;
    let version = scan_version_in(&*tx, run).await?;
    tx.commit().await?;
    Ok(version)
}
/// Observe the token in the caller's snapshot without opening a nested transaction.
/// # Errors
/// Same errors as `scan_version`.
pub async fn scan_version_in(
    db: &dyn switchy_database::DatabaseTransaction,
    run: &str,
) -> Result<ScanVersion, Error> {
    super::super::verified(db).await?;
    let revision = super::super::graph_pages::revision(db, run).await?;
    let epoch = u64::try_from(epoch(db, run).await?).map_err(|_| Error::MaintenanceRequired)?;
    Ok(ScanVersion { revision, epoch })
}

async fn epoch(db: &dyn Database, run: &str) -> Result<i64, Error> {
    let rows = db
        .select(EPOCH)
        .columns(&["epoch"])
        .where_eq("run_id", run)
        .limit(1)
        .execute(db)
        .await?;
    match rows.first().and_then(|row| row.get("epoch")) {
        None => Ok(0),
        Some(DatabaseValue::Int64(n)) if n >= 0 => Ok(n),
        _ => Err(Error::MaintenanceRequired),
    }
}
pub(super) async fn reduce(
    db: &dyn Database,
    run: &str,
    join: &str,
    revision: u64,
    members: &[&str],
    policy: OutputPolicy,
) -> Result<Option<super::parallel::MemberState>, Error> {
    let id = encode(&(run, join, revision))?;
    let epoch = epoch(db, run).await?;
    let rows = db
        .select(JOBS)
        .where_eq("identity", id.as_str())
        .limit(1)
        .execute(db)
        .await?;
    let (mut start, mut bytes) = if let Some(row) = rows
        .first()
        .filter(|row| row.get("epoch") == Some(DatabaseValue::Int64(epoch)))
    {
        let Some(DatabaseValue::Int64(next)) = row.get("next_member") else {
            return Err(Error::MaintenanceRequired);
        };
        let Some(DatabaseValue::Int64(bytes)) = row.get("state_bytes") else {
            return Err(Error::MaintenanceRequired);
        };
        (
            usize::try_from(next).map_err(|_| Error::MaintenanceRequired)?,
            usize::try_from(bytes).map_err(|_| Error::MaintenanceRequired)?,
        )
    } else {
        (0, 0)
    };
    if bytes > policy.max_inline_bytes {
        return Err(Error::InvalidAdmission);
    }
    let chunk_start = start;
    let mut state = super::parallel::MemberState::default();
    if start > members.len() {
        return Err(Error::MaintenanceRequired);
    }
    let end = start.saturating_add(32).min(members.len());
    for member in &members[start..end] {
        let Some(part) = super::parallel::members_state(db, run, &[*member]).await? else {
            break;
        };
        state.outcomes.extend(part.outcomes);
        state.pending.extend(part.pending);
        state.failed |= part.failed;
        state.records.extend(part.records);
        start += 1;
    }
    if start > chunk_start {
        bytes = bytes
            .checked_add(super::parallel_chunks::put(db, &id, epoch, chunk_start, &state).await?)
            .ok_or(Error::InvalidAdmission)?;
    }
    if bytes > policy.max_inline_bytes {
        return Err(Error::InvalidAdmission);
    }
    db.delete(JOBS)
        .where_eq("identity", id.as_str())
        .execute(db)
        .await?;
    db.insert(JOBS)
        .value("identity", id.as_str())
        .value("epoch", epoch)
        .value("complete", start == members.len())
        .value(
            "next_member",
            i64::try_from(start).map_err(|_| Error::InvalidAdmission)?,
        )
        .value(
            "state_bytes",
            i64::try_from(bytes).map_err(|_| Error::InvalidAdmission)?,
        )
        .execute(db)
        .await?;
    if start == members.len() {
        return Ok(Some(
            super::parallel_chunks::collect(db, &id, epoch, bytes, policy.max_inline_bytes).await?,
        ));
    }
    Ok(None)
}

/// Whether a controller has resumable member work rather than only external waiting.
///
/// A schema-valid output on the current activation ends reduction even if its job epoch is stale.
/// A same-epoch cursor blocked on a missing member activation is external waiting; a stale epoch
/// permits a fresh reduction before availability is reconsidered.
/// # Errors
/// Rejects stale revisions, invalid committed controller output and incompatible durable state,
/// and propagates storage failures. Caller must continue under ordinary execution authority.
pub async fn pending(
    db: &dyn Database,
    run: &str,
    join: &str,
    revision: u64,
) -> Result<bool, Error> {
    let tx = db.begin_transaction().await?;
    let current = scan_version_in(&*tx, run).await?;
    if current.revision != revision {
        return Err(Error::InvalidAdmission);
    }
    if let Some(value) = super::output(&*tx, run, join).await? {
        let node = super::super::graph_pages::node(&*tx, run, revision, join)
            .await?
            .ok_or(Error::MaintenanceRequired)?;
        if node.kind != bcode_workflow::NodeKind::Parallel {
            return Err(Error::InvalidAdmission);
        }
        super::schema(&node.output.schema, &value)?;
        tx.commit().await?;
        return Ok(false);
    }
    let completed = encode(&(run, join, revision))?;
    let rows = tx
        .select(JOBS)
        .where_eq("identity", completed)
        .limit(1)
        .execute(&*tx)
        .await?;
    let current_epoch = i64::try_from(current.epoch).map_err(|_| Error::MaintenanceRequired)?;
    let result = if let Some(row) = rows.first() {
        reduction_pending(&*tx, run, join, revision, current_epoch, row).await?
    } else {
        false
    };
    tx.commit().await?;
    Ok(result)
}

async fn reduction_pending(
    db: &dyn Database,
    run: &str,
    join: &str,
    revision: u64,
    current_epoch: i64,
    row: &switchy_database::Row,
) -> Result<bool, Error> {
    let integer = |field| match row.get(field) {
        Some(DatabaseValue::Int64(value)) if value >= 0 => Ok(value),
        _ => Err(Error::MaintenanceRequired),
    };
    let job_epoch = integer("epoch")?;
    if job_epoch > current_epoch {
        return Err(Error::MaintenanceRequired);
    }
    let next = usize::try_from(integer("next_member")?).map_err(|_| Error::MaintenanceRequired)?;
    let _bytes = integer("state_bytes")?;
    let complete = match row.get("complete") {
        Some(DatabaseValue::Bool(value)) => value,
        Some(DatabaseValue::Int64(0)) => false,
        Some(DatabaseValue::Int64(1)) => true,
        _ => return Err(Error::MaintenanceRequired),
    };
    let node = super::super::graph_pages::node(db, run, revision, join)
        .await?
        .ok_or(Error::MaintenanceRequired)?;
    if node.kind != bcode_workflow::NodeKind::Parallel {
        return Err(Error::InvalidAdmission);
    }
    let members =
        bcode_workflow::parallel_join_member_ids(&node).map_err(|_| Error::MaintenanceRequired)?;
    if next > members.len() || complete != (next == members.len()) {
        return Err(Error::MaintenanceRequired);
    }
    if job_epoch != current_epoch {
        return Ok(true);
    }
    if complete {
        return Ok(false);
    }
    // A missing activation is external waiting, not another reduction quantum.
    // Use the reducer's exact availability contract, including skipped/terminal members.
    Ok(super::parallel::members_state(db, run, &[members[next]])
        .await?
        .is_some())
}
