//! Transport-neutral application operations for workflow authoring and execution.

use super::ServerState;

/// Resolve and start one authored workflow revision, active revision, or preset.
pub async fn start_authored(
    client_id: super::ClientId,
    state: &std::sync::Arc<ServerState>,
    request: bcode_ipc::StartAuthoredWorkflowRequest,
    authorize: bool,
) -> Result<bcode_ipc::AuthoredWorkflowRunStartResponse, super::ServerError> {
    super::start_authored_workflow(client_id, state, request, authorize).await
}

/// Import one exact next published revision into an existing authored workflow.
pub async fn import_revision(
    request_id: u64,
    client_id: super::ClientId,
    state: &std::sync::Arc<ServerState>,
    request: bcode_ipc::ImportWorkflowRevisionRequest,
) -> Result<bcode_ipc::WorkflowRevisionImportResult, super::ServerError> {
    super::import_workflow_revision(request_id, client_id, state, request).await
}

/// Publish one draft and admit its resulting revision when publication succeeds.
pub async fn publish_and_start(
    request_id: u64,
    client_id: super::ClientId,
    state: &std::sync::Arc<ServerState>,
    request: bcode_ipc::PublishAndStartWorkflowRequest,
) -> Result<bcode_ipc::WorkflowPublishAndStartResult, super::ServerError> {
    super::publish_and_start_workflow(request_id, client_id, state, request).await
}

/// Publish one validated authored workflow draft.
pub async fn publish_draft(
    request_id: u64,
    client_id: super::ClientId,
    state: &std::sync::Arc<ServerState>,
    request: bcode_ipc::PublishWorkflowDraftRequest,
    operation: bcode_workflow::WorkflowApplicationOperation,
) -> Result<bcode_ipc::WorkflowPublicationResult, super::ServerError> {
    super::publish_workflow_draft(request_id, client_id, state, request, operation).await
}

/// Build one bounded semantic workflow run view.
pub fn run_view(
    state: &ServerState,
    run_id: &str,
    limit: usize,
) -> Result<bcode_workflow_view_models::WorkflowRunView, super::ServerError> {
    super::workflow_run_view(state, run_id, limit)
}

/// Return one bounded page of authored workflows.
pub fn list_authored_workflows(
    state: &ServerState,
    cursor: Option<&bcode_workflow::WorkflowAuthoringListCursor>,
    limit: usize,
) -> Result<
    bcode_ipc::WorkflowAuthoringPage<
        bcode_ipc::AuthoredWorkflowSnapshot,
        bcode_workflow::WorkflowAuthoringListCursor,
    >,
    bcode_workflow_store::WorkflowStoreError,
> {
    let page = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .list_authored_workflows_page(cursor, limit)?;
    Ok(super::authored_workflow_page(page, limit))
}

/// Return one authored workflow description when it exists.
pub fn authored_workflow(
    state: &ServerState,
    workflow_id: &str,
) -> Result<Option<bcode_ipc::AuthoredWorkflowSnapshot>, bcode_workflow_store::WorkflowStoreError> {
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .authored_workflow(workflow_id)
        .map(|workflow| workflow.map(super::authored_workflow_snapshot))
}

/// Return a bounded authored-workflow inspection when the workflow exists.
pub fn inspect_authored_workflow(
    state: &ServerState,
    workflow_id: &str,
    limit: usize,
) -> Result<Option<bcode_ipc::AuthoredWorkflowInspection>, super::ServerError> {
    super::authored_workflow_inspection(state, workflow_id, limit)
}

/// Return one bounded page of drafts for an authored workflow.
pub fn list_drafts(
    state: &ServerState,
    workflow_id: &str,
    cursor: Option<&bcode_workflow::WorkflowAuthoringListCursor>,
    limit: usize,
) -> Result<
    bcode_ipc::WorkflowAuthoringPage<
        bcode_ipc::WorkflowDraftSnapshot,
        bcode_workflow::WorkflowAuthoringListCursor,
    >,
    bcode_workflow_store::WorkflowStoreError,
> {
    let page = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .list_workflow_drafts_page(workflow_id, cursor, limit.saturating_add(1))?;
    Ok(super::workflow_draft_page(page, limit))
}

/// Return one authored workflow draft when it exists.
pub fn draft(
    state: &ServerState,
    workflow_id: &str,
    draft_id: &str,
) -> Result<Option<bcode_ipc::WorkflowDraftSnapshot>, bcode_workflow_store::WorkflowStoreError> {
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .workflow_draft(workflow_id, draft_id)
        .map(|draft| draft.map(super::workflow_draft_snapshot))
}

/// Return one bounded page of published revisions for an authored workflow.
pub fn list_revisions(
    state: &ServerState,
    workflow_id: &str,
    cursor: Option<bcode_workflow::WorkflowRevisionListCursor>,
    limit: usize,
) -> Result<
    bcode_ipc::WorkflowAuthoringPage<
        bcode_ipc::WorkflowRevisionSnapshot,
        bcode_workflow::WorkflowRevisionListCursor,
    >,
    bcode_workflow_store::WorkflowStoreError,
> {
    let page = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .list_workflow_revisions_page(workflow_id, cursor, limit)?;
    Ok(super::workflow_revision_page(page, limit))
}

/// Return one published workflow revision when it exists.
pub fn revision(
    state: &ServerState,
    workflow_id: &str,
    revision: u64,
) -> Result<Option<bcode_ipc::WorkflowRevisionSnapshot>, bcode_workflow_store::WorkflowStoreError> {
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .workflow_revision(workflow_id, revision)
        .map(|revision| revision.map(super::workflow_revision_snapshot))
}

/// Return one bounded page of presets for an authored workflow.
pub fn list_presets(
    state: &ServerState,
    workflow_id: &str,
    cursor: Option<&bcode_workflow::WorkflowAuthoringListCursor>,
    limit: usize,
) -> Result<
    bcode_ipc::WorkflowAuthoringPage<
        bcode_ipc::WorkflowPresetSnapshot,
        bcode_workflow::WorkflowAuthoringListCursor,
    >,
    bcode_workflow_store::WorkflowStoreError,
> {
    let page = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .list_workflow_presets_page(workflow_id, cursor, limit.saturating_add(1))?;
    Ok(super::workflow_preset_page(page, limit))
}

/// Return one authored workflow preset when it exists.
pub fn preset(
    state: &ServerState,
    workflow_id: &str,
    preset_id: &str,
) -> Result<Option<bcode_ipc::WorkflowPresetSnapshot>, bcode_workflow_store::WorkflowStoreError> {
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .workflow_preset(workflow_id, preset_id)
        .map(|preset| preset.map(super::workflow_preset_snapshot))
}

/// Return the latest durable publication receipt for one workflow package.
pub fn package_publication(
    state: &ServerState,
    package_id: &str,
) -> Result<
    Option<bcode_workflow::WorkflowPackagePublicationReceipt>,
    bcode_workflow_store::WorkflowStoreError,
> {
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .workflow_package_publication(package_id, None)
}

/// Atomically apply one validated workflow package to the canonical workflow store.
pub fn apply_package(
    state: &ServerState,
    request: &bcode_ipc::ApplyWorkflowPackageRequest,
) -> Result<bcode_workflow::WorkflowPackageMutationResult, bcode_workflow_store::WorkflowStoreError>
{
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .apply_workflow_package(&request.request, request.applied_at_ms)
}

/// Atomically publish one workflow package in the canonical workflow store.
pub fn publish_package(
    state: &ServerState,
    request: &bcode_ipc::PublishWorkflowPackageRequest,
) -> Result<bcode_workflow::WorkflowPackageMutationResult, bcode_workflow_store::WorkflowStoreError>
{
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .publish_workflow_package(&request.request, request.published_at_ms)
}

/// Validate and plan one bounded workflow package closure.
pub async fn validate_package(
    state: &ServerState,
    request: bcode_ipc::WorkflowPackageComputationRequest,
    fallback_operation_id: String,
) -> Result<bcode_ipc::WorkflowPackageValidationResult, super::ServerError> {
    let catalog = authoring_catalog(state).await?;
    let plan =
        super::run_workflow_computation(state, request.control, fallback_operation_id, move || {
            bcode_workflow::plan_workflow_package_closure(&request.closure, &catalog)
        })
        .await??;
    Ok(bcode_ipc::WorkflowPackageValidationResult { plan })
}

