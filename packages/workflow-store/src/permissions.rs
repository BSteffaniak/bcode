//! Durable grants, mutation approvals and resource lease decisions.
use super::{
    MAX_ID_BYTES, MAX_INLINE_JSON_BYTES, ResourceLeaseMode, RunStatus, WorkflowExecutionAuthority,
    WorkflowGrant, WorkflowMutationApproval, WorkflowMutationApprovalDecision,
    WorkflowMutationApprovalResolution, WorkflowResourceLease, WorkflowStore, WorkflowStoreError,
    append_event_in, bounded_json, bounded_limit, database, execution_authority_in,
    parse_run_status, resource_lease, validate_grant, validate_id, validate_mutation_approval,
    validate_mutation_approval_input_checksum, validate_resource_lease,
};
use crate::database::block_on;
use switchy_database::query::{
    FilterableQuery as _, SelectQuery, SortDirection, bounded_text, identifier, insert, select,
    update, where_eq, where_gt, where_or,
};
use switchy_database::{Database, DatabaseError, DatabaseValue as V, Row, TransactionMode};

impl WorkflowStore {
    /// Persist an immutable bounded workflow permission grant.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, oversized scope, missing run/node, identity
    /// conflict, or database failure.
    pub fn persist_grant(&mut self, grant: &WorkflowGrant) -> Result<(), WorkflowStoreError> {
        validate_grant(grant)?;
        let scope_json = bounded_json("workflow grant scope", &grant.scope)?;
        let transaction = database::begin(
            &self.connection,
            switchy_database::TransactionMode::Immediate,
        )?;
        let db: &dyn switchy_database::Database = &*transaction;
        let changed = block_on(
            db.insert("workflow_grants")
                .value("grant_id", grant.grant_id.as_str())
                .value("run_id", grant.run_id.as_str())
                .value("node_id", grant.node_id.as_str())
                .value("scope_json", scope_json.as_str())
                .value(
                    "granted_at_ms",
                    database::integer_value(grant.granted_at_ms)?,
                )
                .value(
                    "expires_at_ms",
                    database::optional_integer_value(grant.expires_at_ms)?,
                )
                .value("max_uses", grant.max_uses.map(i64::from))
                .value("uses_consumed", i64::from(grant.uses_consumed))
                .or_ignore()
                .execute_count(db),
        )?;
        if changed == 0 {
            let rows = database::rows(
                db,
                &permissions_grant_query()
                    .where_eq("grant_id", grant.grant_id.as_str())
                    .limit(1),
            )?;
            let row = rows.first().ok_or(switchy_database::DatabaseError::NoRow)?;
            let existing = (
                database::text(row, "run_id")?,
                database::text(row, "node_id")?,
                database::text(row, "scope_json")?,
                database::unsigned(row, "granted_at_ms")?,
                database::optional_unsigned(row, "expires_at_ms")?,
                database::optional_unsigned(row, "max_uses")?
                    .map(u32::try_from)
                    .transpose()
                    .map_err(|_| WorkflowStoreError::InvalidData("max uses exceeds u32".into()))?,
                u32::try_from(database::unsigned(row, "uses_consumed")?).map_err(|_| {
                    WorkflowStoreError::InvalidData("uses consumed exceeds u32".into())
                })?,
            );
            if existing
                != (
                    grant.run_id.clone(),
                    grant.node_id.clone(),
                    scope_json,
                    grant.granted_at_ms,
                    grant.expires_at_ms,
                    grant.max_uses,
                    grant.uses_consumed,
                )
            {
                return Err(WorkflowStoreError::InvalidData(format!(
                    "workflow grant identity conflict: {}",
                    grant.grant_id
                )));
            }
        } else {
            append_event_in(
                db,
                &grant.run_id,
                "grant_recorded",
                &serde_json::to_string(grant)?,
                grant.granted_at_ms,
            )?;
        }
        database::commit(transaction)?;
        Ok(())
    }

