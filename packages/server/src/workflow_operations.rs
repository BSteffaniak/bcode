#[derive(Clone)]
pub struct WorkflowApplicationAuthorizationPolicy {
    pub evaluator: std::sync::Arc<
        dyn Fn(
                &bcode_workflow::WorkflowApplicationOperationFacts,
            ) -> WorkflowApplicationAuthorizationDecision
            + Send
            + Sync,
    >,
}

impl std::fmt::Debug for WorkflowApplicationAuthorizationPolicy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkflowApplicationAuthorizationPolicy")
            .finish_non_exhaustive()
    }
}

impl WorkflowApplicationAuthorizationPolicy {
    pub fn evaluate(
        &self,
        facts: &bcode_workflow::WorkflowApplicationOperationFacts,
    ) -> WorkflowApplicationAuthorizationDecision {
        (self.evaluator)(facts)
    }
}

/// Decision returned by the daemon application-operation authorization boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowApplicationAuthorizationDecision {
    /// The normalized operation may proceed to its side-effect boundary.
    Allow,
    /// The normalized operation is denied with a secret-safe reason.
    Deny { reason: String },
}

pub fn authorize_local_workflow_application_operation(
    facts: &bcode_workflow::WorkflowApplicationOperationFacts,
) -> WorkflowApplicationAuthorizationDecision {
    match facts.actor.kind {
        bcode_workflow::WorkflowApplicationActorKind::LocalClient => {
            WorkflowApplicationAuthorizationDecision::Allow
        }
        bcode_workflow::WorkflowApplicationActorKind::Plugin => {
            WorkflowApplicationAuthorizationDecision::Deny {
                reason: "plugins require an explicitly registered authored-workflow application capability"
                    .to_string(),
            }
        }
        bcode_workflow::WorkflowApplicationActorKind::Service => {
            WorkflowApplicationAuthorizationDecision::Deny {
                reason: "services require an explicitly configured authored-workflow maintenance policy"
                    .to_string(),
            }
        }
    }
}

/// Server-owned input used to derive authenticated local-client operation facts.
pub struct LocalApplicationOperationRequest {
    pub operation: bcode_workflow::WorkflowApplicationOperation,
    pub workflow_id: String,
    pub draft_id: Option<String>,
    pub revision: Option<u64>,
    pub preset_id: Option<String>,
    pub producer: Option<bcode_workflow::WorkflowProducerProvenance>,
    pub requirements: bcode_workflow::WorkflowRequirementSummary,
    pub effects: bcode_workflow::WorkflowEffectSummary,
    pub activates: bool,
    pub executes: bool,
}

use super::{ClientEventSink, ServerState};
use std::collections::BTreeMap;

/// Resolve and start one exact published package export without renderer coupling.
///
/// # Errors
///
/// Returns an error for malformed selection, missing or drifted publication facts, an unpublished
/// export, or authored-workflow admission failure.
pub async fn start_package_export(
    client_id: super::ClientId,
    state: &std::sync::Arc<ServerState>,
    request: bcode_ipc::StartWorkflowPackageExportRequest,
) -> Result<bcode_ipc::WorkflowPackageExportRunStartResponse, super::ServerError> {
    request.package_export.validate()?;
    let receipt = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .workflow_package_publication(
            &request.package_export.package_id,
            request.package_export.package_lock_digest_sha256.as_deref(),
        )?
        .ok_or_else(|| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                "published workflow package not found: {}",
                request.package_export.package_id
            ))
        })?;
    let exported = receipt
        .exports
        .iter()
        .find(|export| export.export == request.package_export.export)
        .cloned()
        .ok_or_else(|| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                "published workflow package export not found: {}/{}",
                request.package_export.package_id, request.package_export.export
            ))
        })?;
    let revision = exported.published_revision.as_ref().ok_or_else(|| {
        bcode_workflow_store::WorkflowStoreError::InvalidData(
            "published workflow package export has no exact revision".to_string(),
        )
    })?;
    let started = start_authored(
        client_id,
        state,
        bcode_ipc::StartAuthoredWorkflowRequest {
            selection: bcode_ipc::AuthoredWorkflowRunSelection::Revision {
                workflow_id: revision.workflow_id.clone(),
                revision: revision.revision,
            },
            run_id: request.run_id,
            parent_session_id: request.parent_session_id,
            workspace_snapshot: request.workspace_snapshot,
            parent_session_generation: request.parent_session_generation,
            configuration: request.configuration,
            input: request.input,
        },
        true,
    )
    .await?;
    Ok(bcode_ipc::WorkflowPackageExportRunStartResponse {
        package_export: request.package_export,
        package_lock_digest_sha256: receipt.package_lock_digest_sha256,
        exported,
        started,
    })
}

#[derive(Debug)]
pub struct ComputationCancellation {
    cancelled: std::sync::atomic::AtomicBool,
    notify: tokio::sync::Notify,
}

impl ComputationCancellation {
    pub fn new() -> Self {
        Self {
            cancelled: std::sync::atomic::AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
        }
    }

    pub fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
        self.notify.notify_waiters();
    }
}

pub fn record_authoring_duration(
    metrics: &bcode_metrics::MetricsRegistry,
    metric: &'static str,
    started_at: std::time::Instant,
    outcome: &'static str,
) {
    metrics.record_histogram_with_exact_labels(
        metric,
        u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
        BTreeMap::from([("outcome".to_string(), outcome.to_string())]),
    );
}

pub fn record_authoring_conflict(
    metrics: &bcode_metrics::MetricsRegistry,
    operation: &'static str,
) {
    metrics.add_counter_with_exact_labels(
        "workflow.authoring.conflicts_total",
        1,
        BTreeMap::from([("operation".to_string(), operation.to_string())]),
    );
}

pub async fn run_computation<T, F>(
    state: &ServerState,
    control: bcode_ipc::WorkflowComputationControl,
    fallback_operation_id: String,
    compute: F,
) -> Result<T, super::ServerError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let operation_id = if control.operation_id.is_empty() {
        fallback_operation_id
    } else {
        control.operation_id
    };
    if operation_id.len() > 512
        || operation_id.is_empty()
        || control.timeout_ms == 0
        || control.timeout_ms > bcode_ipc::MAX_WORKFLOW_COMPUTATION_TIMEOUT_MS
    {
        return Err(super::ServerError::WorkflowComputationControlInvalid(
            "operation identity or timeout is outside supported bounds".to_string(),
        ));
    }
    let cancellation = std::sync::Arc::new(ComputationCancellation::new());
    {
        let mut computations = state
            .workflow_computations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if computations.contains_key(&operation_id) {
            return Err(super::ServerError::WorkflowComputationControlInvalid(
                format!("workflow computation operation is already active: {operation_id}"),
            ));
        }
        computations.insert(operation_id.clone(), std::sync::Arc::clone(&cancellation));
    }
    let mut task = tokio::task::spawn_blocking(compute);
    let outcome = tokio::select! {
        result = &mut task => result.map_err(super::ServerError::BlockingTask),
        () = cancellation.notify.notified() => {
            Err(super::ServerError::WorkflowComputationCancelled(operation_id.clone()))
        }
        () = tokio::time::sleep(std::time::Duration::from_millis(control.timeout_ms)) => {
            cancellation.cancel();
            Err(super::ServerError::WorkflowComputationTimedOut(operation_id.clone()))
        }
    };
    state
        .workflow_computations
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&operation_id);
    outcome
}

pub fn cancel_computation(state: &ServerState, operation_id: &str) -> bool {
    let cancellation = state
        .workflow_computations
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(operation_id)
        .cloned();
    cancellation.is_some_and(|cancellation| {
        cancellation.cancel();
        true
    })
}

pub fn authoring_conflict_result(
    error: bcode_workflow_store::WorkflowStoreError,
) -> Result<bcode_ipc::WorkflowAuthoringConflict, bcode_workflow_store::WorkflowStoreError> {
    match error {
        bcode_workflow_store::WorkflowStoreError::AuthoringConflict {
            entity_id,
            expected,
            current,
        } => Ok(bcode_ipc::WorkflowAuthoringConflict {
            entity_id,
            expected_generation: expected,
            current_generation: current,
        }),
        error => Err(error),
    }
}

fn workflow_preset_from_mutation(
    mutation: bcode_ipc::WorkflowPresetMutation,
    generation: u64,
    created_at_ms: u64,
) -> bcode_workflow_store::WorkflowPreset {
    bcode_workflow_store::WorkflowPreset {
        workflow_id: mutation.workflow_id,
        preset_id: mutation.preset_id,
        revision: mutation.revision,
        name: mutation.name,
        generation,
        configuration: mutation.configuration,
        run_limits: mutation.run_limits,
        producer: mutation.producer,
        created_at_ms,
        updated_at_ms: created_at_ms,
    }
}

pub async fn import_revision(
    fallback_operation_id: String,
    client_id: super::ClientId,
    state: &std::sync::Arc<ServerState>,
    request: bcode_ipc::ImportWorkflowRevisionRequest,
) -> Result<bcode_ipc::WorkflowRevisionImportResult, super::ServerError> {
    if request.collision_policy
        != bcode_ipc::WorkflowImportCollisionPolicy::RequireExistingWorkflowNextRevision
    {
        return Err(bcode_workflow_store::WorkflowStoreError::InvalidData(
            "revision import requires require_existing_workflow_next_revision collision policy"
                .to_string(),
        )
        .into());
    }
    let (preview, document) = import_preview(
        state,
        fallback_operation_id,
        request.bundle,
        request.workflow_id.clone(),
        request.control,
    )
    .await?;
    let compiled = preview.compilation.compiled.as_ref().ok_or_else(|| {
        super::ServerError::WorkflowDefinitionUnsupported(
            "revision import requires a successful compilation preview".to_string(),
        )
    })?;
    state.authorize_local_workflow_application_operation(
        client_id,
        LocalApplicationOperationRequest {
            operation: bcode_workflow::WorkflowApplicationOperation::ImportRevision,
            workflow_id: request.workflow_id.clone(),
            draft_id: Some(format!("import-revision-{}", request.revision)),
            revision: None,
            preset_id: None,
            producer: Some(document.producer.clone()),
            requirements: compiled.requirements.clone(),
            effects: compiled.effects.clone(),
            activates: request.activate,
            executes: false,
        },
    )?;
    let publication = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .import_workflow_revision(
            &request.workflow_id,
            request.revision,
            &document,
            &document.producer,
            &preview.compilation,
            request.activate,
            request.expected_active_revision,
            super::current_time_ms(),
        );
    let result = match publication {
        Ok(publication) => bcode_ipc::WorkflowRevisionImportResult::Imported {
            revision: Box::new(workflow_revision_snapshot(publication.revision)),
            active_revision: publication.active_revision,
        },
        Err(error @ bcode_workflow_store::WorkflowStoreError::AuthoringConflict { .. }) => {
            bcode_ipc::WorkflowRevisionImportResult::Conflict(authoring_conflict_result(error)?)
        }
        Err(error) => return Err(error.into()),
    };
    Ok(result)
}

pub async fn publish_draft(
    fallback_operation_id: String,
    client_id: super::ClientId,
    state: &std::sync::Arc<ServerState>,
    request: bcode_ipc::PublishWorkflowDraftRequest,
    operation: bcode_workflow::WorkflowApplicationOperation,
) -> Result<bcode_ipc::WorkflowPublicationResult, super::ServerError> {
    let draft = state
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
    let catalog = authoring_catalog(state).await?;
    let configuration = request.configuration.clone();
    let preview = run_computation(state, request.control.clone(), fallback_operation_id, {
        let document = draft.document.clone();
        move || document.compilation_preview(&catalog, configuration.as_ref())
    })
    .await?;
    let compiled = preview.compiled.as_ref().ok_or_else(|| {
        super::ServerError::WorkflowDefinitionUnsupported(
            "draft publication requires a successful compilation preview".to_string(),
        )
    })?;
    state.authorize_local_workflow_application_operation(
        client_id,
        LocalApplicationOperationRequest {
            operation,
            workflow_id: request.workflow_id.clone(),
            draft_id: Some(request.draft_id.clone()),
            revision: None,
            preset_id: None,
            producer: Some(draft.producer.clone()),
            requirements: compiled.requirements.clone(),
            effects: compiled.effects.clone(),
            activates: request.activate,
            executes: operation == bcode_workflow::WorkflowApplicationOperation::PublishAndStart,
        },
    )?;
    let publication_started_at = std::time::Instant::now();
    let publication = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .publish_workflow_draft(
            &request.workflow_id,
            &request.draft_id,
            request.expected_generation,
            &preview,
            request.activate,
            request.expected_active_revision,
            super::current_time_ms(),
        );
    let (result, outcome) = match publication {
        Ok(publication) => (
            bcode_ipc::WorkflowPublicationResult::Published {
                revision: Box::new(workflow_revision_snapshot(publication.revision)),
                active_revision: publication.active_revision,
            },
            "published",
        ),
        Err(error) => {
            record_authoring_conflict(&state.metrics, "publish");
            (
                bcode_ipc::WorkflowPublicationResult::Conflict(authoring_conflict_result(error)?),
                "conflict",
            )
        }
    };
    record_authoring_duration(
        &state.metrics,
        "workflow.authoring.publication.duration_ms",
        publication_started_at,
        outcome,
    );
    Ok(result)
}

