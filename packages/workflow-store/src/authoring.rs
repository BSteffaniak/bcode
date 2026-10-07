//! Authored workflow, definition and publication lifecycle.
use super::*;
use crate::database::block_on;
use switchy_database::query::{
    FilterableQuery, Projection, SortDirection, bounded_text, byte_length, identifier, select,
    where_and, where_eq, where_gt, where_lt, where_or,
};
use switchy_database::{Database, DatabaseValue, Row, TransactionMode};

impl WorkflowStore {
    /// Persist an immutable normalized definition and checksum.
    ///
    /// The compiled definition is structurally validated before serialization. Re-persisting
    /// byte-identical content is idempotent. Reusing one definition/version for different content
    /// fails closed.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid identity/version, serialization failure, checksum conflict, or
    /// database failure.
    pub fn persist_definition(
        &mut self,
        definition_id: &str,
        version: u32,
        definition: &WorkflowDefinition,
    ) -> Result<StoredWorkflowDefinition, WorkflowStoreError> {
        validate_id("definition_id", definition_id)?;
        bcode_workflow::reject_definition_execution_parent_claims(definition)
            .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
        definition.validate().map_err(|error| {
            WorkflowStoreError::InvalidData(format!("invalid workflow definition: {error}"))
        })?;
        if version == 0 {
            return Err(WorkflowStoreError::InvalidData(
                "definition version must be positive".to_string(),
            ));
        }
        let definition_json = serde_json::to_string(definition)?;
        if definition_json.len() > bcode_workflow::MAX_REGISTERED_WORKFLOW_DEFINITION_BYTES {
            return Err(WorkflowStoreError::InvalidData(format!(
                "workflow definition exceeds {} bytes",
                bcode_workflow::MAX_REGISTERED_WORKFLOW_DEFINITION_BYTES
            )));
        }
        let checksum_sha256 = sha256_hex(definition_json.as_bytes());
        let stored = StoredWorkflowDefinition {
            definition_id: definition_id.to_string(),
            version,
            checksum_sha256,
            definition_json,
        };
        let transaction = database::begin(&self.connection, TransactionMode::Immediate)?;
        verify_store_schema(transaction.as_ref())?;
        persist_definition_transaction(transaction.as_ref(), &stored)?;
        database::commit(transaction)?;
        Ok(stored)
    }

    /// Return a bounded definition list ordered by identity and newest version first.
    ///
    /// Each row is checksum-verified before it is returned. This normal discovery path performs no
    /// replay, repair, or external work.
    ///
    /// # Errors
    ///
    /// Returns an error when `limit` is invalid, a checksum is inconsistent, or the bounded query
    /// fails.
    pub fn list_definitions(
        &self,
        limit: usize,
    ) -> Result<Vec<StoredWorkflowDefinition>, WorkflowStoreError> {
        let limit = authoring_limit(bounded_limit(limit)?)?;
        authoring_rows(
            &self.connection,
            definition_query()
                .sort("definition_id", SortDirection::Asc)
                .sort("version", SortDirection::Desc)
                .limit(limit),
        )?
        .iter()
        .map(decode_authoring_definition)
        .map(|stored| stored.and_then(verify_stored_definition))
        .collect()
    }

    /// Read a complete definition snapshot or reject it before payload loading.
    ///
    /// Unlike a listing window, this must not be used as a partial compilation catalog.
    /// The row check visits at most `limit + 1` index entries; no full count is needed.
    ///
    /// # Errors
    /// Returns an error for invalid limits, row/byte budget overflow, checksum damage,
    /// or storage failure. Larger stores remain intact and accessible by exact lookup.
    pub fn definition_snapshot_with_budget(
        &self,
        limit: usize,
        byte_budget: usize,
    ) -> Result<Vec<StoredWorkflowDefinition>, WorkflowStoreError> {
        let max_candidates = authoring_limit(bounded_limit(limit)?)?;
        let transaction = database::begin(&self.connection, TransactionMode::Deferred)?;
        let count = authoring_rows(
            transaction.as_ref(),
            select("workflow_definitions")
                .columns(&["definition_id"])
                .limit(max_candidates.saturating_add(1)),
        )?
        .len();
        if count > max_candidates {
            return Err(WorkflowStoreError::InvalidData(
                "definition snapshot exceeds candidate budget".into(),
            ));
        }
        let definitions =
            definitions_with_budget_on(transaction.as_ref(), count.max(1), byte_budget)?;
        database::commit(transaction)?;
        Ok(definitions)
    }

    /// Read a row-limited definition window within an aggregate encoded-JSON budget.
    ///
    /// Payload lengths are checked before loading JSON. An oversized window is an
    /// error, never a partial success. This bounds encoded input, not decoded heap.
    ///
    /// # Errors
    /// Returns an error for invalid limits, exceeded budget, checksum damage, or storage failure.
    pub fn list_definitions_with_byte_budget(
        &self,
        limit: usize,
        byte_budget: usize,
    ) -> Result<Vec<StoredWorkflowDefinition>, WorkflowStoreError> {
        definitions_with_budget_on(
            &self.connection,
            authoring_limit(bounded_limit(limit)?)?,
            byte_budget,
        )
    }

    /// Create a logical authored workflow and its initial draft atomically.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed state, mismatched identities, an identity conflict, or a
    /// database failure. Any error rolls back both inserts.
    pub fn create_authored_workflow_with_initial_draft(
        &mut self,
        workflow: &AuthoredWorkflow,
        draft: &WorkflowDraft,
    ) -> Result<(), WorkflowStoreError> {
        validate_authored_workflow(workflow)?;
        validate_workflow_draft(draft)?;
        if workflow.active_revision.is_some()
            || workflow.workflow_id != draft.workflow_id
            || draft.base_revision.is_some()
            || draft.generation != 1
        {
            return Err(WorkflowStoreError::InvalidData(
                "initial authored workflow and draft identities or initial state are invalid"
                    .to_string(),
            ));
        }
        let document_json = bounded_json("draft document", &draft.document)?;
        let producer_json = bounded_json("draft producer", &draft.producer)?;
        let transaction = database::begin(&self.connection, TransactionMode::Deferred)?;
        authoring_insert(
            transaction.as_ref(),
            &(transaction
                .as_ref()
                .insert("authored_workflows")
                .value("workflow_id", DatabaseValue::from(&workflow.workflow_id))
                .value("title", DatabaseValue::from(&workflow.title))
                .value(
                    "description",
                    DatabaseValue::from(workflow.description.clone()),
                )
                .value("archived", DatabaseValue::from(workflow.archived))
                .value("active_revision", DatabaseValue::Null)
                .value("created_at_ms", DatabaseValue::from(workflow.created_at_ms))
                .value("updated_at_ms", DatabaseValue::from(workflow.updated_at_ms))),
        )?;
        authoring_insert(
            transaction.as_ref(),
            &(transaction
                .as_ref()
                .insert("workflow_drafts")
                .value("workflow_id", DatabaseValue::from(&draft.workflow_id))
                .value("draft_id", DatabaseValue::from(&draft.draft_id))
                .value("base_revision", DatabaseValue::Null)
                .value("generation", DatabaseValue::from(draft.generation))
                .value(
                    "checksum_sha256",
                    DatabaseValue::from(&draft.checksum_sha256),
                )
                .value("document_json", DatabaseValue::from(&document_json))
                .value("producer_json", DatabaseValue::from(&producer_json))
                .value("created_at_ms", DatabaseValue::from(draft.created_at_ms))
                .value("updated_at_ms", DatabaseValue::from(draft.updated_at_ms))),
        )?;
        database::commit(transaction)?;
        Ok(())
    }

