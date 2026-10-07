//! Workflow controls, cancellation and replacement requests.
use super::*;
use crate::database::block_on;
use switchy_database::query::{
    FilterableQuery, Projection, SortDirection, bounded_text, identifier, schema_predicate, select,
    update,
};
use switchy_database::schema::{SchemaComparison as C, SchemaExpression as E};
use switchy_database::{Database, DatabaseValue, Row, TransactionMode};

impl WorkflowStore {
    /// Persist an authorized successor intent and stop old admission atomically.
    ///
    /// This does not create or dispatch the successor, nor claim child cancellation.
    /// The application must independently authorize the successor before calling.
    /// Identical retries are idempotent; conflicting successors are rejected.
    ///
    /// # Errors
    /// Rejects stale authority, invalid successor, mismatched binding, conflicting intent,
    /// terminal old runs, oversized payloads, or storage failure.
    pub fn request_replacement_owned(
        &mut self,
        old_run_id: &str,
        authority: &WorkflowExecutionAuthority,
        successor: &NewWorkflowRun,
        now_ms: u64,
    ) -> Result<bool, WorkflowStoreError> {
        validate_run(successor)?;
        let binding = successor.binding.as_ref().ok_or_else(|| {
            WorkflowStoreError::InvalidData("replacement requires an exact binding".into())
        })?;
        if successor.run_id == old_run_id {
            return Err(WorkflowStoreError::InvalidData(
                "replacement must use a new run identity".into(),
            ));
        }
        let payload = serde_json::to_string(successor)?;
        if payload.len() > MAX_INLINE_JSON_BYTES {
            return Err(WorkflowStoreError::InvalidData(
                "replacement intent exceeds inline limit".into(),
            ));
        }
        let tx = database::begin(&self.connection, TransactionMode::Deferred)?;
        verify_execution_authority_in(tx.as_ref(), old_run_id, authority)?;
        if let Some(existing) = pending_replacement_in(tx.as_ref(), old_run_id)? {
            if &existing != successor {
                return Err(WorkflowStoreError::InvalidData(
                    "conflicting replacement intent".into(),
                ));
            }
            return Ok(false);
        }
        let matches = controls_one(
            tx.as_ref(),
            select("workflow_runs")
                .columns(&["run_id"])
                .where_eq("run_id", old_run_id)
                .where_eq("owner_plugin_id", &binding.owner_plugin_id)
                .where_eq("workflow_kind", &binding.workflow_kind)
                .where_eq("scope_key", &binding.scope_key)
                .where_eq("single_active", i64::from(binding.single_active))
                .where_in(
                    "status",
                    vec![
                        DatabaseValue::from("running"),
                        DatabaseValue::from("paused"),
                    ],
                ),
        )?
        .is_some();
        if !matches {
            return Err(WorkflowStoreError::InvalidData(
                "replacement binding or old run state does not match".into(),
            ));
        }
        validate_replacement_package(tx.as_ref(), old_run_id, successor)?;
        validate_replacement_successor(tx.as_ref(), successor, authority)?;
        validate_stored_run_input(tx.as_ref(), successor)?;
        if let Some(provenance) = &successor.authored_provenance {
            validate_persisted_authored_run_provenance(tx.as_ref(), successor, provenance)?;
        }
        controls_insert(
            tx.as_ref(),
            &(tx.insert("workflow_replacement_intents")
                .value("old_run_id", old_run_id)
                .value("successor_run_id", &successor.run_id)
                .value("successor_json", payload)
                .value("requested_at_ms", database::integer_value(now_ms)?)),
        )?;
        request_run_cancellation_in_transaction(tx.as_ref(), old_run_id, now_ms)?;
        append_event_in(
            tx.as_ref(),
            old_run_id,
            "replacement_requested",
            &serde_json::json!({"successor_run_id": successor.run_id}).to_string(),
            now_ms,
        )?;
        database::commit(tx)?;
        Ok(true)
    }

    /// Withdraw an exact pending successor without undoing old-run cancellation.
    ///
    /// This is an explicitly authorized control operation, not automatic repair.
    /// Repeated withdrawal returns `false`; a different pending successor is rejected.
    ///
    /// # Errors
    /// Rejects stale authority, mismatched successor identity, damaged intent, or storage failure.
    pub fn withdraw_replacement_owned(
        &mut self,
        old_run_id: &str,
        successor_run_id: &str,
        authority: &WorkflowExecutionAuthority,
        now_ms: u64,
    ) -> Result<bool, WorkflowStoreError> {
        validate_id("successor run", successor_run_id)?;
        let tx = database::begin(&self.connection, TransactionMode::Deferred)?;
        verify_execution_authority_in(tx.as_ref(), old_run_id, authority)?;
        let Some(pending) = pending_replacement_in(tx.as_ref(), old_run_id)? else {
            return Ok(false);
        };
        if pending.run_id != successor_run_id {
            return Err(WorkflowStoreError::InvalidData(
                "replacement withdrawal targets a different successor".into(),
            ));
        }
        block_on(
            tx.as_ref().exec_delete_count(
                &(tx.as_ref()
                    .delete("workflow_replacement_intents")
                    .where_eq("old_run_id", DatabaseValue::from(old_run_id))),
            ),
        )?;
        append_event_in(
            tx.as_ref(),
            old_run_id,
            "replacement_withdrawn",
            &serde_json::json!({"successor_run_id": successor_run_id}).to_string(),
            now_ms,
        )?;
        database::commit(tx)?;
        Ok(true)
    }

    /// Discover a bounded keyset page of pending replacement owners, including terminal runs.
    ///
    /// These identities are hints only. Callers must acquire current ownership and
    /// reauthorize the pending request before cancellation or handoff. An empty page
    /// ends a sweep; restart from the empty cursor to revisit blocked replacements.
    ///
    /// # Errors
    /// Rejects invalid cursors, invalid bounds, damaged identities, or storage failure.
    pub fn pending_replacement_run_ids_after(
        &self,
        after_run_id: &str,
        limit: usize,
    ) -> Result<Vec<String>, WorkflowStoreError> {
        if !after_run_id.is_empty() {
            validate_id("replacement cursor", after_run_id)?;
        }
        let limit = controls_limit(bounded_limit(limit)?)?;
        database::rows(
            &self.connection,
            &select("workflow_replacement_intents")
                .projections(vec![
                    Projection::new(bounded_text(
                        identifier("old_run_id"),
                        MAX_ID_BYTES as u64,
                        DatabaseValue::Null,
                    ))
                    .alias("old_run_id"),
                ])
                .where_gt("old_run_id", after_run_id)
                .sort("old_run_id", SortDirection::Asc)
                .limit(limit),
        )?
        .iter()
        .map(|row| database::text(row, "old_run_id"))
        .collect()
    }

