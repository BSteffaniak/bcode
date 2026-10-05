//! Canonical revision-pinned edge pages and bounded graph boundary metadata.
use super::{Database, DatabaseValue, RepresentationError as Error};
use switchy_database::{
    query::{FilterableQuery, SortDirection},
    schema::{DataType, create_index, create_table},
};
const EDGES: &str = "workflow_definition_edges_v2";
const BOUNDARY: &str = "workflow_definition_boundary_v2";
/// Direction of an exact endpoint page.
#[derive(Debug, Clone, Copy)]
pub enum EdgeDirection {
    /// Outgoing edges.
    Outgoing,
    /// Incoming edges.
    Incoming,
}
/// One canonical edge and stable revision-local cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    /// Exclusive page cursor.
    pub ordinal: u64,
    /// Domain-owned edge semantics.
    pub edge: bcode_workflow::EdgeDefinition,
}
/// Output boundary, separate from the executable graph.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Boundary {
    /// Exact entry identities.
    pub entries: Vec<String>,
    /// Exact exits.
    pub exits: Vec<String>,
    /// Declared final output schema.
    pub output: bcode_workflow::ValueSchema,
}
pub(super) async fn install(db: &dyn Database) -> Result<(), Error> {
    create_table(EDGES)
        .column(super::column("identity", DataType::Text, false))
        .primary_key("identity")
        .column(super::column("run_id", DataType::Text, false))
        .column(super::column("revision", DataType::Int, false))
        .column(super::column("ordinal", DataType::Int, false))
        .column(super::column("source", DataType::Text, false))
        .column(super::column("target", DataType::Text, false))
        .column(super::column("is_back", DataType::Bool, false))
        .column(super::column("edge_json", DataType::Text, false))
        .execute(db)
        .await?;
    for (name, endpoint) in [
        ("idx_workflow_edges_v2_source", "source"),
        ("idx_workflow_edges_v2_target", "target"),
    ] {
        create_index(name)
            .table(EDGES)
            .columns(vec!["run_id", "revision", endpoint, "ordinal"])
            .execute(db)
            .await?;
    }
    create_index("idx_workflow_edges_v2_back")
        .table(EDGES)
        .columns(vec!["run_id", "revision", "source", "is_back"])
        .execute(db)
        .await?;
    create_table(BOUNDARY)
        .column(super::column("identity", DataType::Text, false))
        .primary_key("identity")
        .column(super::column("boundary_json", DataType::Text, false))
        .execute(db)
        .await?;
    Ok(())
}
pub(super) async fn persist(
    db: &dyn Database,
    run: &str,
    revision: u64,
    definition: &bcode_workflow::WorkflowDefinition,
    edges: &std::collections::BTreeMap<u64, bcode_workflow::EdgeDefinition>,
) -> Result<(), Error> {
    let rev = i64::try_from(revision).map_err(|_| Error::InvalidAdmission)?;
    for (index, edge) in edges {
        let ordinal = i64::try_from(*index).map_err(|_| Error::InvalidAdmission)?;
        db.insert(EDGES)
            .value("identity", encode(&(run, revision, index))?)
            .value("run_id", run)
            .value("revision", rev)
            .value("ordinal", ordinal)
            .value("source", edge.from.as_str())
            .value("target", edge.to.as_str())
            .value(
                "is_back",
                matches!(edge.kind, bcode_workflow::EdgeKind::Back { .. }),
            )
            .value("edge_json", encode(edge)?)
            .execute(db)
            .await?;
    }
    db.insert(BOUNDARY)
        .value("identity", encode(&(run, revision))?)
        .value(
            "boundary_json",
            encode(&Boundary {
                entries: definition.entries.clone(),
                exits: definition.exits.clone(),
                output: definition.output.clone(),
            })?,
        )
        .execute(db)
        .await?;
    Ok(())
}
fn encode<T: serde::Serialize>(value: &T) -> Result<String, Error> {
    let json = serde_json::to_string(value).map_err(|_| Error::InvalidAdmission)?;
    if json.len() > super::MAX_ADMISSION_BYTES {
        return Err(Error::InvalidAdmission);
    }
    Ok(json)
}
fn decode<T: serde::de::DeserializeOwned>(
    row: &switchy_database::Row,
    field: &str,
) -> Result<T, Error> {
    match row.get(field) {
        Some(DatabaseValue::String(json)) if json.len() <= super::MAX_ADMISSION_BYTES => {
            serde_json::from_str(&json).map_err(|_| Error::MaintenanceRequired)
        }
        _ => Err(Error::MaintenanceRequired),
    }
}
/// Read at most limit edges for one endpoint at an exact revision.
/// # Errors
/// Rejects invalid cursors/bounds or malformed canonical rows.
pub async fn edges(
    db: &dyn Database,
    run: &str,
    revision: u64,
    node: &str,
    direction: EdgeDirection,
    after: Option<u64>,
    limit: u16,
) -> Result<Vec<Edge>, Error> {
    if limit == 0 || limit > 100 {
        return Err(Error::InvalidAdmission);
    }
    let endpoint = match direction {
        EdgeDirection::Outgoing => "source",
        EdgeDirection::Incoming => "target",
    };
    let mut query = db
        .select(EDGES)
        .columns(&["ordinal", "edge_json"])
        .where_eq("run_id", run)
        .where_eq(
            "revision",
            i64::try_from(revision).map_err(|_| Error::InvalidAdmission)?,
        )
        .where_eq(endpoint, node)
        .sort("ordinal", SortDirection::Asc)
        .limit(usize::from(limit));
    if let Some(after) = after {
        query = query.where_gt(
            "ordinal",
            i64::try_from(after).map_err(|_| Error::InvalidAdmission)?,
        );
    }
    query
        .execute(db)
        .await?
        .iter()
        .map(|row| {
            let Some(DatabaseValue::Int64(ordinal)) = row.get("ordinal") else {
                return Err(Error::MaintenanceRequired);
            };
            Ok(Edge {
                ordinal: u64::try_from(ordinal).map_err(|_| Error::MaintenanceRequired)?,
                edge: decode(row, "edge_json")?,
            })
        })
        .collect()
}
/// Read the exact graph output boundary without loading nodes or edges.
/// # Errors
/// Rejects absent or malformed canonical boundary metadata.
pub async fn boundary(db: &dyn Database, run: &str, revision: u64) -> Result<Boundary, Error> {
    let rows = db
        .select(BOUNDARY)
        .columns(&["boundary_json"])
        .where_eq("identity", encode(&(run, revision))?)
        .limit(1)
        .execute(db)
        .await?;
    decode(
        rows.first().ok_or(Error::MaintenanceRequired)?,
        "boundary_json",
    )
}

