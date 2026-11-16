//! Invocation-bound product TUI host wire contract. No daemon or Tokio handles cross it.
mod retained_callbacks;
pub(super) use retained_callbacks::RetainedCallbacks;

use super::{
    PluginSessionViewSubscriptionRequest, PluginSessionViewUpdate,
    PluginStructuredGenerationRequest, PluginStructuredGenerationResult, PluginTuiHost,
    PluginTuiHostError, PluginWorkflowAuthoringDraft, PluginWorkflowAuthoringEditResult,
    PluginWorkflowAuthoringPublishResult, PluginWorkflowAuthoringRevision,
    PluginWorkflowControlAction, PluginWorkflowControlResult, PluginWorkflowGeneratedCandidate,
    PluginWorkflowGeneratedCandidateAcceptance, PluginWorkflowInspection, PluginWorkflowLookup,
    PluginWorkflowPackageExportStartRequest, PluginWorkflowStartRequest,
    PluginWorkflowStartResponse, PluginWorkflowSummary, PluginWorkflowTemplateInstantiationRequest,
    PluginWorkflowTemplateStartRequest, SessionId,
};
use bmux_plugin_sdk::definition::PluginHostServices;
use serde::{Deserialize, Serialize};
/// Exact typed TUI callback service.
pub const CONTRACT: &str = "bcode.tui.host/v1";
/// Maximum callback bytes.
pub const MAX_BYTES: usize = crate::SERVICE_BRIDGE_MAX_REQUEST_BYTES;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "operation",
    content = "arguments",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Request {
    /// Copy text using the exact frontend-owned clipboard policy.
    CopyText {
        text: String,
    },
    /// Invalidate the owning frontend only.
    Redraw {},
    /// Open one bounded semantic session observer on the actual host.
    SubscribeSession {
        request: PluginSessionViewSubscriptionRequest,
    },
    /// Read one observer update without replay or implicit resubscription.
    PollSession {
        subscription: uuid::Uuid,
    },
    /// Release one observer owned by this selected callback port.
    CloseSession {
        subscription: uuid::Uuid,
    },
    /// Start observable generation exactly once.
    StartGeneration {
        request: PluginStructuredGenerationRequest,
    },
    /// Read current observation and terminal outcome without starting new work.
    PollGeneration {
        generation: uuid::Uuid,
    },
    /// Request cancellation; this is not terminal acknowledgement.
    CancelGeneration {
        generation: uuid::Uuid,
    },
    WorkflowSchedulingPage {
        request: bcode_workflow::WorkflowSchedulingPageRequest,
    },
    ValidateJudgementSetup {
        selection: bcode_model::judgement::SetupSelection,
    },
    JudgementSetup {},
    UsageReport {
        query: bcode_usage_models::UsageQuery,
    },
    CollectUsage {
        session_id: SessionId,
        query: bcode_session_models::SessionUsageQuery,
    },
    WorkflowDelegationPreflight {
        plugin_id: String,
    },
    StartWorkflow {
        request: PluginWorkflowStartRequest,
    },
    AssociatedWorkflow {
        lookup: PluginWorkflowLookup,
    },
    InspectAssociatedWorkflow {
        lookup: PluginWorkflowLookup,
        limit: usize,
    },
    ControlAssociatedWorkflow {
        lookup: PluginWorkflowLookup,
        action: PluginWorkflowControlAction,
    },
    WorkflowAuthoringCatalog {},
    WorkflowLaunchCatalog {
        request: bcode_workflow::WorkflowLaunchCatalogRequest,
    },
    WorkflowLaunchDetail {
        request: bcode_workflow::WorkflowLaunchDetailRequest,
    },
    PrepareFreshSession {
        existing: Option<SessionId>,
    },
    SessionWorkingDocument {
        request: bcode_session_models::SessionWorkingDocumentRequest,
    },
    GenerateStructuredOutput {
        request: PluginStructuredGenerationRequest,
    },
    AcceptGeneratedWorkflowCandidate {
        candidate: PluginWorkflowGeneratedCandidate,
    },
    InstantiateWorkflowTemplate {
        request: PluginWorkflowTemplateInstantiationRequest,
    },
    WorkflowAuthoringDraft {
        workflow_id: String,
        draft_id: String,
    },
    ForkWorkflowAuthoringRevision {
        workflow_id: String,
        revision: u64,
        draft_id: String,
        producer: bcode_workflow::WorkflowProducerProvenance,
    },
    WorkflowAuthoringRevision {
        workflow_id: String,
        revision: u64,
    },
    ApplyWorkflowAuthoringEdits {
        workflow_id: String,
        draft_id: String,
        batch: bcode_workflow::WorkflowAuthoringEditBatch,
        producer: bcode_workflow::WorkflowProducerProvenance,
    },
    ValidateWorkflowAuthoring {
        document: bcode_workflow::WorkflowAuthoringDocument,
    },
    ApplyWorkflowSource {
        source_format: bcode_workflow::WorkflowSourceFormat,
        source: String,
        draft_id: String,
    },
    PreviewWorkflowAuthoring {
        document: bcode_workflow::WorkflowAuthoringDocument,
        configuration: Option<serde_json::Value>,
    },
    PublishWorkflowAuthoringDraft {
        workflow_id: String,
        draft_id: String,
        expected_generation: u64,
        activate: bool,
    },
    StartAuthoredWorkflowRevision {
        workflow_id: String,
        revision: u64,
        parent_session_id: SessionId,
        workspace_snapshot: Option<String>,
        configuration: Option<serde_json::Value>,
    },
    StartWorkflowPackageExport {
        request: PluginWorkflowPackageExportStartRequest,
    },
    StartWorkflowTemplate {
        request: PluginWorkflowTemplateStartRequest,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "response",
    content = "result",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Response {
    CopyText(Result<(), PluginTuiHostError>),
    Redrawn,
    /// Session observer identity is meaningful only within its bound callback port.
    Subscribed(Result<uuid::Uuid, PluginTuiHostError>),
    SessionUpdate(Result<Box<Option<PluginSessionViewUpdate>>, PluginTuiHostError>),
    Closed(Result<(), PluginTuiHostError>),
    GenerationStarted(Result<uuid::Uuid, PluginTuiHostError>),
    GenerationStatus(Result<Box<GenerationStatus>, PluginTuiHostError>),
    CancellationRequested(Result<(), PluginTuiHostError>),
    WorkflowSchedulingPage(
        Result<
            Box<
                bcode_workflow::WorkflowSchedulingPage<
                    bcode_workflow_view_models::WorkflowSchedulingView,
                >,
            >,
            PluginTuiHostError,
        >,
    ),
    ValidateJudgementSetup(Result<bool, PluginTuiHostError>),
    JudgementSetup(Result<Box<bcode_model::judgement::SetupCatalog>, PluginTuiHostError>),
    UsageReport(Result<Box<bcode_usage_models::UsageReport>, PluginTuiHostError>),
    CollectUsage(Result<Box<bcode_session_models::SessionUsagePage>, PluginTuiHostError>),
    WorkflowDelegationPreflight(
        Result<Box<bcode_workflow::WorkflowDelegationPreflight>, PluginTuiHostError>,
    ),
    StartWorkflow(Result<Box<PluginWorkflowStartResponse>, PluginTuiHostError>),
    AssociatedWorkflow(Result<Box<Option<PluginWorkflowSummary>>, PluginTuiHostError>),
    InspectAssociatedWorkflow(Result<Box<Option<PluginWorkflowInspection>>, PluginTuiHostError>),
    ControlAssociatedWorkflow(Result<Box<PluginWorkflowControlResult>, PluginTuiHostError>),
    WorkflowAuthoringCatalog(
        Result<Box<bcode_workflow::WorkflowAuthoringCatalogSnapshot>, PluginTuiHostError>,
    ),
    WorkflowLaunchCatalog(
        Result<Box<bcode_workflow::WorkflowLaunchCatalogPage>, PluginTuiHostError>,
    ),
    WorkflowLaunchDetail(Result<Box<bcode_workflow::WorkflowLaunchDetail>, PluginTuiHostError>),
    PrepareFreshSession(Result<SessionId, PluginTuiHostError>),
    SessionWorkingDocument(
        Result<Box<Option<bcode_session_models::SessionWorkingDocument>>, PluginTuiHostError>,
    ),
    GenerateStructuredOutput(Result<Box<PluginStructuredGenerationResult>, PluginTuiHostError>),
    AcceptGeneratedWorkflowCandidate(
        Result<Box<PluginWorkflowGeneratedCandidateAcceptance>, PluginTuiHostError>,
    ),
    InstantiateWorkflowTemplate(Result<Box<PluginWorkflowAuthoringDraft>, PluginTuiHostError>),
    WorkflowAuthoringDraft(Result<Box<Option<PluginWorkflowAuthoringDraft>>, PluginTuiHostError>),
    ForkWorkflowAuthoringRevision(Result<Box<PluginWorkflowAuthoringDraft>, PluginTuiHostError>),
    WorkflowAuthoringRevision(
        Result<Box<Option<PluginWorkflowAuthoringRevision>>, PluginTuiHostError>,
    ),
    ApplyWorkflowAuthoringEdits(Result<Box<PluginWorkflowAuthoringEditResult>, PluginTuiHostError>),
    ValidateWorkflowAuthoring(
        Result<Box<bcode_workflow::WorkflowValidationReport>, PluginTuiHostError>,
    ),
    ApplyWorkflowSource(Result<Box<bcode_workflow::WorkflowSourceApplyResult>, PluginTuiHostError>),
    PreviewWorkflowAuthoring(
        Result<Box<bcode_workflow::WorkflowCompilationPreview>, PluginTuiHostError>,
    ),
    PublishWorkflowAuthoringDraft(
        Result<Box<PluginWorkflowAuthoringPublishResult>, PluginTuiHostError>,
    ),
    StartAuthoredWorkflowRevision(Result<Box<PluginWorkflowStartResponse>, PluginTuiHostError>),
    StartWorkflowPackageExport(Result<Box<PluginWorkflowStartResponse>, PluginTuiHostError>),
    StartWorkflowTemplate(Result<Box<PluginWorkflowStartResponse>, PluginTuiHostError>),
}
/// One bounded observation of host-owned generation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerationStatus {
    pub session_id: Option<SessionId>,
    pub result: Option<Result<PluginStructuredGenerationResult, PluginTuiHostError>>,
}

/// Dispatch stateless calls only on the actual frontend-owned host supplied by the caller.
pub async fn dispatch(host: &dyn PluginTuiHost, request: Request) -> Response {
    match request {
        Request::CopyText { text } => Response::CopyText(host.copy_text(text)),
        Request::Redraw {} => {
            host.request_redraw();
            Response::Redrawn
        }
        Request::SubscribeSession { .. } => Response::Subscribed(Err(protocol_error())),
        Request::PollSession { .. } => Response::SessionUpdate(Err(protocol_error())),
        Request::CloseSession { .. } => Response::Closed(Err(protocol_error())),
        Request::StartGeneration { .. } => Response::GenerationStarted(Err(protocol_error())),
        Request::PollGeneration { .. } => Response::GenerationStatus(Err(protocol_error())),
        Request::CancelGeneration { .. } => Response::CancellationRequested(Err(protocol_error())),
        Request::WorkflowSchedulingPage { request } => Response::WorkflowSchedulingPage(
            host.workflow_scheduling_page(request).await.map(Box::new),
        ),
        Request::ValidateJudgementSetup { selection } => {
            Response::ValidateJudgementSetup(host.validate_judgement_setup(selection).await)
        }
        Request::JudgementSetup {} => {
            Response::JudgementSetup(host.judgement_setup().await.map(Box::new))
        }
        Request::UsageReport { query } => {
            Response::UsageReport(host.usage_report(query).await.map(Box::new))
        }
        Request::CollectUsage { session_id, query } => {
            Response::CollectUsage(host.collect_usage(session_id, query).await.map(Box::new))
        }
        Request::WorkflowDelegationPreflight { plugin_id } => {
            Response::WorkflowDelegationPreflight(
                host.workflow_delegation_preflight(plugin_id)
                    .await
                    .map(Box::new),
            )
        }
        Request::StartWorkflow { request } => {
            Response::StartWorkflow(host.start_workflow(request).await.map(Box::new))
        }
        Request::AssociatedWorkflow { lookup } => {
            Response::AssociatedWorkflow(host.associated_workflow(lookup).await.map(Box::new))
        }
        Request::InspectAssociatedWorkflow { lookup, limit } => {
            Response::InspectAssociatedWorkflow(
                host.inspect_associated_workflow(lookup, limit)
                    .await
                    .map(Box::new),
            )
        }
        Request::ControlAssociatedWorkflow { lookup, action } => {
            Response::ControlAssociatedWorkflow(
                host.control_associated_workflow(lookup, action)
                    .await
                    .map(Box::new),
            )
        }
        Request::WorkflowAuthoringCatalog {} => Response::WorkflowAuthoringCatalog(
            host.workflow_authoring_catalog().await.map(Box::new),
        ),
        Request::WorkflowLaunchCatalog { request } => Response::WorkflowLaunchCatalog(
            host.workflow_launch_catalog(request).await.map(Box::new),
        ),
        Request::WorkflowLaunchDetail { request } => {
            Response::WorkflowLaunchDetail(host.workflow_launch_detail(request).await.map(Box::new))
        }
        Request::PrepareFreshSession { existing } => {
            Response::PrepareFreshSession(host.prepare_fresh_session(existing).await)
        }
        Request::SessionWorkingDocument { request } => Response::SessionWorkingDocument(
            host.session_working_document(request).await.map(Box::new),
        ),
        Request::GenerateStructuredOutput { request } => Response::GenerateStructuredOutput(
            host.generate_structured_output(request).await.map(Box::new),
        ),
        request => dispatch_authoring(host, request).await,
    }
}

async fn dispatch_authoring(host: &dyn PluginTuiHost, request: Request) -> Response {
    match request {
        Request::AcceptGeneratedWorkflowCandidate { candidate } => {
            Response::AcceptGeneratedWorkflowCandidate(
                host.accept_generated_workflow_candidate(candidate)
                    .await
                    .map(Box::new),
            )
        }
        Request::InstantiateWorkflowTemplate { request } => Response::InstantiateWorkflowTemplate(
            host.instantiate_workflow_template(request)
                .await
                .map(Box::new),
        ),
        Request::WorkflowAuthoringDraft {
            workflow_id,
            draft_id,
        } => Response::WorkflowAuthoringDraft(
            host.workflow_authoring_draft(workflow_id, draft_id)
                .await
                .map(Box::new),
        ),
        Request::ForkWorkflowAuthoringRevision {
            workflow_id,
            revision,
            draft_id,
            producer,
        } => Response::ForkWorkflowAuthoringRevision(
            host.fork_workflow_authoring_revision(workflow_id, revision, draft_id, producer)
                .await
                .map(Box::new),
        ),
        Request::WorkflowAuthoringRevision {
            workflow_id,
            revision,
        } => Response::WorkflowAuthoringRevision(
            host.workflow_authoring_revision(workflow_id, revision)
                .await
                .map(Box::new),
        ),
        Request::ApplyWorkflowAuthoringEdits {
            workflow_id,
            draft_id,
            batch,
            producer,
        } => Response::ApplyWorkflowAuthoringEdits(
            host.apply_workflow_authoring_edits(workflow_id, draft_id, batch, producer)
                .await
                .map(Box::new),
        ),
        Request::ValidateWorkflowAuthoring { document } => Response::ValidateWorkflowAuthoring(
            host.validate_workflow_authoring(document)
                .await
                .map(Box::new),
        ),
        Request::ApplyWorkflowSource {
            source_format,
            source,
            draft_id,
        } => Response::ApplyWorkflowSource(
            host.apply_workflow_source(source_format, source, draft_id)
                .await
                .map(Box::new),
        ),
        Request::PreviewWorkflowAuthoring {
            document,
            configuration,
        } => Response::PreviewWorkflowAuthoring(
            host.preview_workflow_authoring(document, configuration)
                .await
                .map(Box::new),
        ),
        request => dispatch_publication(host, request).await,
    }
}

async fn dispatch_publication(host: &dyn PluginTuiHost, request: Request) -> Response {
    match request {
        Request::PublishWorkflowAuthoringDraft {
            workflow_id,
            draft_id,
            expected_generation,
            activate,
        } => Response::PublishWorkflowAuthoringDraft(
            host.publish_workflow_authoring_draft(
                workflow_id,
                draft_id,
                expected_generation,
                activate,
            )
            .await
            .map(Box::new),
        ),
        Request::StartAuthoredWorkflowRevision {
            workflow_id,
            revision,
            parent_session_id,
            workspace_snapshot,
            configuration,
        } => Response::StartAuthoredWorkflowRevision(
            host.start_authored_workflow_revision(
                workflow_id,
                revision,
                parent_session_id,
                workspace_snapshot,
                configuration,
            )
            .await
            .map(Box::new),
        ),
        Request::StartWorkflowPackageExport { request } => Response::StartWorkflowPackageExport(
            host.start_workflow_package_export(request)
                .await
                .map(Box::new),
        ),
        Request::StartWorkflowTemplate { request } => Response::StartWorkflowTemplate(
            host.start_workflow_template(request).await.map(Box::new),
        ),
        _ => Response::Closed(Err(protocol_error())),
    }
}
#[derive(Clone)]
pub(super) struct RemoteHost {
    pub services: PluginHostServices,
}
impl RemoteHost {
    /// Execute only the explicitly synchronous presentation callbacks.
    pub(super) fn synchronous(&self, request: &Request) -> Result<Response, PluginTuiHostError> {
        let payload = serde_json::to_vec(&request).map_err(|_| protocol_error())?;
        if payload.len() > MAX_BYTES {
            return Err(protocol_error());
        }
        let bytes = self
            .services
            .invoke(bmux_plugin_sdk::HostServiceRequest {
                kind: bmux_plugin_sdk::ServiceKind::Command,
                capability: CONTRACT,
                interface: CONTRACT,
                operation: "invoke",
                payload: &payload,
            })
            .map_err(|_| protocol_error())?;
        if bytes.len() > MAX_BYTES {
            return Err(protocol_error());
        }
        serde_json::from_slice(&bytes).map_err(|_| protocol_error())
    }

    pub(super) async fn request(&self, request: Request) -> Result<Response, PluginTuiHostError> {
        let payload = serde_json::to_vec(&request).map_err(|_| protocol_error())?;
        if payload.len() > MAX_BYTES {
            return Err(protocol_error());
        }
        let bytes = self
            .services
            .call_service_async(bmux_plugin_sdk::HostServiceOwnedRequest {
                capability: CONTRACT.to_owned(),
                kind: bmux_plugin_sdk::ServiceKind::Command,
                interface: CONTRACT.to_owned(),
                operation: "invoke".to_owned(),
                payload,
            })
            .await
            .map_err(|_| {
                PluginTuiHostError::Internal(
                    "selected frontend callback failed; do not replay".to_owned(),
                )
            })?;
        if bytes.len() > MAX_BYTES {
            return Err(protocol_error());
        }
        serde_json::from_slice(&bytes).map_err(|_| protocol_error())
    }
}
fn protocol_error() -> PluginTuiHostError {
    PluginTuiHostError::InvalidRequest("invalid bounded tui host protocol".to_owned())
}

#[cfg(test)]
mod tests {
    use super::{
        CONTRACT, GenerationStatus, PluginTuiHost, PluginTuiHostError, Request, Response, SessionId,
    };
    use crate::tui::{FocusedTextFieldEditResult, TokioPluginTuiHost};
    use bmux_plugin_sdk::definition::PluginHostServices;
    use std::sync::Arc;
    use tokio::sync::mpsc;
    #[tokio::test(flavor = "current_thread")]
    async fn async_product_proxy_uses_owned_callback_only_when_polled() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let services = PluginHostServices::new(|_| panic!("async proxy used synchronous callback"))
            .with_async(move |request| {
                let observed = Arc::clone(&observed);
                Box::pin(async move {
                    assert_eq!(request.capability, CONTRACT);
                    assert_eq!(request.interface, CONTRACT);
                    assert_eq!(request.operation, "invoke");
                    assert_eq!(request.kind, bmux_plugin_sdk::ServiceKind::Command);
                    let Request::PrepareFreshSession { existing } =
                        serde_json::from_slice(&request.payload).unwrap()
                    else {
                        panic!("wrong typed callback");
                    };
                    observed.fetch_add(1, Ordering::Relaxed);
                    Ok(
                        serde_json::to_vec(&Response::PrepareFreshSession(Ok(existing.unwrap())))
                            .unwrap(),
                    )
                })
            });
        let (sender, _) = mpsc::channel(1);
        let mut host = TokioPluginTuiHost::current(sender);
        host.set_product_host(Some(services));
        let id = SessionId::new();
        let future = host.prepare_fresh_session(Some(id));
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert_eq!(future.await.unwrap(), id);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn deferred_edit_admission_is_not_a_committed_mutation() {
        let pending = FocusedTextFieldEditResult::Pending;
        for unsettled in [pending, FocusedTextFieldEditResult::Unknown] {
            assert_ne!(unsettled, FocusedTextFieldEditResult::Applied);
            let bytes = serde_json::to_vec(&unsettled).unwrap();
            assert_eq!(
                serde_json::from_slice::<FocusedTextFieldEditResult>(&bytes).unwrap(),
                unsettled
            );
        }
        assert!(serde_json::from_str::<FocusedTextFieldEditResult>("\"future_result\"").is_err());
    }

    #[test]
    fn callback_wire_rejects_unknown_authority_and_preserves_nested_results() {
        let request = Request::CancelGeneration {
            generation: uuid::Uuid::new_v4(),
        };
        let value = serde_json::to_value(&request).unwrap();
        let decoded: Request = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), value);
        let mut forged = value;
        forged["arguments"]["activation"] = serde_json::json!("forged");
        assert!(serde_json::from_value::<Request>(forged).is_err());
        assert!(
            serde_json::from_str::<Request>(r#"{"operation":"future_callback","arguments":{}}"#)
                .is_err()
        );
        let response = Response::GenerationStatus(Ok(Box::new(GenerationStatus {
            session_id: None,
            result: Some(Err(PluginTuiHostError::PermissionDenied(
                "denied".to_owned(),
            ))),
        })));
        let encoded = serde_json::to_vec(&response).unwrap();
        let decoded: Response = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(
            serde_json::to_value(response).unwrap(),
            serde_json::to_value(decoded).unwrap()
        );
    }
}
