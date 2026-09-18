//! Explicit offline recovery of quiescent, portable workflow execution.

use super::{
    Digest, RunStatus, Sha256, WorkflowExecutionAuthority, WorkflowStore, WorkflowStoreError,
    append_event, run_graph, validate_id,
};

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
        let run = self
            .run_summary(run_id)?
            .ok_or_else(|| WorkflowStoreError::RunNotFound {
                run_id: run_id.into(),
            })?;
        if run.status != RunStatus::Paused || run.cancellation_requested_at_ms.is_some() {
            return Err(invalid_recovery(
                "offline continuation requires a paused, uncancelled run",
            ));
        }
        self.validate_quiescent_reassignment(run_id)?;
        let revision = run_graph::graph_revision(&self.connection, run_id)?;
        if revision != Some(1) {
            return Err(invalid_recovery(
                "edited graphs require revision-specific compatibility qualification",
            ));
        }
        let children: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM workflow_run_links WHERE parent_run_id = ?1 OR child_run_id = ?1)",
            [run_id], |row| row.get(0))?;
        if children {
            return Err(invalid_recovery(
                "nested workflow recovery requires separate child qualification",
            ));
        }
        let stored = self
            .definition(&run.definition_id, run.definition_version)?
            .ok_or_else(|| invalid_recovery("workflow definition missing"))?;
        if stored.definition_json.len() > 4 * 1024 * 1024 {
            return Err(invalid_recovery(
                "definition exceeds offline compatibility inspection budget",
            ));
        }
        let checksum = format!("{:x}", Sha256::digest(stored.definition_json.as_bytes()));
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
        self.validate_offline_graph(run_id, &definition)
    }

    fn validate_offline_graph(
        &self,
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
            let current = self
                .run_graph_node(run_id, &node.id)?
                .ok_or_else(|| invalid_recovery("graph node missing"))?;
            if current != *node {
                return Err(invalid_recovery("graph differs from qualified definition"));
            }
        }
        if self.pending_replacement(run_id)?.is_some() {
            return Err(invalid_recovery(
                "pending replacement blocks same-run continuation",
            ));
        }
        for (index, expected) in definition.edges.iter().enumerate() {
            let edge = self
                .run_graph_edge(
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
        let page = self.current_run_graph_page(run_id, Some(1), None, None, 1)?;
        let node_count: u64 = self.connection.query_row(
            "SELECT COUNT(*) FROM workflow_run_graph_nodes WHERE run_id=?1",
            [run_id],
            |row| row.get(0),
        )?;
        let edge_count: u64 = self.connection.query_row(
            "SELECT COUNT(*) FROM workflow_run_graph_edges WHERE run_id=?1",
            [run_id],
            |row| row.get(0),
        )?;
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
        let tx = self.connection.unchecked_transaction()?;
        self.verify_execution_authority(run_id, expected)?;
        self.validate_offline_continuation(run_id)?;
        let changed = tx.execute(
            "UPDATE workflow_runs SET target_artifact_id=?2, coordinator_daemon_instance_id=?3,
             coordinator_generation=?4, coordinator_fencing_token=?5, updated_at_ms=?6
             WHERE run_id=?1 AND coordinator_fencing_token=?7",
            rusqlite::params![
                run_id,
                replacement.target_artifact_id,
                replacement.daemon_instance_id,
                replacement.generation,
                replacement.fencing_token,
                now_ms,
                expected.fencing_token
            ],
        )?;
        if changed != 1 {
            return Err(invalid_recovery(
                "offline authority transfer lost ownership",
            ));
        }
        tx.execute(
            "DELETE FROM workflow_recovery_barriers WHERE run_id=?1",
            [run_id],
        )?;
        append_event(
            &tx,
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
        tx.commit()?;
        Ok(())
    }
}

fn invalid_recovery(message: &str) -> WorkflowStoreError {
    WorkflowStoreError::InvalidData(message.into())
}