    /// Atomically consume one use of an exact bounded workflow grant.
    ///
    /// # Errors
    ///
    /// Returns an error when the grant is missing, expired, unlimited, exhausted, concurrently
    /// consumed, malformed, or cannot be persisted.
    pub fn consume_grant(
        &mut self,
        grant_id: &str,
        consumed_at_ms: u64,
    ) -> Result<WorkflowGrant, WorkflowStoreError> {
        validate_id("grant_id", grant_id)?;
        let transaction = database::begin(
            &self.connection,
            switchy_database::TransactionMode::Immediate,
        )?;
        let db: &dyn switchy_database::Database = &*transaction;
        let rows = database::rows(
            db,
            &permissions_grant_query()
                .where_eq("grant_id", grant_id)
                .limit(1),
        )?;
        let row = rows.first().ok_or_else(|| {
            WorkflowStoreError::InvalidData(format!("workflow grant {grant_id} is missing"))
        })?;
        let run_id = database::text(row, "run_id")?;
        let node_id = database::text(row, "node_id")?;
        let scope_json = database::text(row, "scope_json")?;
        let granted_at_ms = database::unsigned(row, "granted_at_ms")?;
        let expires_at_ms = database::optional_unsigned(row, "expires_at_ms")?;
        let max_uses = database::optional_unsigned(row, "max_uses")?
            .map(u32::try_from)
            .transpose()
            .map_err(|_| WorkflowStoreError::InvalidData("max uses exceeds u32".into()))?;
        let uses_consumed = u32::try_from(database::unsigned(row, "uses_consumed")?)
            .map_err(|_| WorkflowStoreError::InvalidData("uses consumed exceeds u32".into()))?;
        if expires_at_ms.is_some_and(|expires| expires < consumed_at_ms) {
            return Err(WorkflowStoreError::InvalidData(format!(
                "workflow grant {grant_id} is expired"
            )));
        }
        let Some(max_uses) = max_uses else {
            return Err(WorkflowStoreError::InvalidData(format!(
                "workflow grant {grant_id} is not use-counted"
            )));
        };
        if uses_consumed >= max_uses {
            return Err(WorkflowStoreError::InvalidData(format!(
                "workflow grant {grant_id} use budget is exhausted"
            )));
        }
        let next_uses = uses_consumed.saturating_add(1);
        let changed = block_on(
            db.exec_update_count(
                &update("workflow_grants")
                    .value("uses_consumed", i64::from(next_uses))
                    .where_eq("grant_id", grant_id)
                    .where_eq("uses_consumed", i64::from(uses_consumed)),
            ),
        )?;
        if changed != 1 {
            return Err(WorkflowStoreError::InvalidData(format!(
                "workflow grant {grant_id} consumption raced with another consumer"
            )));
        }
        let grant = WorkflowGrant {
            grant_id: grant_id.to_string(),
            run_id,
            node_id,
            scope: serde_json::from_str(&scope_json)?,
            granted_at_ms,
            expires_at_ms,
            max_uses: Some(max_uses),
            uses_consumed: next_uses,
        };
        append_event_in(
            db,
            &grant.run_id,
            "grant_consumed",
            &serde_json::to_string(&grant)?,
            consumed_at_ms,
        )?;
        database::commit(transaction)?;
        Ok(grant)
    }

    /// Persist one exact mutation approval wait before a mutating activation becomes dispatchable.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid identity/scope, malformed or stale input, conflicting requests,
    /// or storage failure. No prepared attempt or external dispatch is created by this operation.
    pub fn request_mutation_approval(
        &mut self,
        approval: &WorkflowMutationApproval,
    ) -> Result<(), WorkflowStoreError> {
        validate_mutation_approval(approval)?;
        let scope_json = bounded_json("workflow mutation approval scope", &approval.scope)?;
        let transaction = database::begin(&self.connection, TransactionMode::Immediate)?;
        validate_approval_activation(transaction.as_ref(), approval)?;
        let changed = block_on(
            insert("workflow_mutation_approvals")
                .value("approval_id", approval.approval_id.as_str())
                .value("run_id", approval.run_id.as_str())
                .value("node_id", approval.node_id.as_str())
                .value("activation_id", approval.activation_id.as_str())
                .value("scope_json", scope_json.as_str())
                .value("status", "pending")
                .value(
                    "requested_at_ms",
                    database::integer_value(approval.requested_at_ms)?,
                )
                .value(
                    "expires_at_ms",
                    database::optional_integer_value(approval.expires_at_ms)?,
                )
                .or_ignore()
                .execute_count(transaction.as_ref()),
        )?;
        if changed == 0 {
            let row = permissions_required_row(
                transaction.as_ref(),
                &permissions_approval_query()
                    .where_eq("approval_id", approval.approval_id.as_str())
                    .limit(1),
            )?;
            let existing = (
                database::text(&row, "run_id")?,
                database::text(&row, "node_id")?,
                database::text(&row, "activation_id")?,
                database::text(&row, "scope_json")?,
                database::text(&row, "status")?,
                database::unsigned(&row, "requested_at_ms")?,
                database::optional_unsigned(&row, "expires_at_ms")?,
            );
            if existing
                != (
                    approval.run_id.clone(),
                    approval.node_id.clone(),
                    approval.activation_id.clone(),
                    scope_json,
                    "pending".to_string(),
                    approval.requested_at_ms,
                    approval.expires_at_ms,
                )
            {
                return Err(WorkflowStoreError::InvalidData(format!(
                    "mutation approval identity conflict: {}",
                    approval.approval_id
                )));
            }
        } else {
            permissions_activation_update(
                transaction.as_ref(),
                &approval.run_id,
                &approval.node_id,
                &approval.activation_id,
                "pending",
                "waiting_mutation_approval",
            )?;
            append_event_in(
                transaction.as_ref(),
                &approval.run_id,
                "mutation_approval_requested",
                &serde_json::to_string(approval)?,
                approval.requested_at_ms,
            )?;
        }
        database::commit(transaction)?;
        Ok(())
    }