/// Preview one already planned workflow package without persistence side effects.
pub async fn preview_package(
    state: &ServerState,
    request: bcode_ipc::WorkflowPackagePreviewRequest,
    fallback_operation_id: String,
) -> Result<bcode_workflow::WorkflowPackagePreview, super::ServerError> {
    let mut catalog = authoring_catalog(state).await?;
    for dependency in &request.dependency_plans {
        for member in &dependency.members {
            catalog.workflow_definitions.insert(
                member.definition_identity.definition_id.clone(),
                member.lowering.document.definition.clone(),
            );
        }
    }
    super::run_workflow_computation(state, request.control, fallback_operation_id, move || {
        bcode_workflow::preview_workflow_package(&request.plan, &catalog, &request.configurations)
    })
    .await?
    .map_err(super::ServerError::from)
}

/// Lower and validate one bounded authored-workflow source document.
pub async fn validate_source(
    state: &ServerState,
    request: bcode_ipc::WorkflowSourceComputationRequest,
    fallback_operation_id: String,
) -> Result<bcode_ipc::WorkflowSourceValidationResult, super::ServerError> {
    let catalog = authoring_catalog(state).await?;
    let source_format = request.source_format;
    let lowering =
        super::run_workflow_computation(state, request.control, fallback_operation_id, move || {
            bcode_workflow::lower_workflow_authoring_source(
                &request.source,
                source_format,
                &catalog,
            )
        })
        .await??;
    Ok(bcode_ipc::WorkflowSourceValidationResult {
        source_format,
        lowering,
    })
}

/// Lower and preview compilation for one bounded authored-workflow source document.
pub async fn preview_source(
    state: &ServerState,
    request: bcode_ipc::WorkflowSourcePreviewRequest,
    fallback_operation_id: String,
) -> Result<bcode_ipc::WorkflowSourcePreviewResult, super::ServerError> {
    let catalog = authoring_catalog(state).await?;
    let source_format = request.source_format;
    let configuration = request.configuration;
    let (lowering, preview) =
        super::run_workflow_computation(state, request.control, fallback_operation_id, move || {
            let lowering = bcode_workflow::lower_workflow_authoring_source(
                &request.source,
                source_format,
                &catalog,
            )?;
            let preview = lowering
                .document
                .compilation_preview(&catalog, configuration.as_ref());
            Ok::<_, bcode_workflow::WorkflowError>((lowering, preview))
        })
        .await??;
    Ok(bcode_ipc::WorkflowSourcePreviewResult {
        source_format,
        lowering,
        preview,
    })
}

/// Validate one bounded authored-workflow document.
pub async fn validate_authoring(
    state: &ServerState,
    document: bcode_workflow::WorkflowAuthoringDocument,
    control: bcode_ipc::WorkflowComputationControl,
    fallback_operation_id: String,
) -> Result<bcode_workflow::WorkflowValidationReport, super::ServerError> {
    super::run_workflow_computation(state, control, fallback_operation_id, move || {
        document.validation_report()
    })
    .await
}

/// Preview compilation for one bounded authored-workflow document.
pub async fn preview_compilation(
    state: &ServerState,
    document: bcode_workflow::WorkflowAuthoringDocument,
    configuration: Option<serde_json::Value>,
    control: bcode_ipc::WorkflowComputationControl,
    fallback_operation_id: String,
) -> Result<bcode_workflow::WorkflowCompilationPreview, super::ServerError> {
    let catalog = authoring_catalog(state).await?;
    super::run_workflow_computation(state, control, fallback_operation_id, move || {
        document.compilation_preview(&catalog, configuration.as_ref())
    })
    .await
}

/// Instantiate one exact enabled template as a new authored workflow and initial draft.
pub async fn instantiate_template(
    client_id: super::ClientId,
    state: &std::sync::Arc<ServerState>,
    request: bcode_ipc::WorkflowTemplateInstantiationRequest,
) -> Result<
    (
        bcode_workflow_store::AuthoredWorkflow,
        bcode_workflow_store::WorkflowDraft,
    ),
    super::ServerError,
> {
    super::instantiate_workflow_template(client_id, state, request).await
}

/// Admit one run for an already persisted exact workflow definition.
pub async fn start_run(
    state: &std::sync::Arc<ServerState>,
    request: bcode_ipc::WorkflowRunStartRequest,
    provenance: Option<bcode_workflow_store::AuthoredWorkflowRunProvenance>,
) -> Result<bcode_ipc::WorkflowRunStartResponse, super::ServerError> {
    super::start_workflow_run(state, request, provenance).await
}

/// Validate, persist, and admit one exact workflow definition.
pub async fn start(
    state: &std::sync::Arc<ServerState>,
    request: bcode_ipc::WorkflowStartRequest,
) -> Result<bcode_ipc::WorkflowRunStartResponse, super::ServerError> {
    let started_at = std::time::Instant::now();
    super::validate_workflow_definition_for_production(state, &request.definition)?;
    if request.identity.kind != request.binding.workflow_kind {
        return Err(bcode_workflow_store::WorkflowStoreError::InvalidData(
            "workflow logical identity does not match its binding kind".to_string(),
        )
        .into());
    }
    let expected_identity = bcode_workflow::WorkflowDefinitionIdentity::for_definition(
        request.identity.kind.clone(),
        &request.definition,
    )
    .map_err(|error| bcode_workflow_store::WorkflowStoreError::InvalidData(error.to_string()))?;
    if expected_identity != request.identity {
        return Err(bcode_workflow_store::WorkflowStoreError::InvalidData(
            "workflow exact identity does not match its compiled definition".to_string(),
        )
        .into());
    }
    let stored = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .persist_definition(
            &request.identity.definition_id,
            request.identity.definition_version,
            &request.definition,
        )?;
    if stored.definition_id != request.identity.definition_id
        || stored.version != request.identity.definition_version
    {
        return Err(bcode_workflow_store::WorkflowStoreError::InvalidData(
            "workflow exact identity does not match persisted definition".to_string(),
        )
        .into());
    }
    let result = super::start_workflow_run(
        state,
        bcode_ipc::WorkflowRunStartRequest {
            definition_id: request.identity.definition_id,
            definition_version: request.identity.definition_version,
            run_id: request.run_id,
            workspace_snapshot: request.workspace_snapshot.unwrap_or_default(),
            parent_session_id: request.parent_session_id,
            parent_session_generation: None,
            binding: Some(request.binding),
            input: Some(request.input),
            limits: request.limits,
        },
        None,
    )
    .await;
    state.metrics.record_histogram_with_labels(
        "workflow.admission.duration_ms",
        u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
        std::collections::BTreeMap::from([(
            "outcome".to_string(),
            if result.is_ok() { "ok" } else { "error" }.to_string(),
        )]),
    );
    result
}

/// Compile and start one exact enabled workflow template.
pub async fn start_template(
    state: &std::sync::Arc<ServerState>,
    request: bcode_ipc::WorkflowTemplateStartRequest,
) -> Result<bcode_ipc::WorkflowRunStartResponse, super::ServerError> {
    let template = super::find_workflow_template(
        state,
        &request.owner_plugin_id,
        &request.template_id,
        request.template_version,
    )
    .ok_or_else(|| {
        bcode_workflow_store::WorkflowStoreError::InvalidData(
            "workflow template not found or disabled".to_string(),
        )
    })?;
    let description = template_description(state, &request.owner_plugin_id, template)?;
    if !description.diagnostics.is_empty() {
        return Err(super::ServerError::WorkflowCapabilityUnavailable(
            description
                .diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>()
                .join("; "),
        ));
    }
    let validator =
        jsonschema::validator_for(&template.configuration_schema().schema).map_err(|error| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                "invalid template configuration schema: {error}"
            ))
        })?;
    if let Err(error) = validator.validate(&request.configuration) {
        return Err(
            bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                "template configuration is invalid: {error}"
            ))
            .into(),
        );
    }
    let binding_kind = description.identity.kind.clone();
    let definition = super::compile_workflow_template(template, &request.configuration)?;
    super::persist_exact_template_call_dependencies(state, &definition)?;
    let identity = bcode_workflow::WorkflowDefinitionIdentity::for_definition(
        description.identity.kind,
        &definition,
    )
    .map_err(|error| bcode_workflow_store::WorkflowStoreError::InvalidData(error.to_string()))?;
    start(
        state,
        bcode_ipc::WorkflowStartRequest {
            identity,
            definition,
            run_id: request.run_id,
            workspace_snapshot: request.workspace_snapshot,
            parent_session_id: request.parent_session_id,
            input: request.configuration,
            binding: bcode_workflow_store::WorkflowRunBinding {
                owner_plugin_id: request.owner_plugin_id,
                workflow_kind: binding_kind,
                scope_key: request.template_version.to_string(),
                display_label: Some(template.title.clone()),
                single_active: false,
            },
            limits: request.limits,
        },
    )
    .await
}

