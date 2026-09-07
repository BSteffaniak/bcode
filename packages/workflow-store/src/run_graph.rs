//! Run-owned graph materialization and bounded inspection.

use super::{WorkflowStore, WorkflowStoreError};
use bcode_workflow::{EdgeDefinition, NodeDefinition, WorkflowDefinition};
use rusqlite::{Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

const GRAPH_PAGE_LIMIT: usize = 100;

#[derive(Clone, Copy)]
enum EdgeEndpoint<'a> {
    Source(&'a str),
    Target(&'a str),
}

/// An immutable executable node revision in a run-owned graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunGraphNode {
    /// Revision at which this node representation was admitted.
    pub revision: u64,
    /// Exact executable node data.
    pub node: NodeDefinition,
    /// Whether the initial plan admits this node as an entry.
    pub entry: bool,
    /// Whether the initial plan declares this node an exit.
    pub exit: bool,
}

/// One immutable edge in the initial admitted run graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunGraphEdge {
    /// Stable identity assigned during initial materialization.
    pub edge_id: u64,
    /// Exact admitted edge, including transforms and control flow.
    pub edge: EdgeDefinition,
}

pub fn initialize(connection: &Connection) -> Result<(), WorkflowStoreError> {
    connection.execute_batch(
        "CREATE TABLE workflow_run_graphs (
            run_id TEXT PRIMARY KEY REFERENCES workflow_runs(run_id),
            revision INTEGER NOT NULL CHECK (revision > 0)
        );
        CREATE TABLE workflow_run_graph_nodes (
            run_id TEXT NOT NULL REFERENCES workflow_run_graphs(run_id),
            node_id TEXT NOT NULL,
            revision INTEGER NOT NULL CHECK (revision > 0),
            node_json TEXT NOT NULL,
            is_entry INTEGER NOT NULL CHECK (is_entry IN (0, 1)),
            is_exit INTEGER NOT NULL CHECK (is_exit IN (0, 1)),
            PRIMARY KEY (run_id, node_id, revision)
        );
        CREATE TABLE workflow_run_graph_edges (
            run_id TEXT NOT NULL REFERENCES workflow_run_graphs(run_id),
            edge_id INTEGER NOT NULL CHECK (edge_id >= 0),
            revision INTEGER NOT NULL CHECK (revision > 0),
            source_node_id TEXT NOT NULL,
            target_node_id TEXT NOT NULL,
            edge_json TEXT NOT NULL,
            PRIMARY KEY (run_id, edge_id, revision)
        );
        CREATE INDEX workflow_run_graph_edges_target
            ON workflow_run_graph_edges(run_id, target_node_id, edge_id);",
    )?;
    initialize_source_index(connection)
}

pub fn initialize_source_index(connection: &Connection) -> Result<(), WorkflowStoreError> {
    connection.execute_batch(
        "CREATE INDEX workflow_run_graph_edges_source
            ON workflow_run_graph_edges(run_id, source_node_id, edge_id);",
    )?;
    Ok(())
}