pub async fn publish_and_start(
    fallback_operation_id: String,
    client_id: super::ClientId,
    state: &std::sync::Arc<ServerState>,
    request: bcode_ipc::PublishAndStartWorkflowRequest,
) -> Result<bcode_ipc::WorkflowPublishAndStartResult, super::ServerError> {
    let bcode_ipc::PublishAndStartWorkflowRequest {
        publication,
        run_id,
        parent_session_id,
        workspace_snapshot,
    } = request;
    let configuration = publication.configuration.clone();
    let workflow_id = publication.workflow_id.clone();
    let publication = publish_draft(
        fallback_operation_id,
        client_id,
        state,
        publication,
        bcode_workflow::WorkflowApplicationOperation::PublishAndStart,
    )
    .await?;
    let result = match publication {
        bcode_ipc::WorkflowPublicationResult::Conflict(conflict) => {
            bcode_ipc::WorkflowPublishAndStartResult::PublicationConflict(conflict)
        }
        bcode_ipc::WorkflowPublicationResult::Published {
            revision,
            active_revision,
        } => {
            let run_admission = match start_authored(
                client_id,
                state,
                bcode_ipc::StartAuthoredWorkflowRequest {
                    selection: bcode_ipc::AuthoredWorkflowRunSelection::Revision {
                        workflow_id,
                        revision: revision.identity.revision,
                    },
                    run_id,
                    parent_session_id,
                    workspace_snapshot,
                    parent_session_generation: None,
                    configuration,
                    input: None,
                },
                false,
            )
            .await
            {
                Ok(started) => bcode_ipc::WorkflowRunAdmissionResult::Started(Box::new(started)),
                Err(error) => bcode_ipc::WorkflowRunAdmissionResult::Failed(
                    super::request_error_response(&error),
                ),
            };
            bcode_ipc::WorkflowPublishAndStartResult::Published {
                revision,
                active_revision,
                run_admission,
            }
        }
    };
    Ok(result)
}

pub fn resolve_authored_run(
    state: &std::sync::Arc<ServerState>,
    selection: &bcode_ipc::AuthoredWorkflowRunSelection,
) -> Result<
    (
        String,
        u64,
        bcode_workflow_store::PublishedWorkflowRevision,
        Option<bcode_workflow_store::WorkflowPreset>,
    ),
    super::ServerError,
> {
    let (workflow_id, revision_number, preset) = {
        let store = state
            .workflow_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match selection {
            bcode_ipc::AuthoredWorkflowRunSelection::Revision {
                workflow_id,
                revision,
            } => {
                drop(store);
                (workflow_id.clone(), *revision, None)
            }
            bcode_ipc::AuthoredWorkflowRunSelection::Active { workflow_id } => {
                let workflow = store.authored_workflow(workflow_id)?.ok_or_else(|| {
                    bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                        "authored workflow not found: {workflow_id}"
                    ))
                })?;
                let revision = workflow.active_revision.ok_or_else(|| {
                    bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                        "authored workflow has no active revision: {workflow_id}"
                    ))
                })?;
                drop(store);
                (workflow_id.clone(), revision, None)
            }
            bcode_ipc::AuthoredWorkflowRunSelection::Preset {
                workflow_id,
                preset_id,
                preset_generation,
            } => {
                let preset = store
                    .workflow_preset(workflow_id, preset_id)?
                    .ok_or_else(|| {
                        bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                            "workflow preset not found: {workflow_id}/{preset_id}"
                        ))
                    })?;
                if preset.generation != *preset_generation {
                    return Err(
                        bcode_workflow_store::WorkflowStoreError::AuthoringConflict {
                            entity_id: format!("{workflow_id}/{preset_id}"),
                            expected: *preset_generation,
                            current: preset.generation,
                        }
                        .into(),
                    );
                }
                drop(store);
                (workflow_id.clone(), preset.revision, Some(preset))
            }
        }
    };
    let revision = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .workflow_revision(&workflow_id, revision_number)?
        .ok_or_else(|| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                "published workflow revision not found: {workflow_id} v{revision_number}"
            ))
        })?;
    Ok((workflow_id, revision_number, revision, preset))
}

#[allow(clippy::too_many_lines, clippy::significant_drop_tightening)]
pub async fn start_authored(
    client_id: super::ClientId,
    state: &std::sync::Arc<ServerState>,
    request: bcode_ipc::StartAuthoredWorkflowRequest,
    authorize: bool,
) -> Result<bcode_ipc::AuthoredWorkflowRunStartResponse, super::ServerError> {
    let resolution_started_at = std::time::Instant::now();
    let (workflow_id, revision_number, revision, preset) =
        resolve_authored_run(state, &request.selection)?;
    record_authoring_duration(
        &state.metrics,
        "workflow.authoring.start_resolution.duration_ms",
        resolution_started_at,
        match &request.selection {
            bcode_ipc::AuthoredWorkflowRunSelection::Revision { .. } => "revision",
            bcode_ipc::AuthoredWorkflowRunSelection::Active { .. } => "active",
            bcode_ipc::AuthoredWorkflowRunSelection::Preset { .. } => "preset",
        },
    );
    let configuration = preset
        .as_ref()
        .map(|preset| preset.configuration.clone())
        .or(request.configuration)
        .or_else(|| revision.document.configuration_defaults.clone())
        .unwrap_or_else(|| serde_json::json!({}));
    let catalog = authoring_catalog(state).await?;
    let preview = revision
        .document
        .compilation_preview(&catalog, Some(&configuration));
    let compiled = preview.compiled.ok_or_else(|| {
        super::ServerError::WorkflowDefinitionUnsupported(
            "published authored workflow is no longer executable on this host".to_string(),
        )
    })?;
    if compiled.definition_identity != revision.definition_identity {
        return Err(super::ServerError::WorkflowDefinitionUnsupported(
            "runtime configuration changes immutable compiled definition identity".to_string(),
        ));
    }
    if authorize {
        state.authorize_local_workflow_application_operation(
            client_id,
            LocalApplicationOperationRequest {
                operation: match request.selection {
                    bcode_ipc::AuthoredWorkflowRunSelection::Revision { .. } => {
                        bcode_workflow::WorkflowApplicationOperation::StartRevision
                    }
                    bcode_ipc::AuthoredWorkflowRunSelection::Active { .. } => {
                        bcode_workflow::WorkflowApplicationOperation::StartActiveRevision
                    }
                    bcode_ipc::AuthoredWorkflowRunSelection::Preset { .. } => {
                        bcode_workflow::WorkflowApplicationOperation::StartPreset
                    }
                },
                workflow_id: workflow_id.clone(),
                draft_id: None,
                revision: matches!(
                    request.selection,
                    bcode_ipc::AuthoredWorkflowRunSelection::Revision { .. }
                )
                .then_some(revision_number),
                preset_id: preset.as_ref().map(|preset| preset.preset_id.clone()),
                producer: Some(revision.producer.clone()),
                requirements: compiled.requirements.clone(),
                effects: compiled.effects.clone(),
                activates: false,
                executes: true,
            },
        )?;
    }
    let limits = preset
        .as_ref()
        .and_then(|preset| preset.run_limits.as_ref())
        .unwrap_or(&compiled.run_limits);
    let deadline_at_ms = limits
        .maximum_duration_ms
        .map(|duration| super::current_time_ms().saturating_add(duration));
    let provenance = bcode_workflow_store::AuthoredWorkflowRunProvenance::new(
        workflow_id.clone(),
        revision_number,
        revision.definition_identity.clone(),
        preset.as_ref().map(|preset| preset.preset_id.clone()),
        preset.as_ref().map(|preset| preset.generation),
        configuration.clone(),
    );
    let run_input = request
        .input
        .unwrap_or_else(|| compiled.input_defaults.clone());
    jsonschema::validator_for(&compiled.definition.input.schema)
        .map_err(|error| super::ServerError::WorkflowDefinitionUnsupported(error.to_string()))?
        .validate(&run_input)
        .map_err(|error| {
            super::ServerError::WorkflowDefinitionUnsupported(format!(
                "workflow run input does not match published interface: {error}"
            ))
        })?;
    let started = start_run(
        state,
        bcode_ipc::WorkflowRunStartRequest {
            definition_id: revision.definition_identity.definition_id.clone(),
            definition_version: revision.definition_identity.definition_version,
            run_id: request.run_id,
            workspace_snapshot: request.workspace_snapshot.unwrap_or_default(),
            parent_session_id: request.parent_session_id,
            parent_session_generation: request.parent_session_generation,
            binding: Some(bcode_workflow_store::WorkflowRunBinding {
                owner_plugin_id: "bcode.authored-workflow".to_string(),
                workflow_kind: workflow_id.clone(),
                scope_key: format!("revision/{revision_number}"),
                display_label: Some(revision.document.metadata.title.clone()),
                single_active: false,
            }),
            input: Some(run_input),
            limits: bcode_workflow_store::WorkflowRunLimits {
                deadline_at_ms,
                node_execution_cap: limits.node_execution_cap,
                concurrency_cap: limits.concurrency_cap,
                cycle_cap: limits.cycle_cap,
                retry_cap: limits.retry_cap,
            },
        },
        Some(provenance),
    )
    .await?;
    Ok(bcode_ipc::AuthoredWorkflowRunStartResponse {
        started,
        workflow_id,
        revision: revision_number,
        definition_identity: revision.definition_identity,
        preset_id: preset.as_ref().map(|preset| preset.preset_id.clone()),
        preset_generation: preset.as_ref().map(|preset| preset.generation),
        configuration,
    })
}

const WORKFLOW_RUN_VIEW_COLLECTION_LIMIT_MAX: usize = 1_000;

#[must_use]
pub fn run_view_collection_limit(requested: usize) -> usize {
    requested.min(WORKFLOW_RUN_VIEW_COLLECTION_LIMIT_MAX)
}