    /// Verify one bounded page of immutable terminal work and direct-child proofs.
    /// Progress is persisted; no recursive traversal or replay is performed. Descendants
    /// are advanced separately under their own authority and terminal outcomes never reopen.
    ///
    /// # Errors
    /// Rejects stale authority, invalid limits, inconsistent storage, or database failure.
    pub fn advance_subtree_quiescence_owned(
        &mut self,
        run_id: &str,
        authority: &WorkflowExecutionAuthority,
        limit: usize,
    ) -> Result<bool, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        let limit = controls_limit(bounded_limit(limit)?)?;
        let tx = database::begin(&self.connection, TransactionMode::Deferred)?;
        let db = tx.as_ref();
        verify_execution_authority_in(db, run_id, authority)?;
        let status = database::text(
            &controls_required(
                db,
                select("workflow_runs")
                    .columns(&["status"])
                    .where_eq("run_id", run_id),
            )?,
            "status",
        )?;
        if !matches!(status.as_str(), "completed" | "failed" | "cancelled") {
            return Ok(false);
        }
        block_on(
            db.insert("workflow_quiescence")
                .value("run_id", run_id)
                .or_ignore()
                .execute_count(db),
        )?;
        let row = controls_required(
            db,
            select("workflow_quiescence")
                .projections(vec![
                    Projection::new(bounded_text(
                        identifier("after_dispatch"),
                        64,
                        DatabaseValue::Null,
                    ))
                    .alias("after_dispatch"),
                    Projection::new(bounded_text(
                        identifier("after_child"),
                        512,
                        DatabaseValue::Null,
                    ))
                    .alias("after_child"),
                    Projection::new(identifier("attempts_complete")),
                    Projection::new(identifier("complete")),
                ])
                .where_eq("run_id", run_id),
        )?;
        let after = database::text(&row, "after_dispatch")?;
        let child = database::text(&row, "after_child")?;
        let attempts_complete = controls_bool(&row, "attempts_complete")?;
        let complete = controls_bool(&row, "complete")?;
        if complete {
            database::commit(tx)?;
            return Ok(true);
        }
        let complete =
            advance_quiescence_pages(db, run_id, &child, &after, limit, attempts_complete)?;
        database::commit(tx)?;
        Ok(complete)
    }

    /// Read one pending replacement without acquiring execution authority or mutating state.
    ///
    /// # Errors
    /// Rejects invalid identity, oversized/malformed durable requests, and storage failures.
    pub fn pending_replacement(
        &self,
        old_run_id: &str,
    ) -> Result<Option<NewWorkflowRun>, WorkflowStoreError> {
        let transaction = database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let result = Self::pending_replacement_in(&*transaction, old_run_id)?;
        database::commit(transaction)?;
        Ok(result)
    }

    /// Inspect replacement blockers without changing intent, ownership, or execution.
    ///
    /// # Errors
    /// Rejects damaged intent, missing authority, or storage failures.
    pub fn replacement_readiness(
        &self,
        old_run_id: &str,
    ) -> Result<ReplacementReadiness, WorkflowStoreError> {
        let tx = database::begin(&self.connection, TransactionMode::Deferred)?;
        let db = tx.as_ref();
        replacement_readiness_in(db, old_run_id)
    }

    /// Atomically admit a persisted replacement after a leaf run is fully quiescent.
    ///
    /// The caller must reauthorize the exact successor request before handoff. Child
    /// workflows conservatively block this operation until descendant proof is supported.
    /// Missing intent returns `false`, including retries after a committed handoff.
    ///
    /// # Errors
    /// Rejects stale authority, nonterminal or ambiguous old work, child links,
    /// mismatched successor intent/authority, or admission failure. Failures preserve intent.
    pub fn complete_leaf_replacement_owned(
        &mut self,
        old_run_id: &str,
        authority: &WorkflowExecutionAuthority,
        successor: &NewWorkflowRun,
        now_ms: u64,
    ) -> Result<bool, WorkflowStoreError> {
        validate_run(successor)?;
        let tx = database::begin(&self.connection, TransactionMode::Deferred)?;
        verify_execution_authority_in(tx.as_ref(), old_run_id, authority)?;
        let Some(mut stored) = pending_replacement_in(tx.as_ref(), old_run_id)? else {
            return Ok(false);
        };
        if stored
            .execution_authority
            .as_ref()
            .is_none_or(|original| original.target_artifact_id != authority.target_artifact_id)
        {
            return Err(WorkflowStoreError::InvalidData(
                "replacement execution artifact compatibility is not established".into(),
            ));
        }
        // Coordinator identity is runtime ownership, not authorized successor content.
        // Rebind only after validating the current durable fence; every other field is exact.
        stored.execution_authority = Some(authority.clone());
        if &stored != successor || successor.execution_authority.as_ref() != Some(authority) {
            return Err(WorkflowStoreError::InvalidData(
                "replacement requires the exact reauthorized successor and current coordinator"
                    .into(),
            ));
        }
        if replacement_readiness_in(tx.as_ref(), old_run_id)?
            != ReplacementReadiness::ReadyForAuthorization
        {
            return Err(WorkflowStoreError::InvalidData(
                "replacement requires a terminal leaf run with no unresolved effects".into(),
            ));
        }
        block_on(
            tx.as_ref().exec_delete_count(
                &(tx.as_ref()
                    .delete("workflow_replacement_intents")
                    .where_eq("old_run_id", DatabaseValue::from(old_run_id))),
            ),
        )?;
        let package = validate_replacement_package(tx.as_ref(), old_run_id, successor)?;
        create_run_in_transaction(tx.as_ref(), successor)?;
        if let Some((id, digest)) = package {
            controls_insert(
                tx.as_ref(),
                &(tx.insert("workflow_run_packages")
                    .value("run_id", &successor.run_id)
                    .value("package_id", id)
                    .value("lock_digest", digest)),
            )?;
        }
        #[cfg(test)]
        if std::env::var("BCODE_TEST_REPLACEMENT_CRASH_PHASE").as_deref() == Ok("before_commit") {
            std::process::exit(73);
        }
        append_event_in(
            tx.as_ref(),
            old_run_id,
            "replacement_completed",
            &serde_json::json!({"successor_run_id": successor.run_id}).to_string(),
            now_ms,
        )?;
        database::commit(tx)?;
        Ok(true)
    }

    /// Pause one running workflow before any further activation or attempt admission.
    ///
    /// Active external attempts are not cancelled; they remain observable and reconcilable. Calling
    /// this for an already-paused run is idempotent.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, missing/terminal/cancelling run, or database
    /// failure.
    pub fn pause_run(
        &mut self,
        run_id: &str,
        paused_at_ms: u64,
    ) -> Result<bool, WorkflowStoreError> {
        transition_run_control_state(
            &self.connection,
            run_id,
            RunStatus::Running,
            RunStatus::Paused,
            "run_paused",
            paused_at_ms,
            None,
        )
    }

    /// Pause a run while atomically verifying its durable execution authority.
    ///
    /// # Errors
    /// Rejects stale authority, invalid lifecycle transitions, or database failure.
    pub fn pause_run_owned(
        &mut self,
        run_id: &str,
        paused_at_ms: u64,
        authority: &WorkflowExecutionAuthority,
    ) -> Result<bool, WorkflowStoreError> {
        transition_run_control_state(
            &self.connection,
            run_id,
            RunStatus::Running,
            RunStatus::Paused,
            "run_paused",
            paused_at_ms,
            Some(authority),
        )
    }

    /// Resume one paused workflow for subsequent scheduler admission.
    ///
    /// Calling this for an already-running run is idempotent. A cancellation request permanently
    /// prevents resume.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, missing/terminal/cancelling run, or database
    /// failure.
    pub fn resume_run(
        &mut self,
        run_id: &str,
        resumed_at_ms: u64,
    ) -> Result<bool, WorkflowStoreError> {
        transition_run_control_state(
            &self.connection,
            run_id,
            RunStatus::Paused,
            RunStatus::Running,
            "run_resumed",
            resumed_at_ms,
            None,
        )
    }

    /// Resume a run while atomically verifying its durable execution authority.
    ///
    /// # Errors
    /// Rejects stale authority, cancellation intent, invalid transitions, or database failure.
    pub fn resume_run_owned(
        &mut self,
        run_id: &str,
        resumed_at_ms: u64,
        authority: &WorkflowExecutionAuthority,
    ) -> Result<bool, WorkflowStoreError> {
        transition_run_control_state(
            &self.connection,
            run_id,
            RunStatus::Paused,
            RunStatus::Running,
            "run_resumed",
            resumed_at_ms,
            Some(authority),
        )
    }

    /// Explicitly requeue one exact failed activation for its next bounded attempt.
    ///
    /// Retry is accepted only when the run and activation are failed, the selected attempt is the
    /// latest terminal failed attempt, no output or downstream activation exists, no cancellation
    /// was requested, and the persisted retry cap permits another attempt. The transition is
    /// atomic and idempotently rejects stale operator requests.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed/stale identity, unsafe downstream state, exhausted retry
    /// budget, cancellation, or database failure.
    #[allow(clippy::too_many_lines)]
    pub fn retry_failed_node(
        &self,
        run_id: &str,
        node_id: &str,
        activation_id: &str,
        failed_attempt: u32,
        retried_at_ms: u64,
    ) -> Result<WorkflowNodeRetryResult, WorkflowStoreError> {
        self.retry_failed_node_with_schedule(
            run_id,
            node_id,
            activation_id,
            failed_attempt,
            retried_at_ms,
            None,
            None,
        )
    }

    /// Retry a failed activation while holding the expected durable execution authority.
    ///
    /// # Errors
    /// Rejects stale authority, invalid retry state, exhausted budget, or database failure.
    pub fn retry_failed_node_owned(
        &self,
        run_id: &str,
        node_id: &str,
        activation_id: &str,
        failed_attempt: u32,
        authority: &WorkflowExecutionAuthority,
        retried_at_ms: u64,
    ) -> Result<WorkflowNodeRetryResult, WorkflowStoreError> {
        self.retry_failed_node_with_schedule(
            run_id,
            node_id,
            activation_id,
            failed_attempt,
            retried_at_ms,
            None,
            Some(authority),
        )
    }

    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    pub(super) fn retry_failed_node_with_schedule(
        &self,
        run_id: &str,
        node_id: &str,
        activation_id: &str,
        failed_attempt: u32,
        retried_at_ms: u64,
        retry_schedule: Option<&AutomaticRetrySchedule>,
        authority: Option<&WorkflowExecutionAuthority>,
    ) -> Result<WorkflowNodeRetryResult, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        validate_id("node_id", node_id)?;
        validate_id("activation_id", activation_id)?;
        if failed_attempt == 0 {
            return Err(WorkflowStoreError::InvalidData(
                "failed attempt must be positive".to_string(),
            ));
        }
        let transaction = database::begin(&self.connection, TransactionMode::Immediate)?;
        if let Some(authority) = authority {
            verify_execution_authority_in(transaction.as_ref(), run_id, authority)?;
        }
        let row = controls_required(
            transaction.as_ref(),
            select("workflow_runs")
                .columns(&["status", "cancellation_requested_at_ms", "retry_cap"])
                .where_eq("run_id", run_id),
        )?;
        let run_status = database::text(&row, "status")?;
        let cancellation_requested =
            database::optional_unsigned(&row, "cancellation_requested_at_ms")?.is_some();
        let retry_cap = u32::try_from(database::unsigned(&row, "retry_cap")?)
            .map_err(|_| WorkflowStoreError::InvalidData("retry cap overflow".into()))?;
        if cancellation_requested || parse_run_status(&run_status)? != RunStatus::Failed {
            return Err(WorkflowStoreError::InvalidData(
                "workflow run is not eligible for node retry".to_string(),
            ));
        }
        let (activation_status, output_id, dependency_generation): (String, Option<String>, u64) =
            controls_required(
                transaction.as_ref(),
                select("workflow_activations")
                    .columns(&["status", "output_id", "dependency_generation"])
                    .where_eq("run_id", DatabaseValue::from(run_id))
                    .where_eq("node_id", DatabaseValue::from(node_id))
                    .where_eq("activation_id", DatabaseValue::from(activation_id)),
            )
            .and_then(|row| {
                Ok((
                    database::text(&row, "status")?,
                    database::optional_text(&row, "output_id")?,
                    controls_get(&row, "dependency_generation")?,
                ))
            })?;
        if activation_status != "failed" || output_id.is_some() {
            return Err(WorkflowStoreError::InvalidData(
                "workflow activation is not an output-free failed activation".to_string(),
            ));
        }
        let (latest_attempt, attempt_status): (u32, String) = controls_required(
            transaction.as_ref(),
            select("workflow_attempts")
                .columns(&["attempt", "status"])
                .where_eq("run_id", DatabaseValue::from(run_id))
                .where_eq("node_id", DatabaseValue::from(node_id))
                .where_eq("activation_id", DatabaseValue::from(activation_id))
                .sort("attempt", SortDirection::Desc)
                .limit(1),
        )
        .and_then(|row| {
            Ok((
                controls_get(&row, "attempt")?,
                database::text(&row, "status")?,
            ))
        })?;
        if latest_attempt != failed_attempt || attempt_status != "failed" {
            return Err(WorkflowStoreError::InvalidData(
                "workflow retry requires the latest failed attempt identity".to_string(),
            ));
        }
        let next_attempt = failed_attempt.saturating_add(1);
        if next_attempt > retry_cap.saturating_add(1) {
            return Err(WorkflowStoreError::InvalidData(
                "workflow retry cap exceeded".to_string(),
            ));
        }
        let graph_revision =
            run_graph::graph_revision(transaction.as_ref(), run_id)?.ok_or_else(|| {
                WorkflowStoreError::InvalidData("workflow retry graph is missing".to_string())
            })?;
        let mut cursor = None;
        loop {
            let edges = Self::current_run_graph_incident_edges_in(
                transaction.as_ref(),
                run_id,
                graph_revision,
                node_id,
                true,
                cursor,
                100,
            )?;
            if edges.is_empty() {
                break;
            }
            cursor = edges.last().map(|edge| edge.edge_id);
            for edge in edges {
                if matches!(
                    edge.edge.kind,
                    bcode_workflow::EdgeKind::Retry { .. } | bcode_workflow::EdgeKind::Back { .. }
                ) {
                    continue;
                }
                // Settled targets from earlier loop generations did not consume this
                // activation. Unresolved older targets remain unsafe to overlap.
                let downstream_exists = controls_one(
                    transaction.as_ref(),
                    select("workflow_activations")
                        .columns(&["activation_id"])
                        .where_eq("run_id", run_id)
                        .where_eq("node_id", &edge.edge.to)
                        .filter(Box::new(switchy_database::query::where_or(vec![
                            Box::new(switchy_database::query::where_gte(
                                "dependency_generation",
                                database::integer_value(dependency_generation)?,
                            )),
                            Box::new(switchy_database::query::where_not_in(
                                "status",
                                vec![
                                    DatabaseValue::from("completed"),
                                    DatabaseValue::from("failed"),
                                    DatabaseValue::from("cancelled"),
                                    DatabaseValue::from("skipped"),
                                ],
                            )),
                        ]))),
                )?
                .is_some();
                if downstream_exists {
                    return Err(WorkflowStoreError::InvalidData(
                        "workflow retry is unsafe after downstream activation".to_string(),
                    ));
                }
            }
        }
        block_on(
            transaction.exec_update_count(
                &(transaction
                    .update("workflow_activations")
                    .value("status", DatabaseValue::from("pending"))
                    .where_eq("run_id", DatabaseValue::from(run_id))
                    .where_eq("node_id", DatabaseValue::from(node_id))
                    .where_eq("activation_id", DatabaseValue::from(activation_id))
                    .where_eq("status", DatabaseValue::from("failed"))),
            ),
        )?;
        block_on(
            transaction.exec_update_count(
                &(transaction
                    .update("workflow_runs")
                    .value("status", DatabaseValue::from("running"))
                    .value("updated_at_ms", database::integer_value(retried_at_ms)?)
                    .where_eq("run_id", DatabaseValue::from(run_id))
                    .where_eq("status", DatabaseValue::from("failed"))),
            ),
        )?;
        let result = WorkflowNodeRetryResult {
            run_id: run_id.to_string(),
            node_id: node_id.to_string(),
            activation_id: activation_id.to_string(),
            previous_attempt: failed_attempt,
            next_attempt,
        };
        append_event_in(
            transaction.as_ref(),
            run_id,
            "node_retry_requested",
            &serde_json::to_string(&result)?,
            retried_at_ms,
        )?;
        if let Some(schedule) = retry_schedule {
            if schedule.failed_attempt != failed_attempt || schedule.next_attempt != next_attempt {
                return Err(WorkflowStoreError::InvalidData(
                    "automatic retry schedule next attempt became stale".to_string(),
                ));
            }
            let removed = block_on(
                transaction.exec_delete_count(
                    &(transaction
                        .delete("workflow_retry_schedules")
                        .where_eq("run_id", DatabaseValue::from(run_id))
                        .where_eq("node_id", DatabaseValue::from(node_id))
                        .where_eq("activation_id", DatabaseValue::from(activation_id))
                        .where_eq(
                            "failed_attempt",
                            DatabaseValue::from(schedule.failed_attempt),
                        )
                        .where_eq("next_attempt", DatabaseValue::from(schedule.next_attempt))),
                ),
            )?;
            if removed != 1 {
                return Err(WorkflowStoreError::InvalidData(
                    "automatic retry schedule changed before consumption".to_string(),
                ));
            }
            append_event_in(
                transaction.as_ref(),
                run_id,
                "automatic_retry_consumed",
                &serde_json::to_string(schedule)?,
                retried_at_ms,
            )?;
        }
        database::commit(transaction)?;
        Ok(result)
    }

    /// Settle bounded pending deterministic control nodes without external dispatch.
    ///
    /// Repeat nodes either complete the run when their predicate clears/limit is reached or create
    /// the next generation's body entry activations. Other host-neutral control nodes are settled
    /// by forwarding their input as validated output through the normal atomic successor path.
    ///
    /// # Errors
    ///
    /// Returns an error when bounded discovery, control configuration, schemas, cycle limits, or
    /// persistence invariants are invalid.
    pub fn settle_pending_control_nodes(
        &mut self,
        run_id: &str,
        limit: usize,
        settled_at_ms: u64,
    ) -> Result<ControlSettlementSummary, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        let pending = self.pending_activations_for_run(run_id, limit)?;
        self.settle_control_activations(pending, settled_at_ms)
    }

    pub(super) fn settle_control_activations(
        &mut self,
        pending: Vec<PendingActivation>,
        settled_at_ms: u64,
    ) -> Result<ControlSettlementSummary, WorkflowStoreError> {
        let mut summary = ControlSettlementSummary::default();
        for activation in pending.into_iter().filter(|activation| {
            matches!(
                activation.node.kind,
                bcode_workflow::NodeKind::Branch
                    | bcode_workflow::NodeKind::Repeat
                    | bcode_workflow::NodeKind::Retry
                    | bcode_workflow::NodeKind::Parallel
                    | bcode_workflow::NodeKind::FanOut
            )
        }) {
            let input = activation.input.clone().ok_or_else(|| {
                WorkflowStoreError::InvalidData(format!(
                    "workflow control node has no input: {}",
                    activation.node_id
                ))
            })?;
            if activation.node.kind == bcode_workflow::NodeKind::FanOut {
                let result = self.settle_fan_out_node(&activation, &input, settled_at_ms)?;
                summary.settled.push(activation.activation_id);
                summary.activated.extend(result);
                continue;
            }
            if activation.node.kind == bcode_workflow::NodeKind::Repeat {
                let result = self.settle_repeat_node(&activation, &input, settled_at_ms)?;
                summary.settled.push(activation.activation_id);
                summary.activated.extend(result);
                continue;
            }
            let output = ValidatedOutput {
                output_id: format!("{}:control-output", activation.activation_id),
                run_id: activation.run_id.clone(),
                node_id: activation.node_id.clone(),
                activation_id: activation.activation_id.clone(),
                schema_id: activation.node.output.type_name.clone(),
                schema_version: 1,
                value: input,
                artifact_reference: None,
                created_at_ms: settled_at_ms,
            };
            let result = self.persist_validated_output(&output)?;
            summary.settled.push(activation.activation_id);
            summary.activated.extend(result.activated);
        }
        Ok(summary)
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn settle_fan_out_node(
        &self,
        activation: &PendingActivation,
        input: &serde_json::Value,
        settled_at_ms: u64,
    ) -> Result<Vec<NewActivation>, WorkflowStoreError> {
        let configuration: bcode_workflow::WorkflowFanOutConfiguration =
            serde_json::from_value(activation.node.configuration.clone())?;
        configuration
            .validate()
            .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
        let members = input.as_array().ok_or_else(|| {
            WorkflowStoreError::InvalidData("fan-out activation input must be an array".to_string())
        })?;
        if members.len() > usize::try_from(configuration.max_members).unwrap_or(usize::MAX) {
            return Err(WorkflowStoreError::InvalidData(
                "fan-out member count exceeds the configured bound".to_string(),
            ));
        }
        let transaction = database::begin(&self.connection, TransactionMode::Immediate)?;
        let existing = controls_count(
            transaction.as_ref(),
            select("workflow_fan_out_members")
                .where_eq("run_id", &activation.run_id)
                .where_eq("controller_activation_id", &activation.activation_id),
        )?;
        if existing > 0 && existing != u32::try_from(members.len()).unwrap_or(u32::MAX) {
            return Err(WorkflowStoreError::InvalidData(
                "persisted fan-out member inventory conflicts with controller input".to_string(),
            ));
        }
        let changed = block_on(
            transaction.exec_update_count(
                &(transaction
                    .update("workflow_activations")
                    .value("status", DatabaseValue::from("running"))
                    .where_eq("run_id", DatabaseValue::from(&activation.run_id))
                    .where_eq("node_id", DatabaseValue::from(&activation.node_id))
                    .where_eq(
                        "activation_id",
                        DatabaseValue::from(&activation.activation_id),
                    )
                    .where_eq("status", DatabaseValue::from("pending"))),
            ),
        )?;
        if changed != 1 && existing == 0 {
            return Err(WorkflowStoreError::InvalidData(
                "fan-out controller is not pending for first materialization".to_string(),
            ));
        }
        let mut activated = Vec::new();
        for (index, value) in members.iter().enumerate() {
            let index = u32::try_from(index).map_err(|error| {
                WorkflowStoreError::InvalidData(format!("fan-out member index overflow: {error}"))
            })?;
            let member_node_id = fan_out_member_node_id(&activation.node_id, index);
            let member_activation_id = fan_out_member_activation_identity(
                &activation.run_id,
                &activation.activation_id,
                index,
            );
            let input_json = bounded_json("fan-out member input", value)?;
            let member_status = if index < configuration.max_concurrency {
                "pending"
            } else {
                "waiting"
            };
            let inserted = block_on(
                (transaction
                    .insert("workflow_fan_out_members")
                    .value("run_id", DatabaseValue::from(&activation.run_id))
                    .value(
                        "controller_node_id",
                        DatabaseValue::from(&activation.node_id),
                    )
                    .value(
                        "controller_activation_id",
                        DatabaseValue::from(&activation.activation_id),
                    )
                    .value("member_index", DatabaseValue::from(index))
                    .value("member_node_id", DatabaseValue::from(&member_node_id))
                    .value(
                        "member_activation_id",
                        DatabaseValue::from(&member_activation_id),
                    )
                    .value("status", DatabaseValue::from(member_status))
                    .value("input_json", DatabaseValue::from(&input_json))
                    .value("created_at_ms", database::integer_value(settled_at_ms)?))
                .on_conflict_do_nothing(["run_id", "controller_activation_id", "member_index"])
                .execute_count(transaction.as_ref()),
            )?;
            let activation_inserted = block_on(
                (transaction
                    .insert("workflow_activations")
                    .value("run_id", DatabaseValue::from(&activation.run_id))
                    .value("node_id", DatabaseValue::from(&member_node_id))
                    .value("activation_id", DatabaseValue::from(&member_activation_id))
                    .value(
                        "dependency_generation",
                        database::integer_value(u64::from(index))?,
                    )
                    .value("input_json", DatabaseValue::from(&input_json))
                    .value("status", DatabaseValue::from(member_status))
                    .value("created_at_ms", database::integer_value(settled_at_ms)?))
                .on_conflict_do_nothing(["run_id", "node_id", "activation_id"])
                .execute_count(transaction.as_ref()),
            )?;
            if activation_inserted == 1 {
                record_activation_graph_binding(
                    transaction.as_ref(),
                    &activation.run_id,
                    &member_node_id,
                    &member_activation_id,
                    1,
                )?;
            }
            if inserted == 1 {
                append_event_in(
                    transaction.as_ref(),
                    &activation.run_id,
                    "fan_out_member_created",
                    &serde_json::json!({
                        "controller_activation_id": activation.activation_id,
                        "member_index": index,
                        "member_activation_id": member_activation_id,
                    })
                    .to_string(),
                    settled_at_ms,
                )?;
            } else {
                let stored: (String, String) = controls_required(
                    transaction.as_ref(),
                    select("workflow_fan_out_members")
                        .columns(&["member_activation_id", "input_json"])
                        .where_eq("run_id", DatabaseValue::from(&activation.run_id))
                        .where_eq(
                            "controller_activation_id",
                            DatabaseValue::from(&activation.activation_id),
                        )
                        .where_eq("member_index", DatabaseValue::from(index)),
                )
                .and_then(|row| {
                    Ok((
                        database::text(&row, "member_activation_id")?,
                        database::text(&row, "input_json")?,
                    ))
                })?;
                if stored != (member_activation_id.clone(), input_json.clone()) {
                    return Err(WorkflowStoreError::InvalidData(
                        "fan-out duplicate member identity or input conflicts".to_string(),
                    ));
                }
            }
            if member_status == "pending" {
                activated.push(NewActivation {
                    run_id: activation.run_id.clone(),
                    node_id: member_node_id,
                    activation_id: member_activation_id,
                    dependency_generation: u64::from(index),
                    input: Some(value.clone()),
                    created_at_ms: settled_at_ms,
                });
            }
        }
        append_event_in(
            transaction.as_ref(),
            &activation.run_id,
            "fan_out_materialized",
            &serde_json::json!({
                "controller_activation_id": activation.activation_id,
                "member_count": members.len(),
                "max_concurrency": configuration.max_concurrency,
            })
            .to_string(),
            settled_at_ms,
        )?;
        database::commit(transaction)?;
        Ok(activated)
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn settle_repeat_node(
        &self,
        activation: &PendingActivation,
        input: &serde_json::Value,
        settled_at_ms: u64,
    ) -> Result<Vec<NewActivation>, WorkflowStoreError> {
        let transaction = database::begin(&self.connection, TransactionMode::Deferred)?;
        let graph_revision =
            run_graph::graph_revision(transaction.as_ref(), &activation.run_id)?
                .ok_or_else(|| WorkflowStoreError::InvalidData("repeat graph is missing".into()))?;
        run_graph::reconciled_activation_exit(
            transaction.as_ref(),
            &activation.run_id,
            &activation.node_id,
            &activation.activation_id,
        )?;
        let admitted: u64 = controls_required(
            transaction.as_ref(),
            select("workflow_activation_graph_bindings")
                .columns(&["graph_revision"])
                .where_eq("run_id", DatabaseValue::from(&activation.run_id))
                .where_eq(
                    "activation_id",
                    DatabaseValue::from(&activation.activation_id),
                ),
        )
        .and_then(|row| controls_get(&row, "graph_revision"))?;
        let predicate: bcode_workflow::PredicateExpression = serde_json::from_value(
            activation
                .node
                .configuration
                .get("predicate")
                .cloned()
                .ok_or_else(|| {
                    WorkflowStoreError::InvalidData(
                        "workflow repeat configuration is missing predicate".to_string(),
                    )
                })?,
        )?;
        let max_iterations = activation
            .node
            .configuration
            .get("max_iterations")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData(
                    "workflow repeat configuration is missing max_iterations".to_string(),
                )
            })?;
        let cycle_cap = u64::from(
            controls_required(
                transaction.as_ref(),
                select("workflow_runs")
                    .columns(&["cycle_cap"])
                    .where_eq("run_id", DatabaseValue::from(&activation.run_id)),
            )
            .and_then(|row| controls_get::<u32>(&row, "cycle_cap"))?,
        );
        if cycle_cap == 0 {
            return Err(WorkflowStoreError::InvalidData(
                "cycle_cap must be positive".to_string(),
            ));
        }
        let effective_iteration_bound = max_iterations.min(cycle_cap);
        let exhaustion_policy = activation
            .node
            .configuration
            .get("exhaustion_policy")
            .cloned()
            .map_or(
                Ok(bcode_workflow::WorkflowRepeatExhaustionPolicy::Fail),
                |value| serde_json::from_value(value).map_err(WorkflowStoreError::from),
            )?;
        let should_repeat = evaluate_predicate(&predicate, input)?;
        let next_generation = activation
            .dependency_generation
            .checked_add(1)
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData("workflow repeat generation overflow".to_string())
            })?;
        let within_iteration_bound = next_generation < effective_iteration_bound;
        let iteration_bound_exhausted = should_repeat && !within_iteration_bound;
        let emit_typed_outcome = exhaustion_policy
            == bcode_workflow::WorkflowRepeatExhaustionPolicy::EmitOutcome
            && (!should_repeat || iteration_bound_exhausted);
        let persisted_value = if emit_typed_outcome {
            serde_json::to_value(bcode_workflow::WorkflowRepeatOutcome {
                version: bcode_workflow::WORKFLOW_REPEAT_OUTCOME_VERSION,
                outcome: if iteration_bound_exhausted {
                    bcode_workflow::WorkflowRepeatOutcomeKind::IterationLimitReached
                } else {
                    bcode_workflow::WorkflowRepeatOutcomeKind::ConditionCleared
                },
                iterations_completed: next_generation,
                max_iterations,
                cycle_cap,
                effective_iteration_bound,
                value: input.clone(),
            })?
        } else {
            input.clone()
        };
        activation
            .node
            .output
            .validate_value("repeat.output", &persisted_value)
            .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
        block_on(
            transaction.exec_update_count(
                &(transaction
                    .update("workflow_activations")
                    .value("status", DatabaseValue::from("completed"))
                    .value(
                        "output_id",
                        DatabaseValue::from(format!("{}:control-output", activation.activation_id)),
                    )
                    .where_eq("run_id", DatabaseValue::from(&activation.run_id))
                    .where_eq("node_id", DatabaseValue::from(&activation.node_id))
                    .where_eq(
                        "activation_id",
                        DatabaseValue::from(&activation.activation_id),
                    )
                    .where_eq("status", DatabaseValue::from("pending"))),
            ),
        )?;
        let value_json = serde_json::to_string(&persisted_value)?;
        controls_insert(
            transaction.as_ref(),
            &(transaction
                .insert("workflow_outputs")
                .value(
                    "output_id",
                    DatabaseValue::from(format!("{}:control-output", activation.activation_id)),
                )
                .value("run_id", DatabaseValue::from(&activation.run_id))
                .value("node_id", DatabaseValue::from(&activation.node_id))
                .value(
                    "activation_id",
                    DatabaseValue::from(&activation.activation_id),
                )
                .value(
                    "schema_id",
                    DatabaseValue::from(&activation.node.output.type_name),
                )
                .value("schema_version", DatabaseValue::Int64(1))
                .value("value_json", DatabaseValue::from(&value_json))
                .value("artifact_reference", DatabaseValue::Null)
                .value(
                    "checksum_sha256",
                    DatabaseValue::from(sha256_hex(value_json.as_bytes())),
                )
                .value("created_at_ms", database::integer_value(settled_at_ms)?)),
        )?;
        let mut activated = Vec::new();
        if should_repeat && within_iteration_bound {
            let mut after_edge_id = None;
            loop {
                let edges = Self::current_run_graph_edges_in_snapshot(
                    transaction.as_ref(),
                    &activation.run_id,
                    graph_revision,
                    after_edge_id,
                    100,
                    Some(run_graph::EdgeEndpoint::Source(&activation.node_id)),
                )?;
                let Some(last) = edges.last() else { break };
                after_edge_id = Some(last.edge_id);
                for record in edges {
                    if admitted != graph_revision
                        && run_graph::retained_edge_activation(
                            transaction.as_ref(),
                            &activation.run_id,
                            graph_revision,
                            &record,
                        )?
                        .as_deref()
                            != Some(activation.activation_id.as_str())
                    {
                        return Err(WorkflowStoreError::InvalidData(
                            "repeat continuation requires an exact retained edge binding".into(),
                        ));
                    }
                    let edge = record.edge;
                    if !matches!(edge.kind, bcode_workflow::EdgeKind::Back { .. }) {
                        continue;
                    }
                    let target = Self::current_run_graph_node_in_snapshot(
                        transaction.as_ref(),
                        &activation.run_id,
                        &edge.to,
                    )?
                    .ok_or_else(|| {
                        WorkflowStoreError::InvalidData(format!(
                            "workflow repeat target is missing from run graph: {}",
                            edge.to
                        ))
                    })?;
                    let node = &target.node;
                    let transformed_input = if let Some(transform) = &edge.transform {
                        let run_input_json: Option<String> = controls_required(
                            transaction.as_ref(),
                            select("workflow_runs")
                                .columns(&["input_json"])
                                .where_eq("run_id", DatabaseValue::from(&activation.run_id)),
                        )
                        .and_then(|row| database::optional_text(&row, "input_json"))?;
                        let run_input = run_input_json
                            .as_deref()
                            .map(serde_json::from_str)
                            .transpose()?
                            .unwrap_or(serde_json::Value::Null);
                        transform
                            .evaluate(&[
                                bcode_workflow::WorkflowTransformInput {
                                    name: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_CURRENT,
                                    value: input,
                                },
                                bcode_workflow::WorkflowTransformInput {
                                    name: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_STATE,
                                    value: &run_input,
                                },
                            ])
                            .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?
                    } else {
                        input.clone()
                    };
                    let next = NewActivation {
                        run_id: activation.run_id.clone(),
                        node_id: edge.to.clone(),
                        activation_id: activation_identity(
                            &activation.run_id,
                            &edge.to,
                            next_generation,
                        ),
                        dependency_generation: next_generation,
                        input: Some(transformed_input.clone()),
                        created_at_ms: settled_at_ms,
                    };
                    validate_json_schema(
                        &format!("workflow repeat input for node {}", node.id),
                        &node.input.schema,
                        &transformed_input,
                    )
                    .map_err(|error| {
                        WorkflowStoreError::TargetInputValidation(Box::new(
                            TargetInputValidationFailure {
                                run_id: activation.run_id.clone(),
                                source_node_id: activation.node_id.clone(),
                                source_activation_id: activation.activation_id.clone(),
                                target_node_id: node.id.clone(),
                                target_schema_id: node.input.type_name.clone(),
                                message: error.to_string(),
                            },
                        ))
                    })?;
                    enforce_activation_limits(transaction.as_ref(), &next)?;
                    insert_activation_bound_to_node(
                        transaction.as_ref(),
                        &next,
                        activation_status_for_node(node),
                        node,
                        target.revision,
                    )?;
                    record_activation_graph_binding(
                        transaction.as_ref(),
                        &next.run_id,
                        &next.node_id,
                        &next.activation_id,
                        graph_revision,
                    )?;
                    activated.push(next);
                }
            }
        } else if iteration_bound_exhausted
            && exhaustion_policy == bcode_workflow::WorkflowRepeatExhaustionPolicy::Fail
        {
            block_on(
                transaction.exec_update_count(
                    &(transaction
                        .update("workflow_runs")
                        .value("status", DatabaseValue::from("failed"))
                        .value("updated_at_ms", database::integer_value(settled_at_ms)?)
                        .where_eq("run_id", DatabaseValue::from(&activation.run_id))
                        .where_eq("status", DatabaseValue::from("running"))),
                ),
            )?;
            append_event_in(
                transaction.as_ref(),
                &activation.run_id,
                "run_failed",
                &serde_json::json!({
                    "node_id": activation.node_id,
                    "reason": "repeat_iteration_limit_exhausted",
                    "max_iterations": max_iterations,
                    "cycle_cap": cycle_cap,
                    "effective_iteration_bound": effective_iteration_bound,
                    "generation": activation.dependency_generation,
                })
                .to_string(),
                settled_at_ms,
            )?;
        } else {
            let output = ValidatedOutput {
                output_id: format!("{}:control-output", activation.activation_id),
                run_id: activation.run_id.clone(),
                node_id: activation.node_id.clone(),
                activation_id: activation.activation_id.clone(),
                schema_id: activation.node.output.type_name.clone(),
                schema_version: 1,
                value: persisted_value,
                artifact_reference: None,
                created_at_ms: settled_at_ms,
            };
            activated = settle_output_successors(
                transaction.as_ref(),
                &output,
                &sha256_hex(value_json.as_bytes()),
                &NoopWorkflowOutputFault,
            )?
            .activated;
        }
        append_event_in(
            transaction.as_ref(),
            &activation.run_id,
            "control_node_settled",
            &serde_json::json!({
                "node_id": activation.node_id,
                "activation_id": activation.activation_id,
                "repeat": should_repeat && within_iteration_bound,
                "generation": activation.dependency_generation,
                "iterations_completed": next_generation,
                "next_generation": should_repeat.then_some(next_generation),
                "max_iterations": max_iterations,
                "cycle_cap": cycle_cap,
                "effective_iteration_bound": effective_iteration_bound,
                "iteration_bound_exhausted": iteration_bound_exhausted,
                "exhaustion_policy": exhaustion_policy,
                "outcome": emit_typed_outcome.then_some(if iteration_bound_exhausted {
                    bcode_workflow::WorkflowRepeatOutcomeKind::IterationLimitReached
                } else {
                    bcode_workflow::WorkflowRepeatOutcomeKind::ConditionCleared
                }),
            })
            .to_string(),
            settled_at_ms,
        )?;
        database::commit(transaction)?;
        Ok(activated)
    }

    /// Persist cancellation intent before an executor signals active children.
    ///
    /// The returned value indicates whether this call recorded the first cancellation request.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, missing/terminal run, or database failure.
    pub fn request_cancellation(
        &mut self,
        run_id: &str,
        requested_at_ms: u64,
    ) -> Result<bool, WorkflowStoreError> {
        self.request_cancellation_with_authority(run_id, requested_at_ms, None)
    }

    /// Record inherited cancellation only while the durable direct parent still requests it.
    /// The child authority is required independently; parent ownership grants nothing.
    ///
    /// # Errors
    /// Rejects stale child authority, invalid links, missing parent state, or storage failure.
    pub fn inherit_parent_cancellation_owned(
        &mut self,
        run_id: &str,
        authority: &WorkflowExecutionAuthority,
        now_ms: u64,
    ) -> Result<bool, WorkflowStoreError> {
        let tx = database::begin(&self.connection, TransactionMode::Immediate)?;
        verify_store_schema(tx.as_ref())?;
        verify_execution_authority_in(tx.as_ref(), run_id, authority)?;
        let Some(link) = controls_one(
            tx.as_ref(),
            select("workflow_run_links")
                .columns(&["parent_run_id"])
                .where_eq("child_run_id", run_id),
        )?
        else {
            return Ok(false);
        };
        let parent_id = database::text(&link, "parent_run_id")?;
        let parent = run_summary_in(tx.as_ref(), &parent_id)?
            .ok_or_else(|| WorkflowStoreError::InvalidData("child has missing parent".into()))?;
        if parent.cancellation_requested_at_ms.is_none() {
            return Ok(false);
        }
        let changed = request_run_cancellation_in_transaction(tx.as_ref(), run_id, now_ms)?;
        database::commit(tx)?;
        Ok(changed)
    }

    /// Persist cancellation intent under the initiating caller's execution authority.
    ///
    /// # Errors
    /// Rejects stale authority, invalid run identity, or database failure.
    pub fn request_cancellation_owned(
        &mut self,
        run_id: &str,
        requested_at_ms: u64,
        authority: &WorkflowExecutionAuthority,
    ) -> Result<bool, WorkflowStoreError> {
        self.request_cancellation_with_authority(run_id, requested_at_ms, Some(authority))
    }

    pub(super) fn request_cancellation_with_authority(
        &self,
        run_id: &str,
        requested_at_ms: u64,
        authority: Option<&WorkflowExecutionAuthority>,
    ) -> Result<bool, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        let transaction = database::begin(&self.connection, TransactionMode::Immediate)?;
        verify_store_schema(transaction.as_ref())?;
        if let Some(authority) = authority {
            verify_execution_authority_in(transaction.as_ref(), run_id, authority)?;
        }
        let changed =
            request_run_cancellation_in_transaction(transaction.as_ref(), run_id, requested_at_ms)?;
        database::commit(transaction)?;
        Ok(changed)
    }

    /// Persist cancellation intent for one root and every bounded nonterminal descendant.
    ///
    /// Descendants are selected only from durable run links. All intents commit before callers
    /// signal any external owner. The returned run identities include only runs that were
    /// nonterminal when this operation inspected them; terminal descendants are omitted because
    /// they have no cancellation intent to authorize external signalling.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, a missing/terminal root, excessive descendants,
    /// or database failure.
    pub fn request_cancellation_tree(
        &mut self,
        run_id: &str,
        requested_at_ms: u64,
    ) -> Result<(bool, Vec<String>), WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        let transaction = database::begin(&self.connection, TransactionMode::Immediate)?;
        verify_store_schema(transaction.as_ref())?;
        let descendants = controls_descendants_in(
            transaction.as_ref(),
            run_id,
            MAX_WORKFLOW_RUN_DESCENDANTS as usize,
        )?;
        let mut candidates = Vec::with_capacity(descendants.len().saturating_add(1));
        candidates.push((
            run_id.to_string(),
            run_summary_in(transaction.as_ref(), run_id)?
                .ok_or_else(|| WorkflowStoreError::RunNotFound {
                    run_id: run_id.to_string(),
                })?
                .status,
        ));
        candidates.extend(
            descendants
                .into_iter()
                .map(|descendant| (descendant.run.run_id, descendant.run.status)),
        );
        let mut recorded = false;
        let mut run_ids = Vec::with_capacity(candidates.len());
        for (target_run_id, status) in candidates {
            if matches!(status, RunStatus::Running | RunStatus::Paused) {
                recorded |= request_run_cancellation_in_transaction(
                    transaction.as_ref(),
                    &target_run_id,
                    requested_at_ms,
                )?;
                run_ids.push(target_run_id);
            }
        }
        database::commit(transaction)?;
        Ok((recorded, run_ids))
    }

    /// Cancel a prepared operation only when durable handoff evidence proves no dispatch.
    ///
    /// # Errors
    /// Rejects stale authority, absent cancellation intent, malformed identity, or storage failure.
    /// Returns false when evidence is absent or the operation may have been accepted.
    pub fn cancel_never_dispatched_owned(
        &mut self,
        dispatch_identity: &str,
        authority: &WorkflowExecutionAuthority,
        now_ms: u64,
    ) -> Result<bool, WorkflowStoreError> {
        validate_id("dispatch_identity", dispatch_identity)?;
        let tx = database::begin(&self.connection, TransactionMode::Immediate)?;
        verify_store_schema(tx.as_ref())?;
        let handoff = E::exists(
            "workflow_dispatch_handoffs",
            E::And(vec![
                controls_same(
                    "workflow_dispatch_handoffs",
                    "dispatch_identity",
                    "workflow_attempts",
                    "dispatch_identity",
                ),
                E::Compare(
                    Box::new(controls_column("workflow_dispatch_handoffs", "handed_off")),
                    C::Equal,
                    Box::new(E::Integer(0)),
                ),
            ]),
        );
        let candidate = controls_one(
            tx.as_ref(),
            select("workflow_attempts")
                .columns(&["run_id", "node_id", "activation_id"])
                .where_eq("dispatch_identity", dispatch_identity)
                .where_eq("status", "prepared")
                .where_eq("receipt_json", DatabaseValue::Null)
                .filter(Box::new(schema_predicate(&handoff)?)),
        )?
        .map(|row| {
            Ok::<_, WorkflowStoreError>((
                database::text(&row, "run_id")?,
                database::text(&row, "node_id")?,
                database::text(&row, "activation_id")?,
            ))
        })
        .transpose()?;
        let Some((run_id, node_id, activation_id)) = candidate else {
            return Ok(false);
        };
        verify_execution_authority_in(tx.as_ref(), &run_id, authority)?;
        require_cancellation_requested(tx.as_ref(), &run_id)?;
        block_on(
            tx.as_ref().exec_update_count(
                &(tx.as_ref()
                    .update("workflow_attempts")
                    .value("status", DatabaseValue::from("cancelled"))
                    .value("terminal_at_ms", database::integer_value(now_ms)?)
                    .where_eq("dispatch_identity", DatabaseValue::from(dispatch_identity))),
            ),
        )?;
        block_on(
            tx.as_ref().exec_update_count(
                &(tx.as_ref()
                    .update("workflow_activations")
                    .value("status", DatabaseValue::from("cancelled"))
                    .where_eq("run_id", DatabaseValue::from(&run_id))
                    .where_eq("node_id", DatabaseValue::from(node_id))
                    .where_eq("activation_id", DatabaseValue::from(activation_id))
                    .where_eq("output_id", DatabaseValue::Null)
                    .where_eq("status", DatabaseValue::from("running"))),
            ),
        )?;
        append_event_in(
            tx.as_ref(),
            &run_id,
            "attempt_cancelled",
            &serde_json::json!({
                "dispatch_identity": dispatch_identity, "reason": "never_dispatched"
            })
            .to_string(),
            now_ms,
        )?;
        finalize_run_cancellation_if_settled(tx.as_ref(), &run_id, now_ms)?;
        database::commit(tx)?;
        Ok(true)
    }

    /// Return bounded active attempts only after cancellation intent is durable.
    ///
    /// Receipts are included when present so the owner can route cancellation to the exact turn or
    /// plugin operation. Receipt-less prepared attempts remain discoverable by stable identity.
    ///
    /// # Errors
    ///
    /// Returns an error when the run identity/limit is invalid, cancellation was not persisted,
    /// receipt JSON is malformed, or the bounded query fails.
    pub fn active_attempt_cancellations(
        &self,
        run_id: &str,
        limit: usize,
    ) -> Result<Vec<ActiveAttemptCancellation>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        let limit = controls_limit(bounded_limit(limit)?)?;
        let db: &dyn Database = &self.connection;
        require_cancellation_requested(db, run_id)?;
        let query = controls_cancellation_query()
            .where_eq("run_id", run_id)
            .where_in(
                "status",
                vec![
                    DatabaseValue::from("prepared"),
                    DatabaseValue::from("admitted"),
                    DatabaseValue::from("running"),
                ],
            )
            .sort("prepared_at_ms", SortDirection::Asc)
            .sort("dispatch_identity", SortDirection::Asc)
            .limit(limit);
        database::rows(db, &query)?
            .iter()
            .map(|row| {
                controls_validate_cancellation_receipt(row)
                    .and_then(active_attempt_cancellation_row)
            })
            .map(|row| row.and_then(active_attempt_cancellation))
            .collect()
    }

    /// Whether an exact attempt has a durable publication cancellation intent.
    ///
    /// # Errors
    /// Rejects invalid identities or database failures. This read never changes intent.
    pub fn has_publication_cancellation_intent(
        &self,
        run_id: &str,
        dispatch_identity: &str,
    ) -> Result<bool, WorkflowStoreError> {
        Ok(controls_one(
            &self.connection,
            select("workflow_publication_cancellations")
                .columns(&["dispatch_identity"])
                .where_eq("run_id", run_id)
                .where_eq("dispatch_identity", dispatch_identity),
        )?
        .is_some())
    }

    /// Discover exact accepted publication cancellations under held execution authority.
    ///
    /// A changed graph revision does not revoke previously accepted cancellation. Results
    /// exclude terminal attempts and remain bounded; receipt identities route owner signalling.
    ///
    /// # Errors
    /// Rejects stale authority, invalid limits, malformed receipts, or database failures.
    pub fn pending_publication_cancellations(
        &self,
        run_id: &str,
        authority: &WorkflowExecutionAuthority,
        limit: usize,
    ) -> Result<Vec<ActiveAttemptCancellation>, WorkflowStoreError> {
        self.pending_publication_cancellations_after(run_id, authority, None, limit)
    }

    /// Discover a keyset page of accepted cancellations after a dispatch identity.
    ///
    /// # Errors
    /// Rejects stale authority, invalid cursors/limits, malformed receipts, or database failure.
    pub fn pending_publication_cancellations_after(
        &self,
        run_id: &str,
        authority: &WorkflowExecutionAuthority,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ActiveAttemptCancellation>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        if let Some(cursor) = after {
            validate_id("dispatch cursor", cursor)?;
        }
        let limit = controls_limit(bounded_limit(limit)?)?;
        let tx = database::begin(&self.connection, TransactionMode::Deferred)?;
        let db = tx.as_ref();
        verify_execution_authority_in(db, run_id, authority)?;
        let query = controls_cancellation_query()
            .where_eq("run_id", run_id)
            .where_in(
                "status",
                vec![
                    DatabaseValue::from("prepared"),
                    DatabaseValue::from("admitted"),
                    DatabaseValue::from("running"),
                    DatabaseValue::from("cancelling"),
                ],
            )
            .sort("dispatch_identity", SortDirection::Asc)
            .limit(limit)
            .where_gt("dispatch_identity", after.unwrap_or(""))
            .filter(Box::new(schema_predicate(&E::exists(
                "workflow_publication_cancellations",
                E::And(vec![
                    controls_same(
                        "workflow_publication_cancellations",
                        "run_id",
                        "workflow_attempts",
                        "run_id",
                    ),
                    controls_same(
                        "workflow_publication_cancellations",
                        "dispatch_identity",
                        "workflow_attempts",
                        "dispatch_identity",
                    ),
                ]),
            ))?));
        database::rows(db, &query)?
            .iter()
            .map(|row| {
                controls_validate_cancellation_receipt(row)
                    .and_then(active_attempt_cancellation_row)
            })
            .map(|row| row.and_then(active_attempt_cancellation))
            .collect()
    }

    /// Read a keyset page of active attempts covered by durable run cancellation.
    ///
    /// # Errors
    /// Rejects stale authority, invalid bounds, malformed receipts, or database failures.
    pub fn owned_run_cancellations_after(
        &self,
        run_id: &str,
        authority: &WorkflowExecutionAuthority,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ActiveAttemptCancellation>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        if let Some(cursor) = after {
            validate_id("dispatch cursor", cursor)?;
        }
        let limit = controls_limit(bounded_limit(limit)?)?;
        let tx = database::begin(&self.connection, TransactionMode::Deferred)?;
        let db = tx.as_ref();
        verify_execution_authority_in(db, run_id, authority)?;
        let query = controls_cancellation_query()
            .where_eq("run_id", run_id)
            .where_in(
                "status",
                vec![
                    DatabaseValue::from("prepared"),
                    DatabaseValue::from("admitted"),
                    DatabaseValue::from("running"),
                    DatabaseValue::from("cancelling"),
                ],
            )
            .sort("dispatch_identity", SortDirection::Asc)
            .limit(limit)
            .where_gt("dispatch_identity", after.unwrap_or(""))
            .filter(Box::new(schema_predicate(&controls_run_cancelled())?));
        database::rows(db, &query)?
            .iter()
            .map(|row| {
                controls_validate_cancellation_receipt(row)
                    .and_then(active_attempt_cancellation_row)
            })
            .map(|row| row.and_then(active_attempt_cancellation))
            .collect()
    }

    /// Mark signalling of an accepted publication cancellation under held authority.
    ///
    /// # Errors
    /// Rejects stale authority, absent intent, malformed identity, or database failure.
    pub fn mark_publication_cancellation_signalled(
        &self,
        run_id: &str,
        dispatch_identity: &str,
        authority: &WorkflowExecutionAuthority,
        signalled_at_ms: u64,
    ) -> Result<bool, WorkflowStoreError> {
        let transaction = database::begin(&self.connection, TransactionMode::Deferred)?;
        verify_execution_authority_in(transaction.as_ref(), run_id, authority)?;
        let accepted = controls_one(
            transaction.as_ref(),
            select("workflow_publication_cancellations")
                .columns(&["dispatch_identity"])
                .where_eq("run_id", run_id)
                .where_eq("dispatch_identity", dispatch_identity),
        )
        .map(|row| row.is_some())?;
        if !accepted {
            return Err(WorkflowStoreError::InvalidData(
                "publication cancellation intent missing".into(),
            ));
        }
        let changed = block_on(
            transaction.exec_update_count(
                &(transaction
                    .update("workflow_attempts")
                    .value("status", DatabaseValue::from("cancelling"))
                    .where_eq("run_id", DatabaseValue::from(run_id))
                    .where_eq("dispatch_identity", DatabaseValue::from(dispatch_identity))
                    .where_in(
                        "status",
                        vec![
                            DatabaseValue::from("prepared"),
                            DatabaseValue::from("admitted"),
                            DatabaseValue::from("running"),
                        ],
                    )),
            ),
        )?;
        if changed == 1 {
            append_event_in(
                transaction.as_ref(),
                run_id,
                "attempt_cancellation_signalled",
                &serde_json::json!({"dispatch_identity": dispatch_identity}).to_string(),
                signalled_at_ms,
            )?;
        }
        database::commit(transaction)?;
        Ok(changed == 1)
    }

    /// Return bounded fail-fast sibling cancellation intents that still need owner signaling.
    ///
    /// # Errors
    ///
    /// Returns an error when the bound is invalid, receipt data is malformed, or the query fails.
    pub fn pending_sibling_cancellations(
        &self,
        limit: usize,
    ) -> Result<Vec<ActiveAttemptCancellation>, WorkflowStoreError> {
        let limit = controls_limit(bounded_limit(limit)?)?;
        let db: &dyn Database = &self.connection;
        let query = controls_cancellation_query()
            .where_in("status", vec![DatabaseValue::from("sibling_cancelling")])
            .sort("prepared_at_ms", SortDirection::Asc)
            .sort("dispatch_identity", SortDirection::Asc)
            .limit(limit);
        database::rows(db, &query)?
            .iter()
            .map(|row| {
                controls_validate_cancellation_receipt(row)
                    .and_then(active_attempt_cancellation_row)
            })
            .map(|row| row.and_then(active_attempt_cancellation))
            .collect()
    }

    /// Return bounded sibling cancellation intents for one owned run.
    ///
    /// # Errors
    /// Rejects stale authority, invalid bounds, malformed receipts, or storage failure.
    pub fn pending_owned_sibling_cancellations(
        &self,
        run_id: &str,
        authority: &WorkflowExecutionAuthority,
        limit: usize,
    ) -> Result<Vec<ActiveAttemptCancellation>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        let limit = controls_limit(bounded_limit(limit)?)?;
        let tx = database::begin(&self.connection, TransactionMode::Deferred)?;
        let db = tx.as_ref();
        verify_execution_authority_in(db, run_id, authority)?;
        let query = controls_cancellation_query()
            .where_eq("run_id", run_id)
            .where_in("status", vec![DatabaseValue::from("sibling_cancelling")])
            .sort("prepared_at_ms", SortDirection::Asc)
            .sort("dispatch_identity", SortDirection::Asc)
            .limit(limit);
        database::rows(db, &query)?
            .iter()
            .map(|row| {
                controls_validate_cancellation_receipt(row)
                    .and_then(active_attempt_cancellation_row)
            })
            .map(|row| row.and_then(active_attempt_cancellation))
            .collect()
    }

    /// Settle cancellation for an attempt whose process-local owner is absent.
    ///
    /// Receipt-less/read-only attempts can be cancelled without external ambiguity. Any mutating
    /// attempt instead becomes repair-required because its external outcome cannot be proven after
    /// owner loss.
    ///
    /// A receipt-less attempt with durable handoff evidence is left unchanged: absence
    /// from a runtime registry does not prove that owner admission has finished. Such
    /// attempts remain discoverable for cancellation after their receipt arrives.
    ///
    /// # Errors
    ///
    /// Returns an error when the attempt is missing, cancellation was not requested, or the
    /// durable transition fails.
    pub fn settle_orphaned_attempt_cancellation(
        &mut self,
        dispatch_identity: &str,
        settled_at_ms: u64,
    ) -> Result<RunStatus, WorkflowStoreError> {
        self.settle_orphaned_attempt_cancellation_with_authority(
            dispatch_identity,
            settled_at_ms,
            None,
        )
    }

    /// Reconcile an unavailable cancellation owner under the caller's held authority.
    ///
    /// # Errors
    /// Rejects stale authority, missing cancellation intent, or database failure.
    pub fn settle_orphaned_attempt_cancellation_owned(
        &mut self,
        dispatch_identity: &str,
        settled_at_ms: u64,
        authority: &WorkflowExecutionAuthority,
    ) -> Result<RunStatus, WorkflowStoreError> {
        self.settle_orphaned_attempt_cancellation_with_authority(
            dispatch_identity,
            settled_at_ms,
            Some(authority),
        )
    }

    pub(super) fn settle_orphaned_attempt_cancellation_with_authority(
        &self,
        dispatch_identity: &str,
        settled_at_ms: u64,
        authority: Option<&WorkflowExecutionAuthority>,
    ) -> Result<RunStatus, WorkflowStoreError> {
        validate_id("dispatch_identity", dispatch_identity)?;
        let transaction = database::begin(&self.connection, TransactionMode::Immediate)?;
        verify_store_schema(transaction.as_ref())?;
        let (run_id, node_id, activation_id, status, side_effect) = controls_required(
            transaction.as_ref(),
            select("workflow_attempts")
                .columns(&[
                    "run_id",
                    "node_id",
                    "activation_id",
                    "status",
                    "side_effect",
                ])
                .where_eq("dispatch_identity", DatabaseValue::from(dispatch_identity)),
        )
        .and_then(|row| {
            Ok((
                database::text(&row, "run_id")?,
                database::text(&row, "node_id")?,
                database::text(&row, "activation_id")?,
                database::text(&row, "status")?,
                database::text(&row, "side_effect")?,
            ))
        })?;
        if let Some(authority) = authority {
            verify_execution_authority_in(transaction.as_ref(), &run_id, authority)?;
        }
        require_cancellation_requested(transaction.as_ref(), &run_id)?;
        if !matches!(
            status.as_str(),
            "prepared" | "admitted" | "running" | "cancelling" | "sibling_cancelling"
        ) {
            let run_status = controls_required(
                transaction.as_ref(),
                select("workflow_runs")
                    .columns(&["status"])
                    .where_eq("run_id", DatabaseValue::from(&run_id)),
            )
            .and_then(|row| database::text(&row, "status"))?;
            return parse_run_status(&run_status);
        }
        // A missing runtime entry is not proof of orphanhood while owner admission
        // is between durable handoff and receipt commit. Keep cancellation discoverable
        // until the owner supplies evidence; do not destroy the late receipt's target.
        if cancellation_admission_in_flight(transaction.as_ref(), dispatch_identity)? {
            let run_status: String = controls_required(
                transaction.as_ref(),
                select("workflow_runs")
                    .columns(&["status"])
                    .where_eq("run_id", DatabaseValue::from(&run_id)),
            )
            .and_then(|row| database::text(&row, "status"))?;
            return parse_run_status(&run_status);
        }
        if parse_side_effect(&side_effect)? == DispatchSideEffect::Mutating {
            mark_orphaned_attempt_repair_required(
                transaction.as_ref(),
                &run_id,
                dispatch_identity,
                settled_at_ms,
            )?;
            database::commit(transaction)?;
            return Ok(RunStatus::RepairRequired);
        }
        let run_status = mark_orphaned_attempt_cancelled(
            transaction.as_ref(),
            &run_id,
            &node_id,
            &activation_id,
            dispatch_identity,
            settled_at_ms,
        )?;
        database::commit(transaction)?;
        Ok(run_status)
    }

    /// Mark one successfully signalled active attempt as cancelling.
    ///
    /// This is the durable second half of host-side two-phase cancellation propagation. Calling it
    /// again for an already-cancelling attempt is idempotent and does not append a duplicate event.
    /// Terminal attempts are reported as unchanged.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, missing cancellation intent, or database failure.
    pub fn mark_cancellation_signalled(
        &mut self,
        dispatch_identity: &str,
        signalled_at_ms: u64,
    ) -> Result<bool, WorkflowStoreError> {
        self.mark_cancellation_signalled_with_requirement(
            dispatch_identity,
            signalled_at_ms,
            true,
            None,
        )
    }

    /// Persist successful cancellation signalling only under the caller's held authority.
    ///
    /// # Errors
    /// Rejects stale authority, missing intent, invalid identity, or database failure.
    pub fn mark_cancellation_signalled_owned(
        &mut self,
        dispatch_identity: &str,
        signalled_at_ms: u64,
        authority: &WorkflowExecutionAuthority,
    ) -> Result<bool, WorkflowStoreError> {
        self.mark_cancellation_signalled_with_requirement(
            dispatch_identity,
            signalled_at_ms,
            true,
            Some(authority),
        )
    }

    /// Mark a fail-fast sibling attempt as signalled after its durable attempt-local intent.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed/missing attempt identity or database failure.
    pub fn mark_sibling_cancellation_signalled(
        &mut self,
        dispatch_identity: &str,
        signalled_at_ms: u64,
    ) -> Result<bool, WorkflowStoreError> {
        self.mark_cancellation_signalled_with_requirement(
            dispatch_identity,
            signalled_at_ms,
            false,
            None,
        )
    }

    /// Persist sibling cancellation signalling under current durable authority.
    ///
    /// # Errors
    /// Rejects stale authority, missing attempt identity, or database failure.
    pub fn mark_sibling_cancellation_signalled_owned(
        &mut self,
        dispatch_identity: &str,
        signalled_at_ms: u64,
        authority: &WorkflowExecutionAuthority,
    ) -> Result<bool, WorkflowStoreError> {
        self.mark_cancellation_signalled_with_requirement(
            dispatch_identity,
            signalled_at_ms,
            false,
            Some(authority),
        )
    }

    pub(super) fn mark_cancellation_signalled_with_requirement(
        &self,
        dispatch_identity: &str,
        signalled_at_ms: u64,
        require_run_cancellation: bool,
        authority: Option<&WorkflowExecutionAuthority>,
    ) -> Result<bool, WorkflowStoreError> {
        validate_id("dispatch_identity", dispatch_identity)?;
        let transaction = database::begin(&self.connection, TransactionMode::Immediate)?;
        verify_store_schema(transaction.as_ref())?;
        let run_id = controls_one(
            transaction.as_ref(),
            select("workflow_attempts")
                .columns(&["run_id"])
                .where_eq("dispatch_identity", DatabaseValue::from(dispatch_identity)),
        )?
        .as_ref()
        .map(|row| database::text(row, "run_id"))
        .transpose()?
        .ok_or_else(|| {
            WorkflowStoreError::InvalidData(format!(
                "workflow attempt not found: {dispatch_identity}"
            ))
        })?;
        if let Some(authority) = authority {
            verify_execution_authority_in(transaction.as_ref(), &run_id, authority)?;
        }
        if require_run_cancellation {
            require_cancellation_requested(transaction.as_ref(), &run_id)?;
        }
        let changed = if require_run_cancellation {
            block_on(
                transaction.exec_update_count(
                    &(transaction
                        .update("workflow_attempts")
                        .value("status", DatabaseValue::from("cancelling"))
                        .where_eq("dispatch_identity", DatabaseValue::from(dispatch_identity))
                        .where_in(
                            "status",
                            vec![
                                DatabaseValue::from("prepared"),
                                DatabaseValue::from("admitted"),
                                DatabaseValue::from("running"),
                            ],
                        )),
                ),
            )?
        } else {
            block_on(
                transaction.exec_update_count(
                    &(transaction
                        .update("workflow_attempts")
                        .value("status", DatabaseValue::from("cancelling"))
                        .where_eq("dispatch_identity", DatabaseValue::from(dispatch_identity))
                        .where_eq("status", DatabaseValue::from("sibling_cancelling"))),
                ),
            )?
        };
        if changed == 1 {
            append_event_in(
                transaction.as_ref(),
                &run_id,
                "attempt_cancellation_signalled",
                &serde_json::json!({"dispatch_identity": dispatch_identity}).to_string(),
                signalled_at_ms,
            )?;
        }
        database::commit(transaction)?;
        Ok(changed == 1)
    }

    /// Signal active attempt owners after durable cancellation intent is confirmed.
    ///
    /// Signaling occurs outside a database transaction. Successful signals are marked
    /// `cancelling`; a crash before that marker is safe because owner signaling must be idempotent
    /// and the attempt remains discoverable.
    ///
    /// # Errors
    ///
    /// Returns an error when discovery, owner signaling, or durable marking fails. Unsignalled
    /// attempts remain discoverable for a later retry.
    pub async fn propagate_cancellation<O>(
        &mut self,
        run_id: &str,
        owner: &O,
        limit: usize,
        signalled_at_ms: u64,
    ) -> Result<CancellationPropagationSummary, WorkflowStoreError>
    where
        O: AttemptCancellationOwner + ?Sized,
    {
        let attempts = self.active_attempt_cancellations(run_id, limit)?;
        let mut summary = CancellationPropagationSummary::default();
        for attempt in attempts {
            owner.cancel_attempt(&attempt).await?;
            if self.mark_cancellation_signalled(&attempt.dispatch_identity, signalled_at_ms)? {
                summary.signalled.push(attempt.dispatch_identity);
            } else {
                summary.already_terminal.push(attempt.dispatch_identity);
            }
        }
        Ok(summary)
    }

    /// Return dispatch identities for active children after cancellation intent is durable.
    ///
    /// New owner routers should use [`Self::active_attempt_cancellations`] to retain receipts.
    ///
    /// # Errors
    ///
    /// Returns an error when the run identity/limit is invalid, cancellation was not persisted,
    /// receipt JSON is malformed, or the bounded query fails.
    pub fn active_attempts_for_cancellation(
        &self,
        run_id: &str,
        limit: usize,
    ) -> Result<Vec<String>, WorkflowStoreError> {
        self.active_attempt_cancellations(run_id, limit)
            .map(|attempts| {
                attempts
                    .into_iter()
                    .map(|attempt| attempt.dispatch_identity)
                    .collect()
            })
    }
}