pub fn materialize(
    transaction: &Transaction<'_>,
    run_id: &str,
    definition: &WorkflowDefinition,
) -> Result<(), WorkflowStoreError> {
    if definition.schema_version != bcode_workflow::WORKFLOW_DEFINITION_SCHEMA_VERSION {
        return Err(WorkflowStoreError::InvalidData(
            "unsupported workflow definition schema version during graph materialization"
                .to_string(),
        ));
    }
    super::validate_id("run_id", run_id)?;
    for (id, node) in &definition.nodes {
        super::validate_id("node_id", id)?;
        if id != &node.id {
            return Err(WorkflowStoreError::InvalidData(
                "workflow graph node identity mismatch".to_string(),
            ));
        }
    }
    let entries: BTreeSet<_> = definition.entries.iter().collect();
    let exits: BTreeSet<_> = definition.exits.iter().collect();
    for id in entries.union(&exits) {
        if !definition.nodes.contains_key(*id) {
            return Err(WorkflowStoreError::InvalidData(
                "workflow graph boundary references a missing node".to_string(),
            ));
        }
    }
    for edge in &definition.edges {
        if !definition.nodes.contains_key(&edge.from) || !definition.nodes.contains_key(&edge.to) {
            return Err(WorkflowStoreError::InvalidData(
                "workflow graph edge references a missing node".to_string(),
            ));
        }
    }
    transaction.execute(
        "INSERT INTO workflow_run_graphs (run_id, revision) VALUES (?1, 1)",
        [run_id],
    )?;
    let mut insert_node = transaction.prepare(
        "INSERT INTO workflow_run_graph_nodes
         (run_id, node_id, revision, node_json, is_entry, is_exit)
         VALUES (?1, ?2, 1, ?3, ?4, ?5)",
    )?;
    for (id, node) in &definition.nodes {
        insert_node.execute(rusqlite::params![
            run_id,
            id,
            super::bounded_json("graph node", node)?,
            entries.contains(id),
            exits.contains(id),
        ])?;
    }
    let mut insert_edge = transaction.prepare(
        "INSERT INTO workflow_run_graph_edges
         (run_id, edge_id, revision, source_node_id, target_node_id, edge_json)
         VALUES (?1, ?2, 1, ?3, ?4, ?5)",
    )?;
    for (index, edge) in definition.edges.iter().enumerate() {
        insert_edge.execute(rusqlite::params![
            run_id,
            index,
            edge.from,
            edge.to,
            super::bounded_json("graph edge", edge)?
        ])?;
    }
    Ok(())
}

pub fn migrate(transaction: &Transaction<'_>) -> Result<(), WorkflowStoreError> {
    initialize(transaction)?;
    // Explicit offline migration may scan all runs, but only retains one definition at a time.
    let mut statement = transaction.prepare(
        "SELECT run.run_id,
                CASE WHEN typeof(definition.definition_json) = 'text'
                     AND length(CAST(definition.definition_json AS BLOB)) <= ?1
                     THEN definition.definition_json END,
                CASE WHEN typeof(definition.checksum_sha256) = 'text'
                          AND length(CAST(definition.checksum_sha256 AS BLOB)) = 64
                     THEN definition.checksum_sha256 END,
                definition.definition_id FROM workflow_runs run
         LEFT JOIN workflow_definitions definition ON definition.definition_id = run.definition_id
             AND definition.version = run.definition_version ORDER BY run.run_id",
    )?;
    let mut rows = statement.query([super::MAX_INLINE_JSON_BYTES])?;
    while let Some(row) = rows.next()? {
        let run_id: String = row.get(0)?;
        if row.get::<_, Option<String>>(3)?.is_none() {
            return Err(WorkflowStoreError::InvalidData(
                "foreign-key verification failed: workflow run definition is missing".to_string(),
            ));
        }
        let payload = row.get::<_, Option<String>>(1)?.ok_or_else(|| {
            WorkflowStoreError::InvalidData(
                "workflow definition payload is invalid or oversized during graph migration"
                    .to_string(),
            )
        })?;
        let checksum: Option<String> = row.get(2)?;
        if checksum.as_deref() != Some(super::sha256_hex(payload.as_bytes()).as_str()) {
            return Err(WorkflowStoreError::InvalidData(
                "workflow definition checksum mismatch during graph migration".to_string(),
            ));
        }
        let definition: WorkflowDefinition = serde_json::from_str(&payload)?;
        definition.validate().map_err(|error| {
            WorkflowStoreError::InvalidData(format!(
                "invalid workflow definition during graph migration: {error}"
            ))
        })?;
        materialize(transaction, &run_id, &definition)?;
    }
    Ok(())
}

pub fn graph_revision(
    connection: &Connection,
    run_id: &str,
) -> Result<Option<u64>, WorkflowStoreError> {
    super::validate_id("run_id", run_id)?;
    let row = connection
        .query_row(
            "SELECT CASE WHEN typeof(graph.revision) = 'integer' AND graph.revision > 0
                         THEN graph.revision END FROM workflow_runs run
         LEFT JOIN workflow_run_graphs graph ON graph.run_id = run.run_id
         WHERE run.run_id = ?1",
            [run_id],
            |row| row.get::<_, Option<u64>>(0),
        )
        .optional()?;
    match row {
        None => Ok(None),
        Some(Some(revision)) if revision > 0 => Ok(Some(revision)),
        _ => Err(WorkflowStoreError::InvalidData(
            "workflow run graph is missing or invalid".to_string(),
        )),
    }
}

