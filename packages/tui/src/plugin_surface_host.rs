//! Host adapter for native plugin-owned TUI surfaces.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bcode_client::BcodeClient;
use bcode_ipc::Event as BcodeEvent;
use bcode_plugin_sdk::tui::{
    PluginSessionViewSubscription, PluginSessionViewSubscriptionRequest, PluginSessionViewUpdate,
    PluginStructuredGenerationFuture, PluginStructuredGenerationRequest, PluginTask, PluginTuiHost,
    PluginTuiHostError, PluginWorkflowAuthoringCatalogFuture, PluginWorkflowAuthoringDraft,
    PluginWorkflowAuthoringDraftFuture, PluginWorkflowAuthoringEditFuture,
    PluginWorkflowAuthoringEditResult, PluginWorkflowAuthoringPreviewFuture,
    PluginWorkflowAuthoringPublishFuture, PluginWorkflowAuthoringPublishResult,
    PluginWorkflowAuthoringRevision, PluginWorkflowAuthoringRevisionFuture,
    PluginWorkflowAuthoringStartFuture, PluginWorkflowAuthoringValidationFuture,
    PluginWorkflowControlAction, PluginWorkflowControlFuture, PluginWorkflowControlResult,
    PluginWorkflowGeneratedCandidate, PluginWorkflowGeneratedCandidateAcceptance,
    PluginWorkflowGeneratedCandidateAcceptanceFuture, PluginWorkflowInspection,
    PluginWorkflowInspectionFuture, PluginWorkflowLookup, PluginWorkflowLookupFuture,
    PluginWorkflowPackageExportStartRequest, PluginWorkflowStartFuture, PluginWorkflowStartRequest,
    PluginWorkflowStartResponse, PluginWorkflowStatus, PluginWorkflowSummary,
    PluginWorkflowTemplateInstantiationFuture, PluginWorkflowTemplateInstantiationRequest,
};
use bcode_session_models::SessionId;
use bcode_session_view::SessionView;
use bcode_session_view_models::PermissionView;
use bcode_session_view_models::SessionConnectionViewStatus;
use bmux_tui_runtime::InvalidationSignal;
use tokio::sync::mpsc;

const DEFAULT_PLUGIN_SESSION_VIEW_BUFFER: usize = 32;
const MAX_PLUGIN_SESSION_VIEW_BUFFER: usize = 256;

/// Host services for plugin-owned TUI surfaces running inside Bcode's TUI.
#[derive(Debug, Clone)]
struct BcodePluginTuiHost {
    handle: tokio::runtime::Handle,
    redraw: InvalidationSignal,
    active_tasks: Arc<AtomicUsize>,
    client: BcodeClient,
}

impl BcodePluginTuiHost {
    /// Create a plugin TUI host from the current Tokio runtime.
    ///
    /// # Panics
    ///
    /// Panics if called outside a Tokio runtime.
    #[must_use]
    fn current(redraw: InvalidationSignal, client: BcodeClient) -> Self {
        Self {
            handle: tokio::runtime::Handle::current(),
            redraw,
            active_tasks: Arc::new(AtomicUsize::new(0)),
            client,
        }
    }
}

fn workflow_start_request(
    request: PluginWorkflowStartRequest,
) -> Result<bcode_ipc::WorkflowStartRequest, PluginTuiHostError> {
    let parent_scope = request.parent_session_id.to_string();
    if request.binding.scope_key != parent_scope {
        return Err(PluginTuiHostError::InvalidRequest(
            "workflow binding scope must match the active parent session".to_string(),
        ));
    }
    Ok(bcode_ipc::WorkflowStartRequest {
        identity: request.identity,
        definition: request.definition,
        run_id: request.run_id,
        workspace_snapshot: None,
        parent_session_id: request.parent_session_id,
        input: request.input,
        binding: bcode_workflow_store::WorkflowRunBinding {
            owner_plugin_id: request.binding.owner_plugin_id,
            workflow_kind: request.binding.workflow_kind,
            scope_key: request.binding.scope_key,
            display_label: request.binding.display_label,
            single_active: request.binding.single_active,
        },
        limits: bcode_workflow_store::WorkflowRunLimits::default(),
    })
}