pub fn cancellation_admission_in_flight(
    connection: &dyn Database,
    dispatch_identity: &str,
) -> Result<bool, WorkflowStoreError> {
    let handoff = E::exists(
        "workflow_dispatch_handoffs",
        E::And(vec![
            controls_same(
                "workflow_dispatch_handoffs",
                "dispatch_identity",
                "workflow_attempts",
                "dispatch_identity",
            ),
            E::Compare(
                Box::new(controls_column("workflow_dispatch_handoffs", "handed_off")),
                C::Equal,
                Box::new(E::Integer(1)),
            ),
        ]),
    );
    Ok(controls_one(
        connection,
        select("workflow_attempts")
            .columns(&["dispatch_identity"])
            .where_eq("dispatch_identity", dispatch_identity)
            .where_eq("receipt_json", DatabaseValue::Null)
            .filter(Box::new(schema_predicate(&handoff)?)),
    )?
    .is_some())
}

pub fn sibling_cancellation_requested_for_attempt(
    connection: &dyn Database,
    dispatch_identity: &str,
) -> Result<bool, WorkflowStoreError> {
    let Some(row) = controls_one(
        connection,
        select("workflow_attempts")
            .columns(&["run_id", "status"])
            .where_eq("dispatch_identity", dispatch_identity),
    )?
    else {
        return Ok(false);
    };
    let run_id = database::text(&row, "run_id")?;
    let status = database::text(&row, "status")?;
    let Some(run) = controls_one(
        connection,
        select("workflow_runs")
            .columns(&["cancellation_requested_at_ms"])
            .where_eq("run_id", run_id),
    )?
    else {
        return Ok(false);
    };
    let publication = controls_one(
        connection,
        select("workflow_publication_cancellations")
            .columns(&["dispatch_identity"])
            .where_eq("dispatch_identity", dispatch_identity),
    )?
    .is_some();
    Ok(status == "sibling_cancelling"
        || publication
        || (status == "cancelling"
            && database::optional_unsigned(&run, "cancellation_requested_at_ms")?.is_none()))
}

