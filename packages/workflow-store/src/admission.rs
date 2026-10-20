//! Canonical workflow run and child admission.
use super::{
    DispatchReceipt, MAX_ID_BYTES, MAX_INLINE_JSON_BYTES, NewChildWorkflowRun, NewWorkflowRun,
    WorkflowDefinition, WorkflowRunLimits, WorkflowStore, WorkflowStoreError, append_event_in,
    bounded_descendant_count, child_link_for_parent_attempt, continuation,
    create_run_in_transaction, database, diagnose_run_creation_database_error, dispatch_identity,
    execution_authority_in, parse_run_binding, parse_workflow_capability, recovery,
    required_definition_capability, run_graph, validate_child_ancestry, validate_run,
    validate_run_link, verify_store_schema,
};
use crate::database::block_on;
use switchy_database::query::{
    FilterableQuery as _, SelectQuery, bounded_text, identifier, insert, select, update,
};
use switchy_database::{Database, DatabaseError, DatabaseValue as V, Row, TransactionMode};

impl WorkflowStore {
    /// Read one immutable run package binding within the caller's transaction.
    pub(super) fn package_binding_in(
        db: &dyn Database,
        run_id: &str,
    ) -> Result<Option<(String, String)>, WorkflowStoreError> {
        admission_package_binding(db, run_id)
    }

    /// Idempotently create one durable workflow run using a caller-stable identity.
    ///
    /// Returns `true` when the run was created and `false` when the exact immutable request was
    /// already present with a supported run-graph header. Identity reuse with different
    /// definition, snapshot, parent, input, or limits fails closed. Duplicate acknowledgement
    /// validates the graph header only; it does not verify all graph payloads or repair state.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed input, missing definition, conflicting identity, malformed
    /// stored input, a missing, malformed, or unsupported run-graph header, or database failure.
    #[allow(clippy::too_many_lines)]
    pub fn create_run_idempotent(
        &mut self,
        run: &NewWorkflowRun,
    ) -> Result<bool, WorkflowStoreError> {
        self.create_run_with_package(run, None)
    }

    /// Atomically admit a run with an optional exact package execution binding.
    ///
    /// # Errors
    /// Rejects absent publications, nonmember definitions, changed duplicate bindings,
    /// invalid run facts, and storage failures. Existing unbound runs cannot be rebound.
    pub fn create_run_with_package(
        &mut self,
        run: &NewWorkflowRun,
        package: Option<(&str, &str)>,
    ) -> Result<bool, WorkflowStoreError> {
        let mut operation = "begin_transaction";
        self.create_run_with_package_diagnosed(run, package, &mut operation)
            .inspect_err(|error| diagnose_run_creation_database_error(operation, error))
    }

