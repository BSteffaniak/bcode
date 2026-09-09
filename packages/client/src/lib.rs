#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Programmatic client API for Bcode.

use bcode_agent_profile::{AgentInfo, PolicyStatusResponse};
use bcode_daemon_lifecycle::{DaemonStartError, EnsureDaemonOptions, ensure_daemon_running};
use bcode_ipc::{
    ClientRuntimeContext, CodecError, EnvelopeKind, ErrorResponse, Event, IpcEndpoint,
    LocalIpcStream, PluginServiceResponse, PluginServiceSummary, Request, Response,
    ResponsePayload, ServerStopMode, SessionBulkMigrationOperationStatus,
    SessionBulkMigrationStartRequest, SessionCompatibilityInventoryRequest,
    SessionCompatibilityInventoryResponse, WorktreeCreateOperationStatus, WorktreeCreateRequest,
    WorktreeCreateResponse, WorktreeListRequest, WorktreeListResponse, WorktreeRemoveRequest,
    WorktreeRemoveResponse, current_working_directory, decode_event, decode_response,
    default_endpoint, recv_envelope, request_envelope, send_envelope,
};
use bcode_plugin_sdk::PluginContributions;
use bcode_ralph_models::{
    RalphApproveRequest, RalphCancelRequest, RalphCancelResponse, RalphLifecycleRequest,
    RalphListIterationsRequest, RalphListIterationsResponse, RalphListRunsRequest,
    RalphListRunsResponse, RalphResumeRequest, RalphResumeResponse, RalphRunRequest,
    RalphRunResponse, RalphRunStatusRequest, RalphRunStatusResponse, RalphStatusRequest,
    RalphStatusResponse,
};
use bcode_session_import::ImportWarning as SessionImportWarning;
use bcode_session_models::{
    ClientId, ProjectionWindowRequest, SessionCatalogSourceStatus, SessionCatalogStatus,
    SessionDerivationPromptPage, SessionDerivationPromptQuery, SessionDerivationRequest,
    SessionDerivationSourceSnapshot, SessionDerivationTerminalOutcome, SessionEvent,
    SessionEventKind, SessionHistoryAroundQuery, SessionHistoryPage, SessionHistoryQuery,
    SessionHistoryWindow, SessionId, SessionInputHistoryEntry, SessionInspectionPage,
    SessionInspectionQuery, SessionSummary, WorkId,
};
use bcode_session_models::{PendingToolExchangeSummary, PermissionSummary};
use bcode_skill_models::{SkillId, SkillList, SkillManifest};
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;

const DEFAULT_CLIENT_IPC_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_CLIENT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_CLIENT_DAEMON_START_TIMEOUT: Duration = Duration::from_secs(30);
const LONG_POLL_TRANSPORT_GRACE: Duration = Duration::from_secs(5);

pub use bcode_session_models::SessionArtifactRange;

