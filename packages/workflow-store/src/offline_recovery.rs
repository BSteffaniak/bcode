//! Explicit offline recovery of quiescent, portable workflow execution.

use super::{
    Digest, RunStatus, Sha256, WorkflowExecutionAuthority, WorkflowStore, WorkflowStoreError,
    append_event_in, database, definition_in, run_graph, validate_id,
    validate_quiescent_reassignment_in, verify_execution_authority_in,
};
use crate::database::block_on;
use switchy_database::query::{FilterableQuery, count_all, delete, select, update, where_eq};
use switchy_database::{Database, TransactionMode};

impl WorkflowStore {
    /// Validate an exact paused run for offline continuation under current declarative contracts.
    ///
    /// This maintenance-only operation does not replay history. Graph edits, nested procedures,
    /// plugin blocks and closure-backed tasks require their own compatibility evidence and are
    /// rejected rather than inferred compatible from deserialization.
    ///
    /// # Errors
    /// Rejects unsettled work, cancellation, unsupported contracts, graph revisions or children.
    pub fn validate_offline_continuation(&self, run_id: &str) -> Result<(), WorkflowStoreError> {
        Self::validate_offline_continuation_in(&self.connection, run_id)
    }

    fn validate_offline_continuation_in(
        db: &dyn Database,
        run_id: &str,
    ) -> Result<(), WorkflowStoreError> {
        let run =
            super::run_summary_in(db, run_id)?.ok_or_else(|| WorkflowStoreError::RunNotFound {
                run_id: run_id.into(),
            })?;
        if run.status != RunStatus::Paused || run.cancellation_requested_at_ms.is_some() {
            return Err(invalid_recovery(
                "offline continuation requires a paused, uncancelled run",
            ));
        }
        validate_quiescent_reassignment_in(db, run_id)?;
        let revision = run_graph::graph_revision(db, run_id)?;
        if revision != Some(1) {
            return Err(invalid_recovery(
                "edited graphs require revision-specific compatibility qualification",
            ));
        }
        let children = !database::rows(
            db,
            &select("workflow_run_links")
                .columns(&["child_run_id"])
                .where_or(vec![
                    Box::new(where_eq("parent_run_id", run_id)),
                    Box::new(where_eq("child_run_id", run_id)),
                ])
                .limit(1),
        )?
        .is_empty();
        if children {
            return Err(invalid_recovery(
                "nested workflow recovery requires separate child qualification",
            ));
        }
        let stored = definition_in(db, &run.definition_id, run.definition_version)?
            .ok_or_else(|| invalid_recovery("workflow definition missing"))?;
        if stored.definition_json.len() > 4 * 1024 * 1024 {
            return Err(invalid_recovery(
                "definition exceeds offline compatibility inspection budget",
            ));
        }
        let checksum = hex::encode(Sha256::digest(stored.definition_json.as_bytes()));
        if checksum != stored.checksum_sha256 {
            return Err(invalid_recovery("workflow definition checksum mismatch"));
        }
        let definition: bcode_workflow::WorkflowDefinition =
            serde_json::from_str(&stored.definition_json)
                .map_err(|_| invalid_recovery("unsupported workflow definition"))?;
        definition
            .validate()
            .map_err(|_| invalid_recovery("invalid workflow definition"))?;
        let admission = definition
            .production_admission(&bcode_workflow::WorkflowProductionCapabilities::current())
            .map_err(|_| invalid_recovery("unsupported production contracts"))?;
        if !admission.is_supported() {
            return Err(invalid_recovery("unsupported production contracts"));
        }
        Self::validate_offline_graph(db, run_id, &definition)
    }

