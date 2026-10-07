//! Activation admission, attempt preparation and automatic retries.
use super::database::{begin, commit, optional_text, rows, text, unsigned};
use super::*;
use crate::database::block_on;
use switchy_database::query::{
    SelectQuery, SortDirection, bounded_text, byte_length, coalesce, count_all, identifier, max,
    select,
};
use switchy_database::{Database, DatabaseError, DatabaseValue, TransactionMode};

impl WorkflowStore {
    /// Admit and dispatch a bounded snapshot of pending executable activations.
    ///
    /// For each activation, owner planning happens before mutation, then activation reservation and
    /// intent persistence commit atomically before external dispatch. Owner receipts are persisted
    /// immediately after acceptance. A dispatch error leaves the attempt prepared for explicit
    /// reconciliation and stops the batch; it is never silently retried.
    ///
    /// # Errors
    ///
    /// Returns an error when pending discovery, owner planning/dispatch, admission, receipt
    /// persistence, or durable state access fails.
    pub async fn dispatch_pending_activations<O>(
        &mut self,
        owner: &O,
        limit: usize,
        dispatched_at_ms: u64,
    ) -> Result<ActivationDispatchSummary, WorkflowStoreError>
    where
        O: ActivationDispatchOwner + ?Sized,
    {
        self.dispatch_pending_activations_with_fault(
            owner,
            &NoopWorkflowDispatchFault,
            limit,
            dispatched_at_ms,
        )
        .await
    }

    /// Admit and dispatch pending executable activations for one exact run.
    ///
    /// # Errors
    ///
    /// Returns an error when the run identity or bound is invalid, or when discovery, owner
    /// planning/dispatch, admission, receipt persistence, or durable state access fails.
    pub async fn dispatch_pending_activations_for_run<O>(
        &mut self,
        owner: &O,
        run_id: &str,
        limit: usize,
        dispatched_at_ms: u64,
    ) -> Result<ActivationDispatchSummary, WorkflowStoreError>
    where
        O: ActivationDispatchOwner + ?Sized,
    {
        let pending = self.pending_activations_for_run(run_id, limit)?;
        self.dispatch_activations_with_fault(
            owner,
            &NoopWorkflowDispatchFault,
            pending,
            dispatched_at_ms,
            None,
        )
        .await
    }

    /// Dispatch one run using the caller's existing durable execution authority.
    ///
    /// # Errors
    /// Rejects stale authority before discovery and again at handoff, plus ordinary
    /// planning, dispatch, and persistence failures.
    pub async fn dispatch_owned_pending_activations_for_run<O>(
        &mut self,
        owner: &O,
        run_id: &str,
        authority: &WorkflowExecutionAuthority,
        limit: usize,
        dispatched_at_ms: u64,
    ) -> Result<ActivationDispatchSummary, WorkflowStoreError>
    where
        O: ActivationDispatchOwner + ?Sized,
    {
        self.verify_execution_authority(run_id, authority)?;
        let pending = self.pending_activations_for_run(run_id, limit)?;
        self.dispatch_activations_with_fault(
            owner,
            &NoopWorkflowDispatchFault,
            pending,
            dispatched_at_ms,
            Some(authority),
        )
        .await
    }

    /// Dispatch pending activations with deterministic post-boundary fault injection.
    ///
    /// # Errors
    ///
    /// Returns an error from discovery, planning, admission, dispatch, persistence, or the fault
    /// hook. Already committed boundaries remain durable when a later hook fails.
    pub async fn dispatch_pending_activations_with_fault<O, F>(
        &mut self,
        owner: &O,
        fault: &F,
        limit: usize,
        dispatched_at_ms: u64,
    ) -> Result<ActivationDispatchSummary, WorkflowStoreError>
    where
        O: ActivationDispatchOwner + ?Sized,
        F: WorkflowDispatchFault + ?Sized,
    {
        let pending = self.pending_activations(limit)?;
        self.dispatch_activations_with_fault(owner, fault, pending, dispatched_at_ms, None)
            .await
    }