    /// Resolve one exact pending mutation approval in one transaction.
    ///
    /// Approval persists the exact grant before restoring dispatch eligibility. Denial records the
    /// decision and fails the activation/run without creating an attempt.
    ///
    /// # Errors
    ///
    /// Returns an error for stale/expired/wrong identity, conflicting duplicate decisions, or
    /// storage failure. Any error rolls back decision, grant, and activation state together.
    pub fn resolve_mutation_approval(
        &mut self,
        approval_id: &str,
        decision: WorkflowMutationApprovalDecision,
        resolved_at_ms: u64,
    ) -> Result<WorkflowMutationApprovalResolution, WorkflowStoreError> {
        self.resolve_mutation_approval_with_authority(approval_id, decision, resolved_at_ms, None)
    }

    /// Resolve an approval with authority checked in the decision's write transaction.
    /// `None` permits only a run without durable execution authority.
    ///
    /// # Errors
    /// Returns an error for missing, incomplete, stale or foreign authority, invalid
    /// approval identity, conflicting decisions, or storage failure. No decision is
    /// committed on error.
    pub fn resolve_mutation_approval_with_authority(
        &mut self,
        approval_id: &str,
        decision: WorkflowMutationApprovalDecision,
        resolved_at_ms: u64,
        authority: Option<&WorkflowExecutionAuthority>,
    ) -> Result<WorkflowMutationApprovalResolution, WorkflowStoreError> {
        self.resolve_mutation_approval_with_clock(approval_id, decision, authority, || {
            resolved_at_ms
        })
    }