pub fn require_cancellation_requested(
    connection: &dyn Database,
    run_id: &str,
) -> Result<(), WorkflowStoreError> {
    let row = controls_one(
        connection,
        select("workflow_runs")
            .columns(&["cancellation_requested_at_ms"])
            .where_eq("run_id", run_id),
    )?
    .ok_or_else(|| WorkflowStoreError::RunNotFound {
        run_id: run_id.into(),
    })?;
    if database::optional_unsigned(&row, "cancellation_requested_at_ms")?.is_none() {
        return Err(WorkflowStoreError::InvalidData(format!(
            "workflow cancellation was not requested: {run_id}"
        )));
    }
    Ok(())
}

pub fn request_run_cancellation_in_transaction(
    transaction: &dyn Database,
    run_id: &str,
    requested_at_ms: u64,
) -> Result<bool, WorkflowStoreError> {
    let changed = block_on(
        transaction.exec_update_count(
            &(transaction
                .update("workflow_runs")
                .value(
                    "cancellation_requested_at_ms",
                    database::integer_value(requested_at_ms)?,
                )
                .value("updated_at_ms", database::integer_value(requested_at_ms)?)
                .where_eq("run_id", DatabaseValue::from(run_id))
                .where_eq("cancellation_requested_at_ms", DatabaseValue::Null)
                .where_in(
                    "status",
                    vec![
                        DatabaseValue::from("running"),
                        DatabaseValue::from("paused"),
                        DatabaseValue::from("repair_required"),
                    ],
                )),
        ),
    )?;
    if changed == 1 {
        block_on(
            transaction.exec_update_count(
                &(transaction
                    .update("workflow_fan_out_members")
                    .value("status", DatabaseValue::from("cancelled"))
                    .value("terminal_at_ms", database::integer_value(requested_at_ms)?)
                    .where_eq("run_id", DatabaseValue::from(run_id))
                    .where_in(
                        "status",
                        vec![
                            DatabaseValue::from("pending"),
                            DatabaseValue::from("waiting"),
                        ],
                    )),
            ),
        )?;
        let cancelled_approvals = block_on(
            transaction.exec_update_count(
                &(transaction
                    .update("workflow_mutation_approvals")
                    .value("status", DatabaseValue::from("cancelled"))
                    .value("resolved_at_ms", database::integer_value(requested_at_ms)?)
                    .where_eq("run_id", DatabaseValue::from(run_id))
                    .where_eq("status", DatabaseValue::from("pending"))),
            ),
        )?;
        append_event_in(
            transaction,
            run_id,
            "cancellation_requested",
            &serde_json::json!({"requested_at_ms": requested_at_ms}).to_string(),
            requested_at_ms,
        )?;
        if cancelled_approvals > 0 {
            append_event_in(
                transaction,
                run_id,
                "mutation_approvals_cancelled",
                &serde_json::json!({"count": cancelled_approvals}).to_string(),
                requested_at_ms,
            )?;
        }
    } else if controls_one(
        transaction,
        select("workflow_runs")
            .columns(&["run_id"])
            .where_eq("run_id", run_id),
    )?
    .is_none()
    {
        return Err(WorkflowStoreError::RunNotFound {
            run_id: run_id.to_string(),
        });
    }
    finalize_run_cancellation_if_settled(transaction, run_id, requested_at_ms)?;
    Ok(changed == 1)
}