    /// Create one logical authored workflow.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed metadata, invalid initial state, identity conflict, or a
    /// database failure.
    pub fn create_authored_workflow(
        &mut self,
        workflow: &AuthoredWorkflow,
    ) -> Result<bool, WorkflowStoreError> {
        validate_authored_workflow(workflow)?;
        if workflow.active_revision.is_some() {
            return Err(WorkflowStoreError::InvalidData(
                "a new authored workflow cannot have an active revision".to_string(),
            ));
        }
        let changed = block_on(
            (self
                .connection
                .insert("authored_workflows")
                .value("workflow_id", DatabaseValue::from(&workflow.workflow_id))
                .value("title", DatabaseValue::from(&workflow.title))
                .value(
                    "description",
                    DatabaseValue::from(workflow.description.clone()),
                )
                .value("archived", DatabaseValue::from(workflow.archived))
                .value("active_revision", DatabaseValue::Null)
                .value("created_at_ms", DatabaseValue::from(workflow.created_at_ms))
                .value("updated_at_ms", DatabaseValue::from(workflow.updated_at_ms)))
            .or_ignore()
            .execute_count(&self.connection),
        )?;
        if changed == 1 {
            return Ok(true);
        }
        let existing = self
            .authored_workflow(&workflow.workflow_id)?
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData(
                    "authored workflow insert was not observable".to_string(),
                )
            })?;
        if existing == *workflow {
            Ok(false)
        } else {
            Err(WorkflowStoreError::InvalidData(format!(
                "authored workflow identity conflict: {}",
                workflow.workflow_id
            )))
        }
    }

    /// Load one logical authored workflow without mutation.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, inconsistent stored data, or query failure.
    pub fn authored_workflow(
        &self,
        workflow_id: &str,
    ) -> Result<Option<AuthoredWorkflow>, WorkflowStoreError> {
        validate_id("workflow_id", workflow_id)?;
        authoring_one(
            &self.connection,
            select("authored_workflows")
                .columns(&[
                    "workflow_id",
                    "title",
                    "description",
                    "archived",
                    "active_revision",
                    "created_at_ms",
                    "updated_at_ms",
                ])
                .where_eq("workflow_id", DatabaseValue::from(workflow_id)),
        )?
        .as_ref()
        .map(decode_authored_workflow)
        .transpose()?
        .map(validate_stored_authored_workflow)
        .transpose()
    }

    /// Return a bounded authored-workflow list ordered by recent update.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid bound, inconsistent stored data, or query failure.
    pub fn list_authored_workflows(
        &self,
        limit: usize,
    ) -> Result<Vec<AuthoredWorkflow>, WorkflowStoreError> {
        Ok(self.list_authored_workflows_page(None, limit)?.items)
    }

    /// Return one stable keyset page of authored workflows ordered by recent update.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid cursor/bound, inconsistent stored data, or query failure.
    pub fn list_authored_workflows_page(
        &self,
        cursor: Option<&WorkflowAuthoringListCursor>,
        limit: usize,
    ) -> Result<WorkflowAuthoringStorePage<AuthoredWorkflow>, WorkflowStoreError> {
        if let Some(cursor) = cursor {
            cursor
                .validate()
                .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
        }
        let query_limit = authoring_limit(authoring_page_query_limit(limit)?)?;
        let query = authoring_cursor(
            select("authored_workflows").columns(AUTHORED_COLUMNS),
            cursor,
            "workflow_id",
        )?
        .sort("updated_at_ms", SortDirection::Desc)
        .sort("workflow_id", SortDirection::Asc)
        .limit(query_limit);
        authoring_rows(&self.connection, query)?
            .iter()
            .map(decode_authored_workflow)
            .map(|row| row.and_then(validate_stored_authored_workflow))
            .collect::<Result<Vec<_>, _>>()
            .map(|items| finish_authoring_store_page(items, limit))
    }

    /// Archive or unarchive one logical workflow without deleting history.
    ///
    /// # Errors
    ///
    /// Returns an error when the workflow is missing, the timestamp regresses, or the update fails.
    pub fn set_authored_workflow_archived(
        &mut self,
        workflow_id: &str,
        archived: bool,
        updated_at_ms: u64,
    ) -> Result<(), WorkflowStoreError> {
        validate_id("workflow_id", workflow_id)?;
        let changed = block_on(
            self.connection.exec_update_count(
                &(self
                    .connection
                    .update("authored_workflows")
                    .value("archived", DatabaseValue::from(archived))
                    .value("updated_at_ms", DatabaseValue::from(updated_at_ms))
                    .where_eq("workflow_id", DatabaseValue::from(workflow_id))
                    .where_lte("updated_at_ms", DatabaseValue::from(updated_at_ms))),
            ),
        )?;
        if changed == 0 {
            return Err(WorkflowStoreError::InvalidData(format!(
                "authored workflow is missing or update timestamp regressed: {workflow_id}"
            )));
        }
        Ok(())
    }

    /// Atomically apply every member of one validated package plan as the `package` draft.
    ///
    /// Existing members require exact expected generations; omitted members are create-only. Any
    /// conflict or failure rolls back all authored workflow and draft mutations.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid package facts, stale generations, existing create-only
    /// members, timestamp regression, malformed documents, or database failure.
    #[allow(clippy::too_many_lines)]
    pub fn apply_workflow_package(
        &mut self,
        request: &WorkflowPackageApplyRequest,
        applied_at_ms: u64,
    ) -> Result<WorkflowPackageMutationResult, WorkflowStoreError> {
        self.apply_workflow_package_with_fault(
            request,
            applied_at_ms,
            &NoopWorkflowPackageMutationFault,
        )
    }

    /// Apply one package with deterministic in-transaction fault injection.
    ///
    /// # Errors
    ///
    /// Returns any normal apply error or injected fault; every staged member is rolled back.
    #[allow(clippy::too_many_lines)]
    pub fn apply_workflow_package_with_fault<F>(
        &mut self,
        request: &WorkflowPackageApplyRequest,
        applied_at_ms: u64,
        fault: &F,
    ) -> Result<WorkflowPackageMutationResult, WorkflowStoreError>
    where
        F: WorkflowPackageMutationFault + ?Sized,
    {
        request
            .validate()
            .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
        let expected = request
            .expected_generations
            .iter()
            .map(|fact| (fact.member_id.as_str(), fact.expected_generation))
            .collect::<std::collections::BTreeMap<_, _>>();
        let transaction = database::begin(&self.connection, TransactionMode::Deferred)?;
        let producer = WorkflowProducerProvenance {
            kind: bcode_workflow::WorkflowProducerKind::Human,
            producer_id: Some(format!("package:{}", request.plan.package_id)),
            source_revision: None,
        };
        producer
            .validate()
            .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
        let mut results = Vec::with_capacity(request.plan.members.len());
        for member in &request.plan.members {
            let workflow_id = &member.lowering.document.workflow_id;
            validate_authoring_document(workflow_id, &member.lowering.document)?;
            let current = authoring_one(
                transaction.as_ref(),
                select("workflow_drafts")
                    .columns(&["generation"])
                    .where_eq("workflow_id", DatabaseValue::from(workflow_id))
                    .where_eq("draft_id", DatabaseValue::from("package")),
            )?
            .as_ref()
            .map(|row| database::unsigned(row, "generation"))
            .transpose()?;
            let expected_generation = expected.get(member.member_id.as_str()).copied();
            match (current, expected_generation) {
                (None, None) => {
                    authoring_insert(
                        transaction.as_ref(),
                        &(transaction
                            .as_ref()
                            .insert("authored_workflows")
                            .value("workflow_id", DatabaseValue::from(workflow_id))
                            .value(
                                "title",
                                DatabaseValue::from(
                                    &member.lowering.document.metadata.title.clone(),
                                ),
                            )
                            .value(
                                "description",
                                DatabaseValue::from(
                                    member.lowering.document.metadata.description.clone(),
                                ),
                            )
                            .value("archived", DatabaseValue::Int64(0))
                            .value("active_revision", DatabaseValue::Null)
                            .value("created_at_ms", DatabaseValue::from(applied_at_ms))
                            .value("updated_at_ms", DatabaseValue::from(applied_at_ms))),
                    )?;
                    authoring_insert(
                        transaction.as_ref(),
                        &(transaction
                            .as_ref()
                            .insert("workflow_drafts")
                            .value("workflow_id", DatabaseValue::from(workflow_id))
                            .value("draft_id", DatabaseValue::from("package"))
                            .value("base_revision", DatabaseValue::Null)
                            .value("generation", DatabaseValue::Int64(1))
                            .value(
                                "checksum_sha256",
                                DatabaseValue::from(
                                    member
                                        .lowering
                                        .document
                                        .source_digest_sha256()
                                        .map_err(|error| {
                                            WorkflowStoreError::InvalidData(error.to_string())
                                        })?
                                        .clone(),
                                ),
                            )
                            .value(
                                "document_json",
                                DatabaseValue::from(
                                    bounded_json(
                                        "package draft document",
                                        &member.lowering.document,
                                    )?
                                    .clone(),
                                ),
                            )
                            .value(
                                "producer_json",
                                DatabaseValue::from(
                                    bounded_json("package draft producer", &producer)?.clone(),
                                ),
                            )
                            .value("created_at_ms", DatabaseValue::from(applied_at_ms))
                            .value("updated_at_ms", DatabaseValue::from(applied_at_ms))),
                    )?;
                    results.push(WorkflowPackageMutationMemberResult {
                        member_id: member.member_id.clone(),
                        generation: 1,
                        revision: None,
                        definition_identity: member.definition_identity.clone(),
                    });
                }
                (Some(current_generation), Some(expected_generation))
                    if current_generation == expected_generation =>
                {
                    let next = current_generation.checked_add(1).ok_or_else(|| {
                        WorkflowStoreError::InvalidData(
                            "package draft generation overflow".to_string(),
                        )
                    })?;
                    let changed = block_on(
                        transaction.as_ref().exec_update_count(
                            &(transaction
                                .as_ref()
                                .update("workflow_drafts")
                                .value("generation", DatabaseValue::from(next))
                                .value(
                                    "checksum_sha256",
                                    DatabaseValue::from(
                                        member
                                            .lowering
                                            .document
                                            .source_digest_sha256()
                                            .map_err(|error| {
                                                WorkflowStoreError::InvalidData(error.to_string())
                                            })?
                                            .clone(),
                                    ),
                                )
                                .value(
                                    "document_json",
                                    DatabaseValue::from(
                                        bounded_json(
                                            "package draft document",
                                            &member.lowering.document,
                                        )?
                                        .clone(),
                                    ),
                                )
                                .value(
                                    "producer_json",
                                    DatabaseValue::from(
                                        bounded_json("package draft producer", &producer)?.clone(),
                                    ),
                                )
                                .value("updated_at_ms", DatabaseValue::from(applied_at_ms))
                                .where_eq("workflow_id", DatabaseValue::from(workflow_id))
                                .where_eq("draft_id", DatabaseValue::from("package"))
                                .where_eq("generation", DatabaseValue::from(current_generation))),
                        ),
                    )?;
                    if changed != 1 {
                        return Err(WorkflowStoreError::AuthoringConflict {
                            entity_id: format!("{workflow_id}/package"),
                            expected: expected_generation,
                            current: current_generation,
                        });
                    }
                    results.push(WorkflowPackageMutationMemberResult {
                        member_id: member.member_id.clone(),
                        generation: next,
                        revision: None,
                        definition_identity: member.definition_identity.clone(),
                    });
                }
                (Some(current), expected) => {
                    return Err(WorkflowStoreError::AuthoringConflict {
                        entity_id: format!("{workflow_id}/package"),
                        expected: expected.unwrap_or(0),
                        current,
                    });
                }
                (None, Some(expected)) => {
                    return Err(WorkflowStoreError::AuthoringConflict {
                        entity_id: format!("{workflow_id}/package"),
                        expected,
                        current: 0,
                    });
                }
            }
            fault.after_boundary(
                WorkflowPackageMutationBoundary::MemberStaged,
                Some(&member.member_id),
            )?;
        }
        fault.after_boundary(WorkflowPackageMutationBoundary::AllMembersStaged, None)?;
        results.sort_by(|left, right| left.member_id.cmp(&right.member_id));
        database::commit(transaction)?;
        let result = WorkflowPackageMutationResult {
            version: bcode_workflow::WORKFLOW_PACKAGE_MUTATION_VERSION,
            package_id: request.plan.package_id.clone(),
            outcome: WorkflowPackageMutationOutcome::Applied,
            members: results,
            lock: Some(request.plan.lock.clone()),
            diagnostics: Vec::new(),
        };
        result
            .validate()
            .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
        Ok(result)
    }

    /// Atomically publish every exact package draft generation without activating members.
    ///
    /// All lock, draft, compiled-definition, and generation facts are checked inside one database
    /// transaction. Any mismatch or persistence failure rolls back every member publication.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed package facts, missing/stale drafts, lock drift, archived
    /// workflows, invalid documents, unsupported compilation, or database failure.
    #[allow(clippy::too_many_lines)]
    pub fn publish_workflow_package(
        &mut self,
        request: &WorkflowPackagePublishRequest,
        published_at_ms: u64,
    ) -> Result<WorkflowPackageMutationResult, WorkflowStoreError> {
        self.publish_workflow_package_with_fault(
            request,
            published_at_ms,
            &NoopWorkflowPackageMutationFault,
        )
    }

    /// Publish one package with deterministic in-transaction fault injection.
    ///
    /// # Errors
    ///
    /// Returns any normal publication error or injected fault; no partial revisions or lock facts
    /// are exposed.
    #[allow(clippy::too_many_lines)]
    pub fn publish_workflow_package_with_fault<F>(
        &mut self,
        request: &WorkflowPackagePublishRequest,
        published_at_ms: u64,
        fault: &F,
    ) -> Result<WorkflowPackageMutationResult, WorkflowStoreError>
    where
        F: WorkflowPackageMutationFault + ?Sized,
    {
        request
            .validate()
            .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
        let expected = request
            .expected_generations
            .iter()
            .map(|fact| (fact.member_id.as_str(), fact.expected_generation))
            .collect::<std::collections::BTreeMap<_, _>>();
        let locked = request
            .expected_lock
            .members
            .iter()
            .map(|member| (member.member_id.as_str(), member))
            .collect::<std::collections::BTreeMap<_, _>>();
        let transaction = database::begin(&self.connection, TransactionMode::Deferred)?;
        let mut results = Vec::with_capacity(locked.len());
        let mut published_lock_members = Vec::with_capacity(locked.len());
        for (member_id, lock) in locked {
            let expected_generation = expected[member_id];
            let draft = authoring_one(
                transaction.as_ref(),
                select("workflow_drafts")
                    .columns(&[
                        "workflow_id",
                        "draft_id",
                        "base_revision",
                        "generation",
                        "checksum_sha256",
                        "document_json",
                        "producer_json",
                        "created_at_ms",
                        "updated_at_ms",
                    ])
                    .where_eq(
                        "workflow_id",
                        DatabaseValue::from(&lock.definition_identity.kind),
                    )
                    .where_eq("draft_id", DatabaseValue::from("package")),
            )?
            .as_ref()
            .map(decode_workflow_draft)
            .transpose()?
            .map(validate_stored_workflow_draft)
            .transpose()?
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData(format!(
                    "package member draft is missing: {member_id}"
                ))
            })?;
            if draft.generation != expected_generation {
                return Err(WorkflowStoreError::AuthoringConflict {
                    entity_id: member_id.to_string(),
                    expected: expected_generation,
                    current: draft.generation,
                });
            }
            let source_digest = draft
                .document
                .source_digest_sha256()
                .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
            let executable_digest = draft
                .document
                .executable_source_digest_sha256()
                .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
            let identity = WorkflowDefinitionIdentity::for_definition(
                draft.workflow_id.clone(),
                &draft.document.definition,
            )
            .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
            if source_digest != lock.source_digest_sha256
                || executable_digest != lock.executable_digest_sha256
                || identity != lock.definition_identity
            {
                return Err(WorkflowStoreError::InvalidData(format!(
                    "package lock drift for member '{member_id}'"
                )));
            }
            let workflow = authoring_one(
                transaction.as_ref(),
                select("authored_workflows")
                    .columns(&[
                        "workflow_id",
                        "title",
                        "description",
                        "archived",
                        "active_revision",
                        "created_at_ms",
                        "updated_at_ms",
                    ])
                    .where_eq("workflow_id", DatabaseValue::from(&draft.workflow_id)),
            )?
            .as_ref()
            .map(decode_authored_workflow)
            .transpose()?
            .map(validate_stored_authored_workflow)
            .transpose()?
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData(format!(
                    "authored workflow not found: {}",
                    draft.workflow_id
                ))
            })?;
            if workflow.archived {
                return Err(WorkflowStoreError::InvalidData(format!(
                    "archived package member cannot publish: {member_id}"
                )));
            }
            let revision = next_authoring_revision(transaction.as_ref(), &draft.workflow_id)?;
            let definition_json = bounded_json("compiled definition", &draft.document.definition)?;
            let stored_definition = StoredWorkflowDefinition {
                definition_id: identity.definition_id.clone(),
                version: identity.definition_version,
                checksum_sha256: sha256_hex(definition_json.as_bytes()),
                definition_json,
            };
            persist_definition_transaction(transaction.as_ref(), &stored_definition)?;
            authoring_insert(
                transaction.as_ref(),
                &(transaction
                    .as_ref()
                    .insert("workflow_revisions")
                    .value(
                        "workflow_id",
                        DatabaseValue::from(draft.workflow_id.clone()),
                    )
                    .value("revision", DatabaseValue::from(revision))
                    .value(
                        "source_checksum_sha256",
                        DatabaseValue::from(draft.checksum_sha256.clone()),
                    )
                    .value(
                        "executable_source_checksum_sha256",
                        DatabaseValue::from(&executable_digest),
                    )
                    .value(
                        "definition_id",
                        DatabaseValue::from(stored_definition.definition_id.clone()),
                    )
                    .value(
                        "definition_version",
                        DatabaseValue::from(stored_definition.version),
                    )
                    .value(
                        "document_json",
                        DatabaseValue::from(
                            bounded_json("published document", &draft.document)?.clone(),
                        ),
                    )
                    .value(
                        "producer_json",
                        DatabaseValue::from(
                            bounded_json("published producer", &draft.producer)?.clone(),
                        ),
                    )
                    .value("published_at_ms", DatabaseValue::from(published_at_ms))),
            )?;
            block_on(
                transaction.as_ref().exec_update_count(
                    &(transaction
                        .as_ref()
                        .update("authored_workflows")
                        .value("updated_at_ms", DatabaseValue::from(published_at_ms))
                        .where_eq("workflow_id", DatabaseValue::from(&draft.workflow_id))),
                ),
            )?;
            authoring_insert(
                transaction.as_ref(),
                &(transaction
                    .as_ref()
                    .insert("workflow_authoring_events")
                    .value("workflow_id", DatabaseValue::from(&draft.workflow_id))
                    .value("event_type", DatabaseValue::from("revision_published"))
                    .value(
                        "payload_json",
                        DatabaseValue::from(
                            bounded_json(
                                "package publication event",
                                &serde_json::json!({
                                    "revision": revision,
                                    "definition_id": identity.definition_id,
                                    "definition_version": identity.definition_version,
                                    "activated": false,
                                    "package_id": request.package_id,
                                    "member_id": member_id,
                                }),
                            )?
                            .clone(),
                        ),
                    )
                    .value("created_at_ms", DatabaseValue::from(published_at_ms))),
            )?;
            block_on(
                transaction.as_ref().exec_delete_count(
                    &(transaction
                        .as_ref()
                        .delete("workflow_drafts")
                        .where_eq("workflow_id", DatabaseValue::from(&draft.workflow_id))
                        .where_eq("draft_id", DatabaseValue::from("package"))
                        .where_eq("generation", DatabaseValue::from(expected_generation))),
                ),
            )?;
            let revision_identity = bcode_workflow::WorkflowRevisionIdentity {
                workflow_id: draft.workflow_id.clone(),
                revision,
            };
            results.push(WorkflowPackageMutationMemberResult {
                member_id: member_id.to_string(),
                generation: expected_generation,
                revision: Some(revision_identity.clone()),
                definition_identity: lock.definition_identity.clone(),
            });
            let mut published = lock.clone();
            published.published_revision = Some(revision_identity);
            published_lock_members.push(published);
            fault.after_boundary(
                WorkflowPackageMutationBoundary::MemberStaged,
                Some(member_id),
            )?;
        }
        fault.after_boundary(WorkflowPackageMutationBoundary::AllMembersStaged, None)?;
        results.sort_by(|left, right| left.member_id.cmp(&right.member_id));
        published_lock_members.sort_by(|left, right| left.member_id.cmp(&right.member_id));
        let mut exports = request.expected_lock.exports.clone();
        for export in &mut exports {
            let member = published_lock_members
                .iter()
                .find(|member| member.member_id == export.member_id)
                .ok_or_else(|| {
                    WorkflowStoreError::InvalidData(
                        "package export references absent published member".to_string(),
                    )
                })?;
            export.definition_identity = member.definition_identity.clone();
            export.published_revision = member.published_revision.clone();
        }
        exports.sort_by(|left, right| left.export.cmp(&right.export));
        let published_lock = WorkflowPackageLock {
            version: request.expected_lock.version,
            package_id: request.expected_lock.package_id.clone(),
            package_source_digest_sha256: request
                .expected_lock
                .package_source_digest_sha256
                .clone(),
            imports: request.expected_lock.imports.clone(),
            exports,
            members: published_lock_members,
        };
        let receipt = bcode_workflow::WorkflowPackagePublicationReceipt {
            version: bcode_workflow::WORKFLOW_PACKAGE_PUBLICATION_RECEIPT_VERSION,
            package_id: request.package_id.clone(),
            package_lock_digest_sha256: sha256_hex(
                bounded_json("published package lock", &published_lock)?.as_bytes(),
            ),
            published_at_ms,
            exports: published_lock.exports.clone(),
        };
        receipt
            .validate()
            .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
        authoring_insert(
            transaction.as_ref(),
            &(transaction
                .as_ref()
                .insert("workflow_package_publications")
                .value("package_id", DatabaseValue::from(&receipt.package_id))
                .value(
                    "package_lock_digest_sha256",
                    DatabaseValue::from(&receipt.package_lock_digest_sha256),
                )
                .value(
                    "lock_json",
                    DatabaseValue::from(bounded_json("published package lock", &published_lock)?),
                )
                .value(
                    "receipt_json",
                    DatabaseValue::from(bounded_json("package publication receipt", &receipt)?),
                )
                .value("published_at_ms", DatabaseValue::from(published_at_ms))),
        )?;
        database::commit(transaction)?;
        let result = WorkflowPackageMutationResult {
            version: bcode_workflow::WORKFLOW_PACKAGE_MUTATION_VERSION,
            package_id: request.package_id.clone(),
            outcome: WorkflowPackageMutationOutcome::Published,
            members: results,
            lock: Some(published_lock),
            diagnostics: Vec::new(),
        };
        result
            .validate()
            .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
        Ok(result)
    }

    /// Capture a process-local publication read fence without retaining database locks.
    ///
    /// # Errors
    /// Returns a database error if the connection change counter cannot be read.
    pub fn publication_read_fence(
        &self,
    ) -> Result<WorkflowPublicationReadFence, WorkflowStoreError> {
        let observation = block_on(self.connection.sqlite_change_observation())?;
        Ok(WorkflowPublicationReadFence {
            identity: self.read_identity.clone(),
            data_version: i64::try_from(observation.data_version).map_err(|_| {
                WorkflowStoreError::InvalidData(
                    "workflow data version exceeds storage range".into(),
                )
            })?,
            total_changes: observation.total_changes,
        })
    }

    /// Reject a read fence after any local write, external commit, or store replacement.
    /// This deliberately invalidates on unrelated writes rather than guessing consistency.
    ///
    /// # Errors
    /// Returns an error if the fence is stale or the database cannot be queried.
    pub fn validate_publication_read_fence(
        &self,
        fence: &WorkflowPublicationReadFence,
    ) -> Result<(), WorkflowStoreError> {
        let current = self.publication_read_fence()?;
        if !std::sync::Arc::ptr_eq(&current.identity, &fence.identity)
            || current.data_version != fence.data_version
            || current.total_changes != fence.total_changes
        {
            return Err(WorkflowStoreError::InvalidData(
                "workflow discovery state changed; restart discovery".into(),
            ));
        }
        Ok(())
    }

    /// Read an exact immutable published package lock, verifying its stored digest.
    ///
    /// No latest-publication fallback is permitted. The encoded lock is bounded
    /// before allocation; it remains package-owned binding data, not run authority.
    ///
    /// # Errors
    /// Returns an error for malformed input, oversized/corrupt lock data, unsupported
    /// lock versions, or storage failures.
    pub fn workflow_package_lock(
        &self,
        package_id: &str,
        digest: &str,
    ) -> Result<Option<WorkflowPackageLock>, WorkflowStoreError> {
        Self::package_lock_on(&self.connection, package_id, digest)
    }

    /// Resolve an exact export from the immutable package bound to a run.
    ///
    /// # Errors
    /// Rejects unbound runs, mismatched package/digest, missing exports, or corrupt publication.
    pub fn resolve_run_package_export(
        &self,
        run_id: &str,
        selection: &bcode_workflow::WorkflowPackageExportIdentity,
    ) -> Result<bcode_workflow::AuthoredWorkflowRunSelection, WorkflowStoreError> {
        validate_id("run", run_id)?;
        selection
            .validate()
            .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
        let row = authoring_required(
            &self.connection,
            select("workflow_run_packages")
                .columns(&["package_id", "lock_digest"])
                .where_eq("run_id", run_id),
        )?;
        let id = database::text(&row, "package_id")?;
        let digest = database::text(&row, "lock_digest")?;
        if selection.package_id != id
            || selection
                .package_lock_digest_sha256
                .as_ref()
                .is_some_and(|requested| requested != &digest)
        {
            return Err(WorkflowStoreError::InvalidData(
                "replacement export does not match pinned package".into(),
            ));
        }
        let lock = Self::package_lock_on(&self.connection, &id, &digest)?
            .ok_or_else(|| WorkflowStoreError::InvalidData("pinned package unavailable".into()))?;
        let revision = lock
            .exports
            .iter()
            .find(|export| export.export == selection.export)
            .and_then(|export| export.published_revision.as_ref())
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData("pinned export has no published revision".into())
            })?;
        Ok(bcode_workflow::AuthoredWorkflowRunSelection::Revision {
            workflow_id: revision.workflow_id.clone(),
            revision: revision.revision,
        })
    }

    /// Resolve a package-local member only through a run's immutable publication binding.
    ///
    /// # Errors
    /// Rejects unbound runs, missing members/publications, corrupt or unsupported locks,
    /// malformed identities, and storage failures. Never selects a newer publication.
    pub fn resolve_run_package_member(
        &self,
        run_id: &str,
        member_id: &str,
    ) -> Result<bcode_workflow::WorkflowDefinitionIdentity, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        validate_id("member_id", member_id)?;
        let binding: (String, String) = authoring_one(
            &self.connection,
            select("workflow_run_packages")
                .columns(&["package_id", "lock_digest"])
                .where_eq("run_id", DatabaseValue::from(run_id)),
        )?
        .as_ref()
        .map(|row| {
            Ok::<_, WorkflowStoreError>((
                database::text(row, "package_id")?,
                database::text(row, "lock_digest")?,
            ))
        })
        .transpose()?
        .ok_or_else(|| {
            WorkflowStoreError::InvalidData("package member resolution requires a bound run".into())
        })?;
        let lock = self
            .workflow_package_lock(&binding.0, &binding.1)?
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData("bound package publication is unavailable".into())
            })?;
        lock.members
            .into_iter()
            .find(|member| member.member_id == member_id)
            .map(|member| member.definition_identity)
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData("bound package member is unavailable".into())
            })
    }

    pub(super) fn package_lock_on(
        connection: &dyn Database,
        package_id: &str,
        digest: &str,
    ) -> Result<Option<WorkflowPackageLock>, WorkflowStoreError> {
        validate_id("package_id", package_id)?;
        validate_id("package_lock_digest_sha256", digest)?;
        let row = authoring_one(
            connection,
            select("workflow_package_publications")
                .projections(vec![
                    Projection::new(byte_length(identifier("lock_json"))).alias("bytes"),
                    Projection::new(bounded_text(
                        identifier("lock_json"),
                        MAX_INLINE_JSON_BYTES as u64,
                        DatabaseValue::Null,
                    ))
                    .alias("lock_json"),
                ])
                .where_eq("package_id", package_id)
                .where_eq("package_lock_digest_sha256", digest),
        )?;
        let Some(row) = row else {
            return Ok(None);
        };
        if database::unsigned(&row, "bytes")? > MAX_INLINE_JSON_BYTES as u64 {
            return Err(WorkflowStoreError::InvalidData(
                "published package lock exceeds byte budget".into(),
            ));
        }
        let json = database::text(&row, "lock_json")?;
        if sha256_hex(json.as_bytes()) != digest {
            return Err(WorkflowStoreError::InvalidData(
                "published package lock checksum mismatch".into(),
            ));
        }
        let lock: WorkflowPackageLock = serde_json::from_str(&json)?;
        lock.validate()
            .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
        if lock.package_id != package_id {
            return Err(WorkflowStoreError::InvalidData(
                "published package lock identity mismatch".into(),
            ));
        }
        Ok(Some(lock))
    }

    /// Read one exact or latest bounded package publication receipt.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity/digest input, corrupt durable receipt state, or a
    /// database query failure.
    pub fn workflow_package_publication(
        &self,
        package_id: &str,
        package_lock_digest_sha256: Option<&str>,
    ) -> Result<Option<bcode_workflow::WorkflowPackagePublicationReceipt>, WorkflowStoreError> {
        validate_id("package_id", package_id)?;
        if let Some(digest) = package_lock_digest_sha256 {
            validate_id("package_lock_digest_sha256", digest)?;
        }
        let mut query = select("workflow_package_publications")
            .columns(&["receipt_json"])
            .where_eq("package_id", package_id)
            .sort("published_at_ms", SortDirection::Desc)
            .limit(1);
        if let Some(digest) = package_lock_digest_sha256 {
            query = query.where_eq("package_lock_digest_sha256", digest);
        }
        authoring_one(&self.connection, query)?
            .map(|row| {
                let result: bcode_workflow::WorkflowPackagePublicationReceipt =
                    serde_json::from_str(&database::text(&row, "receipt_json")?)?;
                result
                    .validate()
                    .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
                Ok(result)
            })
            .transpose()
    }

    /// Create one durable mutable draft.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed draft state, missing workflow/base revision, identity
    /// conflict, or database failure.
    pub fn create_workflow_draft(
        &mut self,
        draft: &WorkflowDraft,
    ) -> Result<bool, WorkflowStoreError> {
        validate_workflow_draft(draft)?;
        if draft.generation != 1 {
            return Err(WorkflowStoreError::InvalidData(
                "new workflow draft generation must be 1".to_string(),
            ));
        }
        let document_json = bounded_json("draft document", &draft.document)?;
        let producer_json = bounded_json("draft producer", &draft.producer)?;
        let changed = block_on(
            (self
                .connection
                .insert("workflow_drafts")
                .value("workflow_id", DatabaseValue::from(&draft.workflow_id))
                .value("draft_id", DatabaseValue::from(&draft.draft_id))
                .value("base_revision", DatabaseValue::from(draft.base_revision))
                .value("generation", DatabaseValue::from(draft.generation))
                .value(
                    "checksum_sha256",
                    DatabaseValue::from(&draft.checksum_sha256),
                )
                .value("document_json", DatabaseValue::from(&document_json))
                .value("producer_json", DatabaseValue::from(&producer_json))
                .value("created_at_ms", DatabaseValue::from(draft.created_at_ms))
                .value("updated_at_ms", DatabaseValue::from(draft.updated_at_ms)))
            .or_ignore()
            .execute_count(&self.connection),
        )?;
        if changed == 1 {
            return Ok(true);
        }
        let existing = self
            .workflow_draft(&draft.workflow_id, &draft.draft_id)?
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData("draft insert was not observable".to_string())
            })?;
        if existing == *draft {
            Ok(false)
        } else {
            Err(WorkflowStoreError::InvalidData(format!(
                "workflow draft identity conflict: {}/{}",
                draft.workflow_id, draft.draft_id
            )))
        }
    }

    /// Load one exact mutable draft without mutation.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, checksum/version inconsistency, or query failure.
    pub fn workflow_draft(
        &self,
        workflow_id: &str,
        draft_id: &str,
    ) -> Result<Option<WorkflowDraft>, WorkflowStoreError> {
        validate_id("workflow_id", workflow_id)?;
        validate_id("draft_id", draft_id)?;
        authoring_one(
            &self.connection,
            select("workflow_drafts")
                .columns(&[
                    "workflow_id",
                    "draft_id",
                    "base_revision",
                    "generation",
                    "checksum_sha256",
                    "document_json",
                    "producer_json",
                    "created_at_ms",
                    "updated_at_ms",
                ])
                .where_eq("workflow_id", DatabaseValue::from(workflow_id))
                .where_eq("draft_id", DatabaseValue::from(draft_id)),
        )?
        .as_ref()
        .map(decode_workflow_draft)
        .transpose()?
        .map(validate_stored_workflow_draft)
        .transpose()
    }

    /// Return a bounded draft list for one logical workflow.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid identity/bound, inconsistent data, or query failure.
    pub fn list_workflow_drafts(
        &self,
        workflow_id: &str,
        limit: usize,
    ) -> Result<Vec<WorkflowDraft>, WorkflowStoreError> {
        Ok(self
            .list_workflow_drafts_page(workflow_id, None, limit)?
            .items)
    }

    /// Return one stable keyset page of drafts for a logical workflow.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid identity/cursor/bound, inconsistent data, or query failure.
    pub fn list_workflow_drafts_page(
        &self,
        workflow_id: &str,
        cursor: Option<&WorkflowAuthoringListCursor>,
        limit: usize,
    ) -> Result<WorkflowAuthoringStorePage<WorkflowDraft>, WorkflowStoreError> {
        validate_id("workflow_id", workflow_id)?;
        if let Some(cursor) = cursor {
            cursor
                .validate()
                .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
        }
        let query_limit = authoring_limit(authoring_page_query_limit(limit)?)?;
        let query = authoring_cursor(
            select("workflow_drafts")
                .columns(DRAFT_COLUMNS)
                .where_eq("workflow_id", workflow_id),
            cursor,
            "draft_id",
        )?
        .sort("updated_at_ms", SortDirection::Desc)
        .sort("draft_id", SortDirection::Asc)
        .limit(query_limit);
        authoring_rows(&self.connection, query)?
            .iter()
            .map(decode_workflow_draft)
            .map(|row| row.and_then(validate_stored_workflow_draft))
            .collect::<Result<Vec<_>, _>>()
            .map(|items| finish_authoring_store_page(items, limit))
    }

    /// Replace one draft using an exact expected generation.
    ///
    /// # Errors
    ///
    /// Returns an error for stale generation, malformed source, timestamp regression, or failure.
    pub fn update_workflow_draft(
        &mut self,
        workflow_id: &str,
        draft_id: &str,
        expected_generation: u64,
        document: &WorkflowAuthoringDocument,
        producer: &WorkflowProducerProvenance,
        updated_at_ms: u64,
    ) -> Result<WorkflowDraft, WorkflowStoreError> {
        self.update_workflow_draft_with_fault(
            workflow_id,
            draft_id,
            expected_generation,
            document,
            producer,
            updated_at_ms,
            &NoopWorkflowAuthoringMutationFault,
        )
    }

    /// Replace one draft with deterministic fault observation before commit.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::update_workflow_draft`] plus injected fault errors.
    #[allow(clippy::too_many_arguments)]
    pub fn update_workflow_draft_with_fault<F: WorkflowAuthoringMutationFault + ?Sized>(
        &mut self,
        workflow_id: &str,
        draft_id: &str,
        expected_generation: u64,
        document: &WorkflowAuthoringDocument,
        producer: &WorkflowProducerProvenance,
        updated_at_ms: u64,
        fault: &F,
    ) -> Result<WorkflowDraft, WorkflowStoreError> {
        validate_id("workflow_id", workflow_id)?;
        validate_id("draft_id", draft_id)?;
        validate_authoring_document(workflow_id, document)?;
        producer
            .validate()
            .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
        let transaction = database::begin(&self.connection, TransactionMode::Deferred)?;
        let current = authoring_one(
            transaction.as_ref(),
            select("workflow_drafts")
                .columns(&[
                    "workflow_id",
                    "draft_id",
                    "base_revision",
                    "generation",
                    "checksum_sha256",
                    "document_json",
                    "producer_json",
                    "created_at_ms",
                    "updated_at_ms",
                ])
                .where_eq("workflow_id", DatabaseValue::from(workflow_id))
                .where_eq("draft_id", DatabaseValue::from(draft_id)),
        )?
        .as_ref()
        .map(decode_workflow_draft)
        .transpose()?
        .map(validate_stored_workflow_draft)
        .transpose()?
        .ok_or_else(|| {
            WorkflowStoreError::InvalidData(format!(
                "workflow draft not found: {workflow_id}/{draft_id}"
            ))
        })?;
        if current.generation != expected_generation {
            return Err(WorkflowStoreError::AuthoringConflict {
                entity_id: format!("{workflow_id}/{draft_id}"),
                expected: expected_generation,
                current: current.generation,
            });
        }
        if updated_at_ms < current.updated_at_ms {
            return Err(WorkflowStoreError::InvalidData(
                "draft update timestamp regressed".to_string(),
            ));
        }
        let generation = expected_generation.checked_add(1).ok_or_else(|| {
            WorkflowStoreError::InvalidData("draft generation overflow".to_string())
        })?;
        let checksum_sha256 = document
            .source_digest_sha256()
            .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
        let document_json = bounded_json("draft document", document)?;
        let producer_json = bounded_json("draft producer", producer)?;
        let changed = block_on(
            transaction.as_ref().exec_update_count(
                &(transaction
                    .as_ref()
                    .update("workflow_drafts")
                    .value("generation", DatabaseValue::from(generation))
                    .value("checksum_sha256", DatabaseValue::from(&checksum_sha256))
                    .value("document_json", DatabaseValue::from(&document_json))
                    .value("producer_json", DatabaseValue::from(&producer_json))
                    .value("updated_at_ms", DatabaseValue::from(updated_at_ms))
                    .where_eq("workflow_id", DatabaseValue::from(workflow_id))
                    .where_eq("draft_id", DatabaseValue::from(draft_id))
                    .where_eq("generation", DatabaseValue::from(expected_generation))),
            ),
        )?;
        if changed == 0 {
            return Err(WorkflowStoreError::AuthoringConflict {
                entity_id: format!("{workflow_id}/{draft_id}"),
                expected: expected_generation,
                current: current.generation,
            });
        }
        fault.after_boundary(WorkflowAuthoringMutationBoundary::DraftUpdated)?;
        database::commit(transaction)?;
        Ok(WorkflowDraft {
            workflow_id: workflow_id.to_string(),
            draft_id: draft_id.to_string(),
            base_revision: current.base_revision,
            generation,
            checksum_sha256,
            document: document.clone(),
            producer: producer.clone(),
            created_at_ms: current.created_at_ms,
            updated_at_ms,
        })
    }

    /// Discard one mutable draft using exact optimistic generation.
    ///
    /// # Errors
    ///
    /// Returns an error when the draft is missing, stale, or deletion fails.
    pub fn discard_workflow_draft(
        &mut self,
        workflow_id: &str,
        draft_id: &str,
        expected_generation: u64,
    ) -> Result<(), WorkflowStoreError> {
        validate_id("workflow_id", workflow_id)?;
        validate_id("draft_id", draft_id)?;
        let changed = block_on(
            self.connection.exec_delete_count(
                &(self
                    .connection
                    .delete("workflow_drafts")
                    .where_eq("workflow_id", DatabaseValue::from(workflow_id))
                    .where_eq("draft_id", DatabaseValue::from(draft_id))
                    .where_eq("generation", DatabaseValue::from(expected_generation))),
            ),
        )?;
        if changed == 0 {
            let current = self
                .workflow_draft(workflow_id, draft_id)?
                .map_or(0, |draft| draft.generation);
            return Err(WorkflowStoreError::AuthoringConflict {
                entity_id: format!("{workflow_id}/{draft_id}"),
                expected: expected_generation,
                current,
            });
        }
        Ok(())
    }

    /// Fork one published revision into a new mutable generation-1 draft.
    ///
    /// # Errors
    ///
    /// Returns an error when the revision is missing, the requested draft is invalid, or creation
    /// conflicts.
    pub fn fork_workflow_revision(
        &mut self,
        workflow_id: &str,
        revision: u64,
        draft_id: &str,
        producer: WorkflowProducerProvenance,
        created_at_ms: u64,
    ) -> Result<WorkflowDraft, WorkflowStoreError> {
        let published = self
            .workflow_revision(workflow_id, revision)?
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData(format!(
                    "published workflow revision not found: {workflow_id} v{revision}"
                ))
            })?;
        let draft = WorkflowDraft {
            workflow_id: workflow_id.to_string(),
            draft_id: draft_id.to_string(),
            base_revision: Some(revision),
            generation: 1,
            checksum_sha256: published.source_checksum_sha256,
            document: published.document,
            producer,
            created_at_ms,
            updated_at_ms: created_at_ms,
        };
        self.create_workflow_draft(&draft)?;
        Ok(draft)
    }

    /// Fork one mutable draft into another independent generation-1 draft.
    ///
    /// # Errors
    ///
    /// Returns an error when the source draft is missing, the destination conflicts, or stored
    /// source state is invalid.
    pub fn fork_workflow_draft(
        &mut self,
        workflow_id: &str,
        source_draft_id: &str,
        draft_id: &str,
        producer: WorkflowProducerProvenance,
        created_at_ms: u64,
    ) -> Result<WorkflowDraft, WorkflowStoreError> {
        let source = self
            .workflow_draft(workflow_id, source_draft_id)?
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData(format!(
                    "workflow draft not found: {workflow_id}/{source_draft_id}"
                ))
            })?;
        let draft = WorkflowDraft {
            workflow_id: workflow_id.to_string(),
            draft_id: draft_id.to_string(),
            base_revision: source.base_revision,
            generation: 1,
            checksum_sha256: source.checksum_sha256,
            document: source.document,
            producer,
            created_at_ms,
            updated_at_ms: created_at_ms,
        };
        self.create_workflow_draft(&draft)?;
        Ok(draft)
    }

    /// Publish one exact draft generation atomically, optionally activating the new revision.
    ///
    /// # Errors
    ///
    /// Returns an error for stale generation/active pointer, archived workflow, mismatched or
    /// unsuccessful preview, definition identity mismatch, serialization failure, or database
    /// failure. Any error rolls back the complete publication.
    #[allow(clippy::too_many_arguments)]
    pub fn publish_workflow_draft(
        &mut self,
        workflow_id: &str,
        draft_id: &str,
        expected_generation: u64,
        preview: &WorkflowCompilationPreview,
        activate: bool,
        expected_active_revision: Option<u64>,
        published_at_ms: u64,
    ) -> Result<WorkflowPublication, WorkflowStoreError> {
        self.publish_workflow_draft_with_fault(
            workflow_id,
            draft_id,
            expected_generation,
            preview,
            activate,
            expected_active_revision,
            published_at_ms,
            &NoopWorkflowPublicationFault,
        )
    }

    /// Publish with a deterministic uncommitted-boundary fault hook.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::publish_workflow_draft`] plus injected fault errors.
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub fn publish_workflow_draft_with_fault<F: WorkflowPublicationFault + ?Sized>(
        &mut self,
        workflow_id: &str,
        draft_id: &str,
        expected_generation: u64,
        preview: &WorkflowCompilationPreview,
        activate: bool,
        expected_active_revision: Option<u64>,
        published_at_ms: u64,
        fault: &F,
    ) -> Result<WorkflowPublication, WorkflowStoreError> {
        validate_id("workflow_id", workflow_id)?;
        validate_id("draft_id", draft_id)?;
        if preview.version != WORKFLOW_COMPILATION_PREVIEW_VERSION || !preview.is_compiled() {
            return Err(WorkflowStoreError::InvalidData(
                "publication requires a successful current-version compilation preview".to_string(),
            ));
        }
        let compiled = preview.compiled.as_ref().ok_or_else(|| {
            WorkflowStoreError::InvalidData(
                "successful compilation preview is missing compiled details".to_string(),
            )
        })?;
        let transaction = database::begin(&self.connection, TransactionMode::Deferred)?;
        let draft = authoring_one(
            transaction.as_ref(),
            select("workflow_drafts")
                .columns(&[
                    "workflow_id",
                    "draft_id",
                    "base_revision",
                    "generation",
                    "checksum_sha256",
                    "document_json",
                    "producer_json",
                    "created_at_ms",
                    "updated_at_ms",
                ])
                .where_eq("workflow_id", DatabaseValue::from(workflow_id))
                .where_eq("draft_id", DatabaseValue::from(draft_id)),
        )?
        .as_ref()
        .map(decode_workflow_draft)
        .transpose()?
        .map(validate_stored_workflow_draft)
        .transpose()?
        .ok_or_else(|| {
            WorkflowStoreError::InvalidData(format!(
                "workflow draft not found: {workflow_id}/{draft_id}"
            ))
        })?;
        if draft.generation != expected_generation {
            return Err(WorkflowStoreError::AuthoringConflict {
                entity_id: format!("{workflow_id}/{draft_id}"),
                expected: expected_generation,
                current: draft.generation,
            });
        }
        if preview.validation.source_digest_sha256.as_deref()
            != Some(draft.checksum_sha256.as_str())
        {
            return Err(WorkflowStoreError::InvalidData(
                "compilation preview source digest does not match the durable draft".to_string(),
            ));
        }
        let workflow = authoring_one(
            transaction.as_ref(),
            select("authored_workflows")
                .columns(&[
                    "workflow_id",
                    "title",
                    "description",
                    "archived",
                    "active_revision",
                    "created_at_ms",
                    "updated_at_ms",
                ])
                .where_eq("workflow_id", DatabaseValue::from(workflow_id)),
        )?
        .as_ref()
        .map(decode_authored_workflow)
        .transpose()?
        .map(validate_stored_authored_workflow)
        .transpose()?
        .ok_or_else(|| {
            WorkflowStoreError::InvalidData(format!("authored workflow not found: {workflow_id}"))
        })?;
        if workflow.archived {
            return Err(WorkflowStoreError::InvalidData(
                "archived authored workflows cannot publish new revisions".to_string(),
            ));
        }
        if activate && workflow.active_revision != expected_active_revision {
            return Err(WorkflowStoreError::AuthoringConflict {
                entity_id: workflow_id.to_string(),
                expected: expected_active_revision.unwrap_or(0),
                current: workflow.active_revision.unwrap_or(0),
            });
        }
        let revision = next_authoring_revision(transaction.as_ref(), workflow_id)?;
        let expected_identity = WorkflowDefinitionIdentity::for_definition(
            workflow_id.to_string(),
            &compiled.definition,
        )
        .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
        if expected_identity != compiled.definition_identity {
            return Err(WorkflowStoreError::InvalidData(
                "compilation preview definition identity does not match its exact definition"
                    .to_string(),
            ));
        }
        let definition_json = bounded_json("compiled definition", &compiled.definition)?;
        let stored_definition = StoredWorkflowDefinition {
            definition_id: compiled.definition_identity.definition_id.clone(),
            version: compiled.definition_identity.definition_version,
            checksum_sha256: sha256_hex(definition_json.as_bytes()),
            definition_json,
        };
        persist_definition_transaction(transaction.as_ref(), &stored_definition)?;
        fault.after_boundary(WorkflowPublicationBoundary::DefinitionPersisted)?;
        let document_json = bounded_json("published document", &draft.document)?;
        let producer_json = bounded_json("published producer", &draft.producer)?;
        let executable_source_checksum_sha256 = draft
            .document
            .executable_source_digest_sha256()
            .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
        authoring_insert(
            transaction.as_ref(),
            &(transaction
                .as_ref()
                .insert("workflow_revisions")
                .value("workflow_id", DatabaseValue::from(workflow_id))
                .value("revision", DatabaseValue::from(revision))
                .value(
                    "source_checksum_sha256",
                    DatabaseValue::from(draft.checksum_sha256.clone()),
                )
                .value(
                    "executable_source_checksum_sha256",
                    DatabaseValue::from(&executable_source_checksum_sha256),
                )
                .value(
                    "definition_id",
                    DatabaseValue::from(stored_definition.definition_id.clone()),
                )
                .value(
                    "definition_version",
                    DatabaseValue::from(stored_definition.version),
                )
                .value("document_json", DatabaseValue::from(&document_json))
                .value("producer_json", DatabaseValue::from(&producer_json))
                .value("published_at_ms", DatabaseValue::from(published_at_ms))),
        )?;
        fault.after_boundary(WorkflowPublicationBoundary::RevisionPersisted)?;
        let active_revision = if activate {
            let changed = block_on(
                transaction.as_ref().exec_update_count(
                    &(transaction
                        .as_ref()
                        .update("authored_workflows")
                        .value("active_revision", DatabaseValue::from(revision))
                        .value("updated_at_ms", DatabaseValue::from(published_at_ms))
                        .where_eq("workflow_id", DatabaseValue::from(workflow_id))
                        .filter(Box::new(authoring_is(
                            "active_revision",
                            &(DatabaseValue::from(expected_active_revision)),
                        )?))),
                ),
            )?;
            if changed != 1 {
                return Err(WorkflowStoreError::AuthoringConflict {
                    entity_id: workflow_id.to_string(),
                    expected: expected_active_revision.unwrap_or(0),
                    current: workflow.active_revision.unwrap_or(0),
                });
            }
            Some(revision)
        } else {
            block_on(
                transaction.as_ref().exec_update_count(
                    &(transaction
                        .as_ref()
                        .update("authored_workflows")
                        .value("updated_at_ms", DatabaseValue::from(published_at_ms))
                        .where_eq("workflow_id", DatabaseValue::from(workflow_id))),
                ),
            )?;
            workflow.active_revision
        };
        fault.after_boundary(WorkflowPublicationBoundary::ActiveRevisionUpdated)?;
        let event_payload = bounded_json(
            "publication event",
            &serde_json::json!({
                "revision": revision,
                "definition_id": stored_definition.definition_id,
                "definition_version": stored_definition.version,
                "activated": activate
            }),
        )?;
        authoring_insert(
            transaction.as_ref(),
            &(transaction
                .as_ref()
                .insert("workflow_authoring_events")
                .value("workflow_id", DatabaseValue::from(workflow_id))
                .value("event_type", DatabaseValue::from("revision_published"))
                .value("payload_json", DatabaseValue::from(event_payload))
                .value("created_at_ms", DatabaseValue::from(published_at_ms))),
        )?;
        fault.after_boundary(WorkflowPublicationBoundary::EventPersisted)?;
        block_on(
            transaction.as_ref().exec_delete_count(
                &(transaction
                    .as_ref()
                    .delete("workflow_drafts")
                    .where_eq("workflow_id", DatabaseValue::from(workflow_id))
                    .where_eq("draft_id", DatabaseValue::from(draft_id))
                    .where_eq("generation", DatabaseValue::from(expected_generation))),
            ),
        )?;
        database::commit(transaction)?;
        Ok(WorkflowPublication {
            revision: PublishedWorkflowRevision {
                workflow_id: workflow_id.to_string(),
                revision,
                source_checksum_sha256: draft.checksum_sha256,
                executable_source_checksum_sha256,
                definition_identity: compiled.definition_identity.clone(),
                document: draft.document,
                producer: draft.producer,
                published_at_ms,
            },
            active_revision,
        })
    }

    /// Import validated portable source as the exact next immutable revision atomically.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed content, archived/missing workflow, non-next revision,
    /// stale active pointer, unsuccessful preview, or persistence failure.
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub fn import_workflow_revision(
        &mut self,
        workflow_id: &str,
        revision: u64,
        document: &WorkflowAuthoringDocument,
        producer: &WorkflowProducerProvenance,
        preview: &WorkflowCompilationPreview,
        activate: bool,
        expected_active_revision: Option<u64>,
        published_at_ms: u64,
    ) -> Result<WorkflowPublication, WorkflowStoreError> {
        validate_id("workflow_id", workflow_id)?;
        validate_authoring_document(workflow_id, document)?;
        producer
            .validate()
            .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
        if preview.version != WORKFLOW_COMPILATION_PREVIEW_VERSION || !preview.is_compiled() {
            return Err(WorkflowStoreError::InvalidData(
                "revision import requires a successful current-version compilation preview"
                    .to_string(),
            ));
        }
        let compiled = preview.compiled.as_ref().ok_or_else(|| {
            WorkflowStoreError::InvalidData(
                "successful compilation preview is missing compiled details".to_string(),
            )
        })?;
        let source_checksum_sha256 = document
            .source_digest_sha256()
            .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
        if preview.validation.source_digest_sha256.as_deref()
            != Some(source_checksum_sha256.as_str())
        {
            return Err(WorkflowStoreError::InvalidData(
                "compilation preview source digest does not match imported source".to_string(),
            ));
        }
        let expected_identity = WorkflowDefinitionIdentity::for_definition(
            workflow_id.to_string(),
            &compiled.definition,
        )
        .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
        if expected_identity != compiled.definition_identity {
            return Err(WorkflowStoreError::InvalidData(
                "imported compilation definition identity is inconsistent".to_string(),
            ));
        }
        let definition_json = bounded_json("compiled definition", &compiled.definition)?;
        let document_json = bounded_json("published document", document)?;
        let producer_json = bounded_json("published producer", producer)?;
        let executable_source_checksum_sha256 = document
            .executable_source_digest_sha256()
            .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
        let transaction = database::begin(&self.connection, TransactionMode::Deferred)?;
        let workflow = authoring_one(
            transaction.as_ref(),
            select("authored_workflows")
                .columns(&[
                    "workflow_id",
                    "title",
                    "description",
                    "archived",
                    "active_revision",
                    "created_at_ms",
                    "updated_at_ms",
                ])
                .where_eq("workflow_id", DatabaseValue::from(workflow_id)),
        )?
        .as_ref()
        .map(decode_authored_workflow)
        .transpose()?
        .map(validate_stored_authored_workflow)
        .transpose()?
        .ok_or_else(|| {
            WorkflowStoreError::InvalidData(format!("authored workflow not found: {workflow_id}"))
        })?;
        if workflow.archived {
            return Err(WorkflowStoreError::InvalidData(
                "archived authored workflows cannot import revisions".to_string(),
            ));
        }
        if activate && workflow.active_revision != expected_active_revision {
            return Err(WorkflowStoreError::AuthoringConflict {
                entity_id: workflow_id.to_string(),
                expected: expected_active_revision.unwrap_or(0),
                current: workflow.active_revision.unwrap_or(0),
            });
        }
        let current_revision = latest_authoring_revision(transaction.as_ref(), workflow_id)?;
        let next_revision = current_revision.checked_add(1).ok_or_else(|| {
            WorkflowStoreError::InvalidData("workflow revision overflow".to_string())
        })?;
        if revision != next_revision {
            return Err(WorkflowStoreError::AuthoringConflict {
                entity_id: workflow_id.to_string(),
                expected: revision,
                current: next_revision,
            });
        }
        let stored_definition = StoredWorkflowDefinition {
            definition_id: compiled.definition_identity.definition_id.clone(),
            version: compiled.definition_identity.definition_version,
            checksum_sha256: sha256_hex(definition_json.as_bytes()),
            definition_json,
        };
        persist_definition_transaction(transaction.as_ref(), &stored_definition)?;
        authoring_insert(
            transaction.as_ref(),
            &(transaction
                .as_ref()
                .insert("workflow_revisions")
                .value("workflow_id", DatabaseValue::from(workflow_id))
                .value("revision", DatabaseValue::from(revision))
                .value(
                    "source_checksum_sha256",
                    DatabaseValue::from(source_checksum_sha256.clone()),
                )
                .value(
                    "executable_source_checksum_sha256",
                    DatabaseValue::from(&executable_source_checksum_sha256),
                )
                .value(
                    "definition_id",
                    DatabaseValue::from(compiled.definition_identity.definition_id.clone()),
                )
                .value(
                    "definition_version",
                    DatabaseValue::from(compiled.definition_identity.definition_version),
                )
                .value("document_json", DatabaseValue::from(&document_json))
                .value("producer_json", DatabaseValue::from(&producer_json))
                .value("published_at_ms", DatabaseValue::from(published_at_ms))),
        )?;
        let active_revision = if activate {
            block_on(
                transaction.as_ref().exec_update_count(
                    &(transaction
                        .as_ref()
                        .update("authored_workflows")
                        .value("active_revision", DatabaseValue::from(revision))
                        .value("updated_at_ms", DatabaseValue::from(published_at_ms))
                        .where_eq("workflow_id", DatabaseValue::from(workflow_id))),
                ),
            )?;
            Some(revision)
        } else {
            workflow.active_revision
        };
        let event_payload = bounded_json(
            "revision import event",
            &serde_json::json!({
                "revision": revision,
                "definition_id": compiled.definition_identity.definition_id,
                "definition_version": compiled.definition_identity.definition_version,
                "activated": activate,
                "imported": true,
            }),
        )?;
        authoring_insert(
            transaction.as_ref(),
            &(transaction
                .as_ref()
                .insert("workflow_authoring_events")
                .value("workflow_id", DatabaseValue::from(workflow_id))
                .value("event_type", DatabaseValue::from("revision_imported"))
                .value("payload_json", DatabaseValue::from(event_payload))
                .value("created_at_ms", DatabaseValue::from(published_at_ms))),
        )?;
        database::commit(transaction)?;
        Ok(WorkflowPublication {
            revision: PublishedWorkflowRevision {
                workflow_id: workflow_id.to_string(),
                revision,
                source_checksum_sha256,
                executable_source_checksum_sha256,
                definition_identity: compiled.definition_identity.clone(),
                document: document.clone(),
                producer: producer.clone(),
                published_at_ms,
            },
            active_revision,
        })
    }

    /// Load one exact immutable published revision without mutation.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, inconsistent stored content, or query failure.
    pub fn workflow_revision(
        &self,
        workflow_id: &str,
        revision: u64,
    ) -> Result<Option<PublishedWorkflowRevision>, WorkflowStoreError> {
        validate_id("workflow_id", workflow_id)?;
        if revision == 0 {
            return Err(WorkflowStoreError::InvalidData(
                "published revision must be positive".to_string(),
            ));
        }
        authoring_one(
            &self.connection,
            select("workflow_revisions")
                .columns(&[
                    "workflow_id",
                    "revision",
                    "source_checksum_sha256",
                    "executable_source_checksum_sha256",
                    "definition_id",
                    "definition_version",
                    "document_json",
                    "producer_json",
                    "published_at_ms",
                ])
                .where_eq("workflow_id", DatabaseValue::from(workflow_id))
                .where_eq("revision", DatabaseValue::from(revision)),
        )?
        .as_ref()
        .map(decode_workflow_revision)
        .transpose()?
        .map(validate_stored_workflow_revision)
        .transpose()
    }

    /// Return bounded immutable revisions newest first.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity/bound, inconsistent content, or query failure.
    pub fn list_workflow_revisions(
        &self,
        workflow_id: &str,
        limit: usize,
    ) -> Result<Vec<PublishedWorkflowRevision>, WorkflowStoreError> {
        Ok(self
            .list_workflow_revisions_page(workflow_id, None, limit)?
            .items)
    }

    /// Return one stable keyset page of immutable revisions newest first.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity/cursor/bound, inconsistent content, or query
    /// failure.
    pub fn list_workflow_revisions_page(
        &self,
        workflow_id: &str,
        cursor: Option<WorkflowRevisionListCursor>,
        limit: usize,
    ) -> Result<WorkflowAuthoringStorePage<PublishedWorkflowRevision>, WorkflowStoreError> {
        validate_id("workflow_id", workflow_id)?;
        let query_limit = authoring_limit(authoring_page_query_limit(limit)?)?;
        let mut query = select("workflow_revisions")
            .columns(REVISION_COLUMNS)
            .where_eq("workflow_id", workflow_id)
            .sort("revision", SortDirection::Desc)
            .limit(query_limit);
        if let Some(cursor) = cursor {
            cursor
                .validate()
                .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
            query = query.where_lt("revision", database::integer_value(cursor.revision)?);
        }
        authoring_rows(&self.connection, query)?
            .iter()
            .map(decode_workflow_revision)
            .map(|row| row.and_then(validate_stored_workflow_revision))
            .collect::<Result<Vec<_>, _>>()
            .map(|items| finish_authoring_store_page(items, limit))
    }

    /// Compare-and-set the active immutable revision pointer.
    ///
    /// # Errors
    ///
    /// Returns an error for archived/missing workflow, missing revision, stale pointer, timestamp
    /// regression, or database failure.
    pub fn set_active_workflow_revision(
        &mut self,
        workflow_id: &str,
        expected_active_revision: Option<u64>,
        revision: u64,
        updated_at_ms: u64,
    ) -> Result<(), WorkflowStoreError> {
        if self.workflow_revision(workflow_id, revision)?.is_none() {
            return Err(WorkflowStoreError::InvalidData(format!(
                "published workflow revision not found: {workflow_id} v{revision}"
            )));
        }
        let workflow = self.authored_workflow(workflow_id)?.ok_or_else(|| {
            WorkflowStoreError::InvalidData(format!("authored workflow not found: {workflow_id}"))
        })?;
        if workflow.archived || updated_at_ms < workflow.updated_at_ms {
            return Err(WorkflowStoreError::InvalidData(
                "archived workflow or regressed active-revision timestamp".to_string(),
            ));
        }
        let changed = block_on(
            self.connection.exec_update_count(
                &(self
                    .connection
                    .update("authored_workflows")
                    .value("active_revision", DatabaseValue::from(revision))
                    .value("updated_at_ms", DatabaseValue::from(updated_at_ms))
                    .where_eq("workflow_id", DatabaseValue::from(workflow_id))
                    .filter(Box::new(authoring_is(
                        "active_revision",
                        &(DatabaseValue::from(expected_active_revision)),
                    )?))),
            ),
        )?;
        if changed == 0 {
            let current = self
                .authored_workflow(workflow_id)?
                .and_then(|item| item.active_revision)
                .unwrap_or(0);
            return Err(WorkflowStoreError::AuthoringConflict {
                entity_id: workflow_id.to_string(),
                expected: expected_active_revision.unwrap_or(0),
                current,
            });
        }
        Ok(())
    }

    /// Create one revision-bound reusable configuration preset.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed state, missing revision, identity conflict, or failure.
    pub fn create_workflow_preset(
        &mut self,
        preset: &WorkflowPreset,
    ) -> Result<bool, WorkflowStoreError> {
        validate_workflow_preset(preset)?;
        if preset.generation != 1
            || self
                .workflow_revision(&preset.workflow_id, preset.revision)?
                .is_none()
        {
            return Err(WorkflowStoreError::InvalidData(
                "new preset requires generation 1 and an existing exact revision".to_string(),
            ));
        }
        let configuration_json = bounded_json("preset configuration", &preset.configuration)?;
        let run_limits_json = preset
            .run_limits
            .as_ref()
            .map(|limits| bounded_json("preset run limits", limits))
            .transpose()?;
        let producer_json = bounded_json("preset producer", &preset.producer)?;
        let changed = block_on(
            (self
                .connection
                .insert("workflow_presets")
                .value(
                    "workflow_id",
                    DatabaseValue::from(preset.workflow_id.clone()),
                )
                .value("preset_id", DatabaseValue::from(preset.preset_id.clone()))
                .value("revision", DatabaseValue::from(preset.revision))
                .value("name", DatabaseValue::from(preset.name.clone()))
                .value("generation", DatabaseValue::from(preset.generation))
                .value(
                    "configuration_json",
                    DatabaseValue::from(&configuration_json),
                )
                .value("run_limits_json", DatabaseValue::from(run_limits_json))
                .value("producer_json", DatabaseValue::from(&producer_json))
                .value("created_at_ms", DatabaseValue::from(preset.created_at_ms))
                .value("updated_at_ms", DatabaseValue::from(preset.updated_at_ms)))
            .or_ignore()
            .execute_count(&self.connection),
        )?;
        if changed == 1 {
            return Ok(true);
        }
        let existing = self
            .workflow_preset(&preset.workflow_id, &preset.preset_id)?
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData("preset insert was not observable".to_string())
            })?;
        if existing == *preset {
            Ok(false)
        } else {
            Err(WorkflowStoreError::InvalidData(format!(
                "workflow preset identity conflict: {}/{}",
                preset.workflow_id, preset.preset_id
            )))
        }
    }

    /// Load one exact preset without mutation.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, inconsistent content, or query failure.
    pub fn workflow_preset(
        &self,
        workflow_id: &str,
        preset_id: &str,
    ) -> Result<Option<WorkflowPreset>, WorkflowStoreError> {
        validate_id("workflow_id", workflow_id)?;
        validate_id("preset_id", preset_id)?;
        authoring_one(
            &self.connection,
            select("workflow_presets")
                .columns(&[
                    "workflow_id",
                    "preset_id",
                    "revision",
                    "name",
                    "generation",
                    "configuration_json",
                    "run_limits_json",
                    "producer_json",
                    "created_at_ms",
                    "updated_at_ms",
                ])
                .where_eq("workflow_id", DatabaseValue::from(workflow_id))
                .where_eq("preset_id", DatabaseValue::from(preset_id)),
        )?
        .as_ref()
        .map(decode_workflow_preset)
        .transpose()?
        .map(validate_stored_workflow_preset)
        .transpose()
    }

    /// Return bounded presets for one logical workflow.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity/bound, inconsistent content, or query failure.
    pub fn list_workflow_presets(
        &self,
        workflow_id: &str,
        limit: usize,
    ) -> Result<Vec<WorkflowPreset>, WorkflowStoreError> {
        Ok(self
            .list_workflow_presets_page(workflow_id, None, limit)?
            .items)
    }

    /// Return one stable keyset page of presets for a logical workflow.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity/cursor/bound, inconsistent content, or query
    /// failure.
    pub fn list_workflow_presets_page(
        &self,
        workflow_id: &str,
        cursor: Option<&WorkflowAuthoringListCursor>,
        limit: usize,
    ) -> Result<WorkflowAuthoringStorePage<WorkflowPreset>, WorkflowStoreError> {
        validate_id("workflow_id", workflow_id)?;
        if let Some(cursor) = cursor {
            cursor
                .validate()
                .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
        }
        let query_limit = authoring_limit(authoring_page_query_limit(limit)?)?;
        let query = authoring_cursor(
            select("workflow_presets")
                .columns(PRESET_COLUMNS)
                .where_eq("workflow_id", workflow_id),
            cursor,
            "preset_id",
        )?
        .sort("updated_at_ms", SortDirection::Desc)
        .sort("preset_id", SortDirection::Asc)
        .limit(query_limit);
        authoring_rows(&self.connection, query)?
            .iter()
            .map(decode_workflow_preset)
            .map(|row| row.and_then(validate_stored_workflow_preset))
            .collect::<Result<Vec<_>, _>>()
            .map(|items| finish_authoring_store_page(items, limit))
    }

    /// Return bounded authored lifecycle events without replaying or mutating state.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity/bound, malformed event JSON, or query failure.
    pub fn workflow_authoring_events(
        &self,
        workflow_id: &str,
        after_event_seq: Option<u64>,
        limit: usize,
    ) -> Result<Vec<WorkflowAuthoringEventRow>, WorkflowStoreError> {
        validate_id("workflow_id", workflow_id)?;
        let limit = authoring_limit(bounded_limit(limit)?)?;
        authoring_rows(
            &self.connection,
            select("workflow_authoring_events")
                .columns(&[
                    "event_seq",
                    "workflow_id",
                    "event_type",
                    "payload_json",
                    "created_at_ms",
                ])
                .where_eq("workflow_id", workflow_id)
                .where_gt(
                    "event_seq",
                    database::integer_value(after_event_seq.unwrap_or(0))?,
                )
                .sort("event_seq", SortDirection::Asc)
                .limit(limit),
        )?
        .iter()
        .map(|row| {
            Ok(WorkflowAuthoringEventRow {
                event_seq: database::unsigned(row, "event_seq")?,
                workflow_id: database::text(row, "workflow_id")?,
                event_type: database::text(row, "event_type")?,
                payload: serde_json::from_str(&database::text(row, "payload_json")?)?,
                created_at_ms: database::unsigned(row, "created_at_ms")?,
            })
        })
        .collect()
    }

    /// Diagnose bounded authored-state consistency without repair, replay, or mutation.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity/bound or query failure.
    pub fn diagnose_authored_workflow(
        &self,
        workflow_id: &str,
        limit: usize,
    ) -> Result<Vec<WorkflowAuthoringIssue>, WorkflowStoreError> {
        validate_id("workflow_id", workflow_id)?;
        let limit = authoring_limit(bounded_limit(limit)?)?;
        let mut issues = Vec::new();
        let active = authoring_one(
            &self.connection,
            select("authored_workflows")
                .columns(&["active_revision"])
                .where_eq("workflow_id", workflow_id),
        )?
        .map(|row| database::optional_unsigned(&row, "active_revision"))
        .transpose()?
        .flatten();
        if let Some(revision) = active
            && authoring_one(
                &self.connection,
                select("workflow_revisions")
                    .columns(&["revision"])
                    .where_eq("workflow_id", workflow_id)
                    .where_eq("revision", database::integer_value(revision)?),
            )?
            .is_none()
        {
            issues.push(WorkflowAuthoringIssue::InvalidActiveRevision { revision });
        }
        if issues.len() < limit {
            for row in authoring_rows(
                &self.connection,
                select("workflow_revisions")
                    .columns(&["revision", "definition_id", "definition_version"])
                    .where_eq("workflow_id", workflow_id)
                    .filter(Box::new(authoring_missing_definition()?))
                    .sort("revision", SortDirection::Asc)
                    .limit(limit - issues.len()),
            )? {
                issues.push(WorkflowAuthoringIssue::MissingCompiledDefinition {
                    revision: database::unsigned(&row, "revision")?,
                    definition_id: database::text(&row, "definition_id")?,
                    definition_version: authoring_get(&row, "definition_version")?,
                });
            }
        }
        if issues.len() < limit {
            for row in authoring_rows(
                &self.connection,
                select("workflow_presets")
                    .columns(&["preset_id", "revision"])
                    .where_eq("workflow_id", workflow_id)
                    .filter(Box::new(authoring_missing_revision(
                        "workflow_presets",
                        "revision",
                    )?))
                    .sort("preset_id", SortDirection::Asc)
                    .limit(limit - issues.len()),
            )? {
                issues.push(WorkflowAuthoringIssue::OrphanedPreset {
                    preset_id: database::text(&row, "preset_id")?,
                    revision: database::unsigned(&row, "revision")?,
                });
            }
        }
        if issues.len() < limit {
            for row in authoring_rows(
                &self.connection,
                select("workflow_drafts")
                    .columns(&["draft_id", "base_revision"])
                    .where_eq("workflow_id", workflow_id)
                    .filter(Box::new(authoring_missing_revision(
                        "workflow_drafts",
                        "base_revision",
                    )?))
                    .sort("draft_id", SortDirection::Asc)
                    .limit(limit - issues.len()),
            )? {
                if let Some(base_revision) = database::optional_unsigned(&row, "base_revision")? {
                    issues.push(WorkflowAuthoringIssue::StaleDraftBase {
                        draft_id: database::text(&row, "draft_id")?,
                        base_revision,
                    });
                }
            }
        }
        Ok(issues)
    }

    /// Explicitly repair provably derived authored indexes and an invalid active pointer.
    ///
    /// The caller must provide a backup path in the canonical workflow directory. This operation
    /// first acquires `SQLite` exclusive ownership, creates and verifies a complete backup, then
    /// clears only an active pointer whose target revision is absent and recreates disposable
    /// indexes. Missing definitions, stale draft bases, and orphaned presets remain surfaced because
    /// their intended canonical replacement is ambiguous.
    ///
    /// # Errors
    ///
    /// Returns an error unless exclusive ownership is available, the backup path is new and confined,
    /// backup verification succeeds, or a repair transaction fails.
    pub fn repair_authored_workflow(
        &mut self,
        workflow_id: &str,
        backup_path: &Path,
        repaired_at_ms: u64,
    ) -> Result<WorkflowAuthoringRepairReport, WorkflowStoreError> {
        validate_id("workflow_id", workflow_id)?;
        let root = self.path.parent().ok_or_else(|| {
            WorkflowStoreError::InvalidData("workflow database has no parent directory".to_string())
        })?;
        let root = std::fs::canonicalize(root)?;
        if backup_path.exists() {
            return Err(WorkflowStoreError::InvalidData(
                "workflow maintenance backup path already exists".to_string(),
            ));
        }
        let backup_parent = backup_path.parent().ok_or_else(|| {
            WorkflowStoreError::InvalidData("workflow backup path has no parent".to_string())
        })?;
        std::fs::create_dir_all(backup_parent)?;
        let backup_parent = std::fs::canonicalize(backup_parent)?;
        if backup_parent != root {
            return Err(WorkflowStoreError::InvalidData(
                "workflow maintenance backup must be in the canonical workflow directory"
                    .to_string(),
            ));
        }
        block_on(
            self.connection
                .sqlite_set_busy_timeout(std::time::Duration::ZERO),
        )?;
        let probe = database::begin(&self.connection, TransactionMode::Exclusive)?;
        block_on(probe.rollback())?;
        block_on(self.connection.sqlite_backup_to(backup_path))?;
        let backup = open_verified_backup(backup_path)?;
        let versions = database::rows(
            &backup,
            &backup
                .select("workflow_store_contract")
                .columns(&["schema_version"])
                .where_eq("contract_id", 1_i64)
                .limit(1),
        )?;
        if versions
            .first()
            .map(|row| database::unsigned(row, "schema_version"))
            .transpose()?
            != Some(u64::from(WORKFLOW_STORE_SCHEMA_VERSION))
        {
            return Err(WorkflowStoreError::InvalidData(
                "workflow maintenance backup verification failed".to_string(),
            ));
        }
        drop(backup);

        let transaction = database::begin(&self.connection, TransactionMode::Exclusive)?;
        let statement = transaction
            .update("authored_workflows")
            .value("active_revision", DatabaseValue::Null)
            .value("updated_at_ms", database::integer_value(repaired_at_ms)?)
            .where_eq("workflow_id", workflow_id)
            .filter(Box::new(authoring_missing_revision(
                "authored_workflows",
                "active_revision",
            )?));
        let cleared = block_on(transaction.exec_update_count(&statement))?;
        recreate_authoring_indexes(transaction.as_ref())?;
        database::commit(transaction)?;
        let remaining_issues = self.diagnose_authored_workflow(workflow_id, 1_000)?;
        Ok(WorkflowAuthoringRepairReport {
            backup_path: backup_path.to_path_buf(),
            active_pointer_cleared: cleared == 1,
            indexes_recreated: 5,
            remaining_issues,
        })
    }

    /// Replace one preset using exact optimistic generation.
    ///
    /// # Errors
    ///
    /// Returns an error for stale generation, malformed state, changed revision identity, or failure.
    #[allow(clippy::too_many_arguments)]
    pub fn update_workflow_preset(
        &mut self,
        workflow_id: &str,
        preset_id: &str,
        expected_generation: u64,
        name: &str,
        configuration: &serde_json::Value,
        run_limits: Option<&WorkflowRunLimitPolicy>,
        producer: &WorkflowProducerProvenance,
        updated_at_ms: u64,
    ) -> Result<WorkflowPreset, WorkflowStoreError> {
        self.update_workflow_preset_with_fault(
            workflow_id,
            preset_id,
            expected_generation,
            name,
            configuration,
            run_limits,
            producer,
            updated_at_ms,
            &NoopWorkflowAuthoringMutationFault,
        )
    }

    /// Replace one preset with deterministic fault observation before commit.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::update_workflow_preset`] plus injected fault errors.
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    pub fn update_workflow_preset_with_fault<F: WorkflowAuthoringMutationFault + ?Sized>(
        &mut self,
        workflow_id: &str,
        preset_id: &str,
        expected_generation: u64,
        name: &str,
        configuration: &serde_json::Value,
        run_limits: Option<&WorkflowRunLimitPolicy>,
        producer: &WorkflowProducerProvenance,
        updated_at_ms: u64,
        fault: &F,
    ) -> Result<WorkflowPreset, WorkflowStoreError> {
        validate_id("workflow_id", workflow_id)?;
        validate_id("preset_id", preset_id)?;
        let transaction = database::begin(&self.connection, TransactionMode::Deferred)?;
        let current = authoring_one(
            transaction.as_ref(),
            select("workflow_presets")
                .columns(&[
                    "workflow_id",
                    "preset_id",
                    "revision",
                    "name",
                    "generation",
                    "configuration_json",
                    "run_limits_json",
                    "producer_json",
                    "created_at_ms",
                    "updated_at_ms",
                ])
                .where_eq("workflow_id", DatabaseValue::from(workflow_id))
                .where_eq("preset_id", DatabaseValue::from(preset_id)),
        )?
        .as_ref()
        .map(decode_workflow_preset)
        .transpose()?
        .map(validate_stored_workflow_preset)
        .transpose()?
        .ok_or_else(|| {
            WorkflowStoreError::InvalidData(format!(
                "workflow preset not found: {workflow_id}/{preset_id}"
            ))
        })?;
        if current.generation != expected_generation {
            return Err(WorkflowStoreError::AuthoringConflict {
                entity_id: format!("{workflow_id}/{preset_id}"),
                expected: expected_generation,
                current: current.generation,
            });
        }
        let generation = expected_generation.checked_add(1).ok_or_else(|| {
            WorkflowStoreError::InvalidData("preset generation overflow".to_string())
        })?;
        let candidate = WorkflowPreset {
            workflow_id: workflow_id.to_string(),
            preset_id: preset_id.to_string(),
            revision: current.revision,
            name: name.to_string(),
            generation,
            configuration: configuration.clone(),
            run_limits: run_limits.cloned(),
            producer: producer.clone(),
            created_at_ms: current.created_at_ms,
            updated_at_ms,
        };
        validate_workflow_preset(&candidate)?;
        let configuration_json = bounded_json("preset configuration", configuration)?;
        let run_limits_json = run_limits
            .map(|limits| bounded_json("preset run limits", limits))
            .transpose()?;
        let producer_json = bounded_json("preset producer", producer)?;
        let changed = block_on(
            transaction.as_ref().exec_update_count(
                &(transaction
                    .as_ref()
                    .update("workflow_presets")
                    .value("name", DatabaseValue::from(name))
                    .value("generation", DatabaseValue::from(generation))
                    .value(
                        "configuration_json",
                        DatabaseValue::from(&configuration_json),
                    )
                    .value("run_limits_json", DatabaseValue::from(run_limits_json))
                    .value("producer_json", DatabaseValue::from(&producer_json))
                    .value("updated_at_ms", DatabaseValue::from(updated_at_ms))
                    .where_eq("workflow_id", DatabaseValue::from(workflow_id))
                    .where_eq("preset_id", DatabaseValue::from(preset_id))
                    .where_eq("generation", DatabaseValue::from(expected_generation))),
            ),
        )?;
        if changed == 0 {
            return Err(WorkflowStoreError::AuthoringConflict {
                entity_id: format!("{workflow_id}/{preset_id}"),
                expected: expected_generation,
                current: current.generation,
            });
        }
        fault.after_boundary(WorkflowAuthoringMutationBoundary::PresetUpdated)?;
        database::commit(transaction)?;
        Ok(candidate)
    }

    /// Delete one preset using exact optimistic generation.
    ///
    /// # Errors
    ///
    /// Returns an error when the preset is missing, stale, or deletion fails.
    pub fn delete_workflow_preset(
        &mut self,
        workflow_id: &str,
        preset_id: &str,
        expected_generation: u64,
    ) -> Result<(), WorkflowStoreError> {
        validate_id("workflow_id", workflow_id)?;
        validate_id("preset_id", preset_id)?;
        let changed = block_on(
            self.connection.exec_delete_count(
                &(self
                    .connection
                    .delete("workflow_presets")
                    .where_eq("workflow_id", DatabaseValue::from(workflow_id))
                    .where_eq("preset_id", DatabaseValue::from(preset_id))
                    .where_eq("generation", DatabaseValue::from(expected_generation))),
            ),
        )?;
        if changed == 0 {
            let current = self
                .workflow_preset(workflow_id, preset_id)?
                .map_or(0, |item| item.generation);
            return Err(WorkflowStoreError::AuthoringConflict {
                entity_id: format!("{workflow_id}/{preset_id}"),
                expected: expected_generation,
                current,
            });
        }
        Ok(())
    }

    /// Load one exact definition version with checksum verification.
    ///
    /// # Errors
    ///
    /// Returns an error when the query fails or persisted content does not match its checksum.
    pub fn definition(
        &self,
        definition_id: &str,
        version: u32,
    ) -> Result<Option<StoredWorkflowDefinition>, WorkflowStoreError> {
        validate_id("definition_id", definition_id)?;
        definition_in(&self.connection, definition_id, version)
    }
}