fn validate_artifact_range_response(
    range: &SessionArtifactRange,
    artifact_id: &str,
    reference_key: &str,
    offset: u64,
    length: u32,
) -> Result<(), ClientError> {
    if range.artifact_id != artifact_id
        || range.reference_key != reference_key
        || range.offset != offset
        || offset > range.total_bytes
        || range.bytes.len() as u64 > u64::from(length)
        || range.bytes.len() as u64
            > u64::from(bcode_session_models::MAX_SESSION_ARTIFACT_RANGE_BYTES)
        || (length > 0 && range.bytes.is_empty() && offset < range.total_bytes)
        || (!range.bytes.is_empty()
            && offset
                .checked_add(range.bytes.len() as u64)
                .is_none_or(|end| end > range.total_bytes))
    {
        return Err(ClientError::Protocol(
            "artifact range response does not match the requested range".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod artifact_range_tests {
    use super::SessionArtifactRange;

    #[test]
    fn range_metadata_supports_eof_and_replacement_detection() {
        let range = SessionArtifactRange {
            artifact_id: "artifact".to_owned(),
            reference_key: "recording".to_owned(),
            content_type: Some("application/octet-stream".to_owned()),
            offset: 8,
            total_bytes: 10,
            reference_bytes: Some(10),
            reference_revision: 42,
            finalized: true,
            finalized_event_seq: Some(42),
            availability: Some("complete".to_owned()),
            complete: Some(true),
            checksum_sha256: Some("abc".to_owned()),
            bytes: b"89".to_vec(),
        };
        super::validate_artifact_range_response(&range, "artifact", "recording", 8, 2)
            .expect("valid range");
        for (id, key, offset, length) in [
            ("other", "recording", 8, 2),
            ("artifact", "other", 8, 2),
            ("artifact", "recording", 7, 2),
            ("artifact", "recording", 8, 1),
        ] {
            assert!(
                super::validate_artifact_range_response(&range, id, key, offset, length).is_err()
            );
        }
        let mut invalid = range.clone();
        invalid.total_bytes = 9;
        assert!(
            super::validate_artifact_range_response(&invalid, "artifact", "recording", 8, 2)
                .is_err()
        );
        invalid.offset = u64::MAX;
        invalid.total_bytes = u64::MAX;
        assert!(
            super::validate_artifact_range_response(&invalid, "artifact", "recording", u64::MAX, 2)
                .is_err()
        );
        invalid.bytes.clear();
        invalid.offset = 8;
        assert!(
            super::validate_artifact_range_response(&invalid, "artifact", "recording", 8, 2)
                .is_err()
        );
        invalid.offset = u64::MAX;
        super::validate_artifact_range_response(&invalid, "artifact", "recording", u64::MAX, 2)
            .expect("empty EOF range");
        assert_eq!(range.next_offset(), 10);
        assert!(range.is_eof());
        assert_eq!(range.finalized_event_seq, Some(42));
        assert_eq!(range.checksum_sha256.as_deref(), Some("abc"));
    }
}

/// Domain-owned history result retained here for source compatibility.
pub use bcode_session_models::RuntimeWorkSpan;

const fn runtime_work_span_without_start(work_id: WorkId) -> RuntimeWorkSpan {
    RuntimeWorkSpan {
        work_id,
        parent_work_id: None,
        label: String::new(),
        status: None,
        started_at_ms: None,
        finished_at_ms: None,
        cancelled: false,
        message: None,
    }
}

fn runtime_work_spans(events: Vec<SessionEvent>) -> Vec<RuntimeWorkSpan> {
    let mut spans = BTreeMap::<WorkId, RuntimeWorkSpan>::new();
    for event in events {
        let (SessionEventKind::RuntimeWorkStarted { work_id, .. }
        | SessionEventKind::RuntimeWorkCancelRequested { work_id, .. }
        | SessionEventKind::RuntimeWorkProgress { work_id, .. }
        | SessionEventKind::RuntimeWorkFinished { work_id, .. }) = &event.kind
        else {
            continue;
        };
        if spans
            .get(work_id)
            .and_then(|span| span.status)
            .is_some_and(bcode_session_models::RuntimeWorkStatus::is_terminal)
        {
            continue;
        }
        match event.kind {
            SessionEventKind::RuntimeWorkStarted {
                work_id,
                label,
                parent_work_id,
                started_at_ms,
                ..
            } => {
                spans.insert(
                    work_id.clone(),
                    RuntimeWorkSpan {
                        work_id,
                        parent_work_id,
                        label,
                        status: None,
                        started_at_ms,
                        finished_at_ms: None,
                        cancelled: false,
                        message: None,
                    },
                );
            }
            SessionEventKind::RuntimeWorkCancelRequested { work_id, .. } => {
                spans
                    .entry(work_id.clone())
                    .or_insert_with(|| runtime_work_span_without_start(work_id))
                    .cancelled = true;
            }
            SessionEventKind::RuntimeWorkProgress {
                work_id, message, ..
            } => {
                spans
                    .entry(work_id.clone())
                    .or_insert_with(|| runtime_work_span_without_start(work_id))
                    .message = Some(message);
            }
            SessionEventKind::RuntimeWorkFinished {
                work_id,
                status,
                finished_at_ms,
                message,
            } => {
                let span = spans
                    .entry(work_id.clone())
                    .or_insert_with(|| runtime_work_span_without_start(work_id));
                span.status = Some(status);
                span.finished_at_ms = finished_at_ms;
                if message.is_some() {
                    span.message = message;
                }
            }
            _ => {}
        }
    }
    spans.into_values().collect()
}

#[cfg(test)]
mod runtime_work_history_tests {
    use super::*;
    use bcode_session_models::RuntimeWorkStatus;

    fn event(kind: SessionEventKind) -> SessionEvent {
        SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 1,
            timestamp_ms: 1,
            session_id: SessionId::new(),
            provenance: None,
            kind,
        }
    }

    #[test]
    fn span_domain_contract_preserves_json_and_unknown_duration() {
        let wire = serde_json::json!({
            "work_id": "partial", "parent_work_id": null, "label": "",
            "status": "cancelled", "started_at_ms": null, "finished_at_ms": 50,
            "cancelled": false, "message": "finished λ",
        });
        let domain: bcode_session_models::RuntimeWorkSpan =
            serde_json::from_value(wire.clone()).unwrap();
        let compatible: RuntimeWorkSpan = domain;
        assert_eq!(serde_json::to_value(&compatible).unwrap(), wire);
        assert_eq!(compatible.duration_ms(), None);
        let mut reversed = compatible;
        reversed.started_at_ms = Some(60);
        assert_eq!(reversed.duration_ms(), Some(0));
        reversed.finished_at_ms = None;
        assert_eq!(reversed.duration_ms(), None);
    }

    fn started(work_id: &WorkId, label: &str, started_at_ms: u64) -> SessionEventKind {
        SessionEventKind::RuntimeWorkStarted {
            work_id: work_id.clone(),
            kind: bcode_session_models::RuntimeWorkKind::Tool,
            label: label.into(),
            tool_call_id: None,
            plugin_id: None,
            service_interface: None,
            operation: None,
            parent_work_id: Some(WorkId::new("parent")),
            started_at_ms: Some(started_at_ms),
            cancellable: true,
        }
    }

    #[test]
    fn complete_history_preserves_start_metadata_and_finish_message() {
        let work_id = WorkId::new("complete");
        let spans = runtime_work_spans(vec![
            event(started(&work_id, "inspect", 10)),
            event(SessionEventKind::RuntimeWorkProgress {
                work_id: work_id.clone(),
                message: "running".into(),
                progress_at_ms: None,
                completed_units: None,
                total_units: None,
            }),
            event(SessionEventKind::RuntimeWorkFinished {
                work_id: work_id.clone(),
                status: RuntimeWorkStatus::Completed,
                finished_at_ms: Some(40),
                message: Some("done".into()),
            }),
        ]);
        assert_eq!(
            spans,
            vec![RuntimeWorkSpan {
                work_id,
                parent_work_id: Some(WorkId::new("parent")),
                label: "inspect".into(),
                status: Some(RuntimeWorkStatus::Completed),
                started_at_ms: Some(10),
                finished_at_ms: Some(40),
                cancelled: false,
                message: Some("done".into()),
            }]
        );
        assert_eq!(spans[0].duration_ms(), Some(30));
    }

    #[test]
    fn terminal_history_cannot_be_reopened_or_overwritten() {
        let work_id = WorkId::new("terminal");
        for status in [
            RuntimeWorkStatus::Completed,
            RuntimeWorkStatus::Failed,
            RuntimeWorkStatus::TimedOut,
            RuntimeWorkStatus::Cancelled,
        ] {
            let terminal = event(SessionEventKind::RuntimeWorkFinished {
                work_id: work_id.clone(),
                status,
                finished_at_ms: Some(40),
                message: Some("authoritative".into()),
            });
            let expected = runtime_work_spans(vec![terminal.clone()]);
            let events = vec![
                terminal.clone(),
                terminal,
                event(started(&work_id, "stale start", 50)),
                event(SessionEventKind::RuntimeWorkProgress {
                    work_id: work_id.clone(),
                    message: "stale progress".into(),
                    progress_at_ms: None,
                    completed_units: None,
                    total_units: None,
                }),
                event(SessionEventKind::RuntimeWorkCancelRequested {
                    work_id: work_id.clone(),
                    requested_at_ms: None,
                    client_id: None,
                }),
                event(SessionEventKind::RuntimeWorkFinished {
                    work_id: work_id.clone(),
                    status: RuntimeWorkStatus::Suspended,
                    finished_at_ms: Some(90),
                    message: Some("stale finish".into()),
                }),
            ];
            assert_eq!(runtime_work_spans(events), expected);
        }
    }

    #[test]
    fn resumed_work_resets_the_previous_partial_attempt() {
        let work_id = WorkId::new("resumed");
        let mut events = vec![
            event(SessionEventKind::RuntimeWorkCancelRequested {
                work_id: work_id.clone(),
                requested_at_ms: None,
                client_id: None,
            }),
            event(SessionEventKind::RuntimeWorkFinished {
                work_id: work_id.clone(),
                status: RuntimeWorkStatus::Suspended,
                finished_at_ms: Some(20),
                message: Some("suspended".into()),
            }),
            event(started(&work_id, "resumed attempt", 30)),
        ];
        let spans = runtime_work_spans(events.clone());
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].label, "resumed attempt");
        assert_eq!(spans[0].started_at_ms, Some(30));
        assert_eq!(spans[0].finished_at_ms, None);
        assert_eq!(spans[0].status, None);
        assert_eq!(spans[0].message, None);
        assert!(!spans[0].cancelled);
        events.push(event(SessionEventKind::RuntimeWorkFinished {
            work_id,
            status: RuntimeWorkStatus::Completed,
            finished_at_ms: Some(50),
            message: None,
        }));
        let spans = runtime_work_spans(events);
        assert_eq!(spans[0].status, Some(RuntimeWorkStatus::Completed));
        assert_eq!(spans[0].duration_ms(), Some(20));
        assert_eq!(spans[0].message, None);
        assert!(!spans[0].cancelled);
    }

    #[test]
    fn bounded_history_retains_each_lifecycle_event_without_a_start() {
        let work_id = WorkId::new("partial");
        let kinds = [
            SessionEventKind::RuntimeWorkCancelRequested {
                work_id: work_id.clone(),
                requested_at_ms: Some(20),
                client_id: None,
            },
            SessionEventKind::RuntimeWorkProgress {
                work_id: work_id.clone(),
                message: "progress".into(),
                progress_at_ms: Some(30),
                completed_units: None,
                total_units: None,
            },
            SessionEventKind::RuntimeWorkFinished {
                work_id: work_id.clone(),
                status: RuntimeWorkStatus::Failed,
                finished_at_ms: Some(40),
                message: None,
            },
        ];
        for kind in &kinds {
            let spans = runtime_work_spans(vec![event(kind.clone())]);
            assert_eq!(spans.len(), 1);
            assert_eq!(spans[0].work_id, work_id);
            assert!(spans[0].label.is_empty());
            assert_eq!(spans[0].parent_work_id, None);
            assert_eq!(spans[0].started_at_ms, None);
            assert_eq!(spans[0].duration_ms(), None);
        }
        let spans = runtime_work_spans(kinds.into_iter().map(event).collect());
        assert_eq!(spans.len(), 1);
        assert!(spans[0].cancelled);
        assert_eq!(spans[0].message.as_deref(), Some("progress"));
        assert_eq!(spans[0].status, Some(RuntimeWorkStatus::Failed));
        assert_eq!(spans[0].finished_at_ms, Some(40));
        assert_eq!(spans[0].duration_ms(), None);
    }
}

/// Errors returned by the Bcode client.
#[derive(Debug, Error)]
pub enum ClientError {
    #[error("IPC transport error: {0}")]
    Transport(#[from] bcode_ipc::IpcTransportError),
    #[error("IPC codec error: {0}")]
    Codec(#[from] CodecError),
    #[error("daemon start error: {0}")]
    DaemonStart(#[from] DaemonStartError),
    #[error("server returned error {code}: {message}")]
    Server { code: String, message: String },
    #[error("daemon connection and handshake timed out after {timeout:?}")]
    ConnectTimeout { timeout: Duration },
    #[error("daemon startup timed out after {timeout:?}")]
    DaemonStartupTimeout { timeout: Duration },
    #[error("client request timed out after {timeout:?}")]
    RequestTimeout { timeout: Duration },
    #[error("incompatible daemon: {message}")]
    IncompatibleDaemon { message: String },
    #[error("client protocol error: {0}")]
    Protocol(String),
    #[error("worktree creation failed ({code}): {message}")]
    WorktreeCreate {
        code: String,
        message: String,
        created_path: Option<std::path::PathBuf>,
    },
    #[error("unexpected response payload")]
    UnexpectedResponse,
    #[error("unexpected IPC envelope kind")]
    UnexpectedEnvelope,
}

impl bcode_workflow::WorkflowRunApplication for BcodeClient {
    async fn start_workflow_template(
        &self,
        request: bcode_workflow::WorkflowTemplateStartRequest,
    ) -> Result<bcode_workflow::WorkflowRunStartResponse, Self::Error> {
        Self::start_workflow_template(self, request).await
    }

    async fn repair_workflow_attempt(
        &self,
        dispatch_identity: String,
        resolution: bcode_workflow::RepairResolution,
    ) -> Result<bcode_workflow::RepairResult, Self::Error> {
        Self::repair_workflow_attempt(self, dispatch_identity, resolution).await
    }
    async fn doctor_workflow_run(
        &self,
        run_id: String,
        limit: usize,
    ) -> Result<bcode_workflow::WorkflowDoctorReport, Self::Error> {
        Self::doctor_workflow_run(self, run_id, limit).await
    }
    async fn reconcile_orphaned_workflow_runs(
        &self,
        apply: bool,
        limit: usize,
    ) -> Result<bcode_workflow::OrphanedWorkflowRunReport, Self::Error> {
        Self::reconcile_orphaned_workflow_runs(self, apply, limit).await
    }

    async fn list_all_workflow_mutation_approvals(
        &self,
        limit: usize,
    ) -> Result<Vec<bcode_workflow::WorkflowMutationApprovalInspection>, Self::Error> {
        Self::list_all_workflow_mutation_approvals(self, limit).await
    }
    async fn list_workflow_mutation_approvals(
        &self,
        run_id: String,
        limit: usize,
    ) -> Result<Vec<bcode_workflow::WorkflowMutationApprovalInspection>, Self::Error> {
        Self::list_workflow_mutation_approvals(self, run_id, limit).await
    }
    async fn resolve_workflow_mutation_approval(
        &self,
        approval_id: String,
        decision: bcode_workflow::WorkflowMutationApprovalDecision,
    ) -> Result<bcode_workflow::WorkflowMutationApprovalResolution, Self::Error> {
        Self::resolve_workflow_mutation_approval(self, approval_id, decision).await
    }
    async fn workflow_attempt_history(
        &self,
        run_id: String,
        cursor: Option<bcode_workflow::AttemptCursor>,
        limit: usize,
    ) -> Result<Vec<bcode_workflow::AttemptSummary>, Self::Error> {
        Self::workflow_attempt_history(self, run_id, cursor, limit).await
    }
    async fn workflow_event_history(
        &self,
        run_id: String,
        after_sequence: Option<u64>,
        limit: usize,
    ) -> Result<Vec<bcode_workflow::WorkflowHistoryEvent>, Self::Error> {
        Self::workflow_event_history(self, run_id, after_sequence, limit).await
    }
    async fn retry_workflow_node(
        &self,
        run_id: String,
        node_id: String,
        activation_id: String,
        failed_attempt: u32,
    ) -> Result<bcode_workflow::WorkflowNodeRetryResult, Self::Error> {
        Self::retry_workflow_node(self, run_id, node_id, activation_id, failed_attempt).await
    }
    async fn list_workflow_waits(
        &self,
        run_id: String,
        limit: usize,
    ) -> Result<Vec<bcode_workflow::WaitingActivation>, Self::Error> {
        Self::list_workflow_waits(self, run_id, limit).await
    }
    async fn provide_workflow_input(
        &self,
        run_id: String,
        node_id: String,
        activation_id: String,
        value: serde_json::Value,
    ) -> Result<bcode_workflow::WaitingResolutionResult, Self::Error> {
        Self::provide_workflow_input(self, run_id, node_id, activation_id, value).await
    }
    async fn resolve_workflow_approval(
        &self,
        run_id: String,
        node_id: String,
        activation_id: String,
        approved: bool,
    ) -> Result<bcode_workflow::WaitingResolutionResult, Self::Error> {
        Self::resolve_workflow_approval(self, run_id, node_id, activation_id, approved).await
    }
    async fn start_workflow_run(
        &self,
        request: bcode_workflow::WorkflowRunStartRequest,
    ) -> Result<bcode_workflow::WorkflowRunStartResponse, Self::Error> {
        Self::start_workflow_run(self, request).await
    }
    async fn start_workflow(
        &self,
        request: bcode_workflow::WorkflowStartRequest,
    ) -> Result<bcode_workflow::WorkflowRunStartResponse, Self::Error> {
        Self::start_workflow(self, request).await
    }
    async fn workflow_live_event_catch_up(
        &self,
        after_sequence: u64,
        limit: usize,
    ) -> Result<bcode_workflow_view_models::WorkflowLiveEventPage, Self::Error> {
        match self
            .send_request(Request::WorkflowLiveEventCatchUp {
                after_sequence,
                limit,
            })
            .await?
        {
            ResponsePayload::WorkflowLiveEventCatchUp { page } => Ok(page),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }
    async fn associated_workflow_run(
        &self,
        key: bcode_workflow::WorkflowRunBindingLookup,
    ) -> Result<Option<bcode_workflow::WorkflowRunSummary>, Self::Error> {
        Self::associated_workflow_run(self, key).await
    }
    async fn inspect_associated_workflow_run(
        &self,
        key: bcode_workflow::WorkflowRunBindingLookup,
        limit: usize,
    ) -> Result<Option<bcode_workflow::WorkflowRunInspection>, Self::Error> {
        Self::inspect_associated_workflow_run(self, key, limit).await
    }
    async fn control_associated_workflow_run(
        &self,
        key: bcode_workflow::WorkflowRunBindingLookup,
        action: bcode_workflow::WorkflowRunControlAction,
    ) -> Result<(Option<bcode_workflow::WorkflowRunSummary>, bool), Self::Error> {
        Self::control_associated_workflow_run(self, key, action).await
    }
    async fn inspect_workflow_run_graph(
        &self,
        request: bcode_workflow::WorkflowRunGraphPageRequest,
    ) -> Result<bcode_workflow::WorkflowRunGraphInspection, Self::Error> {
        Self::inspect_workflow_run_graph(self, request).await
    }
    async fn list_workflow_runs(
        &self,
        limit: usize,
    ) -> Result<Vec<bcode_workflow::WorkflowRunSummary>, Self::Error> {
        Self::list_workflow_runs(self, limit).await
    }

    async fn workflow_run_outputs(
        &self,
        run_id: String,
        limit: usize,
    ) -> Result<Vec<bcode_workflow::WorkflowOutputInspection>, Self::Error> {
        Self::workflow_run_outputs(self, run_id, limit).await
    }
    async fn workflow_run_status(
        &self,
        run_id: String,
    ) -> Result<Option<bcode_workflow::WorkflowRunSummary>, Self::Error> {
        Self::workflow_run_status(self, run_id).await
    }
    async fn pause_workflow_run(&self, run_id: String) -> Result<bool, Self::Error> {
        Self::pause_workflow_run(self, run_id).await
    }

    async fn resume_workflow_run(&self, run_id: String) -> Result<bool, Self::Error> {
        Self::resume_workflow_run(self, run_id).await
    }
    async fn cancel_workflow_run(&self, run_id: String) -> Result<bool, Self::Error> {
        Self::cancel_workflow_run(self, run_id).await
    }
    async fn inspect_workflow_run(
        &self,
        run_id: String,
        limit: usize,
    ) -> Result<bcode_workflow::WorkflowRunInspection, Self::Error> {
        Self::inspect_workflow_run(self, run_id, limit).await
    }
    type Error = ClientError;

    async fn start_authored_workflow(
        &self,
        request: bcode_workflow::StartAuthoredWorkflowRequest,
    ) -> Result<bcode_workflow::AuthoredWorkflowRunStartResponse, Self::Error> {
        Self::start_authored_workflow(self, request).await
    }
}

impl bcode_workflow::WorkflowAuthoringApplication for BcodeClient {
    async fn instantiate_workflow_template(
        &self,
        request: bcode_workflow::WorkflowTemplateInstantiationRequest,
    ) -> Result<
        (
            bcode_workflow::AuthoredWorkflowSnapshot,
            bcode_workflow::WorkflowDraftSnapshot,
        ),
        Self::Error,
    > {
        Self::instantiate_workflow_template(self, request).await
    }

    async fn inspect_workflow_definition(
        &self,
        definition_id: String,
        version: u32,
    ) -> Result<Option<bcode_workflow::WorkflowDefinitionInspection>, Self::Error> {
        let stored =
            Self::describe_workflow_definition(self, definition_id.clone(), version).await?;
        if stored
            .as_ref()
            .is_some_and(|value| value.definition_id != definition_id || value.version != version)
        {
            let failure = bcode_workflow::WorkflowAuthoringFailure::StateUnavailable;
            return Err(ClientError::Server {
                code: failure.code().to_string(),
                message: failure.to_string(),
            });
        }
        stored
            .map(bcode_workflow::WorkflowDefinitionInspection::try_from)
            .transpose()
            .map_err(|failure| ClientError::Server {
                code: failure.code().to_string(),
                message: failure.to_string(),
            })
    }

    async fn register_workflow_definition(
        &self,
        request: bcode_workflow::WorkflowDefinitionRegistrationRequest,
    ) -> Result<bcode_workflow::StoredWorkflowDefinition, Self::Error> {
        Self::register_workflow_definition(self, request).await
    }

    async fn list_workflow_definitions(
        &self,
        limit: usize,
    ) -> Result<Vec<bcode_workflow::StoredWorkflowDefinition>, Self::Error> {
        Self::list_workflow_definitions(self, limit).await
    }
    async fn describe_workflow_definition(
        &self,
        definition_id: String,
        version: u32,
    ) -> Result<Option<bcode_workflow::StoredWorkflowDefinition>, Self::Error> {
        Self::describe_workflow_definition(self, definition_id, version).await
    }

    async fn publish_workflow_package(
        &self,
        request: bcode_workflow::PublishWorkflowPackageRequest,
    ) -> Result<bcode_workflow::WorkflowPackageMutationResult, Self::Error> {
        Self::publish_workflow_package(self, request).await
    }
    async fn apply_workflow_package(
        &self,
        request: bcode_workflow::ApplyWorkflowPackageRequest,
    ) -> Result<bcode_workflow::WorkflowPackageMutationResult, Self::Error> {
        Self::apply_workflow_package(self, request).await
    }
    async fn inspect_workflow_templates(
        &self,
        limit: usize,
    ) -> Result<Vec<bcode_workflow::WorkflowTemplateInspection>, Self::Error> {
        match self
            .send_request(Request::InspectWorkflowTemplates { limit })
            .await?
        {
            ResponsePayload::WorkflowTemplateInspections { templates } => Ok(templates),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }
    async fn inspect_workflow_template(
        &self,
        owner_plugin_id: String,
        template_id: String,
        template_version: u32,
    ) -> Result<Option<bcode_workflow::WorkflowTemplateInspection>, Self::Error> {
        match self
            .send_request(Request::InspectWorkflowTemplate {
                owner_plugin_id,
                template_id,
                template_version,
            })
            .await?
        {
            ResponsePayload::WorkflowTemplateInspection { template } => {
                Ok(template.map(|template| *template))
            }
            _ => Err(ClientError::UnexpectedResponse),
        }
    }
    async fn workflow_package_publication(
        &self,
        package_id: String,
    ) -> Result<Option<bcode_workflow::WorkflowPackagePublicationReceipt>, Self::Error> {
        Self::workflow_package_publication(self, package_id).await
    }
    async fn workflow_launch_catalog(
        &self,
        request: bcode_workflow::WorkflowLaunchCatalogRequest,
    ) -> Result<bcode_workflow::WorkflowLaunchCatalogPage, Self::Error> {
        Self::workflow_launch_catalog(self, request).await
    }
    async fn workflow_launch_detail(
        &self,
        request: bcode_workflow::WorkflowLaunchDetailRequest,
    ) -> Result<bcode_workflow::WorkflowLaunchDetail, Self::Error> {
        Self::workflow_launch_detail(self, request).await
    }
    async fn validate_workflow_authoring_with_control(
        &self,
        document: bcode_workflow::WorkflowAuthoringDocument,
        control: bcode_workflow::WorkflowComputationControl,
    ) -> Result<bcode_workflow::WorkflowValidationReport, Self::Error> {
        Self::validate_workflow_authoring_with_control(self, document, control).await
    }
    async fn preview_workflow_compilation_with_control(
        &self,
        document: bcode_workflow::WorkflowAuthoringDocument,
        configuration: Option<serde_json::Value>,
        control: bcode_workflow::WorkflowComputationControl,
    ) -> Result<bcode_workflow::WorkflowCompilationPreview, Self::Error> {
        Self::preview_workflow_compilation_with_control(self, document, configuration, control)
            .await
    }
    async fn validate_workflow_package(
        &self,
        request: bcode_workflow::WorkflowPackageComputationRequest,
    ) -> Result<bcode_workflow::WorkflowPackageValidationResult, Self::Error> {
        Self::validate_workflow_package(self, request).await
    }
    async fn preview_workflow_package(
        &self,
        request: bcode_workflow::WorkflowPackagePreviewRequest,
    ) -> Result<bcode_workflow::WorkflowPackagePreview, Self::Error> {
        Self::preview_workflow_package(self, request).await
    }
    async fn validate_workflow_source(
        &self,
        request: bcode_workflow::WorkflowSourceComputationRequest,
    ) -> Result<bcode_workflow::WorkflowSourceValidationResult, Self::Error> {
        Self::validate_workflow_source(self, request).await
    }
    async fn preview_workflow_source(
        &self,
        request: bcode_workflow::WorkflowSourcePreviewRequest,
    ) -> Result<bcode_workflow::WorkflowSourcePreviewResult, Self::Error> {
        Self::preview_workflow_source(self, request).await
    }
    async fn workflow_revision_requirement_inspection(
        &self,
        workflow_id: String,
        revision: u64,
    ) -> Result<Option<bcode_workflow::WorkflowRevisionRequirementInspection>, Self::Error> {
        Self::workflow_revision_requirement_inspection(self, workflow_id, revision).await
    }
    async fn workflow_authoring_catalog(
        &self,
    ) -> Result<bcode_workflow::WorkflowAuthoringCatalogSnapshot, Self::Error> {
        Self::workflow_authoring_catalog(self).await
    }
    async fn list_workflow_presets(
        &self,
        workflow_id: String,
        cursor: Option<bcode_workflow::WorkflowAuthoringListCursor>,
        limit: usize,
    ) -> Result<
        bcode_workflow::WorkflowAuthoringPage<
            bcode_workflow::WorkflowPresetSnapshot,
            bcode_workflow::WorkflowAuthoringListCursor,
        >,
        Self::Error,
    > {
        Self::list_workflow_presets(self, workflow_id, cursor, limit).await
    }
    async fn workflow_preset(
        &self,
        workflow_id: String,
        preset_id: String,
    ) -> Result<Option<bcode_workflow::WorkflowPresetSnapshot>, Self::Error> {
        Self::workflow_preset(self, workflow_id, preset_id).await
    }
    async fn list_workflow_revisions(
        &self,
        workflow_id: String,
        cursor: Option<bcode_workflow::WorkflowRevisionListCursor>,
        limit: usize,
    ) -> Result<
        bcode_workflow::WorkflowAuthoringPage<
            bcode_workflow::WorkflowRevisionSnapshot,
            bcode_workflow::WorkflowRevisionListCursor,
        >,
        Self::Error,
    > {
        Self::list_workflow_revisions(self, workflow_id, cursor, limit).await
    }
    async fn workflow_revision(
        &self,
        workflow_id: String,
        revision: u64,
    ) -> Result<Option<bcode_workflow::WorkflowRevisionSnapshot>, Self::Error> {
        Self::workflow_revision(self, workflow_id, revision).await
    }
    async fn list_workflow_drafts(
        &self,
        workflow_id: String,
        cursor: Option<bcode_workflow::WorkflowAuthoringListCursor>,
        limit: usize,
    ) -> Result<
        bcode_workflow::WorkflowAuthoringPage<
            bcode_workflow::WorkflowDraftSnapshot,
            bcode_workflow::WorkflowAuthoringListCursor,
        >,
        Self::Error,
    > {
        Self::list_workflow_drafts(self, workflow_id, cursor, limit).await
    }
    async fn workflow_draft(
        &self,
        workflow_id: String,
        draft_id: String,
    ) -> Result<Option<bcode_workflow::WorkflowDraftSnapshot>, Self::Error> {
        Self::workflow_draft(self, workflow_id, draft_id).await
    }
    async fn list_authored_workflows(
        &self,
        cursor: Option<bcode_workflow::WorkflowAuthoringListCursor>,
        limit: usize,
    ) -> Result<
        bcode_workflow::WorkflowAuthoringPage<
            bcode_workflow::AuthoredWorkflowSnapshot,
            bcode_workflow::WorkflowAuthoringListCursor,
        >,
        Self::Error,
    > {
        Self::list_authored_workflows(self, cursor, limit).await
    }
    async fn authored_workflow(
        &self,
        workflow_id: String,
    ) -> Result<Option<bcode_workflow::AuthoredWorkflowSnapshot>, Self::Error> {
        Self::authored_workflow(self, workflow_id).await
    }
    async fn inspect_authored_workflow(
        &self,
        workflow_id: String,
        limit: usize,
    ) -> Result<Option<bcode_workflow::AuthoredWorkflowInspection>, Self::Error> {
        Self::inspect_authored_workflow(self, workflow_id, limit).await
    }
    async fn import_workflow_draft(
        &self,
        request: bcode_workflow::ImportWorkflowDraftRequest,
    ) -> Result<bcode_workflow::WorkflowDraftImportResult, Self::Error> {
        Self::import_workflow_draft(self, request).await
    }
    async fn import_workflow_revision(
        &self,
        request: bcode_workflow::ImportWorkflowRevisionRequest,
    ) -> Result<bcode_workflow::WorkflowRevisionImportResult, Self::Error> {
        Self::import_workflow_revision(self, request).await
    }
    async fn import_workflow(
        &self,
        request: bcode_workflow::ImportWorkflowRequest,
    ) -> Result<
        (
            bcode_workflow::AuthoredWorkflowSnapshot,
            bcode_workflow::WorkflowDraftSnapshot,
        ),
        Self::Error,
    > {
        Self::import_workflow(self, request).await
    }
    async fn preview_workflow_import(
        &self,
        request: bcode_workflow::PreviewWorkflowImportRequest,
    ) -> Result<bcode_workflow::WorkflowImportPreview, Self::Error> {
        Self::preview_workflow_import(self, request).await
    }
    async fn export_workflow_revision(
        &self,
        request: bcode_workflow::ExportWorkflowRevisionRequest,
    ) -> Result<bcode_workflow::WorkflowExportBundle, Self::Error> {
        Self::export_workflow_revision(self, request).await
    }
    async fn create_workflow_preset(
        &self,
        request: bcode_workflow::CreateWorkflowPresetRequest,
    ) -> Result<bcode_workflow::WorkflowPresetSnapshot, Self::Error> {
        Self::create_workflow_preset(self, request).await
    }
    async fn update_workflow_preset(
        &self,
        request: bcode_workflow::UpdateWorkflowPresetRequest,
    ) -> Result<bcode_workflow::WorkflowPresetUpdateResult, Self::Error> {
        Self::update_workflow_preset(self, request).await
    }
    async fn delete_workflow_preset(
        &self,
        request: bcode_workflow::DeleteWorkflowPresetRequest,
    ) -> Result<bcode_workflow::WorkflowAuthoringMutationResult, Self::Error> {
        Self::delete_workflow_preset(self, request).await
    }
    async fn fork_workflow_draft(
        &self,
        request: bcode_workflow::ForkWorkflowDraftRequest,
    ) -> Result<bcode_workflow::WorkflowDraftSnapshot, Self::Error> {
        Self::fork_workflow_draft(self, request).await
    }
    async fn apply_workflow_source(
        &self,
        request: bcode_workflow::ApplyWorkflowSourceRequest,
    ) -> Result<bcode_workflow::WorkflowSourceApplyResult, Self::Error> {
        Self::apply_workflow_source(
            self,
            request.source_format,
            request.source,
            request.draft_id,
        )
        .await
    }

    async fn set_authored_workflow_archived(
        &self,
        request: bcode_workflow::SetAuthoredWorkflowArchivedRequest,
    ) -> Result<bcode_workflow::AuthoredWorkflowSnapshot, Self::Error> {
        Self::set_authored_workflow_archived(self, request).await
    }

    async fn cancel_workflow_computation(&self, operation_id: String) -> Result<bool, Self::Error> {
        Self::cancel_workflow_computation(self, operation_id).await
    }

    async fn create_authored_workflow(
        &self,
        request: bcode_workflow::CreateAuthoredWorkflowRequest,
    ) -> Result<
        (
            bcode_workflow::AuthoredWorkflowSnapshot,
            bcode_workflow::WorkflowDraftSnapshot,
        ),
        Self::Error,
    > {
        Self::create_authored_workflow(self, request).await
    }

    async fn publish_and_start_workflow(
        &self,
        request: bcode_workflow::PublishAndStartWorkflowRequest,
    ) -> Result<bcode_workflow::WorkflowPublishAndStartResult, Self::Error> {
        Self::publish_and_start_workflow(self, request).await
    }
    async fn publish_workflow_draft(
        &self,
        request: bcode_workflow::PublishWorkflowDraftRequest,
    ) -> Result<bcode_workflow::WorkflowPublicationResult, Self::Error> {
        Self::publish_workflow_draft(self, request).await
    }
    type Error = ClientError;

    async fn apply_workflow_draft_edits(
        &self,
        request: bcode_workflow::ApplyWorkflowDraftEditsRequest,
    ) -> Result<bcode_workflow::WorkflowDraftEditResult, Self::Error> {
        Self::apply_workflow_draft_edits(self, request).await
    }

    async fn update_workflow_draft(
        &self,
        request: bcode_workflow::UpdateWorkflowDraftRequest,
    ) -> Result<bcode_workflow::WorkflowDraftUpdateResult, Self::Error> {
        Self::update_workflow_draft(self, request).await
    }

    async fn activate_workflow_revision(
        &self,
        request: bcode_workflow::ActivateWorkflowRevisionRequest,
    ) -> Result<bcode_workflow::WorkflowAuthoringMutationResult, Self::Error> {
        Self::activate_workflow_revision(self, request).await
    }

    async fn discard_workflow_draft(
        &self,
        request: bcode_workflow::DiscardWorkflowDraftRequest,
    ) -> Result<bcode_workflow::WorkflowAuthoringMutationResult, Self::Error> {
        Self::discard_workflow_draft(self, request).await
    }
}

impl ClientError {
    /// Return the domain-owned auth-pool failure for a recognized server code.
    /// Transport failures and unknown future codes remain unclassified.
    // Repository policy avoids redundant must_use on Option-returning functions.
    #[allow(clippy::must_use_candidate)]
    pub fn auth_pool_error(&self) -> Option<bcode_provider_auth_models::AuthPoolOperationError> {
        match self {
            Self::Server { code, .. } => {
                bcode_provider_auth_models::AuthPoolOperationError::from_code(code)
            }
            _ => None,
        }
    }

    /// Return true when an optional domain is unavailable while unrelated daemon capabilities
    /// remain usable.
    #[must_use]
    pub fn is_optional_domain_unavailable(&self) -> bool {
        matches!(
            self,
            Self::Server { code, .. } if code == "workflow_capability_unavailable"
        )
    }

    /// Return true when the error means the local daemon transport is unavailable.
    #[must_use]
    pub fn is_daemon_unavailable(&self) -> bool {
        match self {
            Self::Transport(bcode_ipc::IpcTransportError::Io(error)) => matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof
            ),
            Self::Codec(CodecError::Io(error)) => matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof
            ),
            Self::DaemonStartupTimeout { .. } | Self::DaemonStart(_) => true,
            Self::ConnectTimeout { .. }
            | Self::RequestTimeout { .. }
            | Self::Transport(_)
            | Self::Codec(_)
            | Self::Server { .. }
            | Self::WorktreeCreate { .. }
            | Self::IncompatibleDaemon { .. }
            | Self::Protocol(_)
            | Self::UnexpectedResponse
            | Self::UnexpectedEnvelope => false,
        }
    }
}

/// Receiver and task for cancellable client-side observation of detached session preparation.
pub struct SessionOpenProgressObserver {
    /// Progress snapshots in operation revision order.
    pub receiver:
        tokio::sync::mpsc::UnboundedReceiver<bcode_session_models::SessionOpenOperationSnapshot>,
    /// Client observation task. Dropping the receiver ends this task but not server migration.
    pub task: tokio::task::JoinHandle<Result<(), ClientError>>,
}

/// Session list response with persistent catalog status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionList {
    pub sessions: Vec<SessionSummary>,
    pub catalog_status: SessionCatalogStatus,
    pub catalog_sources: Vec<SessionCatalogSourceStatus>,
    pub catalog_revision: u64,
}

/// History returned when attaching to a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachedSessionHistory {
    pub session: SessionSummary,
    pub history: Vec<SessionEvent>,
    pub input_history: Vec<SessionInputHistoryEntry>,
    pub usage_summary: bcode_session_models::SessionUsageSummary,
    pub import_warnings: Vec<SessionImportWarning>,
    pub draft: Option<String>,
    pub runtime_selection: bcode_session_models::SessionRuntimeSelection,
    /// Projection-window metadata when the attach used a semantic projection request.
    pub projection_window: Option<bcode_session_models::ProjectionWindow>,
}

const CLIENT_RUNTIME_ENV_VARS: &[&str] = &[
    "BCODE_OPENAI_API_KEY",
    "OPENAI_API_KEY",
    "BCODE_OPENAI_AUTH_MODE",
    "BCODE_OPENAI_AUTH_PROFILE",
    "BCODE_OPENAI_AUTH_VAULT",
    "BCODE_OPENAI_BASE_URL",
    "OPENAI_BASE_URL",
    "BCODE_OPENAI_MODEL",
    "OPENAI_MODEL",
    "BCODE_OPENAI_MODELS",
    "OPENAI_MODELS",
    "BCODE_OPENAI_DIALECT",
    "OPENAI_DIALECT",
    "BCODE_OPENAI_CODEX_ACCESS_TOKEN",
    "BCODE_OPENAI_CODEX_REFRESH_TOKEN",
    "BCODE_OPENAI_CODEX_ID_TOKEN",
    "BCODE_OPENAI_CODEX_EXPIRES_AT",
    "BCODE_OPENAI_CODEX_ACCOUNT_ID",
    "BCODE_XAI_AUTH_MODE",
    "BCODE_XAI_AUTH_PROFILE",
    "BCODE_XAI_AUTH_VAULT",
    "BCODE_XAI_API_KEY",
    "XAI_API_KEY",
    "BCODE_XAI_BASE_URL",
    "XAI_BASE_URL",
    "BCODE_XAI_MODEL",
    "XAI_MODEL",
    "BCODE_XAI_MODELS",
    "XAI_MODELS",
    "BCODE_BEDROCK_MODEL",
    "BEDROCK_MODEL",
    "BCODE_BEDROCK_MODELS",
    "BEDROCK_MODELS",
    "BCODE_BEDROCK_REGION",
    "BEDROCK_REGION",
    "BCODE_BEDROCK_AWS_PROFILE",
    "AWS_PROFILE",
    "AWS_REGION",
    "AWS_DEFAULT_REGION",
    "BCODE_BEDROCK_ENDPOINT_URL",
    "BEDROCK_ENDPOINT_URL",
    "AWS_ENDPOINT_URL_BEDROCK",
    "BCODE_BEDROCK_TRANSPORT",
    "BCODE_BEDROCK_MANTLE_BASE_URL",
    "BCODE_BEDROCK_MANTLE_AUTH_HEADER",
    "BCODE_BEDROCK_FORCE_HTTP1",
    "AWS_BEDROCK_FORCE_HTTP1",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_BEARER_TOKEN_BEDROCK",
];

fn resolve_caller_path(path: Option<std::path::PathBuf>) -> std::path::PathBuf {
    resolve_path_from(path, &current_working_directory())
}

fn resolve_path_from(
    path: Option<std::path::PathBuf>,
    caller_cwd: &std::path::Path,
) -> std::path::PathBuf {
    let path = path.map_or_else(
        || caller_cwd.to_path_buf(),
        |path| {
            if path.is_absolute() {
                path
            } else {
                caller_cwd.join(path)
            }
        },
    );
    path.canonicalize().unwrap_or(path)
}

fn current_runtime_context() -> Result<ClientRuntimeContext, ClientError> {
    let working_directory = current_working_directory();
    let config = bcode_config::load_config().map_err(|_| {
        ClientError::Protocol(
            "Client configuration could not be loaded; daemon defaults were not substituted."
                .to_owned(),
        )
    })?;
    let effective_config_toml =
        Some(bcode_config::encode_effective_config(&config).map_err(|_| {
            ClientError::Protocol(
                "Client configuration could not be encoded; daemon defaults were not substituted."
                    .to_owned(),
            )
        })?);
    let env = CLIENT_RUNTIME_ENV_VARS
        .iter()
        .filter_map(|name| match std::env::var(name) {
            Ok(value) if !value.trim().is_empty() => Some(((*name).to_string(), value)),
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    let resolved = config.resolved_model_selection();
    let provider_context = bcode_provider_auth::try_resolve_provider_request_context(
        bcode_provider_auth::ProviderRequestContextResolution {
            config: &config,
            selection: resolved.clone(),
        },
    ).map_err(|_| ClientError::Protocol(
        "Authentication selection could not be verified. Inspect the selected account, pool membership, and authentication metadata before connecting. No substitute account was selected.".to_owned(),
    ))?;
    Ok(runtime_context_from_selection(
        working_directory,
        effective_config_toml,
        resolved,
        provider_context,
        env,
    ))
}

fn runtime_context_from_selection(
    working_directory: std::path::PathBuf,
    effective_config_toml: Option<String>,
    resolved: bcode_config::ResolvedModelSelection,
    mut provider_context: bcode_model::ProviderRequestContext,
    process_env: BTreeMap<String, String>,
) -> ClientRuntimeContext {
    // Process environment retains its existing precedence, but do not mix credentials
    // from unselected pool candidates into the selected account's environment.
    provider_context.env.extend(process_env.clone());
    let env_keys = provider_context
        .env
        .keys()
        .cloned()
        .map(|key| (key, true))
        .collect();
    ClientRuntimeContext {
        working_directory: Some(working_directory),
        effective_config_toml: effective_config_toml.map(Box::new),
        selected_provider_plugin_id: resolved.provider_plugin_id,
        selected_model_id: resolved.model_id,
        requested_model_id: resolved.selected_model_id,
        provider_context,
        process_env,
        interaction_adapters: Vec::new(),
        env_keys,
    }
}

#[cfg(test)]
mod runtime_context_auth_tests {
    use super::*;

    #[test]
    fn provider_validation_decode_preserves_negative_result_and_redacts_errors() {
        let result = decode_provider_validation(&PluginServiceResponse {
            payload: br#"{"valid":false}"#.to_vec(),
            error: None,
        })
        .unwrap();
        assert!(!result.valid);
        for response in [
            PluginServiceResponse {
                payload: b"secret-token-invalid-json".to_vec(),
                error: None,
            },
            PluginServiceResponse {
                payload: Vec::new(),
                error: Some(bcode_ipc::PluginServiceError {
                    code: "secret-token".to_owned(),
                    message: "secret-token".to_owned(),
                }),
            },
        ] {
            let error = decode_provider_validation(&response).unwrap_err();
            assert!(!error.to_string().contains("secret-token"));
        }
    }

    #[test]
    fn single_account_selection_does_not_infer_a_pool_from_chatgpt_scheme() {
        let mut config = bcode_config::BcodeConfig::default();
        config.model.auth_profile = Some("custom-account".to_owned());
        config.auth.profiles.insert(
            "custom-account".to_owned(),
            bcode_config::AuthProfileConfig {
                backend: "env".to_owned(),
                scheme: Some("chatgpt".to_owned()),
                settings: BTreeMap::from([("provider".to_owned(), "openai".to_owned())]),
                ..Default::default()
            },
        );
        let context = bcode_provider_auth::resolve_provider_request_context_with_resolver(
            bcode_provider_auth::ProviderRequestContextResolution {
                config: &config,
                selection: config.resolved_model_selection(),
            },
            &bcode_config::RuntimeAuthSubscriptions::default(),
            |_, _| bcode_provider_auth::ResolvedProviderAuth::default(),
        );
        assert_eq!(context.auth_profile.as_deref(), Some("custom-account"));
        assert!(context.auth_pool.is_none());
        assert!(context.auth_candidates.is_empty());
    }

    #[tokio::test]
    async fn invalid_runtime_context_blocks_connection_before_transport() {
        let mut client = BcodeClient::new(default_endpoint());
        client.runtime_context_error = Some("Configuration unavailable".to_owned());
        assert!(
            matches!(client.connect("test").await, Err(ClientError::Protocol(message)) if message == "Configuration unavailable")
        );
        let explicit = client.with_runtime_context(Some(ClientRuntimeContext::default()));
        assert!(explicit.runtime_context_error.is_none());
    }

    #[test]
    fn selected_account_and_credentials_survive_client_adaptation_together() {
        let auth = bcode_model::ProviderAuthContext {
            profile: Some("preferred-account".to_owned()),
            credentials: BTreeMap::from([(
                "token".to_owned(),
                bcode_model::ProviderAuthCredential {
                    value: "preferred-token".to_owned(),
                    source: None,
                },
            )]),
            ..Default::default()
        };
        let provider_context = bcode_model::ProviderRequestContext {
            auth_profile: Some("preferred-account".to_owned()),
            auth: Some(auth.clone()),
            env: BTreeMap::from([("SELECTED".to_owned(), "selected-value".to_owned())]),
            auth_candidates: vec![bcode_model::ProviderAuthCandidate {
                profile: Some("other-account".to_owned()),
                auth: bcode_model::ProviderAuthContext::default(),
                env: BTreeMap::from([("OTHER_TOKEN".to_owned(), "other-value".to_owned())]),
            }],
            ..Default::default()
        };
        let process_env = BTreeMap::from([("SHELL_SETTING".to_owned(), "shell-value".to_owned())]);
        let context = runtime_context_from_selection(
            ".".into(),
            None,
            bcode_config::ResolvedModelSelection {
                auth_profile: Some("original-primary".to_owned()),
                ..Default::default()
            },
            provider_context,
            process_env.clone(),
        );
        assert_eq!(
            context.provider_context.auth_profile.as_deref(),
            Some("preferred-account")
        );
        assert_eq!(context.provider_context.auth, Some(auth));
        assert!(!context.provider_context.env.contains_key("OTHER_TOKEN"));
        assert!(!context.env_keys.contains_key("OTHER_TOKEN"));
        assert_eq!(context.process_env, process_env);
        assert!(!context.process_env.contains_key("SELECTED"));
    }

    #[test]
    fn explicit_shell_values_keep_precedence_without_changing_account_identity() {
        let context = runtime_context_from_selection(
            ".".into(),
            None,
            bcode_config::ResolvedModelSelection::default(),
            bcode_model::ProviderRequestContext {
                auth_profile: Some("account".to_owned()),
                env: BTreeMap::from([("SETTING".to_owned(), "profile-value".to_owned())]),
                ..Default::default()
            },
            BTreeMap::from([("SETTING".to_owned(), "shell-value".to_owned())]),
        );
        assert_eq!(context.provider_context.env["SETTING"], "shell-value");
        assert_eq!(
            context.provider_context.auth_profile.as_deref(),
            Some("account")
        );
    }
}

fn decode_provider_validation(
    response: &PluginServiceResponse,
) -> Result<bcode_model::ValidateConfigResponse, ClientError> {
    if response.error.is_some() {
        return Err(ClientError::Protocol(
            "Provider configuration validation failed.".to_owned(),
        ));
    }
    serde_json::from_slice(&response.payload).map_err(|_| {
        ClientError::Protocol(
            "Provider returned an incompatible configuration validation response.".to_owned(),
        )
    })
}

impl From<ErrorResponse> for ClientError {
    fn from(value: ErrorResponse) -> Self {
        Self::Server {
            code: value.code,
            message: value.message,
        }
    }
}

/// Compatibility export for the session-owned message acceptance result.
pub use bcode_session_models::MessageAcceptance;

const fn decode_message_acceptance(
    response: &ResponsePayload,
) -> Result<MessageAcceptance, ClientError> {
    match response {
        ResponsePayload::MessageAccepted {
            queued,
            queue_position,
        } => Ok(MessageAcceptance {
            queued: *queued,
            queue_position: *queue_position,
            disposition: bcode_session_models::MessageAcceptanceDisposition::StartedTurn,
        }),
        ResponsePayload::MessageAcceptedWithDisposition {
            queued,
            queue_position,
            disposition,
        } => Ok(MessageAcceptance {
            queued: *queued,
            queue_position: *queue_position,
            disposition: *disposition,
        }),
        ResponsePayload::MessageSent => Ok(MessageAcceptance::sent()),
        _ => Err(ClientError::UnexpectedResponse),
    }
}

/// Client configured for a local Bcode server endpoint.
#[derive(Debug, Clone)]
pub struct BcodeClient {
    expected_state_location: Option<bcode_config::StateLocationId>,
    endpoint: IpcEndpoint,
    runtime_context: Option<ClientRuntimeContext>,
    runtime_context_error: Option<String>,
    daemon_availability: DaemonAvailability,
    connect_timeout: Duration,
    startup_timeout: Duration,
    startup_gate: Arc<tokio::sync::Mutex<()>>,
    request_timeout: Duration,
}

/// Daemon availability policy used by client connections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonAvailability {
    /// Require an already-running daemon and return transport errors directly.
    RequireRunning,
    /// Start the daemon when recoverable IPC failures indicate it is unavailable.
    AutoStart,
}

/// Event-driven session catalog watcher.
#[derive(Debug)]
pub struct SessionCatalogWatcher {
    connection: ClientConnection,
    last_revision: u64,
}

impl SessionCatalogWatcher {
    /// Return the initial catalog snapshot after subscribing to updates.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn initial_snapshot(&mut self) -> Result<SessionList, ClientError> {
        let snapshot = self.connection.list_sessions_with_status().await?;
        self.last_revision = snapshot.catalog_revision;
        Ok(snapshot)
    }

    /// Wait for the next catalog revision and fetch its snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon connection fails or listing fails.
    pub async fn next_snapshot(&mut self) -> Result<SessionList, ClientError> {
        loop {
            match self.connection.recv_event().await? {
                Event::SessionCatalogUpdated { revision } if revision > self.last_revision => {
                    let snapshot = self.connection.list_sessions_with_status().await?;
                    self.last_revision = snapshot.catalog_revision.max(revision);
                    return Ok(snapshot);
                }
                Event::SessionCatalogUpdated { .. }
                | Event::Session(_)
                | Event::SessionLive(_)
                | Event::RuntimeWork(_)
                | Event::Workflow(_)
                | Event::SessionViewResyncRequired { .. } => {}
            }
        }
    }
}

/// Session update received by a long-lived watcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionWatchEvent {
    /// Durable session event.
    Durable(Box<SessionEvent>),
    /// Ephemeral live session event.
    Live(Box<bcode_session_models::SessionLiveEvent>),
    /// The daemon requires this watcher to replace its view from bounded state.
    ResyncRequired,
}

/// Event-driven session watcher initialized with bounded recent history.
#[derive(Debug)]
pub struct SessionWatcher {
    connection: ClientConnection,
    session_id: SessionId,
    initial: Option<AttachedSessionHistory>,
}

impl SessionWatcher {
    const fn initial_session_id(&self) -> SessionId {
        self.session_id
    }

    /// Take the bounded initial session state captured while subscribing.
    #[must_use]
    pub const fn take_initial(&mut self) -> Option<AttachedSessionHistory> {
        self.initial.take()
    }

    /// Wait for the next durable or live session event.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon connection closes or the event cannot be decoded.
    pub async fn next_event(&mut self) -> Result<SessionWatchEvent, ClientError> {
        loop {
            match self.connection.recv_event().await? {
                Event::Session(event) | Event::RuntimeWork(event) => {
                    return Ok(SessionWatchEvent::Durable(Box::new(event)));
                }
                Event::SessionLive(event) => {
                    return Ok(SessionWatchEvent::Live(Box::new(event)));
                }
                Event::SessionViewResyncRequired {
                    session_id: required,
                } if required == self.initial_session_id() => {
                    return Ok(SessionWatchEvent::ResyncRequired);
                }
                Event::SessionCatalogUpdated { .. }
                | Event::Workflow(_)
                | Event::SessionViewResyncRequired { .. } => {}
            }
        }
    }
}

/// Event-driven runtime-work watcher.
#[derive(Debug)]
pub struct RuntimeWorkWatcher {
    connection: ClientConnection,
}

impl RuntimeWorkWatcher {
    /// Wait for the next runtime-work lifecycle event.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon connection closes or the event cannot be decoded.
    pub async fn next_event(&mut self) -> Result<SessionEvent, ClientError> {
        loop {
            match self.connection.recv_event().await? {
                Event::RuntimeWork(event) => return Ok(event),
                Event::Session(_)
                | Event::SessionLive(_)
                | Event::Workflow(_)
                | Event::SessionViewResyncRequired { .. }
                | Event::SessionCatalogUpdated { .. } => {}
            }
        }
    }
}

/// Event-driven workflow-run watcher.
#[derive(Debug)]
pub struct WorkflowRunWatcher {
    connection: ClientConnection,
    sequence: bcode_workflow_view_models::WorkflowLiveSequence,
}

/// Portable workflow notification outcome, retained here for source compatibility.
pub use bcode_workflow::WorkflowRunWatchEvent;

impl bcode_workflow::WorkflowRunObservationApplication for BcodeClient {
    type Error = ClientError;
    type Subscription = WorkflowRunWatcher;

    async fn watch_workflow_runs(&self) -> Result<Self::Subscription, Self::Error> {
        Self::watch_workflow_runs(self).await
    }
}

impl bcode_workflow::WorkflowRunSubscription for WorkflowRunWatcher {
    type Error = ClientError;

    async fn next_event(&mut self) -> Result<WorkflowRunWatchEvent, Self::Error> {
        Self::next_event(self).await
    }
}

impl WorkflowRunWatcher {
    /// Wait for the next workflow canonical-state notification.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon connection closes or the event cannot be decoded.
    pub async fn next_event(&mut self) -> Result<WorkflowRunWatchEvent, ClientError> {
        loop {
            let event = match self.connection.recv_event().await? {
                Event::Workflow(event) => event,
                Event::Session(_)
                | Event::SessionLive(_)
                | Event::RuntimeWork(_)
                | Event::SessionViewResyncRequired { .. }
                | Event::SessionCatalogUpdated { .. } => continue,
            };
            match self.sequence.observe(&event) {
                bcode_workflow_view_models::WorkflowLiveEventDisposition::Refetch => {
                    return Ok(WorkflowRunWatchEvent::Changed(event));
                }
                bcode_workflow_view_models::WorkflowLiveEventDisposition::Duplicate => {}
                bcode_workflow_view_models::WorkflowLiveEventDisposition::UnsupportedVersion => {
                    return Ok(WorkflowRunWatchEvent::UnsupportedVersion {
                        version: event.version,
                    });
                }
                bcode_workflow_view_models::WorkflowLiveEventDisposition::Gap => {
                    let after_sequence = self.sequence.last_observed().unwrap_or(0);
                    let page = self
                        .connection
                        .workflow_live_event_catch_up(after_sequence, 256)
                        .await?;
                    if page.resync_required {
                        return Ok(WorkflowRunWatchEvent::ResyncRequired);
                    }
                    for caught_up in page.events {
                        match self.sequence.observe(&caught_up) {
                            bcode_workflow_view_models::WorkflowLiveEventDisposition::Refetch
                            | bcode_workflow_view_models::WorkflowLiveEventDisposition::Duplicate => {}
                            bcode_workflow_view_models::WorkflowLiveEventDisposition::Gap => {
                                return Ok(WorkflowRunWatchEvent::ResyncRequired);
                            }
                            bcode_workflow_view_models::WorkflowLiveEventDisposition::UnsupportedVersion => {
                                return Ok(WorkflowRunWatchEvent::UnsupportedVersion {
                                    version: caught_up.version,
                                });
                            }
                        }
                    }
                    return Ok(WorkflowRunWatchEvent::Changed(event));
                }
            }
        }
    }
}

fn configured_request_timeout() -> Duration {
    bcode_config::load_config().map_or(DEFAULT_CLIENT_IPC_REQUEST_TIMEOUT, |config| {
        Duration::from_secs(config.client.request_timeout_secs)
    })
}

impl BcodeClient {
    /// Create a client that connects to the default endpoint.
    #[must_use]
    pub fn default_endpoint() -> Self {
        let (runtime_context, runtime_context_error) = match current_runtime_context() {
            Ok(context) => (Some(context), None),
            Err(error) => (None, Some(error.to_string())),
        };
        Self {
            expected_state_location: None,
            endpoint: default_endpoint(),
            runtime_context,
            runtime_context_error,
            daemon_availability: DaemonAvailability::AutoStart,
            connect_timeout: DEFAULT_CLIENT_CONNECT_TIMEOUT,
            startup_timeout: DEFAULT_CLIENT_DAEMON_START_TIMEOUT,
            startup_gate: Arc::new(tokio::sync::Mutex::new(())),
            request_timeout: configured_request_timeout(),
        }
    }

    /// Create a client for a specific endpoint.
    #[must_use]
    pub fn new(endpoint: IpcEndpoint) -> Self {
        Self {
            expected_state_location: None,
            endpoint,
            runtime_context: None,
            runtime_context_error: None,
            daemon_availability: DaemonAvailability::RequireRunning,
            connect_timeout: DEFAULT_CLIENT_CONNECT_TIMEOUT,
            startup_timeout: DEFAULT_CLIENT_DAEMON_START_TIMEOUT,
            startup_gate: Arc::new(tokio::sync::Mutex::new(())),
            request_timeout: DEFAULT_CLIENT_IPC_REQUEST_TIMEOUT,
        }
    }

    /// Create a client for an already-running server at a resolved durable location.
    ///
    /// Handshake and status verification retain this identity across connections without
    /// consulting process state. Automatic daemon launch is disabled for this client,
    /// including after availability policy changes: its host owns scoped server startup.
    #[must_use]
    pub fn for_state_location(
        endpoint: IpcEndpoint,
        location: &bcode_config::StateLocation,
    ) -> Self {
        let mut client = Self::new(endpoint);
        client.expected_state_location = Some(location.id().clone());
        client
    }

    /// Attach a client-supplied runtime context to future connections.
    #[must_use]
    pub fn with_runtime_context(mut self, runtime_context: Option<ClientRuntimeContext>) -> Self {
        self.runtime_context = runtime_context;
        self.runtime_context_error = None;
        self
    }

    /// Attach renderer interaction adapters to future connections.
    #[must_use]
    pub fn with_interaction_adapters(
        mut self,
        interaction_adapters: Vec<
            bcode_plugin_sdk::interaction::PluginInteractionAdapterCapability,
        >,
    ) -> Self {
        let context = self.runtime_context.get_or_insert_default();
        context.interaction_adapters = interaction_adapters;
        self
    }

    /// Add an interaction adapter to future connections while retaining existing runtime context.
    #[must_use]
    pub fn with_interaction_adapter(
        mut self,
        interaction_adapter: bcode_plugin_sdk::interaction::PluginInteractionAdapterCapability,
    ) -> Self {
        self.runtime_context
            .get_or_insert_default()
            .interaction_adapters
            .push(interaction_adapter);
        self
    }

    /// Configure the maximum wait for one transport connection and verified handshake.
    #[must_use]
    pub const fn with_connect_timeout(mut self, connect_timeout: Duration) -> Self {
        self.connect_timeout = connect_timeout;
        self
    }

    /// Configure the maximum wait for daemon lifecycle startup.
    #[must_use]
    pub const fn with_startup_timeout(mut self, startup_timeout: Duration) -> Self {
        self.startup_timeout = startup_timeout;
        self
    }

    /// Configure the maximum wait for application IPC responses.
    #[must_use]
    pub const fn with_request_timeout(mut self, request_timeout: Duration) -> Self {
        self.request_timeout = request_timeout;
        self
    }

    /// Return the configured connection and handshake timeout.
    #[must_use]
    pub const fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }

    /// Return the configured daemon startup timeout.
    #[must_use]
    pub const fn startup_timeout(&self) -> Duration {
        self.startup_timeout
    }

    /// Return the configured application request timeout.
    #[must_use]
    pub const fn request_timeout(&self) -> Duration {
        self.request_timeout
    }

    /// Configure daemon availability behavior for future connections.
    #[must_use]
    pub const fn with_daemon_availability(
        mut self,
        daemon_availability: DaemonAvailability,
    ) -> Self {
        self.daemon_availability = daemon_availability;
        self
    }

    /// Ensure a compatible local daemon is available when auto-start is enabled.
    ///
    /// # Errors
    ///
    /// Returns an error when daemon acquisition fails or this client is configured
    /// to require an already-running daemon.
    pub async fn ensure_daemon_available(&self) -> Result<(), ClientError> {
        if self.daemon_availability == DaemonAvailability::RequireRunning
            || self.expected_state_location.is_some()
        {
            return Ok(());
        }
        let coordination_started = std::time::Instant::now();
        let _startup_guard = self.startup_gate.lock().await;
        tracing::debug!(
            target: "bcode_client::startup",
            elapsed_us = coordination_started.elapsed().as_micros(),
            "client startup gate acquired"
        );
        if self
            .connect_with_deadline("bcode-daemon-availability")
            .await
            .is_ok()
        {
            return Ok(());
        }
        tokio::time::timeout(
            self.startup_timeout,
            ensure_daemon_running(&EnsureDaemonOptions {
                endpoint: self.endpoint.clone(),
                quiet: true,
                log_path: bcode_daemon_lifecycle::default_daemon_log_path(),
            }),
        )
        .await
        .map_err(|_| ClientError::DaemonStartupTimeout {
            timeout: self.startup_timeout,
        })??;
        Ok(())
    }

    /// Create an event-driven session catalog watcher.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the subscription.
    pub async fn watch_session_catalog(&self) -> Result<SessionCatalogWatcher, ClientError> {
        let mut connection = self.connect("bcode-session-catalog").await?;
        connection.subscribe_catalog_updates().await?;
        Ok(SessionCatalogWatcher {
            connection,
            last_revision: 0,
        })
    }

    /// Create an event-driven session watcher with bounded recent history.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the attachment.
    pub async fn watch_session(
        &self,
        session_id: SessionId,
        history_limit: usize,
    ) -> Result<SessionWatcher, ClientError> {
        let mut connection = self.connect("bcode-session-view").await?;
        let initial = connection
            .attach_session_recent_with_input_history(session_id, history_limit)
            .await?;
        Ok(SessionWatcher {
            connection,
            session_id,
            initial: Some(initial),
        })
    }

    /// Create an event-driven session watcher with a bounded semantic projection window.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the attachment.
    pub async fn watch_session_projection_window(
        &self,
        session_id: SessionId,
        request: ProjectionWindowRequest,
    ) -> Result<SessionWatcher, ClientError> {
        let mut connection = self.connect("bcode-session-view").await?;
        let initial = connection
            .attach_session_projection_window_with_input_history(session_id, request)
            .await?;
        Ok(SessionWatcher {
            connection,
            session_id,
            initial: Some(initial),
        })
    }

    /// Create an event-driven runtime-work watcher for a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the subscription.
    pub async fn watch_runtime_work(
        &self,
        session_id: SessionId,
    ) -> Result<RuntimeWorkWatcher, ClientError> {
        let mut connection = self.connect("bcode-runtime-work").await?;
        connection.subscribe_runtime_work(session_id).await?;
        Ok(RuntimeWorkWatcher { connection })
    }

    /// Create an event-driven watcher for workflow-run canonical-state notifications.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the subscription.
    pub async fn watch_workflow_runs(&self) -> Result<WorkflowRunWatcher, ClientError> {
        let mut connection = self.connect("bcode-workflow-runs").await?;
        let after_sequence = connection.subscribe_workflow_runs().await?;
        Ok(WorkflowRunWatcher {
            connection,
            sequence: bcode_workflow_view_models::WorkflowLiveSequence::from_last_observed(
                after_sequence,
            ),
        })
    }

    /// Check whether the local server accepts requests.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn ping(&self) -> Result<(), ClientError> {
        match self.send_request(Request::Ping).await? {
            ResponsePayload::Pong => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Submit a bounded client-side metrics batch to the daemon-owned registry.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the batch.
    pub async fn ingest_client_metrics(
        &self,
        batch: bcode_metrics::ClientMetricBatch,
    ) -> Result<usize, ClientError> {
        match self
            .send_request(Request::IngestClientMetrics { batch })
            .await?
        {
            ResponsePayload::ClientMetricsIngested { accepted } => Ok(accepted),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Query normalized model-catalog diagnostics.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn model_catalog_diagnostics(
        &self,
    ) -> Result<bcode_model_catalog_models::ModelCatalogDiagnostics, ClientError> {
        match self.send_request(Request::ModelCatalogDiagnostics).await? {
            ResponsePayload::ModelCatalogDiagnostics { diagnostics } => Ok(diagnostics),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Query local server status.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn server_status(&self) -> Result<bcode_ipc::ServerStatus, ClientError> {
        match self
            .send_request(Request::ServerStatus {
                working_directory: Some(current_working_directory()),
            })
            .await?
        {
            ResponsePayload::ServerStatus { status } => Ok(status),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    #[cfg(test)]
    fn verify_daemon_identity(status: &bcode_ipc::DaemonStatus) -> Result<(), ClientError> {
        Self::verify_daemon_identity_at(status, &bcode_ipc::state_location_id())
    }

    fn verify_server_identity(&self, status: &bcode_ipc::DaemonStatus) -> Result<(), ClientError> {
        let expected = self
            .expected_state_location
            .as_ref()
            .map_or_else(bcode_ipc::state_location_id, |id| id.as_str().to_owned());
        Self::verify_daemon_identity_at(status, &expected)
    }

    fn verify_daemon_identity_at(
        status: &bcode_ipc::DaemonStatus,
        expected_state_location: &str,
    ) -> Result<(), ClientError> {
        let expected_namespace = bcode_ipc::daemon_namespace();
        let expected_protocol = u32::from(bcode_ipc::CURRENT_PROTOCOL_VERSION);
        let expected_artifact_id = bcode_ipc::ArtifactId::current();
        let expected_writer_epoch = bcode_ipc::CURRENT_SESSION_STORAGE_WRITER_EPOCH;
        let expected_event_schema = bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION;
        if status.namespace == expected_namespace
            && status.protocol_version == expected_protocol
            && status.artifact_id.as_ref() == Some(&expected_artifact_id)
            && status.build_fingerprint == bcode_ipc::BUILD_FINGERPRINT
            && status.storage_writer_epoch == Some(expected_writer_epoch)
            && status.session_event_schema_version == Some(expected_event_schema)
            // A daemon that does not advertise a state location is unverifiable, not
            // assumed compatible: connecting anyway would let this client mutate a
            // different location's canonical session storage.
            && status.state_location_id.as_deref() == Some(expected_state_location)
        {
            return Ok(());
        }
        Err(ClientError::IncompatibleDaemon {
            message: format!(
                "client expects namespace={expected_namespace} artifact={expected_artifact_id} protocol={expected_protocol} build={} session_event_schema={expected_event_schema} storage_writer_epoch={expected_writer_epoch} state_location={expected_state_location}; daemon reported namespace={} artifact={} protocol={} build={} executable={} session_event_schema={} storage_writer_epoch={} state_location={}",
                bcode_ipc::BUILD_FINGERPRINT,
                status.namespace,
                status
                    .artifact_id
                    .as_ref()
                    .map_or("<unknown>", bcode_ipc::ArtifactId::as_str),
                status.protocol_version,
                status.build_fingerprint,
                status.executable_digest.as_deref().unwrap_or("<unknown>"),
                status
                    .session_event_schema_version
                    .map_or_else(|| "<unknown>".to_owned(), |value| value.to_string()),
                status
                    .storage_writer_epoch
                    .map_or_else(|| "<unknown>".to_owned(), |value| value.to_string()),
                status.state_location_id.as_deref().unwrap_or("<unknown>"),
            ),
        })
    }

    /// Return server status after verifying daemon executable identity.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached, rejects the request, or does not match
    /// this client's executable identity.
    pub async fn verified_server_status(&self) -> Result<bcode_ipc::ServerStatus, ClientError> {
        let status = self.server_status().await?;
        self.verify_server_identity(&status.daemon)?;
        Ok(status)
    }

    /// Request graceful local server shutdown.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn server_stop(&self) -> Result<(), ClientError> {
        self.server_stop_with_mode(ServerStopMode::Force).await
    }

    /// Request graceful local server shutdown only if the daemon is idle.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached, rejects the request,
    /// or is not idle.
    pub async fn server_stop_if_idle(&self) -> Result<(), ClientError> {
        self.server_stop_with_mode(ServerStopMode::IfIdle).await
    }

    /// Ask the connected daemon to release one quiescent session's runtime ownership.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached, rejects the request, or returns an
    /// unexpected response.
    pub async fn release_session_ownership(
        &self,
        session_id: bcode_session_models::SessionId,
    ) -> Result<bcode_ipc::SessionOwnershipReleaseOutcome, ClientError> {
        match self
            .send_request(Request::ReleaseSessionOwnership { session_id })
            .await?
        {
            ResponsePayload::SessionOwnershipReleased {
                session_id: released_session_id,
                outcome,
            } if released_session_id == session_id => Ok(outcome),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    async fn server_stop_with_mode(&self, mode: ServerStopMode) -> Result<(), ClientError> {
        match self.send_request(Request::ServerStop { mode }).await? {
            ResponsePayload::ServerStopping => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return the persisted composer draft for a scope.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn composer_draft(
        &self,
        scope: bcode_session_models::ComposerDraftScope,
    ) -> Result<Option<String>, ClientError> {
        match self.send_request(Request::ComposerDraft { scope }).await? {
            ResponsePayload::ComposerDraft { draft } => Ok(draft),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Set or clear the persisted composer draft for a scope.
    ///
    /// Empty text clears the draft.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn set_composer_draft(
        &self,
        scope: bcode_session_models::ComposerDraftScope,
        text: String,
    ) -> Result<(), ClientError> {
        match self
            .send_request(Request::SetComposerDraft { scope, text })
            .await?
        {
            ResponsePayload::ComposerDraftSet => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Create a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn create_session(
        &self,
        name: Option<String>,
    ) -> Result<SessionSummary, ClientError> {
        self.create_session_in_working_directory(name, current_working_directory())
            .await
    }

    /// Create a session in a specific working directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn create_session_in_working_directory(
        &self,
        name: Option<String>,
        working_directory: std::path::PathBuf,
    ) -> Result<SessionSummary, ClientError> {
        let working_directory =
            resolve_path_from(Some(working_directory), &current_working_directory());
        match self
            .send_request(Request::CreateSession {
                name,
                working_directory,
            })
            .await?
        {
            ResponsePayload::SessionCreated { session } => Ok(session),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List sessions.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn list_sessions(&self) -> Result<Vec<SessionSummary>, ClientError> {
        Ok(self.list_sessions_with_status().await?.sessions)
    }

    /// List sessions and return the persistent catalog status observed by the server.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn list_sessions_with_status(&self) -> Result<SessionList, ClientError> {
        self.list_sessions_in_working_directory(current_working_directory())
            .await
    }

    /// List sessions and catalog status for an explicit working directory.
    ///
    /// This selects discovery scope only; it does not confer authority over returned sessions.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn list_sessions_in_working_directory(
        &self,
        working_directory: PathBuf,
    ) -> Result<SessionList, ClientError> {
        let working_directory = std::path::absolute(working_directory).map_err(|_| {
            ClientError::Protocol("cannot resolve catalog working directory".into())
        })?;
        match self
            .send_request(Request::ListSessions { working_directory })
            .await?
        {
            ResponsePayload::SessionList {
                sessions,
                catalog_status,
                catalog_sources,
                catalog_revision,
            } => Ok(SessionList {
                sessions,
                catalog_status,
                catalog_sources,
                catalog_revision,
            }),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Read one bounded, non-mutating session compatibility inventory page.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn session_compatibility_inventory(
        &self,
        request: SessionCompatibilityInventoryRequest,
    ) -> Result<SessionCompatibilityInventoryResponse, ClientError> {
        match self
            .send_request(Request::SessionCompatibilityInventory { request })
            .await?
        {
            ResponsePayload::SessionCompatibilityInventory { response } => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Start explicit bounded bulk canonical migration or its inventory mode.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn start_session_bulk_migration(
        &self,
        request: SessionBulkMigrationStartRequest,
    ) -> Result<SessionBulkMigrationOperationStatus, ClientError> {
        match self
            .send_request(Request::SessionBulkMigrationStart { request })
            .await?
        {
            ResponsePayload::SessionBulkMigrationOperation { status } => Ok(status),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Read transient bulk migration operation status.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or the operation is unavailable.
    pub async fn session_bulk_migration_status(
        &self,
        operation_id: String,
    ) -> Result<SessionBulkMigrationOperationStatus, ClientError> {
        match self
            .send_request(Request::SessionBulkMigrationStatus { operation_id })
            .await?
        {
            ResponsePayload::SessionBulkMigrationOperation { status } => Ok(status),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Wait for a newer transient bulk migration operation revision.
    ///
    /// Aggregate operation state is daemon-local and is not durable across restart.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or the operation is unavailable.
    pub async fn wait_session_bulk_migration(
        &self,
        operation_id: String,
        after_revision: u64,
        timeout_ms: u64,
    ) -> Result<SessionBulkMigrationOperationStatus, ClientError> {
        validate_session_bulk_migration_wait_timeout(timeout_ms)?;
        let server_wait = Duration::from_millis(timeout_ms);
        let response_timeout = self
            .request_timeout
            .max(server_wait.saturating_add(LONG_POLL_TRANSPORT_GRACE));
        match self
            .send_request_with_timeout(
                Request::SessionBulkMigrationWait {
                    operation_id,
                    after_revision,
                    timeout_ms,
                },
                response_timeout,
            )
            .await?
        {
            ResponsePayload::SessionBulkMigrationOperation { status } => Ok(status),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Request cooperative bulk migration cancellation between sessions.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or the operation is unavailable.
    pub async fn cancel_session_bulk_migration(
        &self,
        operation_id: String,
    ) -> Result<SessionBulkMigrationOperationStatus, ClientError> {
        match self
            .send_request(Request::SessionBulkMigrationCancel { operation_id })
            .await?
        {
            ResponsePayload::SessionBulkMigrationOperation { status } => Ok(status),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Import an external session and return the native Bcode session plus one-time warnings.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the import request.
    pub async fn import_external_session(
        &self,
        source_id: impl Into<String>,
        external_session_id: impl Into<String>,
    ) -> Result<(SessionSummary, Vec<SessionImportWarning>), ClientError> {
        match self
            .send_request(Request::ImportExternalSession {
                source_id: source_id.into(),
                external_session_id: external_session_id.into(),
                working_directory: Some(current_working_directory()),
            })
            .await?
        {
            ResponsePayload::ExternalSessionImported { session, warnings } => {
                Ok((session, warnings))
            }
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Refresh the session catalog and return the refreshed snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn refresh_session_catalog(
        &self,
        sources: Option<Vec<String>>,
    ) -> Result<SessionList, ClientError> {
        self.refresh_session_catalog_in_working_directory(current_working_directory(), sources)
            .await
    }

    /// Request catalog refresh for an explicit discovery directory and optional source IDs.
    ///
    /// The returned snapshot may still be loading. This does not repair canonical history
    /// or grant authority over discovered sessions.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn refresh_session_catalog_in_working_directory(
        &self,
        working_directory: PathBuf,
        sources: Option<Vec<String>>,
    ) -> Result<SessionList, ClientError> {
        let working_directory = std::path::absolute(working_directory).map_err(|_| {
            ClientError::Protocol("cannot resolve catalog working directory".into())
        })?;
        match self
            .send_request(Request::RefreshSessionCatalog {
                working_directory: Some(working_directory),
                sources,
            })
            .await?
        {
            ResponsePayload::SessionCatalogRefreshed {
                sessions,
                catalog_status,
                catalog_sources,
                catalog_revision,
            } => Ok(SessionList {
                sessions,
                catalog_status,
                catalog_sources,
                catalog_revision,
            }),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Change a session's canonical working directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn change_session_working_directory(
        &self,
        session_id: SessionId,
        working_directory: impl Into<std::path::PathBuf>,
    ) -> Result<SessionSummary, ClientError> {
        let working_directory =
            resolve_path_from(Some(working_directory.into()), &current_working_directory());
        match self
            .send_request(Request::ChangeSessionWorkingDirectory {
                session_id,
                working_directory,
            })
            .await?
        {
            ResponsePayload::SessionWorkingDirectoryChanged { session, .. } => Ok(session),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List Git worktrees for the current repository.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn list_worktrees(
        &self,
        mut request: WorktreeListRequest,
    ) -> Result<WorktreeListResponse, ClientError> {
        request.cwd = Some(resolve_caller_path(request.cwd));
        match self.send_request(Request::ListWorktrees(request)).await? {
            ResponsePayload::WorktreeList(response) => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Start an idempotent daemon-owned worktree creation operation.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the operation.
    pub async fn start_worktree_create(
        &self,
        operation_id: String,
        mut request: WorktreeCreateRequest,
    ) -> Result<WorktreeCreateOperationStatus, ClientError> {
        request.cwd = Some(resolve_caller_path(request.cwd));
        match self
            .send_request(Request::WorktreeCreateStart {
                operation_id,
                request,
            })
            .await?
        {
            ResponsePayload::WorktreeCreateOperation { status } => Ok(status),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Read one transient worktree creation operation snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or the operation is unavailable.
    pub async fn worktree_create_status(
        &self,
        operation_id: String,
    ) -> Result<WorktreeCreateOperationStatus, ClientError> {
        match self
            .send_request(Request::WorktreeCreateStatus { operation_id })
            .await?
        {
            ResponsePayload::WorktreeCreateOperation { status } => Ok(status),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Wait for a newer worktree creation operation revision.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or the operation is unavailable.
    pub async fn wait_worktree_create(
        &self,
        operation_id: String,
        after_revision: u64,
        timeout_ms: u64,
    ) -> Result<WorktreeCreateOperationStatus, ClientError> {
        let server_wait = Duration::from_millis(timeout_ms);
        let response_timeout = self
            .request_timeout
            .max(server_wait.saturating_add(LONG_POLL_TRANSPORT_GRACE));
        match self
            .send_request_with_timeout(
                Request::WorktreeCreateWait {
                    operation_id,
                    after_revision,
                    timeout_ms,
                },
                response_timeout,
            )
            .await?
        {
            ResponsePayload::WorktreeCreateOperation { status } => Ok(status),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Create a Git worktree, waiting through bounded long-poll requests until completion.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or creation reaches a failed terminal
    /// state.
    pub async fn create_worktree(
        &self,
        request: WorktreeCreateRequest,
    ) -> Result<WorktreeCreateResponse, ClientError> {
        let operation_id = format!(
            "worktree-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let mut status = self
            .start_worktree_create(operation_id.clone(), request)
            .await?;
        loop {
            if let Some(response) = status.response {
                return Ok(response);
            }
            if let Some(error) = status.error {
                return Err(ClientError::WorktreeCreate {
                    code: error.code,
                    message: error.message,
                    created_path: error.created_path,
                });
            }
            status = self
                .wait_worktree_create(operation_id.clone(), status.revision, 30_000)
                .await?;
        }
    }

    /// Remove a Git worktree.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn remove_worktree(
        &self,
        mut request: WorktreeRemoveRequest,
    ) -> Result<WorktreeRemoveResponse, ClientError> {
        request.cwd = Some(resolve_caller_path(request.cwd));
        match self.send_request(Request::RemoveWorktree(request)).await? {
            ResponsePayload::WorktreeRemoved(response) => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return Ralph loop status for a repository.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn ralph_status(
        &self,
        request: RalphStatusRequest,
    ) -> Result<RalphStatusResponse, ClientError> {
        match self.send_request(Request::RalphStatus(request)).await? {
            ResponsePayload::RalphStatus(response) => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Start a bounded Ralph autonomous run.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn run_ralph_loop(
        &self,
        request: RalphRunRequest,
    ) -> Result<RalphRunResponse, ClientError> {
        match self.send_request(Request::RunRalphLoop(request)).await? {
            ResponsePayload::RalphRunStarted(response) => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Approve and start an approval-gated Ralph autonomous run.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn approve_ralph_run(
        &self,
        request: RalphApproveRequest,
    ) -> Result<RalphRunResponse, ClientError> {
        match self.send_request(Request::ApproveRalphRun(request)).await? {
            ResponsePayload::RalphRunApproved(response) => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Cancel a Ralph autonomous run.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn cancel_ralph_loop(
        &self,
        request: RalphCancelRequest,
    ) -> Result<RalphCancelResponse, ClientError> {
        match self.send_request(Request::CancelRalphLoop(request)).await? {
            ResponsePayload::RalphRunCancelled(response) => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List recent Ralph runs for a repository.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn list_ralph_runs(
        &self,
        request: RalphListRunsRequest,
    ) -> Result<RalphListRunsResponse, ClientError> {
        match self
            .send_request(Request::ListRalphRuns(Box::new(request)))
            .await?
        {
            ResponsePayload::RalphRunsListed(response) => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List recent Ralph iterations for a repository.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn list_ralph_iterations(
        &self,
        request: RalphListIterationsRequest,
    ) -> Result<RalphListIterationsResponse, ClientError> {
        match self
            .send_request(Request::ListRalphIterations(Box::new(request)))
            .await?
        {
            ResponsePayload::RalphIterationsListed(response) => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Prepare a Ralph resume run for an interrupted run.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn resume_ralph_run(
        &self,
        request: RalphResumeRequest,
    ) -> Result<RalphResumeResponse, ClientError> {
        match self.send_request(Request::ResumeRalphRun(request)).await? {
            ResponsePayload::RalphRunResumed(response) => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return Ralph autonomous run status for a repository.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn ralph_run_status(
        &self,
        request: RalphRunStatusRequest,
    ) -> Result<RalphRunStatusResponse, ClientError> {
        match self.send_request(Request::RalphRunStatus(request)).await? {
            ResponsePayload::RalphRunStatus(response) => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Record a Ralph lifecycle marker in session history.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn record_ralph_lifecycle(
        &self,
        request: RalphLifecycleRequest,
    ) -> Result<SessionEvent, ClientError> {
        match self
            .send_request(Request::RecordRalphLifecycle(request))
            .await?
        {
            ResponsePayload::RalphLifecycleRecorded { event } => Ok(event),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Rename a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn rename_session(
        &self,
        session_id: SessionId,
        name: Option<String>,
    ) -> Result<SessionSummary, ClientError> {
        match self
            .send_request(Request::RenameSession { session_id, name })
            .await?
        {
            ResponsePayload::SessionRenamed { session } => Ok(session),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Delete a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn delete_session(
        &self,
        session_id: SessionId,
    ) -> Result<SessionSummary, ClientError> {
        match self
            .send_request(Request::DeleteSession { session_id })
            .await?
        {
            ResponsePayload::SessionDeleted { session } => Ok(session),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Deliver opaque schema-versioned input to an active invocation.
    ///
    /// Success means the active invocation's bounded queue accepted the input, not that
    /// the plugin processed it. A lost response leaves delivery uncertain. This operation
    /// does not automatically replay input; `input_id` does not confer host deduplication.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the input.
    pub async fn send_invocation_input(
        &self,
        session_id: SessionId,
        input: bcode_tool::ToolInvocationInput,
    ) -> Result<(), ClientError> {
        match self
            .send_request(Request::InvocationInput { session_id, input })
            .await?
        {
            ResponsePayload::InvocationInputAccepted => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Read a bounded generic artifact range from canonical session metadata.
    ///
    /// `length` must be between 1 and
    /// [`bcode_session_models::MAX_SESSION_ARTIFACT_RANGE_BYTES`] inclusive.
    ///
    /// # Errors
    ///
    /// Returns an error before transport when `length` is outside the documented bounds.
    /// Otherwise returns an error when the daemon cannot be reached, rejects the reference/range,
    /// or returns an unexpected payload.
    pub async fn session_artifact_range(
        &self,
        session_id: SessionId,
        artifact_id: String,
        reference_key: String,
        offset: u64,
        length: u32,
    ) -> Result<SessionArtifactRange, ClientError> {
        if length == 0 || length > bcode_session_models::MAX_SESSION_ARTIFACT_RANGE_BYTES {
            return Err(ClientError::Protocol(format!(
                "artifact range length must be between 1 and {} bytes",
                bcode_session_models::MAX_SESSION_ARTIFACT_RANGE_BYTES,
            )));
        }
        match self
            .send_request(Request::ReadSessionArtifact {
                session_id,
                artifact_id: artifact_id.clone(),
                reference_key: reference_key.clone(),
                offset,
                length,
            })
            .await?
        {
            ResponsePayload::SessionArtifactRange { range } => {
                validate_artifact_range_response(
                    &range,
                    &artifact_id,
                    &reference_key,
                    offset,
                    length,
                )?;
                Ok(range)
            }
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Submit an ordinary turn with generic admission metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn submit_turn(
        &self,
        session_id: SessionId,
        text: String,
        admission: bcode_session_models::TurnAdmissionMetadata,
    ) -> Result<bcode_session_models::TurnAdmission, ClientError> {
        match self
            .send_request(Request::SubmitTurn {
                session_id,
                text,
                admission,
            })
            .await?
        {
            ResponsePayload::TurnAdmission { admission } => Ok(admission),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return complete replayable session history for explicit export/debug/history commands.
    ///
    /// This request performs a full canonical event read on the daemon. Do not use it for
    /// normal UI, attach, prompt/model-context, catalog, or background maintenance flows; use
    /// [`Self::session_history_page`] or projection-specific APIs instead.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn session_history(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<SessionEvent>, ClientError> {
        match self
            .send_request(Request::SessionHistory { session_id })
            .await?
        {
            ResponsePayload::SessionHistory { history, .. } => Ok(history),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Explicitly reprice recorded usage in a time range from a supplied catalog snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error if the daemon rejects ownership, the snapshot, or the projection update.
    pub async fn reprice_session(
        &self,
        session_id: SessionId,
        range: bcode_session_models::SessionCostRange,
        catalog: bcode_model_catalog_models::CatalogDocument,
    ) -> Result<bcode_session_models::SessionRepriceReport, ClientError> {
        match self
            .send_request(Request::RepriceSession {
                session_id,
                range,
                catalog: Box::new(catalog),
            })
            .await?
        {
            ResponsePayload::SessionRepriced { report } => Ok(*report),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Measure physical session files with an explicit directory-entry budget.
    ///
    /// # Errors
    ///
    /// Returns an error if the daemon rejects the budget or cannot inspect the storage roots.
    pub async fn session_storage_usage(
        &self,
        session_id: SessionId,
        entry_budget: u32,
    ) -> Result<bcode_session_models::SessionStorageUsage, ClientError> {
        match self
            .send_request(Request::SessionStorageUsage {
                session_id,
                entry_budget,
            })
            .await?
        {
            ResponsePayload::SessionStorageUsage { usage } => Ok(usage),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Read a bounded canonical history page.
    ///
    /// # Errors
    ///
    /// Returns an error if the daemon cannot read the page or rejects the query.
    pub async fn session_history_page(
        &self,
        session_id: SessionId,
        query: SessionHistoryQuery,
    ) -> Result<SessionHistoryPage, ClientError> {
        match self
            .send_request(Request::SessionHistoryPage { session_id, query })
            .await?
        {
            ResponsePayload::SessionHistoryPage { page } => Ok(page),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return a bounded canonical history window around one event sequence.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn session_history_around(
        &self,
        session_id: SessionId,
        query: SessionHistoryAroundQuery,
    ) -> Result<SessionHistoryWindow, ClientError> {
        match self
            .send_request(Request::SessionHistoryAround { session_id, query })
            .await?
        {
            ResponsePayload::SessionHistoryAround { window } => Ok(window),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return one bounded structured session investigation page.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn session_inspection(
        &self,
        session_id: SessionId,
        query: SessionInspectionQuery,
    ) -> Result<SessionInspectionPage, ClientError> {
        match self
            .send_request(Request::SessionInspection { session_id, query })
            .await?
        {
            ResponsePayload::SessionInspection { page } => Ok(page),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Run one bounded terminal federated session search and optionally hydrate exact locators.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn session_search(
        &self,
        request: bcode_session_search::SessionSearchRequest,
        policy: bcode_session_search::SessionSearchPlanPolicy,
        routes: Vec<bcode_session_search::SessionSearchContentRoute>,
        hydrate: bool,
    ) -> Result<
        (
            bcode_session_search::FederatedSessionSearchResponse,
            Vec<bcode_session_search::HydratedSessionSearchHit>,
        ),
        ClientError,
    > {
        match self
            .send_request(Request::SessionSearch {
                request,
                policy,
                routes,
                hydrate,
            })
            .await?
        {
            ResponsePayload::SessionSearch {
                response,
                hydrated_hits,
            } => Ok((response, hydrated_hits)),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return discovered session-search provider capabilities, status, coverage, and failures.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn session_search_providers(
        &self,
    ) -> Result<bcode_session_search::ListSessionSearchProvidersResponse, ClientError> {
        match self.send_request(Request::SessionSearchProviders).await? {
            ResponsePayload::SessionSearchProviders { response } => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Explain deterministic provider selection without invoking provider searches.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn session_search_explain(
        &self,
        request: bcode_session_search::SessionSearchRequest,
        policy: bcode_session_search::SessionSearchPlanPolicy,
        routes: Vec<bcode_session_search::SessionSearchContentRoute>,
    ) -> Result<bcode_session_search::SessionSearchPlan, ClientError> {
        match self
            .send_request(Request::SessionSearchExplain {
                request,
                policy,
                routes,
            })
            .await?
        {
            ResponsePayload::SessionSearchPlan { plan } => Ok(plan),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Explicitly purge one provider's derived session-search state.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or the provider rejects the operation.
    pub async fn session_search_purge(
        &self,
        provider_id: String,
        confirmation: String,
    ) -> Result<bcode_session_search::SessionSearchMaintenanceResponse, ClientError> {
        match self
            .send_request(Request::SessionSearchPurge {
                provider_id,
                confirmation,
            })
            .await?
        {
            ResponsePayload::SessionSearchMaintenance { response } => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Explicitly recreate one provider's empty derived session-search state.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or the provider rejects the operation.
    pub async fn session_search_rebuild(
        &self,
        provider_id: String,
        confirmation: String,
    ) -> Result<bcode_session_search::SessionSearchMaintenanceResponse, ClientError> {
        match self
            .send_request(Request::SessionSearchRebuild {
                provider_id,
                confirmation,
            })
            .await?
        {
            ResponsePayload::SessionSearchMaintenance { response } => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Start an addressable complete historical backfill operation.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn session_search_complete_backfill_start(
        &self,
        request: bcode_session_search::CompleteSessionSearchBackfillRequest,
    ) -> Result<bcode_session_search::StartSessionSearchBackfillResponse, ClientError> {
        match self
            .send_request(Request::SessionSearchCompleteBackfillStart { request })
            .await?
        {
            ResponsePayload::SessionSearchCompleteBackfillStarted { response } => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Start explicit bounded indexing across every enabled session-search provider.
    ///
    /// The server owns canonical traversal and provider coordination; this client call only starts
    /// the addressable operation.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn session_search_index_all_start(
        &self,
        request: bcode_session_search::CompleteSessionSearchBackfillRequest,
    ) -> Result<bcode_session_search::StartSessionSearchBackfillResponse, ClientError> {
        self.session_search_complete_backfill_start(request).await
    }

    /// Start an addressable bounded single-provider historical backfill operation.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn session_search_backfill_start(
        &self,
        request: bcode_session_search::BackfillSessionSearchRequest,
    ) -> Result<bcode_session_search::StartSessionSearchBackfillResponse, ClientError> {
        match self
            .send_request(Request::SessionSearchBackfillStart { request })
            .await?
        {
            ResponsePayload::SessionSearchBackfillStarted { response } => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Read bounded status for an addressable historical backfill operation.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or the operation is unknown.
    pub async fn session_search_backfill_status(
        &self,
        operation_id: String,
    ) -> Result<bcode_session_search::SessionSearchBackfillOperationStatus, ClientError> {
        match self
            .send_request(Request::SessionSearchBackfillStatus { operation_id })
            .await?
        {
            ResponsePayload::SessionSearchBackfillOperation { status } => Ok(status),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Wait for a newer addressable historical backfill revision or timeout.
    ///
    /// The revision is in-process notification state and does not imply durable resume.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached, the operation is unknown, or the wait
    /// bound is invalid.
    pub async fn session_search_backfill_wait(
        &self,
        operation_id: String,
        after_revision: u64,
        timeout_ms: u64,
    ) -> Result<bcode_session_search::SessionSearchBackfillOperationStatus, ClientError> {
        validate_session_search_backfill_wait_timeout(timeout_ms)?;
        let server_wait = Duration::from_millis(timeout_ms);
        let response_timeout = self
            .request_timeout
            .max(server_wait.saturating_add(LONG_POLL_TRANSPORT_GRACE));
        match self
            .send_request_with_timeout(
                Request::SessionSearchBackfillWait {
                    operation_id,
                    after_revision,
                    timeout_ms,
                },
                response_timeout,
            )
            .await?
        {
            ResponsePayload::SessionSearchBackfillOperation { status } => Ok(status),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Request cancellation of an addressable historical backfill operation.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or the operation is unknown.
    pub async fn session_search_backfill_cancel(
        &self,
        operation_id: String,
    ) -> Result<bcode_session_search::SessionSearchBackfillOperationStatus, ClientError> {
        match self
            .send_request(Request::SessionSearchBackfillCancel { operation_id })
            .await?
        {
            ResponsePayload::SessionSearchBackfillOperation { status } => Ok(status),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Explicitly backfill selected or bounded catalog sessions into one provider.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the bounded maintenance
    /// request.
    pub async fn session_search_backfill(
        &self,
        request: bcode_session_search::BackfillSessionSearchRequest,
    ) -> Result<bcode_session_search::SessionSearchBackfillResponse, ClientError> {
        match self
            .send_request(Request::SessionSearchBackfill { request })
            .await?
        {
            ResponsePayload::SessionSearchBackfill { response } => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Send a user message to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn send_user_message(
        &self,
        session_id: SessionId,
        text: String,
        placement: bcode_session_models::PromptPlacement,
    ) -> Result<MessageAcceptance, ClientError> {
        decode_message_acceptance(
            &self
                .send_request(Request::SendUserMessageWithPlacement {
                    session_id,
                    text,
                    placement,
                })
                .await?,
        )
    }

    /// Send a user message with immutable execution options for its admitted turn.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn send_user_message_with_execution(
        &self,
        session_id: SessionId,
        text: String,
        placement: bcode_session_models::PromptPlacement,
        execution: bcode_session_models::TurnExecutionOptions,
    ) -> Result<MessageAcceptance, ClientError> {
        decode_message_acceptance(
            &self
                .send_request(Request::SendUserMessageWithExecution {
                    session_id,
                    text,
                    placement,
                    execution,
                })
                .await?,
        )
    }

    /// Set a session-specific model selection.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn set_session_model(
        &self,
        session_id: SessionId,
        provider_plugin_id: Option<String>,
        model_id: String,
    ) -> Result<(), ClientError> {
        match self
            .send_request(Request::SetSessionModel {
                session_id,
                provider_plugin_id,
                model_id,
            })
            .await?
        {
            ResponsePayload::SessionModelSet => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List portable, secret-free auth-pool status.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or returns an unexpected response.
    pub async fn auth_pool_list(
        &self,
    ) -> Result<Vec<bcode_provider_auth_models::AuthPoolSummary>, ClientError> {
        match self.send_request(Request::AuthPoolList).await? {
            ResponsePayload::AuthPoolList { pools } => Ok(pools),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Persist or clear an interactive preferred profile for an auth pool.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the pool/profile.
    pub async fn set_auth_pool_preference(
        &self,
        pool: String,
        profile: Option<String>,
    ) -> Result<(), ClientError> {
        self.apply_auth_pool_preference(bcode_provider_auth_models::SetAuthPoolPreferenceRequest {
            pool,
            profile,
        })
        .await
    }

    /// Apply a portable auth-pool preference request through the daemon boundary.
    ///
    /// # Errors
    /// Returns an error if transport fails or the daemon rejects the preference.
    pub async fn apply_auth_pool_preference(
        &self,
        request: bcode_provider_auth_models::SetAuthPoolPreferenceRequest,
    ) -> Result<(), ClientError> {
        let bcode_provider_auth_models::SetAuthPoolPreferenceRequest { pool, profile } = request;
        match self
            .send_request(Request::SetAuthPoolPreference { pool, profile })
            .await?
        {
            ResponsePayload::AuthPoolPreferenceSet => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Set a session-specific reasoning selection.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn set_session_reasoning(
        &self,
        session_id: SessionId,
        effort: Option<String>,
        summary: Option<String>,
    ) -> Result<(), ClientError> {
        match self
            .send_request(Request::SetSessionReasoning {
                session_id,
                effort,
                summary,
            })
            .await?
        {
            ResponsePayload::SessionModelSet => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Append a durable presentation-only note to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the note.
    pub async fn append_presentation_note(
        &self,
        session_id: SessionId,
        source_id: String,
        note_id: String,
        text: String,
        format: bcode_command::CommandTextFormat,
    ) -> Result<(), ClientError> {
        match self
            .send_request(Request::AppendPresentationNote {
                session_id,
                source_id,
                note_id,
                text,
                format,
            })
            .await?
        {
            ResponsePayload::PresentationNoteAppended => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return active model metadata for a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn session_model_status(
        &self,
        session_id: SessionId,
    ) -> Result<bcode_model::SessionModelStatus, ClientError> {
        match self
            .send_request(Request::SessionModelStatus { session_id })
            .await?
        {
            ResponsePayload::SessionModelStatus { status } => Ok(status),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return active model metadata for a new draft session.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn default_model_status(
        &self,
    ) -> Result<bcode_model::SessionModelStatus, ClientError> {
        match self.send_request(Request::DefaultModelStatus).await? {
            ResponsePayload::SessionModelStatus { status } => Ok(status),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return available models for a provider.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn session_model_list(
        &self,
        provider_plugin_id: Option<String>,
    ) -> Result<bcode_model::ModelList, ClientError> {
        match self
            .send_request(Request::SessionModelList { provider_plugin_id })
            .await?
        {
            ResponsePayload::SessionModelList { models, .. } => Ok(models),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Check whether an exact resolved model is visible in the application's provider catalog.
    ///
    /// This is a read-only discovery check, not proof that a remote inference request will succeed.
    /// A model absent from a partial catalog is unverified and returns false, not unsupported.
    ///
    /// # Errors
    /// Returns an error when catalog discovery cannot complete.
    pub async fn model_available_for_setup(
        &self,
        provider_plugin_id: String,
        model_id: &str,
    ) -> Result<bool, ClientError> {
        let models = self.session_model_list(Some(provider_plugin_id)).await?;
        Ok(models.models.iter().any(|model| {
            model.model_id == model_id
                && matches!(model.visibility, bcode_model::ModelVisibility::Visible)
        }))
    }

    /// Request cancellation of the active model turn for a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn cancel_session_turn(&self, session_id: SessionId) -> Result<bool, ClientError> {
        self.cancel_session_turn_with_options(session_id, false)
            .await
    }

    /// Request cancellation of the active model turn and optionally clear queued commands.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn cancel_session_turn_with_options(
        &self,
        session_id: SessionId,
        clear_queue: bool,
    ) -> Result<bool, ClientError> {
        match self
            .send_request(Request::CancelSessionTurn {
                session_id,
                clear_queue,
            })
            .await?
        {
            ResponsePayload::TurnCancellationRequested { cancelled } => Ok(cancelled),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Atomically create one logical workflow and its initial draft.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the authored document.
    pub async fn create_authored_workflow(
        &self,
        request: bcode_workflow::CreateAuthoredWorkflowRequest,
    ) -> Result<
        (
            bcode_workflow::AuthoredWorkflowSnapshot,
            bcode_workflow::WorkflowDraftSnapshot,
        ),
        ClientError,
    > {
        match self
            .send_request(Request::CreateAuthoredWorkflow(request))
            .await?
        {
            ResponsePayload::AuthoredWorkflowCreated { workflow, draft } => Ok((workflow, *draft)),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Apply one already-lowered portable source through the canonical authored-workflow lifecycle.
    ///
    /// The operation creates an absent logical workflow or performs at most one optimistic
    /// replacement of an existing source draft. It never publishes, activates, starts, or retries
    /// a conflict.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached, rejects canonical state, or the logical
    /// workflow exists without the selected source-draft identity.
    pub async fn apply_workflow_source(
        &self,
        source_format: bcode_workflow::WorkflowSourceFormat,
        source: String,
        draft_id: String,
    ) -> Result<bcode_workflow::WorkflowSourceApplyResult, ClientError> {
        match self
            .send_request(Request::ApplyWorkflowSource(
                bcode_workflow::ApplyWorkflowSourceRequest {
                    source_format,
                    source,
                    draft_id,
                },
            ))
            .await?
        {
            ResponsePayload::WorkflowSourceApplied { result } => Ok(result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Apply one bounded renderer-neutral semantic edit batch.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the operation boundary.
    pub async fn apply_workflow_draft_edits(
        &self,
        request: bcode_workflow::ApplyWorkflowDraftEditsRequest,
    ) -> Result<bcode_workflow::WorkflowDraftEditResult, ClientError> {
        match self
            .send_request(Request::ApplyWorkflowDraftEdits(request))
            .await?
        {
            ResponsePayload::WorkflowDraftEditResult { result } => Ok(result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Replace one exact authored-workflow draft generation.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the authored document.
    pub async fn update_workflow_draft(
        &self,
        request: bcode_workflow::UpdateWorkflowDraftRequest,
    ) -> Result<bcode_workflow::WorkflowDraftUpdateResult, ClientError> {
        match self
            .send_request(Request::UpdateWorkflowDraft(request))
            .await?
        {
            ResponsePayload::WorkflowDraftUpdateResult { result } => Ok(result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Publish one exact draft generation, optionally activating the new immutable revision.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects publication.
    pub async fn publish_workflow_draft(
        &self,
        request: bcode_workflow::PublishWorkflowDraftRequest,
    ) -> Result<bcode_workflow::WorkflowPublicationResult, ClientError> {
        match self
            .send_request(Request::PublishWorkflowDraft(request))
            .await?
        {
            ResponsePayload::WorkflowPublicationResult { result } => Ok(result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Publish one exact draft and then attempt separately reported durable run admission.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the operation before its
    /// publication outcome can be produced.
    pub async fn publish_and_start_workflow(
        &self,
        request: bcode_workflow::PublishAndStartWorkflowRequest,
    ) -> Result<bcode_workflow::WorkflowPublishAndStartResult, ClientError> {
        match self
            .send_request(Request::PublishAndStartWorkflow(Box::new(request)))
            .await?
        {
            ResponsePayload::WorkflowPublishAndStartResult { result } => Ok(result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Compare-and-set one immutable revision as active.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects activation.
    pub async fn activate_workflow_revision(
        &self,
        request: bcode_workflow::ActivateWorkflowRevisionRequest,
    ) -> Result<bcode_workflow::WorkflowAuthoringMutationResult, ClientError> {
        match self
            .send_request(Request::ActivateWorkflowRevision(request))
            .await?
        {
            ResponsePayload::WorkflowActivationResult { result } => Ok(result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Archive or unarchive one logical authored workflow.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the mutation.
    pub async fn set_authored_workflow_archived(
        &self,
        request: bcode_workflow::SetAuthoredWorkflowArchivedRequest,
    ) -> Result<bcode_workflow::AuthoredWorkflowSnapshot, ClientError> {
        match self
            .send_request(Request::SetAuthoredWorkflowArchived(request))
            .await?
        {
            ResponsePayload::AuthoredWorkflowArchived { workflow } => Ok(workflow),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Discard one exact mutable draft generation.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the mutation.
    pub async fn discard_workflow_draft(
        &self,
        request: bcode_workflow::DiscardWorkflowDraftRequest,
    ) -> Result<bcode_workflow::WorkflowAuthoringMutationResult, ClientError> {
        match self
            .send_request(Request::DiscardWorkflowDraft(request))
            .await?
        {
            ResponsePayload::WorkflowDraftDiscardResult { result } => Ok(result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Fork one exact draft or immutable revision into a new generation-1 draft.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the source/identity.
    pub async fn fork_workflow_draft(
        &self,
        request: bcode_workflow::ForkWorkflowDraftRequest,
    ) -> Result<bcode_workflow::WorkflowDraftSnapshot, ClientError> {
        match self
            .send_request(Request::ForkWorkflowDraft(request))
            .await?
        {
            ResponsePayload::WorkflowDraftForked { draft } => Ok(*draft),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Create one revision-bound workflow preset.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the preset.
    pub async fn create_workflow_preset(
        &self,
        request: bcode_workflow::CreateWorkflowPresetRequest,
    ) -> Result<bcode_workflow::WorkflowPresetSnapshot, ClientError> {
        match self
            .send_request(Request::CreateWorkflowPreset(request))
            .await?
        {
            ResponsePayload::WorkflowPresetCreated { preset } => Ok(preset),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Replace one exact workflow preset generation.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the preset.
    pub async fn update_workflow_preset(
        &self,
        request: bcode_workflow::UpdateWorkflowPresetRequest,
    ) -> Result<bcode_workflow::WorkflowPresetUpdateResult, ClientError> {
        match self
            .send_request(Request::UpdateWorkflowPreset(request))
            .await?
        {
            ResponsePayload::WorkflowPresetUpdateResult { result } => Ok(result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Delete one exact workflow preset generation.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects deletion.
    pub async fn delete_workflow_preset(
        &self,
        request: bcode_workflow::DeleteWorkflowPresetRequest,
    ) -> Result<bcode_workflow::WorkflowAuthoringMutationResult, ClientError> {
        match self
            .send_request(Request::DeleteWorkflowPreset(request))
            .await?
        {
            ResponsePayload::WorkflowPresetDeleteResult { result } => Ok(result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Export one exact immutable authored revision.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or the revision is unavailable.
    pub async fn export_workflow_revision(
        &self,
        request: bcode_workflow::ExportWorkflowRevisionRequest,
    ) -> Result<bcode_workflow::WorkflowExportBundle, ClientError> {
        match self
            .send_request(Request::ExportWorkflowRevision(request))
            .await?
        {
            ResponsePayload::WorkflowRevisionExported { bundle } => Ok(*bundle),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Preview one portable import without mutation.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or the bundle is incompatible.
    pub async fn preview_workflow_import(
        &self,
        request: bcode_workflow::PreviewWorkflowImportRequest,
    ) -> Result<bcode_workflow::WorkflowImportPreview, ClientError> {
        match self
            .send_request(Request::PreviewWorkflowImport(request))
            .await?
        {
            ResponsePayload::WorkflowImportPreview { preview } => Ok(*preview),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Import one portable bundle as a new logical workflow and initial draft.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or import is incompatible/unauthorized.
    pub async fn import_workflow(
        &self,
        request: bcode_workflow::ImportWorkflowRequest,
    ) -> Result<
        (
            bcode_workflow::AuthoredWorkflowSnapshot,
            bcode_workflow::WorkflowDraftSnapshot,
        ),
        ClientError,
    > {
        match self.send_request(Request::ImportWorkflow(request)).await? {
            ResponsePayload::WorkflowImported { workflow, draft } => Ok((workflow, *draft)),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Import one portable bundle as a generation-1 draft in an existing logical workflow.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or import is incompatible/unauthorized.
    pub async fn import_workflow_draft(
        &self,
        request: bcode_workflow::ImportWorkflowDraftRequest,
    ) -> Result<bcode_workflow::WorkflowDraftImportResult, ClientError> {
        match self
            .send_request(Request::ImportWorkflowDraft(request))
            .await?
        {
            ResponsePayload::WorkflowDraftImported { result } => Ok(result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Import one portable bundle as the exact next immutable revision of an existing workflow.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or import is incompatible/unauthorized.
    pub async fn import_workflow_revision(
        &self,
        request: bcode_workflow::ImportWorkflowRevisionRequest,
    ) -> Result<bcode_workflow::WorkflowRevisionImportResult, ClientError> {
        match self
            .send_request(Request::ImportWorkflowRevision(request))
            .await?
        {
            ResponsePayload::WorkflowRevisionImported { result } => Ok(result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Resolve and start one immutable authored-workflow revision.
    ///
    /// # Errors
    ///
    /// Returns an error when resolution, configuration, authorization, or durable run admission
    /// fails.
    pub async fn start_authored_workflow(
        &self,
        request: bcode_workflow::StartAuthoredWorkflowRequest,
    ) -> Result<bcode_workflow::AuthoredWorkflowRunStartResponse, ClientError> {
        match self
            .send_request(Request::StartAuthoredWorkflow(request))
            .await?
        {
            ResponsePayload::AuthoredWorkflowRunStarted(started) => Ok(started),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List bounded logical authored workflows.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the bound.
    pub async fn list_authored_workflows(
        &self,
        cursor: Option<bcode_workflow::WorkflowAuthoringListCursor>,
        limit: usize,
    ) -> Result<
        bcode_workflow::WorkflowAuthoringPage<
            bcode_workflow::AuthoredWorkflowSnapshot,
            bcode_workflow::WorkflowAuthoringListCursor,
        >,
        ClientError,
    > {
        match self
            .send_request(Request::ListAuthoredWorkflows { cursor, limit })
            .await?
        {
            ResponsePayload::AuthoredWorkflowList { page } => Ok(page),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Get one logical authored workflow.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the identity.
    pub async fn authored_workflow(
        &self,
        workflow_id: String,
    ) -> Result<Option<bcode_workflow::AuthoredWorkflowSnapshot>, ClientError> {
        match self
            .send_request(Request::GetAuthoredWorkflow { workflow_id })
            .await?
        {
            ResponsePayload::AuthoredWorkflowDescription { workflow } => Ok(workflow),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return one bounded aggregate authored-workflow inspection snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the identity/bound.
    pub async fn inspect_authored_workflow(
        &self,
        workflow_id: String,
        limit: usize,
    ) -> Result<Option<bcode_workflow::AuthoredWorkflowInspection>, ClientError> {
        match self
            .send_request(Request::InspectAuthoredWorkflow { workflow_id, limit })
            .await?
        {
            ResponsePayload::AuthoredWorkflowInspection { inspection } => {
                Ok(inspection.map(|inspection| *inspection))
            }
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List bounded mutable drafts for one logical workflow.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the identity/bound.
    pub async fn list_workflow_drafts(
        &self,
        workflow_id: String,
        cursor: Option<bcode_workflow::WorkflowAuthoringListCursor>,
        limit: usize,
    ) -> Result<
        bcode_workflow::WorkflowAuthoringPage<
            bcode_workflow::WorkflowDraftSnapshot,
            bcode_workflow::WorkflowAuthoringListCursor,
        >,
        ClientError,
    > {
        match self
            .send_request(Request::ListWorkflowDrafts {
                workflow_id,
                cursor,
                limit,
            })
            .await?
        {
            ResponsePayload::WorkflowDraftList { page } => Ok(page),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Get one exact mutable workflow draft.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the identity.
    pub async fn workflow_draft(
        &self,
        workflow_id: String,
        draft_id: String,
    ) -> Result<Option<bcode_workflow::WorkflowDraftSnapshot>, ClientError> {
        match self
            .send_request(Request::GetWorkflowDraft {
                workflow_id,
                draft_id,
            })
            .await?
        {
            ResponsePayload::WorkflowDraftDescription { draft } => Ok(draft.map(|draft| *draft)),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List bounded immutable published revisions.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the identity/bound.
    pub async fn list_workflow_revisions(
        &self,
        workflow_id: String,
        cursor: Option<bcode_workflow::WorkflowRevisionListCursor>,
        limit: usize,
    ) -> Result<
        bcode_workflow::WorkflowAuthoringPage<
            bcode_workflow::WorkflowRevisionSnapshot,
            bcode_workflow::WorkflowRevisionListCursor,
        >,
        ClientError,
    > {
        match self
            .send_request(Request::ListWorkflowRevisions {
                workflow_id,
                cursor,
                limit,
            })
            .await?
        {
            ResponsePayload::WorkflowRevisionList { page } => Ok(page),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Get one exact immutable published revision.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the identity.
    pub async fn workflow_revision(
        &self,
        workflow_id: String,
        revision: u64,
    ) -> Result<Option<bcode_workflow::WorkflowRevisionSnapshot>, ClientError> {
        match self
            .send_request(Request::GetWorkflowRevision {
                workflow_id,
                revision,
            })
            .await?
        {
            ResponsePayload::WorkflowRevisionDescription { revision } => {
                Ok(revision.map(|revision| *revision))
            }
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Inspect immutable revision facts with current derived requirement availability.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the identity.
    pub async fn workflow_revision_requirement_inspection(
        &self,
        workflow_id: String,
        revision: u64,
    ) -> Result<Option<bcode_workflow::WorkflowRevisionRequirementInspection>, ClientError> {
        match self
            .send_request(Request::InspectWorkflowRevisionRequirements {
                workflow_id,
                revision,
            })
            .await?
        {
            ResponsePayload::WorkflowRevisionRequirementInspection { inspection } => Ok(inspection),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List bounded revision-bound presets.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the identity/bound.
    pub async fn list_workflow_presets(
        &self,
        workflow_id: String,
        cursor: Option<bcode_workflow::WorkflowAuthoringListCursor>,
        limit: usize,
    ) -> Result<
        bcode_workflow::WorkflowAuthoringPage<
            bcode_workflow::WorkflowPresetSnapshot,
            bcode_workflow::WorkflowAuthoringListCursor,
        >,
        ClientError,
    > {
        match self
            .send_request(Request::ListWorkflowPresets {
                workflow_id,
                cursor,
                limit,
            })
            .await?
        {
            ResponsePayload::WorkflowPresetList { page } => Ok(page),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Get one exact revision-bound preset.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the identity.
    pub async fn workflow_preset(
        &self,
        workflow_id: String,
        preset_id: String,
    ) -> Result<Option<bcode_workflow::WorkflowPresetSnapshot>, ClientError> {
        match self
            .send_request(Request::GetWorkflowPreset {
                workflow_id,
                preset_id,
            })
            .await?
        {
            ResponsePayload::WorkflowPresetDescription { preset } => Ok(preset),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return the portable runtime-workflow authoring catalog.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or catalog construction fails.
    pub async fn workflow_authoring_catalog(
        &self,
    ) -> Result<bcode_workflow::WorkflowAuthoringCatalogSnapshot, ClientError> {
        match self.send_request(Request::WorkflowAuthoringCatalog).await? {
            ResponsePayload::WorkflowAuthoringCatalog { catalog } => Ok(catalog),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Discover one bounded portable launch catalog for a workspace.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or discovery/preview fails.
    pub async fn workflow_launch_catalog(
        &self,
        request: bcode_workflow::WorkflowLaunchCatalogRequest,
    ) -> Result<bcode_workflow::WorkflowLaunchCatalogPage, ClientError> {
        match self
            .send_request(Request::WorkflowLaunchCatalog(request))
            .await?
        {
            ResponsePayload::WorkflowLaunchCatalog { page } => Ok(page),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Inspect one exact workflow launch target without mutation.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or the source is unavailable or stale.
    pub async fn workflow_launch_detail(
        &self,
        request: bcode_workflow::WorkflowLaunchDetailRequest,
    ) -> Result<bcode_workflow::WorkflowLaunchDetail, ClientError> {
        match self
            .send_request(Request::WorkflowLaunchDetail(request))
            .await?
        {
            ResponsePayload::WorkflowLaunchDetail { detail } => Ok(*detail),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Read one bounded derived package publication receipt without mutation.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or the package identity is invalid.
    pub async fn workflow_package_publication(
        &self,
        package_id: String,
    ) -> Result<Option<bcode_workflow::WorkflowPackagePublicationReceipt>, ClientError> {
        match self
            .send_request(Request::GetWorkflowPackagePublication { package_id })
            .await?
        {
            ResponsePayload::WorkflowPackagePublication { receipt } => Ok(receipt),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Atomically apply one validated package plan through the daemon-owned workflow store.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached, optimistic generations conflict, or
    /// the complete transaction cannot commit.
    pub async fn apply_workflow_package(
        &self,
        request: bcode_workflow::ApplyWorkflowPackageRequest,
    ) -> Result<bcode_workflow::WorkflowPackageMutationResult, ClientError> {
        match self
            .send_request(Request::ApplyWorkflowPackage(request))
            .await?
        {
            ResponsePayload::WorkflowPackageApplied { result } => Ok(*result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Atomically publish every exact package draft generation through the daemon-owned store.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached, package facts drift, optimistic
    /// generations conflict, or the complete transaction cannot commit.
    pub async fn publish_workflow_package(
        &self,
        request: bcode_workflow::PublishWorkflowPackageRequest,
    ) -> Result<bcode_workflow::WorkflowPackageMutationResult, ClientError> {
        match self
            .send_request(Request::PublishWorkflowPackage(request))
            .await?
        {
            ResponsePayload::WorkflowPackagePublished { result } => Ok(*result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Validate and plan one bounded workflow package through the daemon-owned catalog.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or package planning fails.
    pub async fn validate_workflow_package(
        &self,
        request: bcode_workflow::WorkflowPackageComputationRequest,
    ) -> Result<bcode_workflow::WorkflowPackageValidationResult, ClientError> {
        match self
            .send_request(Request::ValidateWorkflowPackage(request))
            .await?
        {
            ResponsePayload::WorkflowPackageValidated { result } => Ok(*result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Compile-preview one complete package plan through the daemon-owned catalog.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or any package member cannot compile.
    pub async fn preview_workflow_package(
        &self,
        request: bcode_workflow::WorkflowPackagePreviewRequest,
    ) -> Result<bcode_workflow::WorkflowPackagePreview, ClientError> {
        match self
            .send_request(Request::PreviewWorkflowPackage(request))
            .await?
        {
            ResponsePayload::WorkflowPackagePreviewed { preview } => Ok(*preview),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Validate and lower one raw source through the daemon-owned catalog.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or source lowering fails.
    pub async fn validate_workflow_source(
        &self,
        request: bcode_workflow::WorkflowSourceComputationRequest,
    ) -> Result<bcode_workflow::WorkflowSourceValidationResult, ClientError> {
        match self
            .send_request(Request::ValidateWorkflowSource(request))
            .await?
        {
            ResponsePayload::WorkflowSourceValidated { result } => Ok(*result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Lower and compile-preview one raw source through the daemon-owned catalog.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or source compilation fails.
    pub async fn preview_workflow_source(
        &self,
        request: bcode_workflow::WorkflowSourcePreviewRequest,
    ) -> Result<bcode_workflow::WorkflowSourcePreviewResult, ClientError> {
        match self
            .send_request(Request::PreviewWorkflowSource(request))
            .await?
        {
            ResponsePayload::WorkflowSourcePreviewed { result } => Ok(*result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Validate one portable authoring document without durable mutation.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or returns an incompatible response.
    pub async fn validate_workflow_authoring(
        &self,
        document: bcode_workflow::WorkflowAuthoringDocument,
    ) -> Result<bcode_workflow::WorkflowValidationReport, ClientError> {
        self.validate_workflow_authoring_with_control(
            document,
            bcode_workflow::WorkflowComputationControl::default(),
        )
        .await
    }

    /// Validate one document with an explicit server-side deadline/cancellation identity.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached, computation is cancelled/times out, or
    /// the response is incompatible.
    pub async fn validate_workflow_authoring_with_control(
        &self,
        document: bcode_workflow::WorkflowAuthoringDocument,
        control: bcode_workflow::WorkflowComputationControl,
    ) -> Result<bcode_workflow::WorkflowValidationReport, ClientError> {
        match self
            .send_request(Request::ValidateWorkflowAuthoring { document, control })
            .await?
        {
            ResponsePayload::WorkflowAuthoringValidated { report } => Ok(report),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Compile and preview one authored workflow without persistence or dispatch.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or returns an incompatible response.
    pub async fn preview_workflow_compilation(
        &self,
        document: bcode_workflow::WorkflowAuthoringDocument,
        configuration: Option<serde_json::Value>,
    ) -> Result<bcode_workflow::WorkflowCompilationPreview, ClientError> {
        self.preview_workflow_compilation_with_control(
            document,
            configuration,
            bcode_workflow::WorkflowComputationControl::default(),
        )
        .await
    }

    /// Compile and preview with an explicit server-side deadline/cancellation identity.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached, computation is cancelled/times out, or
    /// the response is incompatible.
    pub async fn preview_workflow_compilation_with_control(
        &self,
        document: bcode_workflow::WorkflowAuthoringDocument,
        configuration: Option<serde_json::Value>,
        control: bcode_workflow::WorkflowComputationControl,
    ) -> Result<bcode_workflow::WorkflowCompilationPreview, ClientError> {
        match self
            .send_request(Request::PreviewWorkflowCompilation {
                document,
                configuration,
                control,
            })
            .await?
        {
            ResponsePayload::WorkflowCompilationPreview { preview } => Ok(*preview),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Request cancellation of one exact authored-workflow validation or compilation.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the operation identity.
    pub async fn cancel_workflow_computation(
        &self,
        operation_id: String,
    ) -> Result<bool, ClientError> {
        match self
            .send_request(Request::CancelWorkflowComputation { operation_id })
            .await?
        {
            ResponsePayload::WorkflowComputationCancellationRequested { cancelled } => {
                Ok(cancelled)
            }
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Explicitly request incompatible workflow-store reset.
    ///
    /// The running daemon refuses this request because it owns the store; clients use the typed
    /// error to direct operators to the offline maintenance entry point.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or refuses online reset.
    pub async fn reset_incompatible_workflow_store(
        &self,
        confirm: String,
    ) -> Result<bcode_workflow_store::WorkflowStoreResetReceipt, ClientError> {
        match self
            .send_request(Request::ResetIncompatibleWorkflowStore { confirm })
            .await?
        {
            ResponsePayload::WorkflowStoreReset { receipt } => Ok(receipt),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List bounded plugin-owned workflow templates with availability diagnostics.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the bound.
    pub async fn list_workflow_templates(
        &self,
        limit: usize,
    ) -> Result<Vec<bcode_ipc::WorkflowTemplateDescription>, ClientError> {
        match self
            .send_request(Request::ListWorkflowTemplates { limit })
            .await?
        {
            ResponsePayload::WorkflowTemplateList { templates } => Ok(templates),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Describe one exact loaded plugin-owned workflow template.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the identity.
    pub async fn describe_workflow_template(
        &self,
        owner_plugin_id: String,
        template_id: String,
        template_version: u32,
    ) -> Result<Option<bcode_ipc::WorkflowTemplateDescription>, ClientError> {
        match self
            .send_request(Request::DescribeWorkflowTemplate {
                owner_plugin_id,
                template_id,
                template_version,
            })
            .await?
        {
            ResponsePayload::WorkflowTemplateDescription { template } => {
                Ok(template.map(|template| *template))
            }
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Start one exact loaded plugin-owned workflow template.
    ///
    /// # Errors
    ///
    /// Returns an error when requirements are unavailable, configuration is invalid, or the
    /// daemon cannot register and start the exact compiled definition.
    pub async fn start_workflow_template(
        &self,
        request: bcode_workflow::WorkflowTemplateStartRequest,
    ) -> Result<bcode_workflow::WorkflowRunStartResponse, ClientError> {
        match self
            .send_request(Request::StartWorkflowTemplate(request))
            .await?
        {
            ResponsePayload::WorkflowTemplateStarted(response) => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Instantiate a maintainable plugin template as a normal mutable authored draft.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached, the template is unavailable, or
    /// authored-state creation is rejected.
    pub async fn instantiate_workflow_template(
        &self,
        request: bcode_workflow::WorkflowTemplateInstantiationRequest,
    ) -> Result<
        (
            bcode_workflow::AuthoredWorkflowSnapshot,
            bcode_workflow::WorkflowDraftSnapshot,
        ),
        ClientError,
    > {
        match self
            .send_request(Request::InstantiateWorkflowTemplate(request))
            .await?
        {
            ResponsePayload::AuthoredWorkflowCreated { workflow, draft } => Ok((workflow, *draft)),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Durably register one structurally validated compiled workflow definition.
    ///
    /// Re-registering byte-identical content is idempotent. Reusing an exact identity/version for
    /// different content fails closed.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the definition contract.
    pub async fn register_workflow_definition(
        &self,
        request: bcode_workflow::WorkflowDefinitionRegistrationRequest,
    ) -> Result<bcode_workflow::StoredWorkflowDefinition, ClientError> {
        match self
            .send_request(Request::RegisterWorkflowDefinition(request))
            .await?
        {
            ResponsePayload::WorkflowDefinitionRegistered { definition } => Ok(definition),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Register an exact typed definition and start one associated durable workflow through one
    /// retry-safe daemon operation.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects identity, definition, input,
    /// binding, or execution context.
    pub async fn start_workflow(
        &self,
        request: bcode_workflow::WorkflowStartRequest,
    ) -> Result<bcode_workflow::WorkflowRunStartResponse, ClientError> {
        match self.send_request(Request::StartWorkflow(request)).await? {
            ResponsePayload::WorkflowRunStarted(response) => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Resolve and start one exact published package export.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached, publication facts drift, or the exact
    /// authored revision cannot start.
    pub async fn start_workflow_package_export(
        &self,
        request: bcode_workflow::StartWorkflowPackageExportRequest,
    ) -> Result<bcode_workflow::WorkflowPackageExportRunStartResponse, ClientError> {
        match self
            .send_request(Request::StartWorkflowPackageExport(request))
            .await?
        {
            ResponsePayload::WorkflowPackageExportRunStarted(response) => Ok(*response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Start one durable workflow from a registered exact definition.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the immutable execution
    /// context, definition identity, or run limits.
    pub async fn start_workflow_run(
        &self,
        request: bcode_workflow::WorkflowRunStartRequest,
    ) -> Result<bcode_workflow::WorkflowRunStartResponse, ClientError> {
        match self
            .send_request(Request::StartWorkflowRun(request))
            .await?
        {
            ResponsePayload::WorkflowRunStarted(response) => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Run one bounded non-mutating workflow doctor inspection.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the bound.
    pub async fn doctor_workflow_run(
        &self,
        run_id: String,
        limit: usize,
    ) -> Result<bcode_workflow::WorkflowDoctorReport, ClientError> {
        match self
            .send_request(Request::DoctorWorkflowRun { run_id, limit })
            .await?
        {
            ResponsePayload::WorkflowDoctorReport { report } => Ok(report),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Apply one explicit typed repair resolution.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or the attempt/resolution is invalid.
    pub async fn repair_workflow_attempt(
        &self,
        dispatch_identity: String,
        resolution: bcode_workflow::RepairResolution,
    ) -> Result<bcode_workflow::RepairResult, ClientError> {
        match self
            .send_request(Request::RepairWorkflowAttempt {
                dispatch_identity,
                resolution,
            })
            .await?
        {
            ResponsePayload::WorkflowAttemptRepaired { result } => Ok(result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List bounded, checksum-verified durable workflow definitions.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the bounded request.
    pub async fn list_workflow_definitions(
        &self,
        limit: usize,
    ) -> Result<Vec<bcode_workflow::StoredWorkflowDefinition>, ClientError> {
        match self
            .send_request(Request::ListWorkflowDefinitions { limit })
            .await?
        {
            ResponsePayload::WorkflowDefinitionList { definitions } => Ok(definitions),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Describe one exact durable workflow definition version.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn describe_workflow_definition(
        &self,
        definition_id: String,
        version: u32,
    ) -> Result<Option<bcode_workflow::StoredWorkflowDefinition>, ClientError> {
        match self
            .send_request(Request::DescribeWorkflowDefinition {
                definition_id,
                version,
            })
            .await?
        {
            ResponsePayload::WorkflowDefinitionDescription { definition } => Ok(definition),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Stage a run-owned graph candidate, without publishing topology or dispatching work.
    ///
    /// Returns true for new candidates and false for identical duplicate delivery.
    ///
    /// # Errors
    /// Returns an error for transport failure, policy denial (including unconfigured policy),
    /// invalid candidates, unverified authority, revision conflicts, or conflicting duplicates.
    pub async fn stage_workflow_run_graph_edit(
        &self,
        request: bcode_workflow::WorkflowRunGraphEditBatch,
    ) -> Result<bool, ClientError> {
        match self
            .send_request(Request::StageWorkflowRunGraphEdit { request })
            .await?
        {
            ResponsePayload::WorkflowRunGraphEditStaged { created } => Ok(created),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Publish an exact staged run-owned graph candidate.
    ///
    /// Publication requires separate authorization; staging approval does not grant it.
    /// Returns the committed graph revision, including for identical duplicate publication.
    ///
    /// # Errors
    /// Returns an error for transport failure, publication policy denial, candidate mismatch,
    /// stale authority or revision, unsupported reconciliation/topology, or an unexpected response.
    pub async fn publish_workflow_run_graph_edit(
        &self,
        request: bcode_workflow::WorkflowRunGraphEditBatch,
    ) -> Result<u64, ClientError> {
        match self
            .send_request(Request::PublishWorkflowRunGraphEdit { request })
            .await?
        {
            ResponsePayload::WorkflowRunGraphEditPublished { revision } => Ok(revision),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Read one bounded run-owned graph page.
    ///
    /// # Errors
    /// Returns an error for transport failures, missing or damaged graphs, invalid
    /// cursors or limits, or an expected revision that is no longer current.
    pub async fn inspect_workflow_run_graph(
        &self,
        request: bcode_workflow::WorkflowRunGraphPageRequest,
    ) -> Result<bcode_workflow::WorkflowRunGraphInspection, ClientError> {
        match self
            .send_request(Request::InspectWorkflowRunGraph { request })
            .await?
        {
            ResponsePayload::WorkflowRunGraphInspection { graph } => Ok(graph),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return one bounded aggregate workflow inspection snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached, the run is absent, or a bounded
    /// canonical query fails.
    pub async fn inspect_workflow_run(
        &self,
        run_id: String,
        limit: usize,
    ) -> Result<bcode_workflow::WorkflowRunInspection, ClientError> {
        match self
            .send_request(Request::InspectWorkflowRun { run_id, limit })
            .await?
        {
            ResponsePayload::WorkflowRunInspection { inspection } => Ok(*inspection),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return one durable workflow run summary.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn workflow_run_status(
        &self,
        run_id: String,
    ) -> Result<Option<bcode_workflow_store::WorkflowRunSummary>, ClientError> {
        match self
            .send_request(Request::WorkflowRunStatus { run_id })
            .await?
        {
            ResponsePayload::WorkflowRunStatus { run } => Ok(run),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return the newest workflow run associated with one exact generic binding key.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the bounded lookup.
    pub async fn associated_workflow_run(
        &self,
        key: bcode_workflow::WorkflowRunBindingLookup,
    ) -> Result<Option<bcode_workflow_store::WorkflowRunSummary>, ClientError> {
        match self
            .send_request(Request::AssociatedWorkflowRun { key })
            .await?
        {
            ResponsePayload::AssociatedWorkflowRun { run } => Ok(run),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Inspect the newest workflow run associated with one exact generic binding key.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or a bounded canonical query fails.
    pub async fn inspect_associated_workflow_run(
        &self,
        key: bcode_workflow::WorkflowRunBindingLookup,
        limit: usize,
    ) -> Result<Option<bcode_workflow::WorkflowRunInspection>, ClientError> {
        match self
            .send_request(Request::InspectAssociatedWorkflowRun { key, limit })
            .await?
        {
            ResponsePayload::AssociatedWorkflowRunInspection { inspection } => {
                Ok(inspection.map(|inspection| *inspection))
            }
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Apply one lifecycle transition to the newest run for one generic binding key.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached, lookup fails, or the transition is not
    /// valid for the associated run.
    pub async fn control_associated_workflow_run(
        &self,
        key: bcode_workflow::WorkflowRunBindingLookup,
        action: bcode_workflow::WorkflowRunControlAction,
    ) -> Result<(Option<bcode_workflow_store::WorkflowRunSummary>, bool), ClientError> {
        match self
            .send_request(Request::ControlAssociatedWorkflowRun { key, action })
            .await?
        {
            ResponsePayload::AssociatedWorkflowRunControlled { run, changed } => Ok((run, changed)),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return one bounded renderer-neutral workflow run projection.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the bounded request.
    pub async fn workflow_run_view(
        &self,
        run_id: String,
        limit: usize,
    ) -> Result<bcode_workflow_view_models::WorkflowRunView, ClientError> {
        match self
            .send_request(Request::WorkflowRunView { run_id, limit })
            .await?
        {
            ResponsePayload::WorkflowRunView { view } => Ok(*view),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return one bounded renderer-neutral workflow run catalog.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the bounded request.
    pub async fn workflow_catalog_view(
        &self,
        request: bcode_workflow_view_models::WorkflowCatalogRequest,
    ) -> Result<bcode_workflow_view_models::WorkflowCatalogView, ClientError> {
        match self
            .send_request(Request::WorkflowCatalogView { request })
            .await?
        {
            ResponsePayload::WorkflowCatalogView { view } => Ok(view),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List bounded durable workflow run summaries.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the bounded request.
    pub async fn list_workflow_runs(
        &self,
        limit: usize,
    ) -> Result<Vec<bcode_workflow_store::WorkflowRunSummary>, ClientError> {
        match self
            .send_request(Request::ListWorkflowRuns { limit })
            .await?
        {
            ResponsePayload::WorkflowRunList { runs } => Ok(runs),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return bounded canonical validated output values for one workflow run.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the bounded request.
    pub async fn workflow_run_outputs(
        &self,
        run_id: String,
        limit: usize,
    ) -> Result<Vec<bcode_workflow::WorkflowOutputInspection>, ClientError> {
        match self
            .send_request(Request::WorkflowRunOutputs { run_id, limit })
            .await?
        {
            ResponsePayload::WorkflowRunOutputs { outputs } => Ok(outputs),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Request durable cancellation for one workflow run.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn cancel_workflow_run(&self, run_id: String) -> Result<bool, ClientError> {
        match self
            .send_request(Request::CancelWorkflowRun { run_id })
            .await?
        {
            ResponsePayload::WorkflowRunCancellationRequested { recorded } => Ok(recorded),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Explicitly reconcile nonterminal workflow runs whose coordinator daemon verifiably ended.
    ///
    /// With `apply == false` this only reports what would change.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn reconcile_orphaned_workflow_runs(
        &self,
        apply: bool,
        limit: usize,
    ) -> Result<bcode_workflow::OrphanedWorkflowRunReport, ClientError> {
        match self
            .send_request(Request::ReconcileOrphanedWorkflowRuns { apply, limit })
            .await?
        {
            ResponsePayload::OrphanedWorkflowRunsReconciled { report } => Ok(report),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Pause one running workflow before further scheduler admission.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the transition.
    pub async fn pause_workflow_run(&self, run_id: String) -> Result<bool, ClientError> {
        match self
            .send_request(Request::PauseWorkflowRun { run_id })
            .await?
        {
            ResponsePayload::WorkflowRunPaused { changed } => Ok(changed),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Resume one paused workflow for subsequent scheduler admission.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the transition.
    pub async fn resume_workflow_run(&self, run_id: String) -> Result<bool, ClientError> {
        match self
            .send_request(Request::ResumeWorkflowRun { run_id })
            .await?
        {
            ResponsePayload::WorkflowRunResumed { changed } => Ok(changed),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Explicitly retry one exact latest failed workflow node attempt.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or the exact activation/attempt is stale,
    /// unsafe, cancelled, or outside its retry budget.
    pub async fn retry_workflow_node(
        &self,
        run_id: String,
        node_id: String,
        activation_id: String,
        failed_attempt: u32,
    ) -> Result<bcode_workflow_store::WorkflowNodeRetryResult, ClientError> {
        match self
            .send_request(Request::RetryWorkflowNode {
                run_id,
                node_id,
                activation_id,
                failed_attempt,
            })
            .await?
        {
            ResponsePayload::WorkflowNodeRetried { result } => Ok(result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List bounded durable input/approval waits for one run.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the bounded request.
    pub async fn list_workflow_waits(
        &self,
        run_id: String,
        limit: usize,
    ) -> Result<Vec<bcode_workflow::WaitingActivation>, ClientError> {
        match self
            .send_request(Request::ListWorkflowWaits { run_id, limit })
            .await?
        {
            ResponsePayload::WorkflowWaitList { waits } => Ok(waits),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Resolve one exact durable input wait with schema-validated JSON.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the identity, state, or value.
    pub async fn provide_workflow_input(
        &self,
        run_id: String,
        node_id: String,
        activation_id: String,
        value: serde_json::Value,
    ) -> Result<bcode_workflow_store::WaitingResolutionResult, ClientError> {
        match self
            .send_request(Request::ProvideWorkflowInput {
                run_id,
                node_id,
                activation_id,
                value,
            })
            .await?
        {
            ResponsePayload::WorkflowWaitResolved { result } => Ok(result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Resolve one exact durable approval wait.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the identity or state.
    pub async fn resolve_workflow_approval(
        &self,
        run_id: String,
        node_id: String,
        activation_id: String,
        approved: bool,
    ) -> Result<bcode_workflow_store::WaitingResolutionResult, ClientError> {
        match self
            .send_request(Request::ResolveWorkflowApproval {
                run_id,
                node_id,
                activation_id,
                approved,
            })
            .await?
        {
            ResponsePayload::WorkflowWaitResolved { result } => Ok(result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List bounded pending mutation approvals across all workflow runs.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the bounded request.
    pub async fn list_all_workflow_mutation_approvals(
        &self,
        limit: usize,
    ) -> Result<Vec<bcode_workflow::WorkflowMutationApprovalInspection>, ClientError> {
        match self
            .send_request(Request::ListWorkflowMutationApprovalsAll { limit })
            .await?
        {
            ResponsePayload::WorkflowMutationApprovalList { approvals } => Ok(approvals),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List bounded pending mutation approvals for one run.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the bounded request.
    pub async fn list_workflow_mutation_approvals(
        &self,
        run_id: String,
        limit: usize,
    ) -> Result<Vec<bcode_workflow::WorkflowMutationApprovalInspection>, ClientError> {
        match self
            .send_request(Request::ListWorkflowMutationApprovals { run_id, limit })
            .await?
        {
            ResponsePayload::WorkflowMutationApprovalList { approvals } => Ok(approvals),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Resolve one exact durable mutation approval.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the identity or decision.
    pub async fn resolve_workflow_mutation_approval(
        &self,
        approval_id: String,
        decision: bcode_workflow::WorkflowMutationApprovalDecision,
    ) -> Result<bcode_workflow::WorkflowMutationApprovalResolution, ClientError> {
        match self
            .send_request(Request::ResolveWorkflowMutationApproval {
                approval_id,
                decision,
            })
            .await?
        {
            ResponsePayload::WorkflowMutationApprovalResolved { result } => Ok(result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return one bounded page of workflow attempts.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the bounded request.
    pub async fn workflow_attempt_history(
        &self,
        run_id: String,
        cursor: Option<bcode_workflow_store::AttemptCursor>,
        limit: usize,
    ) -> Result<Vec<bcode_workflow::AttemptSummary>, ClientError> {
        match self
            .send_request(Request::WorkflowAttemptHistory {
                run_id,
                cursor,
                limit,
            })
            .await?
        {
            ResponsePayload::WorkflowAttemptHistory { attempts } => Ok(attempts),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return one bounded page of workflow events.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the bounded request.
    pub async fn workflow_event_history(
        &self,
        run_id: String,
        after_sequence: Option<u64>,
        limit: usize,
    ) -> Result<Vec<bcode_workflow::WorkflowHistoryEvent>, ClientError> {
        match self
            .send_request(Request::WorkflowEventHistory {
                run_id,
                after_sequence,
                limit,
            })
            .await?
        {
            ResponsePayload::WorkflowEventHistory { events } => Ok(events),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Request cancellation of a specific active runtime-work item.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn cancel_runtime_work(
        &self,
        session_id: SessionId,
        work_id: bcode_session_models::WorkId,
    ) -> Result<bool, ClientError> {
        match self
            .send_request(Request::CancelRuntimeWork {
                session_id,
                work_id,
            })
            .await?
        {
            ResponsePayload::RuntimeWorkCancellationRequested { cancelled } => Ok(cancelled),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List active runtime work for a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn list_runtime_work(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<bcode_session_models::RuntimeWorkSnapshot>, ClientError> {
        match self
            .send_request(Request::ListRuntimeWork { session_id })
            .await?
        {
            ResponsePayload::RuntimeWorkList { work } => Ok(work),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return recent runtime-work lifecycle events reconstructed from the bounded read model.
    ///
    /// The daemon clamps `limit` to 1 through
    /// [`bcode_session_models::MAX_SESSION_HISTORY_READ_EVENTS`]. Zero is not unlimited.
    /// A small window can omit a work's start event; these events are not a complete
    /// canonical event-log export.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn runtime_work_history(
        &self,
        session_id: SessionId,
        limit: usize,
    ) -> Result<Vec<bcode_session_models::SessionEvent>, ClientError> {
        match self
            .send_request(Request::RuntimeWorkHistory { session_id, limit })
            .await?
        {
            ResponsePayload::RuntimeWorkHistory { events } => Ok(events),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return grouped runtime-work lifecycle spans for a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the history request.
    pub async fn runtime_work_spans(
        &self,
        session_id: SessionId,
        limit: usize,
    ) -> Result<Vec<RuntimeWorkSpan>, ClientError> {
        Ok(runtime_work_spans(
            self.runtime_work_history(session_id, limit).await?,
        ))
    }

    /// Compact the model-visible context for a session while preserving append-only history.
    ///
    /// The request deadline applies until the daemon accepts the queued operation. After
    /// acceptance, this future awaits completion without an execution deadline. Poll it in
    /// a background task; session runtime subscriptions provide live progress, and normal
    /// session cancellation remains available. Transport errors are returned without replay.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn compact_session(&self, session_id: SessionId) -> Result<String, ClientError> {
        match self
            .send_request(Request::CompactSession { session_id })
            .await?
        {
            ResponsePayload::SessionCompacted { message, .. } => Ok(message),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List available agent profiles.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn list_agents(&self) -> Result<Vec<AgentInfo>, ClientError> {
        match self.send_request(Request::ListAgents).await? {
            ResponsePayload::AgentList { agents } => Ok(agents),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List available skills.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn list_skills(&self) -> Result<SkillList, ClientError> {
        match self.send_request(Request::ListSkills).await? {
            ResponsePayload::SkillList { skills } => Ok(*skills),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Describe a skill.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn describe_skill(&self, skill_id: SkillId) -> Result<SkillManifest, ClientError> {
        match self
            .send_request(Request::DescribeSkill { skill_id })
            .await?
        {
            ResponsePayload::SkillManifest { skill } => Ok(*skill),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Invoke a skill for one model turn.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn invoke_skill(
        &self,
        session_id: SessionId,
        skill_id: SkillId,
        arguments: String,
        display_text: String,
    ) -> Result<MessageAcceptance, ClientError> {
        self.invoke_skill_request(Request::InvokeSkill {
            session_id,
            skill_id,
            arguments,
            display_text,
        })
        .await
    }

    /// Invoke a skill for one model turn with immutable execution options.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn invoke_skill_with_execution(
        &self,
        session_id: SessionId,
        skill_id: SkillId,
        arguments: String,
        display_text: String,
        execution: bcode_session_models::TurnExecutionOptions,
    ) -> Result<MessageAcceptance, ClientError> {
        self.invoke_skill_request(Request::InvokeSkillWithExecution {
            session_id,
            skill_id,
            arguments,
            display_text,
            execution,
        })
        .await
    }

    async fn invoke_skill_request(
        &self,
        request: Request,
    ) -> Result<MessageAcceptance, ClientError> {
        decode_message_acceptance(&self.send_request(request).await?)
    }

    /// Activate a skill for a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn activate_skill(
        &self,
        session_id: SessionId,
        skill_id: SkillId,
    ) -> Result<(), ClientError> {
        match self
            .send_request(Request::ActivateSkill {
                session_id,
                skill_id,
            })
            .await?
        {
            ResponsePayload::SessionAgentSet => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Deactivate a skill for a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn deactivate_skill(
        &self,
        session_id: SessionId,
        skill_id: SkillId,
    ) -> Result<(), ClientError> {
        match self
            .send_request(Request::DeactivateSkill {
                session_id,
                skill_id,
            })
            .await?
        {
            ResponsePayload::SessionAgentSet => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return active skills for a session as loaded contexts.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn active_skills(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<bcode_skill_models::SkillContextResponse>, ClientError> {
        match self
            .send_request(Request::ActiveSkills { session_id })
            .await?
        {
            ResponsePayload::ActiveSkills { skills } => Ok(skills),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return agent policy provider status.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn agent_policy_status(&self) -> Result<PolicyStatusResponse, ClientError> {
        match self.send_request(Request::AgentPolicyStatus).await? {
            ResponsePayload::AgentPolicyStatus { status } => Ok(status),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Set a session-specific active agent profile.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn set_session_agent(
        &self,
        session_id: SessionId,
        agent_id: String,
    ) -> Result<(), ClientError> {
        match self
            .send_request(Request::SetSessionAgent {
                session_id,
                agent_id,
            })
            .await?
        {
            ResponsePayload::SessionAgentSet => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List pending permission checkpoints.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn list_permissions(&self) -> Result<Vec<PermissionSummary>, ClientError> {
        match self.send_request(Request::ListPermissions).await? {
            ResponsePayload::PermissionList { permissions } => Ok(permissions),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Resolve a pending permission checkpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn resolve_permission(
        &self,
        permission_id: String,
        approved: bool,
    ) -> Result<bool, ClientError> {
        self.resolve_permission_with_remember(permission_id, approved, false)
            .await
    }

    /// Resolve a pending permission checkpoint and optionally remember the policy decision.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn resolve_permission_with_remember(
        &self,
        permission_id: String,
        approved: bool,
        remember: bool,
    ) -> Result<bool, ClientError> {
        match self
            .send_request(Request::ResolvePermission {
                permission_id,
                approved,
                remember,
            })
            .await?
        {
            ResponsePayload::PermissionResolved { resolved } => Ok(resolved),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Resolve all currently pending checkpoints in one authorization batch.
    ///
    /// Batch decisions never persist a remembered policy rule; each targeted checkpoint receives
    /// the same one-time decision. Returns the number of checkpoints resolved.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn resolve_permission_batch(
        &self,
        batch_id: String,
        approved: bool,
    ) -> Result<usize, ClientError> {
        match self
            .send_request(Request::ResolvePermissionBatch { batch_id, approved })
            .await?
        {
            ResponsePayload::PermissionBatchResolved { resolved } => Ok(resolved),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List pending renderer-neutral tool exchanges.
    ///
    /// Results are observations ordered by exchange ID, not reservations. Exchanges
    /// may resolve or be cancelled before a subsequent resolution request. Request
    /// payloads retain their producer-owned schema and must not be interpreted as a
    /// supported older version when that schema is unknown to the caller.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn list_pending_tool_exchanges(
        &self,
    ) -> Result<Vec<PendingToolExchangeSummary>, ClientError> {
        match self.send_request(Request::ListPendingToolExchanges).await? {
            ResponsePayload::PendingToolExchangeList { exchanges } => Ok(exchanges),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Inspect one pending exchange without interpreting its producer-owned payload.
    ///
    /// Returns `None` when the exchange is no longer pending. This observation does
    /// not reserve the exchange or establish that a later resolution will succeed.
    ///
    /// # Errors
    ///
    /// Returns an error if listing fails or multiple exchanges claim the identifier.
    pub async fn inspect_pending_tool_exchange(
        &self,
        exchange_id: &str,
    ) -> Result<Option<PendingToolExchangeSummary>, ClientError> {
        select_pending_tool_exchange(self.list_pending_tool_exchanges().await?, exchange_id)
    }

    /// Resolve a pending renderer-neutral tool exchange.
    ///
    /// Returns `true` when this request resolves the pending exchange, or `false` when
    /// it is already terminal or no longer pending. A `false` result does not confirm
    /// that an earlier response carried the same payload. Transport failure does not
    /// establish whether the daemon committed the resolution.
    ///
    /// Callers may submit only `Responded` or `Cancelled`; other outcomes are host-owned.
    /// The daemon limits the complete JSON-encoded resolution to 64 KiB, including
    /// envelope fields and escaping, and requires a compatible interaction adapter.
    ///
    /// # Errors
    ///
    /// Returns an error when encoding fails, the daemon cannot be reached, the response
    /// is unexpected, or the daemon rejects an invalid/oversized resolution or an
    /// incompatible consumer.
    pub async fn resolve_tool_exchange(
        &self,
        exchange_id: String,
        resolution: bcode_session_models::ToolExchangeResolution,
    ) -> Result<bool, ClientError> {
        let resolution_json = serde_json::to_value(resolution).map_err(|error| {
            ClientError::Protocol(format!("exchange resolution encode failed: {error}"))
        })?;
        match self
            .send_request(Request::ResolveToolExchange {
                exchange_id,
                resolution_json,
            })
            .await?
        {
            ResponsePayload::ToolExchangeResolved { resolved } => Ok(resolved),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Persist and activate a permission policy rule under `[agent.<agent_id>.permission.<category>]`.
    ///
    /// `category` must be one of `command`, `read`, `write`, `edit`, or `web`.
    /// `action` must be one of `allow`, `ask`, or `deny`.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn add_permission_rule(
        &self,
        agent_id: String,
        category: String,
        pattern: String,
        action: String,
    ) -> Result<String, ClientError> {
        match self
            .send_request(Request::AddPermissionRule {
                agent_id,
                category,
                pattern,
                action,
            })
            .await?
        {
            ResponsePayload::PermissionRuleAdded { config_path } => Ok(config_path),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List services provided by loaded daemon plugins.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn plugin_services(&self) -> Result<Vec<PluginServiceSummary>, ClientError> {
        match self.send_request(Request::ListPluginServices).await? {
            ResponsePayload::PluginServices { services } => Ok(services),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List manifest-declared plugin contributions without executing plugin code.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn plugin_contributions(&self) -> Result<PluginContributions, ClientError> {
        match self.send_request(Request::ListPluginContributions).await? {
            ResponsePayload::PluginContributions { contributions } => Ok(contributions),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Capture a bounded stable source snapshot for generic derivation.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or the source cannot be read boundedly.
    pub async fn session_derivation_snapshot(
        &self,
        session_id: SessionId,
    ) -> Result<SessionDerivationSourceSnapshot, ClientError> {
        match self
            .send_request(Request::SessionDerivationSnapshot { session_id })
            .await?
        {
            ResponsePayload::SessionDerivationSnapshot { snapshot } => Ok(snapshot),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Read one bounded generation-pinned page of derivation prompt candidates.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached, the generation changed, or the query is
    /// invalid.
    pub async fn session_derivation_prompts(
        &self,
        session_id: SessionId,
        query: SessionDerivationPromptQuery,
    ) -> Result<SessionDerivationPromptPage, ClientError> {
        match self
            .send_request(Request::SessionDerivationPrompts { session_id, query })
            .await?
        {
            ResponsePayload::SessionDerivationPrompts { page } => Ok(page),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Execute one generic session derivation request.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or derivation fails.
    pub async fn derive_session(
        &self,
        request: SessionDerivationRequest,
    ) -> Result<SessionDerivationTerminalOutcome, ClientError> {
        match self
            .send_request(Request::DeriveSession {
                request: Box::new(request),
            })
            .await?
        {
            ResponsePayload::SessionDerived { outcome } => Ok(outcome),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return the latest derivation operation status.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or the operation is unknown.
    pub async fn session_derivation_status(
        &self,
        operation_id: bcode_session_models::SessionDerivationOperationId,
    ) -> Result<bcode_session_models::SessionDerivationOperationSnapshot, ClientError> {
        match self
            .send_request(Request::SessionDerivationStatus { operation_id })
            .await?
        {
            ResponsePayload::SessionDerivationStatus { snapshot } => Ok(snapshot),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Request cancellation of one running derivation operation.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn cancel_session_derivation(
        &self,
        operation_id: bcode_session_models::SessionDerivationOperationId,
    ) -> Result<bool, ClientError> {
        match self
            .send_request(Request::CancelSessionDerivation { operation_id })
            .await?
        {
            ResponsePayload::SessionDerivationCancellationRequested { accepted } => Ok(accepted),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Invoke a loaded daemon plugin service by explicit plugin ID.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn invoke_plugin_service(
        &self,
        plugin_id: String,
        interface_id: String,
        operation: String,
        payload: Vec<u8>,
    ) -> Result<PluginServiceResponse, ClientError> {
        match self
            .send_request(Request::InvokePluginService {
                plugin_id,
                interface_id,
                operation,
                payload,
            })
            .await?
        {
            ResponsePayload::PluginServiceResult { response } => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Validate a provider's configuration using this client's runtime context.
    ///
    /// # Errors
    /// Returns transport errors or a secret-safe error for provider failure or malformed output.
    pub async fn validate_provider_config(
        &self,
        provider_plugin_id: String,
    ) -> Result<bcode_model::ValidateConfigResponse, ClientError> {
        self.validate_model_config(Some(provider_plugin_id)).await
    }

    /// Validate the selected provider, or the unique registered provider when omitted.
    ///
    /// # Errors
    /// Returns transport, ambiguous-provider, provider-failure, or malformed-response errors.
    pub async fn validate_model_config(
        &self,
        provider_plugin_id: Option<String>,
    ) -> Result<bcode_model::ValidateConfigResponse, ClientError> {
        let response = if let Some(provider) = provider_plugin_id {
            self.invoke_plugin_service(
                provider,
                bcode_model::MODEL_PROVIDER_INTERFACE_ID.to_owned(),
                bcode_model::OP_VALIDATE_CONFIG.to_owned(),
                Vec::new(),
            )
            .await?
        } else {
            self.call_plugin_service(
                bcode_model::MODEL_PROVIDER_INTERFACE_ID.to_owned(),
                bcode_model::OP_VALIDATE_CONFIG.to_owned(),
                Vec::new(),
            )
            .await?
        };
        decode_provider_validation(&response)
    }

    /// Invoke a loaded daemon plugin service by interface ID.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn call_plugin_service(
        &self,
        interface_id: String,
        operation: String,
        payload: Vec<u8>,
    ) -> Result<PluginServiceResponse, ClientError> {
        match self
            .send_request(Request::CallPluginService {
                interface_id,
                operation,
                payload,
            })
            .await?
        {
            ResponsePayload::PluginServiceResult { response } => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Publish an event to matching daemon plugin subscriptions.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn publish_plugin_event(
        &self,
        topic: String,
        payload: Vec<u8>,
    ) -> Result<usize, ClientError> {
        match self
            .send_request(Request::PublishPluginEvent { topic, payload })
            .await?
        {
            ResponsePayload::PluginEventPublished { delivered } => Ok(delivered),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    async fn send_request(&self, request: Request) -> Result<ResponsePayload, ClientError> {
        self.send_request_once(request).await
    }

    async fn send_request_with_timeout(
        &self,
        request: Request,
        request_timeout: Duration,
    ) -> Result<ResponsePayload, ClientError> {
        let mut connection = self.connect("bcode-cli").await?;
        connection.request_timeout = request_timeout;
        connection.send_request(request).await
    }

    async fn send_request_once(&self, request: Request) -> Result<ResponsePayload, ClientError> {
        let mut connection = self.connect("bcode-cli").await?;
        connection.send_request(request).await
    }

    /// Open a long-lived connection to the daemon.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached, rejects the handshake, or reports a
    /// different build fingerprint.
    pub async fn connect(&self, client_name: &str) -> Result<ClientConnection, ClientError> {
        use tracing::Instrument as _;

        if let Some(error) = &self.runtime_context_error {
            return Err(ClientError::Protocol(error.clone()));
        }

        let span = tracing::debug_span!(target: "bcode_client::startup", "daemon_connection");
        Box::pin(async {
            let started = std::time::Instant::now();
            let mut acquisition_required = false;
            let result = async {
                match self.connect_with_deadline(client_name).await {
                    Ok(connection) => Ok(connection),
                    Err(error)
                        if self.daemon_availability == DaemonAvailability::AutoStart
                            && self.expected_state_location.is_none()
                            && error.is_daemon_unavailable() =>
                    {
                        acquisition_required = true;
                        self.ensure_daemon_available().await?;
                        self.connect_with_deadline(client_name).await
                    }
                    Err(error) => Err(error),
                }
            }
            .await;
            tracing::debug!(
                target: "bcode_client::startup",
                elapsed_us = started.elapsed().as_micros(),
                acquisition_required,
                success = result.is_ok(),
                "daemon connection completed"
            );
            result
        })
        .instrument(span)
        .await
    }

    async fn connect_with_deadline(
        &self,
        client_name: &str,
    ) -> Result<ClientConnection, ClientError> {
        let started = std::time::Instant::now();
        let result =
            tokio::time::timeout(self.connect_timeout, self.connect_once(client_name)).await;
        tracing::debug!(
            target: "bcode_client::startup",
            elapsed_us = started.elapsed().as_micros(),
            timed_out = result.is_err(),
            success = matches!(&result, Ok(Ok(_))),
            "verified connection attempt completed"
        );
        result.map_err(|_| ClientError::ConnectTimeout {
            timeout: self.connect_timeout,
        })?
    }

    /// Observe detached session-open preparation until terminal state or receiver drop.
    ///
    /// Dropping the returned receiver stops only this client observer. The server-owned migration
    /// continues independently.
    #[must_use]
    pub fn observe_session_open(&self, session_id: SessionId) -> SessionOpenProgressObserver {
        let client = self.clone();
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut connection = client.connect("bcode-session-open-observer").await?;
            let close_sender = sender.clone();
            let observation = connection.prepare_session_open_while(session_id, |snapshot| {
                sender.send(snapshot.clone()).is_ok()
            });
            tokio::pin!(observation);
            tokio::select! {
                result = &mut observation => {
                    result?;
                }
                () = close_sender.closed() => {}
            }
            Ok(())
        });
        SessionOpenProgressObserver { receiver, task }
    }

    async fn connect_once(&self, client_name: &str) -> Result<ClientConnection, ClientError> {
        let transport_started = std::time::Instant::now();
        let stream = LocalIpcStream::connect(&self.endpoint).await;
        tracing::debug!(
            target: "bcode_client::startup",
            elapsed_us = transport_started.elapsed().as_micros(),
            success = stream.is_ok(),
            "local transport connection completed"
        );
        let stream = stream?;
        let handshake_started = std::time::Instant::now();
        let mut connection = ClientConnection {
            stream,
            next_request_id: 1,
            client_id: None,
            pending_events: VecDeque::new(),
            request_timeout: self.request_timeout,
            reconnect_client: Some(std::sync::Arc::new(self.clone())),
            reconnect_name: std::sync::Arc::from(client_name),
            restore_state: ConnectionRestoreState {
                runtime_context: self.runtime_context.clone(),
                ..ConnectionRestoreState::default()
            },
        };
        match connection
            .send_request(Request::Hello {
                client_name: format!("{client_name};cap=message_accepted"),
                runtime_context: self.runtime_context.clone(),
                daemon_namespace: bcode_ipc::daemon_namespace(),
                artifact_id: Some(bcode_ipc::ArtifactId::current()),
                build_fingerprint: bcode_ipc::BUILD_FINGERPRINT.to_owned(),
                state_location_id: Some(
                    self.expected_state_location
                        .as_ref()
                        .map_or_else(bcode_ipc::state_location_id, |id| id.as_str().to_owned()),
                ),
            })
            .await?
        {
            ResponsePayload::Hello {
                client_id, daemon, ..
            } => {
                self.verify_server_identity(&daemon)?;
                tracing::debug!(
                    target: "bcode_client::startup",
                    elapsed_us = handshake_started.elapsed().as_micros(),
                    "daemon identity handshake verified"
                );
                connection.client_id = Some(client_id);
                Ok(connection)
            }
            _ => Err(ClientError::UnexpectedResponse),
        }
    }
}

#[derive(Debug, Clone)]
enum SessionRestoreAttachment {
    Full {
        session_id: SessionId,
    },
    Recent {
        session_id: SessionId,
        limit: usize,
    },
    Projection {
        session_id: SessionId,
        request: ProjectionWindowRequest,
    },
}

impl SessionRestoreAttachment {
    const fn session_id(&self) -> SessionId {
        match self {
            Self::Full { session_id }
            | Self::Recent { session_id, .. }
            | Self::Projection { session_id, .. } => *session_id,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct ConnectionRestoreState {
    runtime_context: Option<ClientRuntimeContext>,
    attached_session: Option<SessionRestoreAttachment>,
    catalog_updates: bool,
    workflow_runs: bool,
    runtime_work_sessions: std::collections::BTreeSet<SessionId>,
}

/// Long-lived client connection.
#[derive(Debug)]
pub struct ClientConnection {
    stream: LocalIpcStream,
    next_request_id: u64,
    client_id: Option<ClientId>,
    pending_events: VecDeque<Event>,
    request_timeout: Duration,
    reconnect_client: Option<std::sync::Arc<BcodeClient>>,
    reconnect_name: std::sync::Arc<str>,
    restore_state: ConnectionRestoreState,
}

impl ClientConnection {
    /// Deliver input on this connection, retaining its invocation-control identity.
    ///
    /// # Errors
    /// Returns a transport, ownership, or input-routing error. Input is not replayed automatically.
    pub async fn send_invocation_input(
        &mut self,
        session_id: SessionId,
        input: bcode_tool::ToolInvocationInput,
    ) -> Result<(), ClientError> {
        match self
            .send_request(Request::InvocationInput { session_id, input })
            .await?
        {
            ResponsePayload::InvocationInputAccepted => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return the server-assigned client identifier.
    #[must_use]
    pub const fn client_id(&self) -> Option<ClientId> {
        self.client_id
    }

    /// Replace the runtime context attached to this long-lived connection.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn update_runtime_context(
        &mut self,
        runtime_context: Option<ClientRuntimeContext>,
    ) -> Result<(), ClientError> {
        match self
            .send_request(Request::UpdateClientRuntimeContext {
                runtime_context: runtime_context.clone(),
            })
            .await?
        {
            ResponsePayload::ClientRuntimeContextUpdated => {
                self.restore_state.runtime_context = runtime_context;
                Ok(())
            }
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Refresh this long-lived connection's runtime context from the current process.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn refresh_runtime_context(&mut self) -> Result<(), ClientError> {
        self.update_runtime_context(Some(current_runtime_context()?))
            .await
    }

    /// Subscribe this connection to catalog update events.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn subscribe_catalog_updates(&mut self) -> Result<(), ClientError> {
        match self.send_request(Request::SubscribeCatalogUpdates).await? {
            ResponsePayload::CatalogUpdatesSubscribed => {
                self.restore_state.catalog_updates = true;
                Ok(())
            }
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Return a bounded page of workflow live notifications after one global sequence.
    ///
    /// A page marked `resync_required` must be replaced with bounded catalog/run snapshots rather
    /// than repeatedly paging and claiming durable stream resume.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the bounded request.
    pub async fn workflow_live_event_catch_up(
        &mut self,
        after_sequence: u64,
        limit: usize,
    ) -> Result<bcode_workflow_view_models::WorkflowLiveEventPage, ClientError> {
        match self
            .send_request(Request::WorkflowLiveEventCatchUp {
                after_sequence,
                limit,
            })
            .await?
        {
            ResponsePayload::WorkflowLiveEventCatchUp { page } => Ok(page),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Subscribe this connection to workflow-run canonical-state notifications.
    ///
    /// The stream is live-only and does not imply durable resume. Obtain bounded snapshots through
    /// [`Self::workflow_catalog_view`] and [`Self::workflow_run_view`].
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn subscribe_workflow_runs(&mut self) -> Result<u64, ClientError> {
        match self.send_request(Request::SubscribeWorkflowRuns).await? {
            ResponsePayload::WorkflowRunsSubscribed { after_sequence } => {
                self.restore_state.workflow_runs = true;
                Ok(after_sequence)
            }
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Subscribe this connection to runtime-work events for one session.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn subscribe_runtime_work(
        &mut self,
        session_id: SessionId,
    ) -> Result<(), ClientError> {
        match self
            .send_request(Request::SubscribeRuntimeWork { session_id })
            .await?
        {
            ResponsePayload::RuntimeWorkSubscribed => {
                self.restore_state.runtime_work_sessions.insert(session_id);
                Ok(())
            }
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// List sessions for the current working directory on this connection.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn list_sessions_with_status(&mut self) -> Result<SessionList, ClientError> {
        self.list_sessions_in_working_directory(current_working_directory())
            .await
    }

    /// List sessions and catalog status for an explicit working directory on this connection.
    ///
    /// This selects discovery scope only; it does not confer authority over returned sessions.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn list_sessions_in_working_directory(
        &mut self,
        working_directory: PathBuf,
    ) -> Result<SessionList, ClientError> {
        let working_directory = std::path::absolute(working_directory).map_err(|_| {
            ClientError::Protocol("cannot resolve catalog working directory".into())
        })?;
        match self
            .send_request(Request::ListSessions { working_directory })
            .await?
        {
            ResponsePayload::SessionList {
                sessions,
                catalog_status,
                catalog_sources,
                catalog_revision,
            } => Ok(SessionList {
                sessions,
                catalog_status,
                catalog_sources,
                catalog_revision,
            }),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Read one bounded, non-mutating session compatibility inventory page.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn session_compatibility_inventory(
        &mut self,
        request: SessionCompatibilityInventoryRequest,
    ) -> Result<SessionCompatibilityInventoryResponse, ClientError> {
        match self
            .send_request(Request::SessionCompatibilityInventory { request })
            .await?
        {
            ResponsePayload::SessionCompatibilityInventory { response } => Ok(response),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Attach to a session and return replayed history.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn attach_session(
        &mut self,
        session_id: SessionId,
    ) -> Result<Vec<SessionEvent>, ClientError> {
        self.attach_session_with_input_history(session_id)
            .await
            .map(|attached| attached.history)
    }

    /// Attach to a session and return replayed history plus input-history entries.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn attach_session_with_input_history(
        &mut self,
        session_id: SessionId,
    ) -> Result<AttachedSessionHistory, ClientError> {
        let attached = match self
            .send_request(Request::AttachSession { session_id })
            .await?
        {
            ResponsePayload::Attached {
                history,
                input_history,
                usage_summary,
                import_warnings,
                draft,
                runtime_selection,
                projection_window,
                session,
                ..
            } => AttachedSessionHistory {
                session,
                history,
                input_history,
                usage_summary: usage_summary.map_or_else(
                    bcode_session_models::SessionUsageSummary::default,
                    |summary| *summary,
                ),
                import_warnings,
                draft,
                runtime_selection,
                projection_window,
            },
            _ => return Err(ClientError::UnexpectedResponse),
        };
        self.restore_state.attached_session = Some(SessionRestoreAttachment::Full { session_id });
        Ok(attached)
    }

    /// Classify session storage and start or join legacy migration when required.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects preparation.
    pub async fn prepare_session_open(
        &mut self,
        session_id: SessionId,
    ) -> Result<bcode_session_models::SessionOpenOperationSnapshot, ClientError> {
        match self
            .send_request(Request::PrepareSessionOpen { session_id })
            .await?
        {
            ResponsePayload::SessionOpenPrepared { snapshot } => Ok(snapshot),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Wait for a newer session-open snapshot or a bounded server timeout.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached, the operation identity is stale, or
    /// the request is rejected.
    pub async fn wait_session_open_progress(
        &mut self,
        session_id: SessionId,
        operation_id: bcode_session_models::SessionOpenOperationId,
        after_revision: u64,
        timeout: Duration,
    ) -> Result<bcode_session_models::SessionOpenOperationSnapshot, ClientError> {
        let timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
        match self
            .send_request(Request::WaitSessionOpenProgress {
                session_id,
                operation_id,
                after_revision,
                timeout_ms,
            })
            .await?
        {
            ResponsePayload::SessionOpenPrepared { snapshot } => Ok(snapshot),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    /// Prepare a session until it reaches a terminal state, invoking `on_progress` for every
    /// observed snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when preparation or progress observation fails.
    pub async fn prepare_session_open_until_terminal<F>(
        &mut self,
        session_id: SessionId,
        mut on_progress: F,
    ) -> Result<bcode_session_models::SessionOpenOperationSnapshot, ClientError>
    where
        F: FnMut(&bcode_session_models::SessionOpenOperationSnapshot),
    {
        self.prepare_session_open_while(session_id, |snapshot| {
            on_progress(snapshot);
            true
        })
        .await
    }

    async fn prepare_session_open_while<F>(
        &mut self,
        session_id: SessionId,
        mut on_progress: F,
    ) -> Result<bcode_session_models::SessionOpenOperationSnapshot, ClientError>
    where
        F: FnMut(&bcode_session_models::SessionOpenOperationSnapshot) -> bool,
    {
        let mut snapshot = self.prepare_session_open(session_id).await?;
        if !on_progress(&snapshot) {
            return Ok(snapshot);
        }
        let mut reconnect_attempts = 0_u8;
        while snapshot.outcome.is_none() {
            match self
                .wait_session_open_progress(
                    session_id,
                    snapshot.operation_id,
                    snapshot.revision,
                    Duration::from_secs(5),
                )
                .await
            {
                Ok(next) => {
                    snapshot = next;
                    if !on_progress(&snapshot) {
                        return Ok(snapshot);
                    }
                }
                Err(error)
                    if error.is_daemon_unavailable()
                        && reconnect_attempts < 3
                        && self.reconnect_client.is_some() =>
                {
                    reconnect_attempts = reconnect_attempts.saturating_add(1);
                    self.reconnect_for_session_open().await?;
                    snapshot = match self
                        .wait_session_open_progress(
                            session_id,
                            snapshot.operation_id,
                            snapshot.revision,
                            Duration::ZERO,
                        )
                        .await
                    {
                        Ok(recovered) => recovered,
                        Err(ClientError::Server { code, .. })
                            if code == "session_open_operation_not_found" =>
                        {
                            self.prepare_session_open(session_id).await?
                        }
                        Err(error) => return Err(error),
                    };
                    if !on_progress(&snapshot) {
                        return Ok(snapshot);
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Ok(snapshot)
    }

    async fn reconnect_for_session_open(&mut self) -> Result<(), ClientError> {
        self.reconnect_and_restore().await
    }

    async fn reconnect_and_restore(&mut self) -> Result<(), ClientError> {
        let client = self
            .reconnect_client
            .clone()
            .ok_or(ClientError::UnexpectedResponse)?;
        let restore_state = self.restore_state.clone();
        let mut delay = Duration::from_millis(25);
        let mut replacement = loop {
            match client.connect(&self.reconnect_name).await {
                Ok(connection) => break connection,
                Err(error) if error.is_daemon_unavailable() => {
                    tokio::time::sleep(delay).await;
                    delay = delay.saturating_mul(2).min(Duration::from_secs(2));
                }
                Err(error) => return Err(error),
            }
        };
        replacement.restore_connection_state(&restore_state).await?;
        let mut pending_events = std::mem::take(&mut self.pending_events);
        pending_events.append(&mut replacement.pending_events);
        if let Some(attachment) = &restore_state.attached_session {
            pending_events.push_back(Event::SessionViewResyncRequired {
                session_id: attachment.session_id(),
            });
        }
        replacement.pending_events = pending_events;
        *self = replacement;
        Ok(())
    }

    async fn restore_connection_state(
        &mut self,
        restore: &ConnectionRestoreState,
    ) -> Result<(), ClientError> {
        if self.restore_state.runtime_context != restore.runtime_context {
            match self
                .send_request(Request::UpdateClientRuntimeContext {
                    runtime_context: restore.runtime_context.clone(),
                })
                .await?
            {
                ResponsePayload::ClientRuntimeContextUpdated => {}
                _ => return Err(ClientError::UnexpectedResponse),
            }
        }
        if let Some(attachment) = &restore.attached_session {
            let request = match attachment {
                SessionRestoreAttachment::Full { session_id } => Request::AttachSession {
                    session_id: *session_id,
                },
                SessionRestoreAttachment::Recent { session_id, limit } => {
                    Request::AttachSessionRecent {
                        session_id: *session_id,
                        limit: *limit,
                    }
                }
                SessionRestoreAttachment::Projection {
                    session_id,
                    request,
                } => Request::AttachSessionProjectionWindow {
                    session_id: *session_id,
                    request: request.clone(),
                },
            };
            if !matches!(
                self.send_request(request).await?,
                ResponsePayload::Attached { .. }
            ) {
                return Err(ClientError::UnexpectedResponse);
            }
        }
        if restore.catalog_updates
            && !matches!(
                self.send_request(Request::SubscribeCatalogUpdates).await?,
                ResponsePayload::CatalogUpdatesSubscribed
            )
        {
            return Err(ClientError::UnexpectedResponse);
        }
        if restore.workflow_runs
            && !matches!(
                self.send_request(Request::SubscribeWorkflowRuns).await?,
                ResponsePayload::WorkflowRunsSubscribed { .. }
            )
        {
            return Err(ClientError::UnexpectedResponse);
        }
        for session_id in &restore.runtime_work_sessions {
            if !matches!(
                self.send_request(Request::SubscribeRuntimeWork {
                    session_id: *session_id,
                })
                .await?,
                ResponsePayload::RuntimeWorkSubscribed
            ) {
                return Err(ClientError::UnexpectedResponse);
            }
        }
        self.restore_state = restore.clone();
        Ok(())
    }

    /// Attach to a session and return a recent history window.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn attach_session_recent(
        &mut self,
        session_id: SessionId,
        limit: usize,
    ) -> Result<Vec<SessionEvent>, ClientError> {
        self.attach_session_recent_with_input_history(session_id, limit)
            .await
            .map(|attached| attached.history)
    }

    /// Attach to a session and return a recent history window plus input-history entries.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn attach_session_recent_with_input_history(
        &mut self,
        session_id: SessionId,
        limit: usize,
    ) -> Result<AttachedSessionHistory, ClientError> {
        let attached = match self
            .send_request(Request::AttachSessionRecent { session_id, limit })
            .await?
        {
            ResponsePayload::Attached {
                history,
                input_history,
                usage_summary,
                import_warnings,
                draft,
                runtime_selection,
                projection_window,
                session,
                ..
            } => AttachedSessionHistory {
                session,
                history,
                input_history,
                usage_summary: usage_summary.map_or_else(
                    bcode_session_models::SessionUsageSummary::default,
                    |summary| *summary,
                ),
                import_warnings,
                draft,
                runtime_selection,
                projection_window,
            },
            _ => return Err(ClientError::UnexpectedResponse),
        };
        self.restore_state.attached_session =
            Some(SessionRestoreAttachment::Recent { session_id, limit });
        Ok(attached)
    }

    /// Prepare a session to a terminal state, then attach with a bounded projection window.
    ///
    /// # Errors
    ///
    /// Returns an error when preparation fails, reaches a terminal state that cannot be attached,
    /// or attach fails. Ready states use the bounded attach path; degraded/read-only and all other
    /// non-ready terminal states return without attaching.
    pub async fn prepare_then_attach_session_projection_window<F>(
        &mut self,
        session_id: SessionId,
        request: bcode_session_models::ProjectionWindowRequest,
        on_progress: F,
    ) -> Result<AttachedSessionHistory, ClientError>
    where
        F: FnMut(&bcode_session_models::SessionOpenOperationSnapshot),
    {
        let snapshot = self
            .prepare_session_open_until_terminal(session_id, on_progress)
            .await?;
        session_open_attach_readiness(&snapshot)?;
        self.attach_session_projection_window_with_input_history(session_id, request)
            .await
    }

    /// Attach to a session and return a projection-sized history window plus input-history entries.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn attach_session_projection_window_with_input_history(
        &mut self,
        session_id: SessionId,
        request: ProjectionWindowRequest,
    ) -> Result<AttachedSessionHistory, ClientError> {
        let restore_request = request.clone();
        let attached = match self
            .send_request(Request::AttachSessionProjectionWindow {
                session_id,
                request,
            })
            .await?
        {
            ResponsePayload::Attached {
                history,
                input_history,
                usage_summary,
                import_warnings,
                draft,
                runtime_selection,
                projection_window,
                session,
                ..
            } => AttachedSessionHistory {
                session,
                history,
                input_history,
                usage_summary: usage_summary.map_or_else(
                    bcode_session_models::SessionUsageSummary::default,
                    |summary| *summary,
                ),
                import_warnings,
                draft,
                runtime_selection,
                projection_window,
            },
            _ => return Err(ClientError::UnexpectedResponse),
        };
        self.restore_state.attached_session = Some(SessionRestoreAttachment::Projection {
            session_id,
            request: restore_request,
        });
        Ok(attached)
    }

    /// Send a user message to a session.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon cannot be reached or rejects the request.
    pub async fn send_user_message(
        &mut self,
        session_id: SessionId,
        text: String,
        placement: bcode_session_models::PromptPlacement,
    ) -> Result<MessageAcceptance, ClientError> {
        decode_message_acceptance(
            &self
                .send_request(Request::SendUserMessageWithPlacement {
                    session_id,
                    text,
                    placement,
                })
                .await?,
        )
    }

    /// Receive without implicit reconnection, allowing the caller to expose continuity loss.
    ///
    /// # Errors
    /// Returns transport or decoding errors. Keep the future alive until completion;
    /// cancelling a partial envelope read requires discarding this connection.
    pub async fn recv_event_without_reconnect(&mut self) -> Result<Event, ClientError> {
        loop {
            if let Some(event) = self.pending_events.pop_front() {
                return Ok(event);
            }
            let envelope = recv_envelope(&mut self.stream).await?;
            if envelope.kind == EnvelopeKind::Event {
                return decode_event(&envelope.payload).map_err(ClientError::from);
            }
        }
    }

    /// Receive the next server event.
    ///
    /// # Errors
    ///
    /// Returns an error when replacement/reconnection fails or an event cannot be decoded.
    pub async fn recv_event(&mut self) -> Result<Event, ClientError> {
        loop {
            if let Some(event) = self.pending_events.pop_front() {
                return Ok(event);
            }
            match recv_envelope(&mut self.stream).await {
                Ok(envelope) => {
                    if envelope.kind != EnvelopeKind::Event {
                        continue;
                    }
                    return decode_event(&envelope.payload).map_err(ClientError::from);
                }
                Err(error) => {
                    let error = ClientError::from(error);
                    if !error.is_daemon_unavailable() || self.reconnect_client.is_none() {
                        return Err(error);
                    }
                    self.reconnect_and_restore().await?;
                }
            }
        }
    }

    async fn send_request(&mut self, request: Request) -> Result<ResponsePayload, ClientError> {
        let request_id = self.next_request_id;
        self.next_request_id += 1;
        let envelope = request_envelope(request_id, &request)?;
        send_envelope(&mut self.stream, &envelope).await?;

        let mut compaction_accepted = false;
        loop {
            // The deadline covers admission, not queued/provider execution. Transport loss
            // still terminates the wait; never retry potentially accepted work here.
            let envelope = if compaction_accepted {
                recv_envelope(&mut self.stream).await?
            } else {
                tokio::time::timeout(self.request_timeout, recv_envelope(&mut self.stream))
                    .await
                    .map_err(|_| ClientError::RequestTimeout {
                        timeout: self.request_timeout,
                    })??
            };
            if envelope.kind == EnvelopeKind::Event {
                self.pending_events
                    .push_back(decode_event(&envelope.payload).map_err(ClientError::from)?);
                continue;
            }
            if envelope.kind != EnvelopeKind::Response || envelope.request_id != request_id {
                continue;
            }
            let response: Response = decode_response(&envelope.payload)?;
            if matches!(
                response,
                Response::Ok(ResponsePayload::SessionCompactionAccepted)
            ) {
                if !matches!(request, Request::CompactSession { .. }) || compaction_accepted {
                    return Err(ClientError::UnexpectedResponse);
                }
                compaction_accepted = true;
                continue;
            }
            return match response {
                Response::Ok(payload) => Ok(payload),
                Response::Err(error) => Err(error.into()),
            };
        }
    }
}

fn session_open_attach_readiness(
    snapshot: &bcode_session_models::SessionOpenOperationSnapshot,
) -> Result<(), ClientError> {
    let session_id = snapshot.session_id;
    let stage_message = &snapshot.progress.message;
    let verified_backup_path = snapshot.backup_path.as_deref();
    match &snapshot.outcome {
        Some(bcode_session_models::SessionOpenTerminalOutcome::Ready) => Ok(()),
        Some(bcode_session_models::SessionOpenTerminalOutcome::DegradedReadOnly {
            issue_count,
        }) => Err(ClientError::Server {
            code: "session_degraded_read_only".to_owned(),
            message: format!(
                "session contains {issue_count} unsupported persisted event(s); bounded history remains inspectable but writable attach is disabled"
            ),
        }),
        Some(bcode_session_models::SessionOpenTerminalOutcome::WriterIncompatible {
            actual,
            expected,
        }) => Err(ClientError::Server {
            code: "session_writer_incompatible".to_owned(),
            message: terminal_session_open_error_message(
                session_id,
                stage_message,
                &format!(
                    "session writer epoch {actual:?} is incompatible with expected epoch {expected}"
                ),
                verified_backup_path,
            ),
        }),
        Some(bcode_session_models::SessionOpenTerminalOutcome::RepairRequired { reason }) => {
            Err(ClientError::Server {
                code: "session_repair_required".to_owned(),
                message: terminal_session_open_error_message(
                    session_id,
                    stage_message,
                    reason,
                    verified_backup_path,
                ),
            })
        }
        Some(bcode_session_models::SessionOpenTerminalOutcome::Failed {
            kind,
            message,
            backup_path,
        }) => Err(ClientError::Server {
            code: session_open_failure_code(*kind).to_owned(),
            message: terminal_session_open_error_message(
                session_id,
                stage_message,
                message,
                backup_path.as_deref().or(verified_backup_path),
            ),
        }),
        None => Err(ClientError::UnexpectedResponse),
    }
}

fn validate_bounded_long_poll_timeout(
    operation: &'static str,
    timeout_ms: u64,
) -> Result<(), ClientError> {
    if timeout_ms == 0 || timeout_ms > 30_000 {
        return Err(ClientError::Protocol(format!(
            "{operation} wait timeout must be between 1 and 30000 milliseconds"
        )));
    }
    Ok(())
}

fn validate_session_search_backfill_wait_timeout(timeout_ms: u64) -> Result<(), ClientError> {
    validate_bounded_long_poll_timeout("backfill", timeout_ms)
}

fn validate_session_bulk_migration_wait_timeout(timeout_ms: u64) -> Result<(), ClientError> {
    validate_bounded_long_poll_timeout("bulk migration", timeout_ms)
}

fn terminal_session_open_error_message(
    session_id: SessionId,
    stage_message: &str,
    reason: &str,
    backup_path: Option<&std::path::Path>,
) -> String {
    let backup = backup_path.map_or_else(String::new, |path| {
        format!(" Retained backup: {}.", path.display())
    });
    format!(
        "session preparation failed during {stage_message}: {reason}.{backup} Diagnose with `bcode session diagnose {session_id}`."
    )
}

const fn session_open_failure_code(
    kind: bcode_session_models::SessionOpenFailureKind,
) -> &'static str {
    match kind {
        bcode_session_models::SessionOpenFailureKind::OwnedByOtherDaemon => {
            "session_active_elsewhere"
        }
        bcode_session_models::SessionOpenFailureKind::WriterIncompatible => {
            "session_writer_incompatible"
        }
        bcode_session_models::SessionOpenFailureKind::ProjectionStale => "projection_stale",
        bcode_session_models::SessionOpenFailureKind::RepairRequired => "session_repair_required",
        bcode_session_models::SessionOpenFailureKind::BackupFailed => {
            "session_migration_backup_failed"
        }
        bcode_session_models::SessionOpenFailureKind::MigrationFailed => "session_migration_failed",
        bcode_session_models::SessionOpenFailureKind::NotFound => "session_not_found",
    }
}

#[cfg(test)]
mod message_acceptance_tests {
    use super::*;
    use bcode_session_models::MessageAcceptanceDisposition as Disposition;

    #[test]
    fn acceptance_decoder_preserves_all_reported_fields() {
        for disposition in [
            Disposition::StartedTurn,
            Disposition::AppliedSteering,
            Disposition::QueuedFollowUp,
            Disposition::QueuedTurn,
        ] {
            for queued in [false, true] {
                for queue_position in [None, Some(0), Some(u32::MAX)] {
                    let result = decode_message_acceptance(
                        &ResponsePayload::MessageAcceptedWithDisposition {
                            queued,
                            queue_position,
                            disposition,
                        },
                    )
                    .expect("acceptance");
                    assert_eq!(
                        result,
                        MessageAcceptance {
                            queued,
                            queue_position,
                            disposition
                        }
                    );
                }
            }
        }
    }

    #[test]
    fn acceptance_decoder_preserves_compatibility_and_rejects_unrelated_responses() {
        assert_eq!(
            decode_message_acceptance(&ResponsePayload::MessageSent).unwrap(),
            MessageAcceptance::sent()
        );
        for queued in [false, true] {
            for queue_position in [None, Some(0), Some(u32::MAX)] {
                assert_eq!(
                    decode_message_acceptance(&ResponsePayload::MessageAccepted {
                        queued,
                        queue_position
                    })
                    .unwrap(),
                    MessageAcceptance {
                        queued,
                        queue_position,
                        disposition: Disposition::StartedTurn,
                    }
                );
            }
        }
        assert!(matches!(
            decode_message_acceptance(&ResponsePayload::ComposerDraftSet),
            Err(ClientError::UnexpectedResponse)
        ));
    }
}

fn select_pending_tool_exchange(
    exchanges: Vec<PendingToolExchangeSummary>,
    exchange_id: &str,
) -> Result<Option<PendingToolExchangeSummary>, ClientError> {
    let mut matches = exchanges
        .into_iter()
        .filter(|exchange| exchange.request.exchange_id == exchange_id);
    let exchange = matches.next();
    if matches.next().is_some() {
        return Err(ClientError::Protocol(
            "ambiguous pending interaction identifier".to_owned(),
        ));
    }
    Ok(exchange)
}

#[cfg(test)]
mod client_timeout_tests {
    #[test]
    fn pending_exchange_selection_preserves_schema_and_rejects_ambiguity() {
        let exchange = bcode_session_models::PendingToolExchangeSummary {
            session_id: bcode_session_models::SessionId::new(),
            request: bcode_session_models::ToolExchangeRequest {
                invocation_id: "invocation".to_owned(),
                exchange_id: "target".to_owned(),
                producer_id: "producer".to_owned(),
                schema: "future.schema".to_owned(),
                schema_version: u32::MAX,
                payload: serde_json::json!({ "opaque": [1, "value"] }),
                response_policy: bcode_session_models::ToolExchangeResponsePolicy::Required,
            },
        };
        let mut other = exchange.clone();
        other.request.exchange_id = "other".to_owned();
        let selected = super::select_pending_tool_exchange(vec![other, exchange.clone()], "target")
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::to_value(selected).unwrap(),
            serde_json::to_value(exchange.clone()).unwrap()
        );
        for conflicting_session in [false, true] {
            let mut duplicate = exchange.clone();
            if conflicting_session {
                duplicate.session_id = bcode_session_models::SessionId::new();
                duplicate.request.payload = serde_json::json!({ "secret": "must-not-leak" });
            }
            let error =
                super::select_pending_tool_exchange(vec![exchange.clone(), duplicate], "target")
                    .unwrap_err();
            assert!(matches!(error, super::ClientError::Protocol(ref message)
                if message == "ambiguous pending interaction identifier"));
        }
        assert!(
            super::select_pending_tool_exchange(vec![exchange], "missing")
                .unwrap()
                .is_none()
        );
        assert!(
            super::select_pending_tool_exchange(vec![], "target")
                .unwrap()
                .is_none()
        );
    }

    use super::{
        BcodeClient, ClientError, resolve_path_from, session_open_attach_readiness,
        terminal_session_open_error_message,
    };
    use bcode_session_models::{
        SessionId, SessionMigrationProgress, SessionMigrationStage, SessionOpenOperationId,
        SessionOpenOperationSnapshot, SessionOpenTerminalOutcome,
    };
    use bcode_session_search::{
        SessionSearchBackfillOperationState, SessionSearchBackfillOperationStatus,
    };
    use std::path::Path;
    use std::time::Duration;

    #[tokio::test]
    async fn typed_definition_inspection_rejects_untrusted_responses() {
        use bcode_workflow::WorkflowAuthoringApplication as _;
        for (identity, version) in [("requested", 1), ("wrong", 1), ("requested", 2)] {
            let workflow = bcode_workflow::WorkflowBuilder::new(
                "requested",
                bcode_workflow::Step::task("node", |value: u32, _context| async move { Ok(value) }),
            )
            .build()
            .expect("valid workflow");
            let value = serde_json::to_value(workflow.definition()).expect("definition value");
            let mut definition = bcode_workflow::StoredWorkflowDefinition {
                definition_id: identity.to_string(),
                version,
                checksum_sha256: bcode_workflow::workflow_canonical_value_sha256(&value)
                    .expect("checksum"),
                definition_json: serde_json::to_string(&value).expect("definition JSON"),
            };
            assert!(
                definition.definition().is_ok(),
                "mismatch fixtures must otherwise validate"
            );
            if identity == "requested" && version == 1 {
                definition.definition_json = "private malformed content".to_string();
            }
            let directory =
                std::path::PathBuf::from(format!("/tmp/bci-{}", SessionOpenOperationId::new()));
            std::fs::create_dir_all(&directory).expect("socket directory");
            let endpoint = bcode_ipc::IpcEndpoint::unix_socket(directory.join("inspect.sock"));
            let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).expect("listener");
            let server = tokio::spawn(async move {
                let mut stream = listener.accept().await.expect("accept");
                let hello = bcode_ipc::recv_envelope(&mut stream).await.expect("hello");
                let response = bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::Hello {
                    protocol_version: bcode_ipc::ProtocolVersion(
                        bcode_ipc::CURRENT_PROTOCOL_VERSION,
                    ),
                    client_id: bcode_session_models::ClientId::new(),
                    daemon: matching_daemon_status(),
                });
                bcode_ipc::send_envelope(
                    &mut stream,
                    &bcode_ipc::response_envelope(hello.request_id, &response)
                        .expect("hello envelope"),
                )
                .await
                .expect("hello reply");
                let request = bcode_ipc::recv_envelope(&mut stream)
                    .await
                    .expect("inspect request");
                let response = bcode_ipc::Response::Ok(
                    bcode_ipc::ResponsePayload::WorkflowDefinitionDescription {
                        definition: Some(definition),
                    },
                );
                bcode_ipc::send_envelope(
                    &mut stream,
                    &bcode_ipc::response_envelope(request.request_id, &response)
                        .expect("response envelope"),
                )
                .await
                .expect("reply");
            });
            let error = BcodeClient::new(endpoint)
                .inspect_workflow_definition("requested".to_string(), 1)
                .await
                .expect_err("reject untrusted content");
            let failure = bcode_workflow::WorkflowAuthoringFailure::StateUnavailable;
            assert!(
                matches!(error, ClientError::Server { code, message } if code == failure.code() && message == failure.to_string())
            );
            server.await.expect("server");
        }
    }

    fn matching_daemon_status() -> bcode_ipc::DaemonStatus {
        let (_path, digest) = bcode_daemon_lifecycle::current_executable_identity()
            .expect("current executable identity");
        bcode_ipc::DaemonStatus {
            namespace: bcode_ipc::daemon_namespace(),
            protocol_version: u32::from(bcode_ipc::CURRENT_PROTOCOL_VERSION),
            artifact_id: Some(bcode_ipc::ArtifactId::current()),
            build_fingerprint: bcode_ipc::BUILD_FINGERPRINT.to_owned(),
            executable_digest: Some(digest),
            storage_writer_epoch: Some(bcode_ipc::CURRENT_SESSION_STORAGE_WRITER_EPOCH),
            session_event_schema_version: Some(
                bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            ),
            state_location_id: Some(bcode_ipc::state_location_id()),
            ..bcode_ipc::DaemonStatus::default()
        }
    }

    #[test]
    fn daemon_identity_accepts_same_artifact_with_different_executable_digest() {
        let matching = matching_daemon_status();
        let resigned = bcode_ipc::DaemonStatus {
            executable_digest: Some("different-signed-executable-digest".to_owned()),
            ..matching
        };

        BcodeClient::verify_daemon_identity(&resigned)
            .expect("executable digest is diagnostic, not a compatibility boundary");
    }

    #[test]
    fn daemon_identity_matrix_rejects_every_incompatible_capability() {
        let matching = matching_daemon_status();
        BcodeClient::verify_daemon_identity(&matching).expect("matching daemon");

        let cases = [
            bcode_ipc::DaemonStatus {
                artifact_id: Some(
                    bcode_ipc::ArtifactId::parse("other-artifact")
                        .expect("other artifact identity"),
                ),
                ..matching.clone()
            },
            bcode_ipc::DaemonStatus {
                protocol_version: matching.protocol_version.saturating_add(1),
                ..matching.clone()
            },
            bcode_ipc::DaemonStatus {
                build_fingerprint: "other-build".to_owned(),
                ..matching.clone()
            },
            bcode_ipc::DaemonStatus {
                storage_writer_epoch: matching.storage_writer_epoch.map(|value| value + 1),
                ..matching.clone()
            },
            // A daemon serving a different state location must be refused: connecting
            // would let this client mutate another location's canonical session storage.
            bcode_ipc::DaemonStatus {
                state_location_id: Some("other-state-location".to_owned()),
                ..matching.clone()
            },
            // A daemon that advertises no state location is unverifiable, not assumed local.
            bcode_ipc::DaemonStatus {
                state_location_id: None,
                ..matching.clone()
            },
            bcode_ipc::DaemonStatus {
                session_event_schema_version: matching
                    .session_event_schema_version
                    .map(|value| value + 1),
                ..matching.clone()
            },
            bcode_ipc::DaemonStatus {
                storage_writer_epoch: None,
                session_event_schema_version: None,
                ..matching
            },
        ];
        for daemon in cases {
            let error = BcodeClient::verify_daemon_identity(&daemon)
                .expect_err("incompatible capability must fail before requests");
            let ClientError::IncompatibleDaemon { message } = error else {
                panic!("expected incompatible daemon");
            };
            assert!(message.contains("artifact="));
            assert!(message.contains("session_event_schema="));
            assert!(message.contains("storage_writer_epoch="));
            assert!(message.contains("protocol="));
            assert!(message.contains("build="));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn custom_endpoint_warm_handshake_uses_one_connection_without_startup() {
        let socket_dir =
            std::path::PathBuf::from(format!("/tmp/bcw-{}", SessionOpenOperationId::new()));
        std::fs::create_dir_all(&socket_dir).expect("socket directory");
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket_dir.join("warm.sock"));
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).expect("listener");
        let expected_client_id = bcode_session_models::ClientId::new();
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.expect("accept client");
            let hello = bcode_ipc::recv_envelope(&mut stream)
                .await
                .expect("receive hello");
            assert!(matches!(
                bcode_ipc::decode_request(&hello.payload).expect("decode hello"),
                bcode_ipc::Request::Hello { artifact_id: Some(artifact_id), .. }
                    if artifact_id == bcode_ipc::ArtifactId::current()
            ));
            let response = bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::Hello {
                protocol_version: bcode_ipc::ProtocolVersion::current(),
                client_id: expected_client_id,
                daemon: matching_daemon_status(),
            });
            let envelope =
                bcode_ipc::response_envelope(hello.request_id, &response).expect("hello response");
            bcode_ipc::send_envelope(&mut stream, &envelope)
                .await
                .expect("send hello");
        });
        let client = BcodeClient::new(endpoint);

        let connection = client
            .connect("custom-warm-test")
            .await
            .expect("warm connect");
        assert_eq!(connection.client_id(), Some(expected_client_id));

        server.await.expect("server task");
        std::fs::remove_dir_all(socket_dir).expect("socket cleanup");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn missing_custom_endpoint_requires_running_without_auto_start() {
        let socket_dir =
            std::path::PathBuf::from(format!("/tmp/bcm-{}", SessionOpenOperationId::new()));
        std::fs::create_dir_all(&socket_dir).expect("socket directory");
        let socket_path = socket_dir.join("missing.sock");
        let client = BcodeClient::new(bcode_ipc::IpcEndpoint::unix_socket(socket_path.clone()));

        let error = client
            .connect("custom-missing-test")
            .await
            .expect_err("custom endpoint must require a running daemon");
        assert!(error.is_daemon_unavailable());
        assert!(!socket_path.exists());

        std::fs::remove_dir_all(socket_dir).expect("socket cleanup");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn connection_timeout_is_distinct_and_does_not_trigger_auto_start() {
        let socket_dir =
            std::path::PathBuf::from(format!("/tmp/bcc-{}", SessionOpenOperationId::new()));
        std::fs::create_dir_all(&socket_dir).expect("socket directory");
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket_dir.join("connect-timeout.sock"));
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).expect("listener");
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.expect("accept client");
            let _hello = bcode_ipc::recv_envelope(&mut stream)
                .await
                .expect("receive hello");
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        let client = BcodeClient::new(endpoint)
            .with_daemon_availability(super::DaemonAvailability::AutoStart)
            .with_connect_timeout(Duration::from_millis(10))
            .with_request_timeout(Duration::from_secs(1));

        let error = client
            .connect("connect-timeout-test")
            .await
            .expect_err("unresponsive reachable endpoint must time out");
        assert!(matches!(
            error,
            ClientError::ConnectTimeout { timeout } if timeout == Duration::from_millis(10)
        ));
        assert!(!error.is_daemon_unavailable());

        server.await.expect("server task");
        std::fs::remove_dir_all(socket_dir).expect("socket cleanup");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn mismatched_artifact_hello_is_rejected_explicitly() {
        let socket_dir =
            std::path::PathBuf::from(format!("/tmp/bci-{}", SessionOpenOperationId::new()));
        std::fs::create_dir_all(&socket_dir).expect("socket directory");
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket_dir.join("artifact.sock"));
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).expect("listener");
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.expect("accept client");
            let hello = bcode_ipc::recv_envelope(&mut stream)
                .await
                .expect("hello request");
            let daemon = bcode_ipc::DaemonStatus {
                artifact_id: Some(
                    bcode_ipc::ArtifactId::parse("foreign-artifact")
                        .expect("foreign artifact identity"),
                ),
                ..matching_daemon_status()
            };
            let response = bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::Hello {
                protocol_version: bcode_ipc::ProtocolVersion(bcode_ipc::CURRENT_PROTOCOL_VERSION),
                client_id: bcode_session_models::ClientId::new(),
                daemon,
            });
            let envelope = bcode_ipc::response_envelope(hello.request_id, &response)
                .expect("hello response envelope");
            bcode_ipc::send_envelope(&mut stream, &envelope)
                .await
                .expect("send hello response");
        });

        let client = BcodeClient::new(endpoint)
            .with_daemon_availability(super::DaemonAvailability::RequireRunning);
        let error = client
            .connect("artifact-mismatch-test")
            .await
            .expect_err("foreign artifact must be rejected");
        assert!(matches!(error, ClientError::IncompatibleDaemon { .. }));
        assert!(error.to_string().contains("foreign-artifact"));

        server.await.expect("server task");
        std::fs::remove_dir_all(socket_dir).expect("socket cleanup");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn accepted_compaction_outlives_request_deadline() {
        let socket_dir =
            std::path::PathBuf::from(format!("/tmp/bcc-{}", SessionOpenOperationId::new()));
        std::fs::create_dir_all(&socket_dir).expect("socket directory");
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket_dir.join("client.sock"));
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).expect("listener");
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.expect("accept client");
            let request = bcode_ipc::recv_envelope(&mut stream)
                .await
                .expect("request");
            for payload in [
                bcode_ipc::ResponsePayload::SessionCompactionAccepted,
                bcode_ipc::ResponsePayload::SessionCompacted {
                    compacted: true,
                    message: "compacted".to_owned(),
                },
            ] {
                let terminal =
                    matches!(payload, bcode_ipc::ResponsePayload::SessionCompacted { .. });
                if terminal {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                }
                let envelope = bcode_ipc::response_envelope(
                    request.request_id,
                    &bcode_ipc::Response::Ok(payload),
                )
                .expect("response");
                bcode_ipc::send_envelope(&mut stream, &envelope)
                    .await
                    .expect("send");
            }
        });
        let stream = bcode_ipc::LocalIpcStream::connect(&endpoint)
            .await
            .expect("connect");
        let mut connection = super::ClientConnection {
            stream,
            next_request_id: 1,
            client_id: None,
            pending_events: std::collections::VecDeque::new(),
            request_timeout: Duration::from_millis(50),
            reconnect_client: None,
            reconnect_name: std::sync::Arc::from(""),
            restore_state: super::ConnectionRestoreState::default(),
        };
        assert!(matches!(
            connection
                .send_request(bcode_ipc::Request::CompactSession {
                    session_id: SessionId::new(),
                })
                .await,
            Ok(bcode_ipc::ResponsePayload::SessionCompacted {
                compacted: true,
                ..
            })
        ));
        server.await.expect("server task");
        std::fs::remove_dir_all(socket_dir).expect("socket cleanup");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unrelated_events_remain_buffered_in_fifo_order_during_requests() {
        let socket_dir =
            std::path::PathBuf::from(format!("/tmp/bce-{}", SessionOpenOperationId::new()));
        std::fs::create_dir_all(&socket_dir).expect("socket directory");
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket_dir.join("client.sock"));
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).expect("listener");
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.expect("accept client");
            let request = bcode_ipc::recv_envelope(&mut stream)
                .await
                .expect("request envelope");
            for revision in [11, 12] {
                let event = bcode_ipc::Event::SessionCatalogUpdated { revision };
                let envelope = bcode_ipc::event_envelope(&event).expect("event envelope");
                bcode_ipc::send_envelope(&mut stream, &envelope)
                    .await
                    .expect("send event");
            }
            let response = bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::Pong);
            let envelope = bcode_ipc::response_envelope(request.request_id, &response)
                .expect("response envelope");
            bcode_ipc::send_envelope(&mut stream, &envelope)
                .await
                .expect("send response");
        });
        let stream = bcode_ipc::LocalIpcStream::connect(&endpoint)
            .await
            .expect("connect");
        let mut connection = super::ClientConnection {
            stream,
            next_request_id: 1,
            client_id: None,
            pending_events: std::collections::VecDeque::new(),
            request_timeout: Duration::from_secs(1),
            reconnect_client: None,
            reconnect_name: std::sync::Arc::from(""),
            restore_state: super::ConnectionRestoreState::default(),
        };

        assert!(matches!(
            connection.send_request(bcode_ipc::Request::Ping).await,
            Ok(bcode_ipc::ResponsePayload::Pong)
        ));
        for expected in [11, 12] {
            assert_eq!(
                connection.recv_event().await.expect("buffered event"),
                bcode_ipc::Event::SessionCatalogUpdated { revision: expected }
            );
        }
        server.await.expect("server task");
        std::fs::remove_dir_all(socket_dir).expect("event socket cleanup");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stateless_catalog_requests_preserve_directory_and_sources() {
        let socket_dir =
            std::path::PathBuf::from(format!("/tmp/bcs-{}", SessionOpenOperationId::new()));
        std::fs::create_dir_all(&socket_dir).unwrap();
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket_dir.join("catalog.sock"));
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
        let expected = std::env::current_dir().unwrap().join("relative-workspace");
        let server = tokio::spawn(async move {
            for refresh in [false, true] {
                let mut stream = listener.accept().await.unwrap();
                let hello = bcode_ipc::recv_envelope(&mut stream).await.unwrap();
                let response = bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::Hello {
                    protocol_version: bcode_ipc::ProtocolVersion(
                        bcode_ipc::CURRENT_PROTOCOL_VERSION,
                    ),
                    client_id: bcode_session_models::ClientId::new(),
                    daemon: matching_daemon_status(),
                });
                bcode_ipc::send_envelope(
                    &mut stream,
                    &bcode_ipc::response_envelope(hello.request_id, &response).unwrap(),
                )
                .await
                .unwrap();
                let request = bcode_ipc::recv_envelope(&mut stream).await.unwrap();
                let decoded = bcode_ipc::decode_request(&request.payload).unwrap();
                let payload = if refresh {
                    match decoded {
                        bcode_ipc::Request::RefreshSessionCatalog {
                            working_directory,
                            sources,
                        } => {
                            assert_eq!(working_directory.as_ref(), Some(&expected));
                            assert_eq!(sources, Some(vec!["local".to_owned()]));
                        }
                        other => panic!("unexpected refresh request: {other:?}"),
                    }
                    bcode_ipc::ResponsePayload::SessionCatalogRefreshed {
                        sessions: vec![],
                        catalog_status: bcode_session_models::SessionCatalogStatus::Loaded,
                        catalog_sources: vec![],
                        catalog_revision: 2,
                    }
                } else {
                    assert!(
                        matches!(decoded, bcode_ipc::Request::ListSessions { working_directory } if working_directory == expected)
                    );
                    bcode_ipc::ResponsePayload::SessionList {
                        sessions: vec![],
                        catalog_status: bcode_session_models::SessionCatalogStatus::Loaded,
                        catalog_sources: vec![],
                        catalog_revision: 1,
                    }
                };
                let response = bcode_ipc::Response::Ok(payload);
                bcode_ipc::send_envelope(
                    &mut stream,
                    &bcode_ipc::response_envelope(request.request_id, &response).unwrap(),
                )
                .await
                .unwrap();
            }
        });
        let client = BcodeClient::new(endpoint).with_request_timeout(Duration::from_secs(5));
        let listed = client
            .list_sessions_in_working_directory("relative-workspace".into())
            .await
            .unwrap();
        assert_eq!(listed.catalog_revision, 1);
        let refreshed = client
            .refresh_session_catalog_in_working_directory(
                "relative-workspace".into(),
                Some(vec!["local".to_owned()]),
            )
            .await
            .unwrap();
        assert_eq!(refreshed.catalog_revision, 2);
        server.await.unwrap();
        std::fs::remove_dir_all(socket_dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn catalog_relative_directory_is_resolved_before_transport() {
        let socket_dir =
            std::path::PathBuf::from(format!("/tmp/bcd-{}", SessionOpenOperationId::new()));
        std::fs::create_dir_all(&socket_dir).unwrap();
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket_dir.join("catalog.sock"));
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
        let expected = std::env::current_dir().unwrap().join("relative-workspace");
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.unwrap();
            let request = bcode_ipc::recv_envelope(&mut stream).await.unwrap();
            let decoded = bcode_ipc::decode_request(&request.payload).unwrap();
            assert!(
                matches!(decoded, bcode_ipc::Request::ListSessions { working_directory } if working_directory == expected)
            );
            let response = bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::SessionList {
                sessions: vec![],
                catalog_status: bcode_session_models::SessionCatalogStatus::Loaded,
                catalog_sources: vec![],
                catalog_revision: 1,
            });
            bcode_ipc::send_envelope(
                &mut stream,
                &bcode_ipc::response_envelope(request.request_id, &response).unwrap(),
            )
            .await
            .unwrap();
        });
        let stream = bcode_ipc::LocalIpcStream::connect(&endpoint).await.unwrap();
        let mut connection = super::ClientConnection {
            stream,
            next_request_id: 1,
            client_id: None,
            pending_events: std::collections::VecDeque::new(),
            request_timeout: Duration::from_secs(5),
            reconnect_client: None,
            reconnect_name: std::sync::Arc::from(""),
            restore_state: super::ConnectionRestoreState::default(),
        };
        let result = connection
            .list_sessions_in_working_directory("relative-workspace".into())
            .await
            .unwrap();
        assert_eq!(result.catalog_revision, 1);
        server.await.unwrap();
        std::fs::remove_dir_all(socket_dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn long_poll_transport_timeout_is_distinct_from_operation_failure() {
        let socket_dir =
            std::path::PathBuf::from(format!("/tmp/bct-{}", SessionOpenOperationId::new()));
        std::fs::create_dir_all(&socket_dir).expect("socket directory");
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket_dir.join("timeout.sock"));
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).expect("listener");
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.expect("accept client");
            let _request = bcode_ipc::recv_envelope(&mut stream)
                .await
                .expect("wait request");
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        let stream = bcode_ipc::LocalIpcStream::connect(&endpoint)
            .await
            .expect("connect");
        let mut connection = super::ClientConnection {
            stream,
            next_request_id: 1,
            client_id: None,
            pending_events: std::collections::VecDeque::new(),
            request_timeout: Duration::from_millis(10),
            reconnect_client: None,
            reconnect_name: std::sync::Arc::from(""),
            restore_state: super::ConnectionRestoreState::default(),
        };
        let session_id = SessionId::new();

        assert!(matches!(
            connection
                .wait_session_open_progress(
                    session_id,
                    SessionOpenOperationId::new(),
                    0,
                    Duration::from_secs(5),
                )
                .await,
            Err(ClientError::RequestTimeout { timeout })
                if timeout == Duration::from_millis(10)
        ));
        server.await.expect("server task");
        std::fs::remove_dir_all(socket_dir).expect("timeout socket cleanup");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn artifact_client_rejects_invalid_lengths_before_connecting() {
        let socket_dir =
            std::path::PathBuf::from(format!("/tmp/bca-{}", SessionOpenOperationId::new()));
        std::fs::create_dir_all(&socket_dir).unwrap();
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket_dir.join("bounds.sock"));
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
        let client = BcodeClient::new(endpoint).with_request_timeout(Duration::from_millis(50));
        for length in [
            0,
            bcode_session_models::MAX_SESSION_ARTIFACT_RANGE_BYTES + 1,
            u32::MAX,
        ] {
            let error = client
                .session_artifact_range(
                    SessionId::new(),
                    "private-artifact".into(),
                    "private-reference".into(),
                    0,
                    length,
                )
                .await
                .unwrap_err();
            assert!(matches!(error, ClientError::Protocol(ref message)
                if message == &format!("artifact range length must be between 1 and {} bytes", bcode_session_models::MAX_SESSION_ARTIFACT_RANGE_BYTES)));
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
        drop(listener);
        std::fs::remove_dir_all(socket_dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn artifact_client_validates_binary_and_eof_ranges_over_ipc() {
        for (offset, bytes, valid) in [
            (0, vec![], false),
            (0, vec![0, 255], true),
            (2, vec![], true),
            (3, vec![], false),
            (u64::MAX, vec![], false),
        ] {
            let expected_bytes = bytes.clone();
            let socket_dir =
                std::path::PathBuf::from(format!("/tmp/bca-{}", SessionOpenOperationId::new()));
            std::fs::create_dir_all(&socket_dir).expect("socket directory");
            let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket_dir.join("range.sock"));
            let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).expect("listener");
            let server = tokio::spawn(async move {
                let mut stream = listener.accept().await.expect("accept client");
                let hello = bcode_ipc::recv_envelope(&mut stream).await.expect("hello");
                let response = bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::Hello {
                    protocol_version: bcode_ipc::ProtocolVersion(
                        bcode_ipc::CURRENT_PROTOCOL_VERSION,
                    ),
                    client_id: bcode_session_models::ClientId::new(),
                    daemon: matching_daemon_status(),
                });
                bcode_ipc::send_envelope(
                    &mut stream,
                    &bcode_ipc::response_envelope(hello.request_id, &response)
                        .expect("hello envelope"),
                )
                .await
                .expect("send hello");
                let request = bcode_ipc::recv_envelope(&mut stream)
                    .await
                    .expect("range request");
                let response =
                    bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::SessionArtifactRange {
                        range: super::SessionArtifactRange {
                            artifact_id: "artifact".to_owned(),
                            reference_key: "reference".to_owned(),
                            content_type: None,
                            offset,
                            total_bytes: 2,
                            reference_bytes: Some(2),
                            reference_revision: 1,
                            finalized: true,
                            finalized_event_seq: Some(1),
                            availability: None,
                            complete: Some(true),
                            checksum_sha256: None,
                            bytes,
                        },
                    });
                bcode_ipc::send_envelope(
                    &mut stream,
                    &bcode_ipc::response_envelope(request.request_id, &response)
                        .expect("range envelope"),
                )
                .await
                .expect("send range");
            });
            let client = BcodeClient::new(endpoint);
            let result = client
                .session_artifact_range(
                    SessionId::new(),
                    "artifact".to_owned(),
                    "reference".to_owned(),
                    offset,
                    2,
                )
                .await;
            if valid {
                let range = result.expect("valid range");
                assert_eq!(range.bytes, expected_bytes);
                assert_eq!(range.offset, offset);
                assert_eq!(range.next_offset(), 2);
                assert!(range.is_eof());
                assert_eq!(range.finalized_event_seq, Some(1));
            } else {
                assert!(matches!(result, Err(ClientError::Protocol(_))));
            }
            server.await.expect("server task");
            std::fs::remove_dir_all(socket_dir).expect("socket cleanup");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn preparation_recovers_retained_operation_after_transport_interruption() {
        let socket_dir =
            std::path::PathBuf::from(format!("/tmp/bcr-{}", SessionOpenOperationId::new()));
        std::fs::create_dir_all(&socket_dir).expect("socket directory");
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket_dir.join("reconnect.sock"));
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).expect("listener");
        let session_id = SessionId::new();
        let operation_id = SessionOpenOperationId::new();
        let snapshot = |revision, terminal| SessionOpenOperationSnapshot {
            operation_id,
            revision,
            session_id,
            source_writer_epoch: Some(3),
            target_writer_epoch: 4,
            progress: SessionMigrationProgress {
                stage: if terminal {
                    SessionMigrationStage::Complete
                } else {
                    SessionMigrationStage::CopyingBackup
                },
                completed_units: Some(revision),
                total_units: Some(2),
                unit: Some(bcode_session_models::SessionMigrationProgressUnit::Files),
                message: "migration".to_owned(),
            },
            outcome: terminal.then_some(SessionOpenTerminalOutcome::Ready),
            backup_path: None,
        };
        let initial = snapshot(1, false);
        let terminal = snapshot(2, true);
        let server_terminal = terminal.clone();
        let daemon = matching_daemon_status();
        let server = tokio::spawn(async move {
            for (connection_index, prepared) in [initial, server_terminal].into_iter().enumerate() {
                let mut stream = listener.accept().await.expect("accept client");
                let hello = bcode_ipc::recv_envelope(&mut stream).await.expect("hello");
                let response = bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::Hello {
                    protocol_version: bcode_ipc::ProtocolVersion(
                        bcode_ipc::CURRENT_PROTOCOL_VERSION,
                    ),
                    client_id: bcode_session_models::ClientId::new(),
                    daemon: daemon.clone(),
                });
                let envelope = bcode_ipc::response_envelope(hello.request_id, &response)
                    .expect("hello response");
                bcode_ipc::send_envelope(&mut stream, &envelope)
                    .await
                    .expect("send hello");

                let request = bcode_ipc::recv_envelope(&mut stream)
                    .await
                    .expect("preparation request");
                let response =
                    bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::SessionOpenPrepared {
                        snapshot: prepared,
                    });
                let envelope = bcode_ipc::response_envelope(request.request_id, &response)
                    .expect("preparation response");
                bcode_ipc::send_envelope(&mut stream, &envelope)
                    .await
                    .expect("send preparation");
                if connection_index == 0 {
                    let _wait = bcode_ipc::recv_envelope(&mut stream)
                        .await
                        .expect("wait request before disconnect");
                }
            }
        });
        let client = BcodeClient::new(endpoint).with_request_timeout(Duration::from_secs(1));
        let mut connection = client.connect("reconnect-test").await.expect("connect");
        let mut revisions = Vec::new();

        let recovered = connection
            .prepare_session_open_until_terminal(session_id, |snapshot| {
                revisions.push(snapshot.revision);
            })
            .await
            .expect("recover preparation");

        assert_eq!(recovered, terminal);
        assert_eq!(revisions, vec![1, 2]);
        server.await.expect("server task");
        std::fs::remove_dir_all(socket_dir).expect("socket cleanup");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_progress_receiver_stops_client_observation_cleanly() {
        let socket_dir =
            std::path::PathBuf::from(format!("/tmp/bcd-{}", SessionOpenOperationId::new()));
        std::fs::create_dir_all(&socket_dir).expect("socket directory");
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket_dir.join("drop.sock"));
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).expect("listener");
        let session_id = SessionId::new();
        let snapshot = SessionOpenOperationSnapshot {
            operation_id: SessionOpenOperationId::new(),
            revision: 1,
            session_id,
            source_writer_epoch: Some(3),
            target_writer_epoch: 4,
            progress: SessionMigrationProgress {
                stage: SessionMigrationStage::CopyingBackup,
                completed_units: Some(1),
                total_units: Some(2),
                unit: Some(bcode_session_models::SessionMigrationProgressUnit::Files),
                message: "migration".to_owned(),
            },
            outcome: None,
            backup_path: None,
        };
        let daemon = matching_daemon_status();
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.expect("accept client");
            let hello = bcode_ipc::recv_envelope(&mut stream).await.expect("hello");
            let response = bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::Hello {
                protocol_version: bcode_ipc::ProtocolVersion(bcode_ipc::CURRENT_PROTOCOL_VERSION),
                client_id: bcode_session_models::ClientId::new(),
                daemon,
            });
            let envelope =
                bcode_ipc::response_envelope(hello.request_id, &response).expect("hello response");
            bcode_ipc::send_envelope(&mut stream, &envelope)
                .await
                .expect("send hello");
            let request = bcode_ipc::recv_envelope(&mut stream)
                .await
                .expect("prepare request");
            let response =
                bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::SessionOpenPrepared {
                    snapshot,
                });
            let envelope = bcode_ipc::response_envelope(request.request_id, &response)
                .expect("prepare response");
            bcode_ipc::send_envelope(&mut stream, &envelope)
                .await
                .expect("send prepare");
            let first = tokio::time::timeout(
                Duration::from_millis(250),
                bcode_ipc::recv_envelope(&mut stream),
            )
            .await;
            if first.as_ref().is_ok_and(Result::is_ok) {
                tokio::time::timeout(
                    Duration::from_millis(250),
                    bcode_ipc::recv_envelope(&mut stream),
                )
                .await
            } else {
                first
            }
        });
        let client = BcodeClient::new(endpoint).with_request_timeout(Duration::from_secs(1));
        let mut observer = client.observe_session_open(session_id);
        let first = observer.receiver.recv().await.expect("initial progress");
        assert_eq!(first.revision, 1);
        drop(observer.receiver);
        assert!(observer.task.await.expect("observer task").is_ok());
        let next_request = server.await.expect("server task");
        assert!(
            next_request.is_ok_and(|request| request.is_err()),
            "observer sent another wait request after receiver drop"
        );
        std::fs::remove_dir_all(socket_dir).expect("socket cleanup");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn application_request_is_not_replayed_after_response_eof() {
        let socket_dir =
            std::path::PathBuf::from(format!("/tmp/bcd-{}", SessionOpenOperationId::new()));
        std::fs::create_dir_all(&socket_dir).expect("socket directory");
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket_dir.join("single-send.sock"));
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).expect("listener");
        let daemon = matching_daemon_status();
        let session_id = SessionId::new();
        let input = bcode_tool::ToolInvocationInput {
            invocation_id: "call".into(),
            input_id: "input".into(),
            producer_id: "plugin".into(),
            schema: "plugin.input".into(),
            schema_version: 1,
            payload: serde_json::json!({"opaque": "λ"}),
        };
        let expected_input = input.clone();
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.expect("accept client");
            let hello = bcode_ipc::recv_envelope(&mut stream).await.expect("hello");
            let response = bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::Hello {
                protocol_version: bcode_ipc::ProtocolVersion(bcode_ipc::CURRENT_PROTOCOL_VERSION),
                client_id: bcode_session_models::ClientId::new(),
                daemon,
            });
            let envelope =
                bcode_ipc::response_envelope(hello.request_id, &response).expect("hello response");
            bcode_ipc::send_envelope(&mut stream, &envelope)
                .await
                .expect("send hello");
            let request = bcode_ipc::recv_envelope(&mut stream)
                .await
                .expect("application request");
            assert!(matches!(
                bcode_ipc::decode_request(&request.payload).expect("decode request"),
                bcode_ipc::Request::InvocationInput { session_id: actual, input }
                    if actual == session_id && input == expected_input
            ));
            drop(stream);
            assert!(
                tokio::time::timeout(Duration::from_millis(250), listener.accept())
                    .await
                    .is_err(),
                "uncertain input must not be replayed on a second connection"
            );
        });

        let client = BcodeClient::new(endpoint)
            .with_daemon_availability(super::DaemonAvailability::AutoStart)
            .with_request_timeout(Duration::from_secs(1));
        let error = client
            .send_invocation_input(session_id, input)
            .await
            .expect_err("response EOF must fail");
        assert!(matches!(error, ClientError::Codec(_)));
        server.await.expect("server task");
        std::fs::remove_dir_all(socket_dir).expect("socket cleanup");
    }

    #[test]
    fn only_ready_terminal_outcome_allows_writable_attach() {
        let session_id = SessionId::new();
        let snapshot = |outcome| SessionOpenOperationSnapshot {
            operation_id: SessionOpenOperationId::new(),
            revision: 1,
            session_id,
            source_writer_epoch: Some(3),
            target_writer_epoch: 4,
            progress: SessionMigrationProgress {
                stage: SessionMigrationStage::Failed,
                completed_units: None,
                total_units: None,
                unit: None,
                message: "Classifying session".to_owned(),
            },
            outcome: Some(outcome),
            backup_path: Some("/tmp/backup".into()),
        };

        assert!(
            session_open_attach_readiness(&snapshot(SessionOpenTerminalOutcome::Ready)).is_ok()
        );
        for (outcome, expected_code) in [
            (
                SessionOpenTerminalOutcome::DegradedReadOnly { issue_count: 1 },
                "session_degraded_read_only",
            ),
            (
                SessionOpenTerminalOutcome::WriterIncompatible {
                    actual: Some(5),
                    expected: 4,
                },
                "session_writer_incompatible",
            ),
            (
                SessionOpenTerminalOutcome::RepairRequired {
                    reason: "damaged tail".to_owned(),
                },
                "session_repair_required",
            ),
            (
                SessionOpenTerminalOutcome::Failed {
                    kind: bcode_session_models::SessionOpenFailureKind::BackupFailed,
                    message: "backup failed".to_owned(),
                    backup_path: Some("/tmp/failed-backup".into()),
                },
                "session_migration_backup_failed",
            ),
        ] {
            assert!(matches!(
                session_open_attach_readiness(&snapshot(outcome)),
                Err(ClientError::Server { code, .. }) if code == expected_code
            ));
        }
    }

    #[test]
    fn terminal_session_open_error_preserves_recovery_context() {
        let session_id = SessionId::new();
        let message = terminal_session_open_error_message(
            session_id,
            "Verifying retained backup",
            "hash mismatch",
            Some(std::path::Path::new("/tmp/session-backup")),
        );

        assert!(message.contains("Verifying retained backup"));
        assert!(message.contains("hash mismatch"));
        assert!(message.contains("/tmp/session-backup"));
        assert!(message.contains(&format!("bcode session diagnose {session_id}")));
    }

    #[test]
    fn caller_paths_are_absolute_and_relative_paths_use_the_caller_cwd() {
        let caller_cwd = Path::new("/tmp/bcode-client-cwd");

        assert_eq!(
            resolve_path_from(None, caller_cwd),
            caller_cwd.to_path_buf()
        );
        assert_eq!(
            resolve_path_from(Some("nested".into()), caller_cwd),
            caller_cwd.join("nested")
        );
        assert_eq!(
            resolve_path_from(Some("/tmp/explicit".into()), caller_cwd),
            Path::new("/tmp/explicit")
        );
    }

    #[test]
    fn default_endpoint_honors_process_config_override() {
        let guard = bcode_config::push_process_config_overrides(
            bcode_config::ConfigLoadOverrides::from_env_with_cli(
                None,
                Some("[client]\nrequest_timeout_secs = 23\n".to_owned()),
            ),
        );

        let client = BcodeClient::default_endpoint();

        assert_eq!(client.request_timeout(), Duration::from_secs(23));
        drop(guard);
    }

    #[test]
    fn bounded_long_poll_timeouts_use_server_bound_plus_transport_grace() {
        let client =
            BcodeClient::default_endpoint().with_request_timeout(Duration::from_millis(10));
        let response_timeout = client
            .request_timeout()
            .max(Duration::from_secs(30).saturating_add(super::LONG_POLL_TRANSPORT_GRACE));

        assert_eq!(response_timeout, Duration::from_secs(35));
        assert!(super::validate_session_search_backfill_wait_timeout(1).is_ok());
        assert!(super::validate_session_search_backfill_wait_timeout(30_000).is_ok());
        assert!(matches!(
            super::validate_session_search_backfill_wait_timeout(30_001),
            Err(ClientError::Protocol(_))
        ));
        assert!(super::validate_session_bulk_migration_wait_timeout(1).is_ok());
        assert!(super::validate_session_bulk_migration_wait_timeout(30_000).is_ok());
        assert!(matches!(
            super::validate_session_bulk_migration_wait_timeout(30_001),
            Err(ClientError::Protocol(_))
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn backfill_wait_longer_than_default_request_timeout_receives_quiet_response() {
        let socket_dir =
            std::path::PathBuf::from(format!("/tmp/bcl-{}", SessionOpenOperationId::new()));
        std::fs::create_dir_all(&socket_dir).expect("socket directory");
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket_dir.join("long-poll.sock"));
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).expect("listener");
        let operation_id = "quiet-backfill".to_owned();
        let expected_operation_id = operation_id.clone();
        let daemon = matching_daemon_status();
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.expect("accept client");
            let hello = bcode_ipc::recv_envelope(&mut stream).await.expect("hello");
            let response = bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::Hello {
                protocol_version: bcode_ipc::ProtocolVersion::current(),
                client_id: bcode_session_models::ClientId::new(),
                daemon,
            });
            let envelope =
                bcode_ipc::response_envelope(hello.request_id, &response).expect("hello response");
            bcode_ipc::send_envelope(&mut stream, &envelope)
                .await
                .expect("send hello");

            let request = bcode_ipc::recv_envelope(&mut stream)
                .await
                .expect("wait request");
            assert!(matches!(
                bcode_ipc::decode_request(&request.payload).expect("decode wait request"),
                bcode_ipc::Request::SessionSearchBackfillWait {
                    operation_id,
                    after_revision: 1,
                    timeout_ms: 80,
                } if operation_id == expected_operation_id
            ));
            tokio::time::sleep(Duration::from_millis(40)).await;
            let response = bcode_ipc::Response::Ok(
                bcode_ipc::ResponsePayload::SessionSearchBackfillOperation {
                    status: SessionSearchBackfillOperationStatus {
                        operation_id: expected_operation_id,
                        provider_id: "bcode.test-search".to_owned(),
                        revision: 1,
                        state: SessionSearchBackfillOperationState::Running,
                        response: None,
                        complete_progress: None,
                        complete_response: None,
                        error: None,
                    },
                },
            );
            let envelope =
                bcode_ipc::response_envelope(request.request_id, &response).expect("wait response");
            bcode_ipc::send_envelope(&mut stream, &envelope)
                .await
                .expect("send wait response");
        });
        let client = BcodeClient::new(endpoint)
            .with_daemon_availability(super::DaemonAvailability::RequireRunning)
            .with_request_timeout(Duration::from_millis(10));

        let status = client
            .session_search_backfill_wait(operation_id, 1, 80)
            .await
            .expect("long poll must outlive generic request timeout");

        assert_eq!(status.revision, 1);
        assert_eq!(status.state, SessionSearchBackfillOperationState::Running);
        server.await.expect("server task");
        std::fs::remove_dir_all(socket_dir).expect("socket cleanup");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bulk_migration_wait_longer_than_default_request_timeout_receives_quiet_response() {
        let socket_dir =
            std::path::PathBuf::from(format!("/tmp/bcmw-{}", SessionOpenOperationId::new()));
        std::fs::create_dir_all(&socket_dir).expect("socket directory");
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket_dir.join("migration-wait.sock"));
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).expect("listener");
        let operation_id = "quiet-migration".to_owned();
        let expected_operation_id = operation_id.clone();
        let daemon = matching_daemon_status();
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.expect("accept client");
            let hello = bcode_ipc::recv_envelope(&mut stream).await.expect("hello");
            let response = bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::Hello {
                protocol_version: bcode_ipc::ProtocolVersion::current(),
                client_id: bcode_session_models::ClientId::new(),
                daemon,
            });
            let envelope =
                bcode_ipc::response_envelope(hello.request_id, &response).expect("hello response");
            bcode_ipc::send_envelope(&mut stream, &envelope)
                .await
                .expect("send hello");

            let request = bcode_ipc::recv_envelope(&mut stream)
                .await
                .expect("wait request");
            assert!(matches!(
                bcode_ipc::decode_request(&request.payload).expect("decode wait request"),
                bcode_ipc::Request::SessionBulkMigrationWait {
                    operation_id,
                    after_revision: 2,
                    timeout_ms: 80,
                } if operation_id == expected_operation_id
            ));
            tokio::time::sleep(Duration::from_millis(40)).await;
            let response = bcode_ipc::Response::Ok(
                bcode_ipc::ResponsePayload::SessionBulkMigrationOperation {
                    status: bcode_ipc::SessionBulkMigrationOperationStatus {
                        operation_id: expected_operation_id,
                        revision: 2,
                        state: bcode_ipc::SessionBulkMigrationState::Running,
                        mode: bcode_ipc::SessionBulkMigrationMode::Inventory,
                        selected: 0,
                        visited: 0,
                        migrated: 0,
                        blocked: 0,
                        failed: 0,
                        current_session_id: None,
                        outcomes: Vec::new(),
                    },
                },
            );
            let envelope =
                bcode_ipc::response_envelope(request.request_id, &response).expect("wait response");
            bcode_ipc::send_envelope(&mut stream, &envelope)
                .await
                .expect("send wait response");
        });
        let client = BcodeClient::new(endpoint)
            .with_daemon_availability(super::DaemonAvailability::RequireRunning)
            .with_request_timeout(Duration::from_millis(10));

        let status = client
            .wait_session_bulk_migration(operation_id, 2, 80)
            .await
            .expect("long poll must outlive generic request timeout");

        assert_eq!(status.revision, 2);
        assert_eq!(status.state, bcode_ipc::SessionBulkMigrationState::Running);
        server.await.expect("server task");
        std::fs::remove_dir_all(socket_dir).expect("socket cleanup");
    }

    #[test]
    fn request_timeout_can_be_overridden() {
        let client = BcodeClient::default_endpoint().with_request_timeout(Duration::from_secs(17));

        assert_eq!(client.request_timeout(), Duration::from_secs(17));
    }
}