pub fn initial_node(
    connection: &Connection,
    run_id: &str,
    node_id: &str,
) -> Result<Option<NodeDefinition>, WorkflowStoreError> {
    initial_node_record(connection, run_id, node_id).map(|record| record.map(|record| record.node))
}

pub fn initial_exit(
    connection: &Connection,
    run_id: &str,
    node_id: &str,
) -> Result<bool, WorkflowStoreError> {
    initial_node_record(connection, run_id, node_id)?
        .map(|record| record.exit)
        .ok_or_else(|| {
            WorkflowStoreError::InvalidData(
                "completed workflow node is missing from the run graph".to_string(),
            )
        })
}

pub fn initial_edge_between(
    transaction: &Transaction<'_>,
    run_id: &str,
    source_node_id: &str,
    target_node_id: &str,
) -> Result<Option<EdgeDefinition>, WorkflowStoreError> {
    super::validate_id("target_node_id", target_node_id)?;
    let mut cursor = None;
    loop {
        let edges = initial_outgoing_edges(transaction, run_id, source_node_id, cursor)?;
        if edges.is_empty() {
            return Ok(None);
        }
        cursor = edges.last().map(|edge| edge.edge_id);
        let final_page = edges.len() < GRAPH_PAGE_LIMIT;
        if let Some(record) = edges
            .into_iter()
            .find(|record| record.edge.to == target_node_id)
        {
            return Ok(Some(record.edge));
        }
        if final_page {
            return Ok(None);
        }
    }
}

pub fn initial_incoming_edges(
    connection: &Connection,
    run_id: &str,
    target_node_id: &str,
    after_edge_id: Option<u64>,
) -> Result<Vec<RunGraphEdge>, WorkflowStoreError> {
    super::validate_id("target_node_id", target_node_id)?;
    WorkflowStore::graph_edge_page(
        connection,
        run_id,
        Some(EdgeEndpoint::Target(target_node_id)),
        after_edge_id,
        None,
        GRAPH_PAGE_LIMIT,
    )
}

pub fn initial_outgoing_edges(
    connection: &Connection,
    run_id: &str,
    source_node_id: &str,
    after_edge_id: Option<u64>,
) -> Result<Vec<RunGraphEdge>, WorkflowStoreError> {
    super::validate_id("source_node_id", source_node_id)?;
    WorkflowStore::graph_edge_page(
        connection,
        run_id,
        Some(EdgeEndpoint::Source(source_node_id)),
        after_edge_id,
        None,
        GRAPH_PAGE_LIMIT,
    )
}