const AUTHORED_COLUMNS: &[&str] = &[
    "workflow_id",
    "title",
    "description",
    "archived",
    "active_revision",
    "created_at_ms",
    "updated_at_ms",
];
const DRAFT_COLUMNS: &[&str] = &[
    "workflow_id",
    "draft_id",
    "base_revision",
    "generation",
    "checksum_sha256",
    "document_json",
    "producer_json",
    "created_at_ms",
    "updated_at_ms",
];
const REVISION_COLUMNS: &[&str] = &[
    "workflow_id",
    "revision",
    "source_checksum_sha256",
    "executable_source_checksum_sha256",
    "definition_id",
    "definition_version",
    "document_json",
    "producer_json",
    "published_at_ms",
];
const PRESET_COLUMNS: &[&str] = &[
    "workflow_id",
    "preset_id",
    "revision",
    "name",
    "generation",
    "configuration_json",
    "run_limits_json",
    "producer_json",
    "created_at_ms",
    "updated_at_ms",
];

fn authoring_limit(limit: i64) -> Result<usize, WorkflowStoreError> {
    usize::try_from(limit)
        .map_err(|_| WorkflowStoreError::InvalidData("invalid authoring limit".into()))
}
fn authoring_rows(
    db: &dyn Database,
    query: switchy_database::query::SelectQuery<'_>,
) -> Result<Vec<Row>, WorkflowStoreError> {
    database::rows(db, &authoring_bounded_query(query))
}
fn authoring_one(
    db: &dyn Database,
    query: switchy_database::query::SelectQuery<'_>,
) -> Result<Option<Row>, WorkflowStoreError> {
    Ok(block_on(
        authoring_bounded_query(query).limit(1).execute_first(db),
    )?)
}
fn authoring_required(
    db: &dyn Database,
    query: switchy_database::query::SelectQuery<'_>,
) -> Result<Row, WorkflowStoreError> {
    authoring_one(db, query)?
        .ok_or_else(|| WorkflowStoreError::InvalidData("required authoring row missing".into()))
}
fn authoring_get<T: TryFrom<DatabaseValue>>(
    row: &Row,
    column: &str,
) -> Result<T, WorkflowStoreError> {
    let value = row.get(column).ok_or_else(|| {
        WorkflowStoreError::InvalidData(format!("missing authoring column: {column}"))
    })?;
    T::try_from(value)
        .map_err(|_| WorkflowStoreError::InvalidData(format!("invalid authoring column: {column}")))
}
fn authoring_insert(
    db: &dyn Database,
    statement: &switchy_database::query::InsertStatement<'_>,
) -> Result<u64, switchy_database::DatabaseError> {
    block_on(statement.execute(db))?;
    Ok(1)
}
fn authoring_is(
    column: &str,
    value: &DatabaseValue,
) -> Result<switchy_database::query::SchemaPredicate, WorkflowStoreError> {
    use switchy_database::schema::{SchemaComparison as C, SchemaExpression as E};
    let value = match value {
        DatabaseValue::Null => E::Null,
        DatabaseValue::UInt64(value) => E::Integer(i64::try_from(*value).map_err(|_| {
            WorkflowStoreError::InvalidData("active revision exceeds storage range".into())
        })?),
        DatabaseValue::Int64(value) => E::Integer(*value),
        _ => {
            return Err(WorkflowStoreError::InvalidData(
                "active revision is not a nullable integer".into(),
            ));
        }
    };
    Ok(switchy_database::query::schema_predicate(&E::Compare(
        Box::new(E::Column {
            table: None,
            column: column.into(),
        }),
        C::Is,
        Box::new(value),
    ))?)
}
fn authoring_cursor<'a>(
    query: switchy_database::query::SelectQuery<'a>,
    cursor: Option<&WorkflowAuthoringListCursor>,
    key: &str,
) -> Result<switchy_database::query::SelectQuery<'a>, WorkflowStoreError> {
    Ok(if let Some(cursor) = cursor {
        query.filter(Box::new(where_or(vec![
            Box::new(where_lt(
                "updated_at_ms",
                database::integer_value(cursor.updated_at_ms)?,
            )),
            Box::new(where_and(vec![
                Box::new(where_eq(
                    "updated_at_ms",
                    database::integer_value(cursor.updated_at_ms)?,
                )),
                Box::new(where_gt(key, cursor.entity_id.clone())),
            ])),
        ])))
    } else {
        query
    })
}
fn definition_query() -> switchy_database::query::SelectQuery<'static> {
    select("workflow_definitions").projections(vec![
        Projection::new(identifier("definition_id")),
        Projection::new(identifier("version")),
        Projection::new(identifier("checksum_sha256")),
        Projection::new(bounded_text(
            identifier("definition_json"),
            bcode_workflow::MAX_REGISTERED_WORKFLOW_DEFINITION_BYTES as u64,
            DatabaseValue::Null,
        ))
        .alias("definition_json"),
    ])
}
fn decode_authoring_definition(row: &Row) -> Result<StoredWorkflowDefinition, WorkflowStoreError> {
    Ok(StoredWorkflowDefinition {
        definition_id: database::text(row, "definition_id")?,
        version: authoring_get(row, "version")?,
        checksum_sha256: database::text(row, "checksum_sha256")?,
        definition_json: database::text(row, "definition_json")?,
    })
}
fn definitions_with_budget_on(
    db: &dyn Database,
    limit: usize,
    budget: usize,
) -> Result<Vec<StoredWorkflowDefinition>, WorkflowStoreError> {
    if budget == 0 {
        return Err(WorkflowStoreError::InvalidData(
            "definition byte budget must be positive".into(),
        ));
    }
    let query = select("workflow_definitions")
        .projections(vec![
            Projection::new(identifier("definition_id")),
            Projection::new(identifier("version")),
            Projection::new(byte_length(identifier("definition_json"))).alias("bytes"),
        ])
        .sort("definition_id", SortDirection::Asc)
        .sort("version", SortDirection::Desc)
        .limit(limit);
    let mut remaining = budget;
    let mut definitions = Vec::new();
    for row in authoring_rows(db, query)? {
        let bytes = usize::try_from(database::unsigned(&row, "bytes")?)
            .map_err(|_| WorkflowStoreError::InvalidData("definition size overflow".into()))?;
        remaining = remaining.checked_sub(bytes).ok_or_else(|| {
            WorkflowStoreError::InvalidData("definition window exceeds encoded byte budget".into())
        })?;
        let id = database::text(&row, "definition_id")?;
        let version = authoring_get(&row, "version")?;
        definitions.push(definition_in(db, &id, version)?.ok_or_else(|| {
            WorkflowStoreError::InvalidData("definition disappeared during bounded read".into())
        })?);
    }
    Ok(definitions)
}
fn latest_authoring_revision(
    db: &dyn Database,
    workflow_id: &str,
) -> Result<u64, WorkflowStoreError> {
    authoring_one(
        db,
        select("workflow_revisions")
            .columns(&["revision"])
            .where_eq("workflow_id", workflow_id)
            .sort("revision", SortDirection::Desc),
    )?
    .map(|row| database::unsigned(&row, "revision"))
    .transpose()
    .map(|value| value.unwrap_or(0))
}
fn next_authoring_revision(
    db: &dyn Database,
    workflow_id: &str,
) -> Result<u64, WorkflowStoreError> {
    latest_authoring_revision(db, workflow_id)?
        .checked_add(1)
        .ok_or_else(|| WorkflowStoreError::InvalidData("workflow revision overflow".into()))
}

