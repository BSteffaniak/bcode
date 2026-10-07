//! Bounded exhausted-repeat checkpoints and atomic, terminal-preserving successor admission.
use super::{
    MAX_INLINE_JSON_BYTES, NewWorkflowRun, RunStatus, WorkflowDefinition,
    WorkflowExecutionAuthority, WorkflowRunBindingKey, WorkflowRunLimits, WorkflowRunSummary,
    WorkflowStore, WorkflowStoreError, append_event_in, create_run_in_transaction, database,
    definition_in, recovery, sha256_hex, validate_id, validate_run, verify_execution_authority_in,
};
use bcode_workflow::{
    WorkflowContinuationLineage, WorkflowContinuationRequest, WorkflowContinuationSource,
};

use crate::database::block_on;
use switchy_database::query::{
    FilterableQuery, SortDirection, bounded_text, identifier, insert, select, update, where_eq,
};
use switchy_database::schema::{Column, DataType, create_index, create_table};
use switchy_database::{Database, DatabaseValue, TransactionMode};

pub fn initialize(connection: &dyn Database) -> Result<(), WorkflowStoreError> {
    crate::database::block_on(
        create_table("workflow_continuations")
            .if_not_exists(true)
            .columns(
                [
                    "successor_run_id",
                    "predecessor_run_id",
                    "request_json",
                    "lineage_json",
                ]
                .into_iter()
                .map(|name| Column {
                    name: name.into(),
                    nullable: name == "successor_run_id",
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                })
                .collect(),
            )
            .primary_key("successor_run_id")
            .unique_columns(["predecessor_run_id"])
            .foreign_key_column("successor_run_id", "workflow_runs", "run_id")
            .foreign_key_column("predecessor_run_id", "workflow_runs", "run_id")
            .execute(connection),
    )?;
    crate::database::block_on(
        create_index("workflow_events_kind_sequence")
            .if_not_exists(true)
            .table("workflow_events")
            .columns(vec!["run_id", "event_type", "event_seq"])
            .execute(connection),
    )?;
    Ok(())
}

#[cfg(test)]
#[path = "continuation_tests.rs"]
mod tests;

fn continued_concurrency_cap(limits: &WorkflowRunLimits, allowance: u64) -> Option<u32> {
    limits
        .concurrency_cap
        .map(|cap| cap.min(u32::try_from(allowance).unwrap_or(u32::MAX)))
}

fn invalid(message: &str) -> WorkflowStoreError {
    WorkflowStoreError::InvalidData(message.into())
}

pub fn admission_execution_cap(
    connection: &dyn Database,
    run_id: &str,
    current_cap: u64,
) -> Result<u64, WorkflowStoreError> {
    let first = database::rows(
        connection,
        &select("workflow_events")
            .project_as(
                bounded_text(identifier("payload_json"), 1024, DatabaseValue::Null),
                "payload_json",
            )
            .where_eq("run_id", run_id)
            .where_eq("event_type", "execution_allowance_increased")
            .sort("event_seq", SortDirection::Asc)
            .limit(1),
    )?;
    let Some(first) = first.first() else {
        return Ok(current_cap);
    };
    let value: serde_json::Value = serde_json::from_str(
        &database::text(first, "payload_json")
            .map_err(|_| invalid("oversized allowance admission evidence"))?,
    )
    .map_err(|_| invalid("invalid allowance admission evidence"))?;
    match (
        value["version"].as_u64(),
        value["previous_cap"].as_u64(),
        value["target_cap"].as_u64(),
    ) {
        (Some(1), Some(previous), Some(target))
            if previous > 0 && target > previous && target <= current_cap =>
        {
            Ok(previous)
        }
        _ => Err(invalid(
            "unsupported or inconsistent allowance admission evidence",
        )),
    }
}