    // Exclusive borrowing keeps this async dispatch future Send: WorkflowStore is not Sync.
    #[allow(clippy::needless_pass_by_ref_mut)]
    pub(super) async fn dispatch_activations_with_fault<O, F>(
        &mut self,
        owner: &O,
        fault: &F,
        pending: Vec<PendingActivation>,
        dispatched_at_ms: u64,
        expected_authority: Option<&WorkflowExecutionAuthority>,
    ) -> Result<ActivationDispatchSummary, WorkflowStoreError>
    where
        O: ActivationDispatchOwner + ?Sized,
        F: WorkflowDispatchFault + ?Sized,
    {
        let mut summary = ActivationDispatchSummary::default();
        for activation in pending {
            let authority = if let Some(authority) = expected_authority {
                self.verify_execution_authority(&activation.run_id, authority)?;
                Some(authority.clone())
            } else {
                self.execution_authority(&activation.run_id)?
            };
            let Some(plan) = owner.plan(&activation).await? else {
                summary.unsupported.push(activation.activation_id);
                continue;
            };
            let preparation = self.prepare_pending_activation_with_authority(
                &activation.run_id,
                &activation.node_id,
                &activation.activation_id,
                plan.side_effect,
                plan.intent,
                dispatched_at_ms,
                authority.as_ref(),
            );
            let prepared = match preparation {
                Ok(Some(prepared)) => prepared,
                Ok(None) | Err(WorkflowStoreError::ConcurrencyCapacityReached) => {
                    summary.raced.push(activation.activation_id);
                    continue;
                }
                Err(
                    WorkflowStoreError::NodeExecutionAllowanceExhausted
                    | WorkflowStoreError::RootExecutionAllowanceExhausted,
                ) => {
                    self.record_execution_allowance_exhaustion(
                        &activation.run_id,
                        authority.as_ref(),
                        dispatched_at_ms,
                    )?;
                    summary.exhausted.push(activation.activation_id);
                    continue;
                }
                Err(error) if error.to_string().contains("already leased incompatibly") => {
                    summary.raced.push(activation.activation_id);
                    continue;
                }
                Err(error) => return Err(error),
            };
            fault.after_boundary(WorkflowDispatchBoundary::IntentCommitted, &prepared)?;
            self.record_dispatch_handoff(&prepared, authority.as_ref())?;
            let receipt = owner.dispatch(&prepared).await?;
            fault.after_boundary(WorkflowDispatchBoundary::OwnerAccepted, &prepared)?;
            self.persist_dispatch_receipt_with_authority(
                &DispatchReceipt {
                    run_id: prepared.activation.run_id.clone(),
                    node_id: prepared.activation.node_id.clone(),
                    activation_id: prepared.activation.activation_id.clone(),
                    attempt: prepared.attempt,
                    dispatch_identity: prepared.dispatch_identity.clone(),
                    receipt,
                    admitted_at_ms: dispatched_at_ms,
                },
                authority.as_ref(),
            )?;
            fault.after_boundary(WorkflowDispatchBoundary::ReceiptCommitted, &prepared)?;
            summary.admitted.push(prepared.dispatch_identity);
        }
        Ok(summary)
    }

