//! Durable repeat restart: discover, validate, retire, then admit exactly one next generation.
use super::{
    Database, DatabaseValue, Error, FilterableQuery, SortDirection, Value, activation, decode,
    encode, text,
};
use switchy_database::schema::{DataType, create_index, create_table};
const JOBS: &str = "workflow_repeat_restart_v2";
#[cfg(test)]
mod tests;
const VISITS: &str = "workflow_repeat_restart_nodes_v2";
pub(super) async fn install(db: &dyn Database) -> Result<(), Error> {
    create_table(JOBS)
        .column(super::super::column("run_id", DataType::Text, false))
        .primary_key("run_id")
        .column(super::super::column("activation_id", DataType::Text, false))
        .column(super::super::column("repeat_node", DataType::Text, false))
        .column(super::super::column("revision", DataType::Int, false))
        .column(super::super::column("entry_node", DataType::Text, false))
        .column(super::super::column("input_json", DataType::Text, false))
        .column(super::super::column("phase", DataType::Text, false))
        .execute(db)
        .await?;
    create_table(VISITS)
        .column(super::super::column("identity", DataType::Text, false))
        .primary_key("identity")
        .column(super::super::column("restart_id", DataType::Text, false))
        .column(super::super::column("node_id", DataType::Text, false))
        .column(super::super::column("activation_id", DataType::Text, true))
        .column(super::super::column("after_edge", DataType::Int, true))
        .column(super::super::column("phase", DataType::Text, false))
        .execute(db)
        .await?;
    create_index("idx_repeat_restart_nodes_phase")
        .table(VISITS)
        .columns(vec!["restart_id", "phase", "node_id"])
        .execute(db)
        .await?;
    Ok(())
}
/// Whether a run has an unfinished, revision-fenced repeat restart.
/// # Errors
/// Returns database errors without inferring completion.
pub async fn pending(db: &dyn Database, run: &str) -> Result<bool, Error> {
    Ok(!db
        .select(JOBS)
        .columns(&["run_id"])
        .where_eq("run_id", run)
        .limit(1)
        .execute(db)
        .await?
        .is_empty())
}

