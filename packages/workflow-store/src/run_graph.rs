//! Run-owned graph materialization and bounded inspection.

use super::{WorkflowStore, WorkflowStoreError};
use bcode_workflow::{EdgeDefinition, NodeDefinition, WorkflowDefinition};
use rusqlite::{Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

const GRAPH_PAGE_LIMIT: usize = 100;

#[cfg(test)]
mod reconciliation_tests {
    use super::*;

    #[test]
    fn affected_controller_requires_running_member_disposition() {
        let connection = Connection::open_in_memory().expect("database");
        connection.execute_batch(
            "CREATE TABLE workflow_activations (run_id TEXT, node_id TEXT, activation_id TEXT, status TEXT);
             CREATE TABLE workflow_fan_out_members (run_id TEXT, controller_node_id TEXT, member_activation_id TEXT, status TEXT);
             INSERT INTO workflow_fan_out_members VALUES ('run', 'controller', 'member', 'running');",
        ).expect("execution fixture");
        let mut request = bcode_workflow::WorkflowRunGraphEditBatch {
            version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
            run_id: "run".to_string(),
            mutation_id: "edit".to_string(),
            expected_revision: 1,
            edits: vec![bcode_workflow::WorkflowRunGraphEdit::RemoveNode {
                node_id: "controller".to_string(),
            }],
            reconciliation: vec![],
        };
        let before = connection.total_changes();
        assert!(validate_affected_work(&connection, &request, &[]).is_err());
        request
            .reconciliation
            .push(bcode_workflow::WorkflowRunGraphReconciliation::Retain {
                activation_id: "member".to_string(),
            });
        validate_affected_work(&connection, &request, &[]).expect("explicit member disposition");
        assert_eq!(before, connection.total_changes());
    }

    #[test]
    fn affected_dependencies_follow_current_and_new_edges_without_looping() {
        let edge = |from: &str, to: &str| EdgeDefinition {
            from: from.to_string(),
            to: to.to_string(),
            kind: bcode_workflow::EdgeKind::default(),
            transform: None,
        };
        let current = vec![
            RunGraphEdge {
                revision: 1,
                edge_id: 0,
                edge: edge("first", "second"),
            },
            RunGraphEdge {
                revision: 1,
                edge_id: 1,
                edge: edge("second", "third"),
            },
            RunGraphEdge {
                revision: 1,
                edge_id: 2,
                edge: edge("third", "first"),
            },
        ];
        let request = bcode_workflow::WorkflowRunGraphEditBatch {
            version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
            run_id: "run".to_string(),
            mutation_id: "edit".to_string(),
            expected_revision: 1,
            edits: vec![bcode_workflow::WorkflowRunGraphEdit::ReplaceEdge {
                edge_id: 1,
                edge: edge("second", "new"),
            }],
            reconciliation: vec![],
        };
        let mut affected = BTreeSet::from(["first"]);
        expand_affected_dependencies(&mut affected, &request, &current);
        assert_eq!(
            affected,
            BTreeSet::from(["first", "second", "third", "new"])
        );
    }
}

#[derive(Clone, Copy)]
pub enum EdgeEndpoint<'a> {
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

/// One immutable edge representation in an admitted run graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunGraphEdge {
    /// Revision at which this edge representation was admitted.
    pub revision: u64,
    /// Stable identity assigned during initial materialization.
    pub edge_id: u64,
    /// Exact admitted edge, including transforms and control flow.
    pub edge: EdgeDefinition,
}

pub fn initialize_retirement(connection: &Connection) -> Result<(), WorkflowStoreError> {
    connection.execute_batch(
        "ALTER TABLE workflow_run_graph_nodes ADD COLUMN retired_at_revision INTEGER
            CHECK (retired_at_revision > revision);
         ALTER TABLE workflow_run_graph_edges ADD COLUMN retired_at_revision INTEGER
            CHECK (retired_at_revision > revision);
         CREATE INDEX workflow_run_graph_nodes_retirement
            ON workflow_run_graph_nodes(run_id, retired_at_revision);
         CREATE INDEX workflow_run_graph_edges_retirement
            ON workflow_run_graph_edges(run_id, retired_at_revision);
         CREATE INDEX workflow_run_graph_nodes_live
            ON workflow_run_graph_nodes(run_id, node_id) WHERE retired_at_revision IS NULL;
         CREATE INDEX workflow_run_graph_edges_live
            ON workflow_run_graph_edges(run_id, edge_id) WHERE retired_at_revision IS NULL;",
    )?;
    Ok(())
}

pub fn initialize_edit_candidates(connection: &Connection) -> Result<(), WorkflowStoreError> {
    connection.execute_batch(
        "CREATE INDEX IF NOT EXISTS workflow_fan_out_running_controller
            ON workflow_fan_out_members(run_id, controller_node_id, member_activation_id)
            WHERE status = 'running';
        CREATE INDEX IF NOT EXISTS workflow_activations_running_node
            ON workflow_activations(run_id, node_id, activation_id) WHERE status = 'running';
        CREATE INDEX IF NOT EXISTS workflow_activations_identity
            ON workflow_activations(run_id, activation_id);
        CREATE TABLE IF NOT EXISTS workflow_activation_graph_bindings (
            run_id TEXT NOT NULL,
            node_id TEXT NOT NULL,
            activation_id TEXT NOT NULL,
            graph_revision INTEGER NOT NULL CHECK (graph_revision > 0),
            PRIMARY KEY (run_id, node_id, activation_id),
            FOREIGN KEY (run_id, node_id, activation_id)
                REFERENCES workflow_activations(run_id, node_id, activation_id)
        );
        CREATE TABLE IF NOT EXISTS workflow_graph_edit_candidates (
            run_id TEXT NOT NULL REFERENCES workflow_runs(run_id),
            mutation_id TEXT NOT NULL,
            expected_revision INTEGER NOT NULL CHECK (expected_revision > 0),
            request_json TEXT NOT NULL,
            authority_json TEXT NOT NULL,
            created_at_ms INTEGER NOT NULL,
            PRIMARY KEY (run_id, mutation_id)
        );
        CREATE TABLE IF NOT EXISTS workflow_graph_edit_publications (
            run_id TEXT NOT NULL,
            mutation_id TEXT NOT NULL,
            revision INTEGER NOT NULL CHECK (revision > 1),
            PRIMARY KEY (run_id, mutation_id),
            FOREIGN KEY (run_id, mutation_id)
                REFERENCES workflow_graph_edit_candidates(run_id, mutation_id)
        );
        CREATE TABLE IF NOT EXISTS workflow_retained_edge_bindings (
            run_id TEXT NOT NULL,
            mutation_id TEXT NOT NULL,
            graph_revision INTEGER NOT NULL CHECK (graph_revision > 1),
            edge_id INTEGER NOT NULL,
            edge_revision INTEGER NOT NULL,
            source_node_id TEXT NOT NULL,
            source_activation_id TEXT NOT NULL,
            PRIMARY KEY (run_id, graph_revision, edge_id),
            FOREIGN KEY (run_id, mutation_id)
                REFERENCES workflow_graph_edit_publications(run_id, mutation_id),
            FOREIGN KEY (run_id, edge_id, edge_revision)
                REFERENCES workflow_run_graph_edges(run_id, edge_id, revision),
            FOREIGN KEY (run_id, source_node_id, source_activation_id)
                REFERENCES workflow_activations(run_id, node_id, activation_id)
        );
        CREATE TABLE IF NOT EXISTS workflow_leaf_retentions (
            run_id TEXT NOT NULL,
            node_id TEXT NOT NULL,
            activation_id TEXT NOT NULL,
            revision INTEGER NOT NULL CHECK (revision > 1),
            PRIMARY KEY (run_id, node_id, activation_id, revision),
            FOREIGN KEY (run_id, node_id, activation_id)
                REFERENCES workflow_activations(run_id, node_id, activation_id)
        );
        CREATE TABLE IF NOT EXISTS workflow_graph_edit_validations (
            run_id TEXT NOT NULL,
            mutation_id TEXT NOT NULL,
            expected_revision INTEGER NOT NULL CHECK (expected_revision > 0),
            PRIMARY KEY (run_id, mutation_id),
            FOREIGN KEY (run_id, mutation_id)
                REFERENCES workflow_graph_edit_candidates(run_id, mutation_id)
        );
        CREATE TABLE IF NOT EXISTS workflow_graph_edit_nodes (
            run_id TEXT NOT NULL,
            mutation_id TEXT NOT NULL,
            node_id TEXT NOT NULL,
            node_json TEXT,
            is_entry INTEGER NOT NULL CHECK (is_entry IN (0, 1)),
            is_exit INTEGER NOT NULL CHECK (is_exit IN (0, 1)),
            PRIMARY KEY (run_id, mutation_id, node_id),
            FOREIGN KEY (run_id, mutation_id)
                REFERENCES workflow_graph_edit_validations(run_id, mutation_id)
        );
        CREATE TABLE IF NOT EXISTS workflow_graph_edit_edges (
            run_id TEXT NOT NULL,
            mutation_id TEXT NOT NULL,
            edge_id INTEGER NOT NULL CHECK (edge_id >= 0),
            edge_json TEXT,
            PRIMARY KEY (run_id, mutation_id, edge_id),
            FOREIGN KEY (run_id, mutation_id)
                REFERENCES workflow_graph_edit_validations(run_id, mutation_id)
        );",
    )?;
    Ok(())
}

/// Structural validation outcome; neither variant authorizes graph publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunGraphCandidateValidation {
    /// The bounded candidate graph passed domain structural validation.
    Validated,
    /// The graph exceeds one validation slice and requires incremental validation.
    RequiresIncrementalValidation,
}

impl WorkflowStore {
    /// Read an activation's explicitly recorded admission graph revision.
    ///
    /// Missing activations and historical admissions without this fact return `None`.
    /// The executable node revision is not a substitute for the admitted graph revision.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid identities, inconsistent or future bindings, missing
    /// executable data, or database failures. This bounded read never reconstructs history.
    pub fn activation_admitted_graph_revision(
        &self,
        run_id: &str,
        node_id: &str,
        activation_id: &str,
    ) -> Result<Option<u64>, WorkflowStoreError> {
        validate_activation_node_request(run_id, node_id, activation_id)?;
        let transaction = self
            .connection
            .is_autocommit()
            .then(|| self.connection.unchecked_transaction())
            .transpose()?;
        let revision = self
            .connection
            .query_row(
                "SELECT graph_revision FROM workflow_activation_graph_bindings
             WHERE run_id = ?1 AND node_id = ?2 AND activation_id = ?3",
                (run_id, node_id, activation_id),
                |row| row.get::<_, u64>(0),
            )
            .optional()?;
        if let Some(revision) = revision {
            let current = graph_revision(&self.connection, run_id)?.ok_or_else(|| {
                WorkflowStoreError::InvalidData("activation graph is missing".to_string())
            })?;
            let node = self
                .activation_graph_node(run_id, node_id, activation_id)?
                .ok_or_else(|| {
                    WorkflowStoreError::InvalidData("activation executable is missing".to_string())
                })?;
            if revision == 0 || revision > current || node.revision > revision {
                return Err(WorkflowStoreError::InvalidData(
                    "invalid activation admission graph revision".to_string(),
                ));
            }
        }
        if let Some(transaction) = transaction {
            transaction.commit()?;
        }
        Ok(revision)
    }