fn authoring_missing_definition()
-> Result<switchy_database::query::SchemaPredicate, WorkflowStoreError> {
    use switchy_database::schema::{SchemaComparison as C, SchemaExpression as E};
    let exists = E::exists(
        "workflow_definitions",
        E::And(vec![
            schema_columns_equal(
                "workflow_definitions",
                "definition_id",
                "workflow_revisions",
                "definition_id",
            ),
            schema_columns_equal(
                "workflow_definitions",
                "version",
                "workflow_revisions",
                "definition_version",
            ),
        ]),
    );
    Ok(switchy_database::query::schema_predicate(&E::Compare(
        Box::new(exists),
        C::Equal,
        Box::new(E::Integer(0)),
    ))?)
}
fn schema_columns_equal(
    left_table: &str,
    left: &str,
    right_table: &str,
    right: &str,
) -> switchy_database::schema::SchemaExpression {
    use switchy_database::schema::{SchemaComparison as C, SchemaExpression as E};
    E::Compare(
        Box::new(E::Column {
            table: Some(left_table.into()),
            column: left.into(),
        }),
        C::Equal,
        Box::new(E::Column {
            table: Some(right_table.into()),
            column: right.into(),
        }),
    )
}
fn authoring_missing_revision(
    table: &str,
    column: &str,
) -> Result<switchy_database::query::SchemaPredicate, WorkflowStoreError> {
    use switchy_database::schema::{SchemaComparison as C, SchemaExpression as E};
    let non_null = E::Compare(
        Box::new(E::Column {
            table: Some(table.into()),
            column: column.into(),
        }),
        C::IsNot,
        Box::new(E::Null),
    );
    let exists = E::exists(
        "workflow_revisions",
        E::And(vec![
            schema_columns_equal("workflow_revisions", "workflow_id", table, "workflow_id"),
            schema_columns_equal("workflow_revisions", "revision", table, column),
        ]),
    );
    Ok(switchy_database::query::schema_predicate(&E::And(vec![
        non_null,
        E::Compare(Box::new(exists), C::Equal, Box::new(E::Integer(0))),
    ]))?)
}