/// Read a bounded edge page with stable edit IDs, optionally restricted to one source.
/// # Errors
/// Rejects invalid bounds/cursors and malformed rows.
pub async fn page(
    db: &dyn Database,
    run: &str,
    revision: u64,
    source: Option<&str>,
    after: Option<u64>,
    limit: u16,
) -> Result<Vec<Edge>, Error> {
    if limit == 0 || limit > 100 {
        return Err(Error::InvalidAdmission);
    }
    let mut query = db
        .select(EDGES)
        .columns(&["ordinal", "edge_json"])
        .where_eq("run_id", run)
        .where_eq(
            "revision",
            i64::try_from(revision).map_err(|_| Error::InvalidAdmission)?,
        )
        .sort("ordinal", SortDirection::Asc)
        .limit(usize::from(limit));
    if let Some(source) = source {
        query = query.where_eq("source", source);
    }
    if let Some(after) = after {
        query = query.where_gt(
            "ordinal",
            i64::try_from(after).map_err(|_| Error::InvalidAdmission)?,
        );
    }
    query
        .execute(db)
        .await?
        .iter()
        .map(|row| {
            let Some(DatabaseValue::Int64(id)) = row.get("ordinal") else {
                return Err(Error::MaintenanceRequired);
            };
            Ok(Edge {
                ordinal: u64::try_from(id).map_err(|_| Error::MaintenanceRequired)?,
                edge: decode(row, "edge_json")?,
            })
        })
        .collect()
}
/// First never-used edge identity across retained revisions; not a reservation.
/// # Errors
/// Rejects malformed allocation state or database failures.
pub async fn next_edge_id(db: &dyn Database, run: &str) -> Result<Option<u64>, Error> {
    let rows = db
        .select(EDGES)
        .columns(&["ordinal"])
        .where_eq("run_id", run)
        .sort("ordinal", SortDirection::Desc)
        .limit(1)
        .execute(db)
        .await?;
    match rows.first().and_then(|r| r.get("ordinal")) {
        None => Ok(Some(1)),
        Some(DatabaseValue::Int64(n)) if n >= 0 => Ok(u64::try_from(n)
            .ok()
            .and_then(|n| n.checked_add(1))
            .filter(|n| i64::try_from(*n).is_ok())),
        _ => Err(Error::MaintenanceRequired),
    }
}

/// Read an unambiguous repeat back-edge through its typed index filter.
/// # Errors
/// Rejects missing/ambiguous back-edges; total outgoing degree is not limited.
pub async fn back_edge(
    db: &dyn Database,
    run: &str,
    revision: u64,
    node: &str,
) -> Result<bcode_workflow::EdgeDefinition, Error> {
    let rows = db
        .select(EDGES)
        .columns(&["edge_json"])
        .where_eq("run_id", run)
        .where_eq(
            "revision",
            i64::try_from(revision).map_err(|_| Error::InvalidAdmission)?,
        )
        .where_eq("source", node)
        .where_eq("is_back", true)
        .limit(2)
        .execute(db)
        .await?;
    if rows.len() != 1 {
        return Err(Error::UnsupportedLifecycle);
    }
    decode(&rows[0], "edge_json")
}