    /// Validate a persisted edit against one bounded snapshot of the committed graph.
    ///
    /// This validates structure, not execution reconciliation or caller permissions. Large graphs
    /// remain preserved and explicitly require incremental validation rather than being truncated.
    /// Successful validation is persisted for this candidate and graph revision, but never
    /// authorizes publication or bypasses subsequent ownership and reconciliation checks.
    ///
    /// # Errors
    ///
    /// Returns an error for stale ownership/revision, damaged state, invalid structural edits,
    /// missing candidates, or database failures.
    pub fn validate_staged_run_graph_edit(
        &self,
        run_id: &str,
        mutation_id: &str,
        authority: &super::WorkflowExecutionAuthority,
    ) -> Result<RunGraphCandidateValidation, WorkflowStoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        let result = self.validate_run_graph_edit_in_transaction(
            run_id,
            mutation_id,
            authority,
            &transaction,
        )?;
        transaction.commit()?;
        Ok(result)
    }

    fn validate_run_graph_edit_in_transaction(
        &self,
        run_id: &str,
        mutation_id: &str,
        authority: &super::WorkflowExecutionAuthority,
        transaction: &Transaction<'_>,
    ) -> Result<RunGraphCandidateValidation, WorkflowStoreError> {
        let request = self
            .staged_run_graph_edit(run_id, mutation_id, authority)?
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData("graph edit candidate not found".to_string())
            })?;
        ensure_run_accepts_graph_edits(transaction, run_id)?;
        validate_reconciliation_targets(transaction, &request)?;
        let page = self.current_run_graph_page(
            run_id,
            Some(request.expected_revision),
            None,
            None,
            GRAPH_PAGE_LIMIT,
        )?;
        if !page.nodes_complete || !page.edges_complete {
            return Ok(RunGraphCandidateValidation::RequiresIncrementalValidation);
        }
        validate_affected_work(transaction, &request, &page.edges)?;
        let payload: String = transaction.query_row(
            "SELECT CASE WHEN typeof(definition_json) = 'text'
             AND length(CAST(definition_json AS BLOB)) <= ?2 THEN definition_json END
             FROM workflow_definitions definition JOIN workflow_runs run
             ON definition.definition_id = run.definition_id AND definition.version = run.definition_version
             WHERE run.run_id = ?1",
            rusqlite::params![run_id, super::MAX_INLINE_JSON_BYTES], |row| row.get(0),
        )?;
        let mut graph: WorkflowDefinition = serde_json::from_str(&payload)?;
        graph.nodes.clear();
        graph.entries.clear();
        graph.exits.clear();
        for record in page.nodes {
            if record.entry {
                graph.entries.push(record.node.id.clone());
            }
            if record.exit {
                graph.exits.push(record.node.id.clone());
            }
            graph.nodes.insert(record.node.id.clone(), record.node);
        }
        let mut edges = page
            .edges
            .into_iter()
            .map(|record| (record.edge_id, record.edge))
            .collect::<std::collections::BTreeMap<_, _>>();
        for edit in &request.edits {
            apply_candidate_edit(&mut graph, &mut edges, edit)?;
        }
        graph.edges = edges.values().cloned().collect();
        validate_retained_bindings(transaction, &request, &graph, &edges)?;
        graph
            .validate()
            .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
        transaction.execute(
            "INSERT INTO workflow_graph_edit_validations (run_id, mutation_id, expected_revision)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (run_id, mutation_id) DO UPDATE
             SET expected_revision = excluded.expected_revision",
            rusqlite::params![run_id, mutation_id, request.expected_revision],
        )?;
        persist_candidate_delta(transaction, &request, &graph, &edges)?;
        Ok(RunGraphCandidateValidation::Validated)
    }

    /// Read a staged edit under current execution authority without publishing it.
    ///
    /// The stored envelope and provenance are validated in one snapshot. A transferred owner
    /// may inspect a predecessor's intent, but inspection does not authorize publication.
    ///
    /// # Errors
    ///
    /// Returns an error for stale ownership, invalid identities, unsupported or damaged candidate
    /// data, inconsistent indexed revision, or persistence failure. Reads never repair state.
    pub fn staged_run_graph_edit(
        &self,
        run_id: &str,
        mutation_id: &str,
        authority: &super::WorkflowExecutionAuthority,
    ) -> Result<Option<bcode_workflow::WorkflowRunGraphEditBatch>, WorkflowStoreError> {
        super::validate_id("run_id", run_id)?;
        super::validate_id("mutation_id", mutation_id)?;
        let transaction = self
            .connection
            .is_autocommit()
            .then(|| self.connection.unchecked_transaction())
            .transpose()?;
        self.verify_execution_authority(run_id, authority)?;
        let row = self
            .connection
            .query_row(
                "SELECT CASE WHEN typeof(request_json) = 'text'
                 AND length(CAST(request_json AS BLOB)) <= ?3 THEN request_json END,
                 CASE WHEN typeof(authority_json) = 'text'
                 AND length(CAST(authority_json AS BLOB)) <= ?3 THEN authority_json END,
                 expected_revision, created_at_ms
             FROM workflow_graph_edit_candidates WHERE run_id = ?1 AND mutation_id = ?2",
                rusqlite::params![run_id, mutation_id, super::MAX_INLINE_JSON_BYTES],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, u64>(2)?,
                        row.get::<_, u64>(3)?,
                    ))
                },
            )
            .optional()?;
        let request = row
            .map(|(payload, provenance, revision, _created_at_ms)| {
                let request: bcode_workflow::WorkflowRunGraphEditBatch =
                    serde_json::from_str(&payload)?;
                request
                    .validate()
                    .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
                let owner: super::WorkflowExecutionAuthority = serde_json::from_str(&provenance)?;
                super::validate_id("target_artifact_id", &owner.target_artifact_id)?;
                super::validate_id("daemon_instance_id", &owner.daemon_instance_id)?;
                super::validate_id("fencing_token", &owner.fencing_token)?;
                if owner.generation == 0
                    || request.run_id != run_id
                    || request.mutation_id != mutation_id
                    || request.expected_revision != revision
                {
                    return Err(WorkflowStoreError::InvalidData(
                        "inconsistent graph edit candidate".to_string(),
                    ));
                }
                Ok(request)
            })
            .transpose()?;
        if let Some(transaction) = transaction {
            transaction.commit()?;
        }
        Ok(request)
    }

    /// Publish a validated leaf-only graph while the run has no active activations.
    ///
    /// This deliberately rejects connected graphs and reconciliation requests until their
    /// settlement paths support published bindings. Returns the committed revision, including
    /// on duplicate delivery. The caller must authorize the edit before calling this method.
    ///
    /// # Errors
    ///
    /// Returns an error for stale authority/revision, active work, unsupported topology,
    /// invalid candidates, or persistence failure. Failed publication leaves no partial graph.
    pub fn publish_quiescent_run_graph_edit(
        &mut self,
        run_id: &str,
        mutation_id: &str,
        authority: &super::WorkflowExecutionAuthority,
        created_at_ms: u64,
    ) -> Result<u64, WorkflowStoreError> {
        self.publish_leaf_run_graph_edit(run_id, mutation_id, authority, created_at_ms, false, None)
    }

    /// Publish a leaf-only edit while explicitly retaining unchanged active activations.
    ///
    /// Admission bindings remain historical. Settlement consumes the publication's retained
    /// identities. Connected topology and edits to retained nodes remain unsupported.
    /// The caller must authorize the operation before invoking this method.
    ///
    /// # Errors
    ///
    /// Returns an error for stale authority/revision, missing retention, changed active nodes,
    /// unsupported topology, invalid candidates, or persistence failure.
    pub fn publish_retained_leaf_run_graph_edit(
        &mut self,
        run_id: &str,
        mutation_id: &str,
        authority: &super::WorkflowExecutionAuthority,
        created_at_ms: u64,
    ) -> Result<u64, WorkflowStoreError> {
        self.publish_leaf_run_graph_edit(run_id, mutation_id, authority, created_at_ms, true, None)
    }

    /// Publish a retained-leaf candidate on behalf of an authenticated active execution.
    ///
    /// The application must authorize publication separately from staging. Caller verification
    /// and publication share one transaction, preventing settlement from racing this check.
    ///
    /// # Errors
    /// Returns an error for mismatched or inactive callers, stale authority, invalid candidates,
    /// unsupported reconciliation/topology, or persistence failure.
    pub fn publish_retained_leaf_run_graph_edit_from_execution(
        &mut self,
        mutation_id: &str,
        authority: &super::WorkflowExecutionAuthority,
        caller: &super::WorkflowExecutionSessionLink,
        created_at_ms: u64,
    ) -> Result<u64, WorkflowStoreError> {
        self.publish_leaf_run_graph_edit(
            &caller.run_id,
            mutation_id,
            authority,
            created_at_ms,
            true,
            Some(caller),
        )
    }

    fn publish_leaf_run_graph_edit(
        &self,
        run_id: &str,
        mutation_id: &str,
        authority: &super::WorkflowExecutionAuthority,
        created_at_ms: u64,
        retain_active: bool,
        caller: Option<&super::WorkflowExecutionSessionLink>,
    ) -> Result<u64, WorkflowStoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        if let Some(caller) = caller {
            self.verify_active_graph_edit_caller(run_id, caller)?;
        }
        let request = self
            .staged_run_graph_edit(run_id, mutation_id, authority)?
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData("graph edit candidate not found".to_string())
            })?;
        let published: Option<u64> = transaction.query_row(
            "SELECT revision FROM workflow_graph_edit_publications WHERE run_id = ?1 AND mutation_id = ?2",
            (run_id, mutation_id), |row| row.get(0),
        ).optional()?;
        if let Some(revision) = published {
            transaction.commit()?;
            return Ok(revision);
        }
        ensure_run_accepts_graph_edits(&transaction, run_id)?;
        let active: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM workflow_activations WHERE run_id = ?1
             AND status NOT IN ('completed', 'failed', 'cancelled', 'skipped'))",
            [run_id],
            |row| row.get(0),
        )?;
        let retentions = if retain_active {
            validate_leaf_retention(&transaction, &request)?
        } else if active || !request.reconciliation.is_empty() {
            return Err(WorkflowStoreError::InvalidData(
                "publication requires execution reconciliation".to_string(),
            ));
        } else {
            Vec::new()
        };
        // Revalidate in this same transaction, never trusting a historical validation marker.
        if self.validate_run_graph_edit_in_transaction(
            run_id,
            mutation_id,
            authority,
            &transaction,
        )? != RunGraphCandidateValidation::Validated
        {
            return Err(WorkflowStoreError::InvalidData(
                "publication requires incremental graph validation".to_string(),
            ));
        }
        let connected: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM workflow_run_graph_edges edge
             WHERE edge.run_id = ?1 AND edge.retired_at_revision IS NULL
             AND NOT EXISTS(SELECT 1 FROM workflow_graph_edit_edges delta
                 WHERE delta.run_id = edge.run_id AND delta.mutation_id = ?2
                   AND delta.edge_id = edge.edge_id))
             OR EXISTS(SELECT 1 FROM workflow_graph_edit_edges WHERE run_id = ?1 AND mutation_id = ?2 AND edge_json IS NOT NULL)",
            (run_id, mutation_id), |row| row.get(0),
        )?;
        if connected {
            return Err(WorkflowStoreError::InvalidData(
                "publication requires binding-aware successor settlement".to_string(),
            ));
        }
        let revision = request.expected_revision + 1;
        persist_leaf_retentions(&transaction, run_id, revision, &retentions)?;
        publish_candidate_edges(&transaction, run_id, mutation_id, revision)?;
        transaction.execute(
            "UPDATE workflow_run_graph_nodes SET retired_at_revision = ?3
             WHERE run_id = ?1 AND retired_at_revision IS NULL AND node_id IN
             (SELECT node_id FROM workflow_graph_edit_nodes WHERE run_id = ?1 AND mutation_id = ?2)",
            rusqlite::params![run_id, mutation_id, revision],
        )?;
        transaction.execute(
            "INSERT INTO workflow_run_graph_nodes (run_id, node_id, revision, node_json, is_entry, is_exit)
             SELECT run_id, node_id, ?3, node_json, is_entry, is_exit FROM workflow_graph_edit_nodes
             WHERE run_id = ?1 AND mutation_id = ?2 AND node_json IS NOT NULL",
            rusqlite::params![run_id, mutation_id, revision],
        )?;
        if transaction.execute(
            "UPDATE workflow_run_graphs SET revision = ?2 WHERE run_id = ?1 AND revision = ?3",
            rusqlite::params![run_id, revision, request.expected_revision],
        )? != 1
        {
            return Err(WorkflowStoreError::InvalidData(
                "workflow graph revision conflict".to_string(),
            ));
        }
        transaction.execute(
            "INSERT INTO workflow_graph_edit_publications (run_id, mutation_id, revision) VALUES (?1, ?2, ?3)",
            rusqlite::params![run_id, mutation_id, revision],
        )?;
        persist_retained_edge_bindings(&transaction, &request, revision)?;
        super::append_event(
            &transaction,
            run_id,
            "graph_edit_published",
            &serde_json::json!({"mutation_id": mutation_id, "revision": revision}).to_string(),
            created_at_ms,
        )?;
        transaction.commit()?;
        Ok(revision)
    }

    /// Durably stage a live graph edit without publishing executable topology.
    ///
    /// Returns `true` for a new candidate and `false` for identical duplicate delivery.
    /// Candidates preserve their admitting authority and do not authorize execution or bypass
    /// structural validation. Publication and reconciliation are separate required operations.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid or oversized requests, stale ownership or revision,
    /// conflicting duplicate identity, terminal/cancelling runs, or persistence failure.
    pub fn stage_run_graph_edit(
        &mut self,
        request: &bcode_workflow::WorkflowRunGraphEditBatch,
        authority: &super::WorkflowExecutionAuthority,
        created_at_ms: u64,
    ) -> Result<bool, WorkflowStoreError> {
        self.stage_run_graph_edit_for_caller(request, authority, created_at_ms, None)
    }

    /// Stage a candidate only while its exact calling execution session remains active.
    ///
    /// The application must authenticate the caller and authorize the edit before invoking this
    /// operation. A session link alone does not grant permission. Link and active-attempt checks
    /// share the candidate transaction so settlement cannot race with admission.
    ///
    /// # Errors
    /// Returns an error for a mismatched session link, inactive attempt, stale authority,
    /// invalid candidate, conflicting duplicate/revision, or persistence failure.
    pub fn stage_run_graph_edit_from_execution(
        &mut self,
        request: &bcode_workflow::WorkflowRunGraphEditBatch,
        authority: &super::WorkflowExecutionAuthority,
        caller: &super::WorkflowExecutionSessionLink,
        created_at_ms: u64,
    ) -> Result<bool, WorkflowStoreError> {
        self.stage_run_graph_edit_for_caller(request, authority, created_at_ms, Some(caller))
    }

    // Both callers hold a transaction on this connection through their eventual commit.
    fn verify_active_graph_edit_caller(
        &self,
        run_id: &str,
        caller: &super::WorkflowExecutionSessionLink,
    ) -> Result<(), WorkflowStoreError> {
        super::validate_execution_session_link(caller)?;
        let stored = self.execution_session_link(
            &caller.run_id,
            &caller.node_id,
            &caller.activation_id,
            caller.attempt,
        )?;
        if caller.run_id != run_id || stored.as_ref() != Some(caller) {
            return Err(WorkflowStoreError::InvalidData(
                "run edit caller link mismatch".to_string(),
            ));
        }
        let active: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM workflow_attempts attempt
             JOIN workflow_activations activation ON activation.run_id = attempt.run_id
               AND activation.node_id = attempt.node_id AND activation.activation_id = attempt.activation_id
             WHERE attempt.run_id = ?1 AND attempt.node_id = ?2 AND attempt.activation_id = ?3
               AND attempt.attempt = ?4 AND activation.status = 'running'
               AND attempt.status IN ('prepared', 'admitted'))",
            rusqlite::params![caller.run_id, caller.node_id, caller.activation_id, caller.attempt],
            |row| row.get(0),
        )?;
        if !active {
            return Err(WorkflowStoreError::InvalidData(
                "run edit caller is no longer active".to_string(),
            ));
        }
        Ok(())
    }

    fn stage_run_graph_edit_for_caller(
        &self,
        request: &bcode_workflow::WorkflowRunGraphEditBatch,
        authority: &super::WorkflowExecutionAuthority,
        created_at_ms: u64,
        caller: Option<&super::WorkflowExecutionSessionLink>,
    ) -> Result<bool, WorkflowStoreError> {
        request
            .validate()
            .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
        super::validate_id("run_id", &request.run_id)?;
        super::validate_id("mutation_id", &request.mutation_id)?;
        let payload = super::bounded_json("graph edit candidate", request)?;
        let authority_json = super::bounded_json("graph edit authority", authority)?;
        let transaction = self.connection.unchecked_transaction()?;
        self.verify_execution_authority(&request.run_id, authority)?;
        if let Some(caller) = caller {
            self.verify_active_graph_edit_caller(&request.run_id, caller)?;
        }
        if let Some(existing) =
            self.staged_run_graph_edit(&request.run_id, &request.mutation_id, authority)?
        {
            if existing != *request {
                return Err(WorkflowStoreError::InvalidData(
                    "conflicting graph edit mutation identity".to_string(),
                ));
            }
            transaction.commit()?;
            return Ok(false);
        }
        if graph_revision(&transaction, &request.run_id)? != Some(request.expected_revision) {
            return Err(WorkflowStoreError::InvalidData(
                "workflow graph revision conflict".to_string(),
            ));
        }
        ensure_run_accepts_graph_edits(&transaction, &request.run_id)?;
        transaction.execute(
            "INSERT INTO workflow_graph_edit_candidates
             (run_id, mutation_id, expected_revision, request_json, authority_json, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                request.run_id,
                request.mutation_id,
                request.expected_revision,
                payload,
                authority_json,
                created_at_ms
            ],
        )?;
        super::append_event(&transaction, &request.run_id, "graph_edit_staged",
            &serde_json::json!({"mutation_id": request.mutation_id, "expected_revision": request.expected_revision}).to_string(), created_at_ms)?;
        transaction.commit()?;
        Ok(true)
    }
}