/// Request cancellation of a workflow tree while holding its durable execution authority.
pub async fn cancel_run(
    state: &std::sync::Arc<ServerState>,
    run_id: &str,
) -> Result<bool, super::ServerError> {
    let _authority = super::workflow_execution_authority(state, run_id)
        .await?
        .ok_or_else(|| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(
                "active workflow has no durable execution authority".to_string(),
            )
        })?;
    let (recorded, attempts) = {
        let mut store = state
            .workflow_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (recorded, cancelled_run_ids) =
            store.request_cancellation_tree(run_id, super::current_unix_millis())?;
        let mut attempts = Vec::new();
        for cancelled_run_id in cancelled_run_ids {
            let remaining = 1_000_usize.saturating_sub(attempts.len());
            if remaining == 0 {
                break;
            }
            attempts.extend(store.active_attempt_cancellations(&cancelled_run_id, remaining)?);
        }
        drop(store);
        (recorded, attempts)
    };
    super::propagate_persisted_workflow_cancellation(state, attempts).await?;
    Ok(recorded)
}

/// Pause a workflow run while holding its durable execution authority.
pub async fn pause_run(
    state: &std::sync::Arc<ServerState>,
    run_id: &str,
) -> Result<bool, super::ServerError> {
    let _authority = super::workflow_execution_authority(state, run_id)
        .await?
        .ok_or_else(|| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(
                "active workflow has no durable execution authority".to_string(),
            )
        })?;
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .pause_run(run_id, super::current_unix_millis())
        .map_err(super::ServerError::from)
}

/// Resume a workflow run and continue scheduling it without transport framing.
pub async fn resume_run(
    state: &std::sync::Arc<ServerState>,
    run_id: &str,
) -> Result<bool, super::ServerError> {
    let run = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .run_summary(run_id)?
        .ok_or_else(|| bcode_workflow_store::WorkflowStoreError::RunNotFound {
            run_id: run_id.to_string(),
        })?;
    if !matches!(
        run.status,
        bcode_workflow_store::RunStatus::Running | bcode_workflow_store::RunStatus::Paused
    ) {
        return state
            .workflow_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .resume_run(run_id, super::current_unix_millis())
            .map_err(super::ServerError::from);
    }
    let _authority = super::workflow_execution_authority(state, run_id)
        .await?
        .ok_or_else(|| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(
                "active workflow has no durable execution authority".to_string(),
            )
        })?;
    let changed = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .resume_run(run_id, super::current_unix_millis())?;
    super::drive_workflow_run(state, run_id).await?;
    Ok(changed)
}

/// Inspect one workflow run for bounded structural inconsistencies without mutation.
pub fn doctor_run(
    state: &ServerState,
    run_id: &str,
    limit: usize,
) -> Result<bcode_workflow_store::WorkflowDoctorReport, bcode_workflow_store::WorkflowStoreError> {
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .doctor_run(run_id, limit)
}

/// Retry one exact failed workflow node and resume scheduling.
pub async fn retry_node(
    state: &std::sync::Arc<ServerState>,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
    failed_attempt: u32,
) -> Result<bcode_workflow_store::WorkflowNodeRetryResult, super::ServerError> {
    let started_at = std::time::Instant::now();
    let result = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retry_failed_node(
            run_id,
            node_id,
            activation_id,
            failed_attempt,
            super::current_unix_millis(),
        )?;
    state.metrics.record_histogram(
        "workflow.retry.admission.duration_ms",
        u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
    );
    super::drive_workflow_run(state, run_id).await?;
    Ok(result)
}

/// Return bounded durable input and approval waits without transport framing.
pub fn list_waits(
    state: &ServerState,
    run_id: &str,
    limit: usize,
) -> Result<Vec<bcode_workflow_store::WaitingActivation>, bcode_workflow_store::WorkflowStoreError>
{
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .waiting_activations(run_id, limit)
}

/// Resolve one exact workflow input wait and continue runnable descendants.
pub async fn provide_input(
    state: &std::sync::Arc<ServerState>,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
    value: serde_json::Value,
) -> Result<bcode_workflow_store::WaitingResolutionResult, super::ServerError> {
    let started_at = std::time::Instant::now();
    let result = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .provide_input(
            run_id,
            node_id,
            activation_id,
            value,
            super::current_unix_millis(),
        )?;
    super::drive_workflow_run_and_parents(state, run_id).await?;
    state.metrics.record_histogram(
        "workflow.input.wait_resolution.duration_ms",
        u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
    );
    Ok(result)
}

/// Resolve one exact workflow approval wait and continue runnable descendants.
pub async fn resolve_approval(
    state: &std::sync::Arc<ServerState>,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
    approved: bool,
) -> Result<bcode_workflow_store::WaitingResolutionResult, super::ServerError> {
    let started_at = std::time::Instant::now();
    let result = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .resolve_approval(
            run_id,
            node_id,
            activation_id,
            approved,
            super::current_unix_millis(),
        )?;
    super::drive_workflow_run_and_parents(state, run_id).await?;
    state.metrics.record_histogram_with_labels(
        "workflow.approval.resolution.duration_ms",
        u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
        std::collections::BTreeMap::from([(
            "decision".to_string(),
            if approved { "approve" } else { "deny" }.to_string(),
        )]),
    );
    Ok(result)
}

/// Return bounded pending mutation approvals across all workflow runs.
pub fn list_mutation_approvals_all(
    state: &ServerState,
    limit: usize,
) -> Result<
    Vec<bcode_workflow_store::WorkflowMutationApproval>,
    bcode_workflow_store::WorkflowStoreError,
> {
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .pending_mutation_approvals_all(limit)
}

/// Return bounded pending mutation approvals for one workflow run.
pub fn list_mutation_approvals(
    state: &ServerState,
    run_id: &str,
    limit: usize,
) -> Result<
    Vec<bcode_workflow_store::WorkflowMutationApproval>,
    bcode_workflow_store::WorkflowStoreError,
> {
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .pending_mutation_approvals(run_id, limit)
}

/// Resolve one mutation approval and continue an approved workflow.
pub async fn resolve_mutation_approval(
    state: &std::sync::Arc<ServerState>,
    approval_id: &str,
    decision: bcode_workflow_store::WorkflowMutationApprovalDecision,
) -> Result<bcode_workflow_store::WorkflowMutationApprovalResolution, super::ServerError> {
    let started_at = std::time::Instant::now();
    let resolved_at_ms = super::current_unix_millis();
    let approval_context = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .mutation_approval_context(approval_id)?;
    let result = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .resolve_mutation_approval(approval_id, decision, resolved_at_ms)?;
    if matches!(
        decision,
        bcode_workflow_store::WorkflowMutationApprovalDecision::Approve
    ) && let Some((run_id, _)) = approval_context.as_ref()
    {
        super::drive_workflow_run_and_parents(state, run_id).await?;
    }
    let decision_label = match decision {
        bcode_workflow_store::WorkflowMutationApprovalDecision::Approve => "approve",
        bcode_workflow_store::WorkflowMutationApprovalDecision::Deny => "deny",
    };
    state.metrics.record_histogram_with_labels(
        "workflow.approval.wait.duration_ms",
        approval_context.map_or(0, |(_, requested_at_ms)| {
            resolved_at_ms.saturating_sub(requested_at_ms)
        }),
        std::collections::BTreeMap::from([
            ("decision".to_string(), decision_label.to_string()),
            ("status".to_string(), result.status.clone()),
        ]),
    );
    state.metrics.record_histogram_with_labels(
        "workflow.approval.resolution.duration_ms",
        u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
        std::collections::BTreeMap::from([("decision".to_string(), decision_label.to_string())]),
    );
    Ok(result)
}

/// Return bounded keyset-paged attempt history for one workflow run.
pub fn attempt_history(
    state: &ServerState,
    run_id: &str,
    cursor: Option<&bcode_workflow_store::AttemptCursor>,
    limit: usize,
) -> Result<Vec<bcode_workflow_store::AttemptSummary>, bcode_workflow_store::WorkflowStoreError> {
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .attempt_history(run_id, cursor, limit)
}

/// Return bounded keyset-paged event history for one workflow run.
pub fn event_history(
    state: &ServerState,
    run_id: &str,
    after_sequence: Option<u64>,
    limit: usize,
) -> Result<Vec<bcode_workflow_store::WorkflowEventRow>, bcode_workflow_store::WorkflowStoreError> {
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .event_history(run_id, after_sequence, limit)
}

