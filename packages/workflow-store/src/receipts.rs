//! Dispatch receipt persistence and canonical observation/reconciliation.
use super::{
    ActivationDispatchOwner, AsyncAttemptStatusObserver, AttemptObservation, AttemptPauseReason,
    AttemptReconciliationRequest, AttemptStatusObserver, AutomaticRetrySchedule, DispatchReceipt,
    DispatchSideEffect, MAX_ID_BYTES, MAX_INLINE_JSON_BYTES, PreparedActivationDispatch,
    ReceiptReconciliationSummary, WorkflowBlockDefinition, WorkflowExecutionAuthority,
    WorkflowStore, WorkflowStoreError, append_event_in, automatic_retry_schedule, bounded_limit,
    cancellation_requested_for_run, database, dispatch_identity, execution_authority_in,
    fan_out_member_controller, finalize_run_cancellation_if_settled,
    persist_validated_output_transaction, prepared_dispatches_scoped,
    prepared_read_only_dispatches, prepared_read_only_dispatches_for_run, receipt_backed_attempt,
    receipt_backed_attempts, receipt_backed_attempts_after, run_graph,
    settle_fan_out_member_failure, settle_fan_out_member_success, settle_parallel_failure,
    sibling_cancellation_requested_for_attempt, validate_dispatch_receipt, validate_id,
    validate_observed_output, validate_receipt_recovery_cursor, verify_execution_authority_in,
    verify_store_schema,
};
use crate::database::block_on;
use switchy_database::query::{
    FilterableQuery as _, SelectQuery, SortDirection, bounded_text, coalesce, identifier, insert,
    select, update, where_in,
};
use switchy_database::{Database, DatabaseError, DatabaseValue as V, Row, TransactionMode};

fn receipt_redelivery_status(status: &str) -> bool {
    matches!(
        status,
        "admitted"
            | "running"
            | "cancelling"
            | "sibling_cancelling"
            | "succeeded"
            | "failed"
            | "cancelled"
    )
}

impl WorkflowStore {
    /// Persist an external admission/service receipt after dispatch.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, unbounded receipt, identity mismatch, a non-
    /// prepared attempt, or database failure.
    pub fn persist_dispatch_receipt(
        &mut self,
        receipt: &DispatchReceipt,
    ) -> Result<(), WorkflowStoreError> {
        let authority = self.execution_authority(&receipt.run_id)?;
        self.persist_dispatch_receipt_with_authority(receipt, authority.as_ref())
    }

    /// Persist an owner admission receipt using the authority captured before dispatch.
    ///
    /// # Errors
    ///
    /// Rejects stale authority, malformed, conflicting or oversized receipts, new receipts for
    /// terminal attempts, and persistence failures. Exact receipt redelivery is non-mutating,
    /// including after terminal settlement.
    pub fn persist_owned_dispatch_receipt(
        &self,
        receipt: &DispatchReceipt,
        authority: &WorkflowExecutionAuthority,
    ) -> Result<(), WorkflowStoreError> {
        self.persist_dispatch_receipt_with_authority(receipt, Some(authority))
    }

    pub(super) fn persist_dispatch_receipt_with_authority(
        &self,
        receipt: &DispatchReceipt,
        authority: Option<&WorkflowExecutionAuthority>,
    ) -> Result<(), WorkflowStoreError> {
        let transaction = database::begin(&self.connection, TransactionMode::Immediate)?;
        verify_store_schema(transaction.as_ref())?;
        Self::persist_dispatch_receipt_transaction(transaction.as_ref(), receipt, authority)?;
        database::commit(transaction)?;
        Ok(())
    }

    /// Atomically recover an admitted dispatch and apply its bounded owner observation.
    ///
    /// # Errors
    /// Rejects stale authority, conflicting receipts, invalid observations, or persistence failures.
    /// Neither the receipt nor the observation is committed on failure.
    pub fn recover_owned_dispatch_observation(
        &self,
        receipt: &DispatchReceipt,
        authority: &WorkflowExecutionAuthority,
        observation: AttemptObservation,
        observed_at_ms: u64,
    ) -> Result<ReceiptReconciliationSummary, WorkflowStoreError> {
        let transaction = database::begin(&self.connection, TransactionMode::Immediate)?;
        verify_store_schema(transaction.as_ref())?;
        Self::persist_dispatch_receipt_transaction(transaction.as_ref(), receipt, Some(authority))?;
        let request = receipt_backed_attempt(transaction.as_ref(), &receipt.dispatch_identity)?
            .ok_or_else(|| WorkflowStoreError::InvalidData("recovered receipt missing".into()))?;
        let mut summary = ReceiptReconciliationSummary::default();
        apply_attempt_observation(
            transaction.as_ref(),
            &request,
            observation,
            observed_at_ms,
            &mut summary,
        )?;
        database::commit(transaction)?;
        Ok(summary)
    }

    pub(super) fn persist_dispatch_receipt_transaction(
        transaction: &dyn Database,
        receipt: &DispatchReceipt,
        authority: Option<&WorkflowExecutionAuthority>,
    ) -> Result<(), WorkflowStoreError> {
        validate_dispatch_receipt(receipt)?;
        let receipt_json = serde_json::to_string(&receipt.receipt)?;
        if receipt_json.len() > MAX_INLINE_JSON_BYTES {
            return Err(WorkflowStoreError::InvalidData(format!(
                "dispatch receipt exceeds {MAX_INLINE_JSON_BYTES} bytes"
            )));
        }
        let expected = dispatch_identity(
            &receipt.run_id,
            &receipt.node_id,
            &receipt.activation_id,
            receipt.attempt,
        );
        if receipt.dispatch_identity != expected {
            return Err(WorkflowStoreError::InvalidData(
                "dispatch receipt identity does not match durable attempt identity".to_string(),
            ));
        }
        if execution_authority_in(transaction, &receipt.run_id)?.as_ref() != authority {
            return Err(WorkflowStoreError::InvalidData(
                "receipt execution authority changed".into(),
            ));
        }
        let current = receipt_attempt_query(receipt);
        let row = receipt_required_row(
            transaction,
            &current
                .project_as(bounded_text(identifier("status"), 64, V::Null), "status")
                .project_as(
                    bounded_text(
                        identifier("receipt_json"),
                        MAX_INLINE_JSON_BYTES as u64,
                        V::Null,
                    ),
                    "receipt_json",
                )
                .limit(1),
        )?;
        let status = database::text(&row, "status")?;
        let next_status = if status == "prepared" {
            "admitted"
        } else {
            status.as_str()
        };
        let changed = block_on(
            transaction.exec_update_count(
                &update("workflow_attempts")
                    .value("status", next_status)
                    .value("receipt_json", receipt_json.as_str())
                    .value(
                        "admitted_at_ms",
                        database::integer_value(receipt.admitted_at_ms)?,
                    )
                    .where_eq("run_id", receipt.run_id.as_str())
                    .where_eq("node_id", receipt.node_id.as_str())
                    .where_eq("activation_id", receipt.activation_id.as_str())
                    .where_eq("attempt", i64::from(receipt.attempt))
                    .where_eq("dispatch_identity", receipt.dispatch_identity.as_str())
                    .filter(Box::new(where_in(
                        "status",
                        vec![
                            V::from("prepared"),
                            V::from("cancelling"),
                            V::from("sibling_cancelling"),
                        ],
                    )))
                    .where_eq("receipt_json", V::Null),
            ),
        )?;
        if changed != 1 {
            if database::optional_text(&row, "receipt_json")?.as_deref()
                == Some(receipt_json.as_str())
                && receipt_redelivery_status(&status)
            {
                return Ok(());
            }
            return Err(WorkflowStoreError::InvalidData(format!(
                "attempt is not prepared or receipt conflicts: {}",
                receipt.dispatch_identity
            )));
        }
        let status = next_status;
        append_event_in(
            transaction,
            &receipt.run_id,
            if status == "admitted" {
                "attempt_admitted"
            } else {
                "attempt_cancellation_receipt_recorded"
            },
            &serde_json::to_string(receipt)?,
            receipt.admitted_at_ms,
        )?;
        Ok(())
    }