    /// Resolve with decision time sampled after acquiring the `SQLite` write transaction.
    /// The clock returns Unix milliseconds and must not perform database work.
    /// Authority and all decision changes are checked in that same transaction.
    ///
    /// # Errors
    /// Returns an error for invalid approval identity, stale or missing authority,
    /// conflicting decisions, or storage failure, without committing a decision.
    #[allow(clippy::too_many_lines)]
    pub fn resolve_mutation_approval_with_clock(
        &mut self,
        approval_id: &str,
        decision: WorkflowMutationApprovalDecision,
        authority: Option<&WorkflowExecutionAuthority>,
        clock: impl FnOnce() -> u64,
    ) -> Result<WorkflowMutationApprovalResolution, WorkflowStoreError> {
        validate_id("approval_id", approval_id)?;
        let transaction = database::begin(&self.connection, TransactionMode::Immediate)?;
        let resolved_at_ms = clock();
        let approval_row = database::rows(
            transaction.as_ref(),
            &permissions_approval_query()
                .where_eq("approval_id", approval_id)
                .limit(1),
        )?
        .into_iter()
        .next()
        .ok_or_else(|| {
            WorkflowStoreError::InvalidData(format!("mutation approval not found: {approval_id}"))
        })?;
        let run_id = database::text(&approval_row, "run_id")?;
        let node_id = database::text(&approval_row, "node_id")?;
        let activation_id = database::text(&approval_row, "activation_id")?;
        let scope_json = database::text(&approval_row, "scope_json")?;
        let status = database::text(&approval_row, "status")?;
        let expires_at_ms = database::optional_unsigned(&approval_row, "expires_at_ms")?;
        let existing_grant_id = permissions_optional_column(
            transaction.as_ref(),
            &approval_row,
            "workflow_mutation_approvals",
            "approval_id",
            approval_id,
            "grant_id",
        )?;
        let activation =
            permissions_activation(transaction.as_ref(), &run_id, &node_id, &activation_id)?;
        let activation_status = database::text(&activation, "status")?;
        let input_json = permissions_input(
            transaction.as_ref(),
            &activation,
            &run_id,
            &node_id,
            &activation_id,
        )?;
        let run = permissions_run(transaction.as_ref(), &run_id)?;
        let definition_id = database::text(&run, "definition_id")?;
        let definition_version = permissions_u32(&run, "definition_version")?;
        let workspace_snapshot = database::text(&run, "workspace_snapshot")?;
        let run_status = database::text(&run, "status")?;
        let cancellation_requested_at_ms =
            database::optional_unsigned(&run, "cancellation_requested_at_ms")?;
        if execution_authority_in(transaction.as_ref(), &run_id)?.as_ref() != authority {
            return Err(WorkflowStoreError::InvalidData(
                "workflow execution authority is missing, stale or foreign".to_owned(),
            ));
        }
        let expected_status = match decision {
            WorkflowMutationApprovalDecision::Approve => "approved",
            WorkflowMutationApprovalDecision::Deny => "denied",
        };
        if status == expected_status {
            return Ok(WorkflowMutationApprovalResolution {
                continuation: None,
                approval_id: approval_id.to_string(),
                status,
                grant_id: existing_grant_id,
                activation_status,
            });
        }
        if status != "pending" || activation_status != "waiting_mutation_approval" {
            return Err(WorkflowStoreError::InvalidData(format!(
                "mutation approval decision conflict: {approval_id} is {status}"
            )));
        }
        if parse_run_status(&run_status)? != RunStatus::Running
            || cancellation_requested_at_ms.is_some()
        {
            return Err(WorkflowStoreError::InvalidData(
                "workflow run is not accepting mutation approval decisions".to_string(),
            ));
        }
        if expires_at_ms.is_some_and(|expires| resolved_at_ms >= expires) {
            permissions_approval_update(
                transaction.as_ref(),
                approval_id,
                "expired",
                None,
                resolved_at_ms,
            )?;
            permissions_activation_update(
                transaction.as_ref(),
                &run_id,
                &node_id,
                &activation_id,
                "waiting_mutation_approval",
                "failed",
            )?;
            block_on(
                transaction.exec_update_count(
                    &update("workflow_runs")
                        .value("status", "failed")
                        .value("updated_at_ms", database::integer_value(resolved_at_ms)?)
                        .where_eq("run_id", run_id.as_str())
                        .where_eq("status", "running"),
                ),
            )?;
            append_event_in(
                transaction.as_ref(),
                &run_id,
                "mutation_approval_expired",
                &serde_json::json!({"approval_id": approval_id}).to_string(),
                resolved_at_ms,
            )?;
            database::commit(transaction)?;
            return Ok(WorkflowMutationApprovalResolution {
                continuation: None,
                approval_id: approval_id.to_string(),
                status: "expired".to_string(),
                grant_id: None,
                activation_status: "failed".to_string(),
            });
        }
        let scope: bcode_workflow::WorkflowMutationGrantScope = serde_json::from_str(&scope_json)?;
        scope
            .validate()
            .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
        validate_mutation_approval_input_checksum(
            &WorkflowMutationApproval {
                approval_id: approval_id.to_string(),
                run_id: run_id.clone(),
                node_id: node_id.clone(),
                activation_id: activation_id.clone(),
                scope: scope.clone(),
                requested_at_ms: 0,
                expires_at_ms,
            },
            input_json.as_deref(),
            transaction.as_ref(),
        )?;
        if scope.definition_id != definition_id
            || scope.definition_version != definition_version
            || scope.run_id != run_id
            || scope.node_id != node_id
            || scope.activation_id != activation_id
            || scope.workspace_snapshot != workspace_snapshot
        {
            return Err(WorkflowStoreError::InvalidData(
                "mutation approval scope became stale before decision".to_string(),
            ));
        }
        let decision_id = format!("mutation-approval:{approval_id}");
        let decision_value = serde_json::json!({
            "approval_id": approval_id,
            "decision": expected_status,
            "scope": scope,
        });
        let inserted = block_on(
            insert("workflow_decisions")
                .value("decision_id", decision_id)
                .value("run_id", run_id.as_str())
                .value("node_id", node_id.as_str())
                .value("decision_type", "mutation_approval")
                .value("value_json", serde_json::to_string(&decision_value)?)
                .value("created_at_ms", database::integer_value(resolved_at_ms)?)
                .on_conflict_do_nothing(["decision_id"])
                .execute_count(transaction.as_ref()),
        )?;
        permissions_exact_insert(inserted, "mutation approval decision")?;
        match decision {
            WorkflowMutationApprovalDecision::Approve => {
                let grant_id = format!("mutation-grant:{approval_id}");
                let inserted = block_on(
                    insert("workflow_grants")
                        .value("grant_id", grant_id.as_str())
                        .value("run_id", run_id.as_str())
                        .value("node_id", node_id.as_str())
                        .value("scope_json", scope_json)
                        .value("granted_at_ms", database::integer_value(resolved_at_ms)?)
                        .value(
                            "expires_at_ms",
                            database::optional_integer_value(expires_at_ms)?,
                        )
                        .on_conflict_do_nothing(["grant_id"])
                        .execute_count(transaction.as_ref()),
                )?;
                permissions_exact_insert(inserted, "mutation approval grant")?;
                permissions_approval_update(
                    transaction.as_ref(),
                    approval_id,
                    "approved",
                    Some(&grant_id),
                    resolved_at_ms,
                )?;
                let changed = permissions_activation_update(
                    transaction.as_ref(),
                    &run_id,
                    &node_id,
                    &activation_id,
                    "waiting_mutation_approval",
                    "pending",
                )?;
                if changed != 1 {
                    return Err(WorkflowStoreError::InvalidData(
                        "approved mutation activation is no longer waiting".to_string(),
                    ));
                }
                append_event_in(
                    transaction.as_ref(),
                    &run_id,
                    "mutation_approval_approved",
                    &decision_value.to_string(),
                    resolved_at_ms,
                )?;
                database::commit(transaction)?;
                Ok(WorkflowMutationApprovalResolution {
                    continuation: None,
                    approval_id: approval_id.to_string(),
                    status: "approved".to_string(),
                    grant_id: Some(grant_id),
                    activation_status: "pending".to_string(),
                })
            }
            WorkflowMutationApprovalDecision::Deny => {
                permissions_approval_update(
                    transaction.as_ref(),
                    approval_id,
                    "denied",
                    None,
                    resolved_at_ms,
                )?;
                permissions_activation_update(
                    transaction.as_ref(),
                    &run_id,
                    &node_id,
                    &activation_id,
                    "waiting_mutation_approval",
                    "failed",
                )?;
                block_on(
                    transaction.exec_update_count(
                        &update("workflow_runs")
                            .value("status", "failed")
                            .value("updated_at_ms", database::integer_value(resolved_at_ms)?)
                            .where_eq("run_id", run_id.as_str())
                            .where_eq("status", "running"),
                    ),
                )?;
                append_event_in(
                    transaction.as_ref(),
                    &run_id,
                    "mutation_approval_denied",
                    &decision_value.to_string(),
                    resolved_at_ms,
                )?;
                database::commit(transaction)?;
                Ok(WorkflowMutationApprovalResolution {
                    continuation: None,
                    approval_id: approval_id.to_string(),
                    status: "denied".to_string(),
                    grant_id: None,
                    activation_status: "failed".to_string(),
                })
            }
        }
    }