impl WorkflowStore {
    /// Increase one active run's attempt cap after application authorization.
    ///
    /// This compare-and-set operation leaves attempts, topology, outcomes and all other
    /// limits unchanged. Repeating the same expected/target pair is harmless while the
    /// target cap is current; a different intervening cap fails closed. Child grants do
    /// not grant root allowance. Callers must authorize the exact run and both caps.
    ///
    /// # Errors
    /// Rejects invalid caps, stale authority, recovery-only, cancelled or terminal runs,
    /// conflicting current caps, and storage failures.
    pub fn increase_execution_allowance(
        &mut self,
        run_id: &str,
        authority: &WorkflowExecutionAuthority,
        expected_cap: u64,
        target_cap: u64,
        now_ms: u64,
    ) -> Result<(), WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        if expected_cap == 0 || target_cap <= expected_cap || target_cap > i64::MAX as u64 {
            return Err(invalid("invalid execution allowance increase"));
        }
        let transaction = database::begin(&self.connection, TransactionMode::Immediate)?;
        let db: &dyn Database = transaction.as_ref();
        verify_execution_authority_in(db, run_id, authority)?;
        let run = super::run_summary_in(db, run_id)?.ok_or_else(|| invalid("run not found"))?;
        if !matches!(run.status, RunStatus::Running | RunStatus::Paused)
            || run.cancellation_requested_at_ms.is_some()
            || !database::rows(
                db,
                &select("workflow_recovery_barriers")
                    .columns(&["run_id"])
                    .where_eq("run_id", run_id)
                    .limit(1),
            )?
            .is_empty()
        {
            return Err(invalid(
                "run is not eligible for an execution allowance increase",
            ));
        }
        let current_rows = database::rows(
            db,
            &select("workflow_runs")
                .columns(&["node_execution_cap"])
                .where_eq("run_id", run_id)
                .limit(1),
        )?;
        let current = database::unsigned(
            current_rows
                .first()
                .ok_or_else(|| invalid("run not found"))?,
            "node_execution_cap",
        )?;
        let observation = serde_json::json!({"version": 1, "previous_cap": expected_cap, "target_cap": target_cap});
        if current == target_cap {
            let previous = database::rows(
                db,
                &select("workflow_events")
                    .project_as(
                        bounded_text(identifier("payload_json"), 1024, DatabaseValue::Null),
                        "payload_json",
                    )
                    .where_eq("run_id", run_id)
                    .where_eq("event_type", "execution_allowance_increased")
                    .sort("event_seq", SortDirection::Desc)
                    .limit(1),
            )?;
            let previous = previous
                .first()
                .and_then(|row| database::text(row, "payload_json").ok());
            if previous
                .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok())
                .as_ref()
                != Some(&observation)
            {
                return Err(invalid(
                    "execution allowance retry has no matching grant evidence",
                ));
            }
            return Ok(());
        }
        if current != expected_cap {
            return Err(invalid(
                "execution allowance changed; inspect before retrying",
            ));
        }
        block_on(
            update("workflow_runs")
                .value("node_execution_cap", target_cap)
                .where_eq("run_id", run_id)
                .execute(db),
        )?;
        append_event_in(
            db,
            run_id,
            "execution_allowance_increased",
            &observation.to_string(),
            now_ms,
        )?;
        database::commit(transaction)?;
        Ok(())
    }

    /// Read explicit continuation lineage without traversing predecessor history.
    ///
    /// # Errors
    /// Rejects malformed identity, oversized/corrupt lineage, or database failures.
    pub fn continuation_lineage(
        &self,
        run_id: &str,
    ) -> Result<Option<WorkflowContinuationLineage>, WorkflowStoreError> {
        Self::continuation_lineage_in(&self.connection, run_id)
    }

    fn continuation_lineage_in(
        db: &dyn Database,
        run_id: &str,
    ) -> Result<Option<WorkflowContinuationLineage>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        let rows = database::rows(
            db,
            &select("workflow_continuations")
                .project_as(
                    bounded_text(
                        identifier("lineage_json"),
                        MAX_INLINE_JSON_BYTES as u64,
                        DatabaseValue::Null,
                    ),
                    "lineage_json",
                )
                .where_eq("successor_run_id", run_id)
                .limit(1),
        )?;
        rows.first()
            .map(|row| {
                let json = database::text(row, "lineage_json")?;
                serde_json::from_str(&json).map_err(Into::into)
            })
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
        let snapshot = database::begin(&self.connection, TransactionMode::Deferred)?;
        Self::continuation_source_in(snapshot.as_ref(), run_id)
    }

    fn validate_continuation_source_quiescence(
        db: &dyn Database,
        run_id: &str,
    ) -> Result<(), WorkflowStoreError> {
        let blocked = [
            select("workflow_attempts")
                .columns(&["run_id"])
                .where_eq("run_id", run_id)
                .where_not_in("status", vec!["succeeded", "failed", "cancelled"])
                .limit(1),
            select("workflow_activations")
                .columns(&["run_id"])
                .where_eq("run_id", run_id)
                .where_not_in("status", vec!["completed", "failed", "cancelled"])
                .limit(1),
            select("workflow_run_links")
                .columns(&["child_run_id"])
                .where_or(vec![
                    Box::new(where_eq("parent_run_id", run_id)),
                    Box::new(where_eq("child_run_id", run_id)),
                ])
                .limit(1),
            select("workflow_replacement_intents")
                .columns(&["old_run_id"])
                .where_eq("old_run_id", run_id)
                .limit(1),
            select("workflow_resource_leases")
                .columns(&["run_id"])
                .where_eq("run_id", run_id)
                .where_eq("released_at_ms", DatabaseValue::Null)
                .limit(1),
        ]
        .iter()
        .try_fold(
            false,
            |blocked, query| -> Result<bool, WorkflowStoreError> {
                Ok(blocked || !database::rows(db, query)?.is_empty())
            },
        )?;
        if blocked {
            return Err(invalid(
                "continuation source has unsettled work, composition, or replacement intent",
            ));
        }
        Ok(())
    }

    fn exhausted_repeat_output(
        db: &dyn Database,
        run_id: &str,
    ) -> Result<(serde_json::Value, String, u64, serde_json::Value, String), WorkflowStoreError>
    {
        let failures = database::rows(
            db,
            &select("workflow_events")
                .project_as(
                    bounded_text(
                        identifier("payload_json"),
                        MAX_INLINE_JSON_BYTES as u64,
                        DatabaseValue::Null,
                    ),
                    "payload_json",
                )
                .where_eq("run_id", run_id)
                .where_eq("event_type", "run_failed")
                .sort("event_seq", SortDirection::Desc)
                .limit(1),
        )?;
        let failure = database::text(
            failures
                .first()
                .ok_or_else(|| invalid("continuation source has no durable exhaustion reason"))?,
            "payload_json",
        )?;
        let failure: serde_json::Value = serde_json::from_str(&failure)?;
        if failure["reason"] != "repeat_iteration_limit_exhausted" {
            return Err(WorkflowStoreError::ContinuationIneligible);
        }
        let node_id = failure["node_id"]
            .as_str()
            .ok_or_else(|| invalid("missing exhausted repeat identity"))?
            .to_owned();
        let generation = failure["generation"]
            .as_u64()
            .ok_or_else(|| invalid("missing exhausted repeat generation"))?;
        let iterations = generation
            .checked_add(1)
            .ok_or_else(|| invalid("iteration overflow"))?;
        let activations = database::rows(
            db,
            &select("workflow_activations")
                .columns(&["output_id", "activation_id"])
                .where_eq("run_id", run_id)
                .where_eq("node_id", node_id.as_str())
                .where_eq("dependency_generation", generation)
                .where_eq("status", "completed")
                .limit(1),
        )?;
        let activation = activations
            .first()
            .ok_or_else(|| invalid("exhausted repeat activation missing"))?;
        let output_id = database::text(activation, "output_id")?;
        let activation_id = database::text(activation, "activation_id")?;
        let outputs = database::rows(
            db,
            &select("workflow_outputs")
                .project_as(
                    bounded_text(
                        identifier("value_json"),
                        MAX_INLINE_JSON_BYTES as u64,
                        DatabaseValue::Null,
                    ),
                    "value_json",
                )
                .project(identifier("checksum_sha256"))
                .where_eq("output_id", output_id.as_str())
                .where_eq("run_id", run_id)
                .where_eq("node_id", node_id.as_str())
                .where_eq("activation_id", activation_id.as_str())
                .limit(1),
        )?;
        let output = outputs
            .first()
            .ok_or_else(|| invalid("exhausted repeat output missing"))?;
        let json = database::text(output, "value_json")?;
        let checksum = database::text(output, "checksum_sha256")?;
        if sha256_hex(json.as_bytes()) != checksum {
            return Err(invalid("exhausted repeat output checksum mismatch"));
        }
        let input: serde_json::Value = serde_json::from_str(&json)?;
        Ok((failure, node_id, iterations, input, checksum))
    }

    fn continuation_run_limits(
        db: &dyn Database,
        run_id: &str,
    ) -> Result<WorkflowRunLimits, WorkflowStoreError> {
        let limit_rows = database::rows(
            db,
            &select("workflow_runs")
                .columns(&[
                    "deadline_at_ms",
                    "node_execution_cap",
                    "concurrency_cap",
                    "cycle_cap",
                    "retry_cap",
                    "recursion_depth_cap",
                    "descendant_cap",
                ])
                .where_eq("run_id", run_id)
                .limit(1),
        )?;
        let row = limit_rows
            .first()
            .ok_or_else(|| invalid("run limits missing"))?;
        let optional = |column| -> Result<Option<u64>, WorkflowStoreError> {
            if matches!(row.get(column), Some(DatabaseValue::Null)) {
                Ok(None)
            } else {
                database::unsigned(row, column).map(Some)
            }
        };
        let narrow =
            |value: u64| u32::try_from(value).map_err(|_| invalid("run limit exceeds u32"));
        let limits = WorkflowRunLimits {
            deadline_at_ms: optional("deadline_at_ms")?,
            node_execution_cap: database::unsigned(row, "node_execution_cap")?,
            concurrency_cap: optional("concurrency_cap")?.map(narrow).transpose()?,
            cycle_cap: narrow(database::unsigned(row, "cycle_cap")?)?,
            retry_cap: narrow(database::unsigned(row, "retry_cap")?)?,
            recursion_depth_cap: narrow(database::unsigned(row, "recursion_depth_cap")?)?,
            descendant_cap: narrow(database::unsigned(row, "descendant_cap")?)?,
        };
        Ok(limits)
    }

    fn continuation_source_in(
        db: &dyn Database,
        run_id: &str,
    ) -> Result<WorkflowContinuationSource, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        let run = super::run_summary_in(db, run_id)?
            .ok_or_else(|| invalid("continuation source not found"))?;
        if run.status != RunStatus::Failed || run.cancellation_requested_at_ms.is_some() {
            return Err(WorkflowStoreError::ContinuationIneligible);
        }
        Self::validate_continuation_source_quiescence(db, run_id)?;
        let (failure, node_id, iterations, input, checksum) =
            Self::exhausted_repeat_output(db, run_id)?;
        let graph = Self::current_run_graph_page_in(db, run_id, None, None, None, 1_000)?;
        if !graph.nodes_complete || !graph.edges_complete {
            return Err(invalid("continuation graph exceeds bounded admission size"));
        }
        let stored = definition_in(db, &run.definition_id, run.definition_version)?
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
            .get(&node_id)
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
        let limits = Self::continuation_run_limits(db, run_id)?;
        let bound = repeat.configuration["max_iterations"]
            .as_u64()
            .ok_or_else(|| invalid("missing repeat bound"))?
            .min(u64::from(limits.cycle_cap));
        if iterations != bound || failure["effective_iteration_bound"].as_u64() != Some(bound) {
            return Err(invalid("inconsistent exhaustion boundary"));
        }
        let lineage = Self::continuation_lineage_in(db, run_id)?;
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
            repeat_node_id: node_id,
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
        Self::continuation_retry_in(&self.connection, request)
    }

    fn continuation_retry_in(
        db: &dyn Database,
        request: &WorkflowContinuationRequest,
    ) -> Result<Option<WorkflowRunSummary>, WorkflowStoreError> {
        let run_id = request
            .successor
            .run_id
            .as_deref()
            .ok_or_else(|| invalid("continuation requires a stable successor ID"))?;
        let stored = database::rows(
            db,
            &select("workflow_continuations")
                .project(identifier("successor_run_id"))
                .project_as(
                    bounded_text(
                        identifier("request_json"),
                        MAX_INLINE_JSON_BYTES as u64,
                        DatabaseValue::Null,
                    ),
                    "request_json",
                )
                .where_or(vec![
                    Box::new(where_eq("successor_run_id", run_id)),
                    Box::new(where_eq(
                        "predecessor_run_id",
                        request.source_run_id.as_str(),
                    )),
                ])
                .limit(1),
        )?;
        let Some(stored) = stored.first() else {
            return Ok(None);
        };
        let id = database::text(stored, "successor_run_id")?;
        let json = database::text(stored, "request_json")?;
        if id != run_id || serde_json::from_str::<WorkflowContinuationRequest>(&json)? != *request {
            return Err(invalid(
                "conflicting continuation; source already has a successor",
            ));
        }
        super::run_summary_in(db, &id)?
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
        let tx = database::begin(&self.connection, TransactionMode::Immediate)?;
        let db: &dyn Database = tx.as_ref();
        if Self::continuation_retry_in(db, request)?.is_some() {
            return Ok(false);
        }
        verify_execution_authority_in(db, &request.source_run_id, authority)?;
        recovery::require_execution(db, &request.source_run_id)?;
        let source = Self::continuation_source_in(db, &request.source_run_id)?;
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
        if super::associated_run_in(db, &key)?.is_none_or(|run| run.run_id != request.source_run_id)
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
            || successor.limits.concurrency_cap
                != continued_concurrency_cap(&source.limits, successor.limits.node_execution_cap)
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
        create_run_in_transaction(db, successor)?;
        block_on(
            insert("workflow_continuations")
                .value("successor_run_id", successor.run_id.as_str())
                .value("predecessor_run_id", request.source_run_id.as_str())
                .value("request_json", json.as_str())
                .value("lineage_json", serde_json::to_string(&lineage)?)
                .execute(db),
        )?;
        append_event_in(
            db,
            &successor.run_id,
            "run_continued",
            &serde_json::to_string(&lineage)?,
            successor.created_at_ms,
        )?;
        database::commit(tx)?;
        Ok(true)
    }
}