#[allow(clippy::too_many_lines)]
pub fn run_view(
    state: &ServerState,
    run_id: &str,
    limit: usize,
) -> Result<bcode_workflow_view_models::WorkflowRunView, super::ServerError> {
    let limit = run_view_collection_limit(limit);
    let view = {
        let store = state
            .workflow_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let run = store.run_summary(run_id)?.ok_or_else(|| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                "workflow run not found: {run_id}"
            ))
        })?;
        let definition = store
            .definition(&run.definition_id, run.definition_version)?
            .ok_or_else(|| {
                bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                    "workflow definition not found: {} v{}",
                    run.definition_id, run.definition_version
                ))
            })?;
        let outputs = store.output_summaries(run_id, limit)?;
        let output_values = store
            .validated_outputs(run_id, limit)?
            .into_iter()
            .map(|output| (output.output_id, output.value))
            .collect::<BTreeMap<_, _>>();
        let activations = store.activations_for_run(run_id, limit)?;
        let waits = store.waiting_activations(run_id, limit)?;
        let mutation_approvals = store.pending_mutation_approvals(run_id, limit)?;
        let attempts = store.attempt_history(run_id, None, limit)?;
        let retry_schedules = attempts
            .iter()
            .map(|attempt| {
                store.automatic_retry_schedule(run_id, &attempt.node_id, &attempt.activation_id)
            })
            .collect::<Result<Vec<_>, bcode_workflow_store::WorkflowStoreError>>()?
            .into_iter()
            .flatten()
            .map(|schedule| {
                (
                    (schedule.node_id.clone(), schedule.activation_id.clone()),
                    schedule,
                )
            })
            .collect::<BTreeMap<_, _>>();
        let failure_events = store.failure_history(run_id, limit)?;
        let descendant_runs = store.descendant_run_summaries(run_id, limit)?;
        let child_sessions = store.execution_session_links_for_run(run_id, limit)?;
        let run_item = run_list_item(&store, &run)?;
        let parsed_definition: bcode_workflow::WorkflowDefinition =
            serde_json::from_str(&definition.definition_json).map_err(|error| {
                bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                    "workflow definition is not valid current-format JSON: {error}"
                ))
            })?;
        let wait_views = waits
            .into_iter()
            .map(|wait| {
                let node = parsed_definition.node(&wait.node_id).ok_or_else(|| {
                    bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                        "workflow wait references missing node: {}",
                        wait.node_id
                    ))
                })?;
                let kind = match wait.kind {
                    bcode_workflow_store::WorkflowWaitKind::Input => {
                        bcode_workflow_view_models::WorkflowWaitKind::Input
                    }
                    bcode_workflow_store::WorkflowWaitKind::Approval => {
                        bcode_workflow_view_models::WorkflowWaitKind::Approval
                    }
                };
                Ok(bcode_workflow_view_models::WorkflowWaitView {
                    node_id: wait.node_id,
                    activation_id: wait.activation_id,
                    kind,
                    prompt: node.name.clone(),
                    expected_schema: (kind == bcode_workflow_view_models::WorkflowWaitKind::Input)
                        .then(|| node.output.schema.clone()),
                    input: wait.input,
                    requested_at_ms: wait.requested_at_ms,
                })
            })
            .collect::<Result<Vec<_>, bcode_workflow_store::WorkflowStoreError>>()?;
        let mutation_approval_views = mutation_approvals
            .into_iter()
            .map(|approval| {
                let warning = match approval.scope.reconciliation {
                    bcode_workflow::WorkflowBlockReconciliation::IdempotentReplay => None,
                    bcode_workflow::WorkflowBlockReconciliation::ReceiptStatus => Some(
                        "Execution is reconciled through an owner receipt after restart."
                            .to_string(),
                    ),
                    bcode_workflow::WorkflowBlockReconciliation::RepairRequired => Some(
                        "An ambiguous accepted execution requires explicit repair.".to_string(),
                    ),
                };
                bcode_workflow_view_models::WorkflowMutationApprovalView {
                    approval_id: approval.approval_id,
                    node_id: approval.node_id,
                    activation_id: approval.activation_id,
                    plugin_id: approval.scope.plugin_id,
                    block_id: approval.scope.block_id,
                    block_version: approval.scope.block_version,
                    operation: approval.scope.operation,
                    effect: bcode_workflow_view_models::WorkflowOperationEffect::Mutating,
                    input_summary: approval.scope.input_summary,
                    resource_claims: approval
                        .scope
                        .resource_claims
                        .into_iter()
                        .map(
                            |claim| bcode_workflow_view_models::WorkflowResourceClaimView {
                                resource: claim.resource,
                                access: match claim.access {
                                    bcode_workflow::ResourceAccess::Read => "read".to_string(),
                                    bcode_workflow::ResourceAccess::Write => "write".to_string(),
                                },
                            },
                        )
                        .collect(),
                    workspace_snapshot: approval.scope.workspace_snapshot,
                    reconciliation_warning: warning,
                    requested_at_ms: approval.requested_at_ms,
                    expires_at_ms: approval.expires_at_ms,
                }
            })
            .collect();
        let descendant_views = descendant_runs
            .into_iter()
            .map(|descendant| {
                Ok(bcode_workflow_view_models::WorkflowDescendantRunView {
                    run: run_list_item(&store, &descendant.run)?,
                    parent_run_id: descendant.link.parent_run_id,
                    parent_node_id: descendant.link.parent_node_id,
                    depth: descendant.link.depth,
                })
            })
            .collect::<Result<Vec<_>, bcode_workflow_store::WorkflowStoreError>>()?;
        drop(store);
        bcode_workflow_view::project_run(bcode_workflow_view::WorkflowRunProjectionInput {
            run: run_item,
            terminal_output_id: run.terminal_output_id,
            definition: bcode_workflow_view::WorkflowDefinitionProjectionInput {
                definition_json: definition.definition_json,
            },
            activations: activations
                .into_iter()
                .map(
                    |activation| bcode_workflow_view::WorkflowActivationProjectionInput {
                        node_id: activation.node_id,
                        activation_id: activation.activation_id,
                        dependency_generation: activation.dependency_generation,
                        status: activation.status,
                        has_output: activation.has_output,
                        input_summary: match activation.input_summary {
                            bcode_workflow_store::WorkflowActivationInputSummary::Absent => {
                                bcode_workflow_view_models::WorkflowInputSummary::Absent
                            }
                            bcode_workflow_store::WorkflowActivationInputSummary::Inline {
                                value,
                            } => bcode_workflow_view_models::WorkflowInputSummary::Inline { value },
                            bcode_workflow_store::WorkflowActivationInputSummary::Omitted {
                                byte_count,
                            } => bcode_workflow_view_models::WorkflowInputSummary::Omitted {
                                byte_count,
                            },
                        },
                        created_at_ms: activation.created_at_ms,
                    },
                )
                .collect(),
            waits: wait_views,
            mutation_approvals: mutation_approval_views,
            attempts: attempts
                .into_iter()
                .map(|attempt| bcode_workflow_view_models::WorkflowAttemptView {
                    node_id: attempt.node_id,
                    activation_id: attempt.activation_id,
                    attempt: attempt.attempt,
                    dispatch_identity: attempt.dispatch_identity,
                    status: attempt.status,
                    has_receipt: attempt.has_receipt,
                    prepared_at_ms: attempt.prepared_at_ms,
                    admitted_at_ms: attempt.admitted_at_ms,
                    terminal_at_ms: attempt.terminal_at_ms,
                })
                .collect(),
            retry_schedules: retry_schedules
                .into_values()
                .map(|schedule| bcode_workflow_view_models::WorkflowRetryScheduleView {
                    node_id: schedule.node_id,
                    activation_id: schedule.activation_id,
                    failed_attempt: schedule.failed_attempt,
                    next_attempt: schedule.next_attempt,
                    failure_kind: match schedule.failure_kind {
                        bcode_workflow::AutomaticRetryFailureKind::OwnerUnavailableBeforeAcceptance => bcode_workflow_view_models::WorkflowRetryFailureKind::OwnerUnavailableBeforeAcceptance,
                        bcode_workflow::AutomaticRetryFailureKind::OwnerReportedRetryable => bcode_workflow_view_models::WorkflowRetryFailureKind::OwnerReportedRetryable,
                        bcode_workflow::AutomaticRetryFailureKind::Cancellation => bcode_workflow_view_models::WorkflowRetryFailureKind::Cancellation,
                        bcode_workflow::AutomaticRetryFailureKind::TerminalTimeout => bcode_workflow_view_models::WorkflowRetryFailureKind::TerminalTimeout,
                        bcode_workflow::AutomaticRetryFailureKind::ApprovalDenied => bcode_workflow_view_models::WorkflowRetryFailureKind::ApprovalDenied,
                        bcode_workflow::AutomaticRetryFailureKind::SchemaFailure => bcode_workflow_view_models::WorkflowRetryFailureKind::SchemaFailure,
                        bcode_workflow::AutomaticRetryFailureKind::AmbiguousMutation => bcode_workflow_view_models::WorkflowRetryFailureKind::AmbiguousMutation,
                        bcode_workflow::AutomaticRetryFailureKind::TerminalFailure => bcode_workflow_view_models::WorkflowRetryFailureKind::TerminalFailure,
                    },
                    backoff_ms: schedule.backoff_ms,
                    next_attempt_at_ms: schedule.next_attempt_at_ms,
                    scheduled_at_ms: schedule.scheduled_at_ms,
                })
                .collect(),
            outputs: outputs
                .into_iter()
                .map(
                    |output| bcode_workflow_view::WorkflowOutputProjectionInput {
                        value: output_values.get(&output.output_id).cloned(),
                        output_id: output.output_id,
                        node_id: output.node_id,
                        activation_id: output.activation_id,
                        schema_id: output.schema_id,
                        schema_version: output.schema_version,
                        checksum_sha256: output.checksum_sha256,
                        artifact_reference: output.artifact_reference,
                        created_at_ms: output.created_at_ms,
                    },
                )
                .collect(),
            failure_events: failure_events
                .into_iter()
                .map(
                    |event| bcode_workflow_view::WorkflowFailureEventProjectionInput {
                        event_sequence: event.event_seq,
                        event_type: event.event_type,
                        payload: event.payload,
                        created_at_ms: event.created_at_ms,
                    },
                )
                .collect(),
            descendant_runs: descendant_views,
            child_sessions: child_sessions
                .into_iter()
                .map(
                    |link| bcode_workflow_view_models::WorkflowChildSessionView {
                        node_id: link.node_id,
                        activation_id: link.activation_id,
                        attempt: link.attempt,
                        session_id: link.session_id,
                    },
                )
                .collect(),
        })
    };
    Ok(view)
}

fn run_list_item(
    store: &bcode_workflow_store::WorkflowStore,
    run: &bcode_workflow_store::WorkflowRunSummary,
) -> Result<bcode_workflow_view_models::WorkflowRunListItem, bcode_workflow_store::WorkflowStoreError>
{
    let summary = store.run_catalog_summary(&run.run_id)?;
    run_list_item_with_summary(store, run, &summary)
}

fn run_list_item_with_summary(
    store: &bcode_workflow_store::WorkflowStore,
    run: &bcode_workflow_store::WorkflowRunSummary,
    summary: &bcode_workflow_store::WorkflowRunCatalogSummary,
) -> Result<bcode_workflow_view_models::WorkflowRunListItem, bcode_workflow_store::WorkflowStoreError>
{
    let authored_workflow = run
        .authored_provenance
        .as_ref()
        .map(|source| store.authored_workflow(&source.workflow_id))
        .transpose()?
        .flatten();
    let parent_run_id = store
        .parent_run_link(&run.run_id)?
        .map(|link| link.parent_run_id);
    let editable_draft_id = if let Some(source) = &run.authored_provenance {
        store
            .list_workflow_drafts(&source.workflow_id, 1)?
            .first()
            .map(|draft| draft.draft_id.clone())
    } else {
        None
    };
    let display_title = authored_workflow.as_ref().map_or_else(
        || {
            run.binding
                .as_ref()
                .and_then(|binding| binding.display_label.clone())
                .unwrap_or_else(|| run.definition_id.clone())
        },
        |workflow| workflow.title.clone(),
    );
    let definition_disposition = run.authored_provenance.as_ref().map_or(
        bcode_workflow_view_models::WorkflowDefinitionDisposition::CompiledOnly,
        |source| bcode_workflow_view_models::WorkflowDefinitionDisposition::Published {
            workflow_id: source.workflow_id.clone(),
            revision: source.revision,
            editable_draft_id,
        },
    );
    Ok(bcode_workflow_view_models::WorkflowRunListItem {
        run_id: run.run_id.clone(),
        display_title,
        binding_label: run
            .binding
            .as_ref()
            .and_then(|binding| binding.display_label.clone()),
        definition_id: run.definition_id.clone(),
        definition_version: run.definition_version,
        authored_source: run.authored_provenance.as_ref().map(|source| {
            bcode_workflow_view_models::WorkflowAuthoredSourceView {
                workflow_id: source.workflow_id.clone(),
                revision: source.revision,
            }
        }),
        definition_disposition,
        parent_run_id,
        descendant_count: summary.descendant_count,
        progress: bcode_workflow_view_models::WorkflowRunProgress {
            total_nodes: summary.total_nodes,
            not_started: summary.not_started,
            active: summary.active,
            blocked: summary.blocked,
            completed: summary.completed,
            failed: summary.failed,
            cancelled: summary.cancelled,
            skipped: summary.skipped,
            repair_required: summary.repair_required,
        },
        attention: bcode_workflow_view_models::WorkflowAttentionSummary {
            pending_inputs: summary.pending_inputs,
            pending_approvals: summary.pending_approvals,
            pending_mutation_approvals: summary.pending_mutation_approvals,
            retryable_failures: summary.retryable_failures,
            repair_required: run.status == bcode_workflow_store::RunStatus::RepairRequired
                || summary.repair_required > 0,
        },
        status: match run.status {
            bcode_workflow_store::RunStatus::Running => {
                bcode_workflow_view_models::WorkflowRunStatus::Running
            }
            bcode_workflow_store::RunStatus::Paused => {
                bcode_workflow_view_models::WorkflowRunStatus::Paused
            }
            bcode_workflow_store::RunStatus::Completed => {
                bcode_workflow_view_models::WorkflowRunStatus::Completed
            }
            bcode_workflow_store::RunStatus::Failed => {
                bcode_workflow_view_models::WorkflowRunStatus::Failed
            }
            bcode_workflow_store::RunStatus::Cancelled => {
                bcode_workflow_view_models::WorkflowRunStatus::Cancelled
            }
            bcode_workflow_store::RunStatus::RepairRequired => {
                bcode_workflow_view_models::WorkflowRunStatus::RepairRequired
            }
        },
        created_at_ms: run.created_at_ms,
        updated_at_ms: run.updated_at_ms,
    })
}

fn authored_workflow_page(
    page: bcode_workflow_store::WorkflowAuthoringStorePage<bcode_workflow_store::AuthoredWorkflow>,
    _limit: usize,
) -> bcode_ipc::WorkflowAuthoringPage<
    bcode_ipc::AuthoredWorkflowSnapshot,
    bcode_workflow::WorkflowAuthoringListCursor,
> {
    let bcode_workflow_store::WorkflowAuthoringStorePage {
        items: workflows,
        has_more,
    } = page;
    let next_cursor = has_more.then(|| {
        let last = workflows
            .last()
            .expect("a page with more items is non-empty");
        bcode_workflow::WorkflowAuthoringListCursor {
            updated_at_ms: last.updated_at_ms,
            entity_id: last.workflow_id.clone(),
        }
    });
    bcode_ipc::WorkflowAuthoringPage {
        items: workflows
            .into_iter()
            .map(authored_workflow_snapshot)
            .collect(),
        next_cursor,
    }
}

fn workflow_draft_page(
    page: bcode_workflow_store::WorkflowAuthoringStorePage<bcode_workflow_store::WorkflowDraft>,
    _limit: usize,
) -> bcode_ipc::WorkflowAuthoringPage<
    bcode_ipc::WorkflowDraftSnapshot,
    bcode_workflow::WorkflowAuthoringListCursor,
> {
    let bcode_workflow_store::WorkflowAuthoringStorePage {
        items: drafts,
        has_more,
    } = page;
    let next_cursor = has_more.then(|| {
        let last = drafts.last().expect("a page with more items is non-empty");
        bcode_workflow::WorkflowAuthoringListCursor {
            updated_at_ms: last.updated_at_ms,
            entity_id: last.draft_id.clone(),
        }
    });
    bcode_ipc::WorkflowAuthoringPage {
        items: drafts.into_iter().map(workflow_draft_snapshot).collect(),
        next_cursor,
    }
}

fn workflow_revision_page(
    page: bcode_workflow_store::WorkflowAuthoringStorePage<
        bcode_workflow_store::PublishedWorkflowRevision,
    >,
    _limit: usize,
) -> bcode_ipc::WorkflowAuthoringPage<
    bcode_ipc::WorkflowRevisionSnapshot,
    bcode_workflow::WorkflowRevisionListCursor,
> {
    let bcode_workflow_store::WorkflowAuthoringStorePage {
        items: revisions,
        has_more,
    } = page;
    let next_cursor = has_more.then(|| bcode_workflow::WorkflowRevisionListCursor {
        revision: revisions
            .last()
            .expect("a page with more items is non-empty")
            .revision,
    });
    bcode_ipc::WorkflowAuthoringPage {
        items: revisions
            .into_iter()
            .map(workflow_revision_snapshot)
            .collect(),
        next_cursor,
    }
}

fn workflow_preset_page(
    page: bcode_workflow_store::WorkflowAuthoringStorePage<bcode_workflow_store::WorkflowPreset>,
    _limit: usize,
) -> bcode_ipc::WorkflowAuthoringPage<
    bcode_ipc::WorkflowPresetSnapshot,
    bcode_workflow::WorkflowAuthoringListCursor,
> {
    let bcode_workflow_store::WorkflowAuthoringStorePage {
        items: presets,
        has_more,
    } = page;
    let next_cursor = has_more.then(|| {
        let last = presets.last().expect("a page with more items is non-empty");
        bcode_workflow::WorkflowAuthoringListCursor {
            updated_at_ms: last.updated_at_ms,
            entity_id: last.preset_id.clone(),
        }
    });
    bcode_ipc::WorkflowAuthoringPage {
        items: presets.into_iter().map(workflow_preset_snapshot).collect(),
        next_cursor,
    }
}

