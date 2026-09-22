//! Bounded exhausted-repeat checkpoints and atomic, terminal-preserving successor admission.
use super::{
    Connection, MAX_INLINE_JSON_BYTES, NewWorkflowRun, OptionalExtension, RunStatus,
    WorkflowDefinition, WorkflowExecutionAuthority, WorkflowRunBindingKey, WorkflowRunLimits,
    WorkflowRunSummary, WorkflowStore, WorkflowStoreError, append_event, create_run_in_transaction,
    recovery, sha256_hex, validate_id, validate_run,
};
use bcode_workflow::{
    WorkflowContinuationLineage, WorkflowContinuationRequest, WorkflowContinuationSource,
};

pub fn initialize(connection: &Connection) -> Result<(), WorkflowStoreError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS workflow_continuations (
            successor_run_id TEXT PRIMARY KEY REFERENCES workflow_runs(run_id),
            predecessor_run_id TEXT NOT NULL UNIQUE REFERENCES workflow_runs(run_id),
            request_json TEXT NOT NULL,
            lineage_json TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS workflow_events_kind_sequence
         ON workflow_events(run_id, event_type, event_seq);",
    )?;
    Ok(())
}

#[cfg(test)]
#[path = "continuation_tests.rs"]
mod tests;

fn invalid(message: &str) -> WorkflowStoreError {
    WorkflowStoreError::InvalidData(message.into())
}