pub fn transition_run_control_state(
    connection: &dyn switchy_database::Database,
    run_id: &str,
    expected: RunStatus,
    target: RunStatus,
    event_type: &str,
    changed_at_ms: u64,
    authority: Option<&WorkflowExecutionAuthority>,
) -> Result<bool, WorkflowStoreError> {
    validate_id("run_id", run_id)?;
    let transaction = database::begin(connection, switchy_database::TransactionMode::Immediate)?;
    let db: &dyn switchy_database::Database = &*transaction;
    verify_store_schema(db)?;
    if let Some(authority) = authority {
        verify_execution_authority_in(db, run_id, authority)?;
    }
    let rows = database::rows(
        db,
        &select("workflow_runs")
            .columns(&["status", "cancellation_requested_at_ms"])
            .where_eq("run_id", run_id)
            .limit(1),
    )?;
    let row = rows
        .first()
        .ok_or_else(|| WorkflowStoreError::RunNotFound {
            run_id: run_id.to_string(),
        })?;
    let status = database::text(row, "status")?;
    let cancellation_requested =
        database::optional_unsigned(row, "cancellation_requested_at_ms")?.is_some();
    let status = parse_run_status(&status)?;
    if cancellation_requested {
        return Err(WorkflowStoreError::CancellationPreventsControl);
    }
    if status == target {
        return Ok(false);
    }
    if status != expected {
        return Err(WorkflowStoreError::InvalidRunTransition {
            current: status,
            target,
        });
    }
    let query = update("workflow_runs")
        .value("status", target.as_str())
        .value("updated_at_ms", database::integer_value(changed_at_ms)?)
        .where_eq("run_id", run_id)
        .where_eq("status", expected.as_str())
        .where_eq("cancellation_requested_at_ms", DatabaseValue::Null);
    let changed = block_on(db.exec_update_count(&query))?;
    if changed != 1 {
        return Err(WorkflowStoreError::InvalidData(
            "workflow control transition did not update exactly one run".into(),
        ));
    }
    append_event_in(
        db,
        run_id,
        event_type,
        &serde_json::json!({"status": target.as_str()}).to_string(),
        changed_at_ms,
    )?;
    database::commit(transaction)?;
    Ok(true)
}