    /// Safely redispatch bounded receipt-less prepared read-only attempts after restart.
    ///
    /// Mutating attempts are never returned by this operation. Each read-only operation retains
    /// its original stable dispatch identity and becomes admitted only after a new receipt commits.
    ///
    /// # Errors
    ///
    /// Returns an error when bounded discovery, durable decoding, owner dispatch, or receipt
    /// persistence fails. An owner error leaves the attempt prepared for another explicit retry.
    pub async fn redispatch_prepared_read_only<O>(
        &mut self,
        owner: &O,
        limit: usize,
        admitted_at_ms: u64,
    ) -> Result<Vec<String>, WorkflowStoreError>
    where
        O: ActivationDispatchOwner + ?Sized,
    {
        let prepared = receipt_snapshot(&self.connection, |db| {
            prepared_read_only_dispatches(db, bounded_limit(limit)?)
        })?;
        let mut admitted = Vec::with_capacity(prepared.len());
        for request in prepared {
            let authority = self.execution_authority(&request.activation.run_id)?;
            self.record_dispatch_handoff(&request, authority.as_ref())?;
            let receipt = owner.dispatch(&request).await?;
            self.persist_dispatch_receipt(&DispatchReceipt {
                run_id: request.activation.run_id.clone(),
                node_id: request.activation.node_id.clone(),
                activation_id: request.activation.activation_id.clone(),
                attempt: request.attempt,
                dispatch_identity: request.dispatch_identity.clone(),
                receipt,
                admitted_at_ms,
            })?;
            admitted.push(request.dispatch_identity);
        }
        Ok(admitted)
    }

    /// Redispatch receipt-less read-only attempts for one authority-qualified run.
    ///
    /// # Errors
    ///
    /// Returns an error when the run identity, bound, durable state, owner dispatch, or receipt
    /// persistence is invalid.
    pub async fn redispatch_prepared_read_only_for_run<O>(
        &mut self,
        owner: &O,
        run_id: &str,
        limit: usize,
        admitted_at_ms: u64,
    ) -> Result<Vec<String>, WorkflowStoreError>
    where
        O: ActivationDispatchOwner + ?Sized,
    {
        validate_id("run_id", run_id)?;
        let prepared = receipt_snapshot(&self.connection, |db| {
            prepared_read_only_dispatches_for_run(db, run_id, bounded_limit(limit)?)
        })?;
        let mut admitted = Vec::with_capacity(prepared.len());
        for request in prepared {
            let authority = self.execution_authority(&request.activation.run_id)?;
            self.record_dispatch_handoff(&request, authority.as_ref())?;
            let receipt = owner.dispatch(&request).await?;
            self.persist_dispatch_receipt(&DispatchReceipt {
                run_id: request.activation.run_id.clone(),
                node_id: request.activation.node_id.clone(),
                activation_id: request.activation.activation_id.clone(),
                attempt: request.attempt,
                dispatch_identity: request.dispatch_identity.clone(),
                receipt,
                admitted_at_ms,
            })?;
            admitted.push(request.dispatch_identity);
        }
        Ok(admitted)
    }

    /// Redispatch receipt-less read-only work under the caller's held execution authority.
    ///
    /// # Errors
    /// Rejects stale authority at discovery, handoff, and receipt commit, or invalid
    /// durable state, bounds, and owner dispatch failures.
    pub async fn redispatch_owned_prepared_read_only_for_run<O>(
        &mut self,
        owner: &O,
        run_id: &str,
        authority: &WorkflowExecutionAuthority,
        limit: usize,
        admitted_at_ms: u64,
    ) -> Result<Vec<String>, WorkflowStoreError>
    where
        O: ActivationDispatchOwner + ?Sized,
    {
        self.redispatch_owned_prepared_read_only_scoped(
            owner,
            run_id,
            authority,
            limit,
            admitted_at_ms,
            None,
        )
        .await
    }

    /// Redispatch only read-only preparations included in an examined recovery batch.
    ///
    /// # Errors
    ///
    /// Rejects stale authority, invalid bounds, conflicting handoffs, owner failures,
    /// and receipt persistence failures.
    pub async fn redispatch_examined_preparations<O>(
        &mut self,
        owner: &O,
        run_id: &str,
        authority: &WorkflowExecutionAuthority,
        identities: &[String],
        admitted_at_ms: u64,
    ) -> Result<Vec<String>, WorkflowStoreError>
    where
        O: ActivationDispatchOwner + ?Sized,
    {
        self.redispatch_owned_prepared_read_only_scoped(
            owner,
            run_id,
            authority,
            identities.len().max(1),
            admitted_at_ms,
            Some(identities),
        )
        .await
    }

    // Exclusive access keeps this future Send: SQLite connections are Send but not Sync.
    #[allow(clippy::needless_pass_by_ref_mut)]
    async fn redispatch_owned_prepared_read_only_scoped<O>(
        &mut self,
        owner: &O,
        run_id: &str,
        authority: &WorkflowExecutionAuthority,
        limit: usize,
        admitted_at_ms: u64,
        identities: Option<&[String]>,
    ) -> Result<Vec<String>, WorkflowStoreError>
    where
        O: ActivationDispatchOwner + ?Sized,
    {
        let prepared = {
            let tx = database::begin(&self.connection, TransactionMode::Deferred)?;
            verify_execution_authority_in(tx.as_ref(), run_id, authority)?;
            let prepared = prepared_dispatches_scoped(
                tx.as_ref(),
                Some(run_id),
                bounded_limit(limit)?,
                true,
                identities,
            )?;
            database::commit(tx)?;
            prepared
        };
        let mut admitted = Vec::with_capacity(prepared.len());
        for request in prepared {
            self.record_dispatch_handoff(&request, Some(authority))?;
            let receipt = owner
                .dispatch(&request)
                .await
                .map_err(|error| WorkflowStoreError::OwnerDispatch(Box::new(error)))?;
            self.persist_dispatch_receipt_with_authority(
                &DispatchReceipt {
                    run_id: request.activation.run_id,
                    node_id: request.activation.node_id,
                    activation_id: request.activation.activation_id,
                    attempt: request.attempt,
                    dispatch_identity: request.dispatch_identity.clone(),
                    receipt,
                    admitted_at_ms,
                },
                Some(authority),
            )?;
            admitted.push(request.dispatch_identity);
        }
        Ok(admitted)
    }