    /// Load bounded pending mutation approval requests across all workflow runs.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid limit or malformed durable state.
    pub fn pending_mutation_approvals_all(
        &self,
        limit: usize,
    ) -> Result<Vec<WorkflowMutationApproval>, WorkflowStoreError> {
        permissions_pending(&self.connection, None, limit)
    }

    /// Load the owning run and request timestamp for one exact mutation approval without mutating it.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity or database failure.
    pub fn mutation_approval_context(
        &self,
        approval_id: &str,
    ) -> Result<Option<(String, u64)>, WorkflowStoreError> {
        validate_id("approval_id", approval_id)?;
        let rows = database::rows(
            &self.connection,
            &select("workflow_mutation_approvals")
                .project_as(
                    bounded_text(identifier("run_id"), MAX_ID_BYTES as u64, V::Null),
                    "run_id",
                )
                .project(identifier("requested_at_ms"))
                .where_eq("approval_id", approval_id)
                .limit(1),
        )?;
        rows.first()
            .map(|row| {
                Ok((
                    database::text(row, "run_id")?,
                    database::unsigned(row, "requested_at_ms")?,
                ))
            })
            .transpose()
    }

    /// Load bounded pending mutation approval requests for one run.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid identity/limit or malformed durable state.
    pub fn pending_mutation_approvals(
        &self,
        run_id: &str,
        limit: usize,
    ) -> Result<Vec<WorkflowMutationApproval>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        permissions_pending(&self.connection, Some(run_id), limit)
    }

    /// Load one exact durable workflow grant.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, invalid persisted JSON, or database failure.
    pub fn grant(&self, grant_id: &str) -> Result<Option<WorkflowGrant>, WorkflowStoreError> {
        validate_id("grant_id", grant_id)?;
        let rows = database::rows(
            &self.connection,
            &permissions_grant_query()
                .where_eq("grant_id", grant_id)
                .limit(1),
        )?;
        rows.first()
            .map(|row| permissions_decode_grant(row, grant_id))
            .transpose()
    }

    /// Acquire a durable workflow resource lease while enforcing reader/writer exclusion.
    ///
    /// Reacquiring the exact lease is idempotent. Conflicting active leases fail closed.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, missing activation, lease conflict, identity
    /// conflict, or database failure.
    pub fn acquire_resource_lease(
        &mut self,
        lease: &WorkflowResourceLease,
    ) -> Result<(), WorkflowStoreError> {
        validate_resource_lease(lease)?;
        let transaction = database::begin(
            &self.connection,
            switchy_database::TransactionMode::Immediate,
        )?;
        let db: &dyn switchy_database::Database = &*transaction;
        let existing = resource_lease(db, &lease.lease_id)?;
        if let Some((existing, released_at_ms)) = existing {
            if existing == *lease && released_at_ms.is_none() {
                return Ok(());
            }
            return Err(WorkflowStoreError::InvalidData(format!(
                "workflow resource lease identity conflict: {}",
                lease.lease_id
            )));
        }
        let mut query = select("workflow_resource_leases")
            .project_as(
                bounded_text(identifier("lease_id"), MAX_ID_BYTES as u64, V::Null),
                "lease_id",
            )
            .where_eq("run_id", lease.run_id.as_str())
            .where_eq("resource_key", lease.resource_key.as_str())
            .where_eq("released_at_ms", switchy_database::DatabaseValue::Null)
            .filter(Box::new(where_or(vec![
                Box::new(where_eq(
                    "expires_at_ms",
                    switchy_database::DatabaseValue::Null,
                )),
                Box::new(where_gt(
                    "expires_at_ms",
                    database::integer_value(lease.acquired_at_ms)?,
                )),
            ])))
            .limit(1);
        if lease.mode != ResourceLeaseMode::Write {
            query = query.where_eq("mode", "write");
        }
        let conflict = !database::rows(db, &query)?.is_empty();
        if conflict {
            return Err(WorkflowStoreError::InvalidData(format!(
                "workflow resource is already leased incompatibly: {}",
                lease.resource_key
            )));
        }
        let inserted = block_on(
            db.insert("workflow_resource_leases")
                .value("lease_id", lease.lease_id.as_str())
                .value("run_id", lease.run_id.as_str())
                .value("node_id", lease.node_id.as_str())
                .value("activation_id", lease.activation_id.as_str())
                .value("resource_key", lease.resource_key.as_str())
                .value("mode", lease.mode.as_str())
                .value(
                    "acquired_at_ms",
                    database::integer_value(lease.acquired_at_ms)?,
                )
                .value(
                    "expires_at_ms",
                    database::optional_integer_value(lease.expires_at_ms)?,
                )
                .on_conflict_do_nothing(["lease_id"])
                .execute_count(db),
        )?;
        permissions_exact_insert(inserted, "resource lease")?;
        append_event_in(
            db,
            &lease.run_id,
            "resource_lease_acquired",
            &serde_json::to_string(lease)?,
            lease.acquired_at_ms,
        )?;
        database::commit(transaction)?;
        Ok(())
    }

    /// Release one exact durable resource lease.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed/missing/already released lease or database failure.
    pub fn release_resource_lease(
        &mut self,
        lease_id: &str,
        released_at_ms: u64,
    ) -> Result<(), WorkflowStoreError> {
        validate_id("lease_id", lease_id)?;
        let transaction = database::begin(
            &self.connection,
            switchy_database::TransactionMode::Immediate,
        )?;
        let db: &dyn switchy_database::Database = &*transaction;
        let rows = database::rows(
            db,
            &select("workflow_resource_leases")
                .project_as(
                    bounded_text(identifier("run_id"), MAX_ID_BYTES as u64, V::Null),
                    "run_id",
                )
                .where_eq("lease_id", lease_id)
                .where_eq("released_at_ms", switchy_database::DatabaseValue::Null)
                .limit(1),
        )?;
        let row = rows.first().ok_or_else(|| {
            WorkflowStoreError::InvalidData(format!(
                "active workflow resource lease not found: {lease_id}"
            ))
        })?;
        let run_id = database::text(row, "run_id")?;
        let changed = block_on(
            db.exec_update_count(
                &update("workflow_resource_leases")
                    .value("released_at_ms", database::integer_value(released_at_ms)?)
                    .where_eq("lease_id", lease_id)
                    .where_eq("released_at_ms", V::Null),
            ),
        )?;
        if changed != 1 {
            return Err(WorkflowStoreError::InvalidData(format!(
                "active workflow resource lease changed before release: {lease_id}"
            )));
        }
        append_event_in(
            db,
            &run_id,
            "resource_lease_released",
            &serde_json::json!({"lease_id": lease_id}).to_string(),
            released_at_ms,
        )?;
        database::commit(transaction)?;
        Ok(())
    }
}