pub fn pending_replacement_in(
    db: &dyn switchy_database::Database,
    old_run_id: &str,
) -> Result<Option<NewWorkflowRun>, WorkflowStoreError> {
    use switchy_database::DatabaseValue as V;
    use switchy_database::query::{FilterableQuery as _, bounded_text, identifier, select};
    validate_id("old run", old_run_id)?;
    let rows = database::rows(
        db,
        &select("workflow_replacement_intents")
            .project_as(
                bounded_text(
                    identifier("successor_json"),
                    MAX_INLINE_JSON_BYTES as u64,
                    V::Null,
                ),
                "successor_json",
            )
            .project_as(
                bounded_text(identifier("successor_run_id"), MAX_ID_BYTES as u64, V::Null),
                "successor_run_id",
            )
            .where_eq("old_run_id", old_run_id)
            .limit(1),
    )?;
    rows.first()
        .map(|row| {
            let successor: NewWorkflowRun =
                serde_json::from_str(&database::text(row, "successor_json")?)?;
            let successor_id = database::text(row, "successor_run_id")?;
            validate_run(&successor)?;
            let binding = successor.binding.as_ref().ok_or_else(|| {
                WorkflowStoreError::InvalidData("replacement binding is missing".into())
            })?;
            let matches = database::rows(
                db,
                &select("workflow_runs")
                    .columns(&["run_id"])
                    .where_eq("run_id", old_run_id)
                    .where_eq("owner_plugin_id", binding.owner_plugin_id.as_str())
                    .where_eq("workflow_kind", binding.workflow_kind.as_str())
                    .where_eq("scope_key", binding.scope_key.as_str())
                    .where_eq("single_active", i64::from(binding.single_active))
                    .where_not_eq("cancellation_requested_at_ms", V::Null)
                    .limit(1),
            )?;
            if successor.run_id != successor_id
                || successor.run_id == old_run_id
                || matches.is_empty()
            {
                return Err(WorkflowStoreError::InvalidData(
                    "replacement intent identity or binding is inconsistent".into(),
                ));
            }
            Ok(successor)
        })
        .transpose()
}