    /// Atomically reserve one exact pending activation and persist its dispatch intent.
    ///
    /// This is the scheduler admission boundary: only one caller can transition a pending
    /// activation to running and create its next attempt. External dispatch must happen only after
    /// this method commits. Repeated admission returns `Ok(None)` without creating another attempt.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, oversized intent, missing durable state, exhausted
    /// run limits, or database failure.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_pending_activation(
        &mut self,
        run_id: &str,
        node_id: &str,
        activation_id: &str,
        side_effect: DispatchSideEffect,
        intent: serde_json::Value,
        prepared_at_ms: u64,
    ) -> Result<Option<PreparedActivationDispatch>, WorkflowStoreError> {
        let authority = self.execution_authority(run_id)?;
        self.prepare_pending_activation_with_authority(
            run_id,
            node_id,
            activation_id,
            side_effect,
            intent,
            prepared_at_ms,
            authority.as_ref(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn prepare_pending_activation_with_authority(
        &self,
        run_id: &str,
        node_id: &str,
        activation_id: &str,
        side_effect: DispatchSideEffect,
        intent: serde_json::Value,
        prepared_at_ms: u64,
        expected_authority: Option<&WorkflowExecutionAuthority>,
    ) -> Result<Option<PreparedActivationDispatch>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        validate_id("node_id", node_id)?;
        validate_id("activation_id", activation_id)?;
        let intent_json = bounded_json("dispatch intent", &intent)?;
        let transaction = begin(&self.connection, TransactionMode::Immediate)?;
        verify_store_schema(transaction.as_ref())?;
        if execution_authority_in(transaction.as_ref(), run_id)?.as_ref() != expected_authority {
            return Err(WorkflowStoreError::InvalidData(
                "preparation execution authority changed".into(),
            ));
        }
        if fan_out_retirement_pending(transaction.as_ref(), run_id, activation_id)? {
            return Ok(None);
        }
        let activation = match pending_activation_by_identity(
            transaction.as_ref(),
            run_id,
            node_id,
            activation_id,
        )? {
            Some(activation) => Some(activation),
            None => pending_fan_out_member_by_identity(
                transaction.as_ref(),
                run_id,
                node_id,
                activation_id,
            )?,
        };
        let Some(activation) = activation else {
            return Ok(None);
        };
        Self::acquire_activation_resources_in_transaction(
            transaction.as_ref(),
            &activation,
            prepared_at_ms,
        )?;
        let attempt = next_attempt(transaction.as_ref(), run_id, node_id, activation_id)?;
        let prepared = PreparedAttempt {
            run_id: run_id.to_string(),
            node_id: node_id.to_string(),
            activation_id: activation_id.to_string(),
            attempt,
            side_effect,
            intent,
            prepared_at_ms,
        };
        enforce_attempt_limits(transaction.as_ref(), &prepared)?;
        let dispatch_identity = prepared.dispatch_identity();
        let checksum = sha256_hex(intent_json.as_bytes());
        mark_pending_activation_running(transaction.as_ref(), run_id, node_id, activation_id)?;
        insert_prepared(
            transaction.as_ref(),
            &prepared,
            &dispatch_identity,
            &intent_json,
            &checksum,
        )?;
        block_on(
            transaction
                .insert("workflow_dispatch_handoffs")
                .value("dispatch_identity", dispatch_identity.as_str())
                .value("handed_off", 0)
                .execute(transaction.as_ref()),
        )?;
        append_event_in(
            transaction.as_ref(),
            run_id,
            "attempt_prepared",
            &serde_json::to_string(&prepared)?,
            prepared_at_ms,
        )?;
        commit(transaction)?;
        Ok(Some(PreparedActivationDispatch {
            activation,
            attempt,
            dispatch_identity,
            intent: prepared.intent,
        }))
    }

    /// Check an exact owner admission against current durable state, or recover its receipt.
    ///
    /// Receipt recovery does not authorize new execution. Receipt-less admission requires
    /// a running, uncancelled run and activation with a prepared attempt.
    ///
    /// # Errors
    ///
    /// Returns an error for stale authority, conflicting request identity or intent,
    /// ineligible work, oversized receipt data, or database/serialization failure.
    pub fn check_owner_admission(
        &self,
        request: &PreparedActivationDispatch,
        authority: &WorkflowExecutionAuthority,
    ) -> Result<Option<serde_json::Value>, WorkflowStoreError> {
        let transaction = begin(&self.connection, TransactionMode::Deferred)?;
        Self::verify_execution_authority_in(
            transaction.as_ref(),
            &request.activation.run_id,
            authority,
        )?;
        // Preserve the original inner-join requirement even when recovering a receipt.
        first_row(
            transaction.as_ref(),
            &select("workflow_activations")
                .columns(&["run_id"])
                .where_eq("run_id", request.activation.run_id.as_str())
                .where_eq("node_id", request.activation.node_id.as_str())
                .where_eq("activation_id", request.activation.activation_id.as_str())
                .limit(1),
        )?;
        first_row(
            transaction.as_ref(),
            &select("workflow_runs")
                .columns(&["run_id"])
                .where_eq("run_id", request.activation.run_id.as_str())
                .limit(1),
        )?;
        let row = first_row(
            transaction.as_ref(),
            &attempt_query(
                &request.activation.run_id,
                &request.activation.node_id,
                &request.activation.activation_id,
            )
            .where_eq("attempt", request.attempt)
            .where_eq("dispatch_identity", request.dispatch_identity.as_str())
            .project_as(
                bounded_text(
                    identifier("intent_json"),
                    MAX_INLINE_JSON_BYTES as u64,
                    DatabaseValue::Null,
                ),
                "intent",
            )
            .project_as(
                bounded_text(
                    identifier("receipt_json"),
                    MAX_INLINE_JSON_BYTES as u64,
                    DatabaseValue::Null,
                ),
                "receipt",
            )
            .project_as(
                Box::new(coalesce(vec![
                    Box::new(byte_length(identifier("receipt_json"))),
                    Box::new(DatabaseValue::Int64(0)),
                ])) as Box<dyn switchy_database::query::Expression>,
                "receipt_length",
            )
            .project(identifier("status")),
        )?;
        let intent = optional_text(&row, "intent")?;
        let receipt = optional_text(&row, "receipt")?;
        let receipt_length = unsigned(&row, "receipt_length")?;
        let eligible = text(&row, "status")? == "prepared"
            && dispatch_eligible(
                transaction.as_ref(),
                &request.activation.run_id,
                &request.activation.node_id,
                &request.activation.activation_id,
            )?;
        let intent = intent.ok_or_else(|| {
            WorkflowStoreError::InvalidData("owner admission intent exceeds inline limit".into())
        })?;
        if serde_json::from_str::<serde_json::Value>(&intent)? != request.intent {
            return Err(WorkflowStoreError::InvalidData(
                "owner admission intent conflicts".into(),
            ));
        }
        if receipt_length > MAX_INLINE_JSON_BYTES as u64 {
            return Err(WorkflowStoreError::InvalidData(
                "owner admission receipt exceeds inline limit".into(),
            ));
        }
        if let Some(receipt) = receipt {
            return Ok(Some(serde_json::from_str(&receipt)?));
        }
        if !eligible
            || fan_out_retirement_pending(
                transaction.as_ref(),
                &request.activation.run_id,
                &request.activation.activation_id,
            )?
        {
            return Err(WorkflowStoreError::InvalidData(
                "owner admission is no longer eligible".into(),
            ));
        }
        run_graph::reconciled_activation_exit(
            transaction.as_ref(),
            &request.activation.run_id,
            &request.activation.node_id,
            &request.activation.activation_id,
        )?;
        Ok(None)
    }

    pub(super) fn record_dispatch_handoff(
        &self,
        request: &PreparedActivationDispatch,
        expected_authority: Option<&WorkflowExecutionAuthority>,
    ) -> Result<(), WorkflowStoreError> {
        let transaction = begin(&self.connection, TransactionMode::Immediate)?;
        verify_store_schema(transaction.as_ref())?;
        if execution_authority_in(transaction.as_ref(), &request.activation.run_id)?.as_ref()
            != expected_authority
        {
            return Err(WorkflowStoreError::InvalidData(
                "dispatch handoff execution authority changed".to_string(),
            ));
        }
        recovery::require_execution(transaction.as_ref(), &request.activation.run_id)?;
        let attempts = rows(
            transaction.as_ref(),
            &select("workflow_attempts")
                .columns(&["run_id", "node_id", "activation_id"])
                .where_eq("dispatch_identity", request.dispatch_identity.as_str())
                .where_eq("status", "prepared")
                .limit(1),
        )?;
        let eligible = if let Some(row) = attempts.first() {
            dispatch_eligible(
                transaction.as_ref(),
                &text(row, "run_id")?,
                &text(row, "node_id")?,
                &text(row, "activation_id")?,
            )?
        } else {
            false
        };
        if !eligible
            || fan_out_retirement_pending(
                transaction.as_ref(),
                &request.activation.run_id,
                &request.activation.activation_id,
            )?
        {
            return Err(WorkflowStoreError::InvalidData(
                "dispatch handoff is no longer eligible".to_string(),
            ));
        }
        run_graph::reconciled_activation_exit(
            transaction.as_ref(),
            &request.activation.run_id,
            &request.activation.node_id,
            &request.activation.activation_id,
        )?;
        // Absent rows represent historical/low-level preparations with unknown handoff.
        // They may follow existing recovery policy, but never gain never-dispatched proof.
        block_on(
            transaction.exec_update_count(
                &transaction
                    .update("workflow_dispatch_handoffs")
                    .value("handed_off", 1)
                    .where_eq("dispatch_identity", request.dispatch_identity.as_str()),
            ),
        )?;
        commit(transaction)?;
        Ok(())
    }

    /// Observe current run and composition-root execution allowance without mutation.
    /// Counts stop at the durable caps; this does not confer execution authority.
    ///
    /// # Errors
    /// Returns an error for missing runs, invalid durable values, or database failures.
    pub fn execution_allowance_observation(
        &self,
        run_id: &str,
    ) -> Result<bcode_workflow::WorkflowExecutionAllowanceObservation, WorkflowStoreError> {
        const READ_BUDGET: u64 = 1_000;
        validate_id("run_id", run_id)?;
        let transaction = begin(&self.connection, TransactionMode::Deferred)?;
        let root = root_run_id(transaction.as_ref(), run_id)?;
        let cap = |id: &str| run_cap(transaction.as_ref(), id, "node_execution_cap");
        let run_cap = cap(run_id)?;
        let root_cap = cap(&root)?;
        if run_cap == 0 || root_cap == 0 {
            return Err(WorkflowStoreError::InvalidData(
                "execution allowance cap must be positive".into(),
            ));
        }
        // Inspection work is independent of the caller's execution allowance. A count
        // that exceeds this read budget is unknown, never an estimate of remaining work.
        let count = |id: &str, cap: u64| -> Result<Option<u64>, WorkflowStoreError> {
            let consumed =
                bounded_attempt_count(transaction.as_ref(), id, cap.min(READ_BUDGET + 1))?;
            Ok((consumed <= READ_BUDGET || consumed == cap).then_some(consumed))
        };
        let run_consumed = count(run_id, run_cap)?;
        let children = rows(
            transaction.as_ref(),
            &select("workflow_run_links")
                .columns(&["child_run_id"])
                .where_eq("root_run_id", root.as_str())
                .sort("created_at_ms", SortDirection::Asc)
                .sort("child_run_id", SortDirection::Asc)
                .limit(usize::try_from(READ_BUDGET + 1).map_err(|_| {
                    WorkflowStoreError::InvalidData(
                        "allowance read budget exceeds platform range".into(),
                    )
                })?),
        )?
        .iter()
        .map(|row| text(row, "child_run_id"))
        .collect::<Result<Vec<_>, _>>()?;
        let mut root_consumed = None;
        if u64::try_from(children.len()).is_ok_and(|length| length <= READ_BUDGET) {
            let mut consumed = 0;
            for id in std::iter::once(&root).chain(children.iter()) {
                let remaining = root_cap.saturating_sub(consumed);
                let value = bounded_attempt_count(
                    transaction.as_ref(),
                    id,
                    remaining.min(READ_BUDGET + 1 - consumed),
                )?;
                consumed += value;
                if consumed >= root_cap || consumed > READ_BUDGET {
                    break;
                }
            }
            if consumed <= READ_BUDGET || consumed == root_cap {
                root_consumed = Some(consumed);
            }
        }
        Ok(bcode_workflow::WorkflowExecutionAllowanceObservation {
            run_cap,
            run_consumed,
            root_cap,
            root_consumed,
        })
    }

    // One bounded, indexed historical observation per run, not a current-status projection
    // or permission to extend its allowance. Existing attempts must still reconcile normally.
    pub(super) fn record_execution_allowance_exhaustion(
        &self,
        run_id: &str,
        authority: Option<&WorkflowExecutionAuthority>,
        observed_at_ms: u64,
    ) -> Result<(), WorkflowStoreError> {
        let transaction = begin(&self.connection, TransactionMode::Deferred)?;
        if execution_authority_in(transaction.as_ref(), run_id)?.as_ref() != authority {
            return Err(WorkflowStoreError::InvalidData(
                "exhaustion observation authority changed".into(),
            ));
        }
        let cap = run_cap(transaction.as_ref(), run_id, "node_execution_cap")?;
        let local_exhausted = bounded_attempt_count(transaction.as_ref(), run_id, cap)? >= cap;
        if !local_exhausted {
            match enforce_root_attempt_limit(transaction.as_ref(), run_id) {
                Ok(()) => return Ok(()),
                Err(WorkflowStoreError::RootExecutionAllowanceExhausted) => {}
                Err(error) => return Err(error),
            }
        }
        let exists = !rows(
            transaction.as_ref(),
            &select("workflow_events")
                .columns(&["run_id"])
                .where_eq("run_id", run_id)
                .where_eq("event_type", "execution_allowance_exhausted")
                .limit(1),
        )?
        .is_empty();
        if !exists {
            append_event_in(
                transaction.as_ref(),
                run_id,
                "execution_allowance_exhausted",
                r#"{"version":1,"additional_work_admitted":false}"#,
                observed_at_ms,
            )?;
        }
        commit(transaction)?;
        Ok(())
    }

    /// Persist prepared intent before an external operation is dispatched.
    ///
    /// This operation is idempotent only for byte-equivalent intent and the same side-effect
    /// classification at the same durable attempt identity. Conflicting intent or classification
    /// fails closed. Acknowledgment of existing intent does not authorize another dispatch.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, unbounded intent, missing activation, conflicting
    /// prepared intent, or database failure.
    pub fn prepare_attempt(
        &mut self,
        attempt: &PreparedAttempt,
    ) -> Result<String, WorkflowStoreError> {
        validate_prepared_attempt(attempt)?;
        let identity = attempt.dispatch_identity();
        let intent_json = serde_json::to_string(&attempt.intent)?;
        if intent_json.len() > MAX_INLINE_JSON_BYTES {
            return Err(WorkflowStoreError::InvalidData(format!(
                "dispatch intent exceeds {MAX_INLINE_JSON_BYTES} bytes"
            )));
        }
        let checksum = sha256_hex(intent_json.as_bytes());
        let transaction = begin(&self.connection, TransactionMode::Immediate)?;
        verify_store_schema(transaction.as_ref())?;
        let existing = rows(
            transaction.as_ref(),
            &attempt_query(&attempt.run_id, &attempt.node_id, &attempt.activation_id)
                .columns(&["dispatch_identity", "intent_checksum", "side_effect"])
                .where_eq("attempt", attempt.attempt)
                .limit(1),
        )?
        .first()
        .map(|row| {
            Ok::<_, WorkflowStoreError>((
                text(row, "dispatch_identity")?,
                text(row, "intent_checksum")?,
                text(row, "side_effect")?,
            ))
        })
        .transpose()?;
        if let Some((existing_identity, existing_checksum, existing_side_effect)) = existing {
            if existing_identity == identity
                && existing_checksum == checksum
                && existing_side_effect == attempt.side_effect.as_str()
            {
                return Ok(identity);
            }
            return Err(WorkflowStoreError::InvalidData(format!(
                "prepared attempt identity conflict: {identity}"
            )));
        }
        // Identical delivery acknowledges existing intent, not a new execution. Only new
        // attempts consume capacity; check inside the same transaction as their insertion.
        enforce_attempt_limits(transaction.as_ref(), attempt)?;
        insert_prepared(
            transaction.as_ref(),
            attempt,
            &identity,
            &intent_json,
            &checksum,
        )?;
        append_event_in(
            transaction.as_ref(),
            &attempt.run_id,
            "attempt_prepared",
            &serde_json::to_string(attempt)?,
            attempt.prepared_at_ms,
        )?;
        commit(transaction)?;
        Ok(identity)
    }

    /// Classify one owner observation and schedule it only when the durable retry policy permits.
    ///
    /// Non-terminal observations return `Ok(None)`. Terminal unsafe outcomes fail closed through
    /// the same finite policy used by direct scheduling.
    ///
    /// # Errors
    ///
    /// Returns an error for an ineligible terminal outcome, missing/stale durable attempt state,
    /// exhausted limits, invalid backoff, conflicting schedule identity, or database failure.
    #[allow(clippy::too_many_arguments)]
    pub fn schedule_automatic_retry_from_observation(
        &mut self,
        request: &AttemptReconciliationRequest,
        observation: &AttemptObservation,
        effect: bcode_workflow::WorkflowBlockEffect,
        reconciliation: bcode_workflow::WorkflowBlockReconciliation,
        max_attempts: u32,
        backoff_ms: u64,
        scheduled_at_ms: u64,
    ) -> Result<Option<AutomaticRetrySchedule>, WorkflowStoreError> {
        let failure = match observation {
            AttemptObservation::RetryableFailure { .. } => {
                bcode_workflow::AutomaticRetryFailureKind::OwnerReportedRetryable
            }
            AttemptObservation::Paused {
                reason: AttemptPauseReason::IdleTimeout,
                ..
            } => bcode_workflow::AutomaticRetryFailureKind::TerminalTimeout,
            AttemptObservation::Failed { .. } | AttemptObservation::Paused { .. } => {
                bcode_workflow::AutomaticRetryFailureKind::TerminalFailure
            }
            AttemptObservation::Unknown => {
                bcode_workflow::AutomaticRetryFailureKind::AmbiguousMutation
            }
            AttemptObservation::Cancelled => {
                bcode_workflow::AutomaticRetryFailureKind::Cancellation
            }
            AttemptObservation::Admitted
            | AttemptObservation::Running
            | AttemptObservation::Deferred { .. }
            | AttemptObservation::Succeeded { .. } => return Ok(None),
        };
        self.schedule_automatic_retry_for_failed_attempt(
            &request.run_id,
            &request.node_id,
            &request.activation_id,
            failure,
            effect,
            reconciliation,
            max_attempts,
            backoff_ms,
            scheduled_at_ms,
        )
        .map(Some)
    }

    /// Derive current durable attempt/run facts and persist an eligible retry schedule.
    ///
    /// # Errors
    ///
    /// Returns an error for missing durable state, ineligible failure/effect/reconciliation facts,
    /// exhausted limits, conflicting schedules, invalid backoff, or database failure.
    #[allow(clippy::too_many_arguments)]
    pub fn schedule_automatic_retry_for_failed_attempt(
        &mut self,
        run_id: &str,
        node_id: &str,
        activation_id: &str,
        failure_kind: bcode_workflow::AutomaticRetryFailureKind,
        effect: bcode_workflow::WorkflowBlockEffect,
        reconciliation: bcode_workflow::WorkflowBlockReconciliation,
        max_attempts: u32,
        backoff_ms: u64,
        scheduled_at_ms: u64,
    ) -> Result<AutomaticRetrySchedule, WorkflowStoreError> {
        let transaction = begin(&self.connection, TransactionMode::Deferred)?;
        let attempts_completed =
            latest_attempt(transaction.as_ref(), run_id, node_id, activation_id)?;
        let retry_cap = u32::try_from(run_cap(transaction.as_ref(), run_id, "retry_cap")?)
            .map_err(|_| WorkflowStoreError::InvalidData("retry cap exceeds u32".into()))?;
        drop(transaction);
        self.schedule_automatic_retry(
            bcode_workflow::AutomaticRetryEligibility {
                version: bcode_workflow::WORKFLOW_AUTOMATIC_RETRY_POLICY_VERSION,
                effect,
                reconciliation,
                failure: failure_kind,
                attempts_completed,
                max_attempts,
                retry_cap,
            },
            run_id,
            node_id,
            activation_id,
            failure_kind,
            backoff_ms,
            scheduled_at_ms,
        )
    }

    /// Persist an eligible automatic-retry schedule without sleeping or requeueing work.
    ///
    /// The exact next attempt and due time are durable and idempotent. Production capability
    /// advertisement remains disabled until a scheduler consumes this state safely.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, an ineligible policy decision, invalid/overflowing
    /// backoff, conflicting schedule identity, or database failure.
    #[allow(clippy::too_many_arguments)]
    pub fn schedule_automatic_retry(
        &mut self,
        eligibility: bcode_workflow::AutomaticRetryEligibility,
        run_id: &str,
        node_id: &str,
        activation_id: &str,
        failure_kind: bcode_workflow::AutomaticRetryFailureKind,
        backoff_ms: u64,
        scheduled_at_ms: u64,
    ) -> Result<AutomaticRetrySchedule, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        validate_id("node_id", node_id)?;
        validate_id("activation_id", activation_id)?;
        if eligibility.failure != failure_kind {
            return Err(WorkflowStoreError::InvalidData(
                "automatic retry failure kind does not match eligibility facts".to_string(),
            ));
        }
        let next_attempt = match bcode_workflow::automatic_retry_decision(eligibility) {
            bcode_workflow::AutomaticRetryDecision::Eligible { next_attempt } => next_attempt,
            bcode_workflow::AutomaticRetryDecision::Ineligible { reason } => {
                return Err(WorkflowStoreError::InvalidData(format!(
                    "automatic retry is ineligible: {reason:?}"
                )));
            }
        };
        let next_attempt_at_ms = scheduled_at_ms.checked_add(backoff_ms).ok_or_else(|| {
            WorkflowStoreError::InvalidData(
                "automatic retry backoff timestamp overflow".to_string(),
            )
        })?;
        let schedule = AutomaticRetrySchedule {
            run_id: run_id.to_string(),
            node_id: node_id.to_string(),
            activation_id: activation_id.to_string(),
            failed_attempt: eligibility.attempts_completed,
            next_attempt,
            failure_kind,
            backoff_ms,
            next_attempt_at_ms,
            scheduled_at_ms,
        };
        let failure_json = serde_json::to_string(&schedule.failure_kind)?;
        let transaction = begin(&self.connection, TransactionMode::Deferred)?;
        let inserted = block_on(
            transaction
                .insert("workflow_retry_schedules")
                .value("run_id", run_id)
                .value("node_id", node_id)
                .value("activation_id", activation_id)
                .value("failed_attempt", schedule.failed_attempt)
                .value("next_attempt", schedule.next_attempt)
                .value("failure_kind", failure_json.as_str())
                .value("backoff_ms", schedule.backoff_ms)
                .value("next_attempt_at_ms", schedule.next_attempt_at_ms)
                .value("scheduled_at_ms", schedule.scheduled_at_ms)
                .on_conflict_do_nothing(["run_id", "node_id", "activation_id"])
                .execute_count(transaction.as_ref()),
        )?;
        let stored =
            automatic_retry_schedule(transaction.as_ref(), run_id, node_id, activation_id)?
                .ok_or_else(|| {
                    WorkflowStoreError::InvalidData(
                        "automatic retry schedule was not persisted".to_string(),
                    )
                })?;
        if stored != schedule {
            return Err(WorkflowStoreError::InvalidData(
                "automatic retry schedule identity conflict".to_string(),
            ));
        }
        if inserted == 1 {
            append_event_in(
                transaction.as_ref(),
                run_id,
                "automatic_retry_scheduled",
                &serde_json::to_string(&schedule)?,
                scheduled_at_ms,
            )?;
        }
        commit(transaction)?;
        Ok(schedule)
    }

    /// Consume the next due automatic-retry schedule for one run using bounded indexed discovery.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, corrupt schedule state, stale/cancelled retry state,
    /// or database failure.
    pub fn consume_next_due_automatic_retry_for_run(
        &mut self,
        run_id: &str,
        now_ms: u64,
    ) -> Result<Option<WorkflowNodeRetryResult>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        let schedule = rows(
            &self.connection,
            &select("workflow_retry_schedules")
                .columns(&["node_id", "activation_id"])
                .where_eq("run_id", run_id)
                .where_lte("next_attempt_at_ms", now_ms)
                .sort("next_attempt_at_ms", SortDirection::Asc)
                .sort("node_id", SortDirection::Asc)
                .sort("activation_id", SortDirection::Asc)
                .limit(1),
        )?;
        let Some(row) = schedule.first() else {
            return Ok(None);
        };
        let node_id = text(row, "node_id")?;
        let activation_id = text(row, "activation_id")?;
        self.consume_due_automatic_retry(run_id, &node_id, &activation_id, now_ms)
            .map(Some)
    }

    /// Atomically consume one due automatic-retry schedule and requeue its exact failed activation.
    ///
    /// # Errors
    ///
    /// Returns an error when the schedule is missing, not due, stale, cancelled, exhausted, or no
    /// longer points at the latest failed attempt.
    pub fn consume_due_automatic_retry(
        &mut self,
        run_id: &str,
        node_id: &str,
        activation_id: &str,
        now_ms: u64,
    ) -> Result<WorkflowNodeRetryResult, WorkflowStoreError> {
        let schedule = self
            .automatic_retry_schedule(run_id, node_id, activation_id)?
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData(
                    "automatic retry schedule does not exist".to_string(),
                )
            })?;
        if schedule.next_attempt_at_ms > now_ms {
            return Err(WorkflowStoreError::InvalidData(
                "automatic retry schedule is not due".to_string(),
            ));
        }
        self.retry_failed_node_with_schedule(
            run_id,
            node_id,
            activation_id,
            schedule.failed_attempt,
            now_ms,
            Some(&schedule),
            None,
        )
    }

    /// Load one exact persisted automatic-retry schedule.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, invalid stored failure data, or database failure.
    pub fn automatic_retry_schedule(
        &self,
        run_id: &str,
        node_id: &str,
        activation_id: &str,
    ) -> Result<Option<AutomaticRetrySchedule>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        validate_id("node_id", node_id)?;
        validate_id("activation_id", activation_id)?;
        automatic_retry_schedule(&self.connection, run_id, node_id, activation_id)
    }
}