fn authored_workflow_inspection(
    state: &ServerState,
    workflow_id: &str,
    limit: usize,
) -> Result<Option<bcode_ipc::AuthoredWorkflowInspection>, super::ServerError> {
    let store = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(workflow) = store.authored_workflow(workflow_id)? else {
        return Ok(None);
    };
    let drafts = store
        .list_workflow_drafts(workflow_id, limit)?
        .into_iter()
        .map(|draft| bcode_ipc::WorkflowDraftInspectionSummary {
            identity: bcode_workflow::WorkflowDraftIdentity {
                workflow_id: draft.workflow_id,
                draft_id: draft.draft_id,
            },
            base_revision: draft.base_revision,
            generation: draft.generation,
            checksum_sha256: draft.checksum_sha256,
            created_at_ms: draft.created_at_ms,
            updated_at_ms: draft.updated_at_ms,
        })
        .collect();
    let revisions = store
        .list_workflow_revisions(workflow_id, limit)?
        .into_iter()
        .map(|revision| bcode_ipc::WorkflowRevisionInspectionSummary {
            identity: bcode_workflow::WorkflowRevisionIdentity {
                workflow_id: revision.workflow_id,
                revision: revision.revision,
            },
            source_checksum_sha256: revision.source_checksum_sha256,
            executable_source_checksum_sha256: revision.executable_source_checksum_sha256,
            definition_identity: revision.definition_identity,
            published_at_ms: revision.published_at_ms,
        })
        .collect();
    let presets = store
        .list_workflow_presets(workflow_id, limit)?
        .into_iter()
        .map(|preset| bcode_ipc::WorkflowPresetInspectionSummary {
            workflow_id: preset.workflow_id,
            preset_id: preset.preset_id,
            revision: preset.revision,
            generation: preset.generation,
            has_run_limit_override: preset.run_limits.is_some(),
            created_at_ms: preset.created_at_ms,
            updated_at_ms: preset.updated_at_ms,
        })
        .collect();
    let events = store
        .workflow_authoring_events(workflow_id, None, limit)?
        .into_iter()
        .map(|event| bcode_ipc::WorkflowAuthoringEventSnapshot {
            event_seq: event.event_seq,
            workflow_id: event.workflow_id,
            event_type: event.event_type,
            revision: event
                .payload
                .get("revision")
                .and_then(serde_json::Value::as_u64),
            definition_id: event
                .payload
                .get("definition_id")
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string),
            definition_version: event
                .payload
                .get("definition_version")
                .and_then(serde_json::Value::as_u64)
                .and_then(|version| u32::try_from(version).ok()),
            activated: event
                .payload
                .get("activated")
                .and_then(serde_json::Value::as_bool),
            created_at_ms: event.created_at_ms,
        })
        .collect();
    let issues = store
        .diagnose_authored_workflow(workflow_id, limit)?
        .into_iter()
        .map(workflow_authoring_issue_snapshot)
        .collect();
    drop(store);
    Ok(Some(bcode_ipc::AuthoredWorkflowInspection {
        workflow: authored_workflow_snapshot(workflow),
        drafts,
        revisions,
        presets,
        events,
        issues,
    }))
}

fn workflow_authoring_issue_snapshot(
    issue: bcode_workflow_store::WorkflowAuthoringIssue,
) -> bcode_ipc::WorkflowAuthoringIssueSnapshot {
    match issue {
        bcode_workflow_store::WorkflowAuthoringIssue::InvalidActiveRevision { revision } => {
            bcode_ipc::WorkflowAuthoringIssueSnapshot::InvalidActiveRevision { revision }
        }
        bcode_workflow_store::WorkflowAuthoringIssue::MissingCompiledDefinition {
            revision,
            definition_id,
            definition_version,
        } => bcode_ipc::WorkflowAuthoringIssueSnapshot::MissingCompiledDefinition {
            revision,
            definition_id,
            definition_version,
        },
        bcode_workflow_store::WorkflowAuthoringIssue::OrphanedPreset {
            preset_id,
            revision,
        } => bcode_ipc::WorkflowAuthoringIssueSnapshot::OrphanedPreset {
            preset_id,
            revision,
        },
        bcode_workflow_store::WorkflowAuthoringIssue::StaleDraftBase {
            draft_id,
            base_revision,
        } => bcode_ipc::WorkflowAuthoringIssueSnapshot::StaleDraftBase {
            draft_id,
            base_revision,
        },
    }
}

pub fn authored_workflow_snapshot(
    workflow: bcode_workflow_store::AuthoredWorkflow,
) -> bcode_ipc::AuthoredWorkflowSnapshot {
    bcode_ipc::AuthoredWorkflowSnapshot {
        workflow_id: workflow.workflow_id,
        title: workflow.title,
        description: workflow.description,
        archived: workflow.archived,
        active_revision: workflow.active_revision,
        created_at_ms: workflow.created_at_ms,
        updated_at_ms: workflow.updated_at_ms,
    }
}

pub fn workflow_draft_snapshot(
    draft: bcode_workflow_store::WorkflowDraft,
) -> bcode_ipc::WorkflowDraftSnapshot {
    bcode_ipc::WorkflowDraftSnapshot {
        identity: bcode_workflow::WorkflowDraftIdentity {
            workflow_id: draft.workflow_id,
            draft_id: draft.draft_id,
        },
        base_revision: draft.base_revision,
        generation: draft.generation,
        checksum_sha256: draft.checksum_sha256,
        document: draft.document,
        producer: draft.producer,
        created_at_ms: draft.created_at_ms,
        updated_at_ms: draft.updated_at_ms,
    }
}

pub fn workflow_revision_snapshot(
    revision: bcode_workflow_store::PublishedWorkflowRevision,
) -> bcode_ipc::WorkflowRevisionSnapshot {
    bcode_ipc::WorkflowRevisionSnapshot {
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

pub fn workflow_preset_snapshot(
    preset: bcode_workflow_store::WorkflowPreset,
) -> bcode_ipc::WorkflowPresetSnapshot {
    bcode_ipc::WorkflowPresetSnapshot {
        workflow_id: preset.workflow_id,
        preset_id: preset.preset_id,
        revision: preset.revision,
        name: preset.name,
        generation: preset.generation,
        configuration: preset.configuration,
        run_limits: preset.run_limits,
        producer: preset.producer,
        created_at_ms: preset.created_at_ms,
        updated_at_ms: preset.updated_at_ms,
    }
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
    Ok(authored_workflow_page(page, limit))
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
        .map(|workflow| workflow.map(authored_workflow_snapshot))
}

/// Return a bounded authored-workflow inspection when the workflow exists.
pub fn inspect_authored_workflow(
    state: &ServerState,
    workflow_id: &str,
    limit: usize,
) -> Result<Option<bcode_ipc::AuthoredWorkflowInspection>, super::ServerError> {
    authored_workflow_inspection(state, workflow_id, limit)
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
    Ok(workflow_draft_page(page, limit))
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
        .map(|draft| draft.map(workflow_draft_snapshot))
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
    Ok(workflow_revision_page(page, limit))
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
        .map(|revision| revision.map(workflow_revision_snapshot))
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
    Ok(workflow_preset_page(page, limit))
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
        .map(|preset| preset.map(workflow_preset_snapshot))
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

/// Inspect one exact workflow launch target without mutation.
pub async fn launch_detail(
    state: &ServerState,
    request: &bcode_workflow::WorkflowLaunchDetailRequest,
) -> Result<bcode_workflow::WorkflowLaunchDetail, super::ServerError> {
    request.validate()?;
    match &request.source {
        bcode_workflow::WorkflowLaunchSourceIdentity::ExplicitSource {
            source_path,
            source_format,
        } => {
            let source = bcode_workflow_discovery::inspect_explicit_source(source_path)?;
            let bcode_workflow_discovery::DiscoveredWorkflowSource::Standalone {
                source,
                source_format: actual_format,
                source_path,
                ..
            } = source
            else {
                return Err(bcode_workflow_store::WorkflowStoreError::InvalidData(
                    "explicit launch detail identity expected a standalone source".to_string(),
                )
                .into());
            };
            if actual_format != *source_format {
                return Err(bcode_workflow_store::WorkflowStoreError::InvalidData(
                    "explicit launch source format changed".to_string(),
                )
                .into());
            }
            let catalog = authoring_catalog(state).await?;
            let lowering =
                bcode_workflow::lower_workflow_authoring_source(&source, actual_format, &catalog)?;
            let preview = lowering.document.compilation_preview(&catalog, None);
            let item = standalone_launch_item(
                "explicit".to_string(),
                0,
                source_path.clone(),
                actual_format,
                &lowering,
                &preview,
                true,
            );
            Ok(bcode_workflow::WorkflowLaunchDetail {
                version: bcode_workflow::WORKFLOW_LAUNCH_CATALOG_VERSION,
                item,
                document: lowering.document,
                package_plan: None,
            })
        }
        source => {
            let page = launch_catalog(
                state,
                &bcode_workflow::WorkflowLaunchCatalogRequest {
                    version: bcode_workflow::WORKFLOW_LAUNCH_CATALOG_VERSION,
                    workspace: request.workspace.clone(),
                    limit: bcode_workflow::MAX_WORKFLOW_LAUNCH_CATALOG_PAGE_SIZE,
                    cursor: None,
                    search: None,
                    source_kind: None,
                    readiness: None,
                },
            )
            .await?;
            let item = page
                .items
                .into_iter()
                .find(|item| &item.source == source)
                .ok_or_else(|| {
                    bcode_workflow_store::WorkflowStoreError::InvalidData(
                        "workflow launch target is no longer discoverable".to_string(),
                    )
                })?;
            launch_detail_for_catalog_item(state, &request.workspace, item).await
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn launch_detail_for_catalog_item(
    state: &ServerState,
    workspace: &std::path::Path,
    item: bcode_workflow::WorkflowLaunchCatalogItem,
) -> Result<bcode_workflow::WorkflowLaunchDetail, super::ServerError> {
    let catalog = authoring_catalog(state).await?;
    let (document, package_plan) = match &item.source {
        bcode_workflow::WorkflowLaunchSourceIdentity::PackageExport {
            export,
            manifest_path,
            ..
        } => {
            let source = bcode_workflow_discovery::inspect_explicit_source(manifest_path)?;
            let bcode_workflow_discovery::DiscoveredWorkflowSource::Package { closure, .. } =
                source
            else {
                return Err(bcode_workflow_store::WorkflowStoreError::InvalidData(
                    "package launch identity no longer resolves to a package".to_string(),
                )
                .into());
            };
            let plan = bcode_workflow::plan_workflow_package_closure(&closure, &catalog)?;
            let entry = plan
                .packages
                .iter()
                .find(|entry| entry.package_id == plan.entry_package_id)
                .ok_or_else(|| {
                    bcode_workflow_store::WorkflowStoreError::InvalidData(
                        "package plan has no entry package".to_string(),
                    )
                })?;
            let member_id = entry
                .plan
                .lock
                .exports
                .iter()
                .find(|candidate| candidate.export == *export)
                .map(|candidate| candidate.member_id.as_str())
                .ok_or_else(|| {
                    bcode_workflow_store::WorkflowStoreError::InvalidData(
                        "package export is no longer available".to_string(),
                    )
                })?;
            let document = entry
                .plan
                .members
                .iter()
                .find(|member| member.member_id == member_id)
                .map(|member| member.lowering.document.clone())
                .ok_or_else(|| {
                    bcode_workflow_store::WorkflowStoreError::InvalidData(
                        "package export member is missing".to_string(),
                    )
                })?;
            (document, Some(plan))
        }
        bcode_workflow::WorkflowLaunchSourceIdentity::StandaloneSource {
            source_path,
            source_format,
            ..
        } => {
            let source = bcode_workflow_discovery::inspect_explicit_source(source_path)?;
            let bcode_workflow_discovery::DiscoveredWorkflowSource::Standalone {
                source,
                source_format: actual_format,
                ..
            } = source
            else {
                return Err(bcode_workflow_store::WorkflowStoreError::InvalidData(
                    "standalone launch identity no longer resolves to a source".to_string(),
                )
                .into());
            };
            if actual_format != *source_format {
                return Err(bcode_workflow_store::WorkflowStoreError::InvalidData(
                    "standalone launch source format changed".to_string(),
                )
                .into());
            }
            (
                bcode_workflow::lower_workflow_authoring_source(&source, actual_format, &catalog)?
                    .document,
                None,
            )
        }
        bcode_workflow::WorkflowLaunchSourceIdentity::Template {
            owner_plugin_id,
            template_id,
            template_version,
        } => {
            let template =
                describe_template(state, owner_plugin_id, template_id, *template_version)?
                    .ok_or_else(|| {
                        bcode_workflow_store::WorkflowStoreError::InvalidData(
                            "workflow template is no longer available".to_string(),
                        )
                    })?;
            let document = template.authoring_document.ok_or_else(|| {
                bcode_workflow_store::WorkflowStoreError::InvalidData(
                    "workflow template has no maintainable authoring document".to_string(),
                )
            })?;
            (document, None)
        }
        bcode_workflow::WorkflowLaunchSourceIdentity::ExplicitSource { .. } => {
            return Err(
                bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                    "explicit source detail must use its direct path operation in {}",
                    workspace.display()
                ))
                .into(),
            );
        }
    };
    Ok(bcode_workflow::WorkflowLaunchDetail {
        version: bcode_workflow::WORKFLOW_LAUNCH_CATALOG_VERSION,
        item,
        document,
        package_plan,
    })
}

/// Discover and semantically preview one bounded workflow launch-catalog page.
///
/// Discovery is read-only. It never applies, publishes, repairs, or starts a workflow.
#[allow(clippy::too_many_lines)]
pub async fn launch_catalog(
    state: &ServerState,
    request: &bcode_workflow::WorkflowLaunchCatalogRequest,
) -> Result<bcode_workflow::WorkflowLaunchCatalogPage, super::ServerError> {
    request.validate()?;
    let config = &state.startup_config.workflows;
    let discovery = bcode_workflow_discovery::discover_workflows(
        &request.workspace,
        config,
        request.limit.saturating_add(1),
    )?;
    let catalog = authoring_catalog(state).await?;
    let mut items = Vec::new();
    for source in discovery.sources {
        match source {
            bcode_workflow_discovery::DiscoveredWorkflowSource::Package {
                source_label,
                precedence,
                manifest_path,
                closure,
                ..
            } => {
                let plan = bcode_workflow::plan_workflow_package_closure(&closure, &catalog)?;
                let entry_index = plan
                    .packages
                    .iter()
                    .position(|entry| entry.package_id == plan.entry_package_id)
                    .ok_or_else(|| {
                        bcode_workflow_store::WorkflowStoreError::InvalidData(
                            "planned workflow package closure has no entry package".to_string(),
                        )
                    })?;
                let entry = &plan.packages[entry_index];
                let mut preview_catalog = catalog.clone();
                for dependency in &plan.packages[..entry_index] {
                    for member in &dependency.plan.members {
                        preview_catalog.workflow_definitions.insert(
                            member.definition_identity.definition_id.clone(),
                            member.lowering.document.definition.clone(),
                        );
                    }
                }
                let preview = bcode_workflow::preview_workflow_package(
                    &entry.plan,
                    &preview_catalog,
                    &BTreeMap::new(),
                )?;
                let receipt = package_publication(state, &entry.package_id)?;
                let lock_digest = preview.lock.digest_sha256()?;
                let readiness = receipt.as_ref().map_or(
                    bcode_workflow::WorkflowLaunchReadiness::Unpublished,
                    |receipt| {
                        if receipt.package_lock_digest_sha256 == lock_digest {
                            bcode_workflow::WorkflowLaunchReadiness::Ready
                        } else {
                            bcode_workflow::WorkflowLaunchReadiness::Drifted
                        }
                    },
                );
                for locked_export in &entry.plan.lock.exports {
                    let export = &locked_export.export;
                    let member_id = &locked_export.member_id;
                    let member = preview
                        .members
                        .iter()
                        .find(|member| &member.member_id == member_id)
                        .ok_or_else(|| {
                            bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                                "workflow package export '{export}' references missing member '{member_id}'"
                            ))
                        })?;
                    let planned = entry
                        .plan
                        .members
                        .iter()
                        .find(|planned| &planned.member_id == member_id)
                        .ok_or_else(|| {
                            bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                                "workflow package export '{export}' has no planned member '{member_id}'"
                            ))
                        })?;
                    let compiled = member.compilation.compiled.as_ref();
                    items.push(bcode_workflow::WorkflowLaunchCatalogItem {
                        source: bcode_workflow::WorkflowLaunchSourceIdentity::PackageExport {
                            package_id: entry.package_id.clone(),
                            export: export.clone(),
                            manifest_path: manifest_path.clone(),
                        },
                        source_label: source_label.clone(),
                        precedence,
                        title: planned.lowering.document.metadata.title.clone(),
                        description: planned.lowering.document.metadata.description.clone(),
                        readiness,
                        unavailable_reason: match readiness {
                            bcode_workflow::WorkflowLaunchReadiness::Ready => None,
                            bcode_workflow::WorkflowLaunchReadiness::Unpublished => {
                                Some("package must be explicitly applied and published".to_string())
                            }
                            bcode_workflow::WorkflowLaunchReadiness::Drifted => Some(
                                "published package lock differs from discovered source".to_string(),
                            ),
                            _ => Some("workflow package is unavailable".to_string()),
                        },
                        package_lock_digest_sha256: Some(lock_digest.clone()),
                        publication: receipt
                            .as_ref()
                            .and_then(|receipt| {
                                receipt
                                    .exports
                                    .iter()
                                    .find(|candidate| candidate.export == *export)
                            })
                            .and_then(|published| {
                                published.published_revision.as_ref().map(|revision| {
                                    bcode_workflow::WorkflowLaunchPublicationIdentity {
                                        workflow_id: revision.workflow_id.clone(),
                                        revision: revision.revision,
                                        definition_identity: published.definition_identity.clone(),
                                    }
                                })
                            }),
                        actions: package_launch_actions(readiness),
                        requirements: compiled
                            .map(|compiled| compiled.requirements.clone())
                            .unwrap_or_default(),
                        effects: compiled
                            .map(|compiled| compiled.effects.clone())
                            .unwrap_or_default(),
                        permissions: compiled
                            .map(|compiled| compiled.permissions.clone())
                            .unwrap_or_default(),
                        input_schema: planned.lowering.document.definition.input.clone(),
                        configuration_schema: planned
                            .lowering
                            .document
                            .configuration_schema
                            .clone(),
                        diagnostics: member.compilation.validation.diagnostics.clone(),
                    });
                }
            }
            bcode_workflow_discovery::DiscoveredWorkflowSource::Standalone {
                source_label,
                precedence,
                source_path,
                source_format,
                source,
            } => {
                let lowering = bcode_workflow::lower_workflow_authoring_source(
                    &source,
                    source_format,
                    &catalog,
                )?;
                let preview = lowering.document.compilation_preview(&catalog, None);
                items.push(standalone_launch_item(
                    source_label,
                    precedence,
                    source_path,
                    source_format,
                    &lowering,
                    &preview,
                    false,
                ));
            }
        }
    }
    for template in list_templates(
        state,
        request
            .limit
            .saturating_add(1)
            .min(bcode_workflow::MAX_WORKFLOW_LAUNCH_CATALOG_PAGE_SIZE),
    )? {
        let Some(document) = template.authoring_document else {
            continue;
        };
        let preview = document.compilation_preview(&catalog, None);
        let compiled = preview.compiled.as_ref();
        let readiness = if template.diagnostics.is_empty() && preview.is_compiled() {
            bcode_workflow::WorkflowLaunchReadiness::Ready
        } else {
            bcode_workflow::WorkflowLaunchReadiness::Unavailable
        };
        items.push(bcode_workflow::WorkflowLaunchCatalogItem {
            source: bcode_workflow::WorkflowLaunchSourceIdentity::Template {
                owner_plugin_id: template.owner_plugin_id.clone(),
                template_id: template.template.template_id.clone(),
                template_version: template.template.template_version,
            },
            source_label: format!("plugin:{}", template.owner_plugin_id),
            precedence: 200,
            title: document.metadata.title.clone(),
            description: document.metadata.description.clone(),
            readiness,
            unavailable_reason: (!template.diagnostics.is_empty()).then(|| {
                template
                    .diagnostics
                    .iter()
                    .map(|diagnostic| diagnostic.message.as_str())
                    .collect::<Vec<_>>()
                    .join("; ")
            }),
            package_lock_digest_sha256: None,
            publication: None,
            actions: template_launch_actions(readiness),
            requirements: compiled.map_or_else(
                || document.requirements.clone(),
                |compiled| compiled.requirements.clone(),
            ),
            effects: compiled
                .map(|compiled| compiled.effects.clone())
                .unwrap_or_default(),
            permissions: compiled
                .map(|compiled| compiled.permissions.clone())
                .unwrap_or_default(),
            input_schema: document.definition.input.clone(),
            configuration_schema: document.configuration_schema.clone(),
            diagnostics: preview.validation.diagnostics,
        });
    }
    Ok(project_launch_catalog_page(
        items,
        discovery
            .diagnostics
            .into_iter()
            .map(|diagnostic| bcode_workflow::WorkflowLaunchDiagnostic {
                source_label: diagnostic.source_label,
                path: diagnostic.path,
                code: diagnostic.code,
                message: diagnostic.message,
            })
            .collect(),
        request,
    ))
}