fn initial_node_record(
    connection: &Connection,
    run_id: &str,
    node_id: &str,
) -> Result<Option<RunGraphNode>, WorkflowStoreError> {
    super::validate_id("node_id", node_id)?;
    match graph_revision(connection, run_id)? {
        None => return Ok(None),
        Some(1) => {}
        _ => {
            return Err(WorkflowStoreError::InvalidData(
                "initial graph lookup requires an intact initial graph revision".to_string(),
            ));
        }
    }
    let payload = connection
        .query_row(
            "SELECT CASE WHEN typeof(node_json) = 'text'
                     AND length(CAST(node_json AS BLOB)) <= ?3
                     AND revision = 1
                     AND NOT EXISTS (
                         SELECT 1 FROM workflow_run_graph_nodes other
                         WHERE other.run_id = ?1 AND other.node_id = ?2
                         AND other.revision > 1
                     )
                     AND is_entry IN (0, 1) AND is_exit IN (0, 1) THEN node_json END,
                     is_entry, is_exit
         FROM workflow_run_graph_nodes WHERE run_id = ?1 AND node_id = ?2
         ORDER BY revision LIMIT 1",
            rusqlite::params![run_id, node_id, super::MAX_INLINE_JSON_BYTES],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, bool>(1)?,
                    row.get::<_, bool>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((payload, entry, exit)) = payload else {
        return Ok(None);
    };
    let payload = payload.ok_or_else(|| {
        WorkflowStoreError::InvalidData(
            "workflow graph node payload is invalid or oversized".to_string(),
        )
    })?;
    let node: NodeDefinition = serde_json::from_str(&payload)?;
    if node.id != node_id {
        return Err(WorkflowStoreError::InvalidData(
            "workflow graph node identity mismatch".to_string(),
        ));
    }
    Ok(Some(RunGraphNode {
        revision: 1,
        node,
        entry,
        exit,
    }))
}

fn edge_cursor(after_edge_id: Option<u64>) -> Result<i64, WorkflowStoreError> {
    after_edge_id
        .map(i64::try_from)
        .transpose()
        .map(|cursor| cursor.unwrap_or(i64::MIN))
        .map_err(|_| {
            WorkflowStoreError::InvalidData("edge cursor exceeds storage range".to_string())
        })
}

impl WorkflowStore {
    /// Read one initial node revision with its admitted entry and exit roles.
    ///
    /// Missing runs or nodes return `None`. This bounded lookup never repairs state.
    ///
    /// # Errors
    /// Returns an error for invalid identities, missing or unsupported graph state,
    /// corrupt node data, oversized payloads, or database failures.
    pub fn run_graph_node_record(
        &self,
        run_id: &str,
        node_id: &str,
    ) -> Result<Option<RunGraphNode>, WorkflowStoreError> {
        initial_node_record(&self.connection, run_id, node_id)
    }

    /// Read one exact node from the initial admitted run graph.
    ///
    /// # Errors
    /// Returns an error for missing or unsupported graph state, inconsistent node identity,
    /// invalid/oversized payloads, or database failures. A missing run or node returns `None`.
    pub fn run_graph_node(
        &self,
        run_id: &str,
        node_id: &str,
    ) -> Result<Option<NodeDefinition>, WorkflowStoreError> {
        initial_node(&self.connection, run_id, node_id)
    }

    /// Read a bounded page of initial edges without loading the full graph.
    ///
    /// # Errors
    /// Returns an error for unsupported revisions, invalid graph relationships or payloads,
    /// invalid identities, or database failures. Missing runs return an empty page.
    pub fn run_graph_edges(
        &self,
        run_id: &str,
        after_edge_id: Option<u64>,
        limit: usize,
    ) -> Result<Vec<RunGraphEdge>, WorkflowStoreError> {
        Self::graph_edge_page(&self.connection, run_id, None, after_edge_id, None, limit)
    }

    /// Read one exact edge from the initial run graph by its stable identity.
    ///
    /// Missing runs or edges return `None`.
    /// # Errors
    /// Returns an error for invalid identities, unsupported revisions, damaged edge
    /// relationships or payloads, or database failures.
    pub fn run_graph_edge(
        &self,
        run_id: &str,
        edge_id: u64,
    ) -> Result<Option<RunGraphEdge>, WorkflowStoreError> {
        let edge_id = i64::try_from(edge_id).map_err(|_| {
            WorkflowStoreError::InvalidData("edge identity exceeds storage range".to_string())
        })?;
        Ok(Self::graph_edge_page(&self.connection, run_id, None, None, Some(edge_id), 1)?.pop())
    }

    /// Read a bounded page of initial edges targeting one node, ordered by edge identity.
    ///
    /// Missing runs or targets with no incoming edges return an empty page.
    /// # Errors
    /// Returns an error for invalid identities or cursors, unsupported graph revisions,
    /// damaged edge relationships or payloads, or database failures.
    pub fn run_graph_incoming_edges(
        &self,
        run_id: &str,
        target_node_id: &str,
        after_edge_id: Option<u64>,
        limit: usize,
    ) -> Result<Vec<RunGraphEdge>, WorkflowStoreError> {
        super::validate_id("target_node_id", target_node_id)?;
        Self::graph_edge_page(
            &self.connection,
            run_id,
            Some(EdgeEndpoint::Target(target_node_id)),
            after_edge_id,
            None,
            limit,
        )
    }

    /// Read a bounded page of initial edges leaving a node, ordered by edge identity.
    ///
    /// Missing runs or sources with no outgoing edges return an empty page.
    /// # Errors
    /// Returns an error for invalid identities or cursors, unsupported graph revisions,
    /// damaged edge relationships or payloads, or database failures.
    pub fn run_graph_outgoing_edges(
        &self,
        run_id: &str,
        source_node_id: &str,
        after_edge_id: Option<u64>,
        limit: usize,
    ) -> Result<Vec<RunGraphEdge>, WorkflowStoreError> {
        super::validate_id("source_node_id", source_node_id)?;
        Self::graph_edge_page(
            &self.connection,
            run_id,
            Some(EdgeEndpoint::Source(source_node_id)),
            after_edge_id,
            None,
            limit,
        )
    }

    fn graph_edge_page(
        connection: &Connection,
        run_id: &str,
        endpoint: Option<EdgeEndpoint<'_>>,
        after_edge_id: Option<u64>,
        exact_edge_id: Option<i64>,
        limit: usize,
    ) -> Result<Vec<RunGraphEdge>, WorkflowStoreError> {
        let cursor_operator = if after_edge_id.is_some() { ">" } else { ">=" };
        let after_edge_id = edge_cursor(after_edge_id)?;
        let Some(revision) = graph_revision(connection, run_id)? else {
            return Ok(Vec::new());
        };
        if revision != 1 {
            return Err(WorkflowStoreError::InvalidData(
                "initial graph inspection does not support revised graphs".to_string(),
            ));
        }
        let identity_filter = if exact_edge_id.is_some() {
            "AND edge.edge_id = ?6"
        } else {
            "AND ?6 IS NULL"
        };
        let endpoint_index = match endpoint {
            Some(EdgeEndpoint::Source(_)) => "INDEXED BY workflow_run_graph_edges_source",
            Some(EdgeEndpoint::Target(_)) => "INDEXED BY workflow_run_graph_edges_target",
            None => "",
        };
        let (target_filter, endpoint_id) = match endpoint {
            Some(EdgeEndpoint::Source(id)) => ("AND edge.source_node_id = ?5", Some(id)),
            Some(EdgeEndpoint::Target(id)) => ("AND edge.target_node_id = ?5", Some(id)),
            None => ("AND ?5 IS NULL", None),
        };
        let sql = format!(
            "SELECT edge.edge_id,
                    CASE WHEN typeof(edge.edge_json) = 'text'
                         AND length(CAST(edge.edge_json AS BLOB)) <= ?4
                         AND NOT EXISTS (
                             SELECT 1 FROM workflow_run_graph_edges other
                             WHERE other.run_id = edge.run_id AND other.edge_id = edge.edge_id
                             AND other.revision > 1
                         )
                         THEN edge.edge_json END,
                    edge.source_node_id, edge.target_node_id,
                    CASE WHEN source.is_entry IN (0, 1) AND source.is_exit IN (0, 1)
                         AND NOT EXISTS (
                             SELECT 1 FROM workflow_run_graph_nodes other
                             WHERE other.run_id = source.run_id AND other.node_id = source.node_id
                             AND other.revision > 1
                         ) THEN source.node_id END,
                    CASE WHEN target.is_entry IN (0, 1) AND target.is_exit IN (0, 1)
                         AND NOT EXISTS (
                             SELECT 1 FROM workflow_run_graph_nodes other
                             WHERE other.run_id = target.run_id AND other.node_id = target.node_id
                             AND other.revision > 1
                         ) THEN target.node_id END, edge.revision
             FROM workflow_run_graph_edges edge {endpoint_index}
             LEFT JOIN workflow_run_graph_nodes source ON source.run_id = edge.run_id
                 AND source.node_id = edge.source_node_id AND source.revision = 1
             LEFT JOIN workflow_run_graph_nodes target ON target.run_id = edge.run_id
                 AND target.node_id = edge.target_node_id AND target.revision = 1
             WHERE edge.run_id = ?1
                 AND edge.edge_id {cursor_operator} ?2 {target_filter} {identity_filter}
             ORDER BY edge.edge_id, edge.revision LIMIT ?3",
        );
        let mut statement = connection.prepare(&sql)?;
        let mut rows = statement.query(rusqlite::params![
            run_id,
            after_edge_id,
            limit.clamp(1, GRAPH_PAGE_LIMIT),
            super::MAX_INLINE_JSON_BYTES,
            endpoint_id,
            exact_edge_id
        ])?;
        let mut edges = Vec::new();
        while let Some(row) = rows.next()? {
            if row.get::<_, i64>(6)? != 1 {
                return Err(WorkflowStoreError::InvalidData(
                    "workflow graph edge revision is invalid".to_string(),
                ));
            }
            let json = row.get::<_, Option<String>>(1)?.ok_or_else(|| {
                WorkflowStoreError::InvalidData(
                    "workflow graph edge payload is invalid or oversized".to_string(),
                )
            })?;
            let edge: EdgeDefinition = serde_json::from_str(&json)?;
            super::validate_id("source_node_id", &edge.from)?;
            super::validate_id("target_node_id", &edge.to)?;
            if row.get::<_, String>(2)? != edge.from
                || row.get::<_, String>(3)? != edge.to
                || row.get::<_, Option<String>>(4)?.is_none()
                || row.get::<_, Option<String>>(5)?.is_none()
            {
                return Err(WorkflowStoreError::InvalidData(
                    "workflow graph edge relationships are inconsistent".to_string(),
                ));
            }
            edges.push(RunGraphEdge {
                edge_id: row.get(0)?,
                edge,
            });
        }
        Ok(edges)
    }

    /// Read the admitted graph revision without replay or repair.
    ///
    /// # Errors
    /// Returns an error for an invalid run identity, missing graph for an existing run,
    /// corrupt revision, or database failure.
    pub fn run_graph_revision(&self, run_id: &str) -> Result<Option<u64>, WorkflowStoreError> {
        graph_revision(&self.connection, run_id)
    }

    /// Read a bounded page of initial graph nodes ordered by stable node identity.
    ///
    /// This initial materialization API does not authorize graph edits or change scheduling.
    /// # Errors
    /// Returns an error for invalid identities, missing/corrupt graph data, or database failure.
    pub fn run_graph_nodes(
        &self,
        run_id: &str,
        after_node_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RunGraphNode>, WorkflowStoreError> {
        if let Some(id) = after_node_id {
            super::validate_id("after_node_id", id)?;
        }
        let Some(revision) = self.run_graph_revision(run_id)? else {
            return Ok(Vec::new());
        };
        if revision != 1 {
            return Err(WorkflowStoreError::InvalidData(
                "initial graph inspection does not support revised graphs".to_string(),
            ));
        }
        let cursor_operator = if after_node_id.is_some() { ">" } else { ">=" };
        let sql = format!(
            "SELECT node_id, revision,
                    CASE WHEN typeof(node_json) = 'text'
                         AND length(CAST(node_json AS BLOB)) <= ?4
                         AND NOT EXISTS (
                             SELECT 1 FROM workflow_run_graph_nodes other
                             WHERE other.run_id = node.run_id AND other.node_id = node.node_id
                             AND other.revision > 1
                         ) THEN node_json END,
                    is_entry, is_exit
             FROM workflow_run_graph_nodes node WHERE run_id = ?1
             AND node_id {cursor_operator} ?2 ORDER BY node_id, revision LIMIT ?3",
        );
        let mut statement = self.connection.prepare(&sql)?;
        let mut rows = statement.query(rusqlite::params![
            run_id,
            after_node_id.unwrap_or(""),
            limit.clamp(1, GRAPH_PAGE_LIMIT),
            super::MAX_INLINE_JSON_BYTES
        ])?;
        let mut nodes = Vec::new();
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            super::validate_id("node_id", &id)?;
            let json = row.get::<_, Option<String>>(2)?.ok_or_else(|| {
                WorkflowStoreError::InvalidData(
                    "workflow graph node payload is invalid or oversized".to_string(),
                )
            })?;
            let node: NodeDefinition = serde_json::from_str(&json)?;
            let entry: i64 = row.get(3)?;
            let exit: i64 = row.get(4)?;
            let node_revision: u64 = row.get(1)?;
            if node_revision != 1 || !matches!(entry, 0 | 1) || !matches!(exit, 0 | 1) {
                return Err(WorkflowStoreError::InvalidData(
                    "workflow graph node revision or boundary flags are invalid".to_string(),
                ));
            }
            if node.id != id {
                return Err(WorkflowStoreError::InvalidData(
                    "workflow graph node identity mismatch".to_string(),
                ));
            }
            nodes.push(RunGraphNode {
                revision: node_revision,
                node,
                entry: entry == 1,
                exit: exit == 1,
            });
        }
        Ok(nodes)
    }
}
