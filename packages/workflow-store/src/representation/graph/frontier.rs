//! Durable successor frontier with bounded settlement steps.
use super::{
    Database, DatabaseValue, Error, FilterableQuery, SortDirection, Value, decode, encode, text,
};
use switchy_database::schema::{DataType, create_index, create_table};
const TABLE: &str = "workflow_successor_frontier_v2";
fn observation(
    row: &switchy_database::Row,
    node: &str,
    revision: u64,
) -> Result<super::Activation, Error> {
    Ok(super::Activation {
        node_id: node.into(),
        activation_id: text(row, "identity")?,
        created_at_ms: super::creation_time(row)?,
        generation: match row.get("generation") {
            Some(DatabaseValue::Int64(n)) => {
                u64::try_from(n).map_err(|_| Error::MaintenanceRequired)?
            }
            _ => return Err(Error::MaintenanceRequired),
        },
        revision,
        input: decode(row, "input_json")?,
        output: match row.get("output_json") {
            Some(DatabaseValue::Null) => None,
            _ => Some(decode(row, "output_json")?),
        },
    })
}
pub(super) async fn install(db: &dyn Database) -> Result<(), Error> {
    create_table(TABLE)
        .column(super::super::column("identity", DataType::Text, false))
        .primary_key("identity")
        .column(super::super::column("run_id", DataType::Text, false))
        .column(super::super::column("revision", DataType::Int, false))
        .column(super::super::column("node_id", DataType::Text, false))
        .column(super::super::column("value_json", DataType::Text, false))
        .column(super::super::column("after_edge", DataType::Int, true))
        .column(super::super::column(
            "after_dependency",
            DataType::Int,
            true,
        ))
        .execute(db)
        .await?;
    create_index("idx_workflow_frontier_v2_run")
        .table(TABLE)
        .columns(vec!["run_id", "identity"])
        .execute(db)
        .await?;
    Ok(())
}
pub(super) async fn enqueue(
    db: &dyn Database,
    run: &str,
    revision: u64,
    node: &str,
    value: &Value,
) -> Result<(), Error> {
    let activation = super::activation_id(db, run, node)
        .await?
        .ok_or(Error::MaintenanceRequired)?;
    let identity = encode(&(run, revision, activation))?;
    let rows = db
        .select(TABLE)
        .where_eq("identity", identity.as_str())
        .limit(1)
        .execute(db)
        .await?;
    if rows.is_empty() {
        db.insert(TABLE)
            .value("identity", identity)
            .value("run_id", run)
            .value(
                "revision",
                i64::try_from(revision).map_err(|_| Error::InvalidAdmission)?,
            )
            .value("node_id", node)
            .value("value_json", encode(value)?)
            .value("after_edge", DatabaseValue::Null)
            .value("after_dependency", DatabaseValue::Null)
            .execute(db)
            .await?;
    }
    Ok(())
}
pub(super) async fn drain_in(
    db: &dyn Database,
    run: &crate::NewWorkflowRun,
    budget: u16,
) -> Result<usize, Error> {
    Ok(drain_observed(db, run, budget).await?.0)
}
pub(super) async fn drain_observed(
    db: &dyn Database,
    run: &crate::NewWorkflowRun,
    budget: u16,
) -> Result<(usize, Vec<super::Activation>), Error> {
    let mut activated = Vec::new();
    let mut consumed = 0;
    while consumed < usize::from(budget) {
        let jobs = db
            .select(TABLE)
            .where_eq("run_id", run.run_id.as_str())
            .sort("identity", SortDirection::Asc)
            .limit(1)
            .execute(db)
            .await?;
        let Some(job) = jobs.first() else { break };
        let id = text(job, "identity")?;
        let node = text(job, "node_id")?;
        let value: Value = decode(job, "value_json")?;
        let Some(DatabaseValue::Int64(revision)) = job.get("revision") else {
            return Err(Error::MaintenanceRequired);
        };
        let revision = u64::try_from(revision).map_err(|_| Error::MaintenanceRequired)?;
        if super::super::graph_pages::revision(db, &run.run_id).await? != revision {
            return Err(Error::InvalidAdmission);
        }
        let after = match job.get("after_edge") {
            Some(DatabaseValue::Null) => None,
            Some(DatabaseValue::Int64(n)) => {
                Some(u64::try_from(n).map_err(|_| Error::MaintenanceRequired)?)
            }
            _ => return Err(Error::MaintenanceRequired),
        };
        let edges = super::super::graph_edges::edges(
            db,
            &run.run_id,
            revision,
            &node,
            super::super::graph_edges::EdgeDirection::Outgoing,
            after,
            1,
        )
        .await?;
        if let Some(edge) = edges.first() {
            let cursor = match job.get("after_dependency") {
                Some(DatabaseValue::Null) => None,
                Some(DatabaseValue::Int64(n)) => {
                    Some(u64::try_from(n).map_err(|_| Error::MaintenanceRequired)?)
                }
                _ => return Err(Error::MaintenanceRequired),
            };
            let step = dependency_step(db, &run.run_id, revision, &edge.edge.to, cursor).await?;
            if let DependencyStep::More(next) = step {
                db.update(TABLE)
                    .value(
                        "after_dependency",
                        i64::try_from(next).map_err(|_| Error::MaintenanceRequired)?,
                    )
                    .where_eq("identity", id)
                    .execute(db)
                    .await?;
                consumed += 1;
                continue;
            }
            let before = super::activation_id(db, &run.run_id, &edge.edge.to).await?;
            if matches!(step, DependencyStep::Ready)
                && let Some((node, value)) =
                    super::settle_edge(db, run, revision, &edge.edge, &value).await?
            {
                enqueue(db, &run.run_id, revision, &node, &value).await?;
            }
            if before.is_none()
                && let Some(row) = super::activation(db, &run.run_id, &edge.edge.to).await?
            {
                activated.push(observation(&row, &edge.edge.to, revision)?);
            }
            db.update(TABLE)
                .value(
                    "after_edge",
                    i64::try_from(edge.ordinal).map_err(|_| Error::MaintenanceRequired)?,
                )
                .value("after_dependency", DatabaseValue::Null)
                .where_eq("identity", id)
                .execute(db)
                .await?;
        } else {
            db.delete(TABLE)
                .where_eq("identity", id)
                .execute(db)
                .await?;
        }
        consumed += 1;
    }
    Ok((consumed, activated))
}
/// Check for durable successor work without scanning graph history.
/// # Errors
/// Returns database failures; this does not grant execution authority.
pub async fn pending(db: &dyn Database, run: &str) -> Result<bool, Error> {
    if super::restart::pending(db, run).await? {
        return Ok(true);
    }
    Ok(!db
        .select(TABLE)
        .columns(&["identity"])
        .where_eq("run_id", run)
        .limit(1)
        .execute(db)
        .await?
        .is_empty())
}
/// Advance at most budget successor transitions under current durable authority.
/// # Errors
/// Rejects stale owner/revision, invalid bounds or inconsistent state. No external effects dispatch.
pub async fn advance(
    db: &dyn Database,
    owner: &crate::WorkflowExecutionAuthority,
    run: &str,
    budget: u16,
) -> Result<usize, Error> {
    if budget == 0 || budget > 100 {
        return Err(Error::InvalidAdmission);
    }
    let tx = db
        .begin_transaction_with_mode(super::TransactionMode::Immediate)
        .await?;
    super::super::verified(&*tx).await?;
    let rows = tx
        .select(super::RUNS)
        .where_eq("run_id", run)
        .limit(1)
        .execute(&*tx)
        .await?;
    let row = rows.first().ok_or(Error::InvalidAdmission)?;
    if text(row, "authority_json")? != encode(owner)? {
        return Err(Error::AuthorityConflict);
    }
    if text(row, "status")? != "paused" {
        return Err(Error::InvalidAdmission);
    }
    let request: crate::NewWorkflowRun = decode(row, "admission_json")?;
    let restart = super::restart::drain_in(&*tx, run, budget).await?;
    let remaining =
        budget.saturating_sub(u16::try_from(restart).map_err(|_| Error::MaintenanceRequired)?);
    let count = restart + drain_in(&*tx, &request, remaining).await?;
    tx.commit().await?;
    Ok(count)
}

enum DependencyStep {
    Ready,
    Waiting,
    More(u64),
}
async fn dependency_step(
    db: &dyn Database,
    run: &str,
    revision: u64,
    target: &str,
    after: Option<u64>,
) -> Result<DependencyStep, Error> {
    let edges = super::super::graph_edges::edges(
        db,
        run,
        revision,
        target,
        super::super::graph_edges::EdgeDirection::Incoming,
        after,
        1,
    )
    .await?;
    let Some(item) = edges.first() else {
        return Ok(DependencyStep::Ready);
    };
    if !matches!(
        item.edge.kind,
        bcode_workflow::EdgeKind::Back { .. } | bcode_workflow::EdgeKind::Retry { .. }
    ) {
        let Some(row) = super::activation(db, run, &item.edge.from).await? else {
            return Ok(DependencyStep::Waiting);
        };
        if row.get("output_json") == Some(DatabaseValue::Null)
            && row.get("skipped") != Some(DatabaseValue::Int64(1))
        {
            return Ok(DependencyStep::Waiting);
        }
    }
    Ok(DependencyStep::More(item.ordinal))
}