fn controls_limit(limit: i64) -> Result<usize, WorkflowStoreError> {
    usize::try_from(limit)
        .map_err(|_| WorkflowStoreError::InvalidData("invalid control page limit".into()))
}
fn controls_one(
    db: &dyn Database,
    query: switchy_database::query::SelectQuery<'_>,
) -> Result<Option<Row>, WorkflowStoreError> {
    Ok(block_on(query.limit(1).execute_first(db))?)
}
fn controls_required(
    db: &dyn Database,
    query: switchy_database::query::SelectQuery<'_>,
) -> Result<Row, WorkflowStoreError> {
    controls_one(db, query)?
        .ok_or_else(|| WorkflowStoreError::InvalidData("required control row missing".into()))
}
fn controls_get<T: TryFrom<DatabaseValue>>(
    row: &Row,
    column: &str,
) -> Result<T, WorkflowStoreError> {
    T::try_from(row.get(column).ok_or_else(|| {
        WorkflowStoreError::InvalidData(format!("missing control column: {column}"))
    })?)
    .map_err(|_| WorkflowStoreError::InvalidData(format!("invalid control column: {column}")))
}
fn controls_bool(row: &Row, column: &str) -> Result<bool, WorkflowStoreError> {
    match database::unsigned(row, column)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(WorkflowStoreError::InvalidData(format!(
            "invalid control boolean: {column}"
        ))),
    }
}
fn controls_insert(
    db: &dyn Database,
    statement: &switchy_database::query::InsertStatement<'_>,
) -> Result<u64, switchy_database::DatabaseError> {
    block_on(statement.execute(db))?;
    Ok(1)
}
fn controls_count(
    db: &dyn Database,
    query: switchy_database::query::SelectQuery<'_>,
) -> Result<u32, WorkflowStoreError> {
    let row = controls_required(
        db,
        query.projections(vec![
            Projection::new(switchy_database::query::count(identifier("run_id"))).alias("count"),
        ]),
    )?;
    u32::try_from(database::unsigned(&row, "count")?)
        .map_err(|_| WorkflowStoreError::InvalidData("control count overflow".into()))
}
fn controls_column(table: &str, column: &str) -> E {
    E::Column {
        table: Some(table.into()),
        column: column.into(),
    }
}
fn controls_same(left_table: &str, left: &str, right_table: &str, right: &str) -> E {
    E::Compare(
        Box::new(controls_column(left_table, left)),
        C::Equal,
        Box::new(controls_column(right_table, right)),
    )
}
fn controls_run_cancelled() -> E {
    E::exists(
        "workflow_runs",
        E::And(vec![
            controls_same("workflow_runs", "run_id", "workflow_attempts", "run_id"),
            E::Compare(
                Box::new(controls_column(
                    "workflow_runs",
                    "cancellation_requested_at_ms",
                )),
                C::IsNot,
                Box::new(E::Null),
            ),
        ]),
    )
}
fn controls_cancellation_query() -> switchy_database::query::SelectQuery<'static> {
    select("workflow_attempts").projections(
        [
            "run_id",
            "node_id",
            "activation_id",
            "attempt",
            "dispatch_identity",
        ]
        .into_iter()
        .map(|column| Projection::new(identifier(column)))
        .chain([
            Projection::new(bounded_text(
                identifier("receipt_json"),
                MAX_INLINE_JSON_BYTES as u64,
                DatabaseValue::Null,
            ))
            .alias("receipt_json"),
            Projection::new(Box::new(switchy_database::query::where_eq_expression(
                identifier("receipt_json"),
                DatabaseValue::Null,
            ))
                as Box<dyn switchy_database::query::Expression>)
            .alias("receipt_is_null"),
        ])
        .collect(),
    )
}