fn ensure_run_accepts_graph_edits(
    connection: &Connection,
    run_id: &str,
) -> Result<(), WorkflowStoreError> {
    let accepts: bool = connection.query_row(
        "SELECT status IN ('running', 'paused') AND cancellation_requested_at_ms IS NULL
         FROM workflow_runs WHERE run_id = ?1",
        [run_id],
        |row| row.get(0),
    )?;
    if !accepts {
        return Err(WorkflowStoreError::InvalidData(
            "run does not accept graph edits".to_string(),
        ));
    }
    Ok(())
}

fn validate_affected_work(
    connection: &Connection,
    request: &bcode_workflow::WorkflowRunGraphEditBatch,
    current_edges: &[RunGraphEdge],
) -> Result<(), WorkflowStoreError> {
    use bcode_workflow::{
        WorkflowRunGraphEdit as Edit, WorkflowRunGraphReconciliation as Reconciliation,
    };
    let mut affected = BTreeSet::new();
    for edit in &request.edits {
        match edit {
            Edit::AddNode { node, .. } | Edit::ReplaceNode { node, .. } => {
                affected.insert(node.id.as_str());
            }
            Edit::RemoveNode { node_id } => {
                affected.insert(node_id.as_str());
            }
            Edit::AddEdge { edge, .. } => {
                affected.insert(edge.to.as_str());
            }
            Edit::ReplaceEdge { edge_id, edge } => {
                affected.insert(edge.to.as_str());
                if let Some(old) = current_edges.iter().find(|old| old.edge_id == *edge_id) {
                    affected.insert(old.edge.to.as_str());
                }
            }
            Edit::RemoveEdge { edge_id } => {
                if let Some(old) = current_edges.iter().find(|old| old.edge_id == *edge_id) {
                    affected.insert(old.edge.to.as_str());
                }
            }
        }
    }
    expand_affected_dependencies(&mut affected, request, current_edges);
    let dispositions: BTreeSet<_> = request
        .reconciliation
        .iter()
        .map(|item| {
            let (Reconciliation::Retain { activation_id }
            | Reconciliation::RetainWithBindings { activation_id, .. }
            | Reconciliation::Cancel { activation_id }) = item;
            activation_id.as_str()
        })
        .collect();
    for node_id in affected {
        let mut statement = connection.prepare(
            "SELECT activation_id FROM workflow_activations
             WHERE run_id = ?1 AND node_id = ?2 AND status = 'running'
             UNION ALL
             SELECT member_activation_id FROM workflow_fan_out_members
             WHERE run_id = ?1 AND controller_node_id = ?2 AND status = 'running'
             LIMIT ?3",
        )?;
        let mut rows = statement.query(rusqlite::params![
            request.run_id,
            node_id,
            bcode_workflow::MAX_WORKFLOW_RUN_GRAPH_EDITS + 1
        ])?;
        while let Some(row) = rows.next()? {
            let identity: String = row.get(0)?;
            if !dispositions.contains(identity.as_str()) {
                return Err(WorkflowStoreError::InvalidData(
                    "affected running activation requires explicit graph reconciliation"
                        .to_string(),
                ));
            }
        }
    }
    Ok(())
}