    fn validate_offline_graph(
        db: &dyn Database,
        run_id: &str,
        definition: &bcode_workflow::WorkflowDefinition,
    ) -> Result<(), WorkflowStoreError> {
        for node in definition.nodes.values() {
            match node.kind {
                bcode_workflow::NodeKind::Agent => {
                    let prompt: bcode_workflow::WorkflowPromptConfiguration =
                        serde_json::from_value(node.configuration.clone())
                            .map_err(|_| invalid_recovery("unsupported agent prompt contract"))?;
                    prompt
                        .validate()
                        .map_err(|_| invalid_recovery("invalid agent prompt contract"))?;
                }
                bcode_workflow::NodeKind::Branch
                | bcode_workflow::NodeKind::Repeat
                | bcode_workflow::NodeKind::Retry => {}
                _ => {
                    return Err(invalid_recovery(
                        "node owner has not qualified cross-artifact continuation",
                    ));
                }
            }
            let current = Self::run_graph_node_in(db, run_id, &node.id)?
                .ok_or_else(|| invalid_recovery("graph node missing"))?;
            if current != *node {
                return Err(invalid_recovery("graph differs from qualified definition"));
            }
        }
        if super::pending_replacement_in(db, run_id)?.is_some() {
            return Err(invalid_recovery(
                "pending replacement blocks same-run continuation",
            ));
        }
        for (index, expected) in definition.edges.iter().enumerate() {
            let edge = Self::run_graph_edge_in(
                db,
                run_id,
                u64::try_from(index).map_err(|_| invalid_recovery("edge identity overflow"))?,
            )?
            .ok_or_else(|| invalid_recovery("graph edge missing"))?;
            if edge.edge != *expected {
                return Err(invalid_recovery(
                    "graph edge differs from qualified definition",
                ));
            }
        }
        let page = Self::current_run_graph_page_in(db, run_id, Some(1), None, None, 1)?;
        let node_count = count_rows(db, "workflow_run_graph_nodes", run_id)?;
        let edge_count = count_rows(db, "workflow_run_graph_edges", run_id)?;
        if node_count != definition.nodes.len() as u64
            || edge_count != definition.edges.len() as u64
            || page.revision != 1
        {
            return Err(invalid_recovery(
                "graph membership differs from qualified definition",
            ));
        }
        Ok(())
    }

    /// Transfer an offline, exclusively fenced run and qualify its portable execution contracts.
    ///
    /// The application must hold state-location execution maintenance and parent-session write
    /// ownership, and require explicit confirmation that all old clients/daemons were upgraded.
    /// The run remains paused. Completed attempts, outputs and iteration state are unchanged.
    ///
    /// # Errors
    /// Rejects stale authority, failed compatibility checks, invalid identities and storage errors.
    pub fn recover_offline_continuation(
        &mut self,
        run_id: &str,
        expected: &WorkflowExecutionAuthority,
        replacement: &WorkflowExecutionAuthority,
        now_ms: u64,
    ) -> Result<(), WorkflowStoreError> {
        validate_id("replacement artifact", &replacement.target_artifact_id)?;
        validate_id("replacement daemon", &replacement.daemon_instance_id)?;
        validate_id("replacement fence", &replacement.fencing_token)?;
        if expected.daemon_instance_id == replacement.daemon_instance_id
            || expected.fencing_token == replacement.fencing_token
            || expected.generation.checked_add(1) != Some(replacement.generation)
        {
            return Err(invalid_recovery("invalid offline authority transfer"));
        }
        let tx = database::begin(&self.connection, TransactionMode::Immediate)?;
        let db: &dyn Database = tx.as_ref();
        verify_execution_authority_in(db, run_id, expected)?;
        Self::validate_offline_continuation_in(db, run_id)?;
        let changed = block_on(
            update("workflow_runs")
                .value(
                    "target_artifact_id",
                    replacement.target_artifact_id.as_str(),
                )
                .value(
                    "coordinator_daemon_instance_id",
                    replacement.daemon_instance_id.as_str(),
                )
                .value("coordinator_generation", replacement.generation)
                .value(
                    "coordinator_fencing_token",
                    replacement.fencing_token.as_str(),
                )
                .value("updated_at_ms", now_ms)
                .where_eq("run_id", run_id)
                .where_eq("coordinator_fencing_token", expected.fencing_token.as_str())
                .execute(db),
        )?
        .len();
        if changed != 1 {
            return Err(invalid_recovery(
                "offline authority transfer lost ownership",
            ));
        }
        block_on(
            delete("workflow_recovery_barriers")
                .where_eq("run_id", run_id)
                .execute(db),
        )?;
        append_event_in(
            db,
            run_id,
            "offline_continuation_qualified",
            &serde_json::json!({
                "version": 1, "from": expected, "to": replacement,
                "evidence": "exclusive_state_execution_and_session_maintenance_after_full_upgrade",
                "compatibility": "production_declarative_v1", "resumed": false
            })
            .to_string(),
            now_ms,
        )?;
        database::commit(tx)?;
        Ok(())
    }
}

fn count_rows(db: &dyn Database, table: &str, run_id: &str) -> Result<u64, WorkflowStoreError> {
    let rows = database::rows(
        db,
        &select(table)
            .project_as(count_all(), "count")
            .where_eq("run_id", run_id),
    )?;
    database::unsigned(
        rows.first()
            .ok_or_else(|| invalid_recovery("missing graph count"))?,
        "count",
    )
}

fn invalid_recovery(message: &str) -> WorkflowStoreError {
    WorkflowStoreError::InvalidData(message.into())
}