pub(super) async fn begin(
    db: &dyn Database,
    run: &crate::NewWorkflowRun,
    revision: u64,
    repeat: &str,
    input: &Value,
) -> Result<(), Error> {
    super::terminal_scan::invalidate(db, &run.run_id).await?;
    if pending(db, &run.run_id).await? {
        return Err(Error::UnsupportedLifecycle);
    }
    let back = super::super::graph_edges::back_edge(db, &run.run_id, revision, repeat).await?;
    let row = activation(db, &run.run_id, repeat)
        .await?
        .ok_or(Error::InvalidAdmission)?;
    let id = text(&row, "identity")?;
    let next = super::transformed(db, run, &back, input).await?;
    db.insert(JOBS)
        .value("run_id", run.run_id.as_str())
        .value("activation_id", id.as_str())
        .value("repeat_node", repeat)
        .value(
            "revision",
            i64::try_from(revision).map_err(|_| Error::InvalidAdmission)?,
        )
        .value("entry_node", back.to.as_str())
        .value("input_json", encode(&next)?)
        .value("phase", "discover")
        .execute(db)
        .await?;
    visit(db, &id, &back.to).await?;
    // The control decision commits with its job; repeated observation cannot increment cycles again.
    db.update(super::ACTIVATIONS)
        .value("output_json", encode(input)?)
        .where_eq("identity", id)
        .execute(db)
        .await?;
    Ok(())
}
async fn visit(db: &dyn Database, restart: &str, node: &str) -> Result<(), Error> {
    let id = encode(&(restart, node))?;
    if db
        .select(VISITS)
        .columns(&["identity"])
        .where_eq("identity", id.as_str())
        .limit(1)
        .execute(db)
        .await?
        .is_empty()
    {
        db.insert(VISITS)
            .value("identity", id)
            .value("restart_id", restart)
            .value("node_id", node)
            .value("activation_id", DatabaseValue::Null)
            .value("after_edge", DatabaseValue::Null)
            .value("phase", "discover")
            .execute(db)
            .await?;
    }
    Ok(())
}
async fn next_visit(
    db: &dyn Database,
    id: &str,
    phase: &str,
) -> Result<Option<switchy_database::Row>, Error> {
    Ok(db
        .select(VISITS)
        .where_eq("restart_id", id)
        .where_eq("phase", phase)
        .sort("node_id", SortDirection::Asc)
        .limit(1)
        .execute(db)
        .await?
        .into_iter()
        .next())
}
/// Advance at most budget repeat restart steps in an already fenced immediate transaction.
/// No next-generation activation exists until the final retirement step commits.
pub(super) async fn drain_in(db: &dyn Database, run: &str, budget: u16) -> Result<usize, Error> {
    let mut used = 0;
    while used < usize::from(budget) {
        let jobs = db
            .select(JOBS)
            .where_eq("run_id", run)
            .limit(1)
            .execute(db)
            .await?;
        let Some(job) = jobs.first() else { break };
        let id = text(job, "activation_id")?;
        let repeat = text(job, "repeat_node")?;
        let Some(DatabaseValue::Int64(revision)) = job.get("revision") else {
            return Err(Error::MaintenanceRequired);
        };
        let revision = u64::try_from(revision).map_err(|_| Error::MaintenanceRequired)?;
        if super::super::graph_pages::revision(db, run).await? != revision {
            return Err(Error::InvalidAdmission);
        }
        match text(job, "phase")?.as_str() {
            "discover" => discover(db, run, &id, &repeat, revision).await?,
            "validate" => validate_one(db, run, &id, &repeat).await?,
            "retire" => retire_one(db, run, &id, &repeat, revision, job).await?,
            _ => return Err(Error::MaintenanceRequired),
        }
        used += 1;
    }
    Ok(used)
}
async fn discover(
    db: &dyn Database,
    run: &str,
    id: &str,
    repeat: &str,
    revision: u64,
) -> Result<(), Error> {
    let Some(row) = next_visit(db, id, "discover").await? else {
        if db
            .select(VISITS)
            .where_eq("identity", encode(&(id, repeat))?)
            .limit(1)
            .execute(db)
            .await?
            .is_empty()
        {
            return Err(Error::MaintenanceRequired);
        }
        db.update(JOBS)
            .value("phase", "validate")
            .where_eq("run_id", run)
            .execute(db)
            .await?;
        return Ok(());
    };
    let node = text(&row, "node_id")?;
    let key = text(&row, "identity")?;
    let after = match row.get("after_edge") {
        Some(DatabaseValue::Null) => None,
        Some(DatabaseValue::Int64(n)) => {
            Some(u64::try_from(n).map_err(|_| Error::MaintenanceRequired)?)
        }
        _ => return Err(Error::MaintenanceRequired),
    };
    let edges = if node == repeat {
        Vec::new()
    } else {
        super::super::graph_edges::edges(
            db,
            run,
            revision,
            &node,
            super::super::graph_edges::EdgeDirection::Outgoing,
            after,
            1,
        )
        .await?
    };
    if let Some(edge) = edges.first() {
        if !matches!(
            edge.edge.kind,
            bcode_workflow::EdgeKind::Back { .. } | bcode_workflow::EdgeKind::Retry { .. }
        ) {
            visit(db, id, &edge.edge.to).await?;
        }
        db.update(VISITS)
            .value(
                "after_edge",
                i64::try_from(edge.ordinal).map_err(|_| Error::MaintenanceRequired)?,
            )
            .where_eq("identity", key)
            .execute(db)
            .await?;
    } else {
        db.update(VISITS)
            .value("phase", "validate")
            .where_eq("identity", key)
            .execute(db)
            .await?;
    }
    Ok(())
}
async fn validate_one(db: &dyn Database, run: &str, id: &str, repeat: &str) -> Result<(), Error> {
    let Some(row) = next_visit(db, id, "validate").await? else {
        db.update(JOBS)
            .value("phase", "retire")
            .where_eq("run_id", run)
            .execute(db)
            .await?;
        return Ok(());
    };
    let node = text(&row, "node_id")?;
    let activation = activation(db, run, &node).await?;
    let binding = if let Some(active) = activation {
        let identity = text(&active, "identity")?;
        if node == repeat && identity != id {
            return Err(Error::InvalidAdmission);
        }
        if active.get("output_json") == Some(DatabaseValue::Null)
            && active.get("skipped") != Some(DatabaseValue::Int64(1))
        {
            return Err(Error::UnsupportedLifecycle);
        }
        // An unresolved operation cannot be retired even if its activation projection says complete.
        if !db
            .select("workflow_attempts_v2")
            .columns(&["dispatch_identity"])
            .where_eq("run_id", run)
            .where_eq("activation_id", identity.as_str())
            .where_eq("outcome_json", DatabaseValue::Null)
            .limit(1)
            .execute(db)
            .await?
            .is_empty()
        {
            return Err(Error::UnsupportedLifecycle);
        }
        DatabaseValue::String(identity)
    } else {
        DatabaseValue::Null
    };
    db.update(VISITS)
        .value("activation_id", binding)
        .value("phase", "retire")
        .where_eq("identity", text(&row, "identity")?)
        .execute(db)
        .await?;
    Ok(())
}
async fn retire_one(
    db: &dyn Database,
    run: &str,
    id: &str,
    repeat: &str,
    revision: u64,
    job: &switchy_database::Row,
) -> Result<(), Error> {
    let Some(row) = next_visit(db, id, "retire").await? else {
        let input: Value = decode(job, "input_json")?;
        let entry = text(job, "entry_node")?;
        super::insert_activation(db, run, &entry, &input).await?;
        db.delete(JOBS).where_eq("run_id", run).execute(db).await?;
        return Ok(());
    };
    let node = text(&row, "node_id")?;
    let current = activation(db, run, &node).await?;
    match (row.get("activation_id"), current) {
        (Some(DatabaseValue::Null), None) => {}
        (Some(DatabaseValue::String(expected)), Some(active))
            if text(&active, "identity")? == expected =>
        {
            db.update(super::ACTIVATIONS)
                .value("current", false)
                .where_eq("identity", expected)
                .execute(db)
                .await?;
        }
        _ => return Err(Error::InvalidAdmission),
    }
    if node != repeat
        && super::super::graph_pages::node(db, run, revision, &node)
            .await?
            .is_some_and(|n| n.kind == bcode_workflow::NodeKind::Repeat)
    {
        super::control_state::record(db, run, &node, 0).await?;
    }
    db.update(VISITS)
        .value("phase", "done")
        .where_eq("identity", text(&row, "identity")?)
        .execute(db)
        .await?;
    Ok(())
}