    /// Read a bounded batch of receipt-less preparations for owner-evidence recovery.
    ///
    /// # Errors
    ///
    /// Rejects invalid limits, malformed stored preparations, or stale execution authority.
    pub fn prepared_admission_recovery_for_run(
        &self,
        run_id: &str,
        authority: &WorkflowExecutionAuthority,
        limit: usize,
    ) -> Result<Vec<PreparedActivationDispatch>, WorkflowStoreError> {
        self.prepared_admission_recovery_after(run_id, authority, "", limit)
    }

    /// Read the next bounded preparation page without changing admission or execution state.
    ///
    /// The cursor is the last returned dispatch identity, independent of mutable timestamps.
    ///
    /// # Errors
    ///
    /// Rejects invalid bounds, stale authority, malformed preparations, or database failures.
    pub fn prepared_admission_recovery_after(
        &self,
        run_id: &str,
        authority: &WorkflowExecutionAuthority,
        after_dispatch_identity: &str,
        limit: usize,
    ) -> Result<Vec<PreparedActivationDispatch>, WorkflowStoreError> {
        let transaction = database::begin(&self.connection, TransactionMode::Deferred)?;
        let db: &dyn Database = transaction.as_ref();
        verify_execution_authority_in(db, run_id, authority)?;
        let limit = bounded_limit(limit)?;
        let identities = database::rows(
            db,
            &select("workflow_attempts")
                .project_as(
                    bounded_text(
                        identifier("dispatch_identity"),
                        MAX_ID_BYTES as u64,
                        V::Null,
                    ),
                    "dispatch_identity",
                )
                .where_eq("run_id", run_id)
                .filter(Box::new(where_in(
                    "status",
                    vec![
                        V::from("prepared"),
                        V::from("cancelling"),
                        V::from("sibling_cancelling"),
                    ],
                )))
                .where_eq("receipt_json", V::Null)
                .where_gt("dispatch_identity", after_dispatch_identity)
                .sort("dispatch_identity", SortDirection::Asc)
                .limit(receipt_page_limit(limit)?),
        )?
        .iter()
        .map(|row| {
            let identity = database::text(row, "dispatch_identity")?;
            validate_id("dispatch_identity", &identity)?;
            Ok(identity)
        })
        .collect::<Result<Vec<String>, WorkflowStoreError>>()?;
        let mut preparations =
            prepared_dispatches_scoped(db, Some(run_id), limit, false, Some(&identities))?;
        // Eligibility changes must not silently turn a nonempty keyset page into end-of-stream.
        if preparations.len() != identities.len() {
            return Err(WorkflowStoreError::InvalidData(
                "preparation recovery page changed eligibility".into(),
            ));
        }
        preparations.sort_by(|left, right| left.dispatch_identity.cmp(&right.dispatch_identity));
        database::commit(transaction)?;
        Ok(preparations)
    }

    /// Persist one exact owner observation for a receipt-backed attempt.
    ///
    /// # Errors
    ///
    /// Returns an error when the attempt is missing/not receipt-backed or the durable terminal
    /// transition fails validation.
    pub fn apply_attempt_observation(
        &mut self,
        dispatch_identity: &str,
        observation: AttemptObservation,
        observed_at_ms: u64,
    ) -> Result<ReceiptReconciliationSummary, WorkflowStoreError> {
        validate_id("dispatch_identity", dispatch_identity)?;
        let transaction = database::begin(&self.connection, TransactionMode::Immediate)?;
        verify_store_schema(transaction.as_ref())?;
        let request =
            receipt_backed_attempt(transaction.as_ref(), dispatch_identity)?.ok_or_else(|| {
                WorkflowStoreError::InvalidData(format!(
                    "receipt-backed workflow attempt not found: {dispatch_identity}"
                ))
            })?;
        let mut summary = ReceiptReconciliationSummary::default();
        apply_attempt_observation(
            transaction.as_ref(),
            &request,
            observation,
            observed_at_ms,
            &mut summary,
        )?;
        database::commit(transaction)?;
        Ok(summary)
    }

    /// Apply an owner observation only under the current durable execution authority.
    ///
    /// # Errors
    /// Rejects missing receipts, stale authority, invalid observations, or persistence failures.
    pub fn apply_owned_attempt_observation(
        &mut self,
        dispatch_identity: &str,
        authority: &WorkflowExecutionAuthority,
        observation: AttemptObservation,
        observed_at_ms: u64,
    ) -> Result<ReceiptReconciliationSummary, WorkflowStoreError> {
        validate_id("dispatch_identity", dispatch_identity)?;
        let transaction = database::begin(&self.connection, TransactionMode::Immediate)?;
        verify_store_schema(transaction.as_ref())?;
        let request =
            receipt_backed_attempt(transaction.as_ref(), dispatch_identity)?.ok_or_else(|| {
                WorkflowStoreError::InvalidData(format!(
                    "receipt-backed workflow attempt not found: {dispatch_identity}"
                ))
            })?;
        verify_execution_authority_in(transaction.as_ref(), &request.run_id, authority)?;
        let mut summary = ReceiptReconciliationSummary::default();
        apply_attempt_observation(
            transaction.as_ref(),
            &request,
            observation,
            observed_at_ms,
            &mut summary,
        )?;
        database::commit(transaction)?;
        Ok(summary)
    }

    /// Reconcile receipt-backed admitted/running attempts through a bounded owner status API.
    ///
    /// Observation is read-only. Each returned state is then persisted atomically. Unknown
    /// mutating outcomes become repair-required and are never redispatched.
    ///
    /// # Errors
    ///
    /// Returns an error when `limit` is invalid, receipt data is malformed, owner observation
    /// fails, or a durable transition cannot be committed.
    pub fn reconcile_receipt_backed_attempts<O>(
        &mut self,
        observer: &O,
        limit: usize,
        reconciled_at_ms: u64,
    ) -> Result<ReceiptReconciliationSummary, WorkflowStoreError>
    where
        O: AttemptStatusObserver + ?Sized,
    {
        let limit = bounded_limit(limit)?;
        let requests = receipt_snapshot(&self.connection, |db| {
            receipt_backed_attempts(db, None, limit)
        })?;
        let observations = requests
            .iter()
            .map(|request| observer.observe(request).map(|status| (request, status)))
            .collect::<Result<Vec<_>, _>>()?;
        let transaction = database::begin(&self.connection, TransactionMode::Immediate)?;
        let mut summary = ReceiptReconciliationSummary::default();
        for (request, observation) in observations {
            apply_attempt_observation(
                transaction.as_ref(),
                request,
                observation,
                reconciled_at_ms,
                &mut summary,
            )?;
        }
        database::commit(transaction)?;
        Ok(summary)
    }