/// Return bounded workflow live-event catch-up state without transport framing.
pub fn live_event_catch_up(
    state: &ServerState,
    after_sequence: u64,
    limit: usize,
) -> Result<
    bcode_workflow_view_models::WorkflowLiveEventPage,
    bcode_workflow_store::WorkflowStoreError,
> {
    if limit == 0 || limit > 1_000 {
        return Err(bcode_workflow_store::WorkflowStoreError::InvalidData(
            "workflow live catch-up limit must be in 1..=1000".to_string(),
        ));
    }
    let (positions, latest_sequence) = {
        let store = state
            .workflow_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (
            store.event_positions_after(after_sequence, limit)?,
            store.latest_global_event_sequence()?,
        )
    };
    let resync_required = positions
        .last()
        .is_some_and(|(event_sequence, _, _)| *event_sequence < latest_sequence);
    let events = positions
        .into_iter()
        .map(|(event_sequence, run_id, changed_at_ms)| {
            bcode_workflow_view_models::WorkflowLiveEvent {
                version: bcode_workflow_view_models::WORKFLOW_LIVE_EVENT_VERSION,
                run_id,
                event_sequence,
                changed_at_ms,
            }
        })
        .collect();
    Ok(bcode_workflow_view_models::WorkflowLiveEventPage {
        events,
        resync_required,
    })
}

/// Return one workflow run summary without transport framing.
pub fn run_status(
    state: &ServerState,
    run_id: &str,
) -> Result<
    Option<bcode_workflow_store::WorkflowRunSummary>,
    bcode_workflow_store::WorkflowStoreError,
> {
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .run_summary(run_id)
}

/// Return the workflow run associated with one plugin-owned binding.
pub fn associated_run(
    state: &ServerState,
    key: &bcode_workflow_store::WorkflowRunBindingKey,
) -> Result<
    Option<bcode_workflow_store::WorkflowRunSummary>,
    bcode_workflow_store::WorkflowStoreError,
> {
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .associated_run(key)
}

/// Inspect the workflow run associated with one plugin-owned binding.
pub async fn inspect_associated_run(
    state: &ServerState,
    key: &bcode_workflow_store::WorkflowRunBindingKey,
    limit: usize,
) -> Result<Option<Box<bcode_ipc::WorkflowRunInspection>>, super::ServerError> {
    let Some(run) = associated_run(state, key)? else {
        return Ok(None);
    };
    Ok(Some(Box::new(
        super::workflow_run_inspection(state, &run.run_id, limit).await?,
    )))
}

/// Control the workflow run associated with one plugin-owned binding.
pub async fn control_associated_run(
    state: &std::sync::Arc<ServerState>,
    key: &bcode_workflow_store::WorkflowRunBindingKey,
    action: bcode_ipc::WorkflowRunControlAction,
) -> Result<(Option<bcode_workflow_store::WorkflowRunSummary>, bool), super::ServerError> {
    let run = associated_run(state, key)?;
    let changed = if let Some(run) = &run {
        match action {
            bcode_ipc::WorkflowRunControlAction::Pause => pause_run(state, &run.run_id).await?,
            bcode_ipc::WorkflowRunControlAction::Resume => resume_run(state, &run.run_id).await?,
            bcode_ipc::WorkflowRunControlAction::Cancel => {
                let _authority = super::workflow_execution_authority(state, &run.run_id)
                    .await?
                    .ok_or_else(|| {
                        bcode_workflow_store::WorkflowStoreError::InvalidData(
                            "active workflow has no durable execution authority".to_string(),
                        )
                    })?;
                let (recorded, attempts) = {
                    let mut store = state
                        .workflow_store
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let recorded =
                        store.request_cancellation(&run.run_id, super::current_unix_millis())?;
                    let attempts = store.active_attempt_cancellations(&run.run_id, 1_000)?;
                    drop(store);
                    (recorded, attempts)
                };
                super::propagate_persisted_workflow_cancellation(state, attempts).await?;
                recorded
            }
        }
    } else {
        false
    };
    Ok((associated_run(state, key)?, changed))
}

/// Apply one explicit repair resolution to an exact workflow attempt.
pub fn repair_attempt(
    state: &ServerState,
    dispatch_identity: &str,
    resolution: &bcode_workflow_store::RepairResolution,
) -> Result<bcode_workflow_store::RepairResult, bcode_workflow_store::WorkflowStoreError> {
    let started_at = std::time::Instant::now();
    let result = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .repair_attempt(dispatch_identity, resolution, super::current_unix_millis())?;
    let resolution_label = match resolution {
        bcode_workflow_store::RepairResolution::ConfirmSucceeded { .. } => "confirm_succeeded",
        bcode_workflow_store::RepairResolution::ConfirmFailed { .. } => "confirm_failed",
        bcode_workflow_store::RepairResolution::ConfirmCancelled { .. } => "confirm_cancelled",
        bcode_workflow_store::RepairResolution::AbandonForExplicitRetry { .. } => {
            "abandon_for_explicit_retry"
        }
    };
    state.metrics.record_histogram_with_labels(
        "workflow.reconciliation.duration_ms",
        u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
        std::collections::BTreeMap::from([(
            "resolution".to_string(),
            resolution_label.to_string(),
        )]),
    );
    Ok(result)
}

/// Project a bounded workflow catalog page without transport framing.
pub fn catalog_view(
    state: &ServerState,
    request: &bcode_workflow_view_models::WorkflowCatalogRequest,
) -> Result<bcode_workflow_view_models::WorkflowCatalogView, bcode_workflow_store::WorkflowStoreError>
{
    let store = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let query = bcode_workflow_store::WorkflowRunCatalogQuery {
        limit: request.limit,
        cursor: request.cursor.as_ref().map(|cursor| {
            bcode_workflow_store::WorkflowRunCatalogCursor {
                sort: match cursor.sort {
                    bcode_workflow_view_models::WorkflowCatalogSort::UpdatedAt => {
                        bcode_workflow_store::WorkflowRunCatalogSort::UpdatedAt
                    }
                    bcode_workflow_view_models::WorkflowCatalogSort::CreatedAt => {
                        bcode_workflow_store::WorkflowRunCatalogSort::CreatedAt
                    }
                    bcode_workflow_view_models::WorkflowCatalogSort::Status => {
                        bcode_workflow_store::WorkflowRunCatalogSort::Status
                    }
                },
                timestamp_ms: cursor.timestamp_ms,
                status_rank: cursor.status_rank,
                run_id: cursor.run_id.clone(),
            }
        }),
        filter: match request.filter {
            bcode_workflow_view_models::WorkflowCatalogFilter::Active => {
                bcode_workflow_store::WorkflowRunCatalogFilter::Active
            }
            bcode_workflow_view_models::WorkflowCatalogFilter::NeedsAttention => {
                bcode_workflow_store::WorkflowRunCatalogFilter::NeedsAttention
            }
            bcode_workflow_view_models::WorkflowCatalogFilter::Failed => {
                bcode_workflow_store::WorkflowRunCatalogFilter::Failed
            }
            bcode_workflow_view_models::WorkflowCatalogFilter::Completed => {
                bcode_workflow_store::WorkflowRunCatalogFilter::Completed
            }
            bcode_workflow_view_models::WorkflowCatalogFilter::All => {
                bcode_workflow_store::WorkflowRunCatalogFilter::All
            }
        },
        sort: match request.sort {
            bcode_workflow_view_models::WorkflowCatalogSort::UpdatedAt => {
                bcode_workflow_store::WorkflowRunCatalogSort::UpdatedAt
            }
            bcode_workflow_view_models::WorkflowCatalogSort::CreatedAt => {
                bcode_workflow_store::WorkflowRunCatalogSort::CreatedAt
            }
            bcode_workflow_view_models::WorkflowCatalogSort::Status => {
                bcode_workflow_store::WorkflowRunCatalogSort::Status
            }
        },
        search: request.search.clone(),
    };
    let page = store.workflow_run_catalog_page(&query)?;
    let items = page
        .entries
        .iter()
        .map(|entry| super::workflow_run_list_item_with_summary(&store, &entry.run, &entry.summary))
        .collect::<Result<Vec<_>, _>>()?;
    drop(store);
    Ok(bcode_workflow_view::project_catalog(
        items,
        request,
        page.has_more,
    ))
}

/// Return the current global workflow-event sequence for subscription setup.
pub fn latest_event_sequence(
    state: &ServerState,
) -> Result<u64, bcode_workflow_store::WorkflowStoreError> {
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .latest_global_event_sequence()
}