fn project_launch_catalog_page(
    mut items: Vec<bcode_workflow::WorkflowLaunchCatalogItem>,
    diagnostics: Vec<bcode_workflow::WorkflowLaunchDiagnostic>,
    request: &bcode_workflow::WorkflowLaunchCatalogRequest,
) -> bcode_workflow::WorkflowLaunchCatalogPage {
    let search = request.search.as_ref().map(|search| search.to_lowercase());
    items.retain(|item| {
        request.source_kind.is_none_or(|kind| {
            matches!(
                (kind, &item.source),
                (
                    bcode_workflow::WorkflowLaunchSourceKind::PackageExport,
                    bcode_workflow::WorkflowLaunchSourceIdentity::PackageExport { .. }
                ) | (
                    bcode_workflow::WorkflowLaunchSourceKind::StandaloneSource,
                    bcode_workflow::WorkflowLaunchSourceIdentity::StandaloneSource { .. }
                        | bcode_workflow::WorkflowLaunchSourceIdentity::ExplicitSource { .. }
                ) | (
                    bcode_workflow::WorkflowLaunchSourceKind::Template,
                    bcode_workflow::WorkflowLaunchSourceIdentity::Template { .. }
                )
            )
        }) && request
            .readiness
            .is_none_or(|readiness| readiness == item.readiness)
            && search.as_ref().is_none_or(|search| {
                item.title.to_lowercase().contains(search)
                    || item
                        .description
                        .as_ref()
                        .is_some_and(|description| description.to_lowercase().contains(search))
                    || item.source_label.to_lowercase().contains(search)
            })
    });
    items.sort_by(|left, right| {
        (&left.title, launch_source_key(&left.source))
            .cmp(&(&right.title, launch_source_key(&right.source)))
    });
    if let Some(cursor) = &request.cursor {
        items.retain(|item| {
            (&item.title, launch_source_key(&item.source))
                > (&cursor.title, cursor.source_key.clone())
        });
    }
    let has_more = items.len() > request.limit;
    items.truncate(request.limit);
    let next_cursor = has_more.then(|| items.last()).flatten().map(|item| {
        bcode_workflow::WorkflowLaunchCatalogCursor {
            title: item.title.clone(),
            source_key: launch_source_key(&item.source),
        }
    });
    bcode_workflow::WorkflowLaunchCatalogPage {
        version: bcode_workflow::WORKFLOW_LAUNCH_CATALOG_VERSION,
        items,
        diagnostics,
        next_cursor,
    }
}

fn standalone_launch_item(
    source_label: String,
    precedence: u32,
    source_path: std::path::PathBuf,
    source_format: bcode_workflow::WorkflowSourceFormat,
    lowering: &bcode_workflow::WorkflowSourceLoweringResult,
    preview: &bcode_workflow::WorkflowCompilationPreview,
    explicit: bool,
) -> bcode_workflow::WorkflowLaunchCatalogItem {
    let compiled = preview.compiled.as_ref();
    bcode_workflow::WorkflowLaunchCatalogItem {
        source: if explicit {
            bcode_workflow::WorkflowLaunchSourceIdentity::ExplicitSource {
                source_path,
                source_format,
            }
        } else {
            bcode_workflow::WorkflowLaunchSourceIdentity::StandaloneSource {
                workflow_id: lowering.document.workflow_id.clone(),
                source_path,
                source_format,
            }
        },
        source_label,
        precedence,
        title: lowering.document.metadata.title.clone(),
        description: lowering.document.metadata.description.clone(),
        readiness: if preview.is_compiled() {
            bcode_workflow::WorkflowLaunchReadiness::Unpublished
        } else {
            bcode_workflow::WorkflowLaunchReadiness::Invalid
        },
        unavailable_reason: Some(
            "source must be explicitly applied and published before start".to_string(),
        ),
        package_lock_digest_sha256: None,
        publication: None,
        actions: source_launch_actions(preview.is_compiled()),
        requirements: compiled.map_or_else(
            || lowering.document.requirements.clone(),
            |compiled| compiled.requirements.clone(),
        ),
        effects: compiled
            .map(|compiled| compiled.effects.clone())
            .unwrap_or_default(),
        permissions: compiled
            .map(|compiled| compiled.permissions.clone())
            .unwrap_or_default(),
        input_schema: lowering.document.definition.input.clone(),
        configuration_schema: lowering.document.configuration_schema.clone(),
        diagnostics: preview.validation.diagnostics.clone(),
    }
}

fn package_launch_actions(
    readiness: bcode_workflow::WorkflowLaunchReadiness,
) -> Vec<bcode_workflow::WorkflowLaunchActionAffordance> {
    use bcode_workflow::{
        WorkflowLaunchActionAffordance as Action, WorkflowLaunchActionKind as Kind,
    };
    let available = |kind| Action {
        kind,
        enabled: true,
        unavailable_reason: None,
    };
    let unavailable = |kind, reason: &str| Action {
        kind,
        enabled: false,
        unavailable_reason: Some(reason.to_string()),
    };
    match readiness {
        bcode_workflow::WorkflowLaunchReadiness::Ready => vec![
            available(Kind::Validate),
            available(Kind::Preview),
            available(Kind::Start),
            available(Kind::OpenAuthoring),
            available(Kind::OpenSource),
        ],
        bcode_workflow::WorkflowLaunchReadiness::Unpublished
        | bcode_workflow::WorkflowLaunchReadiness::Drifted => vec![
            available(Kind::Validate),
            available(Kind::Preview),
            available(Kind::Apply),
            unavailable(
                Kind::Publish,
                "apply the exact discovered package drafts first",
            ),
            unavailable(Kind::Start, "publish the exact package lock first"),
            available(Kind::OpenAuthoring),
            available(Kind::OpenSource),
        ],
        _ => vec![
            available(Kind::Validate),
            unavailable(Kind::Start, "workflow package is not launchable"),
            available(Kind::OpenSource),
        ],
    }
}

fn source_launch_actions(compiled: bool) -> Vec<bcode_workflow::WorkflowLaunchActionAffordance> {
    use bcode_workflow::{
        WorkflowLaunchActionAffordance as Action, WorkflowLaunchActionKind as Kind,
    };
    [
        (Kind::Validate, true, None),
        (
            Kind::Preview,
            compiled,
            Some("source validation must succeed"),
        ),
        (
            Kind::Apply,
            compiled,
            Some("source validation must succeed"),
        ),
        (Kind::Publish, false, Some("apply the source draft first")),
        (
            Kind::Start,
            false,
            Some("publish an immutable revision first"),
        ),
        (Kind::OpenAuthoring, true, None),
        (Kind::OpenSource, true, None),
    ]
    .into_iter()
    .map(|(kind, enabled, reason)| Action {
        kind,
        enabled,
        unavailable_reason: (!enabled).then(|| reason.unwrap_or("unavailable").to_string()),
    })
    .collect()
}