    /// Asynchronously reconcile receipt-backed attempts through a bounded owner status API.
    ///
    /// # Errors
    ///
    /// Returns an error when bounded discovery/observation or a durable transition fails.
    pub async fn reconcile_receipt_backed_attempts_async<O>(
        &mut self,
        observer: &O,
        limit: usize,
        reconciled_at_ms: u64,
    ) -> Result<ReceiptReconciliationSummary, WorkflowStoreError>
    where
        O: AsyncAttemptStatusObserver + ?Sized,
    {
        let limit = bounded_limit(limit)?;
        let requests = receipt_snapshot(&self.connection, |db| {
            receipt_backed_attempts(db, None, limit)
        })?;
        self.reconcile_attempt_requests_async(observer, requests, reconciled_at_ms)
            .await
    }

    /// Asynchronously reconcile receipt-backed attempts for one exact run.
    ///
    /// # Errors
    ///
    /// Returns an error when the run identity or bound is invalid, or when bounded
    /// discovery/observation or a durable transition fails.
    pub async fn reconcile_receipt_backed_attempts_for_run_async<O>(
        &mut self,
        observer: &O,
        run_id: &str,
        limit: usize,
        reconciled_at_ms: u64,
    ) -> Result<ReceiptReconciliationSummary, WorkflowStoreError>
    where
        O: AsyncAttemptStatusObserver + ?Sized,
    {
        validate_id("run_id", run_id)?;
        let limit = bounded_limit(limit)?;
        let requests = receipt_snapshot(&self.connection, |db| {
            receipt_backed_attempts(db, Some(run_id), limit)
        })?;
        self.reconcile_attempt_requests_async(observer, requests, reconciled_at_ms)
            .await
    }

    /// Observe and settle one run's receipts under durable execution authority.
    ///
    /// Authority and graph revision are captured with receipt discovery in one snapshot and
    /// rechecked inside the settlement transaction. No database transaction spans owner awaits;
    /// ownership transfer or a graph edit during observation rejects the entire settlement batch.
    ///
    /// # Errors
    ///
    /// Returns an error for stale authority, invalid bounds, owner observation failure,
    /// or invalid durable transitions. Failed settlement rolls back the entire batch.
    pub async fn reconcile_owned_receipts_for_run_async<O>(
        &mut self,
        observer: &O,
        run_id: &str,
        authority: &WorkflowExecutionAuthority,
        limit: usize,
        reconciled_at_ms: u64,
    ) -> Result<ReceiptReconciliationSummary, WorkflowStoreError>
    where
        O: AsyncAttemptStatusObserver + ?Sized,
    {
        self.reconcile_owned_receipts_page_async(
            observer,
            run_id,
            authority,
            (None, limit),
            reconciled_at_ms,
        )
        .await
        .map(|(summary, _)| summary)
    }

    /// Reconcile a bounded receipt page ordered by immutable dispatch identity.
    ///
    /// The returned cursor advances past observed attempts even when they remain active.
    /// Pass it to the next call; `None` indicates the end of this sweep. Each page independently
    /// captures and rechecks authority and graph revision before committing settlement.
    ///
    /// # Errors
    /// Returns an error for invalid bounds, stale authority, observation failure, or invalid state.
    pub async fn reconcile_owned_receipts_page_async<O>(
        &mut self,
        observer: &O,
        run_id: &str,
        authority: &WorkflowExecutionAuthority,
        page: (Option<&str>, usize),
        reconciled_at_ms: u64,
    ) -> Result<(ReceiptReconciliationSummary, Option<String>), WorkflowStoreError>
    where
        O: AsyncAttemptStatusObserver + ?Sized,
    {
        let (revision, requests) = {
            let transaction = database::begin(
                &self.connection,
                switchy_database::TransactionMode::Deferred,
            )?;
            let db: &dyn switchy_database::Database = &*transaction;
            verify_execution_authority_in(db, run_id, authority)?;
            let revision = run_graph::graph_revision(db, run_id)?.ok_or_else(|| {
                WorkflowStoreError::InvalidData(
                    "receipt reconciliation graph is missing".to_string(),
                )
            })?;
            let requests =
                receipt_backed_attempts_after(db, run_id, page.0, bounded_limit(page.1)?)?;
            database::commit(transaction)?;
            (revision, requests)
        };
        let cursor = (requests.len() == page.1)
            .then(|| {
                requests
                    .last()
                    .map(|request| request.dispatch_identity.clone())
            })
            .flatten();
        let summary = self
            .reconcile_attempt_requests_with_authority_async(
                observer,
                requests,
                reconciled_at_ms,
                Some((run_id, authority, revision)),
            )
            .await?;
        Ok((summary, cursor))
    }

    /// Reconcile one keyset page without repeatedly observing only the oldest attempts.
    ///
    /// The returned cursor is the last inspected dispatch identity, including deferred work.
    /// An empty page returns no cursor; callers then restart discovery on their next pass.
    /// It is an observation cursor, not a durable execution-resume token.
    ///
    /// # Errors
    /// Rejects invalid bounds, stale ownership, changed graphs, or failed observation/settlement.
    #[allow(clippy::too_many_arguments)]
    pub async fn reconcile_owned_receipt_page<O>(
        &mut self,
        observer: &O,
        run_id: &str,
        authority: &WorkflowExecutionAuthority,
        after_dispatch_identity: &str,
        limit: usize,
        now_ms: u64,
    ) -> Result<(ReceiptReconciliationSummary, Option<String>), WorkflowStoreError>
    where
        O: AsyncAttemptStatusObserver + ?Sized,
    {
        validate_id("run_id", run_id)?;
        validate_receipt_recovery_cursor(after_dispatch_identity)?;
        let (revision, requests) = {
            let tx = database::begin(
                &self.connection,
                switchy_database::TransactionMode::Deferred,
            )?;
            let db: &dyn switchy_database::Database = &*tx;
            verify_execution_authority_in(db, run_id, authority)?;
            let revision = run_graph::graph_revision(db, run_id)?.ok_or_else(|| {
                WorkflowStoreError::InvalidData("receipt reconciliation graph is missing".into())
            })?;
            let requests = receipt_backed_attempts_after(
                db,
                run_id,
                Some(after_dispatch_identity),
                bounded_limit(limit)?,
            )?;
            database::commit(tx)?;
            (revision, requests)
        };
        let cursor = requests
            .last()
            .map(|request| request.dispatch_identity.clone());
        let summary = self
            .reconcile_attempt_requests_with_authority_async(
                observer,
                requests,
                now_ms,
                Some((run_id, authority, revision)),
            )
            .await?;
        Ok((summary, cursor))
    }

