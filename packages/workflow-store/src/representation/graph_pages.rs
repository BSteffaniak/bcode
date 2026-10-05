//! Revision-pinned exact node reads, independent of whole-definition decoding.
use super::{Database, DatabaseValue, RepresentationError as Error};
use switchy_database::{
    query::{FilterableQuery, SortDirection},
    schema::{DataType, create_index, create_table},
};
const NODES: &str = "workflow_definition_nodes_v2";
#[cfg(test)]
mod tests;
pub(super) async fn install(db: &dyn Database) -> Result<(), Error> {
    create_table(NODES)
        .column(super::column("identity", DataType::Text, false))
        .primary_key("identity")
        .column(super::column("run_id", DataType::Text, false))
        .column(super::column("revision", DataType::Int, false))
        .column(super::column("node_id", DataType::Text, false))
        .column(super::column("node_json", DataType::Text, false))
        .execute(db)
        .await?;
    create_index("idx_workflow_definition_nodes_v2_page")
        .table(NODES)
        .columns(vec!["run_id", "revision", "node_id"])
        .execute(db)
        .await?;
    Ok(())
}
pub(super) async fn persist(
    db: &dyn Database,
    run: &str,
    revision: u64,
    definition: &bcode_workflow::WorkflowDefinition,
) -> Result<(), Error> {
    for node in definition.nodes.values() {
        let json = serde_json::to_string(node).map_err(|_| Error::InvalidAdmission)?;
        if json.len() > super::MAX_ADMISSION_BYTES {
            return Err(Error::InvalidAdmission);
        }
        db.insert(NODES)
            .value(
                "identity",
                serde_json::to_string(&(run, revision, &node.id))
                    .map_err(|_| Error::InvalidAdmission)?,
            )
            .value("run_id", run)
            .value(
                "revision",
                i64::try_from(revision).map_err(|_| Error::InvalidAdmission)?,
            )
            .value("node_id", node.id.as_str())
            .value("node_json", json)
            .execute(db)
            .await?;
    }
    Ok(())
}
/// Read the revision scalar only; never decode the graph.
/// # Errors
/// Rejects absent, malformed or unsupported revision state.
pub async fn revision(db: &dyn Database, run: &str) -> Result<u64, Error> {
    let rows = db
        .select("workflow_graphs_v2")
        .columns(&["revision"])
        .where_eq("run_id", run)
        .limit(1)
        .execute(db)
        .await?;
    match rows.first().and_then(|row| row.get("revision")) {
        Some(DatabaseValue::Int64(n)) if n > 0 => {
            u64::try_from(n).map_err(|_| Error::MaintenanceRequired)
        }
        _ => Err(Error::MaintenanceRequired),
    }
}
fn decode(row: &switchy_database::Row) -> Result<bcode_workflow::NodeDefinition, Error> {
    match row.get("node_json") {
        Some(DatabaseValue::String(json)) if json.len() <= super::MAX_ADMISSION_BYTES => {
            serde_json::from_str(&json).map_err(|_| Error::MaintenanceRequired)
        }
        _ => Err(Error::MaintenanceRequired),
    }
}
/// Read one canonical node at an exact retained revision without opening a whole snapshot.
/// # Errors
/// Rejects malformed node payloads or storage failure. Absence is explicit.
pub async fn node(
    db: &dyn Database,
    run: &str,
    revision: u64,
    id: &str,
) -> Result<Option<bcode_workflow::NodeDefinition>, Error> {
    let rows = db
        .select(NODES)
        .columns(&["node_json"])
        .where_eq("run_id", run)
        .where_eq(
            "revision",
            i64::try_from(revision).map_err(|_| Error::InvalidAdmission)?,
        )
        .where_eq("node_id", id)
        .limit(1)
        .execute(db)
        .await?;
    rows.first().map(decode).transpose()
}
/// Page exact retained node definitions. Page count is not a graph-size cap.
/// # Errors
/// Rejects invalid bounds, malformed node payloads or storage failure.
pub async fn nodes(
    db: &dyn Database,
    run: &str,
    revision: u64,
    after: Option<&str>,
    limit: u16,
) -> Result<Vec<bcode_workflow::NodeDefinition>, Error> {
    if limit == 0 || limit > 100 {
        return Err(Error::InvalidAdmission);
    }
    let mut query = db
        .select(NODES)
        .columns(&["node_json"])
        .where_eq("run_id", run)
        .where_eq(
            "revision",
            i64::try_from(revision).map_err(|_| Error::InvalidAdmission)?,
        )
        .sort("node_id", SortDirection::Asc)
        .limit(usize::from(limit));
    if let Some(after) = after {
        query = query.where_gt("node_id", after);
    }
    query.execute(db).await?.iter().map(decode).collect()
}