/// Validate and persist one exact workflow definition without transport framing.
pub fn register_definition(
    state: &ServerState,
    request: &bcode_ipc::WorkflowDefinitionRegistrationRequest,
) -> Result<bcode_workflow_store::StoredWorkflowDefinition, super::ServerError> {
    super::validate_workflow_definition_for_production(state, &request.definition)?;
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .persist_definition(&request.definition_id, request.version, &request.definition)
        .map_err(super::ServerError::from)
}

/// Build the validated workflow authoring catalog without transport framing.
pub async fn authoring_catalog(
    state: &ServerState,
) -> Result<bcode_workflow::WorkflowAuthoringCatalogSnapshot, super::ServerError> {
    let plugins = state
        .plugins
        .registry()
        .manifests()
        .keys()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let blocks = state
        .plugins
        .registry()
        .workflow_blocks()
        .into_iter()
        .map(|block| (bcode_workflow::workflow_block_catalog_key(&block), block))
        .collect::<std::collections::BTreeMap<_, _>>();
    let authoring_actions = state
        .plugins
        .registry()
        .workflow_authoring_actions()
        .into_iter()
        .map(|action| (action.catalog_key(), action))
        .collect::<std::collections::BTreeMap<_, _>>();
    let profiles = super::list_profiles(state, None)
        .await
        .into_iter()
        .flat_map(|agent| std::iter::once(agent.id).chain(agent.aliases))
        .collect::<std::collections::BTreeSet<_>>();
    let workflow_definitions = {
        let store = state
            .workflow_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        store
            .list_definitions(1_000)?
            .into_iter()
            .map(|stored| {
                let definition: bcode_workflow::WorkflowDefinition =
                    serde_json::from_str(&stored.definition_json)?;
                Ok((stored.definition_id, definition))
            })
            .collect::<Result<
                std::collections::BTreeMap<_, _>,
                bcode_workflow_store::WorkflowStoreError,
            >>()?
    };
    let catalog = bcode_workflow::WorkflowAuthoringCatalogSnapshot {
        version: bcode_workflow::WORKFLOW_AUTHORING_CATALOG_VERSION,
        capabilities: bcode_workflow::WorkflowAuthoringCapabilitySummary::from(
            &bcode_workflow::WorkflowProductionCapabilities::current(),
        ),
        plugins,
        blocks,
        node_configuration_schemas: bcode_workflow::workflow_node_configuration_schemas(),
        workflow_definitions,
        agent_profiles: profiles,
        authoring_actions,
    };
    catalog
        .validate()
        .map_err(|error| super::ServerError::WorkflowCapabilityUnavailable(error.to_string()))?;
    Ok(catalog)
}

/// Describe one plugin-contributed workflow template and its current availability.
pub fn template_description(
    state: &ServerState,
    owner_plugin_id: &str,
    template: &bcode_plugin::WorkflowTemplateContribution,
) -> Result<bcode_ipc::WorkflowTemplateDescription, super::ServerError> {
    let loaded_plugins = state
        .plugins
        .registry()
        .manifests()
        .keys()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let capabilities = bcode_workflow::WorkflowProductionCapabilities::current();
    let mut diagnostics = Vec::new();
    for requirement in &template.required_plugins {
        if !loaded_plugins.contains(requirement) {
            diagnostics.push(bcode_ipc::WorkflowTemplateDiagnostic {
                code: "missing_plugin".to_string(),
                requirement: requirement.clone(),
                message: format!("required plugin '{requirement}' is not loaded"),
            });
        }
    }
    let supported_capabilities = std::collections::BTreeSet::from([
        format!("workflow-production/v{}", capabilities.capability_version),
        format!(
            "workflow-block/v{}",
            capabilities.workflow_block_interface_version
        ),
    ]);
    for requirement in &template.required_capabilities {
        if !supported_capabilities.contains(requirement) {
            diagnostics.push(bcode_ipc::WorkflowTemplateDiagnostic {
                code: "unsupported_capability".to_string(),
                requirement: requirement.clone(),
                message: format!("required capability '{requirement}' is unsupported"),
            });
        }
    }
    Ok(bcode_ipc::WorkflowTemplateDescription {
        owner_plugin_id: owner_plugin_id.to_string(),
        identity: template
            .definition_identity(owner_plugin_id)
            .map_err(|error| {
                bcode_workflow_store::WorkflowStoreError::InvalidData(error.to_string())
            })?,
        template: template.clone(),
        authoring_document: template.authoring_document().cloned(),
        diagnostics,
    })
}

/// Return bounded plugin-contributed workflow template descriptions.
pub fn list_templates(
    state: &ServerState,
    limit: usize,
) -> Result<Vec<bcode_ipc::WorkflowTemplateDescription>, super::ServerError> {
    if limit == 0 || limit > 1_000 {
        return Err(bcode_workflow_store::WorkflowStoreError::InvalidData(
            "workflow template limit must be in 1..=1000".to_string(),
        )
        .into());
    }
    state
        .plugins
        .registry()
        .workflow_templates()
        .into_iter()
        .take(limit)
        .map(|(owner, template)| template_description(state, owner, template))
        .collect()
}

/// Describe one exact plugin-contributed workflow template.
pub fn describe_template(
    state: &ServerState,
    owner_plugin_id: &str,
    template_id: &str,
    template_version: u32,
) -> Result<Option<Box<bcode_ipc::WorkflowTemplateDescription>>, super::ServerError> {
    state
        .plugins
        .registry()
        .workflow_templates()
        .into_iter()
        .find(|(owner, template)| {
            *owner == owner_plugin_id
                && template.template_id == template_id
                && template.template_version == template_version
        })
        .map(|(_, template)| template_description(state, owner_plugin_id, template))
        .transpose()
        .map(|template| template.map(Box::new))
}

/// Inspect one authored workflow revision against current requirement availability.
pub async fn revision_requirement_inspection(
    state: &ServerState,
    workflow_id: &str,
    revision: u64,
) -> Result<Option<bcode_ipc::WorkflowRevisionRequirementInspection>, super::ServerError> {
    let revision = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .workflow_revision(workflow_id, revision)?;
    let Some(revision) = revision else {
        return Ok(None);
    };
    let catalog = authoring_catalog(state).await?;
    let current_availability = bcode_workflow::workflow_requirement_availability(
        &revision.document.requirements,
        &catalog,
    )?;
    Ok(Some(bcode_ipc::WorkflowRevisionRequirementInspection {
        revision: Box::new(super::workflow_revision_snapshot(revision)),
        current_availability,
    }))
}

/// Authorize and create one workflow preset without transport framing.
pub fn create_preset(
    state: &std::sync::Arc<ServerState>,
    client_id: super::ClientId,
    request: bcode_ipc::CreateWorkflowPresetRequest,
) -> Result<bcode_workflow_store::WorkflowPreset, super::ServerError> {
    state.authorize_local_workflow_application_operation(
        client_id,
        super::LocalWorkflowApplicationOperationRequest {
            operation: bcode_workflow::WorkflowApplicationOperation::CreatePreset,
            workflow_id: request.preset.workflow_id.clone(),
            draft_id: None,
            revision: None,
            preset_id: Some(request.preset.preset_id.clone()),
            producer: Some(request.preset.producer.clone()),
            requirements: bcode_workflow::WorkflowRequirementSummary::default(),
            effects: bcode_workflow::WorkflowEffectSummary::default(),
            activates: false,
            executes: false,
        },
    )?;
    let preset = super::workflow_preset_from_mutation(request.preset, 1, super::current_time_ms());
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .create_workflow_preset(&preset)?;
    Ok(preset)
}

/// Authorize and update one workflow preset without transport framing.
pub fn update_preset(
    state: &std::sync::Arc<ServerState>,
    client_id: super::ClientId,
    request: bcode_ipc::UpdateWorkflowPresetRequest,
) -> Result<bcode_ipc::WorkflowPresetUpdateResult, super::ServerError> {
    state.authorize_local_workflow_application_operation(
        client_id,
        super::LocalWorkflowApplicationOperationRequest {
            operation: bcode_workflow::WorkflowApplicationOperation::UpdatePreset,
            workflow_id: request.preset.workflow_id.clone(),
            draft_id: None,
            revision: None,
            preset_id: Some(request.preset.preset_id.clone()),
            producer: Some(request.preset.producer.clone()),
            requirements: bcode_workflow::WorkflowRequirementSummary::default(),
            effects: bcode_workflow::WorkflowEffectSummary::default(),
            activates: false,
            executes: false,
        },
    )?;
    let preset = request.preset;
    let update = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .update_workflow_preset(
            &preset.workflow_id,
            &preset.preset_id,
            request.expected_generation,
            &preset.name,
            &preset.configuration,
            preset.run_limits.as_ref(),
            &preset.producer,
            super::current_time_ms(),
        );
    match update {
        Ok(preset) => Ok(bcode_ipc::WorkflowPresetUpdateResult::Updated(
            super::workflow_preset_snapshot(preset),
        )),
        Err(error) => {
            super::record_workflow_authoring_conflict(&state.metrics, "update_preset");
            Ok(bcode_ipc::WorkflowPresetUpdateResult::Conflict(
                super::authoring_conflict_result(error)?,
            ))
        }
    }
}