fn permissions_required_row(
    db: &dyn Database,
    query: &SelectQuery<'_>,
) -> Result<Row, WorkflowStoreError> {
    database::rows(db, query)?
        .into_iter()
        .next()
        .ok_or_else(|| DatabaseError::NoRow.into())
}
fn permissions_u32(row: &Row, column: &str) -> Result<u32, WorkflowStoreError> {
    u32::try_from(database::unsigned(row, column)?)
        .map_err(|_| WorkflowStoreError::InvalidData(format!("workflow {column} exceeds u32")))
}
fn permissions_text_query(
    table: &'static str,
    text: &[&str],
    numeric: &[&str],
) -> SelectQuery<'static> {
    let mut query = select(table);
    for column in text {
        let budget = if matches!(*column, "scope_json" | "input_json") {
            MAX_INLINE_JSON_BYTES
        } else {
            MAX_ID_BYTES
        };
        query = query.project_as(
            bounded_text(identifier(column), budget as u64, V::Null),
            *column,
        );
    }
    for column in numeric {
        query = query.project(identifier(column));
    }
    query
}
fn permissions_approval_query() -> SelectQuery<'static> {
    permissions_text_query(
        "workflow_mutation_approvals",
        &[
            "approval_id",
            "run_id",
            "node_id",
            "activation_id",
            "scope_json",
            "status",
            "grant_id",
        ],
        &["requested_at_ms", "expires_at_ms"],
    )
}
fn permissions_grant_query() -> SelectQuery<'static> {
    permissions_text_query(
        "workflow_grants",
        &["run_id", "node_id", "scope_json"],
        &[
            "granted_at_ms",
            "expires_at_ms",
            "max_uses",
            "uses_consumed",
        ],
    )
}
fn permissions_activation(
    db: &dyn Database,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
) -> Result<Row, WorkflowStoreError> {
    permissions_required_row(
        db,
        &permissions_text_query("workflow_activations", &["status", "input_json"], &[])
            .where_eq("run_id", run_id)
            .where_eq("node_id", node_id)
            .where_eq("activation_id", activation_id)
            .limit(1),
    )
}
fn permissions_run(db: &dyn Database, run_id: &str) -> Result<Row, WorkflowStoreError> {
    permissions_required_row(
        db,
        &permissions_text_query(
            "workflow_runs",
            &["definition_id", "workspace_snapshot", "status"],
            &["definition_version", "cancellation_requested_at_ms"],
        )
        .where_eq("run_id", run_id)
        .limit(1),
    )
}
fn permissions_optional_column(
    db: &dyn Database,
    row: &Row,
    table: &str,
    key: &str,
    id: &str,
    column: &str,
) -> Result<Option<String>, WorkflowStoreError> {
    let value = database::optional_text(row, column)?;
    if value.is_none()
        && database::rows(
            db,
            &select(table)
                .project(identifier(key))
                .where_eq(key, id)
                .where_eq(column, V::Null)
                .limit(1),
        )?
        .is_empty()
    {
        return Err(WorkflowStoreError::InvalidData(format!(
            "workflow {column} is invalid or oversized"
        )));
    }
    Ok(value)
}
fn permissions_input(
    db: &dyn Database,
    row: &Row,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
) -> Result<Option<String>, WorkflowStoreError> {
    let value = database::optional_text(row, "input_json")?;
    if value.is_none()
        && database::rows(
            db,
            &select("workflow_activations")
                .columns(&["activation_id"])
                .where_eq("run_id", run_id)
                .where_eq("node_id", node_id)
                .where_eq("activation_id", activation_id)
                .where_eq("input_json", V::Null)
                .limit(1),
        )?
        .is_empty()
    {
        return Err(WorkflowStoreError::InvalidData(
            "mutation approval input is invalid or oversized".into(),
        ));
    }
    Ok(value)
}
fn permissions_activation_update(
    db: &dyn Database,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
    before: &str,
    after: &str,
) -> Result<u64, WorkflowStoreError> {
    Ok(block_on(
        db.exec_update_count(
            &update("workflow_activations")
                .value("status", after)
                .where_eq("run_id", run_id)
                .where_eq("node_id", node_id)
                .where_eq("activation_id", activation_id)
                .where_eq("status", before),
        ),
    )?)
}
fn permissions_approval_update(
    db: &dyn Database,
    approval_id: &str,
    status: &str,
    grant_id: Option<&str>,
    now: u64,
) -> Result<u64, WorkflowStoreError> {
    let mut query = update("workflow_mutation_approvals")
        .value("status", status)
        .value("resolved_at_ms", database::integer_value(now)?)
        .where_eq("approval_id", approval_id)
        .where_eq("status", "pending");
    if let Some(grant_id) = grant_id {
        query = query.value("grant_id", grant_id);
    }
    Ok(block_on(db.exec_update_count(&query))?)
}
fn permissions_exact_insert(count: u64, label: &str) -> Result<(), WorkflowStoreError> {
    if count != 1 {
        return Err(WorkflowStoreError::InvalidData(format!(
            "{label} insertion did not create exactly one row"
        )));
    }
    Ok(())
}
fn permissions_pending(
    db: &dyn Database,
    run_id: Option<&str>,
    limit: usize,
) -> Result<Vec<WorkflowMutationApproval>, WorkflowStoreError> {
    let limit = usize::try_from(bounded_limit(limit)?)
        .map_err(|_| WorkflowStoreError::InvalidData("invalid approval limit".into()))?;
    let mut query = permissions_approval_query()
        .where_eq("status", "pending")
        .sort("requested_at_ms", SortDirection::Asc)
        .sort("approval_id", SortDirection::Asc)
        .limit(limit);
    if let Some(run_id) = run_id {
        query = query.where_eq("run_id", run_id);
    }
    database::rows(db, &query)?
        .iter()
        .map(|row| {
            let approval = WorkflowMutationApproval {
                approval_id: database::text(row, "approval_id")?,
                run_id: database::text(row, "run_id")?,
                node_id: database::text(row, "node_id")?,
                activation_id: database::text(row, "activation_id")?,
                scope: serde_json::from_str(&database::text(row, "scope_json")?)?,
                requested_at_ms: database::unsigned(row, "requested_at_ms")?,
                expires_at_ms: database::optional_unsigned(row, "expires_at_ms")?,
            };
            validate_mutation_approval(&approval)?;
            Ok(approval)
        })
        .collect()
}
fn permissions_decode_grant(
    row: &Row,
    grant_id: &str,
) -> Result<WorkflowGrant, WorkflowStoreError> {
    let grant = WorkflowGrant {
        grant_id: grant_id.to_owned(),
        run_id: database::text(row, "run_id")?,
        node_id: database::text(row, "node_id")?,
        scope: serde_json::from_str(&database::text(row, "scope_json")?)?,
        granted_at_ms: database::unsigned(row, "granted_at_ms")?,
        expires_at_ms: database::optional_unsigned(row, "expires_at_ms")?,
        max_uses: database::optional_unsigned(row, "max_uses")?
            .map(u32::try_from)
            .transpose()
            .map_err(|_| WorkflowStoreError::InvalidData("max uses exceeds u32".into()))?,
        uses_consumed: permissions_u32(row, "uses_consumed")?,
    };
    validate_grant(&grant)?;
    Ok(grant)
}

fn validate_approval_activation(
    db: &dyn Database,
    approval: &WorkflowMutationApproval,
) -> Result<(), WorkflowStoreError> {
    let activation = permissions_activation(
        db,
        &approval.run_id,
        &approval.node_id,
        &approval.activation_id,
    )?;
    let run = permissions_run(db, &approval.run_id)?;
    let status = database::text(&activation, "status")?;
    let definition_id = database::text(&run, "definition_id")?;
    let definition_version = permissions_u32(&run, "definition_version")?;
    let workspace_snapshot = database::text(&run, "workspace_snapshot")?;
    let input_json = permissions_input(
        db,
        &activation,
        &approval.run_id,
        &approval.node_id,
        &approval.activation_id,
    )?;
    if !matches!(status.as_str(), "pending" | "waiting_mutation_approval")
        || definition_id != approval.scope.definition_id
        || definition_version != approval.scope.definition_version
        || workspace_snapshot != approval.scope.workspace_snapshot
    {
        return Err(WorkflowStoreError::InvalidData(
            "mutation approval does not match dispatchable activation identity".to_string(),
        ));
    }
    validate_mutation_approval_input_checksum(approval, input_json.as_deref(), db)?;
    Ok(())
}