fn attempt_query<'a>(run_id: &str, node_id: &str, activation_id: &str) -> SelectQuery<'a> {
    select("workflow_attempts")
        .where_eq("run_id", run_id)
        .where_eq("node_id", node_id)
        .where_eq("activation_id", activation_id)
}

fn first_row(
    db: &dyn Database,
    query: &SelectQuery<'_>,
) -> Result<switchy_database::Row, WorkflowStoreError> {
    rows(db, query)?
        .into_iter()
        .next()
        .ok_or_else(|| DatabaseError::NoRow.into())
}

fn latest_attempt(
    db: &dyn Database,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
) -> Result<u32, WorkflowStoreError> {
    let row = first_row(
        db,
        &attempt_query(run_id, node_id, activation_id).project_as(
            Box::new(coalesce(vec![
                Box::new(max(identifier("attempt"))),
                Box::new(DatabaseValue::Int64(0)),
            ])) as Box<dyn switchy_database::query::Expression>,
            "latest",
        ),
    )?;
    u32::try_from(unsigned(&row, "latest")?)
        .map_err(|_| WorkflowStoreError::InvalidData("attempt exceeds u32".into()))
}

fn next_attempt(
    db: &dyn Database,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
) -> Result<u32, WorkflowStoreError> {
    latest_attempt(db, run_id, node_id, activation_id)?
        .checked_add(1)
        .ok_or_else(|| WorkflowStoreError::InvalidData("attempt exceeds u32".into()))
}