/// Authorize and delete one workflow preset without transport framing.
pub fn delete_preset(
    state: &std::sync::Arc<ServerState>,
    client_id: super::ClientId,
    request: &bcode_ipc::DeleteWorkflowPresetRequest,
) -> Result<bcode_ipc::WorkflowAuthoringMutationResult, super::ServerError> {
    state.authorize_local_workflow_application_operation(
        client_id,
        super::LocalWorkflowApplicationOperationRequest {
            operation: bcode_workflow::WorkflowApplicationOperation::DeletePreset,
            workflow_id: request.workflow_id.clone(),
            draft_id: None,
            revision: None,
            preset_id: Some(request.preset_id.clone()),
            producer: None,
            requirements: bcode_workflow::WorkflowRequirementSummary::default(),
            effects: bcode_workflow::WorkflowEffectSummary::default(),
            activates: false,
            executes: false,
        },
    )?;
    let deletion = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .delete_workflow_preset(
            &request.workflow_id,
            &request.preset_id,
            request.expected_generation,
        );
    match deletion {
        Ok(()) => Ok(bcode_ipc::WorkflowAuthoringMutationResult::Applied),
        Err(error) => {
            super::record_workflow_authoring_conflict(&state.metrics, "delete_preset");
            Ok(bcode_ipc::WorkflowAuthoringMutationResult::Conflict(
                super::authoring_conflict_result(error)?,
            ))
        }
    }
}

/// Authorize and archive or unarchive one authored workflow.
pub fn set_archived(
    state: &std::sync::Arc<ServerState>,
    client_id: super::ClientId,
    request: &bcode_ipc::SetAuthoredWorkflowArchivedRequest,
) -> Result<bcode_workflow_store::AuthoredWorkflow, super::ServerError> {
    state.authorize_local_workflow_application_operation(
        client_id,
        super::LocalWorkflowApplicationOperationRequest {
            operation: if request.archived {
                bcode_workflow::WorkflowApplicationOperation::ArchiveWorkflow
            } else {
                bcode_workflow::WorkflowApplicationOperation::UnarchiveWorkflow
            },
            workflow_id: request.workflow_id.clone(),
            draft_id: None,
            revision: None,
            preset_id: None,
            producer: None,
            requirements: bcode_workflow::WorkflowRequirementSummary::default(),
            effects: bcode_workflow::WorkflowEffectSummary::default(),
            activates: false,
            executes: false,
        },
    )?;
    let mut store = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    store.set_authored_workflow_archived(
        &request.workflow_id,
        request.archived,
        super::current_time_ms(),
    )?;
    Ok(store
        .authored_workflow(&request.workflow_id)?
        .expect("updated authored workflow remains present"))
}

/// Authorize and activate one exact authored workflow revision.
pub fn activate_revision(
    state: &std::sync::Arc<ServerState>,
    client_id: super::ClientId,
    request: &bcode_ipc::ActivateWorkflowRevisionRequest,
) -> Result<bcode_ipc::WorkflowAuthoringMutationResult, super::ServerError> {
    state.authorize_local_workflow_application_operation(
        client_id,
        super::LocalWorkflowApplicationOperationRequest {
            operation: bcode_workflow::WorkflowApplicationOperation::ActivateRevision,
            workflow_id: request.workflow_id.clone(),
            draft_id: None,
            revision: Some(request.revision),
            preset_id: None,
            producer: None,
            requirements: bcode_workflow::WorkflowRequirementSummary::default(),
            effects: bcode_workflow::WorkflowEffectSummary::default(),
            activates: true,
            executes: false,
        },
    )?;
    let update = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .set_active_workflow_revision(
            &request.workflow_id,
            request.expected_active_revision,
            request.revision,
            super::current_time_ms(),
        );
    match update {
        Ok(()) => Ok(bcode_ipc::WorkflowAuthoringMutationResult::Applied),
        Err(error) => {
            super::record_workflow_authoring_conflict(&state.metrics, "activate");
            Ok(bcode_ipc::WorkflowAuthoringMutationResult::Conflict(
                super::authoring_conflict_result(error)?,
            ))
        }
    }
}

/// Authorize and discard one exact workflow draft.
pub fn discard_draft(
    state: &std::sync::Arc<ServerState>,
    client_id: super::ClientId,
    request: &bcode_ipc::DiscardWorkflowDraftRequest,
) -> Result<bcode_ipc::WorkflowAuthoringMutationResult, super::ServerError> {
    state.authorize_local_workflow_application_operation(
        client_id,
        super::LocalWorkflowApplicationOperationRequest {
            operation: bcode_workflow::WorkflowApplicationOperation::DiscardDraft,
            workflow_id: request.workflow_id.clone(),
            draft_id: Some(request.draft_id.clone()),
            revision: None,
            preset_id: None,
            producer: None,
            requirements: bcode_workflow::WorkflowRequirementSummary::default(),
            effects: bcode_workflow::WorkflowEffectSummary::default(),
            activates: false,
            executes: false,
        },
    )?;
    let discard = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .discard_workflow_draft(
            &request.workflow_id,
            &request.draft_id,
            request.expected_generation,
        );
    match discard {
        Ok(()) => Ok(bcode_ipc::WorkflowAuthoringMutationResult::Applied),
        Err(error) => {
            super::record_workflow_authoring_conflict(&state.metrics, "discard_draft");
            Ok(bcode_ipc::WorkflowAuthoringMutationResult::Conflict(
                super::authoring_conflict_result(error)?,
            ))
        }
    }
}

/// Authorize and fork one workflow draft or revision into a new draft.
pub fn fork_draft(
    state: &std::sync::Arc<ServerState>,
    client_id: super::ClientId,
    request: bcode_ipc::ForkWorkflowDraftRequest,
) -> Result<bcode_workflow_store::WorkflowDraft, super::ServerError> {
    state.authorize_local_workflow_application_operation(
        client_id,
        super::LocalWorkflowApplicationOperationRequest {
            operation: bcode_workflow::WorkflowApplicationOperation::ForkDraft,
            workflow_id: request.workflow_id.clone(),
            draft_id: Some(request.draft_id.clone()),
            revision: None,
            preset_id: None,
            producer: Some(request.producer.clone()),
            requirements: bcode_workflow::WorkflowRequirementSummary::default(),
            effects: bcode_workflow::WorkflowEffectSummary::default(),
            activates: false,
            executes: false,
        },
    )?;
    let mut store = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match request.source {
        bcode_ipc::WorkflowDraftForkSource::Draft { draft_id } => store
            .fork_workflow_draft(
                &request.workflow_id,
                &draft_id,
                &request.draft_id,
                request.producer,
                super::current_time_ms(),
            )
            .map_err(super::ServerError::from),
        bcode_ipc::WorkflowDraftForkSource::Revision { revision } => store
            .fork_workflow_revision(
                &request.workflow_id,
                revision,
                &request.draft_id,
                request.producer,
                super::current_time_ms(),
            )
            .map_err(super::ServerError::from),
    }
}

/// Authorize and create one authored workflow with its initial draft.
pub fn create_authored_workflow(
    state: &std::sync::Arc<ServerState>,
    client_id: super::ClientId,
    request: bcode_ipc::CreateAuthoredWorkflowRequest,
) -> Result<
    (
        bcode_workflow_store::AuthoredWorkflow,
        bcode_workflow_store::WorkflowDraft,
    ),
    super::ServerError,