    /// Advance one durable receipt-observation page under current ownership.
    ///
    /// Cursor persistence follows settlement. A crash between them repeats only idempotent
    /// observation, never dispatch. Empty pages restart the next sweep.
    ///
    /// # Errors
    /// Rejects stale ownership, invalid bounds, damaged cursors, or failed reconciliation.
    pub async fn advance_receipt_recovery<O>(
        &mut self,
        observer: &O,
        run_id: &str,
        authority: &WorkflowExecutionAuthority,
        limit: usize,
        now_ms: u64,
    ) -> Result<ReceiptReconciliationSummary, WorkflowStoreError>
    where
        O: AsyncAttemptStatusObserver + ?Sized,
    {
        let (cursor, revision) = {
            let tx = database::begin(&self.connection, TransactionMode::Deferred)?;
            verify_execution_authority_in(tx.as_ref(), run_id, authority)?;
            let rows = database::rows(
                tx.as_ref(),
                &select("workflow_receipt_cursors")
                    .project_as(
                        bounded_text(identifier("after_dispatch_identity"), 64, V::Null),
                        "cursor",
                    )
                    .where_eq("run_id", run_id)
                    .limit(1),
            )?;
            let cursor = rows
                .first()
                .map(|row| database::text(row, "cursor"))
                .transpose()?
                .unwrap_or_default();
            validate_receipt_recovery_cursor(&cursor)?;
            let revision = run_graph::graph_revision(tx.as_ref(), run_id)?;
            database::commit(tx)?;
            (cursor, revision)
        };
        let (summary, next) = self
            .reconcile_owned_receipt_page(observer, run_id, authority, &cursor, limit, now_ms)
            .await?;
        let tx = database::begin(&self.connection, TransactionMode::Immediate)?;
        verify_execution_authority_in(tx.as_ref(), run_id, authority)?;
        if run_graph::graph_revision(tx.as_ref(), run_id)? != revision {
            // Settlement discarded a page observed against an obsolete graph. Keep its
            // cursor unchanged so the next pass observes those attempts again.
            return Ok(summary);
        }
        let next = next.unwrap_or_default();
        let inserted = block_on(
            insert("workflow_receipt_cursors")
                .value("run_id", run_id)
                .value("after_dispatch_identity", next.as_str())
                .on_conflict_do_nothing(["run_id"])
                .execute_count(tx.as_ref()),
        )?;
        if inserted == 0 {
            block_on(
                tx.exec_update_count(
                    &update("workflow_receipt_cursors")
                        .value("after_dispatch_identity", next.as_str())
                        .where_eq("run_id", run_id)
                        .where_eq("after_dispatch_identity", cursor.as_str()),
                ),
            )?;
        }
        database::commit(tx)?;
        Ok(summary)
    }

    async fn reconcile_attempt_requests_async<O>(
        &self,
        observer: &O,
        requests: Vec<AttemptReconciliationRequest>,
        reconciled_at_ms: u64,
    ) -> Result<ReceiptReconciliationSummary, WorkflowStoreError>
    where
        O: AsyncAttemptStatusObserver + ?Sized,
    {
        self.reconcile_attempt_requests_with_authority_async(
            observer,
            requests,
            reconciled_at_ms,
            None,
        )
        .await
    }

    pub(super) async fn reconcile_attempt_requests_with_authority_async<O>(
        &self,
        observer: &O,
        requests: Vec<AttemptReconciliationRequest>,
        reconciled_at_ms: u64,
        authority: Option<(&str, &WorkflowExecutionAuthority, u64)>,
    ) -> Result<ReceiptReconciliationSummary, WorkflowStoreError>
    where
        O: AsyncAttemptStatusObserver + ?Sized,
    {
        let mut observations = Vec::with_capacity(requests.len());
        for request in requests {
            if let Some((run_id, authority, revision)) = authority {
                let tx = database::begin(&self.connection, TransactionMode::Deferred)?;
                verify_execution_authority_in(tx.as_ref(), run_id, authority)?;
                if run_graph::graph_revision(tx.as_ref(), run_id)? != Some(revision) {
                    return Err(WorkflowStoreError::InvalidData(
                        "receipt observation graph revision conflict".into(),
                    ));
                }
                database::commit(tx)?;
            }
            // Cancellation intent is not terminal evidence. The operation owner must
            // still prove that execution ended, including after a coordinator restart.
            let observation = observer.observe_async(&request).await?;
            observations.push((request, observation));
        }
        let transaction = database::begin(&self.connection, TransactionMode::Immediate)?;
        if let Some((run_id, authority, revision)) = authority {
            let revision_matches =
                run_graph::graph_revision(transaction.as_ref(), run_id)? == Some(revision);
            verify_execution_authority_in(transaction.as_ref(), run_id, authority)?;
            if !revision_matches {
                // Publication raced the asynchronous observations. Discard this bounded page;
                // the durable attempts remain discoverable for observation against the new graph.
                return Ok(ReceiptReconciliationSummary::default());
            }
        }
        let mut summary = ReceiptReconciliationSummary::default();
        for (request, observation) in observations {
            apply_attempt_observation(
                transaction.as_ref(),
                &request,
                observation,
                reconciled_at_ms,
                &mut summary,
            )?;
        }
        database::commit(transaction)?;
        Ok(summary)
    }
}