fn expand_affected_dependencies<'a>(
    affected: &mut BTreeSet<&'a str>,
    request: &'a bcode_workflow::WorkflowRunGraphEditBatch,
    current_edges: &'a [RunGraphEdge],
) {
    use bcode_workflow::WorkflowRunGraphEdit as Edit;
    let mut dependencies = std::collections::BTreeMap::<&str, BTreeSet<&str>>::new();
    for edge in
        current_edges
            .iter()
            .map(|record| &record.edge)
            .chain(request.edits.iter().filter_map(|edit| match edit {
                Edit::AddEdge { edge, .. } | Edit::ReplaceEdge { edge, .. } => Some(edge),
                _ => None,
            }))
    {
        dependencies
            .entry(edge.from.as_str())
            .or_default()
            .insert(edge.to.as_str());
    }
    let mut pending: Vec<_> = affected.iter().copied().collect();
    while let Some(node) = pending.pop() {
        if let Some(targets) = dependencies.get(node) {
            for &target in targets {
                if affected.insert(target) {
                    pending.push(target);
                }
            }
        }
    }
}

pub fn retained_edge_activation(
    connection: &Connection,
    run_id: &str,
    graph_revision: u64,
    edge: &RunGraphEdge,
) -> Result<Option<String>, WorkflowStoreError> {
    let binding: Option<Option<String>> = connection
        .query_row(
            "SELECT CASE WHEN binding.edge_revision = ?4
             AND binding.source_node_id = ?5 AND publication.revision = binding.graph_revision
             AND typeof(binding.source_activation_id) = 'text'
             AND length(CAST(binding.source_activation_id AS BLOB)) <= 256
             THEN binding.source_activation_id END
         FROM workflow_retained_edge_bindings binding
         LEFT JOIN workflow_graph_edit_publications publication
           ON publication.run_id = binding.run_id AND publication.mutation_id = binding.mutation_id
         WHERE binding.run_id = ?1 AND binding.graph_revision = ?2 AND binding.edge_id = ?3",
            rusqlite::params![
                run_id,
                graph_revision,
                edge.edge_id,
                edge.revision,
                edge.edge.from
            ],
            |row| row.get(0),
        )
        .optional()?;
    binding
        .map(|identity| {
            let identity = identity.ok_or_else(|| {
                WorkflowStoreError::InvalidData(
                    "retained edge binding disagrees with committed publication or edge"
                        .to_string(),
                )
            })?;
            super::validate_id("activation_id", &identity)?;
            Ok(identity)
        })
        .transpose()
}

pub fn persist_retained_edge_bindings(
    transaction: &Transaction<'_>,
    request: &bcode_workflow::WorkflowRunGraphEditBatch,
    revision: u64,
) -> Result<(), WorkflowStoreError> {
    for disposition in &request.reconciliation {
        let bcode_workflow::WorkflowRunGraphReconciliation::RetainWithBindings {
            activation_id,
            edge_ids,
        } = disposition
        else {
            continue;
        };
        for edge_id in edge_ids {
            let inserted = transaction.execute(
                "INSERT INTO workflow_retained_edge_bindings
                 (run_id, mutation_id, graph_revision, edge_id, edge_revision,
                  source_node_id, source_activation_id)
                 SELECT edge.run_id, ?2, ?3, edge.edge_id, edge.revision,
                        edge.source_node_id, activation.activation_id
                 FROM workflow_run_graph_edges edge
                 JOIN workflow_activations activation ON activation.run_id = edge.run_id
                   AND activation.node_id = edge.source_node_id AND activation.activation_id = ?5
                 WHERE edge.run_id = ?1 AND edge.edge_id = ?4
                   AND edge.revision <= ?3 AND edge.retired_at_revision IS NULL",
                rusqlite::params![
                    request.run_id,
                    request.mutation_id,
                    revision,
                    edge_id,
                    activation_id
                ],
            )?;
            if inserted != 1 {
                return Err(WorkflowStoreError::InvalidData(
                    "retained edge publication requires one exact source and committed edge"
                        .to_string(),
                ));
            }
        }
    }
    Ok(())
}

fn persist_leaf_retentions(
    transaction: &Transaction<'_>,
    run_id: &str,
    revision: u64,
    retentions: &[(String, String)],
) -> Result<(), WorkflowStoreError> {
    for (node_id, activation_id) in retentions {
        transaction.execute(
            "INSERT INTO workflow_leaf_retentions (run_id, node_id, activation_id, revision)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![run_id, node_id, activation_id, revision],
        )?;
    }
    Ok(())
}

fn validate_leaf_retention(
    connection: &Connection,
    request: &bcode_workflow::WorkflowRunGraphEditBatch,
) -> Result<Vec<(String, String)>, WorkflowStoreError> {
    let mut retentions = Vec::new();
    let mut retained = BTreeSet::new();
    for disposition in &request.reconciliation {
        let bcode_workflow::WorkflowRunGraphReconciliation::Retain { activation_id } = disposition
        else {
            return Err(WorkflowStoreError::InvalidData(
                "leaf publication supports only explicit retention".to_string(),
            ));
        };
        retained.insert(activation_id.as_str());
    }
    let mut statement = connection.prepare(
        "SELECT node_id, activation_id FROM workflow_activations WHERE run_id = ?1
         AND status NOT IN ('completed', 'failed', 'cancelled', 'skipped') LIMIT ?2",
    )?;
    let mut rows = statement.query(rusqlite::params![
        request.run_id,
        bcode_workflow::MAX_WORKFLOW_RUN_GRAPH_EDITS + 1
    ])?;
    while let Some(row) = rows.next()? {
        let node_id: String = row.get(0)?;
        let activation_id: String = row.get(1)?;
        let changed: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM workflow_graph_edit_nodes WHERE run_id = ?1 AND mutation_id = ?2 AND node_id = ?3)",
            (&request.run_id, &request.mutation_id, &node_id), |row| row.get(0),
        )?;
        if changed || !retained.remove(activation_id.as_str()) {
            return Err(WorkflowStoreError::InvalidData(
                "leaf publication requires explicit retention of every unchanged active node"
                    .to_string(),
            ));
        }
        reconciled_activation_exit(connection, &request.run_id, &node_id, &activation_id)?;
        retentions.push((node_id, activation_id));
    }
    if !retained.is_empty() {
        return Err(WorkflowStoreError::InvalidData(
            "retention does not identify active leaf work".to_string(),
        ));
    }
    Ok(retentions)
}

pub fn validate_reconciliation_targets(
    connection: &Connection,
    request: &bcode_workflow::WorkflowRunGraphEditBatch,
) -> Result<(), WorkflowStoreError> {
    use bcode_workflow::WorkflowRunGraphReconciliation as Reconciliation;
    for disposition in &request.reconciliation {
        let (Reconciliation::Retain { activation_id }
        | Reconciliation::RetainWithBindings { activation_id, .. }
        | Reconciliation::Cancel { activation_id }) = disposition;
        let mut statement = connection.prepare(
            "SELECT status, output_id, node_id FROM workflow_activations
             WHERE run_id = ?1 AND activation_id = ?2 LIMIT 2",
        )?;
        let mut rows = statement.query((&request.run_id, activation_id))?;
        let valid = if let Some(row) = rows.next()? {
            let status: String = row.get(0)?;
            let output: Option<String> = row.get(1)?;
            if status == "completed"
                && output.is_some()
                && matches!(disposition, Reconciliation::RetainWithBindings { .. })
            {
                let node_id: String = row.get(2)?;
                super::activation_output_value_by_identity(
                    connection,
                    &request.run_id,
                    &node_id,
                    activation_id,
                )?;
                true
            } else {
                output.is_none()
                    && matches!(
                        status.as_str(),
                        "pending"
                            | "running"
                            | "waiting_input"
                            | "waiting_approval"
                            | "waiting_mutation_approval"
                    )
            }
        } else {
            false
        };
        if !valid || rows.next()?.is_some() {
            return Err(WorkflowStoreError::InvalidData(
                "graph reconciliation requires an unambiguous active activation or explicitly bound completed result in this run"
                    .to_string(),
            ));
        }
    }
    Ok(())
}

fn publish_candidate_edges(
    transaction: &Transaction<'_>,
    run_id: &str,
    mutation_id: &str,
    revision: u64,
) -> Result<(), WorkflowStoreError> {
    transaction.execute(
        "UPDATE workflow_run_graph_edges SET retired_at_revision = ?3
         WHERE run_id = ?1 AND retired_at_revision IS NULL AND edge_id IN
         (SELECT edge_id FROM workflow_graph_edit_edges WHERE run_id = ?1 AND mutation_id = ?2)",
        rusqlite::params![run_id, mutation_id, revision],
    )?;
    transaction.execute(
        "INSERT INTO workflow_run_graph_edges
         (run_id, edge_id, revision, source_node_id, target_node_id, edge_json)
         SELECT run_id, edge_id, ?3, json_extract(edge_json, '$.from'),
                json_extract(edge_json, '$.to'), edge_json
         FROM workflow_graph_edit_edges
         WHERE run_id = ?1 AND mutation_id = ?2 AND edge_json IS NOT NULL",
        rusqlite::params![run_id, mutation_id, revision],
    )?;
    Ok(())
}

fn validate_retained_bindings(
    connection: &Connection,
    request: &bcode_workflow::WorkflowRunGraphEditBatch,
    graph: &WorkflowDefinition,
    edges: &std::collections::BTreeMap<u64, EdgeDefinition>,
) -> Result<(), WorkflowStoreError> {
    for disposition in &request.reconciliation {
        let bcode_workflow::WorkflowRunGraphReconciliation::RetainWithBindings {
            activation_id,
            edge_ids,
        } = disposition
        else {
            continue;
        };
        let (source_id, completed): (String, bool) = connection.query_row(
            "SELECT node_id, status = 'completed' FROM workflow_activations WHERE run_id = ?1 AND activation_id = ?2",
            (&request.run_id, activation_id),
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let completed_value = completed
            .then(|| {
                super::activation_output_value_by_identity(
                    connection,
                    &request.run_id,
                    &source_id,
                    activation_id,
                )
            })
            .transpose()?;
        let source = bound_activation_node(connection, &request.run_id, &source_id, activation_id)?
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData("binding source is missing".to_string())
            })?;
        for edge_id in edge_ids {
            let edge = edges.get(edge_id).ok_or_else(|| {
                WorkflowStoreError::InvalidData(
                    "binding edge is missing from candidate".to_string(),
                )
            })?;
            let target = graph.nodes.get(&edge.to).ok_or_else(|| {
                WorkflowStoreError::InvalidData(
                    "binding target is missing from candidate".to_string(),
                )
            })?;
            // Exact schema equality is a conservative proof. Transform-aware compatibility
            // requires a separate proof and must not be inferred from a node identity.
            if edge.from != source_id || edge.transform.is_some() || source.output != target.input {
                return Err(WorkflowStoreError::InvalidData(
                    "retained-result binding lacks compatible source and target schemas"
                        .to_string(),
                ));
            }
            if let Some(value) = &completed_value {
                super::validate_json_schema(
                    "retained-result target input",
                    &target.input.schema,
                    value,
                )
                .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
            }
        }
    }
    Ok(())
}