fn workflow_package_export_start_request(
    request: PluginWorkflowPackageExportStartRequest,
) -> bcode_ipc::StartWorkflowPackageExportRequest {
    bcode_ipc::StartWorkflowPackageExportRequest {
        package_export: request.package_export,
        run_id: request.run_id,
        parent_session_id: request.parent_session_id,
        workspace_snapshot: request.workspace_snapshot,
        parent_session_generation: request.parent_session_generation,
        configuration: request.configuration,
        input: request.input,
    }
}

impl PluginTuiHost for BcodePluginTuiHost {
    fn spawn(&self, task: PluginTask) {
        let redraw = self.redraw.clone();
        let active_tasks = Arc::clone(&self.active_tasks);
        active_tasks.fetch_add(1, Ordering::AcqRel);
        drop(self.handle.spawn(async move {
            task.await;
            active_tasks.fetch_sub(1, Ordering::AcqRel);
            redraw.request();
        }));
    }

    fn spawn_blocking(&self, task: Box<dyn FnOnce() + Send + 'static>) {
        let redraw = self.redraw.clone();
        let active_tasks = Arc::clone(&self.active_tasks);
        active_tasks.fetch_add(1, Ordering::AcqRel);
        drop(self.handle.spawn_blocking(move || {
            task();
            active_tasks.fetch_sub(1, Ordering::AcqRel);
            redraw.request();
        }));
    }

    fn request_redraw(&self) {
        self.redraw.request();
    }

    fn copy_text(&self, text: String) -> Result<(), PluginTuiHostError> {
        let mut clipboard = arboard::Clipboard::new()
            .map_err(|error| PluginTuiHostError::Internal(error.to_string()))?;
        clipboard
            .set_text(text)
            .map_err(|error| PluginTuiHostError::Internal(error.to_string()))
    }

    fn start_workflow(&self, request: PluginWorkflowStartRequest) -> PluginWorkflowStartFuture {
        let client = self.client.clone();
        Box::pin(async move {
            let started = client
                .start_workflow(workflow_start_request(request)?)
                .await
                .map_err(|error| PluginTuiHostError::Internal(error.to_string()))?;
            Ok(PluginWorkflowStartResponse {
                run_id: started.run.run_id,
                runtime_work_id: started.runtime_work_id.to_string(),
            })
        })
    }

    fn associated_workflow(&self, lookup: PluginWorkflowLookup) -> PluginWorkflowLookupFuture {
        let client = self.client.clone();
        Box::pin(async move {
            client
                .associated_workflow_run(workflow_lookup(lookup))
                .await
                .map(|run| run.map(workflow_summary))
                .map_err(|error| PluginTuiHostError::Internal(error.to_string()))
        })
    }

    fn inspect_associated_workflow(
        &self,
        lookup: PluginWorkflowLookup,
        limit: usize,
    ) -> PluginWorkflowInspectionFuture {
        let client = self.client.clone();
        Box::pin(async move {
            client
                .inspect_associated_workflow_run(workflow_lookup(lookup), limit)
                .await
                .and_then(|inspection| inspection.map(workflow_inspection).transpose())
                .map_err(|error| PluginTuiHostError::Internal(error.to_string()))
        })
    }

    fn control_associated_workflow(
        &self,
        lookup: PluginWorkflowLookup,
        action: PluginWorkflowControlAction,
    ) -> PluginWorkflowControlFuture {
        let client = self.client.clone();
        Box::pin(async move {
            let (run, changed) = client
                .control_associated_workflow_run(
                    workflow_lookup(lookup),
                    match action {
                        PluginWorkflowControlAction::Pause => {
                            bcode_ipc::WorkflowRunControlAction::Pause
                        }
                        PluginWorkflowControlAction::Resume => {
                            bcode_ipc::WorkflowRunControlAction::Resume
                        }
                        PluginWorkflowControlAction::Cancel => {
                            bcode_ipc::WorkflowRunControlAction::Cancel
                        }
                    },
                )
                .await
                .map_err(|error| PluginTuiHostError::Internal(error.to_string()))?;
            Ok(PluginWorkflowControlResult {
                run: run.map(workflow_summary),
                changed,
            })
        })
    }

    fn workflow_authoring_catalog(&self) -> PluginWorkflowAuthoringCatalogFuture {
        let client = self.client.clone();
        Box::pin(async move {
            client
                .workflow_authoring_catalog()
                .await
                .map_err(|error| PluginTuiHostError::Internal(error.to_string()))
        })
    }