pub fn schedule_retry_for_observation_transaction(
    transaction: &dyn Database,
    request: &AttemptReconciliationRequest,
    failure_kind: bcode_workflow::AutomaticRetryFailureKind,
    scheduled_at_ms: u64,
) -> Result<Option<AutomaticRetrySchedule>, WorkflowStoreError> {
    let retry_cap = u32::try_from(database::unsigned(
        &receipt_required_row(
            transaction,
            &select("workflow_runs")
                .columns(&["retry_cap"])
                .where_eq("run_id", request.run_id.as_str())
                .limit(1),
        )?,
        "retry_cap",
    )?)
    .map_err(|_| WorkflowStoreError::InvalidData("retry cap exceeds u32".into()))?;
    run_graph::reconciled_activation_exit(
        transaction,
        &request.run_id,
        &request.node_id,
        &request.activation_id,
    )?;
    let node = run_graph::bound_activation_node(
        transaction,
        &request.run_id,
        &request.node_id,
        &request.activation_id,
    )?
    .ok_or_else(|| {
        WorkflowStoreError::InvalidData(format!(
            "retry observation references missing run-graph node: {}",
            request.node_id
        ))
    })?;
    if node.kind != bcode_workflow::NodeKind::PluginBlock {
        return Ok(None);
    }
    let block: WorkflowBlockDefinition = serde_json::from_value(node.configuration)?;
    let Some(policy) = block.automatic_retry else {
        return Ok(None);
    };
    policy
        .validate()
        .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
    if !policy.admits(failure_kind) {
        return Ok(None);
    }
    let decision =
        bcode_workflow::automatic_retry_decision(bcode_workflow::AutomaticRetryEligibility {
            version: policy.version,
            effect: block.effect,
            reconciliation: block.reconciliation,
            failure: failure_kind,
            attempts_completed: request.attempt,
            max_attempts: policy.max_attempts,
            retry_cap,
        });
    let bcode_workflow::AutomaticRetryDecision::Eligible { next_attempt } = decision else {
        return Ok(None);
    };
    let backoff_ms = policy
        .backoff_ms(next_attempt)
        .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
    let next_attempt_at_ms = scheduled_at_ms.checked_add(backoff_ms).ok_or_else(|| {
        WorkflowStoreError::InvalidData("automatic retry backoff timestamp overflow".to_string())
    })?;
    let schedule = AutomaticRetrySchedule {
        run_id: request.run_id.clone(),
        node_id: request.node_id.clone(),
        activation_id: request.activation_id.clone(),
        failed_attempt: request.attempt,
        next_attempt,
        failure_kind,
        backoff_ms,
        next_attempt_at_ms,
        scheduled_at_ms,
    };
    persist_automatic_retry_schedule(transaction, &schedule)?;
    Ok(Some(schedule))
}