fn persist_candidate_delta(
    transaction: &Transaction<'_>,
    request: &bcode_workflow::WorkflowRunGraphEditBatch,
    graph: &WorkflowDefinition,
    edges: &std::collections::BTreeMap<u64, EdgeDefinition>,
) -> Result<(), WorkflowStoreError> {
    use bcode_workflow::WorkflowRunGraphEdit as Edit;
    let mut nodes = BTreeSet::new();
    let mut edge_ids = BTreeSet::new();
    for edit in &request.edits {
        match edit {
            Edit::AddNode { node, .. } | Edit::ReplaceNode { node, .. } => {
                nodes.insert(node.id.as_str());
            }
            Edit::RemoveNode { node_id } => {
                nodes.insert(node_id.as_str());
            }
            Edit::AddEdge { edge_id, .. }
            | Edit::ReplaceEdge { edge_id, .. }
            | Edit::RemoveEdge { edge_id } => {
                edge_ids.insert(*edge_id);
            }
        }
    }
    for node_id in nodes {
        let payload = graph
            .nodes
            .get(node_id)
            .map(|node| super::bounded_json("validated graph node", node))
            .transpose()?;
        transaction.execute(
            "INSERT INTO workflow_graph_edit_nodes
             (run_id, mutation_id, node_id, node_json, is_entry, is_exit)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(run_id, mutation_id, node_id) DO UPDATE SET
             node_json = excluded.node_json, is_entry = excluded.is_entry, is_exit = excluded.is_exit",
            rusqlite::params![request.run_id, request.mutation_id, node_id, payload,
                graph.entries.iter().any(|id| id == node_id), graph.exits.iter().any(|id| id == node_id)],
        )?;
    }
    for edge_id in edge_ids {
        let payload = edges
            .get(&edge_id)
            .map(|edge| super::bounded_json("validated graph edge", edge))
            .transpose()?;
        transaction.execute(
            "INSERT INTO workflow_graph_edit_edges (run_id, mutation_id, edge_id, edge_json)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(run_id, mutation_id, edge_id) DO UPDATE SET edge_json = excluded.edge_json",
            rusqlite::params![request.run_id, request.mutation_id, edge_id, payload],
        )?;
    }
    Ok(())
}

fn apply_candidate_edit(
    graph: &mut WorkflowDefinition,
    edges: &mut std::collections::BTreeMap<u64, EdgeDefinition>,
    edit: &bcode_workflow::WorkflowRunGraphEdit,
) -> Result<(), WorkflowStoreError> {
    use bcode_workflow::WorkflowRunGraphEdit as Edit;
    let invalid =
        || WorkflowStoreError::InvalidData("graph edit identity precondition failed".to_string());
    match edit {
        Edit::AddNode { node, entry, exit } | Edit::ReplaceNode { node, entry, exit } => {
            if graph.nodes.contains_key(&node.id) != matches!(edit, Edit::ReplaceNode { .. }) {
                return Err(invalid());
            }
            graph.entries.retain(|id| id != &node.id);
            graph.exits.retain(|id| id != &node.id);
            if *entry {
                graph.entries.push(node.id.clone());
            }
            if *exit {
                graph.exits.push(node.id.clone());
            }
            graph.nodes.insert(node.id.clone(), node.clone());
        }
        Edit::RemoveNode { node_id } => {
            graph.nodes.remove(node_id).ok_or_else(invalid)?;
            graph.entries.retain(|id| id != node_id);
            graph.exits.retain(|id| id != node_id);
        }
        Edit::AddEdge { edge_id, edge } | Edit::ReplaceEdge { edge_id, edge } => {
            if edges.contains_key(edge_id) != matches!(edit, Edit::ReplaceEdge { .. }) {
                return Err(invalid());
            }
            edges.insert(*edge_id, edge.clone());
        }
        Edit::RemoveEdge { edge_id } => {
            edges.remove(edge_id).ok_or_else(invalid)?;
        }
    }
    Ok(())
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
    initialize_source_index(connection)?;
    initialize_retirement(connection)
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
        Some(Some(revision)) if revision > 0 => {
            let future: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM workflow_run_graph_nodes
                     WHERE run_id = ?1 AND retired_at_revision > ?2)
                 OR EXISTS(SELECT 1 FROM workflow_run_graph_edges
                     WHERE run_id = ?1 AND retired_at_revision > ?2)",
                (run_id, revision),
                |row| row.get(0),
            )?;
            if future {
                return Err(WorkflowStoreError::InvalidData(
                    "retirement exceeds committed graph revision".to_string(),
                ));
            }
            Ok(Some(revision))
        }
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

pub fn initial_activation_node(
    connection: &Connection,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
) -> Result<Option<NodeDefinition>, WorkflowStoreError> {
    validate_activation_node_request(run_id, node_id, activation_id)?;
    let transaction = connection
        .is_autocommit()
        .then(|| connection.unchecked_transaction())
        .transpose()?;
    let node = initial_activation_node_in_snapshot(connection, run_id, node_id, activation_id)?;
    if let Some(transaction) = transaction {
        transaction.commit()?;
    }
    Ok(node)
}

fn initial_activation_node_in_snapshot(
    connection: &Connection,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
) -> Result<Option<NodeDefinition>, WorkflowStoreError> {
    super::validate_id("activation_id", activation_id)?;
    let Some(record) = initial_node_record(connection, run_id, node_id)? else {
        return Ok(None);
    };
    let admitted_revision = connection
        .query_row(
            "SELECT graph_revision FROM workflow_activation_graph_bindings
         WHERE run_id = ?1 AND node_id = ?2 AND activation_id = ?3",
            (run_id, node_id, activation_id),
            |row| row.get::<_, u64>(0),
        )
        .optional()?;
    if admitted_revision.is_some_and(|revision| revision != 1) {
        return Err(WorkflowStoreError::InvalidData(
            "activation admission binding does not match initial graph".to_string(),
        ));
    }
    let node = bound_activation_node(connection, run_id, node_id, activation_id)?;
    if node.as_ref() != Some(&record.node) {
        return Err(WorkflowStoreError::InvalidData(
            "activation executable binding does not match initial graph".to_string(),
        ));
    }
    Ok(node)
}

pub fn bound_activation_node(
    connection: &Connection,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
) -> Result<Option<NodeDefinition>, WorkflowStoreError> {
    activation_node_record(connection, run_id, node_id, activation_id)?
        .map(|record| Some(record.node))
        .ok_or_else(|| {
            WorkflowStoreError::InvalidData("activation executable binding is missing".to_string())
        })
}

fn validate_activation_node_request(
    run_id: &str,
    node_id: &str,
    activation_id: &str,
) -> Result<(), WorkflowStoreError> {
    super::validate_id("activation_id", activation_id)?;
    super::validate_id("node_id", node_id)?;
    super::validate_id("run_id", run_id)
}

fn activation_node_record(
    connection: &Connection,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
) -> Result<Option<RunGraphNode>, WorkflowStoreError> {
    validate_activation_node_request(run_id, node_id, activation_id)?;
    let transaction = connection
        .is_autocommit()
        .then(|| connection.unchecked_transaction())
        .transpose()?;
    let node = activation_node_record_in_snapshot(connection, run_id, node_id, activation_id)?;
    if let Some(transaction) = transaction {
        transaction.commit()?;
    }
    Ok(node)
}

fn activation_node_record_in_snapshot(
    connection: &Connection,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
) -> Result<Option<RunGraphNode>, WorkflowStoreError> {
    validate_activation_node_request(run_id, node_id, activation_id)?;
    let revision = connection
        .query_row(
            "SELECT node_revision FROM workflow_activations
         WHERE run_id = ?1 AND node_id = ?2 AND activation_id = ?3",
            (run_id, node_id, activation_id),
            |row| row.get::<_, u64>(0),
        )
        .optional()?;
    let Some(revision) = revision else {
        return Ok(None);
    };
    WorkflowStore::node_revision(connection, run_id, node_id, revision)?
        .map(Some)
        .ok_or_else(|| {
            WorkflowStoreError::InvalidData("activation executable revision is missing".to_string())
        })
}

pub fn revised_leaf_exit(
    connection: &Connection,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
) -> Result<bool, WorkflowStoreError> {
    let exit = reconciled_activation_exit(connection, run_id, node_id, activation_id)?;
    let has_edges: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM workflow_run_graph_edges
         WHERE run_id = ?1 AND source_node_id = ?2 AND retired_at_revision IS NULL)",
        (run_id, node_id),
        |row| row.get(0),
    )?;
    if has_edges {
        return Err(WorkflowStoreError::InvalidData(
            "revised successor settlement requires execution reconciliation".to_string(),
        ));
    }
    Ok(exit)
}