> {
    request.document.validate()?;
    let workflow_id = request.document.workflow_id.clone();
    let producer = request.document.producer.clone();
    state.authorize_local_workflow_application_operation(
        client_id,
        super::LocalWorkflowApplicationOperationRequest {
            operation: bcode_workflow::WorkflowApplicationOperation::CreateWorkflow,
            workflow_id: workflow_id.clone(),
            draft_id: None,
            revision: None,
            preset_id: None,
            producer: Some(producer.clone()),
            requirements: request.document.requirements.clone(),
            effects: bcode_workflow::WorkflowEffectSummary::default(),
            activates: false,
            executes: false,
        },
    )?;
    let now = super::current_time_ms();
    let workflow = bcode_workflow_store::AuthoredWorkflow {
        workflow_id: workflow_id.clone(),
        title: request.document.metadata.title.clone(),
        description: request.document.metadata.description.clone(),
        archived: false,
        active_revision: None,
        created_at_ms: now,
        updated_at_ms: now,
    };
    let draft = bcode_workflow_store::WorkflowDraft {
        workflow_id,
        draft_id: request.draft_id,
        base_revision: None,
        generation: 1,
        checksum_sha256: request.document.source_digest_sha256()?,
        document: request.document,
        producer,
        created_at_ms: now,
        updated_at_ms: now,
    };
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .create_authored_workflow_with_initial_draft(&workflow, &draft)?;
    Ok((workflow, draft))
}

/// Authorize and update one complete workflow draft document.
pub fn update_draft(
    state: &std::sync::Arc<ServerState>,
    client_id: super::ClientId,
    request: &bcode_ipc::UpdateWorkflowDraftRequest,
) -> Result<bcode_ipc::WorkflowDraftUpdateResult, super::ServerError> {
    request.document.validate()?;
    if request.document.workflow_id != request.workflow_id {
        return Err(super::ServerError::WorkflowDefinitionUnsupported(
            "draft document workflow identity does not match the request".to_string(),
        ));
    }
    state.authorize_local_workflow_application_operation(
        client_id,
        super::LocalWorkflowApplicationOperationRequest {
            operation: bcode_workflow::WorkflowApplicationOperation::UpdateDraft,
            workflow_id: request.workflow_id.clone(),
            draft_id: Some(request.draft_id.clone()),
            revision: None,
            preset_id: None,
            producer: Some(request.producer.clone()),
            requirements: request.document.requirements.clone(),
            effects: bcode_workflow::WorkflowEffectSummary::default(),
            activates: false,
            executes: false,
        },
    )?;
    let update = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .update_workflow_draft(
            &request.workflow_id,
            &request.draft_id,
            request.expected_generation,
            &request.document,
            &request.producer,
            super::current_time_ms(),
        );
    match update {
        Ok(draft) => Ok(bcode_ipc::WorkflowDraftUpdateResult::Updated(Box::new(
            super::workflow_draft_snapshot(draft),
        ))),
        Err(bcode_workflow_store::WorkflowStoreError::AuthoringConflict {
            entity_id,
            expected,
            current,
        }) => {
            super::record_workflow_authoring_conflict(&state.metrics, "update_draft");
            Ok(bcode_ipc::WorkflowDraftUpdateResult::Conflict(
                bcode_ipc::WorkflowAuthoringConflict {
                    entity_id,
                    expected_generation: expected,
                    current_generation: current,
                },
            ))
        }
        Err(error) => Err(error.into()),
    }
}

/// Apply one validated semantic edit batch to an exact workflow draft generation.
pub fn apply_draft_edits(
    state: &std::sync::Arc<ServerState>,
    client_id: super::ClientId,
    request: bcode_ipc::ApplyWorkflowDraftEditsRequest,
) -> Result<bcode_ipc::WorkflowDraftEditResult, super::ServerError> {
    request.batch.validate()?;
    let current = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .workflow_draft(&request.workflow_id, &request.draft_id)?
        .ok_or_else(|| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                "workflow draft not found: {}/{}",
                request.workflow_id, request.draft_id
            ))
        })?;
    if current.generation != request.batch.expected_generation {
        return Ok(bcode_ipc::WorkflowDraftEditResult::Conflict(
            bcode_ipc::WorkflowAuthoringConflict {
                entity_id: request.draft_id,
                expected_generation: request.batch.expected_generation,
                current_generation: current.generation,
            },
        ));
    }
    let document = match bcode_workflow::apply_workflow_authoring_edits(
        &current.document,
        &request.batch,
    ) {
        Ok(document) => document,
        Err(bcode_workflow::WorkflowError::Build { path, message }) => {
            return Ok(bcode_ipc::WorkflowDraftEditResult::Rejected {
                diagnostics: vec![bcode_workflow::WorkflowValidationDiagnostic {
                    code: "semantic_edit_rejected".to_string(),
                    severity: bcode_workflow::WorkflowValidationSeverity::Error,
                    document_path: path,
                    message,
                    remediation: "Revise the addressed semantic edit and retry against the same draft generation."
                        .to_string(),
                }],
            });
        }
        Err(error) => {
            return Err(super::ServerError::WorkflowDefinitionUnsupported(
                error.to_string(),
            ));
        }
    };
    state.authorize_local_workflow_application_operation(
        client_id,
        super::LocalWorkflowApplicationOperationRequest {
            operation: bcode_workflow::WorkflowApplicationOperation::UpdateDraft,
            workflow_id: request.workflow_id.clone(),
            draft_id: Some(request.draft_id.clone()),
            revision: None,
            preset_id: None,
            producer: Some(request.producer.clone()),
            requirements: document.requirements.clone(),
            effects: bcode_workflow::WorkflowEffectSummary::default(),
            activates: false,
            executes: false,
        },
    )?;
    let update = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .update_workflow_draft(
            &request.workflow_id,
            &request.draft_id,
            request.batch.expected_generation,
            &document,
            &request.producer,
            super::current_time_ms(),
        );
    match update {
        Ok(draft) => Ok(bcode_ipc::WorkflowDraftEditResult::Updated(Box::new(
            super::workflow_draft_snapshot(draft),
        ))),
        Err(bcode_workflow_store::WorkflowStoreError::AuthoringConflict {
            entity_id,
            expected,
            current,
        }) => Ok(bcode_ipc::WorkflowDraftEditResult::Conflict(
            bcode_ipc::WorkflowAuthoringConflict {
                entity_id,
                expected_generation: expected,
                current_generation: current,
            },
        )),
        Err(error) => Err(error.into()),
    }
}

/// Convert one stored published revision into its portable representation.
#[must_use]
pub fn portable_revision(
    revision: bcode_workflow_store::PublishedWorkflowRevision,
) -> bcode_workflow::WorkflowPortableRevision {
    bcode_workflow::WorkflowPortableRevision {
        identity: bcode_workflow::WorkflowRevisionIdentity {
            workflow_id: revision.workflow_id,
            revision: revision.revision,
        },
        source_checksum_sha256: revision.source_checksum_sha256,
        executable_source_checksum_sha256: revision.executable_source_checksum_sha256,
        definition_identity: revision.definition_identity,
        document: revision.document,
        producer: revision.producer,
        published_at_ms: revision.published_at_ms,
    }
}

/// Export one exact published workflow revision as a validated portable bundle.
pub fn export_revision(
    state: &ServerState,
    request: &bcode_ipc::ExportWorkflowRevisionRequest,
) -> Result<bcode_workflow::WorkflowExportBundle, super::ServerError> {
    let revision = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .workflow_revision(&request.workflow_id, request.revision)?
        .ok_or_else(|| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                "published workflow revision not found: {} v{}",
                request.workflow_id, request.revision
            ))
        })?;
    let dependencies = bcode_workflow::workflow_dependency_manifest(&revision.document.definition)?;
    let bundle = bcode_workflow::WorkflowExportBundle {
        version: bcode_workflow::WORKFLOW_EXPORT_BUNDLE_VERSION,
        revision: portable_revision(revision),
        dependencies,
    };
    bundle.validate()?;
    Ok(bundle)
}

/// Validate and preview one portable workflow import without transport framing.
pub async fn import_preview(
    state: &ServerState,
    operation_id: String,
    bundle: bcode_workflow::WorkflowExportBundle,
    target_workflow_id: String,
    control: bcode_ipc::WorkflowComputationControl,
) -> Result<
    (
        bcode_workflow::WorkflowImportPreview,
        bcode_workflow::WorkflowAuthoringDocument,
    ),
    super::ServerError,