pub fn persist_automatic_retry_schedule(
    transaction: &dyn Database,
    schedule: &AutomaticRetrySchedule,
) -> Result<(), WorkflowStoreError> {
    let failure_json = serde_json::to_string(&schedule.failure_kind)?;
    let inserted = block_on(
        insert("workflow_retry_schedules")
            .value("run_id", schedule.run_id.as_str())
            .value("node_id", schedule.node_id.as_str())
            .value("activation_id", schedule.activation_id.as_str())
            .value("failed_attempt", i64::from(schedule.failed_attempt))
            .value("next_attempt", i64::from(schedule.next_attempt))
            .value("failure_kind", failure_json)
            .value("backoff_ms", database::integer_value(schedule.backoff_ms)?)
            .value(
                "next_attempt_at_ms",
                database::integer_value(schedule.next_attempt_at_ms)?,
            )
            .value(
                "scheduled_at_ms",
                database::integer_value(schedule.scheduled_at_ms)?,
            )
            .on_conflict_do_nothing(["run_id", "node_id", "activation_id"])
            .execute_count(transaction),
    )?;
    let stored = automatic_retry_schedule(
        transaction,
        &schedule.run_id,
        &schedule.node_id,
        &schedule.activation_id,
    )?
    .ok_or_else(|| {
        WorkflowStoreError::InvalidData("automatic retry schedule was not persisted".to_string())
    })?;
    if stored != *schedule {
        return Err(WorkflowStoreError::InvalidData(
            "automatic retry schedule conflicts with persisted state".to_string(),
        ));
    }
    if inserted == 1 {
        append_event_in(
            transaction,
            &schedule.run_id,
            "automatic_retry_scheduled",
            &serde_json::to_string(schedule)?,
            schedule.scheduled_at_ms,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
pub fn apply_attempt_observation(
    transaction: &dyn Database,
    request: &AttemptReconciliationRequest,
    mut observation: AttemptObservation,
    reconciled_at_ms: u64,
    summary: &mut ReceiptReconciliationSummary,
) -> Result<(), WorkflowStoreError> {
    let current_status = database::text(
        &receipt_required_row(
            transaction,
            &select("workflow_attempts")
                .project_as(bounded_text(identifier("status"), 64, V::Null), "status")
                .where_eq("dispatch_identity", request.dispatch_identity.as_str())
                .limit(1),
        )?,
        "status",
    )?;
    if matches!(
        current_status.as_str(),
        "succeeded" | "failed" | "paused" | "cancelled" | "repair_required"
    ) {
        let compatible = matches!(
            observation,
            AttemptObservation::Admitted
                | AttemptObservation::Running
                | AttemptObservation::Deferred { .. }
        ) || matches!(
            (current_status.as_str(), &observation),
            ("succeeded", AttemptObservation::Succeeded { .. })
                | ("paused", AttemptObservation::Paused { .. })
                | ("cancelled", AttemptObservation::Cancelled)
                | ("repair_required", AttemptObservation::Unknown)
                | (
                    "failed",
                    AttemptObservation::Failed { .. } | AttemptObservation::RetryableFailure { .. }
                )
        );
        if compatible {
            return Ok(());
        }
        return Err(WorkflowStoreError::InvalidData(format!(
            "terminal workflow attempt observation conflicts with persisted status: {} is {}",
            request.dispatch_identity, current_status
        )));
    }
    let cancellation_requested = cancellation_requested_for_run(transaction, &request.run_id)?
        || sibling_cancellation_requested_for_attempt(transaction, &request.dispatch_identity)?;
    // Intent is not evidence that the owner stopped. Keep every owner kind
    // nonterminal until a terminal observation confirms its execution ended.
    if cancellation_requested
        && matches!(
            observation,
            AttemptObservation::Admitted | AttemptObservation::Running
        )
    {
        transition_attempt(transaction, request, "cancelling", None)?;
        summary.running.push(request.dispatch_identity.clone());
        return Ok(());
    }
    if cancellation_requested && matches!(observation, AttemptObservation::Unknown) {
        if request.side_effect == DispatchSideEffect::Mutating {
            transition_attempt(
                transaction,
                request,
                "repair_required",
                Some(reconciled_at_ms),
            )?;
            receipt_run_transition(
                transaction,
                &request.run_id,
                "repair_required",
                reconciled_at_ms,
                false,
            )?;
            summary
                .repair_required
                .push(request.dispatch_identity.clone());
        } else {
            summary
                .unresolved_read_only
                .push(request.dispatch_identity.clone());
        }
        return Ok(());
    }
    if cancellation_requested && matches!(observation, AttemptObservation::Deferred { .. }) {
        summary.deferred.push(request.dispatch_identity.clone());
        return Ok(());
    }
    let publication_cancellation = !database::rows(
        transaction,
        &select("workflow_publication_cancellations")
            .columns(&["run_id"])
            .where_eq("run_id", request.run_id.as_str())
            .where_eq("dispatch_identity", request.dispatch_identity.as_str())
            .limit(1),
    )?
    .is_empty();
    let preserve_terminal = matches!(
        observation,
        AttemptObservation::Succeeded { .. }
            | AttemptObservation::Failed { .. }
            | AttemptObservation::RetryableFailure { .. }
    ) && (publication_cancellation
        || fan_out_retirement_pending(transaction, &request.run_id, &request.activation_id)?);
    if cancellation_requested && !preserve_terminal {
        observation = AttemptObservation::Cancelled;
    }
    let retryable_observation = matches!(&observation, AttemptObservation::RetryableFailure { .. });
    match observation {
        AttemptObservation::Admitted => {
            transition_attempt(transaction, request, "admitted", None)?;
            summary.admitted.push(request.dispatch_identity.clone());
        }
        AttemptObservation::Running => {
            transition_attempt(transaction, request, "running", None)?;
            summary.running.push(request.dispatch_identity.clone());
        }
        AttemptObservation::Deferred { reason: _ } => {
            summary.deferred.push(request.dispatch_identity.clone());
        }
        AttemptObservation::Succeeded { output } => {
            validate_observed_output(request, &output)?;
            if fan_out_member_controller(transaction, request)?.is_some() {
                settle_fan_out_member_success(transaction, request, &output, reconciled_at_ms)?;
            } else {
                persist_validated_output_transaction(transaction, &output)?;
            }
            transition_attempt(transaction, request, "succeeded", Some(reconciled_at_ms))?;
            summary.succeeded.push(request.dispatch_identity.clone());
        }
        AttemptObservation::RetryableFailure { message }
        | AttemptObservation::Failed { message } => {
            if fan_out_member_controller(transaction, request)?.is_some() {
                let sibling_cancellations = settle_fan_out_member_failure(
                    transaction,
                    request,
                    &message,
                    reconciled_at_ms,
                )?;
                transition_attempt(transaction, request, "failed", Some(reconciled_at_ms))?;
                summary.sibling_cancellations.extend(sibling_cancellations);
                summary.failed.push(request.dispatch_identity.clone());
                return Ok(());
            }
            transition_attempt(transaction, request, "failed", Some(reconciled_at_ms))?;
            let activation_changed = receipt_activation_transition(transaction, request, "failed")?;
            if activation_changed != 1 {
                return Err(WorkflowStoreError::InvalidData(format!(
                    "failed workflow attempt has no running output-free activation: {}",
                    request.dispatch_identity
                )));
            }
            let retry_schedule = if retryable_observation {
                schedule_retry_for_observation_transaction(
                    transaction,
                    request,
                    bcode_workflow::AutomaticRetryFailureKind::OwnerReportedRetryable,
                    reconciled_at_ms,
                )?
            } else {
                None
            };
            if retry_schedule.is_none() {
                let parallel_state = settle_parallel_failure(
                    transaction,
                    &request.run_id,
                    &request.node_id,
                    &request.activation_id,
                    true,
                    reconciled_at_ms,
                )?;
                if let Some(parallel_state) = &parallel_state {
                    summary
                        .sibling_cancellations
                        .extend(parallel_state.sibling_cancellations.clone());
                }
                if parallel_state.is_none() {
                    receipt_run_transition(
                        transaction,
                        &request.run_id,
                        "failed",
                        reconciled_at_ms,
                        true,
                    )?;
                }
            } else {
                receipt_run_transition(
                    transaction,
                    &request.run_id,
                    "failed",
                    reconciled_at_ms,
                    true,
                )?;
            }
            append_event_in(
                transaction,
                &request.run_id,
                "attempt_failed",
                &serde_json::json!({
                    "dispatch_identity": request.dispatch_identity,
                    "message": message,
                })
                .to_string(),
                reconciled_at_ms,
            )?;
            if let Some(schedule) = retry_schedule {
                summary.retries_scheduled.push(schedule);
            }
            summary.failed.push(request.dispatch_identity.clone());
        }
        AttemptObservation::Paused { reason, message } => {
            apply_paused_attempt_observation(
                transaction,
                request,
                reason,
                &message,
                reconciled_at_ms,
            )?;
            summary.paused.push(request.dispatch_identity.clone());
        }
        AttemptObservation::Cancelled => {
            transition_attempt(transaction, request, "cancelled", Some(reconciled_at_ms))?;
            receipt_activation_transition(transaction, request, "cancelled")?;
            // Only an independently cancelled member participates in its join's failure
            // policy. Run cancellation and graph-edit retirement own their own settlement.
            if !publication_cancellation
                && !fan_out_retirement_pending(
                    transaction,
                    &request.run_id,
                    &request.activation_id,
                )?
                && !cancellation_requested_for_run(transaction, &request.run_id)?
            {
                let parallel_state = settle_parallel_failure(
                    transaction,
                    &request.run_id,
                    &request.node_id,
                    &request.activation_id,
                    true,
                    reconciled_at_ms,
                )?;
                if let Some(parallel_state) = parallel_state {
                    summary
                        .sibling_cancellations
                        .extend(parallel_state.sibling_cancellations);
                }
            }
            finalize_run_cancellation_if_settled(transaction, &request.run_id, reconciled_at_ms)?;
            append_event_in(
                transaction,
                &request.run_id,
                "attempt_cancelled",
                &serde_json::json!({"dispatch_identity": request.dispatch_identity}).to_string(),
                reconciled_at_ms,
            )?;
            summary.cancelled.push(request.dispatch_identity.clone());
        }
        AttemptObservation::Unknown => {
            // Admission is not proof of safe replay, even for read-only work.
            transition_attempt(
                transaction,
                request,
                "repair_required",
                Some(reconciled_at_ms),
            )?;
            receipt_run_transition(
                transaction,
                &request.run_id,
                "repair_required",
                reconciled_at_ms,
                false,
            )?;
            append_event_in(
                transaction,
                &request.run_id,
                "attempt_repair_required",
                &serde_json::json!({"dispatch_identity": request.dispatch_identity, "reason": "operation_outcome_unproven"}).to_string(),
                reconciled_at_ms,
            )?;
            summary
                .repair_required
                .push(request.dispatch_identity.clone());
        }
    }
    Ok(())
}

pub fn apply_paused_attempt_observation(
    transaction: &dyn Database,
    request: &AttemptReconciliationRequest,
    reason: AttemptPauseReason,
    message: &str,
    reconciled_at_ms: u64,
) -> Result<(), WorkflowStoreError> {
    transition_attempt(transaction, request, "paused", Some(reconciled_at_ms))?;
    let activation_changed = receipt_activation_transition(transaction, request, "pending")?;
    if activation_changed != 1 {
        return Err(WorkflowStoreError::InvalidData(format!(
            "paused workflow attempt has no running output-free activation: {}",
            request.dispatch_identity
        )));
    }
    let run_changed = block_on(
        transaction.exec_update_count(
            &update("workflow_runs")
                .value("status", "paused")
                .value("updated_at_ms", database::integer_value(reconciled_at_ms)?)
                .where_eq("run_id", request.run_id.as_str())
                .where_eq("status", "running")
                .where_eq("cancellation_requested_at_ms", V::Null),
        ),
    )?;
    if run_changed != 1 {
        return Err(WorkflowStoreError::InvalidData(format!(
            "paused workflow attempt has no resumable running run: {}",
            request.dispatch_identity
        )));
    }
    append_event_in(
        transaction,
        &request.run_id,
        "attempt_paused",
        &serde_json::json!({
            "dispatch_identity": request.dispatch_identity,
            "reason": reason,
            "message": message,
        })
        .to_string(),
        reconciled_at_ms,
    )?;
    Ok(())
}

pub fn fan_out_retirement_pending(
    db: &dyn switchy_database::Database,
    run_id: &str,
    activation_id: &str,
) -> Result<bool, WorkflowStoreError> {
    use switchy_database::DatabaseValue as V;
    use switchy_database::query::{FilterableQuery as _, bounded_text, identifier, select};
    let members = database::rows(
        db,
        &select("workflow_fan_out_members")
            .project_as(
                bounded_text(
                    identifier("controller_activation_id"),
                    MAX_ID_BYTES as u64,
                    V::Null,
                ),
                "controller_activation_id",
            )
            .where_eq("run_id", run_id)
            .where_eq("member_activation_id", activation_id)
            .limit(1),
    )?;
    let Some(member) = members.first() else {
        return Ok(false);
    };
    let controller = database::text(member, "controller_activation_id")?;
    let pending = database::rows(
        db,
        &select("workflow_pending_publications")
            .project_as(
                bounded_text(identifier("mutation_id"), MAX_ID_BYTES as u64, V::Null),
                "mutation_id",
            )
            .project(identifier("expected_revision"))
            .where_eq("run_id", run_id)
            .limit(1),
    )?;
    let Some(pending) = pending.first() else {
        return Ok(false);
    };
    if run_graph::graph_revision(db, run_id)?
        != Some(database::unsigned(pending, "expected_revision")?)
    {
        return Ok(false);
    }
    let mutation = database::text(pending, "mutation_id")?;
    let candidates = database::rows(
        db,
        &select("workflow_graph_edit_candidates")
            .project_as(
                bounded_text(
                    identifier("request_json"),
                    MAX_INLINE_JSON_BYTES as u64,
                    V::Null,
                ),
                "request_json",
            )
            .where_eq("run_id", run_id)
            .where_eq("mutation_id", mutation)
            .limit(1),
    )?;
    let Some(candidate) = candidates.first() else {
        return Ok(false);
    };
    let request: bcode_workflow::WorkflowRunGraphEditBatch =
        serde_json::from_str(&database::text(candidate, "request_json")?)?;
    Ok(request.reconciliation.iter().any(|disposition|matches!(disposition,bcode_workflow::WorkflowRunGraphReconciliation::Cancel{activation_id} if activation_id==&controller)))
}

pub fn transition_attempt(
    transaction: &dyn Database,
    request: &AttemptReconciliationRequest,
    status: &str,
    terminal_at_ms: Option<u64>,
) -> Result<(), WorkflowStoreError> {
    let changed = block_on(
        transaction.exec_update_count(
            &update("workflow_attempts")
                .value("status", status)
                .value(
                    "terminal_at_ms",
                    Box::new(coalesce(vec![
                        Box::new(database::optional_integer_value(terminal_at_ms)?),
                        Box::new(identifier("terminal_at_ms")),
                    ])) as Box<dyn switchy_database::query::Expression>,
                )
                .where_eq("dispatch_identity", request.dispatch_identity.as_str())
                .filter(Box::new(where_in(
                    "status",
                    vec![
                        V::from("admitted"),
                        V::from("running"),
                        V::from("cancelling"),
                        V::from("sibling_cancelling"),
                    ],
                ))),
        ),
    )?;
    if changed != 1 {
        return Err(WorkflowStoreError::InvalidData(format!(
            "attempt cannot transition during reconciliation: {}",
            request.dispatch_identity
        )));
    }
    if terminal_at_ms.is_some() {
        block_on(
            transaction.exec_update_count(
                &update("workflow_resource_leases")
                    .value(
                        "released_at_ms",
                        database::optional_integer_value(terminal_at_ms)?,
                    )
                    .where_eq("run_id", request.run_id.as_str())
                    .where_eq("activation_id", request.activation_id.as_str())
                    .where_eq("released_at_ms", V::Null),
            ),
        )?;
    }
    Ok(())
}

fn receipt_required_row(
    db: &dyn Database,
    query: &SelectQuery<'_>,
) -> Result<Row, WorkflowStoreError> {
    database::rows(db, query)?
        .into_iter()
        .next()
        .ok_or_else(|| DatabaseError::NoRow.into())
}
fn receipt_attempt_query(receipt: &DispatchReceipt) -> SelectQuery<'static> {
    select("workflow_attempts")
        .where_eq("run_id", receipt.run_id.as_str())
        .where_eq("node_id", receipt.node_id.as_str())
        .where_eq("activation_id", receipt.activation_id.as_str())
        .where_eq("attempt", i64::from(receipt.attempt))
        .where_eq("dispatch_identity", receipt.dispatch_identity.as_str())
}
fn receipt_activation_transition(
    db: &dyn Database,
    request: &AttemptReconciliationRequest,
    status: &str,
) -> Result<u64, WorkflowStoreError> {
    Ok(block_on(
        db.exec_update_count(
            &update("workflow_activations")
                .value("status", status)
                .where_eq("run_id", request.run_id.as_str())
                .where_eq("node_id", request.node_id.as_str())
                .where_eq("activation_id", request.activation_id.as_str())
                .where_eq("status", "running")
                .where_eq("output_id", V::Null),
        ),
    )?)
}
fn receipt_run_transition(
    db: &dyn Database,
    run_id: &str,
    status: &str,
    now: u64,
    running_only: bool,
) -> Result<u64, WorkflowStoreError> {
    let mut query = update("workflow_runs")
        .value("status", status)
        .value("updated_at_ms", database::integer_value(now)?)
        .where_eq("run_id", run_id);
    if running_only {
        query = query.where_eq("status", "running");
    }
    Ok(block_on(db.exec_update_count(&query))?)
}

fn receipt_page_limit(limit: i64) -> Result<usize, WorkflowStoreError> {
    usize::try_from(limit)
        .map_err(|_| WorkflowStoreError::InvalidData("invalid receipt page limit".into()))
}

fn receipt_snapshot<T>(
    db: &dyn Database,
    read: impl FnOnce(&dyn Database) -> Result<T, WorkflowStoreError>,
) -> Result<T, WorkflowStoreError> {
    let tx = database::begin(db, TransactionMode::Deferred)?;
    let result = read(tx.as_ref())?;
    database::commit(tx)?;
    Ok(result)
}
