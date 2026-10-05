//! Durable bounded terminal proof scan, invalidated by canonical graph mutations.
use super::{Database, DatabaseValue, Error, FilterableQuery, NodeKind, activation, output};
use switchy_database::schema::{DataType, create_table};
const TABLE: &str = "workflow_terminal_scan_v2";
#[cfg(test)]
mod tests;
pub(super) async fn install(db: &dyn Database) -> Result<(), Error> {
    create_table(TABLE)
        .column(super::super::column("run_id", DataType::Text, false))
        .primary_key("run_id")
        .column(super::super::column("revision", DataType::Int, false))
        .column(super::super::column("after_node", DataType::Text, true))
        .column(super::super::column("complete", DataType::Bool, false))
        .column(super::super::column("member_index", DataType::Int, false))
        .execute(db)
        .await?;
    Ok(())
}
/// Invalidate terminal proof in the same write transaction as a canonical graph mutation.
/// # Errors
/// Returns persistence failures; callers must roll back the mutation as well.
pub async fn invalidate(db: &dyn Database, run: &str) -> Result<(), Error> {
    super::parallel_progress::invalidate(db, run).await?;
    clear(db, run).await?;
    Ok(())
}
/// Whether a terminal proof scan has locally advanceable work, not an external receipt wait.
///
/// This single-statement observation preserves the invalidated scan while attempts remain
/// unresolved. It grants no execution authority; terminalization rechecks its own snapshot.
/// # Errors
/// Returns database failure; false is not proof that the run is terminal.
pub async fn pending(db: &dyn Database, run: &str) -> Result<bool, Error> {
    let idle = db
        .select(super::RUNS)
        .columns(&["run_id"])
        .where_eq("run_id", run)
        .where_eq("status", "paused")
        .where_eq("active_attempts", 0_i64);
    let unresolved = db
        .select("workflow_attempts_v2")
        .columns(&["run_id"])
        .where_eq("run_id", run)
        .where_eq("outcome_json", DatabaseValue::Null)
        .limit(1);
    Ok(!db
        .select(TABLE)
        .columns(&["run_id"])
        .where_eq("run_id", run)
        .where_in("run_id", idle)
        .where_not_in("run_id", unresolved)
        .where_eq("complete", false)
        .limit(1)
        .execute(db)
        .await?
        .is_empty())
}
/// Advance one bounded page of terminal graph proof under the caller's fenced transaction.
/// # Errors
/// Rejects malformed state; pending/blocked proof returns false without optimistic completion.
pub(in crate::representation) async fn step(
    db: &dyn Database,
    run: &str,
    limit: u16,
) -> Result<bool, Error> {
    if limit == 0 || limit > 100 {
        return Err(Error::InvalidAdmission);
    }
    let revision = super::super::graph_pages::revision(db, run).await?;
    let rows = db
        .select(TABLE)
        .where_eq("run_id", run)
        .limit(1)
        .execute(db)
        .await?;
    let after = if let Some(row) = rows.first() {
        if row.get("revision")
            != Some(DatabaseValue::Int64(
                i64::try_from(revision).map_err(|_| Error::MaintenanceRequired)?,
            ))
        {
            clear(db, run).await?;
            None
        } else if row.get("complete") == Some(DatabaseValue::Int64(1)) {
            return Ok(true);
        } else {
            match row.get("after_node") {
                Some(DatabaseValue::Null) => None,
                Some(DatabaseValue::String(id)) => Some(id),
                _ => return Err(Error::MaintenanceRequired),
            }
        }
    } else {
        None
    };
    let scan_matches = rows.first().is_some_and(|row| {
        row.get("revision") == i64::try_from(revision).ok().map(DatabaseValue::Int64)
    });
    let member = match rows
        .first()
        .filter(|_| scan_matches)
        .and_then(|row| row.get("member_index"))
    {
        None => 0,
        Some(DatabaseValue::Int64(n)) => {
            usize::try_from(n).map_err(|_| Error::MaintenanceRequired)?
        }
        _ => return Err(Error::MaintenanceRequired),
    };
    let Some((complete, cursor, member)) =
        scan_page(db, run, revision, after, member, limit).await?
    else {
        clear(db, run).await?;
        return Ok(false);
    };
    clear(db, run).await?;
    db.insert(TABLE)
        .value("run_id", run)
        .value(
            "revision",
            i64::try_from(revision).map_err(|_| Error::MaintenanceRequired)?,
        )
        .value(
            "after_node",
            cursor.map_or(DatabaseValue::Null, DatabaseValue::String),
        )
        .value("complete", complete)
        .value(
            "member_index",
            i64::try_from(member).map_err(|_| Error::MaintenanceRequired)?,
        )
        .execute(db)
        .await?;
    Ok(complete)
}
async fn scan_page(
    db: &dyn Database,
    run: &str,
    revision: u64,
    mut after: Option<String>,
    mut member: usize,
    limit: u16,
) -> Result<Option<(bool, Option<String>, usize)>, Error> {
    for _ in 0..limit {
        let nodes =
            super::super::graph_pages::nodes(db, run, revision, after.as_deref(), 1).await?;
        let Some(node) = nodes.first() else {
            return Ok(Some((true, after, 0)));
        };
        if node.kind == NodeKind::Parallel && output(db, run, &node.id).await?.is_none() {
            let members = bcode_workflow::parallel_join_member_ids(node)
                .map_err(|_| Error::MaintenanceRequired)?;
            if let Some(id) = members.get(member) {
                if activation(db, run, id).await?.is_some()
                    && db
                        .select("workflow_control_failures_v2")
                        .columns(&["identity"])
                        .where_eq("run_id", run)
                        .where_eq("identity", format!("parallel:{}", node.id))
                        .limit(1)
                        .execute(db)
                        .await?
                        .is_empty()
                {
                    return Ok(None);
                }
                member += 1;
                continue;
            }
        }
        after = Some(node.id.clone());
        member = 0;
    }
    Ok(Some((false, after, member)))
}

async fn clear(db: &dyn Database, run: &str) -> Result<(), Error> {
    db.delete(TABLE).where_eq("run_id", run).execute(db).await?;
    Ok(())
}