fn insert_prepared(
    db: &dyn Database,
    attempt: &PreparedAttempt,
    identity: &str,
    intent: &str,
    checksum: &str,
) -> Result<(), WorkflowStoreError> {
    block_on(
        db.insert("workflow_attempts")
            .value("run_id", attempt.run_id.as_str())
            .value("node_id", attempt.node_id.as_str())
            .value("activation_id", attempt.activation_id.as_str())
            .value("attempt", attempt.attempt)
            .value("dispatch_identity", identity)
            .value("side_effect", attempt.side_effect.as_str())
            .value("status", "prepared")
            .value("intent_json", intent)
            .value("intent_checksum", checksum)
            .value("prepared_at_ms", attempt.prepared_at_ms)
            .execute(db),
    )?;
    Ok(())
}

fn root_run_id(db: &dyn Database, run_id: &str) -> Result<String, WorkflowStoreError> {
    let links = rows(
        db,
        &select("workflow_run_links")
            .columns(&["root_run_id"])
            .where_eq("child_run_id", run_id)
            .limit(1),
    )?;
    links
        .first()
        .map_or_else(|| Ok(run_id.to_owned()), |row| text(row, "root_run_id"))
}

fn run_cap(db: &dyn Database, run_id: &str, column: &str) -> Result<u64, WorkflowStoreError> {
    unsigned(
        &first_row(
            db,
            &select("workflow_runs")
                .columns(&[column])
                .where_eq("run_id", run_id)
                .limit(1),
        )?,
        column,
    )
}

fn bounded_attempt_count(
    db: &dyn Database,
    run_id: &str,
    limit: u64,
) -> Result<u64, WorkflowStoreError> {
    let members = select("workflow_attempts")
        .columns(&["dispatch_identity"])
        .where_eq("run_id", run_id)
        .limit(usize::try_from(limit).unwrap_or(usize::MAX));
    let query = select("workflow_attempts")
        .project_as(count_all(), "count")
        .where_in("dispatch_identity", members);
    unsigned(&first_row(db, &query)?, "count")
}

fn dispatch_eligible(
    db: &dyn Database,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
) -> Result<bool, WorkflowStoreError> {
    let activation = rows(
        db,
        &select("workflow_activations")
            .columns(&["run_id"])
            .where_eq("run_id", run_id)
            .where_eq("node_id", node_id)
            .where_eq("activation_id", activation_id)
            .where_eq("status", "running")
            .limit(1),
    )?;
    let run = rows(
        db,
        &select("workflow_runs")
            .columns(&["run_id"])
            .where_eq("run_id", run_id)
            .where_eq("status", "running")
            .where_eq("cancellation_requested_at_ms", DatabaseValue::Null)
            .limit(1),
    )?;
    Ok(!activation.is_empty() && !run.is_empty())
}