    fn generate_structured_output(
        &self,
        request: PluginStructuredGenerationRequest,
    ) -> PluginStructuredGenerationFuture {
        let client = self.client.clone();
        Box::pin(async move {
            if request.timeout_ms == 0 {
                return Err(PluginTuiHostError::InvalidRequest(
                    "structured generation timeout must be positive".to_string(),
                ));
            }
            let session = client
                .create_session(Some(request.session_name))
                .await
                .map_err(|error| PluginTuiHostError::Internal(error.to_string()))?;
            let prompt = format!("{}\n\n{}", request.system_prompt, request.prompt);
            client
                .send_user_message_with_execution(
                    session.id,
                    prompt,
                    bcode_ipc::PromptPlacement::FollowUp,
                    bcode_session_models::TurnExecutionOptions {
                        tools: bcode_session_models::TurnToolPolicy::Disabled,
                        structured_output: Some(
                            bcode_session_models::TurnStructuredOutputRequest {
                                name: request.output_name,
                                schema: request.output_schema,
                                strict: true,
                            },
                        ),
                        ..bcode_session_models::TurnExecutionOptions::default()
                    },
                )
                .await
                .map_err(|error| PluginTuiHostError::Internal(error.to_string()))?;
            let started = std::time::Instant::now();
            let mut cursor = None;
            let mut assistant = None;
            loop {
                let page = client
                    .session_history_page(
                        session.id,
                        bcode_session_models::SessionHistoryQuery {
                            cursor,
                            limit: 100,
                            direction: bcode_session_models::SessionHistoryDirection::Forward,
                        },
                    )
                    .await
                    .map_err(|error| PluginTuiHostError::Internal(error.to_string()))?;
                for event in page.events {
                    cursor = Some(bcode_session_models::SessionHistoryCursor {
                        sequence: event.sequence,
                    });
                    match event.kind {
                        bcode_session_models::SessionEventKind::AssistantMessage { text } => {
                            assistant = Some(text);
                        }
                        bcode_session_models::SessionEventKind::ModelTurnFinished {
                            outcome,
                            message,
                            ..
                        } => {
                            if outcome != bcode_session_models::ModelTurnOutcome::Completed {
                                return Err(PluginTuiHostError::Internal(format!(
                                    "structured generation ended with {outcome:?}: {}",
                                    message.unwrap_or_default()
                                )));
                            }
                            let text = assistant.ok_or_else(|| {
                                PluginTuiHostError::Internal(
                                    "structured generation returned no assistant payload"
                                        .to_string(),
                                )
                            })?;
                            return serde_json::from_str(&text).map_err(|error| {
                                PluginTuiHostError::Internal(format!(
                                    "structured generation returned invalid JSON: {error}"
                                ))
                            });
                        }
                        _ => {}
                    }
                }
                if started.elapsed() >= std::time::Duration::from_millis(request.timeout_ms) {
                    let _ = client.cancel_session_turn(session.id).await;
                    return Err(PluginTuiHostError::Internal(
                        "structured generation timed out".to_string(),
                    ));
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
    }

    fn accept_generated_workflow_candidate(
        &self,
        candidate: PluginWorkflowGeneratedCandidate,
    ) -> PluginWorkflowGeneratedCandidateAcceptanceFuture {
        let client = self.client.clone();
        Box::pin(async move {
            let producer = candidate.document.producer.clone();
            if producer.kind != bcode_workflow::WorkflowProducerKind::Generated {
                return Err(PluginTuiHostError::InvalidRequest(
                    "generated candidate provenance is required".to_string(),
                ));
            }
            if let Some(target) = candidate.target {
                return client
                    .update_workflow_draft(bcode_ipc::UpdateWorkflowDraftRequest {
                        workflow_id: target.workflow_id,
                        draft_id: target.draft_id,
                        expected_generation: target.expected_generation,
                        document: candidate.document,
                        producer,
                    })
                    .await
                    .map(|result| match result {
                        bcode_ipc::WorkflowDraftUpdateResult::Updated(draft) => {
                            PluginWorkflowGeneratedCandidateAcceptance::Updated(Box::new(
                                plugin_workflow_authoring_draft(*draft),
                            ))
                        }
                        bcode_ipc::WorkflowDraftUpdateResult::Conflict(conflict) => {
                            PluginWorkflowGeneratedCandidateAcceptance::Conflict {
                                expected_generation: conflict.expected_generation,
                                current_generation: conflict.current_generation,
                            }
                        }
                    })
                    .map_err(|error| PluginTuiHostError::Internal(error.to_string()));
            }
            let (_, draft) = client
                .create_authored_workflow(bcode_ipc::CreateAuthoredWorkflowRequest {
                    document: candidate.document,
                    draft_id: candidate.draft_id,
                })
                .await
                .map_err(|error| PluginTuiHostError::Internal(error.to_string()))?;
            Ok(PluginWorkflowGeneratedCandidateAcceptance::Created(
                Box::new(plugin_workflow_authoring_draft(draft)),
            ))
        })
    }

    fn instantiate_workflow_template(
        &self,
        request: PluginWorkflowTemplateInstantiationRequest,
    ) -> PluginWorkflowTemplateInstantiationFuture {
        let client = self.client.clone();
        Box::pin(async move {
            let (_, draft) = client
                .instantiate_workflow_template(bcode_ipc::WorkflowTemplateInstantiationRequest {
                    owner_plugin_id: request.owner_plugin_id,
                    template_id: request.template_id,
                    template_version: request.template_version,
                    workflow_id: request.workflow_id,
                    draft_id: request.draft_id,
                })
                .await
                .map_err(|error| PluginTuiHostError::Internal(error.to_string()))?;
            Ok(plugin_workflow_authoring_draft(draft))
        })
    }

    fn workflow_authoring_draft(
        &self,
        workflow_id: String,
        draft_id: String,
    ) -> PluginWorkflowAuthoringDraftFuture {
        let client = self.client.clone();
        Box::pin(async move {
            client
                .workflow_draft(workflow_id, draft_id)
                .await
                .map(|draft| draft.map(plugin_workflow_authoring_draft))
                .map_err(|error| PluginTuiHostError::Internal(error.to_string()))
        })
    }

    fn workflow_authoring_revision(
        &self,
        workflow_id: String,
        revision: u64,
    ) -> PluginWorkflowAuthoringRevisionFuture {
        let client = self.client.clone();
        Box::pin(async move {
            client
                .workflow_revision(workflow_id, revision)
                .await
                .map(|revision| revision.map(plugin_workflow_authoring_revision))
                .map_err(|error| PluginTuiHostError::Internal(error.to_string()))
        })
    }

    fn apply_workflow_authoring_edits(
        &self,
        workflow_id: String,
        draft_id: String,
        batch: bcode_workflow::WorkflowAuthoringEditBatch,
        producer: bcode_workflow::WorkflowProducerProvenance,
    ) -> PluginWorkflowAuthoringEditFuture {
        let client = self.client.clone();
        Box::pin(async move {
            client
                .apply_workflow_draft_edits(bcode_ipc::ApplyWorkflowDraftEditsRequest {
                    workflow_id,
                    draft_id,
                    batch,
                    producer,
                })
                .await
                .map(plugin_workflow_authoring_edit_result)
                .map_err(|error| PluginTuiHostError::Internal(error.to_string()))
        })
    }

    fn validate_workflow_authoring(
        &self,
        document: bcode_workflow::WorkflowAuthoringDocument,
    ) -> PluginWorkflowAuthoringValidationFuture {
        let client = self.client.clone();
        Box::pin(async move {
            client
                .validate_workflow_authoring(document)
                .await
                .map_err(|error| PluginTuiHostError::Internal(error.to_string()))
        })
    }

    fn preview_workflow_authoring(
        &self,
        document: bcode_workflow::WorkflowAuthoringDocument,
        configuration: Option<serde_json::Value>,
    ) -> PluginWorkflowAuthoringPreviewFuture {
        let client = self.client.clone();
        Box::pin(async move {
            client
                .preview_workflow_compilation(document, configuration)
                .await
                .map_err(|error| PluginTuiHostError::Internal(error.to_string()))
        })
    }

    fn publish_workflow_authoring_draft(
        &self,
        workflow_id: String,
        draft_id: String,
        expected_generation: u64,
        activate: bool,
    ) -> PluginWorkflowAuthoringPublishFuture {
        let client = self.client.clone();
        Box::pin(async move {
            client
                .publish_workflow_draft(bcode_ipc::PublishWorkflowDraftRequest {
                    workflow_id,
                    draft_id,
                    expected_generation,
                    configuration: None,
                    activate,
                    expected_active_revision: None,
                    control: bcode_ipc::WorkflowComputationControl::default(),
                })
                .await
                .map(plugin_workflow_authoring_publish_result)
                .map_err(|error| PluginTuiHostError::Internal(error.to_string()))
        })
    }

    fn start_authored_workflow_revision(
        &self,
        workflow_id: String,
        revision: u64,
        parent_session_id: SessionId,
        workspace_snapshot: Option<String>,
        configuration: Option<serde_json::Value>,
    ) -> PluginWorkflowAuthoringStartFuture {
        let client = self.client.clone();
        Box::pin(async move {
            client
                .start_authored_workflow(bcode_ipc::StartAuthoredWorkflowRequest {
                    selection: bcode_ipc::AuthoredWorkflowRunSelection::Revision {
                        workflow_id,
                        revision,
                    },
                    run_id: None,
                    parent_session_id,
                    workspace_snapshot,
                    parent_session_generation: None,
                    configuration,
                    input: None,
                })
                .await
                .map(|started| PluginWorkflowStartResponse {
                    run_id: started.started.run.run_id,
                    runtime_work_id: started.started.runtime_work_id.to_string(),
                })
                .map_err(|error| PluginTuiHostError::Internal(error.to_string()))
        })
    }

    fn start_workflow_package_export(
        &self,
        request: PluginWorkflowPackageExportStartRequest,
    ) -> PluginWorkflowAuthoringStartFuture {
        let client = self.client.clone();
        Box::pin(async move {
            client
                .start_workflow_package_export(workflow_package_export_start_request(request))
                .await
                .map(|started| PluginWorkflowStartResponse {
                    run_id: started.started.started.run.run_id,
                    runtime_work_id: started.started.started.runtime_work_id.to_string(),
                })
                .map_err(|error| PluginTuiHostError::Internal(error.to_string()))
        })
    }

    fn subscribe_session_view(
        &self,
        request: PluginSessionViewSubscriptionRequest,
    ) -> Result<PluginSessionViewSubscription, PluginTuiHostError> {
        let buffer = request
            .buffer
            .clamp(1, MAX_PLUGIN_SESSION_VIEW_BUFFER)
            .max(DEFAULT_PLUGIN_SESSION_VIEW_BUFFER.min(MAX_PLUGIN_SESSION_VIEW_BUFFER));
        let (sender, receiver) = mpsc::channel(buffer);
        let client = self.client.clone();
        let redraw = self.redraw.clone();
        drop(self.handle.spawn(async move {
            Box::pin(stream_plugin_session_view(client, request, sender, redraw)).await;
        }));
        Ok(PluginSessionViewSubscription { receiver })
    }
}

pub fn root_host(redraw: InvalidationSignal, client: BcodeClient) -> impl PluginTuiHost {
    BcodePluginTuiHost::current(redraw, client)
}

fn plugin_workflow_authoring_draft(
    draft: bcode_ipc::WorkflowDraftSnapshot,
) -> PluginWorkflowAuthoringDraft {
    PluginWorkflowAuthoringDraft {
        workflow_id: draft.identity.workflow_id,
        draft_id: draft.identity.draft_id,
        base_revision: draft.base_revision,
        generation: draft.generation,
        document: draft.document,
        producer: draft.producer,
    }
}

fn plugin_workflow_authoring_revision(
    revision: bcode_ipc::WorkflowRevisionSnapshot,
) -> PluginWorkflowAuthoringRevision {
    PluginWorkflowAuthoringRevision {
        workflow_id: revision.identity.workflow_id,
        revision: revision.identity.revision,
        document: revision.document,
    }
}

fn plugin_workflow_authoring_edit_result(
    result: bcode_ipc::WorkflowDraftEditResult,
) -> PluginWorkflowAuthoringEditResult {
    match result {
        bcode_ipc::WorkflowDraftEditResult::Updated(draft) => {
            PluginWorkflowAuthoringEditResult::Updated(Box::new(plugin_workflow_authoring_draft(
                *draft,
            )))
        }
        bcode_ipc::WorkflowDraftEditResult::Conflict(conflict) => {
            PluginWorkflowAuthoringEditResult::Conflict {
                expected_generation: conflict.expected_generation,
                current_generation: conflict.current_generation,
            }
        }
        bcode_ipc::WorkflowDraftEditResult::Rejected { diagnostics } => {
            PluginWorkflowAuthoringEditResult::Rejected { diagnostics }
        }
    }
}

fn plugin_workflow_authoring_publish_result(
    result: bcode_ipc::WorkflowPublicationResult,
) -> PluginWorkflowAuthoringPublishResult {
    match result {
        bcode_ipc::WorkflowPublicationResult::Published {
            revision,
            active_revision,
        } => PluginWorkflowAuthoringPublishResult::Published {
            revision: revision.identity.revision,
            activated: active_revision == Some(revision.identity.revision),
        },
        bcode_ipc::WorkflowPublicationResult::Conflict(conflict) => {
            PluginWorkflowAuthoringPublishResult::Conflict {
                expected_generation: conflict.expected_generation,
                current_generation: conflict.current_generation,
            }
        }
    }
}

fn workflow_lookup(lookup: PluginWorkflowLookup) -> bcode_ipc::WorkflowRunBindingLookup {
    bcode_ipc::WorkflowRunBindingLookup {
        owner_plugin_id: lookup.owner_plugin_id,
        workflow_kind: lookup.workflow_kind,
        scope_key: lookup.scope_key,
    }
}

const fn workflow_status(status: bcode_workflow_store::RunStatus) -> PluginWorkflowStatus {
    match status {
        bcode_workflow_store::RunStatus::Running => PluginWorkflowStatus::Running,
        bcode_workflow_store::RunStatus::Paused => PluginWorkflowStatus::Paused,
        bcode_workflow_store::RunStatus::Completed => PluginWorkflowStatus::Completed,
        bcode_workflow_store::RunStatus::Failed => PluginWorkflowStatus::Failed,
        bcode_workflow_store::RunStatus::Cancelled => PluginWorkflowStatus::Cancelled,
        bcode_workflow_store::RunStatus::RepairRequired => PluginWorkflowStatus::RepairRequired,
    }
}

fn workflow_summary(run: bcode_workflow_store::WorkflowRunSummary) -> PluginWorkflowSummary {
    PluginWorkflowSummary {
        run_id: run.run_id,
        definition_id: run.definition_id,
        definition_version: run.definition_version,
        status: workflow_status(run.status),
        cancellation_requested: run.cancellation_requested_at_ms.is_some(),
        created_at_ms: run.created_at_ms,
        updated_at_ms: run.updated_at_ms,
    }
}

fn workflow_inspection(
    inspection: bcode_ipc::WorkflowRunInspection,
) -> Result<PluginWorkflowInspection, bcode_client::ClientError> {
    Ok(PluginWorkflowInspection {
        run: workflow_summary(inspection.run),
        definition: serde_json::from_str(&inspection.definition.definition_json)
            .map_err(|_| bcode_client::ClientError::UnexpectedResponse)?,
        waits: inspection
            .waits
            .into_iter()
            .map(|value| {
                serde_json::to_value(value)
                    .map_err(|_| bcode_client::ClientError::UnexpectedResponse)
            })
            .collect::<Result<_, _>>()?,
        attempts: inspection
            .attempts
            .into_iter()
            .map(|value| {
                serde_json::to_value(value)
                    .map_err(|_| bcode_client::ClientError::UnexpectedResponse)
            })
            .collect::<Result<_, _>>()?,
        events: inspection
            .events
            .into_iter()
            .map(|value| {
                serde_json::to_value(value)
                    .map_err(|_| bcode_client::ClientError::UnexpectedResponse)
            })
            .collect::<Result<_, _>>()?,
        grants: inspection
            .grants
            .into_iter()
            .map(|value| {
                serde_json::to_value(value)
                    .map_err(|_| bcode_client::ClientError::UnexpectedResponse)
            })
            .collect::<Result<_, _>>()?,
        resource_leases: inspection
            .resource_leases
            .into_iter()
            .map(|value| {
                serde_json::to_value(value)
                    .map_err(|_| bcode_client::ClientError::UnexpectedResponse)
            })
            .collect::<Result<_, _>>()?,
        outputs: inspection
            .outputs
            .into_iter()
            .map(|value| {
                serde_json::to_value(value)
                    .map_err(|_| bcode_client::ClientError::UnexpectedResponse)
            })
            .collect::<Result<_, _>>()?,
        child_session_ids: inspection
            .child_sessions
            .into_iter()
            .map(|session| session.id)
            .collect(),
    })
}

async fn stream_plugin_session_view(
    client: BcodeClient,
    request: PluginSessionViewSubscriptionRequest,
    sender: mpsc::Sender<PluginSessionViewUpdate>,
    redraw: InvalidationSignal,
) {
    if let Err(error) =
        stream_plugin_session_view_inner(client, request, sender.clone(), redraw.clone()).await
    {
        let _ = sender
            .send(PluginSessionViewUpdate::Disconnected {
                message: error.to_string(),
            })
            .await;
        redraw.request();
    }
}

async fn attach_plugin_session_view(
    client: &BcodeClient,
    connection: &mut bcode_client::ClientConnection,
    request: &PluginSessionViewSubscriptionRequest,
) -> Result<SessionView, bcode_client::ClientError> {
    let attached = connection
        .attach_session_projection_window_with_input_history(
            request.session_id,
            request.projection.clone(),
        )
        .await?;
    let runtime = attached.runtime_selection;
    let mut view = SessionView::new();
    view.set_session_summary(attached.session);
    view.set_runtime_selection(
        runtime.provider_plugin_id,
        runtime.requested_model_id.or(runtime.model_id),
        runtime.effective_model_id,
        runtime.reasoning_effort,
        runtime.reasoning_summary,
        None,
    );
    view.set_agent_id(runtime.agent_id);
    view.set_reasoning_presentation_policy(request.reasoning_policy);
    if let Some(window) = attached.projection_window.as_ref() {
        view.set_history_window_metadata(
            window.source_range.map(|range| range.start_sequence),
            window.source_range.map(|range| range.end_sequence),
            window.has_older,
            window.has_newer,
        );
    }
    view.apply_history(&attached.history);
    let permissions = client_permission_views(
        client.list_permissions().await.unwrap_or_default(),
        request.session_id,
    );
    view.set_pending_permissions(permissions);
    if let Ok(runtime_work) = client.list_runtime_work(request.session_id).await {
        view.set_runtime_work_snapshots(&runtime_work);
    }
    if let Ok(interactions) =
        super::effects::load_pending_interactions(client, request.session_id).await
    {
        view.set_pending_interactions(interactions);
    }
    view.set_connection_status(SessionConnectionViewStatus::Attached);
    Ok(view)
}

fn client_permission_views(
    permissions: Vec<bcode_ipc::PermissionSummary>,
    session_id: SessionId,
) -> Vec<PermissionView> {
    permissions
        .into_iter()
        .filter(|permission| permission.session_id == session_id)
        .map(|permission| PermissionView {
            permission_id: permission.permission_id,
            session_id: Some(permission.session_id),
            tool_call_id: permission.tool_call_id,
            tool_name: permission.tool_name,
            arguments_json: permission.arguments_json,
            batch: permission
                .batch
                .map(|batch| bcode_session_view_models::PermissionBatchView {
                    batch_id: batch.batch_id,
                    call_index: batch.call_index,
                    call_count: batch.call_count,
                }),
            agent_id: permission.agent_id,
            title: Some("Permission requested".to_string()),
            policy_source: permission.policy_source,
            detail: permission.policy_reason,
            resolved: false,
            approved: None,
            can_remember: permission.can_remember_policy,
        })
        .collect()
}

async fn send_plugin_session_snapshot(
    sender: &mpsc::Sender<PluginSessionViewUpdate>,
    redraw: &InvalidationSignal,
    view: &SessionView,
) -> bool {
    if sender
        .send(PluginSessionViewUpdate::Snapshot(Box::new(
            view.snapshot().clone(),
        )))
        .await
        .is_err()
    {
        return false;
    }
    redraw.request();
    true
}

async fn stream_plugin_session_view_inner(
    client: BcodeClient,
    request: PluginSessionViewSubscriptionRequest,
    sender: mpsc::Sender<PluginSessionViewUpdate>,
    redraw: InvalidationSignal,
) -> Result<(), bcode_client::ClientError> {
    let session_id = request.session_id;
    let mut connection = client.connect("bcode-plugin-tui-session-view").await?;
    let mut view = attach_plugin_session_view(&client, &mut connection, &request).await?;
    if !send_plugin_session_snapshot(&sender, &redraw, &view).await {
        return Ok(());
    }

    let mut reconnect_delay = std::time::Duration::from_millis(100);
    loop {
        let needs_resync = match connection.recv_event().await {
            Ok(BcodeEvent::SessionViewResyncRequired {
                session_id: required,
            }) if required == session_id => true,
            Ok(event) => {
                let changed = match event {
                    BcodeEvent::Session(event) | BcodeEvent::RuntimeWork(event)
                        if event.session_id == session_id =>
                    {
                        view.apply_event(&event);
                        true
                    }
                    BcodeEvent::SessionLive(event) if event.session_id == session_id => {
                        view.apply_live_event(&event);
                        true
                    }
                    BcodeEvent::Session(_)
                    | BcodeEvent::SessionLive(_)
                    | BcodeEvent::RuntimeWork(_)
                    | BcodeEvent::SessionViewResyncRequired { .. }
                    | BcodeEvent::SessionCatalogUpdated { .. } => false,
                };
                if changed && !send_plugin_session_snapshot(&sender, &redraw, &view).await {
                    return Ok(());
                }
                false
            }
            Err(_error) => true,
        };
        if !needs_resync {
            continue;
        }

        view.set_connection_status(SessionConnectionViewStatus::Reconnecting);
        if !send_plugin_session_snapshot(&sender, &redraw, &view).await {
            return Ok(());
        }
        drop(connection);
        loop {
            if sender.is_closed() {
                return Ok(());
            }
            if let Ok(mut next_connection) = client.connect("bcode-plugin-tui-session-view").await
                && let Ok(next_view) =
                    attach_plugin_session_view(&client, &mut next_connection, &request).await
            {
                view = next_view;
                if !send_plugin_session_snapshot(&sender, &redraw, &view).await {
                    return Ok(());
                }
                connection = next_connection;
                reconnect_delay = std::time::Duration::from_millis(100);
                break;
            }
            tokio::time::sleep(reconnect_delay).await;
            reconnect_delay = (reconnect_delay * 2).min(std::time::Duration::from_secs(2));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PluginWorkflowPackageExportStartRequest, SessionId, workflow_package_export_start_request,
    };

    #[test]
    fn bmux_invalidation_signal_coalesces_plugin_redraw_requests() {
        let redraw = bmux_tui_runtime::InvalidationSignal::new();
        redraw.request();
        redraw.request();

        assert!(redraw.take());
        assert!(!redraw.take());
        assert_eq!(redraw.requests(), 2);
        assert_eq!(redraw.coalesced(), 1);
    }

    #[test]
    fn plugin_surface_host_source_uses_bmux_redraw_latch() {
        let source = include_str!("plugin_surface_host.rs");
        assert!(source.contains("InvalidationSignal"));
    }

    #[test]
    fn plugin_surface_host_adapts_portable_package_export_start() {
        let parent_session_id = SessionId::new();
        let request =
            workflow_package_export_start_request(PluginWorkflowPackageExportStartRequest {
                package_export: bcode_workflow::WorkflowPackageExportIdentity {
                    package_id: "example/package".to_string(),
                    export: "main".to_string(),
                    package_lock_digest_sha256: Some("a".repeat(64)),
                },
                run_id: Some("run-1".to_string()),
                parent_session_id,
                workspace_snapshot: Some("workspace".to_string()),
                parent_session_generation: Some(1),
                configuration: None,
                input: Some(serde_json::json!({"subject": "change"})),
            });
        assert_eq!(request.package_export.package_id, "example/package");
        assert_eq!(request.parent_session_id, parent_session_id);
        assert_eq!(request.input.expect("input")["subject"], "change");
    }

    #[test]
    fn plugin_surface_host_exposes_portable_workflow_authoring_services() {
        let source = include_str!("plugin_surface_host.rs");
        for service in [
            "workflow_authoring_catalog",
            "generate_structured_output",
            "accept_generated_workflow_candidate",
            "instantiate_workflow_template",
            "workflow_authoring_draft",
            "workflow_authoring_revision",
            "apply_workflow_authoring_edits",
            "validate_workflow_authoring",
            "preview_workflow_authoring",
            "publish_workflow_authoring_draft",
            "start_authored_workflow_revision",
            "start_workflow_package_export",
        ] {
            assert!(
                source.contains(service),
                "missing authoring service: {service}"
            );
        }
        assert!(!source.contains(concat!("super::terminal_", "events")));
        assert!(!source.contains(concat!("Terminal<", "&mut")));
    }
}