fn replacement_readiness_in(
    db: &dyn Database,
    old_run_id: &str,
) -> Result<ReplacementReadiness, WorkflowStoreError> {
    let Some(pending) = pending_replacement_in(db, old_run_id)? else {
        return Ok(ReplacementReadiness::Absent);
    };
    let authority = WorkflowStore::execution_authority_in(db, old_run_id)?.ok_or_else(|| {
        WorkflowStoreError::InvalidData("replacement owner is unavailable".into())
    })?;
    if pending
        .execution_authority
        .as_ref()
        .is_none_or(|original| original.target_artifact_id != authority.target_artifact_id)
    {
        return Ok(ReplacementReadiness::ExecutionCompatibilityRequired);
    }
    let row = controls_required(
        db,
        select("workflow_runs")
            .columns(&["status", "cancellation_requested_at_ms"])
            .where_eq("run_id", old_run_id),
    )?;
    let terminal = matches!(
        database::text(&row, "status")?.as_str(),
        "completed" | "failed" | "cancelled"
    ) && database::optional_unsigned(&row, "cancellation_requested_at_ms")?
        .is_some();
    let children = controls_one(
        db,
        select("workflow_run_links")
            .columns(&["child_run_id"])
            .where_eq("parent_run_id", old_run_id),
    )?
    .is_some();
    let unsettled = controls_one(
        db,
        select("workflow_attempts")
            .columns(&["dispatch_identity"])
            .where_eq("run_id", old_run_id)
            .where_not_in(
                "status",
                vec![
                    DatabaseValue::from("succeeded"),
                    DatabaseValue::from("failed"),
                    DatabaseValue::from("cancelled"),
                ],
            ),
    )?
    .is_some();
    let complete = controls_one(
        db,
        select("workflow_quiescence")
            .columns(&["complete"])
            .where_eq("run_id", old_run_id),
    )?
    .map(|row| controls_bool(&row, "complete"))
    .transpose()?
    .unwrap_or(false);
    if !terminal {
        return Ok(ReplacementReadiness::WaitingForOldRun);
    }
    if unsettled {
        return Ok(ReplacementReadiness::WaitingForEffects);
    }
    if children && !complete {
        return Ok(ReplacementReadiness::DescendantProofRequired);
    }
    Ok(ReplacementReadiness::ReadyForAuthorization)
}

fn controls_descendants_in(
    db: &dyn Database,
    root_run_id: &str,
    limit: usize,
) -> Result<Vec<WorkflowDescendantRunSummary>, WorkflowStoreError> {
    database::rows(
        db,
        &select("workflow_run_links")
            .columns(&[
                "root_run_id",
                "parent_run_id",
                "parent_node_id",
                "parent_activation_id",
                "parent_attempt",
                "child_run_id",
                "version",
                "target_json",
                "depth",
                "created_at_ms",
            ])
            .where_eq("root_run_id", root_run_id)
            .sort("created_at_ms", SortDirection::Asc)
            .sort("child_run_id", SortDirection::Asc)
            .limit(limit),
    )?
    .iter()
    .map(|row| {
        let link = decode_run_link(row)?;
        let run = run_summary_in(db, &link.child_run_id)?.ok_or_else(|| {
            WorkflowStoreError::InvalidData(
                "workflow run link references a missing child run".into(),
            )
        })?;
        Ok(WorkflowDescendantRunSummary { link, run })
    })
    .collect()
}

fn advance_quiescence_pages(
    db: &dyn Database,
    run_id: &str,
    child: &str,
    after: &str,
    limit: usize,
    attempts_complete: bool,
) -> Result<bool, WorkflowStoreError> {
    if attempts_complete {
        let rows = database::rows(
            db,
            &select("workflow_run_links")
                .indexed_by("workflow_quiescence_children")
                .columns(&["child_run_id"])
                .where_eq("parent_run_id", run_id)
                .where_gt("child_run_id", child)
                .sort("child_run_id", SortDirection::Asc)
                .limit(limit),
        )?;
        let mut last = None;
        for row in &rows {
            let id = database::text(row, "child_run_id")?;
            let proof = controls_one(
                db,
                select("workflow_quiescence")
                    .columns(&["complete"])
                    .where_eq("run_id", &id),
            )?
            .map(|row| controls_bool(&row, "complete"))
            .transpose()?
            .unwrap_or(false);
            if !proof {
                return Ok(false);
            }
            last = Some(id);
        }
        if let Some(last) = last {
            block_on(
                db.exec_update_count(
                    &db.update("workflow_quiescence")
                        .value("after_child", last)
                        .where_eq("run_id", run_id),
                ),
            )?;
        } else {
            block_on(
                db.exec_update_count(
                    &db.update("workflow_quiescence")
                        .value("complete", 1_i64)
                        .where_eq("run_id", run_id),
                ),
            )?;

            return Ok(true);
        }
    } else {
        let rows = database::rows(
            db,
            &select("workflow_attempts")
                .indexed_by("workflow_quiescence_attempts")
                .columns(&["dispatch_identity", "status"])
                .where_eq("run_id", run_id)
                .where_gt("dispatch_identity", after)
                .sort("dispatch_identity", SortDirection::Asc)
                .limit(limit),
        )?;
        for row in &rows {
            if !matches!(
                database::text(row, "status")?.as_str(),
                "succeeded" | "failed" | "cancelled"
            ) {
                block_on(db.exec_delete_count(
                    &db.delete("workflow_quiescence").where_eq("run_id", run_id),
                ))?;

                return Ok(false);
            }
        }
        let update = db.update("workflow_quiescence").where_eq("run_id", run_id);
        let update = if let Some(row) = rows.last() {
            update.value("after_dispatch", database::text(row, "dispatch_identity")?)
        } else {
            update.value("attempts_complete", 1_i64)
        };
        block_on(db.exec_update_count(&update))?;
    }

    Ok(false)
}

fn mark_orphaned_attempt_repair_required(
    transaction: &dyn Database,
    run_id: &str,
    dispatch_identity: &str,
    settled_at_ms: u64,
) -> Result<(), WorkflowStoreError> {
    block_on(
        transaction.exec_update_count(
            &(transaction
                .update("workflow_attempts")
                .value("status", DatabaseValue::from("repair_required"))
                .value("terminal_at_ms", database::integer_value(settled_at_ms)?)
                .where_eq("dispatch_identity", DatabaseValue::from(dispatch_identity))),
        ),
    )?;
    block_on(
        transaction.exec_update_count(
            &(transaction
                .update("workflow_runs")
                .value("status", DatabaseValue::from("repair_required"))
                .value("updated_at_ms", database::integer_value(settled_at_ms)?)
                .where_eq("run_id", DatabaseValue::from(run_id))),
        ),
    )?;
    append_event_in(
        transaction,
        run_id,
        "attempt_repair_required",
        &serde_json::json!({
            "dispatch_identity": dispatch_identity,
            "reason": "cancellation_owner_unavailable",
        })
        .to_string(),
        settled_at_ms,
    )?;

    Ok(())
}

fn mark_orphaned_attempt_cancelled(
    transaction: &dyn Database,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
    dispatch_identity: &str,
    settled_at_ms: u64,
) -> Result<RunStatus, WorkflowStoreError> {
    block_on(
        transaction.exec_update_count(
            &(transaction
                .update("workflow_attempts")
                .value("status", DatabaseValue::from("cancelled"))
                .value("terminal_at_ms", database::integer_value(settled_at_ms)?)
                .where_eq("dispatch_identity", DatabaseValue::from(dispatch_identity))),
        ),
    )?;
    block_on(
        transaction.exec_update_count(
            &(transaction
                .update("workflow_activations")
                .value("status", DatabaseValue::from("cancelled"))
                .where_eq("run_id", DatabaseValue::from(run_id))
                .where_eq("node_id", DatabaseValue::from(node_id))
                .where_eq("activation_id", DatabaseValue::from(activation_id))
                .where_in(
                    "status",
                    vec![
                        DatabaseValue::from("pending"),
                        DatabaseValue::from("running"),
                    ],
                )
                .where_eq("output_id", DatabaseValue::Null)),
        ),
    )?;
    append_event_in(
        transaction,
        run_id,
        "attempt_cancelled",
        &serde_json::json!({
            "dispatch_identity": dispatch_identity,
            "reason": "cancellation_owner_unavailable",
        })
        .to_string(),
        settled_at_ms,
    )?;
    finalize_run_cancellation_if_settled(transaction, run_id, settled_at_ms)?;
    let run_status = controls_required(
        transaction,
        select("workflow_runs")
            .columns(&["status"])
            .where_eq("run_id", DatabaseValue::from(run_id)),
    )
    .and_then(|row| database::text(&row, "status"))?;
    let run_status = parse_run_status(&run_status)?;

    Ok(run_status)
}

pub fn controls_validate_cancellation_receipt(row: &Row) -> Result<&Row, WorkflowStoreError> {
    let receipt = database::optional_text(row, "receipt_json")?;
    let absent = controls_bool(row, "receipt_is_null")?;
    if absent != receipt.is_none() {
        return Err(WorkflowStoreError::InvalidData(
            "cancellation receipt is oversized or not text".into(),
        ));
    }
    Ok(row)
}