fn reconciled_activation_exit(
    connection: &Connection,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
) -> Result<bool, WorkflowStoreError> {
    let current = graph_revision(connection, run_id)?.ok_or_else(|| {
        WorkflowStoreError::InvalidData("settlement graph is missing".to_string())
    })?;
    let admitted = connection
        .query_row(
            "SELECT graph_revision FROM workflow_activation_graph_bindings
         WHERE run_id = ?1 AND node_id = ?2 AND activation_id = ?3",
            (run_id, node_id, activation_id),
            |row| row.get::<_, u64>(0),
        )
        .optional()?;
    if admitted != Some(current) {
        let retained: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM workflow_leaf_retentions
             WHERE run_id = ?1 AND node_id = ?2 AND activation_id = ?3 AND revision = ?4)",
            rusqlite::params![run_id, node_id, activation_id, current],
            |row| row.get(0),
        )?;
        if admitted.is_none_or(|revision| revision == 0 || revision > current) || !retained {
            return Err(WorkflowStoreError::InvalidData(
                "settlement requires reconciliation with the current graph".to_string(),
            ));
        }
    }
    let node =
        activation_node_record(connection, run_id, node_id, activation_id)?.ok_or_else(|| {
            WorkflowStoreError::InvalidData("settlement binding is missing".to_string())
        })?;
    let latest: (u64, Option<u64>) = connection.query_row(
        "SELECT revision, retired_at_revision FROM workflow_run_graph_nodes
         WHERE run_id = ?1 AND node_id = ?2 ORDER BY revision DESC LIMIT 1",
        (run_id, node_id),
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if latest != (node.revision, None) {
        return Err(WorkflowStoreError::InvalidData(
            "revised successor settlement requires execution reconciliation".to_string(),
        ));
    }
    Ok(node.exit)
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

#[cfg(test)]
pub fn initial_edge_between(
    transaction: &Transaction<'_>,
    run_id: &str,
    source_node_id: &str,
    target_node_id: &str,
) -> Result<Option<EdgeDefinition>, WorkflowStoreError> {
    super::validate_id("target_node_id", target_node_id)?;
    let mut cursor = None;
    let mut selected = None;
    loop {
        let edges = initial_outgoing_edges(transaction, run_id, source_node_id, cursor)?;
        if edges.is_empty() {
            return Ok(selected);
        }
        cursor = edges.last().map(|edge| edge.edge_id);
        let final_page = edges.len() < GRAPH_PAGE_LIMIT;
        for record in edges
            .into_iter()
            .filter(|record| record.edge.to == target_node_id)
        {
            if selected.replace(record.edge).is_some() {
                return Err(WorkflowStoreError::InvalidData(
                    "successor input requires an unambiguous edge binding".to_string(),
                ));
            }
        }
        if final_page {
            return Ok(selected);
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
    super::validate_id("run_id", run_id)?;
    super::validate_id("node_id", node_id)?;
    let transaction = connection
        .is_autocommit()
        .then(|| connection.unchecked_transaction())
        .transpose()?;
    let node = initial_node_record_in_snapshot(connection, run_id, node_id)?;
    if let Some(transaction) = transaction {
        transaction.commit()?;
    }
    Ok(node)
}

fn initial_node_record_in_snapshot(
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

/// One consistent, bounded page of a run-owned graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunGraphPage {
    /// Committed graph revision shared by every field.
    pub revision: u64,
    /// Current node representations in identity order.
    pub nodes: Vec<RunGraphNode>,
    /// Current edges in identity order.
    pub edges: Vec<RunGraphEdge>,
    /// No nodes remain after this page.
    pub nodes_complete: bool,
    /// No edges remain after this page.
    pub edges_complete: bool,
}

impl WorkflowStore {
    fn validate_graph_page_request(
        run_id: &str,
        expected_revision: Option<u64>,
        after_node_id: Option<&str>,
        after_edge_id: Option<u64>,
        limit: usize,
    ) -> Result<(), WorkflowStoreError> {
        super::validate_id("run_id", run_id)?;
        if limit == 0
            || expected_revision
                .is_some_and(|revision| revision == 0 || i64::try_from(revision).is_err())
        {
            return Err(WorkflowStoreError::InvalidData(
                "invalid graph page limit or revision".to_string(),
            ));
        }
        if let Some(cursor) = after_node_id {
            super::validate_id("node cursor", cursor)?;
        }
        edge_cursor(after_edge_id)?;
        if expected_revision.is_none() && (after_node_id.is_some() || after_edge_id.is_some()) {
            return Err(WorkflowStoreError::InvalidData(
                "graph continuation requires an expected revision".to_string(),
            ));
        }
        Ok(())
    }

    /// Read nodes, edges, and continuation status from one read snapshot.
    ///
    /// First-page discovery may omit the expected revision. Continuation cursors
    /// require the revision returned by the preceding page to avoid mixing plans.
    ///
    /// # Errors
    /// Returns an error for missing graphs, revision conflicts, invalid cursors or
    /// limits, damaged graph data, or database failures.
    pub fn current_run_graph_page(
        &self,
        run_id: &str,
        expected_revision: Option<u64>,
        after_node_id: Option<&str>,
        after_edge_id: Option<u64>,
        limit: usize,
    ) -> Result<RunGraphPage, WorkflowStoreError> {
        Self::validate_graph_page_request(
            run_id,
            expected_revision,
            after_node_id,
            after_edge_id,
            limit,
        )?;
        let transaction = self
            .connection
            .is_autocommit()
            .then(|| self.connection.unchecked_transaction())
            .transpose()?;
        let revision = graph_revision(&self.connection, run_id)?.ok_or_else(|| {
            WorkflowStoreError::InvalidData("workflow run graph not found".to_string())
        })?;
        if expected_revision.is_some_and(|expected| expected != revision) {
            return Err(WorkflowStoreError::InvalidData(
                "workflow graph revision conflict".to_string(),
            ));
        }
        let nodes =
            self.current_run_graph_nodes_in_snapshot(run_id, revision, after_node_id, limit)?;
        let edges = Self::current_run_graph_edges_in_snapshot(
            &self.connection,
            run_id,
            revision,
            after_edge_id,
            limit,
            None,
        )?;
        let nodes_complete = match nodes.last() {
            Some(last) => self
                .current_run_graph_nodes_in_snapshot(run_id, revision, Some(&last.node.id), 1)?
                .is_empty(),
            None => true,
        };
        let edges_complete = match edges.last() {
            Some(last) => Self::current_run_graph_edges_in_snapshot(
                &self.connection,
                run_id,
                revision,
                Some(last.edge_id),
                1,
                None,
            )?
            .is_empty(),
            None => true,
        };
        if let Some(transaction) = transaction {
            transaction.commit()?;
        }
        Ok(RunGraphPage {
            revision,
            nodes,
            edges,
            nodes_complete,
            edges_complete,
        })
    }

    /// Read the immutable executable node selected when an activation was admitted.
    ///
    /// Binding and executable data are read from one snapshot, reusing an existing
    /// transaction when the caller already owns one.
    ///
    /// # Errors
    /// Returns an error for invalid identities, missing or malformed revision bindings,
    /// missing executable data, or database failures. An absent activation returns `None`.
    pub fn activation_graph_node(
        &self,
        run_id: &str,
        node_id: &str,
        activation_id: &str,
    ) -> Result<Option<RunGraphNode>, WorkflowStoreError> {
        activation_node_record(&self.connection, run_id, node_id, activation_id)
    }

    /// Read an exact immutable node revision rather than the current graph topology.
    ///
    /// This does not authorize execution or resolve an activation's binding. Missing
    /// runs or node revisions return `None`; an uncommitted revision is rejected.
    /// Graph metadata and executable data are read from one snapshot. An existing
    /// caller-owned transaction is reused and is neither committed nor rolled back
    /// by this method, including when the read fails.
    ///
    /// # Errors
    /// Returns an error for invalid identities/revisions, damaged graph metadata,
    /// malformed or oversized node payloads, or database failures.
    pub fn run_graph_node_revision(
        &self,
        run_id: &str,
        node_id: &str,
        revision: u64,
    ) -> Result<Option<RunGraphNode>, WorkflowStoreError> {
        Self::validate_node_revision_request(run_id, node_id, revision)?;
        let transaction = self
            .connection
            .is_autocommit()
            .then(|| self.connection.unchecked_transaction())
            .transpose()?;
        let node = Self::node_revision(&self.connection, run_id, node_id, revision)?;
        if let Some(transaction) = transaction {
            transaction.commit()?;
        }
        Ok(node)
    }

    fn validate_node_revision_request(
        run_id: &str,
        node_id: &str,
        revision: u64,
    ) -> Result<(), WorkflowStoreError> {
        super::validate_id("run_id", run_id)?;
        super::validate_id("node_id", node_id)?;
        if revision == 0 || i64::try_from(revision).is_err() {
            return Err(WorkflowStoreError::InvalidData(
                "node revision must be a positive storage integer".to_string(),
            ));
        }
        Ok(())
    }

    fn node_revision(
        connection: &Connection,
        run_id: &str,
        node_id: &str,
        revision: u64,
    ) -> Result<Option<RunGraphNode>, WorkflowStoreError> {
        Self::validate_node_revision_request(run_id, node_id, revision)?;
        let Some(current) = graph_revision(connection, run_id)? else {
            return Ok(None);
        };
        if revision > current {
            return Err(WorkflowStoreError::InvalidData(
                "node revision exceeds committed graph revision".to_string(),
            ));
        }
        let row = connection
            .query_row(
                "SELECT CASE WHEN typeof(node_json) = 'text'
                         AND length(CAST(node_json AS BLOB)) <= ?4
                         AND is_entry IN (0, 1) AND is_exit IN (0, 1)
                         THEN node_json END, is_entry, is_exit
             FROM workflow_run_graph_nodes
             WHERE run_id = ?1 AND node_id = ?2 AND revision = ?3",
                rusqlite::params![run_id, node_id, revision, super::MAX_INLINE_JSON_BYTES],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, bool>(1)?,
                        row.get::<_, bool>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((payload, entry, exit)) = row else {
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
            revision,
            node,
            entry,
            exit,
        }))
    }

    /// Read the latest committed representation of a run-owned node.
    ///
    /// Unlike an activation binding, this lookup follows node revisions. It does not
    /// authorize dispatch. Missing runs or nodes return `None`.
    /// Revision selection and executable data are read from one snapshot. An
    /// existing caller-owned transaction is reused; its completion remains the
    /// caller's responsibility on both success and failure.
    ///
    /// # Errors
    /// Returns an error for invalid identities, uncommitted revisions, damaged graph
    /// metadata, malformed or oversized executable data, or database failures.
    pub fn current_run_graph_node(
        &self,
        run_id: &str,
        node_id: &str,
    ) -> Result<Option<RunGraphNode>, WorkflowStoreError> {
        super::validate_id("run_id", run_id)?;
        super::validate_id("node_id", node_id)?;
        let transaction = self
            .connection
            .is_autocommit()
            .then(|| self.connection.unchecked_transaction())
            .transpose()?;
        let node = Self::current_run_graph_node_in_snapshot(&self.connection, run_id, node_id)?;
        if let Some(transaction) = transaction {
            transaction.commit()?;
        }
        Ok(node)
    }

    pub(super) fn current_run_graph_node_in_snapshot(
        connection: &Connection,
        run_id: &str,
        node_id: &str,
    ) -> Result<Option<RunGraphNode>, WorkflowStoreError> {
        super::validate_id("node_id", node_id)?;
        let Some(current) = graph_revision(connection, run_id)? else {
            return Ok(None);
        };
        let revision = connection
            .query_row(
                "SELECT revision, retired_at_revision FROM workflow_run_graph_nodes
             WHERE run_id = ?1 AND node_id = ?2 ORDER BY revision DESC LIMIT 1",
                (run_id, node_id),
                |row| Ok((row.get::<_, u64>(0)?, row.get::<_, Option<u64>>(1)?)),
            )
            .optional()?;
        let Some((revision, retired)) = revision else {
            return Ok(None);
        };
        if revision > current || retired.is_some_and(|end| end > current || end <= revision) {
            return Err(WorkflowStoreError::InvalidData(
                "node revision exceeds committed graph revision".to_string(),
            ));
        }
        if retired.is_some() {
            return Ok(None);
        }
        Self::node_revision(connection, run_id, node_id, revision)
    }

    /// Read a bounded page of current nodes at an expected graph revision.
    ///
    /// Resume with the last returned node ID and the same expected revision. A
    /// concurrent graph edit invalidates that cursor rather than mixing plans.
    /// Each page contains at most 100 nodes and uses one read snapshot.
    ///
    /// # Errors
    /// Returns an error for invalid cursors or limits, missing or changed graph
    /// state, invalid executable data, or database failures.
    pub fn current_run_graph_nodes(
        &self,
        run_id: &str,
        expected_revision: u64,
        after_node_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RunGraphNode>, WorkflowStoreError> {
        Self::validate_graph_page_request(
            run_id,
            Some(expected_revision),
            after_node_id,
            None,
            limit,
        )?;
        let transaction = self
            .connection
            .is_autocommit()
            .then(|| self.connection.unchecked_transaction())
            .transpose()?;
        let nodes = self.current_run_graph_nodes_in_snapshot(
            run_id,
            expected_revision,
            after_node_id,
            limit,
        )?;
        if let Some(transaction) = transaction {
            transaction.commit()?;
        }
        Ok(nodes)
    }

    fn current_run_graph_nodes_in_snapshot(
        &self,
        run_id: &str,
        expected_revision: u64,
        after_node_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RunGraphNode>, WorkflowStoreError> {
        if limit == 0 || expected_revision == 0 {
            return Err(WorkflowStoreError::InvalidData(
                "graph page limit and expected revision must be positive".to_string(),
            ));
        }
        if let Some(cursor) = after_node_id {
            super::validate_id("node cursor", cursor)?;
        }
        let transaction = &self.connection;
        if graph_revision(transaction, run_id)? != Some(expected_revision) {
            return Err(WorkflowStoreError::InvalidData(
                "graph page expected revision does not match committed graph".to_string(),
            ));
        }
        let mut nodes = Vec::new();
        let mut cursor = after_node_id.map(str::to_string);
        for _ in 0..limit.min(GRAPH_PAGE_LIMIT) {
            let id = transaction
                .query_row(
                    if cursor.is_some() {
                        "SELECT node_id FROM workflow_run_graph_nodes
                         WHERE run_id = ?1 AND node_id > ?2
                         AND retired_at_revision IS NULL
                         ORDER BY node_id LIMIT 1"
                    } else {
                        "SELECT node_id FROM workflow_run_graph_nodes
                         WHERE run_id = ?1 AND node_id >= ?2
                         AND retired_at_revision IS NULL
                         ORDER BY node_id LIMIT 1"
                    },
                    (run_id, cursor.as_deref().unwrap_or("")),
                    |row| {
                        let id = row.get_ref(0)?.as_str()?;
                        super::validate_id("node_id", id).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                0,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        })?;
                        Ok(id.to_string())
                    },
                )
                .optional()?;
            let Some(id) = id else {
                break;
            };
            nodes.push(
                Self::current_run_graph_node_in_snapshot(&self.connection, run_id, &id)?
                    .ok_or_else(|| {
                        WorkflowStoreError::InvalidData("graph page node is missing".to_string())
                    })?,
            );
            cursor = Some(id);
        }
        Ok(nodes)
    }

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
    /// invalid identities, zero limits, or database failures. Missing runs return an empty page.
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

    /// Read an immutable edge revision from a committed run graph.
    ///
    /// This inspection does not authorize dispatch. Missing runs or revisions
    /// return `None`; endpoint identities must exist in the same read snapshot.
    /// An existing caller-owned transaction is reused and remains owned by the
    /// caller on both success and failure.
    ///
    /// # Errors
    /// Returns an error for invalid identities or revisions, uncommitted data,
    /// malformed or oversized payloads, inconsistent endpoints, or database failures.
    pub fn run_graph_edge_revision(
        &self,
        run_id: &str,
        edge_id: u64,
        revision: u64,
    ) -> Result<Option<RunGraphEdge>, WorkflowStoreError> {
        Self::validate_edge_revision_request(run_id, edge_id, revision)?;
        let transaction = self
            .connection
            .is_autocommit()
            .then(|| self.connection.unchecked_transaction())
            .transpose()?;
        let edge =
            Self::run_graph_edge_revision_in_snapshot(&self.connection, run_id, edge_id, revision)?;
        if let Some(transaction) = transaction {
            transaction.commit()?;
        }
        Ok(edge)
    }

    fn validate_edge_revision_request(
        run_id: &str,
        edge_id: u64,
        revision: u64,
    ) -> Result<i64, WorkflowStoreError> {
        super::validate_id("run_id", run_id)?;
        let edge_id = i64::try_from(edge_id).map_err(|_| {
            WorkflowStoreError::InvalidData("edge identity exceeds storage range".to_string())
        })?;
        if revision == 0 || i64::try_from(revision).is_err() {
            return Err(WorkflowStoreError::InvalidData(
                "invalid edge revision".to_string(),
            ));
        }
        Ok(edge_id)
    }

    fn run_graph_edge_revision_in_snapshot(
        connection: &Connection,
        run_id: &str,
        edge_id: u64,
        revision: u64,
    ) -> Result<Option<RunGraphEdge>, WorkflowStoreError> {
        let edge_id = Self::validate_edge_revision_request(run_id, edge_id, revision)?;
        let transaction = connection;
        let Some(current) = graph_revision(transaction, run_id)? else {
            return Ok(None);
        };
        if revision > current {
            return Err(WorkflowStoreError::InvalidData(
                "edge revision exceeds committed graph revision".to_string(),
            ));
        }
        let row = transaction
            .query_row(
                "SELECT CASE WHEN typeof(edge_json) = 'text'
             AND length(CAST(edge_json AS BLOB)) <= ?4 THEN edge_json END,
             source_node_id, target_node_id FROM workflow_run_graph_edges
             WHERE run_id = ?1 AND edge_id = ?2 AND revision = ?3",
                rusqlite::params![run_id, edge_id, revision, super::MAX_INLINE_JSON_BYTES],
                |row| {
                    for column in [1, 2] {
                        super::validate_id("edge endpoint", row.get_ref(column)?.as_str()?)
                            .map_err(|error| {
                                rusqlite::Error::FromSqlConversionFailure(
                                    column,
                                    rusqlite::types::Type::Text,
                                    Box::new(error),
                                )
                            })?;
                    }
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((payload, source, target)) = row else {
            return Ok(None);
        };
        let payload = payload.ok_or_else(|| {
            WorkflowStoreError::InvalidData("invalid or oversized edge payload".to_string())
        })?;
        let edge: EdgeDefinition = serde_json::from_str(&payload)?;
        if edge.from != source || edge.to != target {
            return Err(WorkflowStoreError::InvalidData(
                "edge endpoint identity mismatch".to_string(),
            ));
        }
        // A self-loop has one endpoint identity; validate it once in this snapshot.
        let endpoints = [&source, &target];
        for id in &endpoints[..if source == target { 1 } else { 2 }] {
            super::validate_id("edge endpoint", id)?;
            let endpoint_revision = transaction
                .query_row(
                    "SELECT revision, retired_at_revision FROM workflow_run_graph_nodes
                 WHERE run_id = ?1 AND node_id = ?2 AND revision <= ?3
                 ORDER BY revision DESC LIMIT 1",
                    (run_id, id, revision),
                    |row| Ok((row.get::<_, u64>(0)?, row.get::<_, Option<u64>>(1)?)),
                )
                .optional()?
                .ok_or_else(|| {
                    WorkflowStoreError::InvalidData(
                        "edge endpoint is missing at revision".to_string(),
                    )
                })?;
            let (endpoint_revision, retired) = endpoint_revision;
            if retired.is_some_and(|end| end <= revision) {
                return Err(WorkflowStoreError::InvalidData(
                    "edge endpoint was retired at edge revision".to_string(),
                ));
            }
            Self::node_revision(connection, run_id, id, endpoint_revision)?.ok_or_else(|| {
                WorkflowStoreError::InvalidData("edge endpoint is missing".to_string())
            })?;
        }
        Ok(Some(RunGraphEdge {
            revision,
            edge_id: u64::try_from(edge_id).map_err(|_| {
                WorkflowStoreError::InvalidData("invalid edge identity".to_string())
            })?,
            edge,
        }))
    }

    /// Read the latest committed edge and validate its current endpoints.
    ///
    /// Missing runs or edges return `None`. This bounded read does not authorize
    /// dispatch. Caller-owned transactions are reused and left to the caller.
    ///
    /// # Errors
    /// Returns an error for invalid identities, uncommitted revisions, damaged
    /// graph metadata, malformed payloads, invalid endpoints, or database failures.
    pub fn current_run_graph_edge(
        &self,
        run_id: &str,
        edge_id: u64,
    ) -> Result<Option<RunGraphEdge>, WorkflowStoreError> {
        super::validate_id("run_id", run_id)?;
        i64::try_from(edge_id).map_err(|_| {
            WorkflowStoreError::InvalidData("edge identity exceeds storage range".to_string())
        })?;
        let transaction = self
            .connection
            .is_autocommit()
            .then(|| self.connection.unchecked_transaction())
            .transpose()?;
        let edge = Self::current_run_graph_edge_in_snapshot(
            &self.connection,
            run_id,
            edge_id,
            &mut BTreeSet::new(),
        )?;
        if let Some(transaction) = transaction {
            transaction.commit()?;
        }
        Ok(edge)
    }

    fn current_run_graph_edge_in_snapshot(
        connection: &Connection,
        run_id: &str,
        edge_id: u64,
        validated_endpoints: &mut BTreeSet<String>,
    ) -> Result<Option<RunGraphEdge>, WorkflowStoreError> {
        let id = i64::try_from(edge_id).map_err(|_| {
            WorkflowStoreError::InvalidData("edge identity exceeds storage range".to_string())
        })?;
        let Some(current) = graph_revision(connection, run_id)? else {
            return Ok(None);
        };
        let revision = connection.query_row(
            "SELECT revision, retired_at_revision FROM workflow_run_graph_edges WHERE run_id = ?1 AND edge_id = ?2 ORDER BY revision DESC LIMIT 1",
            (run_id, id), |row| Ok((row.get::<_, u64>(0)?, row.get::<_, Option<u64>>(1)?)),
        ).optional()?;
        let Some((revision, retired)) = revision else {
            return Ok(None);
        };
        if revision > current || retired.is_some_and(|end| end > current || end <= revision) {
            return Err(WorkflowStoreError::InvalidData(
                "edge revision exceeds committed graph revision".to_string(),
            ));
        }
        if retired.is_some() {
            return Ok(None);
        }
        let edge =
            Self::run_graph_edge_revision_in_snapshot(connection, run_id, edge_id, revision)?
                .ok_or_else(|| {
                    WorkflowStoreError::InvalidData("graph edge is missing".to_string())
                })?;
        for endpoint in [&edge.edge.from, &edge.edge.to] {
            if validated_endpoints.contains(endpoint) {
                continue;
            }
            Self::current_run_graph_node_in_snapshot(connection, run_id, endpoint)?.ok_or_else(
                || {
                    WorkflowStoreError::InvalidData(
                        "current graph edge endpoint is missing".to_string(),
                    )
                },
            )?;
            validated_endpoints.insert(endpoint.clone());
        }
        Ok(Some(edge))
    }

    /// Read current edges in bounded identity order at an expected graph revision.
    ///
    /// # Errors
    /// Returns an error for invalid limits/cursors, changed or missing graph state,
    /// uncommitted revisions, invalid edge data, or database failures.
    pub fn current_run_graph_edges(
        &self,
        run_id: &str,
        expected_revision: u64,
        after_edge_id: Option<u64>,
        limit: usize,
    ) -> Result<Vec<RunGraphEdge>, WorkflowStoreError> {
        Self::validate_graph_page_request(
            run_id,
            Some(expected_revision),
            None,
            after_edge_id,
            limit,
        )?;
        let transaction = self
            .connection
            .is_autocommit()
            .then(|| self.connection.unchecked_transaction())
            .transpose()?;
        let edges = Self::current_run_graph_edges_in_snapshot(
            &self.connection,
            run_id,
            expected_revision,
            after_edge_id,
            limit,
            None,
        )?;
        if let Some(transaction) = transaction {
            transaction.commit()?;
        }
        Ok(edges)
    }

    /// Read a bounded page of current edges incident to one source or target.
    ///
    /// `outgoing` selects source edges; otherwise target edges are returned.
    /// This query does not authorize execution or resolve activation bindings.
    ///
    /// # Errors
    /// Returns an error for invalid identities, limits, revision conflicts,
    /// inconsistent endpoints, malformed edge revisions, or database failures.
    pub fn current_run_graph_incident_edges(
        &self,
        run_id: &str,
        expected_revision: u64,
        node_id: &str,
        outgoing: bool,
        after_edge_id: Option<u64>,
        limit: usize,
    ) -> Result<Vec<RunGraphEdge>, WorkflowStoreError> {
        super::validate_id("node_id", node_id)?;
        Self::validate_graph_page_request(
            run_id,
            Some(expected_revision),
            None,
            after_edge_id,
            limit,
        )?;
        let transaction = self
            .connection
            .is_autocommit()
            .then(|| self.connection.unchecked_transaction())
            .transpose()?;
        let endpoint = if outgoing {
            EdgeEndpoint::Source(node_id)
        } else {
            EdgeEndpoint::Target(node_id)
        };
        let edges = Self::current_run_graph_edges_in_snapshot(
            &self.connection,
            run_id,
            expected_revision,
            after_edge_id,
            limit,
            Some(endpoint),
        )?;
        if let Some(transaction) = transaction {
            transaction.commit()?;
        }
        Ok(edges)
    }

    pub(super) fn current_run_graph_edges_in_snapshot(
        connection: &Connection,
        run_id: &str,
        expected_revision: u64,
        after_edge_id: Option<u64>,
        limit: usize,
        endpoint: Option<EdgeEndpoint<'_>>,
    ) -> Result<Vec<RunGraphEdge>, WorkflowStoreError> {
        let mut cursor = edge_cursor(after_edge_id)?;
        if limit == 0 || expected_revision == 0 {
            return Err(WorkflowStoreError::InvalidData(
                "invalid graph page limit or revision".to_string(),
            ));
        }
        let transaction = connection;
        if graph_revision(transaction, run_id)? != Some(expected_revision) {
            return Err(WorkflowStoreError::InvalidData(
                "graph page revision conflict".to_string(),
            ));
        }
        let mut edges = Vec::new();
        // Current endpoint validity is shared only within this bounded read snapshot.
        let mut validated_endpoints = BTreeSet::new();
        for _ in 0..limit.min(GRAPH_PAGE_LIMIT) {
            let inclusive = after_edge_id.is_none() && edges.is_empty();
            let comparison = if inclusive { ">=" } else { ">" };
            let (filter, node_id) = match endpoint {
                Some(EdgeEndpoint::Source(id)) => ("AND source_node_id = ?3", id),
                Some(EdgeEndpoint::Target(id)) => ("AND target_node_id = ?3", id),
                None => ("AND ?3 IS NOT NULL", ""),
            };
            let sql = format!(
                "SELECT edge_id FROM workflow_run_graph_edges
                 WHERE run_id = ?1 AND edge_id {comparison} ?2
                 AND retired_at_revision IS NULL {filter}
                 ORDER BY edge_id LIMIT 1"
            );
            let row = transaction
                .query_row(&sql, (run_id, cursor, node_id), |row| row.get::<_, u64>(0))
                .optional()?;
            let Some(id) = row else {
                break;
            };
            let edge = Self::current_run_graph_edge_in_snapshot(
                connection,
                run_id,
                id,
                &mut validated_endpoints,
            )?
            .ok_or_else(|| WorkflowStoreError::InvalidData("graph edge is missing".to_string()))?;
            edges.push(edge);
            cursor = i64::try_from(id).map_err(|_| {
                WorkflowStoreError::InvalidData("invalid edge identity".to_string())
            })?;
        }
        Ok(edges)
    }

    /// Read a bounded page of initial edges targeting one node, ordered by edge identity.
    ///
    /// Missing runs or targets with no incoming edges return an empty page.
    /// # Errors
    /// Returns an error for invalid identities or cursors, zero limits, unsupported graph revisions,
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
    /// Returns an error for invalid identities or cursors, zero limits, unsupported graph revisions,
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
        Self::validate_graph_page_request(run_id, Some(1), None, after_edge_id, limit)?;
        let transaction = connection
            .is_autocommit()
            .then(|| connection.unchecked_transaction())
            .transpose()?;
        let edges = Self::graph_edge_page_in_snapshot(
            connection,
            run_id,
            endpoint,
            after_edge_id,
            exact_edge_id,
            limit,
        )?;
        if let Some(transaction) = transaction {
            transaction.commit()?;
        }
        Ok(edges)
    }

    fn graph_edge_page_in_snapshot(
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
        let endpoint_index = Self::initial_edge_endpoint_index(endpoint);
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
                         ) THEN 1 END,
                    CASE WHEN target.is_entry IN (0, 1) AND target.is_exit IN (0, 1)
                         AND NOT EXISTS (
                             SELECT 1 FROM workflow_run_graph_nodes other
                             WHERE other.run_id = target.run_id AND other.node_id = target.node_id
                             AND other.revision > 1
                         ) THEN 1 END, edge.revision
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
        // Snapshot-local and bounded by at most two endpoint identities per edge.
        let mut validated_endpoints = BTreeSet::new();
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
            if row.get_ref(2)?.as_str().map_err(rusqlite::Error::from)? != edge.from
                || row.get_ref(3)?.as_str().map_err(rusqlite::Error::from)? != edge.to
                || row.get::<_, Option<i64>>(4)? != Some(1)
                || row.get::<_, Option<i64>>(5)? != Some(1)
            {
                return Err(WorkflowStoreError::InvalidData(
                    "workflow graph edge relationships are inconsistent".to_string(),
                ));
            }
            Self::validate_initial_edge_endpoints(
                connection,
                run_id,
                &edge,
                &mut validated_endpoints,
            )?;
            edges.push(RunGraphEdge {
                revision: 1,
                edge_id: row.get(0)?,
                edge,
            });
        }
        Ok(edges)
    }

    const fn initial_edge_endpoint_index(endpoint: Option<EdgeEndpoint<'_>>) -> &'static str {
        match endpoint {
            Some(EdgeEndpoint::Source(_)) => "INDEXED BY workflow_run_graph_edges_source",
            Some(EdgeEndpoint::Target(_)) => "INDEXED BY workflow_run_graph_edges_target",
            None => "",
        }
    }

    fn validate_initial_edge_endpoints(
        connection: &Connection,
        run_id: &str,
        edge: &EdgeDefinition,
        validated: &mut BTreeSet<String>,
    ) -> Result<(), WorkflowStoreError> {
        for endpoint in [&edge.from, &edge.to] {
            if validated.contains(endpoint) {
                continue;
            }
            initial_node_record_in_snapshot(connection, run_id, endpoint)?.ok_or_else(|| {
                WorkflowStoreError::InvalidData(
                    "initial graph edge endpoint is missing".to_string(),
                )
            })?;
            validated.insert(endpoint.clone());
        }
        Ok(())
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
    /// Returns an error for invalid identities, zero limits, missing/corrupt graph data, or database failure.
    pub fn run_graph_nodes(
        &self,
        run_id: &str,
        after_node_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RunGraphNode>, WorkflowStoreError> {
        Self::validate_graph_page_request(run_id, Some(1), after_node_id, None, limit)?;
        let transaction = self
            .connection
            .is_autocommit()
            .then(|| self.connection.unchecked_transaction())
            .transpose()?;
        let nodes = self.initial_graph_nodes_in_snapshot(run_id, after_node_id, limit)?;
        if let Some(transaction) = transaction {
            transaction.commit()?;
        }
        Ok(nodes)
    }

    fn initial_graph_nodes_in_snapshot(
        &self,
        run_id: &str,
        after_node_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RunGraphNode>, WorkflowStoreError> {
        if limit == 0 {
            return Err(WorkflowStoreError::InvalidData(
                "graph page limit must be positive".to_string(),
            ));
        }
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
            let id = row.get_ref(0)?.as_str().map_err(rusqlite::Error::from)?;
            super::validate_id("node_id", id)?;
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

#[cfg(test)]
mod publication_tests {
    use super::*;

    #[test]
    fn edge_publication_replaces_adds_removes_and_rolls_back() {
        let mut connection = Connection::open_in_memory().expect("database");
        connection
            .execute_batch(
                r#"CREATE TABLE workflow_run_graph_edges (
                run_id TEXT, edge_id INTEGER, revision INTEGER,
                source_node_id TEXT NOT NULL, target_node_id TEXT NOT NULL,
                edge_json TEXT, retired_at_revision INTEGER,
                PRIMARY KEY (run_id, edge_id, revision));
             CREATE TABLE workflow_graph_edit_edges (
                run_id TEXT, mutation_id TEXT, edge_id INTEGER, edge_json TEXT);
             INSERT INTO workflow_run_graph_edges VALUES
                ('run', 1, 1, 'a', 'b', '{}', NULL),
                ('run', 2, 1, 'b', 'c', '{}', NULL);
             INSERT INTO workflow_graph_edit_edges VALUES
                ('run', 'edit', 1, '{"from":"a","to":"c"}'),
                ('run', 'edit', 2, NULL),
                ('run', 'edit', 3, '{"from":"c","to":"d"}');"#,
            )
            .expect("fixture");
        for commit in [false, true] {
            let transaction = connection.transaction().expect("transaction");
            publish_candidate_edges(&transaction, "run", "edit", 2).expect("publish edges");
            let mut statement = transaction
                .prepare(
                    "SELECT edge_id, revision, source_node_id, target_node_id
                 FROM workflow_run_graph_edges WHERE retired_at_revision IS NULL ORDER BY edge_id",
                )
                .expect("query");
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, u64>(0)?,
                        row.get::<_, u64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })
                .expect("rows")
                .collect::<Result<Vec<_>, _>>()
                .expect("edges");
            assert_eq!(
                rows,
                vec![
                    (1, 2, "a".into(), "c".into()),
                    (3, 2, "c".into(), "d".into())
                ]
            );
            drop(statement);
            if commit {
                transaction.commit().expect("commit");
            } else {
                transaction.rollback().expect("rollback");
            }
        }
        let historical: u64 = connection.query_row(
            "SELECT count(*) FROM workflow_run_graph_edges WHERE revision = 1 AND retired_at_revision = 2",
            [], |row| row.get(0)
        ).expect("history");
        assert_eq!(historical, 2);
    }
}