fn template_launch_actions(
    readiness: bcode_workflow::WorkflowLaunchReadiness,
) -> Vec<bcode_workflow::WorkflowLaunchActionAffordance> {
    use bcode_workflow::{
        WorkflowLaunchActionAffordance as Action, WorkflowLaunchActionKind as Kind,
    };
    let ready = readiness == bcode_workflow::WorkflowLaunchReadiness::Ready;
    vec![
        Action {
            kind: Kind::Preview,
            enabled: ready,
            unavailable_reason: (!ready)
                .then(|| "template requirements are unavailable".to_string()),
        },
        Action {
            kind: Kind::Start,
            enabled: ready,
            unavailable_reason: (!ready)
                .then(|| "template requirements are unavailable".to_string()),
        },
        Action {
            kind: Kind::OpenAuthoring,
            enabled: true,
            unavailable_reason: None,
        },
    ]
}

fn launch_source_key(source: &bcode_workflow::WorkflowLaunchSourceIdentity) -> String {
    match source {
        bcode_workflow::WorkflowLaunchSourceIdentity::PackageExport {
            package_id, export, ..
        } => format!("package:{package_id}:{export}"),
        bcode_workflow::WorkflowLaunchSourceIdentity::StandaloneSource {
            workflow_id,
            source_path,
            ..
        } => format!("source:{workflow_id}:{}", source_path.display()),
        bcode_workflow::WorkflowLaunchSourceIdentity::ExplicitSource { source_path, .. } => {
            format!("explicit:{}", source_path.display())
        }
        bcode_workflow::WorkflowLaunchSourceIdentity::Template {
            owner_plugin_id,
            template_id,
            template_version,
        } => format!("template:{owner_plugin_id}:{template_id}:{template_version}"),
    }
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
    let plan = run_computation(state, request.control, fallback_operation_id, move || {
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
    run_computation(state, request.control, fallback_operation_id, move || {
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
    let lowering = run_computation(state, request.control, fallback_operation_id, move || {
        bcode_workflow::lower_workflow_authoring_source(&request.source, source_format, &catalog)
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
        run_computation(state, request.control, fallback_operation_id, move || {
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
    let started_at = std::time::Instant::now();
    let report = run_computation(state, control, fallback_operation_id, move || {
        document.validation_report()
    })
    .await?;
    record_authoring_duration(
        &state.metrics,
        "workflow.authoring.validation.duration_ms",
        started_at,
        if report.valid { "valid" } else { "invalid" },
    );
    Ok(report)
}

/// Preview compilation for one bounded authored-workflow document.
pub async fn preview_compilation(
    state: &ServerState,
    document: bcode_workflow::WorkflowAuthoringDocument,
    configuration: Option<serde_json::Value>,
    control: bcode_ipc::WorkflowComputationControl,
    fallback_operation_id: String,
) -> Result<bcode_workflow::WorkflowCompilationPreview, super::ServerError> {
    let started_at = std::time::Instant::now();
    let catalog = authoring_catalog(state).await?;
    let preview = run_computation(state, control, fallback_operation_id, move || {
        document.compilation_preview(&catalog, configuration.as_ref())
    })
    .await?;
    record_authoring_duration(
        &state.metrics,
        "workflow.authoring.compilation.duration_ms",
        started_at,
        if preview.compiled.is_some() {
            "compiled"
        } else {
            "rejected"
        },
    );
    Ok(preview)
}

pub struct AuthorityGuard {
    pub authority: bcode_workflow_store::WorkflowExecutionAuthority,
    pub _session_ownership: Option<bcode_session::SessionOwnershipGuard>,
}

/// How the current daemon relates to a run's recorded coordinator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PriorOwnerLiveness {
    /// Canonical evidence shows the recorded coordinator is still live or cannot be classified.
    LiveOrUnverifiable,
    /// A session-owner observation positively classified the recorded coordinator as stale.
    ObservedEnded,
    /// No lease observation and no live daemon record name the recorded coordinator instance.
    NoLiveTrace,
}

/// Classify whether the run's recorded coordinator daemon still exists.
///
/// Two independent canonical sources are consulted: session-owner lease observations for the
/// run's parent session, and the daemon registry's live records. A daemon whose lease was lost
/// but whose process is still registered as live counts as live. Ambiguity fails closed.
async fn prior_owner_liveness(
    state: &ServerState,
    session_id: super::SessionId,
    current: &bcode_workflow_store::WorkflowExecutionAuthority,
) -> Result<PriorOwnerLiveness, super::ServerError> {
    let root = state.sessions.session_store_root().ok_or_else(|| {
        bcode_workflow_store::WorkflowStoreError::InvalidData(
            "workflow ownership transfer requires a persistent session store".to_string(),
        )
    })?;
    let observations = bcode_session::lease::session_owner_observations(&root, session_id)
        .map_err(|error| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(error.to_string())
        })?;
    let mut observed_ended = false;
    for observation in observations {
        if observation.owner.daemon_instance_id.as_deref()
            != Some(current.daemon_instance_id.as_str())
        {
            continue;
        }
        match observation.liveness {
            bcode_session::lease::SessionOwnerLiveness::Live
            | bcode_session::lease::SessionOwnerLiveness::Unverifiable => {
                return Ok(PriorOwnerLiveness::LiveOrUnverifiable);
            }
            bcode_session::lease::SessionOwnerLiveness::Stale => observed_ended = true,
        }
    }
    // A lease can be lost while the daemon process survives, so consult the daemon registry
    // before concluding the owner is gone. Only the record naming the recorded coordinator is
    // probed: classifying every registry record costs a bounded endpoint probe per record.
    let state_dir = bcode_config::default_state_dir();
    for (_, record) in bcode_daemon_lifecycle::read_records(&state_dir) {
        if record.instance_id != current.daemon_instance_id {
            continue;
        }
        match bcode_daemon_lifecycle::classify_daemon_record(&record).await {
            bcode_daemon_lifecycle::DaemonRecordClassification::CurrentHealthy
            | bcode_daemon_lifecycle::DaemonRecordClassification::HistoricalExactResponsive
            | bcode_daemon_lifecycle::DaemonRecordClassification::HistoricalProcessVerifiedProtocolUnsupported
            | bcode_daemon_lifecycle::DaemonRecordClassification::ResponsiveIdentityMismatch
            | bcode_daemon_lifecycle::DaemonRecordClassification::Unverifiable => {
                return Ok(PriorOwnerLiveness::LiveOrUnverifiable);
            }
            bcode_daemon_lifecycle::DaemonRecordClassification::UnreachableStale => {
                observed_ended = true;
            }
        }
    }
    Ok(if observed_ended {
        PriorOwnerLiveness::ObservedEnded
    } else {
        PriorOwnerLiveness::NoLiveTrace
    })
}

fn current_artifact_id(state: &ServerState) -> String {
    state.daemon_status.artifact_id.as_ref().map_or_else(
        || state.daemon_status.build_fingerprint.clone(),
        ToString::to_string,
    )
}

fn run_parent_session_id(
    state: &ServerState,
    run_id: &str,
) -> Result<super::SessionId, super::ServerError> {
    let run = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .run_summary(run_id)?
        .ok_or_else(|| bcode_workflow_store::WorkflowStoreError::RunNotFound {
            run_id: run_id.to_string(),
        })?;
    run.parent_session_id
        .as_deref()
        .and_then(|value| value.parse::<super::SessionId>().ok())
        .ok_or_else(|| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(
                "workflow run has no canonical parent session for ownership transfer".to_string(),
            )
            .into()
        })
}

/// Resolve the current daemon's durable execution authority for one run.
///
/// * Same daemon instance: returns the recorded authority.
/// * Same artifact, different (ended) instance: same-artifact compare-and-swap transfer.
/// * Different artifact: allowed only when the recorded coordinator verifiably ended and the run
///   holds no live attempt. The run is then reassigned to this artifact with an audited evidence
///   record. Live or unverifiable prior owners are refused with an error that names them.
///
/// # Errors
///
/// Returns an error when the recorded coordinator is live or unverifiable, when the run still has
/// live attempts on another artifact, when session ownership cannot be acquired, or when the
/// compare-and-swap loses a race.
pub async fn execution_authority(
    state: &std::sync::Arc<ServerState>,
    run_id: &str,
) -> Result<Option<AuthorityGuard>, super::ServerError> {
    let current = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .execution_authority(run_id)?;
    let Some(current) = current else {
        return Ok(None);
    };
    if current.daemon_instance_id == state.daemon_status.instance_id {
        return Ok(Some(AuthorityGuard {
            authority: current,
            _session_ownership: None,
        }));
    }
    let artifact_id = current_artifact_id(state);
    let session_id = run_parent_session_id(state, run_id)?;
    let liveness = prior_owner_liveness(state, session_id, &current).await?;
    if liveness == PriorOwnerLiveness::LiveOrUnverifiable {
        return Err(super::ServerError::WorkflowOwnedByLiveDaemon {
            run_id: run_id.to_string(),
            daemon_instance_id: current.daemon_instance_id.clone(),
            target_artifact_id: current.target_artifact_id.clone(),
            same_artifact: current.target_artifact_id == artifact_id,
        });
    }
    let session_ownership = state
        .sessions
        .acquire_session_ownership(session_id, bcode_session::SessionOwnershipKind::RuntimeWork)
        .await
        .map_err(|error| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(error.to_string())
        })?;
    let replacement = bcode_workflow_store::WorkflowExecutionAuthority {
        target_artifact_id: artifact_id.clone(),
        daemon_instance_id: state.daemon_status.instance_id.clone(),
        generation: current.generation.saturating_add(1),
        fencing_token: uuid::Uuid::new_v4().to_string(),
    };
    let now_ms = super::current_unix_millis();
    let mut store = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if current.target_artifact_id == artifact_id {
        store.transfer_execution_authority(run_id, &current, &replacement, now_ms)?;
    } else {
        let evidence = bcode_workflow_store::EndedOwnerEvidence {
            ended_daemon_instance_id: current.daemon_instance_id.clone(),
            ended_target_artifact_id: current.target_artifact_id.clone(),
            liveness: match liveness {
                PriorOwnerLiveness::ObservedEnded => {
                    bcode_workflow_store::EndedOwnerLiveness::ObservedEnded
                }
                PriorOwnerLiveness::NoLiveTrace | PriorOwnerLiveness::LiveOrUnverifiable => {
                    bcode_workflow_store::EndedOwnerLiveness::NoLiveTrace
                }
            },
            artifact_image_available: bcode_daemon_lifecycle::artifact_image_is_available(
                &bcode_config::default_state_dir(),
                &current.target_artifact_id,
            ),
        };
        store.reassign_execution_authority_from_ended_owner(
            run_id,
            &current,
            &replacement,
            &evidence,
            now_ms,
        )?;
        tracing::info!(
            target: "bcode_server::workflow",
            run_id,
            from_artifact = %current.target_artifact_id,
            from_instance = %current.daemon_instance_id,
            liveness = ?evidence.liveness,
            "reassigned quiescent workflow run from an ended coordinator on another artifact"
        );
    }
    drop(store);
    Ok(Some(AuthorityGuard {
        authority: replacement,
        _session_ownership: Some(session_ownership),
    }))
}

pub fn validate_workflow_definition_for_production(
    state: &ServerState,
    definition: &bcode_workflow::WorkflowDefinition,
) -> Result<(), super::ServerError> {
    let capabilities = bcode_workflow::WorkflowProductionCapabilities::current();
    let admission = definition
        .production_admission(&capabilities)
        .map_err(|error| super::ServerError::WorkflowDefinitionUnsupported(error.to_string()))?;
    if !admission.is_supported() {
        let summary = admission
            .diagnostics
            .iter()
            .map(|diagnostic| {
                diagnostic.node_id.as_ref().map_or_else(
                    || format!("{}: {}", diagnostic.code, diagnostic.message),
                    |node_id| {
                        format!(
                            "{} at node '{}': {}",
                            diagnostic.code, node_id, diagnostic.message
                        )
                    },
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        return Err(super::ServerError::WorkflowDefinitionUnsupported(summary));
    }
    for node in definition.nodes.values() {
        if node.kind != bcode_workflow::NodeKind::PluginBlock {
            continue;
        }
        let block: bcode_workflow::WorkflowBlockDefinition =
            serde_json::from_value(node.configuration.clone()).map_err(|error| {
                super::ServerError::WorkflowDefinitionUnsupported(format!(
                    "plugin block node '{}' has invalid configuration: {error}",
                    node.id
                ))
            })?;
        block.validate().map_err(|error| {
            super::ServerError::WorkflowDefinitionUnsupported(format!(
                "plugin block node '{}' is invalid: {error}",
                node.id
            ))
        })?;
        let declared = state
            .plugins
            .registry()
            .workflow_blocks()
            .into_iter()
            .any(|candidate| candidate == block);
        if !declared {
            return Err(super::ServerError::WorkflowCapabilityUnavailable(format!(
                "plugin block node '{}' requires unavailable exact contract {}:{} v{} ({})",
                node.id, block.plugin_id, block.block_id, block.block_version, block.operation
            )));
        }
    }
    Ok(())
}

fn compile_workflow_template(
    template: &bcode_plugin::WorkflowTemplateContribution,
    _configuration: &serde_json::Value,
) -> Result<bcode_workflow::WorkflowDefinition, super::ServerError> {
    let mut definition = template.definition().clone();
    let uses_configuration_envelope =
        definition.input.type_name == template.configuration_schema().type_name;
    if !uses_configuration_envelope {
        return Ok(definition);
    }
    definition.input = template.configuration_schema().clone();
    definition.output = template.configuration_schema().clone();
    for (node_id, node) in &mut definition.nodes {
        if node.kind != bcode_workflow::NodeKind::Agent {
            continue;
        }
        let original_input = node.input.clone();
        let mut agent = serde_json::from_value::<bcode_workflow::WorkflowPromptConfiguration>(
            node.configuration.clone(),
        )
        .map_err(|error| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                "workflow template entry agent configuration is invalid: {error}"
            ))
        })?;
        if original_input.type_name == template.configuration_schema().type_name {
            node.input = template.configuration_schema().clone();
            for edge in definition
                .edges
                .iter_mut()
                .filter(|edge| edge.to == *node_id)
            {
                if let Some(transform) = &mut edge.transform
                    && transform.output.type_name == original_input.type_name
                {
                    transform.output = template.configuration_schema().clone();
                }
            }
        }
        if node.output.type_name == template.configuration_schema().type_name {
            node.output = template.configuration_schema().clone();
            if let bcode_workflow::WorkflowPromptOutputPolicy::Structured { result } =
                &mut agent.output
            {
                result.schema = template.configuration_schema().clone();
            }
        }
        node.configuration = serde_json::to_value(agent).map_err(|error| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                "workflow template entry agent configuration cannot be serialized: {error}"
            ))
        })?;
    }
    definition.validate().map_err(|error| {
        bcode_workflow_store::WorkflowStoreError::InvalidData(error.to_string())
    })?;
    Ok(definition)
}

fn find_workflow_template<'a>(
    state: &'a ServerState,
    owner_plugin_id: &str,
    template_id: &str,
    template_version: u32,
) -> Option<&'a bcode_plugin::WorkflowTemplateContribution> {
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
        .map(|(_, template)| template)
}

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
    let template = find_workflow_template(
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
    let source = template.authoring_document().ok_or_else(|| {
        bcode_workflow_store::WorkflowStoreError::InvalidData(
            "only standard authoring-document templates can be instantiated as drafts".to_string(),
        )
    })?;
    let mut document = source.clone();
    document.workflow_id.clone_from(&request.workflow_id);
    document.definition.name.clone_from(&request.workflow_id);
    document.producer = bcode_workflow::WorkflowProducerProvenance {
        kind: bcode_workflow::WorkflowProducerKind::Plugin,
        producer_id: Some(request.owner_plugin_id.clone()),
        source_revision: None,
    };
    document.validate()?;
    let catalog = authoring_catalog(state).await?;
    let preview = document.compilation_preview(&catalog, None);
    let compiled = preview.compiled.as_ref().ok_or_else(|| {
        super::ServerError::WorkflowDefinitionUnsupported(format!(
            "template instantiation requires a successful compilation preview: {:?}",
            preview.validation.diagnostics
        ))
    })?;
    state.authorize_local_workflow_application_operation(
        client_id,
        LocalApplicationOperationRequest {
            operation: bcode_workflow::WorkflowApplicationOperation::CreateWorkflow,
            workflow_id: request.workflow_id.clone(),
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
        workflow_id: request.workflow_id.clone(),
        title: document.metadata.title.clone(),
        description: document.metadata.description.clone(),
        archived: false,
        active_revision: None,
        created_at_ms: now,
        updated_at_ms: now,
    };
    let draft = bcode_workflow_store::WorkflowDraft {
        workflow_id: request.workflow_id,
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

fn persist_exact_template_call_dependencies(
    state: &ServerState,
    definition: &bcode_workflow::WorkflowDefinition,
) -> Result<(), super::ServerError> {
    let dependencies = bcode_workflow::workflow_dependency_manifest(definition)?;
    for dependency in dependencies {
        let identity = dependency.target.definition_identity();
        let Some((owner_plugin_id, template)) = state
            .plugins
            .registry()
            .workflow_templates()
            .into_iter()
            .find(|(owner, template)| {
                template
                    .definition_identity(owner)
                    .is_ok_and(|candidate| &candidate == identity)
            })
        else {
            continue;
        };
        let child_definition = template.definition();
        let actual = template
            .definition_identity(owner_plugin_id)
            .map_err(|error| {
                bcode_workflow_store::WorkflowStoreError::InvalidData(error.to_string())
            })?;
        if &actual != identity {
            return Err(
                bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                    "template dependency identity mismatch for {}",
                    identity.kind
                ))
                .into(),
            );
        }
        state
            .workflow_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .persist_definition(
                &actual.definition_id,
                actual.definition_version,
                child_definition,
            )?;
        persist_exact_template_call_dependencies(state, child_definition)?;
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
pub async fn start_run(
    state: &std::sync::Arc<ServerState>,
    request: bcode_ipc::WorkflowRunStartRequest,
    authored_provenance: Option<bcode_workflow_store::AuthoredWorkflowRunProvenance>,
) -> Result<bcode_ipc::WorkflowRunStartResponse, super::ServerError> {
    let stored_definition = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .definition(&request.definition_id, request.definition_version)?
        .ok_or_else(|| {
            super::ServerError::WorkflowCapabilityUnavailable(format!(
                "workflow definition not found: {} v{}",
                request.definition_id, request.definition_version
            ))
        })?;
    let definition: bcode_workflow::WorkflowDefinition =
        serde_json::from_str(&stored_definition.definition_json)?;
    validate_workflow_definition_for_production(state, &definition)?;
    let uses_fixed_generation = definition.nodes.values().any(|node| {
        node.kind == bcode_workflow::NodeKind::Agent
            && serde_json::from_value::<bcode_workflow::WorkflowPromptConfiguration>(
                node.configuration.clone(),
            )
            .is_ok_and(|configuration| {
                configuration.execution_target
                    == bcode_workflow::PromptContextTarget::FixedGenerationFork
            })
    });
    if uses_fixed_generation && request.parent_session_generation.is_none() {
        return Err(bcode_workflow_store::WorkflowStoreError::InvalidData(
            "fixed-generation workflow prompts require parent_session_generation at start"
                .to_string(),
        )
        .into());
    }
    let parent_session = state
        .sessions
        .session_summary(request.parent_session_id)
        .await?;
    if let Some(expected_generation) = request.parent_session_generation {
        let current_generation = state
            .sessions
            .current_session_generation(request.parent_session_id)
            .await?;
        if current_generation != expected_generation {
            return Err(bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                "parent session generation changed before workflow admission: expected {expected_generation}, current {current_generation}"
            ))
            .into());
        }
    }
    let run_id = request
        .run_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let created_at_ms = super::current_unix_millis();
    let workspace_snapshot = if request.workspace_snapshot.is_empty() {
        parent_session
            .working_directory
            .to_string_lossy()
            .into_owned()
    } else {
        request.workspace_snapshot
    };
    let authorization_profile = state
        .plugins
        .invoke_service_by_interface_json::<_, super::AgentPolicyProfileIdentity>(
            super::AGENT_PROFILE_INTERFACE_ID,
            super::OP_RESOLVE_POLICY_PROFILE_IDENTITY,
            &super::ResolveAgentPolicyProfileIdentityRequest {
                profile_id: super::session_agent_selection(state, parent_session.id).await,
                effective_config_toml: bcode_config::encode_effective_config(
                    &state.session_config(parent_session.id).await,
                )
                .ok()
                .map(Box::new),
            },
        )
        .await
        .map_err(|error| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                "workflow authorization profile resolution failed closed: {error}"
            ))
        })?;
    let authorization_profile = bcode_workflow::WorkflowAuthorizationProfileIdentity {
        version: authorization_profile.version,
        provider_id: authorization_profile.provider_id,
        profile_id: authorization_profile.profile_id,
        policy_digest_sha256: authorization_profile.policy_digest_sha256,
    };
    authorization_profile.validate().map_err(|error| {
        bcode_workflow_store::WorkflowStoreError::InvalidData(error.to_string())
    })?;
    let workflow_session_ownership = state
        .sessions
        .acquire_session_ownership(
            parent_session.id,
            bcode_session::SessionOwnershipKind::RuntimeWork,
        )
        .await
        .map_err(|error| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(error.to_string())
        })?;
    let new_run = bcode_workflow_store::NewWorkflowRun {
        run_id: run_id.clone(),
        definition_id: request.definition_id.clone(),
        definition_version: request.definition_version,
        workspace_snapshot,
        parent_session_id: Some(request.parent_session_id.to_string()),
        parent_session_generation: request.parent_session_generation,
        binding: request.binding,
        authored_provenance,
        input: request.input,
        execution_authority: Some(bcode_workflow_store::WorkflowExecutionAuthority {
            target_artifact_id: state.daemon_status.artifact_id.as_ref().map_or_else(
                || state.daemon_status.build_fingerprint.clone(),
                ToString::to_string,
            ),
            daemon_instance_id: state.daemon_status.instance_id.clone(),
            generation: 1,
            fencing_token: uuid::Uuid::new_v4().to_string(),
        }),
        created_at_ms,
        authorization_profile,
        authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
        limits: request.limits,
    };
    let run = {
        let mut store = state
            .workflow_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _created = store.create_run_idempotent(&new_run)?;
        store
            .run_summary(&run_id)?
            .expect("created or existing workflow run must be readable")
    };
    let runtime_work_id = super::register_workflow_runtime_work(
        state,
        parent_session.id,
        &run_id,
        format!(
            "workflow {} v{}",
            request.definition_id, request.definition_version
        ),
    )
    .await;
    super::drive_workflow_run(state, &run_id).await?;
    drop(workflow_session_ownership);
    Ok(bcode_ipc::WorkflowRunStartResponse {
        run,
        runtime_work_id,
    })
}