fn authoring_bounded_query(
    mut query: switchy_database::query::SelectQuery<'_>,
) -> switchy_database::query::SelectQuery<'_> {
    if query.projections.is_empty() {
        query.projections = query
            .columns
            .iter()
            .map(|column| {
                let expression = identifier(column);
                match *column {
                    "document_json" | "producer_json" | "configuration_json"
                    | "run_limits_json" | "payload_json" | "receipt_json" => {
                        Projection::new(bounded_text(
                            expression,
                            MAX_INLINE_JSON_BYTES as u64,
                            DatabaseValue::Null,
                        ))
                        .alias(*column)
                    }
                    _ => Projection::new(expression),
                }
            })
            .collect();
    }
    query
}

fn recreate_authoring_indexes(transaction: &dyn Database) -> Result<(), WorkflowStoreError> {
    for (name, table, columns) in [
        (
            "idx_authored_workflows_updated",
            "authored_workflows",
            vec![
                ("updated_at_ms", SortDirection::Desc),
                ("workflow_id", SortDirection::Asc),
            ],
        ),
        (
            "idx_workflow_drafts_updated",
            "workflow_drafts",
            vec![
                ("workflow_id", SortDirection::Asc),
                ("updated_at_ms", SortDirection::Desc),
                ("draft_id", SortDirection::Asc),
            ],
        ),
        (
            "idx_workflow_revisions_published",
            "workflow_revisions",
            vec![
                ("workflow_id", SortDirection::Asc),
                ("revision", SortDirection::Desc),
            ],
        ),
        (
            "idx_workflow_presets_updated",
            "workflow_presets",
            vec![
                ("workflow_id", SortDirection::Asc),
                ("updated_at_ms", SortDirection::Desc),
                ("preset_id", SortDirection::Asc),
            ],
        ),
        (
            "idx_workflow_authoring_events_identity",
            "workflow_authoring_events",
            vec![
                ("workflow_id", SortDirection::Asc),
                ("event_seq", SortDirection::Asc),
            ],
        ),
    ] {
        block_on(
            switchy_database::schema::drop_index(name, table)
                .if_exists()
                .execute(transaction),
        )?;
        let mut index = switchy_database::schema::create_index(name).table(table);
        for (column, direction) in columns {
            index = index.column_ordered(column, direction);
        }
        block_on(index.execute(transaction))?;
    }

    Ok(())
}