> {
    bundle.validate()?;
    let mut document = bundle.revision.document.clone();
    document.workflow_id.clone_from(&target_workflow_id);
    document.producer = bcode_workflow::WorkflowProducerProvenance {
        kind: bcode_workflow::WorkflowProducerKind::Generated,
        producer_id: Some("workflow-import".to_string()),
        source_revision: Some(bundle.revision.identity.clone()),
    };
    let catalog = authoring_catalog(state).await?;
    let preview_document = document.clone();
    let started_at = std::time::Instant::now();
    let compilation = super::run_workflow_computation(state, control, operation_id, move || {
        preview_document.compilation_preview(&catalog, None)
    })
    .await?;
    super::record_workflow_authoring_duration(
        &state.metrics,
        "workflow.authoring.import_preview.duration_ms",
        started_at,
        if compilation.compiled.is_some() {
            "accepted"
        } else {
            "rejected"
        },
    );
    Ok((
        bcode_workflow::WorkflowImportPreview {
            version: bcode_workflow::WORKFLOW_IMPORT_PREVIEW_VERSION,
            bundle_version: bundle.version,
            source_identity: bundle.revision.identity,
            target_workflow_id,
            compilation,
        },
        document,
    ))
}

/// Persist a validated import as a new authored workflow and initial draft.
pub async fn import_new_workflow(
    state: &std::sync::Arc<ServerState>,
    client_id: super::ClientId,
    operation_id: String,
    request: bcode_ipc::ImportWorkflowRequest,
) -> Result<
    (
        bcode_workflow_store::AuthoredWorkflow,
        bcode_workflow_store::WorkflowDraft,
    ),
    super::ServerError,
> {
    if request.collision_policy != bcode_ipc::WorkflowImportCollisionPolicy::RequireNewWorkflow {
        return Err(bcode_workflow_store::WorkflowStoreError::InvalidData(
            "new-workflow import requires require_new_workflow collision policy".to_string(),
        )
        .into());
    }
    let (preview, document) = import_preview(
        state,
        operation_id,
        request.bundle,
        request.target_workflow_id.clone(),
        request.control,
    )
    .await?;
    let compiled = preview.compilation.compiled.as_ref().ok_or_else(|| {
        super::ServerError::WorkflowDefinitionUnsupported(
            "import requires a successful compilation preview".to_string(),
        )
    })?;
    state.authorize_local_workflow_application_operation(
        client_id,
        super::LocalWorkflowApplicationOperationRequest {
            operation: bcode_workflow::WorkflowApplicationOperation::ImportWorkflow,
            workflow_id: request.target_workflow_id.clone(),
            draft_id: None,
            revision: None,
            preset_id: None,
            producer: Some(document.producer.clone()),
            requirements: compiled.requirements.clone(),
            effects: compiled.effects.clone(),
            activates: false,
            executes: false,
        },
    )?;
    let now = super::current_time_ms();
    let workflow = bcode_workflow_store::AuthoredWorkflow {
        workflow_id: request.target_workflow_id.clone(),
        title: document.metadata.title.clone(),
        description: document.metadata.description.clone(),
        archived: false,
        active_revision: None,
        created_at_ms: now,
        updated_at_ms: now,
    };
    let draft = bcode_workflow_store::WorkflowDraft {
        workflow_id: request.target_workflow_id,
        draft_id: request.draft_id,
        base_revision: None,
        generation: 1,
        checksum_sha256: document.source_digest_sha256()?,
        producer: document.producer.clone(),
        document,
        created_at_ms: now,
        updated_at_ms: now,
    };
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .create_authored_workflow_with_initial_draft(&workflow, &draft)?;
    Ok((workflow, draft))
}

/// Persist a validated import as a new draft on an existing authored workflow.
pub async fn import_draft(
    state: &std::sync::Arc<ServerState>,
    client_id: super::ClientId,
    operation_id: String,
    request: bcode_ipc::ImportWorkflowDraftRequest,
) -> Result<bcode_ipc::WorkflowDraftImportResult, super::ServerError> {
    if request.collision_policy
        != bcode_ipc::WorkflowImportCollisionPolicy::RequireExistingWorkflowNewDraft
    {
        return Err(bcode_workflow_store::WorkflowStoreError::InvalidData(
            "existing-workflow import requires require_existing_workflow_new_draft collision policy"
                .to_string(),
        )
        .into());
    }
    let (preview, document) = import_preview(
        state,
        operation_id,
        request.bundle,
        request.workflow_id.clone(),
        request.control,
    )
    .await?;
    let compiled = preview.compilation.compiled.as_ref().ok_or_else(|| {
        super::ServerError::WorkflowDefinitionUnsupported(
            "import requires a successful compilation preview".to_string(),
        )
    })?;
    state.authorize_local_workflow_application_operation(
        client_id,
        super::LocalWorkflowApplicationOperationRequest {
            operation: bcode_workflow::WorkflowApplicationOperation::ImportDraft,
            workflow_id: request.workflow_id.clone(),
            draft_id: Some(request.draft_id.clone()),
            revision: None,
            preset_id: None,
            producer: Some(document.producer.clone()),
            requirements: compiled.requirements.clone(),
            effects: compiled.effects.clone(),
            activates: false,
            executes: false,
        },
    )?;
    let now = super::current_time_ms();
    let draft = bcode_workflow_store::WorkflowDraft {
        workflow_id: request.workflow_id.clone(),
        draft_id: request.draft_id.clone(),
        base_revision: None,
        generation: 1,
        checksum_sha256: document.source_digest_sha256()?,
        producer: document.producer.clone(),
        document,
        created_at_ms: now,
        updated_at_ms: now,
    };
    let mut store = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let workflow = store
        .authored_workflow(&request.workflow_id)?
        .ok_or_else(|| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                "authored workflow not found: {}",
                request.workflow_id
            ))
        })?;
    if store
        .workflow_draft(&request.workflow_id, &request.draft_id)?
        .is_some()
    {
        return Ok(bcode_ipc::WorkflowDraftImportResult::DraftAlreadyExists {
            workflow_id: request.workflow_id,
            draft_id: request.draft_id,
        });
    }
    let created = store.create_workflow_draft(&draft)?;
    drop(store);
    assert!(
        created,
        "draft absence was checked while holding the store lock"
    );
    Ok(bcode_ipc::WorkflowDraftImportResult::Imported {
        workflow: super::authored_workflow_snapshot(workflow),
        draft: Box::new(super::workflow_draft_snapshot(draft)),
    })
}

/// Return bounded workflow run summaries without transport framing.
pub fn list_runs(
    state: &ServerState,
    limit: usize,
) -> Result<Vec<bcode_workflow_store::WorkflowRunSummary>, bcode_workflow_store::WorkflowStoreError>
{
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .list_runs(limit)
}

/// Return checksum-verified bounded workflow outputs without transport framing.
pub fn run_outputs(
    state: &ServerState,
    run_id: &str,
    limit: usize,
) -> Result<Vec<bcode_ipc::WorkflowOutputInspection>, bcode_workflow_store::WorkflowStoreError> {
    let store = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let checksums = store
        .output_summaries(run_id, limit)?
        .into_iter()
        .map(|output| (output.output_id, output.checksum_sha256))
        .collect::<std::collections::BTreeMap<_, _>>();
    Ok(store
        .validated_outputs(run_id, limit)?
        .into_iter()
        .map(|output| bcode_ipc::WorkflowOutputInspection {
            version: bcode_ipc::WORKFLOW_OUTPUT_INSPECTION_VERSION,
            checksum_sha256: checksums
                .get(&output.output_id)
                .cloned()
                .expect("validated output has a matching bounded summary"),
            output_id: output.output_id,
            run_id: output.run_id,
            node_id: output.node_id,
            activation_id: output.activation_id,
            schema_id: output.schema_id,
            schema_version: output.schema_version,
            value: output.value,
            artifact_reference: output.artifact_reference,
            created_at_ms: output.created_at_ms,
        })
        .collect())
}

/// Return one bounded workflow run inspection without transport framing.
pub async fn inspect_run(
    state: &ServerState,
    run_id: &str,
    limit: usize,
) -> Result<bcode_ipc::WorkflowRunInspection, super::ServerError> {
    super::workflow_run_inspection(state, run_id, limit).await
}

/// Return bounded workflow definitions without transport framing.
pub fn list_definitions(
    state: &ServerState,
    limit: usize,
) -> Result<
    Vec<bcode_workflow_store::StoredWorkflowDefinition>,
    bcode_workflow_store::WorkflowStoreError,
> {
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .list_definitions(limit)
}

/// Return one versioned workflow definition without transport framing.
pub fn describe_definition(
    state: &ServerState,
    definition_id: &str,
    version: u32,
) -> Result<
    Option<bcode_workflow_store::StoredWorkflowDefinition>,
    bcode_workflow_store::WorkflowStoreError,
> {
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .definition(definition_id, version)
}