/// Validate, persist, and admit one exact workflow definition.
pub async fn start(
    state: &std::sync::Arc<ServerState>,
    request: bcode_ipc::WorkflowStartRequest,
) -> Result<bcode_ipc::WorkflowRunStartResponse, super::ServerError> {
    let started_at = std::time::Instant::now();
    validate_workflow_definition_for_production(state, &request.definition)?;
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
    let result = start_run(
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
    let template = find_workflow_template(
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
    let definition = compile_workflow_template(template, &request.configuration)?;
    persist_exact_template_call_dependencies(state, &definition)?;
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
    let _authority = execution_authority(state, run_id).await?.ok_or_else(|| {
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
    super::settle_workflow_runtime_work(state, run_id).await?;
    Ok(recorded)
}

/// Explicit maintenance: reconcile nonterminal runs whose recorded coordinator verifiably ended.
///
/// Each nonterminal run is classified with the same evidence `execution_authority` uses. Runs
/// whose coordinator is live or unverifiable, runs that still hold live attempts on another
/// artifact, and runs already owned by this daemon are reported as skipped. When `apply` is set,
/// every orphaned run is reassigned to this daemon (audited) and cancelled; a `repair_required`
/// run keeps that status because it still needs explicit attempt-level repair.
///
/// # Errors
///
/// Returns an error when the workflow store cannot enumerate runs.
pub async fn reconcile_orphaned_runs(
    state: &std::sync::Arc<ServerState>,
    apply: bool,
    limit: usize,
) -> Result<bcode_ipc::OrphanedWorkflowRunReport, super::ServerError> {
    let runs = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .nonterminal_runs(limit)?;
    let mut report = bcode_ipc::OrphanedWorkflowRunReport {
        applied: apply,
        ..Default::default()
    };
    for run in runs {
        let skip = |reason: String| bcode_ipc::SkippedWorkflowRun {
            run_id: run.run_id.clone(),
            status: run.status,
            reason,
        };
        let authority = state
            .workflow_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .execution_authority(&run.run_id)?;
        let Some(authority) = authority else {
            report
                .skipped
                .push(skip("run has no durable execution authority".to_string()));
            continue;
        };
        if authority.daemon_instance_id == state.daemon_status.instance_id {
            report
                .skipped
                .push(skip("run is owned by this daemon".to_string()));
            continue;
        }
        let Ok(session_id) = run_parent_session_id(state, &run.run_id) else {
            report
                .skipped
                .push(skip("run has no canonical parent session".to_string()));
            continue;
        };
        match prior_owner_liveness(state, session_id, &authority).await? {
            PriorOwnerLiveness::LiveOrUnverifiable => {
                report.skipped.push(skip(format!(
                    "coordinator daemon {} (artifact {}) is live or unverifiable",
                    authority.daemon_instance_id, authority.target_artifact_id
                )));
                continue;
            }
            PriorOwnerLiveness::ObservedEnded | PriorOwnerLiveness::NoLiveTrace => {}
        }
        let orphan = bcode_ipc::OrphanedWorkflowRun {
            run_id: run.run_id.clone(),
            workflow_kind: run
                .binding
                .as_ref()
                .map(|binding| binding.workflow_kind.clone()),
            parent_session_id: run.parent_session_id.clone(),
            status: run.status,
            ended_daemon_instance_id: authority.daemon_instance_id.clone(),
            ended_target_artifact_id: authority.target_artifact_id.clone(),
            artifact_image_available: bcode_daemon_lifecycle::artifact_image_is_available(
                &bcode_config::default_state_dir(),
                &authority.target_artifact_id,
            ),
            updated_at_ms: run.updated_at_ms,
        };
        if !apply {
            report.reconciled.push(orphan);
            continue;
        }
        // Reassignment goes through the same fenced path every control operation uses, so a
        // run that still holds live attempts on the ended artifact is refused here too.
        if let Err(error) = execution_authority(state, &run.run_id).await {
            report
                .skipped
                .push(skip(format!("authority reassignment refused: {error}")));
            continue;
        }
        if run.status == bcode_workflow_store::RunStatus::RepairRequired {
            // Ownership is now local; the run still needs explicit attempt repair.
            report.reconciled.push(orphan);
            continue;
        }
        if let Err(error) = cancel_run(state, &run.run_id).await {
            report.skipped.push(skip(format!(
                "cancellation after reassignment failed: {error}"
            )));
            continue;
        }
        report.reconciled.push(orphan);
    }
    Ok(report)
}

/// Pause a workflow run while holding its durable execution authority.
pub async fn pause_run(
    state: &std::sync::Arc<ServerState>,
    run_id: &str,
) -> Result<bool, super::ServerError> {
    let _authority = execution_authority(state, run_id).await?.ok_or_else(|| {
        bcode_workflow_store::WorkflowStoreError::InvalidData(
            "active workflow has no durable execution authority".to_string(),
        )
    })?;
    let changed = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .pause_run(run_id, super::current_unix_millis())?;
    if changed {
        // An explicitly paused run with in-flight attempts keeps its node work registered until
        // those attempts observe the pause; the run-level registration is suspended now so a
        // fully paused run never pins the daemon.
        super::settle_workflow_runtime_work(state, run_id).await?;
    }
    Ok(changed)
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
    let _authority = execution_authority(state, run_id).await?.ok_or_else(|| {
        bcode_workflow_store::WorkflowStoreError::InvalidData(
            "active workflow has no durable execution authority".to_string(),
        )
    })?;
    let changed = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .resume_run(run_id, super::current_unix_millis())?;
    if changed
        && let Some(parent_session_id) = run
            .parent_session_id
            .as_deref()
            .and_then(|value| value.parse::<super::SessionId>().ok())
    {
        // Pausing suspended the run-level runtime work; a resumed run is live again.
        super::register_workflow_runtime_work(
            state,
            parent_session_id,
            run_id,
            format!("workflow {} v{}", run.definition_id, run.definition_version),
        )
        .await;
    }
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
        inspect_run(state, &run.run_id, limit).await?,
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
                let _authority =
                    execution_authority(state, &run.run_id)
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
                // A quiescent run cancels immediately; settle its runtime work so the daemon does
                // not keep a phantom registration for a run that is already terminal.
                super::settle_workflow_runtime_work(state, &run.run_id).await?;
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
        .map(|entry| run_list_item_with_summary(&store, &entry.run, &entry.summary))
        .collect::<Result<Vec<_>, _>>()?;
    drop(store);
    Ok(bcode_workflow_view::project_catalog(
        items,
        request,
        page.has_more,
    ))
}

#[allow(clippy::too_many_lines)]
pub async fn apply_source(
    client_id: super::ClientId,
    state: &std::sync::Arc<ServerState>,
    request: bcode_ipc::ApplyWorkflowSourceRequest,
) -> Result<bcode_workflow::WorkflowSourceApplyResult, super::ServerError> {
    let catalog = authoring_catalog(state).await?;
    let lowering = bcode_workflow::lower_workflow_authoring_source(
        &request.source,
        request.source_format,
        &catalog,
    )?;
    let source_map = lowering.source_map;
    let document = lowering.document;
    let mut validation = lowering.validation;
    validation.diagnostics = source_map.remap_diagnostics(&validation.diagnostics);
    let source_profile = lowering.profile;
    document.validate()?;
    let preview = document.compilation_preview(&catalog, None);
    let compiled = preview.compiled.as_ref().ok_or_else(|| {
        super::ServerError::WorkflowDefinitionUnsupported(
            "source apply requires a successful canonical compilation preview".to_string(),
        )
    })?;
    let requirements = compiled.requirements.clone();
    let effects = compiled.effects.clone();
    let permissions = compiled.permissions.clone();
    let workflow_id = document.workflow_id.clone();
    let canonical_digest_sha256 = document.source_digest_sha256()?;
    let existing = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .authored_workflow(&workflow_id)?;
    let (outcome, generation) = if existing.is_none() {
        let producer = document.producer.clone();
        state.authorize_local_workflow_application_operation(
            client_id,
            LocalApplicationOperationRequest {
                operation: bcode_workflow::WorkflowApplicationOperation::CreateWorkflow,
                workflow_id: workflow_id.clone(),
                draft_id: None,
                revision: None,
                preset_id: None,
                producer: Some(producer.clone()),
                requirements: document.requirements.clone(),
                effects: bcode_workflow::WorkflowEffectSummary::default(),
                activates: false,
                executes: false,
            },
        )?;
        let now = super::current_time_ms();
        let workflow = bcode_workflow_store::AuthoredWorkflow {
            workflow_id: workflow_id.clone(),
            title: document.metadata.title.clone(),
            description: document.metadata.description.clone(),
            archived: false,
            active_revision: None,
            created_at_ms: now,
            updated_at_ms: now,
        };
        let draft = bcode_workflow_store::WorkflowDraft {
            workflow_id: workflow_id.clone(),
            draft_id: request.draft_id.clone(),
            base_revision: None,
            generation: 1,
            checksum_sha256: canonical_digest_sha256.clone(),
            document,
            producer,
            created_at_ms: now,
            updated_at_ms: now,
        };
        state
            .workflow_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .create_authored_workflow_with_initial_draft(&workflow, &draft)?;
        (bcode_workflow::WorkflowSourceApplyOutcome::Created, 1)
    } else {
        let current = state
            .workflow_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .workflow_draft(&workflow_id, &request.draft_id)?
            .ok_or_else(|| {
                bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                    "source draft not found: {workflow_id}/{}",
                    request.draft_id
                ))
            })?;
        state.authorize_local_workflow_application_operation(
            client_id,
            LocalApplicationOperationRequest {
                operation: bcode_workflow::WorkflowApplicationOperation::UpdateDraft,
                workflow_id: workflow_id.clone(),
                draft_id: Some(request.draft_id.clone()),
                revision: None,
                preset_id: None,
                producer: Some(document.producer.clone()),
                requirements: document.requirements.clone(),
                effects: bcode_workflow::WorkflowEffectSummary::default(),
                activates: false,
                executes: false,
            },
        )?;
        let expected_generation = current.generation;
        let update = state
            .workflow_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .update_workflow_draft(
                &workflow_id,
                &request.draft_id,
                expected_generation,
                &document,
                &document.producer,
                super::current_time_ms(),
            );
        match update {
            Ok(draft) => (
                bcode_workflow::WorkflowSourceApplyOutcome::Updated,
                draft.generation,
            ),
            Err(bcode_workflow_store::WorkflowStoreError::AuthoringConflict {
                expected,
                current,
                ..
            }) => (
                bcode_workflow::WorkflowSourceApplyOutcome::Conflict {
                    expected_generation: expected,
                    current_generation: current,
                },
                current,
            ),
            Err(error) => return Err(error.into()),
        }
    };
    Ok(bcode_workflow::WorkflowSourceApplyResult {
        version: bcode_workflow::WORKFLOW_SOURCE_APPLY_RESULT_VERSION,
        source_format: request.source_format,
        source_profile,
        workflow_id,
        draft_id: request.draft_id,
        generation,
        canonical_digest_sha256,
        validation,
        source_map,
        requirements,
        effects,
        permissions,
        outcome,
    })
}

/// Register one connection-scoped workflow event consumer and return its snapshot boundary.
///
/// The caller owns transport framing and response encoding; the supplied sink owns bounded event
/// delivery and connection lifecycle.
pub async fn subscribe_runs(
    state: &ServerState,
    client_id: super::ClientId,
    event_sink: ClientEventSink,
) -> Result<u64, bcode_workflow_store::WorkflowStoreError> {
    state
        .workflow_event_clients
        .lock()
        .await
        .insert(client_id, event_sink);
    latest_event_sequence(state)
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
    validate_workflow_definition_for_production(state, &request.definition)?;
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
        revision: Box::new(workflow_revision_snapshot(revision)),
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
        LocalApplicationOperationRequest {
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
    let preset = workflow_preset_from_mutation(request.preset, 1, super::current_time_ms());
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
        LocalApplicationOperationRequest {
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
            workflow_preset_snapshot(preset),
        )),
        Err(error) => {
            record_authoring_conflict(&state.metrics, "update_preset");
            Ok(bcode_ipc::WorkflowPresetUpdateResult::Conflict(
                authoring_conflict_result(error)?,
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
        LocalApplicationOperationRequest {
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
            record_authoring_conflict(&state.metrics, "delete_preset");
            Ok(bcode_ipc::WorkflowAuthoringMutationResult::Conflict(
                authoring_conflict_result(error)?,
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
        LocalApplicationOperationRequest {
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
        LocalApplicationOperationRequest {
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
            record_authoring_conflict(&state.metrics, "activate");
            Ok(bcode_ipc::WorkflowAuthoringMutationResult::Conflict(
                authoring_conflict_result(error)?,
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
        LocalApplicationOperationRequest {
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
            record_authoring_conflict(&state.metrics, "discard_draft");
            Ok(bcode_ipc::WorkflowAuthoringMutationResult::Conflict(
                authoring_conflict_result(error)?,
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
        LocalApplicationOperationRequest {
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
        LocalApplicationOperationRequest {
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
        LocalApplicationOperationRequest {
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
            workflow_draft_snapshot(draft),
        ))),
        Err(bcode_workflow_store::WorkflowStoreError::AuthoringConflict {
            entity_id,
            expected,
            current,
        }) => {
            record_authoring_conflict(&state.metrics, "update_draft");
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
        LocalApplicationOperationRequest {
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
            workflow_draft_snapshot(draft),
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
    let compilation = run_computation(state, control, operation_id, move || {
        preview_document.compilation_preview(&catalog, None)
    })
    .await?;
    record_authoring_duration(
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
        LocalApplicationOperationRequest {
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
        LocalApplicationOperationRequest {
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
        workflow: authored_workflow_snapshot(workflow),
        draft: Box::new(workflow_draft_snapshot(draft)),
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

#[allow(clippy::too_many_lines)]
pub async fn inspect_run(
    state: &ServerState,
    run_id: &str,
    limit: usize,
) -> Result<bcode_ipc::WorkflowRunInspection, super::ServerError> {
    let (
        run,
        definition,
        terminal_output,
        activations,
        waits,
        mutation_approvals,
        attempts,
        events,
        decisions,
        grants,
        resource_leases,
        outputs,
        child_run_links,
        descendant_runs,
        repeat_outcomes,
        execution_session_links,
    ) = {
        let store = state
            .workflow_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let run = store.run_summary(run_id)?.ok_or_else(|| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                "workflow run not found: {run_id}"
            ))
        })?;
        let definition = store
            .definition(&run.definition_id, run.definition_version)?
            .ok_or_else(|| {
                bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                    "workflow definition not found: {} v{}",
                    run.definition_id, run.definition_version
                ))
            })?;
        let terminal_output = store.canonical_terminal_output(run_id)?.map(|output| {
            bcode_ipc::WorkflowTerminalOutputInspection {
                version: bcode_ipc::WORKFLOW_TERMINAL_OUTPUT_INSPECTION_VERSION,
                checksum_sha256: run
                    .terminal_output_checksum_sha256
                    .clone()
                    .expect("canonical terminal output has a checksum"),
                output_id: output.output_id,
                node_id: output.node_id,
                activation_id: output.activation_id,
                schema_id: output.schema_id,
                schema_version: output.schema_version,
                value: output.value,
                artifact_reference: output.artifact_reference,
                created_at_ms: output.created_at_ms,
            }
        });
        let descendant_runs = store.descendant_run_summaries(run_id, limit)?;
        let mut repeat_outcomes = store.repeat_outcomes(run_id, limit)?;
        for descendant in &descendant_runs {
            let remaining = limit.saturating_sub(repeat_outcomes.len());
            if remaining == 0 {
                break;
            }
            repeat_outcomes.extend(store.repeat_outcomes(&descendant.run.run_id, remaining)?);
        }
        let execution_session_links = store.execution_session_links_for_run(run_id, limit)?;
        (
            run,
            definition,
            terminal_output,
            store.activations_for_run(run_id, limit)?,
            store.waiting_activations(run_id, limit)?,
            store.pending_mutation_approvals(run_id, limit)?,
            store.attempt_history(run_id, None, limit)?,
            store.event_history(run_id, None, limit)?,
            store.decisions_for_run(run_id, limit)?,
            store.grants_for_run(run_id, limit)?,
            store.resource_leases_for_run(run_id, limit)?,
            store.output_summaries(run_id, limit)?,
            store.child_run_links(run_id, limit)?,
            descendant_runs,
            repeat_outcomes,
            execution_session_links,
        )
    };
    let mut child_sessions = Vec::with_capacity(execution_session_links.len());
    for link in execution_session_links {
        if child_sessions.len() >= limit {
            break;
        }
        let session_id = link.session_id.parse().map_err(|_| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(
                "workflow execution-session link has invalid session identity".to_string(),
            )
        })?;
        let summary = state.sessions.session_summary(session_id).await?;
        if summary.execution.as_ref().is_none_or(|execution| {
            execution.provenance.run_id != run_id
                || execution.provenance.node_id != link.node_id
                || execution.provenance.activation_id.as_deref()
                    != Some(link.activation_id.as_str())
                || execution.provenance.attempt != link.attempt
        }) {
            return Err(bcode_workflow_store::WorkflowStoreError::InvalidData(
                "workflow execution-session link conflicts with session provenance".to_string(),
            )
            .into());
        }
        child_sessions.push(summary);
    }
    let authority = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .execution_authority(run_id)?;
    let coordinator = match authority {
        Some(authority) => {
            let owned_by_this_daemon =
                authority.daemon_instance_id == state.daemon_status.instance_id;
            let controllable_from_this_daemon = if owned_by_this_daemon {
                true
            } else {
                match run
                    .parent_session_id
                    .as_deref()
                    .and_then(|value| value.parse::<super::SessionId>().ok())
                {
                    Some(session_id) => matches!(
                        prior_owner_liveness(state, session_id, &authority).await?,
                        PriorOwnerLiveness::ObservedEnded | PriorOwnerLiveness::NoLiveTrace
                    ),
                    None => false,
                }
            };
            Some(bcode_ipc::WorkflowCoordinatorStatus {
                target_artifact_id: authority.target_artifact_id,
                daemon_instance_id: authority.daemon_instance_id,
                owned_by_this_daemon,
                controllable_from_this_daemon,
            })
        }
        None => None,
    };
    Ok(bcode_ipc::WorkflowRunInspection {
        run,
        definition,
        terminal_output,
        activations,
        waits,
        mutation_approvals,
        attempts,
        events,
        decisions,
        grants,
        resource_leases,
        outputs,
        child_run_links,
        descendant_runs,
        repeat_outcomes,
        child_sessions,
        coordinator,
    })
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