impl WorkflowStore {
    /// Read explicit continuation lineage without traversing predecessor history.
    ///
    /// # Errors
    /// Rejects malformed identity, oversized/corrupt lineage, or database failures.
    pub fn continuation_lineage(
        &self,
        run_id: &str,
    ) -> Result<Option<WorkflowContinuationLineage>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        let json: Option<String> = self
            .connection
            .query_row(
                "SELECT CASE WHEN length(CAST(lineage_json AS BLOB)) <= ?2 THEN lineage_json END
             FROM workflow_continuations WHERE successor_run_id=?1",
                (run_id, MAX_INLINE_JSON_BYTES),
                |row| row.get(0),
            )
            .optional()?;
        json.map(|json| serde_json::from_str(&json).map_err(Into::into))
            .transpose()
    }

    /// Read a settled repeat-limit checkpoint from bounded canonical records.
    /// No repair, execution-authority acquisition, or event replay is performed.
    ///
    /// # Errors
    /// Rejects non-exhausted runs, unsettled work, composition, damaged outputs, or oversized graphs.
    #[allow(clippy::too_many_lines)]
    pub fn continuation_source(
        &self,
        run_id: &str,
    ) -> Result<WorkflowContinuationSource, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        let _snapshot = self
            .connection
            .is_autocommit()
            .then(|| self.connection.unchecked_transaction())
            .transpose()?;
        let run = self
            .run_summary(run_id)?
            .ok_or_else(|| invalid("continuation source not found"))?;
        if run.status != RunStatus::Failed || run.cancellation_requested_at_ms.is_some() {
            return Err(invalid(
                "continuation requires a settled iteration-limit failure; paused runs use resume",
            ));
        }
        let blocked: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM workflow_attempts WHERE run_id=?1 AND status NOT IN ('succeeded','failed','cancelled'))
             OR EXISTS(SELECT 1 FROM workflow_activations WHERE run_id=?1 AND status NOT IN ('completed','failed','cancelled'))
             OR EXISTS(SELECT 1 FROM workflow_run_links WHERE parent_run_id=?1 OR child_run_id=?1)
             OR EXISTS(SELECT 1 FROM workflow_replacement_intents WHERE old_run_id=?1)
             OR EXISTS(SELECT 1 FROM workflow_resource_leases WHERE run_id=?1 AND released_at_ms IS NULL)",
            [run_id], |row| row.get(0),
        )?;
        if blocked {
            return Err(invalid(
                "continuation source has unsettled work, composition, or replacement intent",
            ));
        }
        let failure: String = self.connection.query_row(
            "SELECT CASE WHEN length(CAST(payload_json AS BLOB)) <= ?2 THEN payload_json END
             FROM workflow_events WHERE run_id=?1 AND event_type='run_failed' ORDER BY event_seq DESC LIMIT 1",
            (run_id, MAX_INLINE_JSON_BYTES), |row| row.get(0),
        ).optional()?.ok_or_else(|| invalid("continuation source has no durable exhaustion reason"))?;
        let failure: serde_json::Value = serde_json::from_str(&failure)?;
        if failure["reason"] != "repeat_iteration_limit_exhausted" {
            return Err(invalid(
                "run failed for a reason other than iteration exhaustion",
            ));
        }
        let node_id = failure["node_id"]
            .as_str()
            .ok_or_else(|| invalid("missing exhausted repeat identity"))?;
        let generation = failure["generation"]
            .as_u64()
            .ok_or_else(|| invalid("missing exhausted repeat generation"))?;
        let iterations = generation
            .checked_add(1)
            .ok_or_else(|| invalid("iteration overflow"))?;
        let (json, checksum): (String, String) = self.connection.query_row(
            "SELECT CASE WHEN length(CAST(o.value_json AS BLOB)) <= ?4 THEN o.value_json END, o.checksum_sha256
             FROM workflow_activations a JOIN workflow_outputs o ON o.output_id=a.output_id
             WHERE a.run_id=?1 AND a.node_id=?2 AND a.dependency_generation=?3 AND a.status='completed'
             AND o.run_id=a.run_id AND o.node_id=a.node_id AND o.activation_id=a.activation_id",
            (run_id, node_id, generation, MAX_INLINE_JSON_BYTES), |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if sha256_hex(json.as_bytes()) != checksum {
            return Err(invalid("exhausted repeat output checksum mismatch"));
        }
        let input: serde_json::Value = serde_json::from_str(&json)?;
        let graph = self.current_run_graph_page(run_id, None, None, None, 1_000)?;
        if !graph.nodes_complete || !graph.edges_complete {
            return Err(invalid("continuation graph exceeds bounded admission size"));
        }
        let stored = self
            .definition(&run.definition_id, run.definition_version)?
            .ok_or_else(|| invalid("source definition missing"))?;
        let mut definition: WorkflowDefinition = serde_json::from_str(&stored.definition_json)?;
        definition.entries = graph
            .nodes
            .iter()
            .filter(|node| node.entry)
            .map(|node| node.node.id.clone())
            .collect();
        definition.exits = graph
            .nodes
            .iter()
            .filter(|node| node.exit)
            .map(|node| node.node.id.clone())
            .collect();
        definition.nodes = graph
            .nodes
            .into_iter()
            .map(|node| (node.node.id.clone(), node.node))
            .collect();
        definition.edges = graph.edges.into_iter().map(|edge| edge.edge).collect();
        let repeat = definition
            .nodes
            .get(node_id)
            .ok_or_else(|| invalid("exhausted repeat missing from graph"))?;
        if repeat.kind != bcode_workflow::NodeKind::Repeat {
            return Err(invalid("exhaustion does not reference a repeat"));
        }
        let predicate: bcode_workflow::PredicateExpression =
            serde_json::from_value(repeat.configuration["predicate"].clone())?;
        if !predicate
            .evaluate_value(&input)
            .map_err(|error| invalid(&error.to_string()))?
        {
            return Err(invalid("exhausted repeat predicate has cleared"));
        }
        let limits = self.connection.query_row(
            "SELECT deadline_at_ms,node_execution_cap,concurrency_cap,cycle_cap,retry_cap FROM workflow_runs WHERE run_id=?1",
            [run_id], |row| Ok(WorkflowRunLimits { deadline_at_ms: row.get(0)?, node_execution_cap: row.get(1)?, concurrency_cap: row.get(2)?, cycle_cap: row.get(3)?, retry_cap: row.get(4)? }),
        )?;
        let bound = repeat.configuration["max_iterations"]
            .as_u64()
            .ok_or_else(|| invalid("missing repeat bound"))?
            .min(u64::from(limits.cycle_cap));
        if iterations != bound || failure["effective_iteration_bound"].as_u64() != Some(bound) {
            return Err(invalid("inconsistent exhaustion boundary"));
        }
        let lineage = self.continuation_lineage(run_id)?;
        let total = lineage
            .as_ref()
            .map_or(0, |value| value.prior_iterations)
            .checked_add(iterations)
            .ok_or_else(|| invalid("cumulative iteration overflow"))?;
        let document_scope_id =
            lineage.map_or_else(|| run_id.to_owned(), |value| value.document_scope_id);
        Ok(WorkflowContinuationSource {
            run,
            definition,
            input,
            repeat_node_id: node_id.into(),
            graph_revision: graph.revision,
            output_checksum: checksum,
            iterations_completed: iterations,
            total_iterations_completed: total,
            document_scope_id,
            limits,
        })
    }

    /// Return the committed successor for an identical request, even after later continuations.
    ///
    /// # Errors
    /// Rejects conflicting duplicate IDs or another successor already admitted for the source.
    pub fn continuation_retry(
        &self,
        request: &WorkflowContinuationRequest,
    ) -> Result<Option<WorkflowRunSummary>, WorkflowStoreError> {
        let run_id = request
            .successor
            .run_id
            .as_deref()
            .ok_or_else(|| invalid("continuation requires a stable successor ID"))?;
        let stored: Option<(String, String)> = self.connection.query_row(
            "SELECT successor_run_id, CASE WHEN length(CAST(request_json AS BLOB)) <= ?3 THEN request_json END
             FROM workflow_continuations WHERE successor_run_id=?1 OR predecessor_run_id=?2 LIMIT 1",
            (run_id, &request.source_run_id, MAX_INLINE_JSON_BYTES), |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?;
        let Some((id, json)) = stored else {
            return Ok(None);
        };
        if id != run_id || serde_json::from_str::<WorkflowContinuationRequest>(&json)? != *request {
            return Err(invalid(
                "conflicting continuation; source already has a successor",
            ));
        }
        self.run_summary(&id)?
            .ok_or_else(|| invalid("continuation successor missing"))
            .map(Some)
    }

    /// Atomically admit one successor, with source fencing and exact association comparison.
    /// The predecessor's status, activations, outputs, and timestamps remain unchanged.
    ///
    /// # Errors
    /// Rejects stale authority/checkpoints, changed associations, conflicting grants, or invalid admission.
    pub fn continue_run_owned(
        &mut self,
        request: &WorkflowContinuationRequest,
        successor: &NewWorkflowRun,
        authority: &WorkflowExecutionAuthority,
    ) -> Result<bool, WorkflowStoreError> {
        validate_run(successor)?;
        let tx = self.connection.unchecked_transaction()?;
        if self.continuation_retry(request)?.is_some() {
            return Ok(false);
        }
        self.verify_execution_authority(&request.source_run_id, authority)?;
        recovery::require_execution(&tx, &request.source_run_id)?;
        let source = self.continuation_source(&request.source_run_id)?;
        let binding = source
            .run
            .binding
            .as_ref()
            .ok_or_else(|| invalid("continuation requires a bound source"))?;
        let key = WorkflowRunBindingKey {
            owner_plugin_id: binding.owner_plugin_id.clone(),
            workflow_kind: binding.workflow_kind.clone(),
            scope_key: binding.scope_key.clone(),
        };
        if self
            .associated_run(&key)?
            .is_none_or(|run| run.run_id != request.source_run_id)
        {
            return Err(invalid("session association changed; refresh status"));
        }
        if request.additional_iterations == 0
            || source.graph_revision != request.expected_graph_revision
            || source.output_checksum != request.expected_output_checksum
            || successor.run_id != request.successor.run_id.clone().unwrap_or_default()
            || successor.binding.as_ref() != Some(binding)
            || successor.parent_session_id != source.run.parent_session_id
            || successor.workspace_snapshot != source.run.workspace_snapshot
            || successor.limits.cycle_cap != request.additional_iterations
            || successor.limits.deadline_at_ms != source.limits.deadline_at_ms
            || successor.limits.retry_cap != source.limits.retry_cap
            || successor.limits.concurrency_cap != source.limits.concurrency_cap
            || successor.authorization_ceiling > source.run.authorization_ceiling
        {
            return Err(invalid(
                "continuation checkpoint, binding, or allowance conflict",
            ));
        }
        if successor.definition_id != request.successor.identity.definition_id
            || successor.definition_version != request.successor.identity.definition_version
            || successor.input.as_ref() != Some(&request.successor.input)
            || successor.limits != request.successor.limits
            || successor.binding.as_ref() != Some(&request.successor.binding)
            || successor.parent_session_id.as_deref()
                != Some(request.successor.parent_session_id.to_string().as_str())
            || successor.execution_authority.is_none()
            || successor.authorization_profile != source.run.authorization_profile
        {
            return Err(invalid(
                "prepared successor differs from authorized continuation",
            ));
        }
        let repeat = request
            .successor
            .definition
            .nodes
            .get(&source.repeat_node_id)
            .ok_or_else(|| invalid("successor must retain the exhausted repeat"))?;
        if repeat.kind != bcode_workflow::NodeKind::Repeat
            || repeat.configuration["max_iterations"].as_u64()
                != Some(u64::from(request.additional_iterations))
        {
            return Err(invalid(
                "successor repeat bound differs from the explicit grant",
            ));
        }
        let lineage = WorkflowContinuationLineage {
            predecessor_run_id: request.source_run_id.clone(),
            document_scope_id: source.document_scope_id,
            prior_iterations: source.total_iterations_completed,
            additional_iterations: request.additional_iterations,
        };
        let json = serde_json::to_string(request)?;
        if json.len() > MAX_INLINE_JSON_BYTES {
            return Err(invalid("continuation request exceeds durable byte limit"));
        }
        create_run_in_transaction(&tx, successor)?;
        tx.execute(
            "INSERT INTO workflow_continuations VALUES (?1,?2,?3,?4)",
            (
                &successor.run_id,
                &request.source_run_id,
                &json,
                serde_json::to_string(&lineage)?,
            ),
        )?;
        append_event(
            &tx,
            &successor.run_id,
            "run_continued",
            &serde_json::to_string(&lineage)?,
            successor.created_at_ms,
        )?;
        tx.commit()?;
        Ok(true)
    }
}