    pub(super) fn create_run_with_package_diagnosed(
        &self,
        run: &NewWorkflowRun,
        package: Option<(&str, &str)>,
        operation: &mut &'static str,
    ) -> Result<bool, WorkflowStoreError> {
        validate_run(run)?;
        let transaction = database::begin(&self.connection, TransactionMode::Immediate)?;
        *operation = "verify_schema";
        verify_store_schema(transaction.as_ref())?;
        *operation = "verify_package";
        if let Some((id, digest)) = package {
            let lock =
                Self::package_lock_on(transaction.as_ref(), id, digest)?.ok_or_else(|| {
                    WorkflowStoreError::InvalidData("exact run package is unavailable".into())
                })?;
            if !lock.members.iter().any(|member| {
                member.definition_identity.definition_id == run.definition_id
                    && member.definition_identity.definition_version == run.definition_version
            }) {
                return Err(WorkflowStoreError::InvalidData(
                    "run definition is not a package member".into(),
                ));
            }
        }
        *operation = "create_or_verify_run";
        let created = Self::create_or_verify_run(transaction.as_ref(), run, true)?;
        *operation = "read_run_package";
        let existing = admission_package_binding(transaction.as_ref(), &run.run_id)?;
        match (existing, package, created) {
            (None, Some((id, digest)), true) => {
                admission_insert_package(transaction.as_ref(), &run.run_id, id, digest)?;
            }
            (Some((id, digest)), Some((requested_id, requested_digest)), _)
                if id == requested_id && digest == requested_digest => {}
            (None, None, _) => {}
            _ => {
                return Err(WorkflowStoreError::InvalidData(
                    "workflow run package binding conflicts with the exact request".into(),
                ));
            }
        }
        *operation = "commit_run";
        database::commit(transaction)?;
        Ok(created)
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn create_or_verify_run(
        transaction: &dyn Database,
        run: &NewWorkflowRun,
        allow_creation: bool,
    ) -> Result<bool, WorkflowStoreError> {
        let authorization_profile_json = serde_json::to_string(&run.authorization_profile)?;
        let rows = database::rows(
            transaction,
            &admission_run_query()
                .where_eq("run_id", run.run_id.as_str())
                .limit(1),
        )?;
        let existing = rows
            .first()
            .map(|row| -> Result<_, WorkflowStoreError> {
                Ok((
                    database::text(row, "definition_id")?,
                    admission_u32(row, "definition_version")?,
                    database::text(row, "workspace_snapshot")?,
                    admission_optional_bounded_text(
                        transaction,
                        row,
                        &run.run_id,
                        "parent_session_id",
                    )?,
                    database::optional_unsigned(row, "parent_session_generation")?,
                    admission_optional_json(transaction, row, &run.run_id, "input_json")?,
                    database::optional_unsigned(row, "deadline_at_ms")?,
                    database::unsigned(row, "node_execution_cap")?,
                    admission_optional_u32(row, "concurrency_cap")?,
                    admission_u32(row, "cycle_cap")?,
                    admission_u32(row, "retry_cap")?,
                    admission_optional_bounded_text(
                        transaction,
                        row,
                        &run.run_id,
                        "owner_plugin_id",
                    )?,
                    admission_optional_bounded_text(
                        transaction,
                        row,
                        &run.run_id,
                        "workflow_kind",
                    )?,
                    admission_optional_bounded_text(transaction, row, &run.run_id, "scope_key")?,
                    admission_optional_bounded_text(
                        transaction,
                        row,
                        &run.run_id,
                        "display_label",
                    )?,
                    admission_i64(row, "single_active")?,
                    admission_optional_json(
                        transaction,
                        row,
                        &run.run_id,
                        "authored_provenance_json",
                    )?,
                    database::text(row, "authorization_profile_json")?,
                    database::text(row, "authorization_ceiling")?,
                ))
            })
            .transpose()?;
        let Some((
            definition_id,
            definition_version,
            workspace_snapshot,
            parent_session_id,
            parent_session_generation,
            input_json,
            deadline_at_ms,
            node_execution_cap,
            concurrency_cap,
            cycle_cap,
            retry_cap,
            owner_plugin_id,
            workflow_kind,
            scope_key,
            display_label,
            single_active,
            authored_provenance_json,
            existing_authorization_profile_json,
            authorization_ceiling,
        )) = existing
        else {
            if !allow_creation {
                return Err(WorkflowStoreError::InvalidData(
                    "workflow child link references a missing run".to_string(),
                ));
            }
            create_run_in_transaction(transaction, run)?;
            return Ok(true);
        };
        if input_json.as_deref() == Some("")
            || authored_provenance_json.as_deref() == Some("")
            || existing_authorization_profile_json.is_empty()
        {
            return Err(WorkflowStoreError::InvalidData(
                "stored workflow run JSON is invalid or oversized".to_string(),
            ));
        }
        let input = input_json
            .map(|value| serde_json::from_str(&value))
            .transpose()
            .map_err(|_| {
                WorkflowStoreError::InvalidData(
                    "stored workflow run input JSON is malformed".to_string(),
                )
            })?;
        let authored_provenance = authored_provenance_json
            .map(|value| serde_json::from_str(&value))
            .transpose()
            .map_err(|_| {
                WorkflowStoreError::InvalidData(
                    "stored workflow run authored provenance JSON is malformed".to_string(),
                )
            })?;
        let limits = WorkflowRunLimits {
            deadline_at_ms,
            node_execution_cap: continuation::admission_execution_cap(
                transaction,
                &run.run_id,
                node_execution_cap,
            )?,
            concurrency_cap,
            cycle_cap,
            retry_cap,
            recursion_depth_cap: admission_u32(&rows[0], "recursion_depth_cap")?,
            descendant_cap: admission_u32(&rows[0], "descendant_cap")?,
        };
        let existing_binding = parse_run_binding(
            owner_plugin_id,
            workflow_kind,
            scope_key,
            display_label,
            single_active,
        )?;
        if definition_id == run.definition_id
            && definition_version == run.definition_version
            && workspace_snapshot == run.workspace_snapshot
            && parent_session_id == run.parent_session_id
            && parent_session_generation == run.parent_session_generation
            && existing_binding == run.binding
            && authored_provenance == run.authored_provenance
            && existing_authorization_profile_json == authorization_profile_json
            && parse_workflow_capability(&authorization_ceiling)? == run.authorization_ceiling
            && input == run.input
            && limits == run.limits
        {
            if run_graph::graph_revision(transaction, &run.run_id).map_err(|error| match error {
                WorkflowStoreError::StructuredDatabase(
                    switchy_database::DatabaseError::Rusqlite(
                        switchy_database::rusqlite::RusqliteDatabaseError::UnsupportedBlob,
                    ),
                ) => {
                    WorkflowStoreError::InvalidData("workflow run graph revision is invalid".into())
                }
                other => other,
            })? != Some(1)
            {
                return Err(WorkflowStoreError::InvalidData(
                    "idempotent workflow start requires a supported run graph revision".to_string(),
                ));
            }
            return Ok(false);
        }
        Err(WorkflowStoreError::InvalidData(format!(
            "workflow run identity conflicts with an existing run: {}",
            run.run_id
        )))
    }

    /// Create one durable workflow run bound to an existing exact definition version.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, missing definition, identity conflict, or database
    /// failure.
    #[allow(clippy::too_many_lines)]
    pub fn create_run(&mut self, run: &NewWorkflowRun) -> Result<(), WorkflowStoreError> {
        validate_run(run)?;
        let transaction = database::begin(&self.connection, TransactionMode::Immediate)?;
        verify_store_schema(transaction.as_ref())?;
        create_run_in_transaction(transaction.as_ref(), run)?;
        database::commit(transaction)?;
        Ok(())
    }

    /// Atomically create and link one exact child run before parent admission completes.
    ///
    /// Returns `true` when created and `false` for a byte-equivalent duplicate parent attempt.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed ownership links, missing/invalid parent attempts, workspace
    /// mismatch, depth/descendant overflow, exact-target mismatch, identity conflict, or database
    /// failure. No child or link is retained on error.
    #[allow(clippy::too_many_lines)]
    pub fn create_child_run_idempotent(
        &mut self,
        request: &NewChildWorkflowRun,
    ) -> Result<bool, WorkflowStoreError> {
        validate_run(&request.run)?;
        validate_run_link(&request.link, &request.run)?;
        let transaction = database::begin(&self.connection, TransactionMode::Immediate)?;
        recovery::require_execution(transaction.as_ref(), &request.link.parent_run_id)?;
        let parent_row = admission_required_row(
            transaction.as_ref(),
            &select("workflow_runs")
                .project_as(
                    bounded_text(
                        identifier("workspace_snapshot"),
                        MAX_ID_BYTES as u64,
                        V::Null,
                    ),
                    "workspace_snapshot",
                )
                .project_as(bounded_text(identifier("status"), 64, V::Null), "status")
                .project_as(
                    bounded_text(identifier("authorization_ceiling"), 64, V::Null),
                    "authorization_ceiling",
                )
                .project_as(
                    bounded_text(
                        identifier("authorization_profile_json"),
                        MAX_INLINE_JSON_BYTES as u64,
                        V::Null,
                    ),
                    "authorization_profile_json",
                )
                .where_eq("run_id", request.link.parent_run_id.as_str())
                .where_eq("cancellation_requested_at_ms", V::Null)
                .limit(1),
        )
        .map_err(|error| match error {
            WorkflowStoreError::StructuredDatabase(DatabaseError::NoRow) => {
                WorkflowStoreError::InvalidData(
                    "workflow child parent run/activation is missing".into(),
                )
            }
            error => error,
        })?;
        validate_child_parent_definition(transaction.as_ref(), &request.link.parent_run_id)?;
        let parent_activation = admission_required_row(
            transaction.as_ref(),
            &select("workflow_activations")
                .project_as(bounded_text(identifier("status"), 64, V::Null), "status")
                .where_eq("run_id", request.link.parent_run_id.as_str())
                .where_eq("node_id", request.link.parent_node_id.as_str())
                .where_eq("activation_id", request.link.parent_activation_id.as_str())
                .limit(1),
        )
        .map_err(|error| match error {
            WorkflowStoreError::StructuredDatabase(DatabaseError::NoRow) => {
                WorkflowStoreError::InvalidData(
                    "workflow child parent run/activation is missing".into(),
                )
            }
            error => error,
        })?;
        let parent = (
            database::text(&parent_row, "workspace_snapshot")?,
            database::text(&parent_row, "status")?,
            database::text(&parent_activation, "status")?,
            database::text(&parent_row, "authorization_ceiling")?,
            database::text(&parent_row, "authorization_profile_json")?,
        );
        if parent.0 != request.run.workspace_snapshot || parent.1 != "running" {
            return Err(WorkflowStoreError::InvalidData(
                "workflow child must share the active parent workspace".to_string(),
            ));
        }
        let parent_authorization_ceiling = parse_workflow_capability(&parent.3)?;
        let parent_authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity =
            serde_json::from_str(&parent.4)?;
        if request.run.authorization_profile != parent_authorization_profile {
            return Err(WorkflowStoreError::InvalidData(
                "workflow child authorization profile differs from its parent".to_string(),
            ));
        }
        if request.run.authorization_ceiling > parent_authorization_ceiling {
            return Err(WorkflowStoreError::InvalidData(
                "workflow child authorization ceiling exceeds its parent".to_string(),
            ));
        }
        if request.run.binding.is_some() {
            return Err(WorkflowStoreError::InvalidData(
                "workflow child cannot inherit or introduce an ambient product binding".to_string(),
            ));
        }
        let parent_node = run_graph::initial_activation_node(
            transaction.as_ref(),
            &request.link.parent_run_id,
            &request.link.parent_node_id,
            &request.link.parent_activation_id,
        )?
        .ok_or_else(|| {
            WorkflowStoreError::InvalidData("parent call executable binding is missing".to_string())
        })?;
        if parent_node.kind != bcode_workflow::NodeKind::WorkflowCall {
            return Err(WorkflowStoreError::InvalidData(
                "workflow child parent node is not a workflow call".to_string(),
            ));
        }
        let configuration: bcode_workflow::WorkflowCallConfiguration =
            serde_json::from_value(parent_node.configuration)?;
        configuration
            .validate()
            .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
        if configuration.target != request.link.target {
            return Err(WorkflowStoreError::InvalidData(
                "workflow child exact target differs from the parent call node".to_string(),
            ));
        }
        let authority = request.run.execution_authority.as_ref();
        let owned = execution_authority_in(transaction.as_ref(), &request.link.parent_run_id)?
            .as_ref()
            == authority;
        if !owned {
            return Err(WorkflowStoreError::InvalidData(
                "workflow child authority is stale or foreign".into(),
            ));
        }
        let mut resolved_local_identity = None;
        let inherited_package =
            if let bcode_workflow::WorkflowCallTarget::PackageLocal { reference } =
                &configuration.target
            {
                if authority.is_none() {
                    return Err(WorkflowStoreError::InvalidData(
                        "package child requires execution authority".into(),
                    ));
                }
                let binding =
                    admission_package_binding(transaction.as_ref(), &request.link.parent_run_id)?
                        .ok_or_else(|| {
                        WorkflowStoreError::InvalidData("local call requires bound parent".into())
                    })?;
                let lock = Self::package_lock_on(transaction.as_ref(), &binding.0, &binding.1)?
                    .ok_or_else(|| {
                        WorkflowStoreError::InvalidData("immutable package lock unavailable".into())
                    })?;
                let member = lock
                    .members
                    .iter()
                    .find(|member| member.member_id == reference.member_id)
                    .ok_or_else(|| {
                        WorkflowStoreError::InvalidData(
                            "local member absent from parent lock".into(),
                        )
                    })?;
                let stored = Self::definition_in(
                    transaction.as_ref(),
                    &member.definition_identity.definition_id,
                    member.definition_identity.definition_version,
                )?
                .ok_or_else(|| {
                    WorkflowStoreError::InvalidData("locked child definition unavailable".into())
                })?;
                let definition: WorkflowDefinition = serde_json::from_str(&stored.definition_json)?;
                resolved_local_identity = Some(
                    reference
                        .resolve(&lock, &definition)
                        .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?,
                );
                Some(binding)
            } else if let bcode_workflow::WorkflowCallTarget::PackageMember {
                member_id,
                definition_identity,
            } = &configuration.target
            {
                if authority.is_none() {
                    return Err(WorkflowStoreError::InvalidData(
                        "package child requires execution authority".into(),
                    ));
                }
                let binding =
                    admission_package_binding(transaction.as_ref(), &request.link.parent_run_id)?
                        .ok_or_else(|| {
                        WorkflowStoreError::InvalidData(
                            "package member call requires a bound parent run".into(),
                        )
                    })?;
                let lock = Self::package_lock_on(transaction.as_ref(), &binding.0, &binding.1)?
                    .ok_or_else(|| {
                        WorkflowStoreError::InvalidData("parent package is unavailable".into())
                    })?;
                if !lock.members.iter().any(|member| {
                    member.member_id == *member_id
                        && member.definition_identity == *definition_identity
                }) {
                    return Err(WorkflowStoreError::InvalidData(
                        "package member target differs from parent lock".into(),
                    ));
                }
                Some(binding)
            } else {
                None
            };
        if let Some(existing) = child_link_for_parent_attempt(transaction.as_ref(), &request.link)?
        {
            if existing == request.link {
                let child_binding =
                    admission_package_binding(transaction.as_ref(), &request.run.run_id)?;
                if child_binding != inherited_package {
                    return Err(WorkflowStoreError::InvalidData(
                        "duplicate child package binding differs".into(),
                    ));
                }
                return Self::create_or_verify_run(transaction.as_ref(), &request.run, false);
            }
            return Err(WorkflowStoreError::InvalidData(
                "workflow child parent attempt conflicts with an existing link".to_string(),
            ));
        }
        let prepared_exists = !database::rows(
            transaction.as_ref(),
            &select("workflow_attempts")
                .columns(&["attempt"])
                .where_eq("run_id", request.link.parent_run_id.as_str())
                .where_eq("node_id", request.link.parent_node_id.as_str())
                .where_eq("activation_id", request.link.parent_activation_id.as_str())
                .where_eq("attempt", i64::from(request.link.parent_attempt))
                .where_eq("status", "prepared")
                .limit(1),
        )?
        .is_empty();
        if !prepared_exists {
            return Err(WorkflowStoreError::InvalidData(
                "workflow child parent attempt is not durably prepared".to_string(),
            ));
        }
        let parent_limits = admission_limits(&admission_required_row(
            transaction.as_ref(),
            &admission_run_query()
                .where_eq("run_id", request.link.parent_run_id.as_str())
                .limit(1),
        )?)?;
        let deadline_exceeds = match (
            request.run.limits.deadline_at_ms,
            parent_limits.deadline_at_ms,
        ) {
            (None, Some(_)) => true,
            (Some(child), Some(parent)) => child > parent,
            _ => false,
        };
        if deadline_exceeds
            || request.run.limits.node_execution_cap > parent_limits.node_execution_cap
            || parent_limits.concurrency_cap.is_some_and(|parent| {
                request
                    .run
                    .limits
                    .concurrency_cap
                    .is_none_or(|child| child > parent)
            })
            || request.run.limits.cycle_cap > parent_limits.cycle_cap
            || request.run.limits.retry_cap > parent_limits.retry_cap
            || request.run.limits.recursion_depth_cap > parent_limits.recursion_depth_cap
            || request.run.limits.descendant_cap > parent_limits.descendant_cap
            || request.link.depth > parent_limits.recursion_depth_cap
        {
            return Err(WorkflowStoreError::InvalidData(
                "workflow child execution limits exceed the inherited parent limits".to_string(),
            ));
        }
        let root_execution_count = crate::typed_root_attempt_count(
            transaction.as_ref(),
            &request.link.root_run_id,
            parent_limits.node_execution_cap,
        )?;
        if root_execution_count >= parent_limits.node_execution_cap {
            return Err(WorkflowStoreError::InvalidData(
                "workflow root node-execution cap reached before child admission".to_string(),
            ));
        }
        let descendant_count = bounded_descendant_count(
            transaction.as_ref(),
            &request.link.root_run_id,
            parent_limits.descendant_cap,
        )?;
        if descendant_count >= parent_limits.descendant_cap {
            return Err(WorkflowStoreError::InvalidData(
                "workflow child descendant limit reached".to_string(),
            ));
        }
        let target = if let Some(identity) = resolved_local_identity.as_ref() {
            identity
        } else {
            request
                .link
                .target
                .definition_identity()
                .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?
        };
        let child_definition_json = database::text(
            &admission_required_row(
                transaction.as_ref(),
                &select("workflow_definitions")
                    .project_as(
                        bounded_text(
                            identifier("definition_json"),
                            MAX_INLINE_JSON_BYTES as u64,
                            V::Null,
                        ),
                        "definition_json",
                    )
                    .where_eq("definition_id", target.definition_id.as_str())
                    .where_eq("version", i64::from(target.definition_version))
                    .limit(1),
            )?,
            "definition_json",
        )?;
        let child_definition: WorkflowDefinition = serde_json::from_str(&child_definition_json)?;
        if required_definition_capability(&child_definition, "")?
            > request.run.authorization_ceiling
        {
            return Err(WorkflowStoreError::InvalidData(
                "workflow child requirements exceed the inherited authorization ceiling"
                    .to_string(),
            ));
        }
        validate_child_ancestry(transaction.as_ref(), &request.link)?;
        if request.run.definition_id != target.definition_id
            || request.run.definition_version != target.definition_version
        {
            return Err(WorkflowStoreError::InvalidData(
                "workflow child run does not use the exact target definition".to_string(),
            ));
        }
        create_run_in_transaction(transaction.as_ref(), &request.run)?;
        if let Some((id, digest)) = inherited_package {
            admission_insert_package(transaction.as_ref(), &request.run.run_id, &id, &digest)?;
        }
        let child_receipt = serde_json::json!({
            "owner": "bcode.server.workflow-child/v1",
            "child_run_id": request.link.child_run_id,
            "root_run_id": request.link.root_run_id,
            "depth": request.link.depth,
            "target": request.link.target,
        });
        let child_receipt_json = serde_json::to_string(&child_receipt)?;
        let linked = block_on(
            insert("workflow_run_links")
                .value("root_run_id", request.link.root_run_id.as_str())
                .value("parent_run_id", request.link.parent_run_id.as_str())
                .value("parent_node_id", request.link.parent_node_id.as_str())
                .value(
                    "parent_activation_id",
                    request.link.parent_activation_id.as_str(),
                )
                .value("parent_attempt", i64::from(request.link.parent_attempt))
                .value("child_run_id", request.link.child_run_id.as_str())
                .value("version", i64::from(request.link.version))
                .value("target_json", serde_json::to_string(&request.link.target)?)
                .value("depth", i64::from(request.link.depth))
                .value(
                    "created_at_ms",
                    database::integer_value(request.link.created_at_ms)?,
                )
                .on_conflict_do_nothing(["child_run_id"])
                .execute_count(transaction.as_ref()),
        )?;
        if linked != 1 {
            return Err(WorkflowStoreError::InvalidData(
                "workflow child link did not create exactly one row".into(),
            ));
        }
        let admitted = block_on(
            transaction.exec_update_count(
                &update("workflow_attempts")
                    .value("status", "admitted")
                    .value("receipt_json", child_receipt_json)
                    .value(
                        "admitted_at_ms",
                        database::integer_value(request.link.created_at_ms)?,
                    )
                    .where_eq("run_id", request.link.parent_run_id.as_str())
                    .where_eq("node_id", request.link.parent_node_id.as_str())
                    .where_eq("activation_id", request.link.parent_activation_id.as_str())
                    .where_eq("attempt", i64::from(request.link.parent_attempt))
                    .where_eq("status", "prepared"),
            ),
        )?;
        if admitted != 1 {
            return Err(WorkflowStoreError::InvalidData(
                "workflow child parent attempt could not commit its admission receipt".to_string(),
            ));
        }
        append_event_in(
            transaction.as_ref(),
            &request.link.parent_run_id,
            "attempt_admitted",
            &serde_json::to_string(&DispatchReceipt {
                run_id: request.link.parent_run_id.clone(),
                node_id: request.link.parent_node_id.clone(),
                activation_id: request.link.parent_activation_id.clone(),
                attempt: request.link.parent_attempt,
                dispatch_identity: dispatch_identity(
                    &request.link.parent_run_id,
                    &request.link.parent_node_id,
                    &request.link.parent_activation_id,
                    request.link.parent_attempt,
                ),
                receipt: child_receipt,
                admitted_at_ms: request.link.created_at_ms,
            })?,
            request.link.created_at_ms,
        )?;
        append_event_in(
            transaction.as_ref(),
            &request.link.parent_run_id,
            "child_run_linked",
            &serde_json::to_string(&request.link)?,
            request.link.created_at_ms,
        )?;
        database::commit(transaction)?;
        Ok(true)
    }
}

fn admission_required_row(
    db: &dyn Database,
    query: &SelectQuery<'_>,
) -> Result<Row, WorkflowStoreError> {
    database::rows(db, query)?
        .into_iter()
        .next()
        .ok_or_else(|| DatabaseError::NoRow.into())
}
fn admission_u32(row: &Row, column: &str) -> Result<u32, WorkflowStoreError> {
    u32::try_from(database::unsigned(row, column)?).map_err(|_| {
        WorkflowStoreError::InvalidData(format!("workflow admission {column} exceeds u32"))
    })
}
fn admission_optional_u32(row: &Row, column: &str) -> Result<Option<u32>, WorkflowStoreError> {
    database::optional_unsigned(row, column)?
        .map(|value| {
            u32::try_from(value).map_err(|_| {
                WorkflowStoreError::InvalidData(format!("workflow admission {column} exceeds u32"))
            })
        })
        .transpose()
}
fn admission_i64(row: &Row, column: &str) -> Result<i64, WorkflowStoreError> {
    match row.get(column) {
        Some(V::Int64(value)) => Ok(value),
        _ => Err(WorkflowStoreError::InvalidData(format!(
            "invalid workflow admission {column}"
        ))),
    }
}
fn admission_run_query() -> SelectQuery<'static> {
    let mut query = select("workflow_runs");
    for column in [
        "definition_version",
        "parent_session_generation",
        "deadline_at_ms",
        "node_execution_cap",
        "concurrency_cap",
        "cycle_cap",
        "retry_cap",
        "recursion_depth_cap",
        "descendant_cap",
        "single_active",
    ] {
        query = query.project(identifier(column));
    }
    for column in [
        "definition_id",
        "workspace_snapshot",
        "parent_session_id",
        "owner_plugin_id",
        "workflow_kind",
        "scope_key",
        "display_label",
        "authorization_ceiling",
    ] {
        query = query.project_as(
            bounded_text(identifier(column), MAX_ID_BYTES as u64, V::Null),
            column,
        );
    }
    for column in [
        "input_json",
        "authored_provenance_json",
        "authorization_profile_json",
    ] {
        query = query.project_as(
            bounded_text(identifier(column), MAX_INLINE_JSON_BYTES as u64, V::Null),
            column,
        );
    }
    query
}
fn admission_optional_bounded_text(
    db: &dyn Database,
    row: &Row,
    run_id: &str,
    column: &str,
) -> Result<Option<String>, WorkflowStoreError> {
    let value = database::optional_text(row, column)?;
    if value.is_none()
        && database::rows(
            db,
            &select("workflow_runs")
                .columns(&["run_id"])
                .where_eq("run_id", run_id)
                .where_eq(column, V::Null)
                .limit(1),
        )?
        .is_empty()
    {
        return Err(WorkflowStoreError::InvalidData(format!(
            "stored workflow run {column} is invalid or oversized"
        )));
    }
    Ok(value)
}
fn admission_optional_json(
    db: &dyn Database,
    row: &Row,
    run_id: &str,
    column: &str,
) -> Result<Option<String>, WorkflowStoreError> {
    admission_optional_bounded_text(db, row, run_id, column)
}
fn admission_limits(row: &Row) -> Result<WorkflowRunLimits, WorkflowStoreError> {
    Ok(WorkflowRunLimits {
        deadline_at_ms: database::optional_unsigned(row, "deadline_at_ms")?,
        node_execution_cap: database::unsigned(row, "node_execution_cap")?,
        concurrency_cap: admission_optional_u32(row, "concurrency_cap")?,
        cycle_cap: admission_u32(row, "cycle_cap")?,
        retry_cap: admission_u32(row, "retry_cap")?,
        recursion_depth_cap: admission_u32(row, "recursion_depth_cap")?,
        descendant_cap: admission_u32(row, "descendant_cap")?,
    })
}
fn admission_package_binding(
    db: &dyn Database,
    run_id: &str,
) -> Result<Option<(String, String)>, WorkflowStoreError> {
    database::rows(
        db,
        &select("workflow_run_packages")
            .project_as(
                bounded_text(identifier("package_id"), MAX_ID_BYTES as u64, V::Null),
                "package_id",
            )
            .project_as(
                bounded_text(identifier("lock_digest"), 64, V::Null),
                "lock_digest",
            )
            .where_eq("run_id", run_id)
            .limit(1),
    )?
    .first()
    .map(|row| {
        Ok((
            database::text(row, "package_id")?,
            database::text(row, "lock_digest")?,
        ))
    })
    .transpose()
}
fn admission_insert_package(
    db: &dyn Database,
    run_id: &str,
    id: &str,
    digest: &str,
) -> Result<(), WorkflowStoreError> {
    let changed = block_on(
        insert("workflow_run_packages")
            .value("run_id", run_id)
            .value("package_id", id)
            .value("lock_digest", digest)
            .on_conflict_do_nothing(["run_id"])
            .execute_count(db),
    )?;
    if changed != 1 {
        return Err(WorkflowStoreError::InvalidData(
            "workflow run package insertion did not create exactly one row".into(),
        ));
    }
    Ok(())
}

fn validate_child_parent_definition(
    db: &dyn Database,
    run_id: &str,
) -> Result<(), WorkflowStoreError> {
    let row = admission_required_row(
        db,
        &select("workflow_runs")
            .columns(&["definition_id", "definition_version"])
            .where_eq("run_id", run_id)
            .limit(1),
    )?;
    let definition_id = database::text(&row, "definition_id")?;
    let version = admission_u32(&row, "definition_version")?;
    crate::definition_in(db, &definition_id, version)?.ok_or_else(|| {
        WorkflowStoreError::InvalidData("workflow child parent definition missing".into())
    })?;
    Ok(())
}
