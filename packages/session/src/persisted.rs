//! Durable JSON codec for the current session event schema.
//!
//! These persistence DTOs intentionally live in the session package instead of IPC because the
//! durable JSON and non-self-describing wire formats have different requirements. Historical
//! conversion and retired-event policy live exclusively in `bcode_session_migration`; this module
//! strictly accepts only the current durable event schema.

use bcode_session_models::{
    CURRENT_SESSION_EVENT_SCHEMA_VERSION, ClientId, ModelTurnOutcome, ProviderContextSnapshot,
    RequestContextObservation, RuntimeWorkKind, RuntimeWorkStatus, SessionEvent, SessionEventKind,
    SessionEventProvenance, SessionForkKind, SessionId, SessionTokenUsage, SessionTraceEvent,
    TurnAdmissionMetadata, WorkId, current_unix_timestamp_ms,
};
use bcode_skill_models::{SkillActivationMode, SkillId, SkillSource};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use thiserror::Error;

/// Decode a persisted session event from durable JSON.
///
/// # Errors
///
/// Returns an error when the event is not a supported persisted session-event
/// shape or cannot be converted into the current domain model.
pub fn decode_session_event(payload: &str) -> Result<SessionEvent, PersistedSessionEventError> {
    let value = serde_json::from_str::<serde_json::Value>(payload)?;
    reject_unsupported_future_shape(&value)?;
    let persisted = serde_json::from_value::<PersistedSessionEvent>(value)?;
    persisted.into_domain()
}

/// Encode a session event into the durable JSON persistence DTO shape.
///
/// # Errors
///
/// Returns an error when the event cannot be serialized as JSON.
pub fn encode_session_event(event: &SessionEvent) -> Result<String, serde_json::Error> {
    serde_json::to_string(&PersistedSessionEvent::from(event))
}

fn reject_unsupported_future_shape(
    value: &serde_json::Value,
) -> Result<(), PersistedSessionEventError> {
    if let Some(schema_version) = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
    {
        let schema_version = u16::try_from(schema_version).unwrap_or(u16::MAX);
        if schema_version != CURRENT_SESSION_EVENT_SCHEMA_VERSION {
            return Err(PersistedSessionEventError::UnsupportedSchemaVersion {
                actual: schema_version,
                current: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            });
        }
    }

    let Some(kind) = value.get("kind") else {
        return Ok(());
    };
    match serde_json::from_value::<PersistedSessionEventKind>(kind.clone()) {
        Err(error) if is_unknown_variant_error(&error) => {
            Err(PersistedSessionEventError::UnsupportedEventKind {
                kind: first_persisted_event_kind_name(kind),
            })
        }
        Ok(_) | Err(_) => Ok(()),
    }
}

fn is_unknown_variant_error(error: &serde_json::Error) -> bool {
    error.to_string().starts_with("unknown variant `")
}

fn first_persisted_event_kind_name(kind: &serde_json::Value) -> String {
    kind.as_object()
        .and_then(|object| object.keys().next().cloned())
        .unwrap_or_else(|| "<invalid>".to_string())
}

/// Errors returned when decoding persisted session events.
#[derive(Debug, Error)]
pub enum PersistedSessionEventError {
    /// Persisted JSON was malformed or incompatible with known DTOs.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// Persisted event uses a future schema version not supported by this build.
    #[error(
        "unsupported persisted session event schema version {actual}; current version is {current}"
    )]
    UnsupportedSchemaVersion { actual: u16, current: u16 },
    /// Persisted event uses an unknown future event kind not supported by this build.
    #[error("unsupported persisted session event kind {kind}")]
    UnsupportedEventKind { kind: String },
}

/// Persisted session event DTO.
#[derive(Debug, Serialize, Deserialize)]
struct PersistedSessionEvent {
    schema_version: u16,
    sequence: u64,
    #[serde(default = "current_unix_timestamp_ms")]
    timestamp_ms: u64,
    session_id: SessionId,
    #[serde(default)]
    provenance: Option<SessionEventProvenance>,
    kind: PersistedSessionEventKind,
}

impl From<&SessionEvent> for PersistedSessionEvent {
    fn from(value: &SessionEvent) -> Self {
        Self {
            schema_version: value.schema_version,
            sequence: value.sequence,
            timestamp_ms: value.timestamp_ms,
            session_id: value.session_id,
            provenance: value.provenance.clone(),
            kind: PersistedSessionEventKind::from(&value.kind),
        }
    }
}

impl PersistedSessionEvent {
    fn into_domain(self) -> Result<SessionEvent, PersistedSessionEventError> {
        if self.schema_version != CURRENT_SESSION_EVENT_SCHEMA_VERSION {
            return Err(PersistedSessionEventError::UnsupportedSchemaVersion {
                actual: self.schema_version,
                current: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            });
        }
        Ok(SessionEvent {
            schema_version: self.schema_version,
            sequence: self.sequence,
            timestamp_ms: self.timestamp_ms,
            session_id: self.session_id,
            provenance: self.provenance,
            kind: self.kind.into_domain(),
        })
    }
}

/// Persisted session event kind DTO.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PersistedSessionEventKind {
    SessionCreated {
        name: Option<String>,
        #[serde(default)]
        working_directory: PathBuf,
    },
    ClientAttached {
        client_id: ClientId,
    },
    ClientDetached {
        client_id: ClientId,
    },
    UserMessage {
        client_id: ClientId,
        text: String,
        #[serde(default)]
        admission: TurnAdmissionMetadata,
    },
    AssistantDelta {
        text: String,
    },
    AssistantMessage {
        text: String,
    },
    ToolCallRequested {
        tool_call_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        producer_plugin_id: Option<String>,
        tool_name: String,
        arguments_json: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        working_directory: Option<std::path::PathBuf>,
    },
    PermissionRequested {
        permission_id: String,
        tool_call_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        producer_plugin_id: Option<String>,
        tool_name: String,
        arguments_json: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        batch: Option<bcode_session_models::PermissionBatchCorrelation>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        policy_source: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        policy_reason: Option<String>,
    },
    PermissionResolved {
        permission_id: String,
        approved: bool,
    },
    ModelChanged {
        provider: String,
        model: String,
        #[serde(default)]
        selection_source: bcode_session_models::ModelSelectionSource,
    },
    SystemMessage {
        text: String,
    },
    AgentChanged {
        agent_id: String,
    },
    ModelTurnStarted {
        turn_id: String,
    },
    ModelTurnFinished {
        turn_id: String,
        outcome: ModelTurnOutcome,
        #[serde(default)]
        message: Option<String>,
    },
    ModelUsage {
        turn_id: String,
        usage: SessionTokenUsage,
    },
    ContextCompacted {
        summary: String,
        compacted_through_sequence: u64,
    },
    SessionRenamed {
        name: Option<String>,
    },
    TraceEvent {
        trace: Box<SessionTraceEvent>,
    },
    SkillInvoked {
        skill_id: SkillId,
        arguments: String,
        #[serde(default)]
        source: Option<SkillSource>,
        invoked_at_ms: u64,
    },
    SkillSuggested {
        skill_id: SkillId,
        #[serde(default)]
        reason: Option<String>,
        suggested_at_ms: u64,
    },
    SkillActivated {
        skill_id: SkillId,
        #[serde(default)]
        source: Option<SkillSource>,
        mode: SkillActivationMode,
        activated_at_ms: u64,
    },
    SkillDeactivated {
        skill_id: SkillId,
        deactivated_at_ms: u64,
    },
    SkillContextLoaded {
        skill_id: SkillId,
        bytes_loaded: usize,
        truncated: bool,
        loaded_at_ms: u64,
        #[serde(default)]
        source: Option<SkillSource>,
        #[serde(default)]
        preview: Option<String>,
    },
    SkillInvocationFailed {
        skill_id: SkillId,
        error: String,
        failed_at_ms: u64,
    },
    /// Provider-exposed reasoning text delta.
    AssistantReasoningDelta {
        text: String,
    },
    /// Completed provider-exposed reasoning text.
    AssistantReasoningMessage {
        text: String,
    },
    /// Durable runtime work start marker.
    RuntimeWorkStarted {
        work_id: WorkId,
        kind: RuntimeWorkKind,
        label: String,
        #[serde(default)]
        tool_call_id: Option<String>,
        #[serde(default)]
        plugin_id: Option<String>,
        #[serde(default)]
        service_interface: Option<String>,
        #[serde(default)]
        operation: Option<String>,
        #[serde(default)]
        parent_work_id: Option<WorkId>,
        #[serde(default)]
        started_at_ms: Option<u64>,
        #[serde(default)]
        cancellable: bool,
    },
    /// Durable runtime work cancellation request marker.
    RuntimeWorkCancelRequested {
        work_id: WorkId,
        #[serde(default)]
        requested_at_ms: Option<u64>,
        #[serde(default)]
        client_id: Option<ClientId>,
    },
    /// Durable runtime work finish marker.
    RuntimeWorkFinished {
        work_id: WorkId,
        status: RuntimeWorkStatus,
        #[serde(default)]
        finished_at_ms: Option<u64>,
        #[serde(default)]
        message: Option<String>,
    },
    /// Durable runtime work progress marker.
    RuntimeWorkProgress {
        work_id: WorkId,
        message: String,
        #[serde(default)]
        progress_at_ms: Option<u64>,
        #[serde(default)]
        completed_units: Option<u64>,
        #[serde(default)]
        total_units: Option<u64>,
    },
    /// Durable marker that a model turn cancellation was requested.
    ModelTurnCancelRequested {
        turn_id: String,
        #[serde(default)]
        requested_at_ms: Option<u64>,
        #[serde(default)]
        client_id: Option<ClientId>,
    },
    /// Durable marker that moves the session's canonical working directory.
    WorkingDirectoryChanged {
        old_working_directory: PathBuf,
        new_working_directory: PathBuf,
    },
    /// Durable provenance marker for sessions imported from external agents.
    SessionImported {
        source_id: String,
        source_display_name: String,
        external_session_id: String,
        imported_at_ms: u64,
    },
    /// Durable provenance marker for sessions forked or cloned from another session.
    SessionForked {
        source_session_id: SessionId,
        #[serde(default)]
        source_title: Option<String>,
        #[serde(default)]
        source_cutoff_sequence: Option<u64>,
        #[serde(default)]
        source_prompt_sequence: Option<u64>,
        forked_at_ms: u64,
        kind: SessionForkKind,
    },
    /// Durable marker for Ralph loop lifecycle events relevant to this session.
    RalphLifecycle {
        loop_name: String,
        state_dir: PathBuf,
        kind: String,
        message: String,
        occurred_at_ms: u64,
    },
    /// Durable session-specific model reasoning selection.
    ReasoningChanged {
        #[serde(default)]
        effort: Option<String>,
        #[serde(default)]
        summary: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_scope: Option<bcode_session_models::ModelScopeKey>,
    },
    ProviderContextCompacted {
        snapshot: ProviderContextSnapshot,
        compacted_through_sequence: u64,
    },
    RequestContextObserved {
        observation: RequestContextObservation,
    },
    PluginStatusNote {
        plugin_id: String,
        note_id: String,
        text: String,
        #[serde(default)]
        metadata: BTreeMap<String, serde_json::Value>,
    },
    InertHistory {
        event_type: String,
        payload: serde_json::Value,
    },
    ToolInvocationLifecycle {
        event: bcode_session_models::ToolInvocationLifecycleEvent,
    },
    ToolContribution {
        event: bcode_session_models::ToolContributionEvent,
    },
    ToolExchangeRequested {
        request: bcode_session_models::ToolExchangeRequest,
    },
    ToolExchangeResolved {
        event: bcode_session_models::ToolExchangeResolutionEvent,
    },
    ToolInvocationResultRecorded {
        record: bcode_session_models::ToolInvocationResultRecord,
    },
    ToolContributionPlaced {
        envelope: bcode_session_models::ToolContributionEnvelope,
    },
    ExecutionSessionCreated {
        provenance: Box<bcode_session_models::ExecutionSessionProvenance>,
        visibility: bcode_session_models::SessionVisibility,
    },
    AssistantReasoningActivity {
        turn_id: String,
        activity: bcode_session_models::ReasoningActivity,
    },
    AssistantResponseSegment {
        turn_id: String,
        segment_id: String,
        segment_order: u32,
        text: String,
    },
    PositionedAssistantResponseSegment {
        turn_id: String,
        output_position: bcode_session_models::TurnOutputPosition,
        segment_id: String,
        segment_order: u32,
        text: String,
    },
    PositionedAssistantReasoningActivity {
        turn_id: String,
        output_position: bcode_session_models::TurnOutputPosition,
        activity: bcode_session_models::ReasoningActivity,
    },
    PositionedToolCallRequested {
        turn_id: String,
        output_position: bcode_session_models::TurnOutputPosition,
        tool_call_id: String,
        producer_plugin_id: Option<String>,
        tool_name: String,
        arguments_json: String,
        working_directory: Option<PathBuf>,
    },
}

impl From<&SessionEventKind> for PersistedSessionEventKind {
    #[allow(clippy::too_many_lines)]
    fn from(value: &SessionEventKind) -> Self {
        match value {
            SessionEventKind::SessionCreated {
                name,
                working_directory,
            } => Self::SessionCreated {
                name: name.clone(),
                working_directory: working_directory.clone(),
            },
            SessionEventKind::ClientAttached { client_id } => Self::ClientAttached {
                client_id: *client_id,
            },
            SessionEventKind::ClientDetached { client_id } => Self::ClientDetached {
                client_id: *client_id,
            },
            SessionEventKind::UserMessage {
                client_id,
                text,
                admission,
            } => Self::UserMessage {
                client_id: *client_id,
                text: text.clone(),
                admission: admission.clone(),
            },
            SessionEventKind::AssistantDelta { text } => {
                Self::AssistantDelta { text: text.clone() }
            }
            SessionEventKind::AssistantMessage { text } => {
                Self::AssistantMessage { text: text.clone() }
            }
            SessionEventKind::ToolCallRequested {
                tool_call_id,
                producer_plugin_id,
                tool_name,
                arguments_json,
                working_directory,
            } => Self::ToolCallRequested {
                tool_call_id: tool_call_id.clone(),
                producer_plugin_id: producer_plugin_id.clone(),
                tool_name: tool_name.clone(),
                arguments_json: arguments_json.clone(),
                working_directory: working_directory.clone(),
            },
            SessionEventKind::PermissionRequested {
                permission_id,
                tool_call_id,
                producer_plugin_id,
                tool_name,
                arguments_json,
                batch,
                policy_source,
                policy_reason,
            } => Self::PermissionRequested {
                permission_id: permission_id.clone(),
                tool_call_id: tool_call_id.clone(),
                producer_plugin_id: producer_plugin_id.clone(),
                tool_name: tool_name.clone(),
                arguments_json: arguments_json.clone(),
                batch: batch.clone(),
                policy_source: policy_source.clone(),
                policy_reason: policy_reason.clone(),
            },
            SessionEventKind::PermissionResolved {
                permission_id,
                approved,
            } => Self::PermissionResolved {
                permission_id: permission_id.clone(),
                approved: *approved,
            },
            SessionEventKind::ModelChanged {
                provider,
                model,
                selection_source,
            } => Self::ModelChanged {
                provider: provider.clone(),
                model: model.clone(),
                selection_source: *selection_source,
            },
            SessionEventKind::SystemMessage { text } => Self::SystemMessage { text: text.clone() },
            SessionEventKind::AgentChanged { agent_id } => Self::AgentChanged {
                agent_id: agent_id.clone(),
            },
            SessionEventKind::ModelTurnStarted { turn_id } => Self::ModelTurnStarted {
                turn_id: turn_id.clone(),
            },
            SessionEventKind::ModelTurnFinished {
                turn_id,
                outcome,
                message,
            } => Self::ModelTurnFinished {
                turn_id: turn_id.clone(),
                outcome: *outcome,
                message: message.clone(),
            },
            SessionEventKind::ModelUsage { turn_id, usage } => Self::ModelUsage {
                turn_id: turn_id.clone(),
                usage: usage.clone(),
            },
            SessionEventKind::ContextCompacted {
                summary,
                compacted_through_sequence,
            } => Self::ContextCompacted {
                summary: summary.clone(),
                compacted_through_sequence: *compacted_through_sequence,
            },
            SessionEventKind::SessionRenamed { name } => {
                Self::SessionRenamed { name: name.clone() }
            }
            SessionEventKind::TraceEvent { trace } => Self::TraceEvent {
                trace: trace.clone(),
            },
            SessionEventKind::SkillInvoked {
                skill_id,
                arguments,
                source,
                invoked_at_ms,
            } => Self::SkillInvoked {
                skill_id: skill_id.clone(),
                arguments: arguments.clone(),
                source: source.clone(),
                invoked_at_ms: *invoked_at_ms,
            },
            SessionEventKind::SkillSuggested {
                skill_id,
                reason,
                suggested_at_ms,
            } => Self::SkillSuggested {
                skill_id: skill_id.clone(),
                reason: reason.clone(),
                suggested_at_ms: *suggested_at_ms,
            },
            SessionEventKind::SkillActivated {
                skill_id,
                source,
                mode,
                activated_at_ms,
            } => Self::SkillActivated {
                skill_id: skill_id.clone(),
                source: source.clone(),
                mode: *mode,
                activated_at_ms: *activated_at_ms,
            },
            SessionEventKind::SkillDeactivated {
                skill_id,
                deactivated_at_ms,
            } => Self::SkillDeactivated {
                skill_id: skill_id.clone(),
                deactivated_at_ms: *deactivated_at_ms,
            },
            SessionEventKind::SkillContextLoaded {
                skill_id,
                bytes_loaded,
                truncated,
                loaded_at_ms,
                source,
                preview,
            } => Self::SkillContextLoaded {
                skill_id: skill_id.clone(),
                bytes_loaded: *bytes_loaded,
                truncated: *truncated,
                loaded_at_ms: *loaded_at_ms,
                source: source.clone(),
                preview: preview.clone(),
            },
            SessionEventKind::SkillInvocationFailed {
                skill_id,
                error,
                failed_at_ms,
            } => Self::SkillInvocationFailed {
                skill_id: skill_id.clone(),
                error: error.clone(),
                failed_at_ms: *failed_at_ms,
            },
            SessionEventKind::AssistantReasoningDelta { text } => {
                Self::AssistantReasoningDelta { text: text.clone() }
            }
            SessionEventKind::AssistantReasoningMessage { text } => {
                Self::AssistantReasoningMessage { text: text.clone() }
            }
            SessionEventKind::RuntimeWorkStarted {
                work_id,
                kind,
                label,
                tool_call_id,
                plugin_id,
                service_interface,
                operation,
                parent_work_id,
                started_at_ms,
                cancellable,
            } => Self::RuntimeWorkStarted {
                work_id: work_id.clone(),
                kind: *kind,
                label: label.clone(),
                tool_call_id: tool_call_id.clone(),
                plugin_id: plugin_id.clone(),
                service_interface: service_interface.clone(),
                operation: operation.clone(),
                parent_work_id: parent_work_id.clone(),
                started_at_ms: *started_at_ms,
                cancellable: *cancellable,
            },
            SessionEventKind::RuntimeWorkCancelRequested {
                work_id,
                requested_at_ms,
                client_id,
            } => Self::RuntimeWorkCancelRequested {
                work_id: work_id.clone(),
                requested_at_ms: *requested_at_ms,
                client_id: *client_id,
            },
            SessionEventKind::RuntimeWorkFinished {
                work_id,
                status,
                finished_at_ms,
                message,
            } => Self::RuntimeWorkFinished {
                work_id: work_id.clone(),
                status: *status,
                finished_at_ms: *finished_at_ms,
                message: message.clone(),
            },
            SessionEventKind::RuntimeWorkProgress {
                work_id,
                message,
                progress_at_ms,
                completed_units,
                total_units,
            } => Self::RuntimeWorkProgress {
                work_id: work_id.clone(),
                message: message.clone(),
                progress_at_ms: *progress_at_ms,
                completed_units: *completed_units,
                total_units: *total_units,
            },
            SessionEventKind::ModelTurnCancelRequested {
                turn_id,
                requested_at_ms,
                client_id,
            } => Self::ModelTurnCancelRequested {
                turn_id: turn_id.clone(),
                requested_at_ms: *requested_at_ms,
                client_id: *client_id,
            },
            SessionEventKind::ToolInvocationLifecycle { event } => Self::ToolInvocationLifecycle {
                event: event.clone(),
            },
            SessionEventKind::ToolContribution { event } => Self::ToolContribution {
                event: event.clone(),
            },
            SessionEventKind::ToolExchangeRequested { request } => Self::ToolExchangeRequested {
                request: request.clone(),
            },
            SessionEventKind::ToolExchangeResolved { event } => Self::ToolExchangeResolved {
                event: event.clone(),
            },
            SessionEventKind::ToolInvocationResultRecorded { record } => {
                Self::ToolInvocationResultRecorded {
                    record: record.clone(),
                }
            }
            SessionEventKind::ToolContributionPlaced { envelope } => Self::ToolContributionPlaced {
                envelope: envelope.clone(),
            },
            SessionEventKind::ExecutionSessionCreated {
                provenance,
                visibility,
            } => Self::ExecutionSessionCreated {
                provenance: provenance.clone(),
                visibility: *visibility,
            },
            SessionEventKind::AssistantReasoningActivity { turn_id, activity } => {
                Self::AssistantReasoningActivity {
                    turn_id: turn_id.clone(),
                    activity: activity.clone(),
                }
            }
            SessionEventKind::AssistantResponseSegment {
                turn_id,
                segment_id,
                segment_order,
                text,
            } => Self::AssistantResponseSegment {
                turn_id: turn_id.clone(),
                segment_id: segment_id.clone(),
                segment_order: *segment_order,
                text: text.clone(),
            },
            SessionEventKind::PositionedAssistantResponseSegment {
                turn_id,
                output_position,
                segment_id,
                segment_order,
                text,
            } => Self::PositionedAssistantResponseSegment {
                turn_id: turn_id.clone(),
                output_position: *output_position,
                segment_id: segment_id.clone(),
                segment_order: *segment_order,
                text: text.clone(),
            },
            SessionEventKind::PositionedAssistantReasoningActivity {
                turn_id,
                output_position,
                activity,
            } => Self::PositionedAssistantReasoningActivity {
                turn_id: turn_id.clone(),
                output_position: *output_position,
                activity: activity.clone(),
            },
            SessionEventKind::PositionedToolCallRequested {
                turn_id,
                output_position,
                tool_call_id,
                producer_plugin_id,
                tool_name,
                arguments_json,
                working_directory,
            } => Self::PositionedToolCallRequested {
                turn_id: turn_id.clone(),
                output_position: *output_position,
                tool_call_id: tool_call_id.clone(),
                producer_plugin_id: producer_plugin_id.clone(),
                tool_name: tool_name.clone(),
                arguments_json: arguments_json.clone(),
                working_directory: working_directory.clone(),
            },
            SessionEventKind::WorkingDirectoryChanged {
                old_working_directory,
                new_working_directory,
            } => Self::WorkingDirectoryChanged {
                old_working_directory: old_working_directory.clone(),
                new_working_directory: new_working_directory.clone(),
            },
            SessionEventKind::SessionImported {
                source_id,
                source_display_name,
                external_session_id,
                imported_at_ms,
            } => Self::SessionImported {
                source_id: source_id.clone(),
                source_display_name: source_display_name.clone(),
                external_session_id: external_session_id.clone(),
                imported_at_ms: *imported_at_ms,
            },
            SessionEventKind::SessionForked {
                source_session_id,
                source_title,
                source_cutoff_sequence,
                source_prompt_sequence,
                forked_at_ms,
                kind,
            } => Self::SessionForked {
                source_session_id: *source_session_id,
                source_title: source_title.clone(),
                source_cutoff_sequence: *source_cutoff_sequence,
                source_prompt_sequence: *source_prompt_sequence,
                forked_at_ms: *forked_at_ms,
                kind: *kind,
            },
            SessionEventKind::RalphLifecycle {
                loop_name,
                state_dir,
                kind,
                message,
                occurred_at_ms,
            } => Self::RalphLifecycle {
                loop_name: loop_name.clone(),
                state_dir: state_dir.clone(),
                kind: kind.clone(),
                message: message.clone(),
                occurred_at_ms: *occurred_at_ms,
            },
            SessionEventKind::ReasoningChanged {
                effort,
                summary,
                model_scope,
            } => Self::ReasoningChanged {
                effort: effort.clone(),
                summary: summary.clone(),
                model_scope: model_scope.clone(),
            },
            SessionEventKind::ProviderContextCompacted {
                snapshot,
                compacted_through_sequence,
            } => Self::ProviderContextCompacted {
                snapshot: snapshot.clone(),
                compacted_through_sequence: *compacted_through_sequence,
            },
            SessionEventKind::RequestContextObserved { observation } => {
                Self::RequestContextObserved {
                    observation: observation.clone(),
                }
            }
            SessionEventKind::PluginStatusNote {
                plugin_id,
                note_id,
                text,
                metadata,
            } => Self::PluginStatusNote {
                plugin_id: plugin_id.clone(),
                note_id: note_id.clone(),
                text: text.clone(),
                metadata: metadata.clone(),
            },
            SessionEventKind::InertHistory {
                event_type,
                payload,
            } => Self::InertHistory {
                event_type: event_type.clone(),
                payload: payload.clone(),
            },
        }
    }
}

impl PersistedSessionEventKind {
    #[allow(clippy::too_many_lines)]
    fn into_domain(self) -> SessionEventKind {
        match self {
            Self::SessionCreated {
                name,
                working_directory,
            } => SessionEventKind::SessionCreated {
                name,
                working_directory,
            },
            Self::ClientAttached { client_id } => SessionEventKind::ClientAttached { client_id },
            Self::ClientDetached { client_id } => SessionEventKind::ClientDetached { client_id },
            Self::UserMessage {
                client_id,
                text,
                admission,
            } => SessionEventKind::UserMessage {
                client_id,
                text,
                admission,
            },
            Self::AssistantDelta { text } => SessionEventKind::AssistantDelta { text },
            Self::AssistantMessage { text } => SessionEventKind::AssistantMessage { text },
            Self::ToolCallRequested {
                tool_call_id,
                producer_plugin_id,
                tool_name,
                arguments_json,
                working_directory,
            } => SessionEventKind::ToolCallRequested {
                tool_call_id,
                producer_plugin_id,
                tool_name,
                arguments_json,
                working_directory,
            },
            Self::PermissionRequested {
                permission_id,
                tool_call_id,
                producer_plugin_id,
                tool_name,
                arguments_json,
                batch,
                policy_source,
                policy_reason,
            } => SessionEventKind::PermissionRequested {
                permission_id,
                tool_call_id,
                producer_plugin_id,
                tool_name,
                arguments_json,
                batch,
                policy_source,
                policy_reason,
            },
            Self::PermissionResolved {
                permission_id,
                approved,
            } => SessionEventKind::PermissionResolved {
                permission_id,
                approved,
            },
            Self::ModelChanged {
                provider,
                model,
                selection_source,
            } => SessionEventKind::ModelChanged {
                provider,
                model,
                selection_source,
            },
            Self::SystemMessage { text } => SessionEventKind::SystemMessage { text },
            Self::AgentChanged { agent_id } => SessionEventKind::AgentChanged { agent_id },
            Self::ModelTurnStarted { turn_id } => SessionEventKind::ModelTurnStarted { turn_id },
            Self::ModelTurnFinished {
                turn_id,
                outcome,
                message,
            } => SessionEventKind::ModelTurnFinished {
                turn_id,
                outcome,
                message,
            },
            Self::ModelUsage { turn_id, usage } => SessionEventKind::ModelUsage { turn_id, usage },
            Self::ContextCompacted {
                summary,
                compacted_through_sequence,
            } => SessionEventKind::ContextCompacted {
                summary,
                compacted_through_sequence,
            },
            Self::SessionRenamed { name } => SessionEventKind::SessionRenamed { name },
            Self::TraceEvent { trace } => SessionEventKind::TraceEvent { trace },
            Self::SkillInvoked {
                skill_id,
                arguments,
                source,
                invoked_at_ms,
            } => SessionEventKind::SkillInvoked {
                skill_id,
                arguments,
                source,
                invoked_at_ms,
            },
            Self::SkillSuggested {
                skill_id,
                reason,
                suggested_at_ms,
            } => SessionEventKind::SkillSuggested {
                skill_id,
                reason,
                suggested_at_ms,
            },
            Self::SkillActivated {
                skill_id,
                source,
                mode,
                activated_at_ms,
            } => SessionEventKind::SkillActivated {
                skill_id,
                source,
                mode,
                activated_at_ms,
            },
            Self::SkillDeactivated {
                skill_id,
                deactivated_at_ms,
            } => SessionEventKind::SkillDeactivated {
                skill_id,
                deactivated_at_ms,
            },
            Self::SkillContextLoaded {
                skill_id,
                bytes_loaded,
                truncated,
                loaded_at_ms,
                source,
                preview,
            } => SessionEventKind::SkillContextLoaded {
                skill_id,
                bytes_loaded,
                truncated,
                loaded_at_ms,
                source,
                preview,
            },
            Self::SkillInvocationFailed {
                skill_id,
                error,
                failed_at_ms,
            } => SessionEventKind::SkillInvocationFailed {
                skill_id,
                error,
                failed_at_ms,
            },
            Self::AssistantReasoningDelta { text } => {
                SessionEventKind::AssistantReasoningDelta { text }
            }
            Self::AssistantReasoningMessage { text } => {
                SessionEventKind::AssistantReasoningMessage { text }
            }
            Self::RuntimeWorkStarted {
                work_id,
                kind,
                label,
                tool_call_id,
                plugin_id,
                service_interface,
                operation,
                parent_work_id,
                started_at_ms,
                cancellable,
            } => SessionEventKind::RuntimeWorkStarted {
                work_id,
                kind,
                label,
                tool_call_id,
                plugin_id,
                service_interface,
                operation,
                parent_work_id,
                started_at_ms,
                cancellable,
            },
            Self::RuntimeWorkCancelRequested {
                work_id,
                requested_at_ms,
                client_id,
            } => SessionEventKind::RuntimeWorkCancelRequested {
                work_id,
                requested_at_ms,
                client_id,
            },
            Self::RuntimeWorkFinished {
                work_id,
                status,
                finished_at_ms,
                message,
            } => SessionEventKind::RuntimeWorkFinished {
                work_id,
                status,
                finished_at_ms,
                message,
            },
            Self::RuntimeWorkProgress {
                work_id,
                message,
                progress_at_ms,
                completed_units,
                total_units,
            } => SessionEventKind::RuntimeWorkProgress {
                work_id,
                message,
                progress_at_ms,
                completed_units,
                total_units,
            },
            Self::ModelTurnCancelRequested {
                turn_id,
                requested_at_ms,
                client_id,
            } => SessionEventKind::ModelTurnCancelRequested {
                turn_id,
                requested_at_ms,
                client_id,
            },
            Self::ToolInvocationLifecycle { event } => {
                SessionEventKind::ToolInvocationLifecycle { event }
            }
            Self::ToolContribution { event } => SessionEventKind::ToolContribution { event },
            Self::ToolExchangeRequested { request } => {
                SessionEventKind::ToolExchangeRequested { request }
            }
            Self::ToolExchangeResolved { event } => {
                SessionEventKind::ToolExchangeResolved { event }
            }
            Self::ToolInvocationResultRecorded { record } => {
                SessionEventKind::ToolInvocationResultRecorded { record }
            }
            Self::ToolContributionPlaced { envelope } => {
                SessionEventKind::ToolContributionPlaced { envelope }
            }
            Self::WorkingDirectoryChanged {
                old_working_directory,
                new_working_directory,
            } => SessionEventKind::WorkingDirectoryChanged {
                old_working_directory,
                new_working_directory,
            },
            Self::SessionImported {
                source_id,
                source_display_name,
                external_session_id,
                imported_at_ms,
            } => SessionEventKind::SessionImported {
                source_id,
                source_display_name,
                external_session_id,
                imported_at_ms,
            },
            Self::SessionForked {
                source_session_id,
                source_title,
                source_cutoff_sequence,
                source_prompt_sequence,
                forked_at_ms,
                kind,
            } => SessionEventKind::SessionForked {
                source_session_id,
                source_title,
                source_cutoff_sequence,
                source_prompt_sequence,
                forked_at_ms,
                kind,
            },
            Self::RalphLifecycle {
                loop_name,
                state_dir,
                kind,
                message,
                occurred_at_ms,
            } => SessionEventKind::RalphLifecycle {
                loop_name,
                state_dir,
                kind,
                message,
                occurred_at_ms,
            },
            Self::ReasoningChanged {
                effort,
                summary,
                model_scope,
            } => SessionEventKind::ReasoningChanged {
                effort,
                summary,
                model_scope,
            },
            Self::ProviderContextCompacted {
                snapshot,
                compacted_through_sequence,
            } => SessionEventKind::ProviderContextCompacted {
                snapshot,
                compacted_through_sequence,
            },
            Self::RequestContextObserved { observation } => {
                SessionEventKind::RequestContextObserved { observation }
            }
            Self::PluginStatusNote {
                plugin_id,
                note_id,
                text,
                metadata,
            } => SessionEventKind::PluginStatusNote {
                plugin_id,
                note_id,
                text,
                metadata,
            },
            Self::InertHistory {
                event_type,
                payload,
            } => SessionEventKind::InertHistory {
                event_type,
                payload,
            },
            Self::ExecutionSessionCreated {
                provenance,
                visibility,
            } => SessionEventKind::ExecutionSessionCreated {
                provenance,
                visibility,
            },
            Self::AssistantReasoningActivity { turn_id, activity } => {
                SessionEventKind::AssistantReasoningActivity { turn_id, activity }
            }
            Self::AssistantResponseSegment {
                turn_id,
                segment_id,
                segment_order,
                text,
            } => SessionEventKind::AssistantResponseSegment {
                turn_id,
                segment_id,
                segment_order,
                text,
            },
            Self::PositionedAssistantResponseSegment {
                turn_id,
                output_position,
                segment_id,
                segment_order,
                text,
            } => SessionEventKind::PositionedAssistantResponseSegment {
                turn_id,
                output_position,
                segment_id,
                segment_order,
                text,
            },
            Self::PositionedAssistantReasoningActivity {
                turn_id,
                output_position,
                activity,
            } => SessionEventKind::PositionedAssistantReasoningActivity {
                turn_id,
                output_position,
                activity,
            },
            Self::PositionedToolCallRequested {
                turn_id,
                output_position,
                tool_call_id,
                producer_plugin_id,
                tool_name,
                arguments_json,
                working_directory,
            } => SessionEventKind::PositionedToolCallRequested {
                turn_id,
                output_position,
                tool_call_id,
                producer_plugin_id,
                tool_name,
                arguments_json,
                working_directory,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_models::ToolInvocationResult;

    #[test]
    fn completed_assistant_segment_round_trips_through_persistence() {
        let event = SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 7,
            timestamp_ms: 11,
            session_id: SessionId::new(),
            provenance: None,
            kind: SessionEventKind::AssistantResponseSegment {
                turn_id: "turn-1".to_owned(),
                segment_id: "segment-2".to_owned(),
                segment_order: 2,
                text: "complete answer".to_owned(),
            },
        };

        let encoded = encode_session_event(&event).expect("encode assistant segment");
        let decoded = decode_session_event(&encoded).expect("decode assistant segment");

        assert_eq!(decoded, event);
    }

    #[test]
    fn positioned_turn_outputs_round_trip_through_persistence() {
        let position = bcode_session_models::TurnOutputPosition::new(3);
        let variants = [
            SessionEventKind::PositionedAssistantResponseSegment {
                turn_id: "turn-1".to_owned(),
                output_position: position,
                segment_id: "segment-0".to_owned(),
                segment_order: 0,
                text: "answer".to_owned(),
            },
            SessionEventKind::PositionedAssistantReasoningActivity {
                turn_id: "turn-1".to_owned(),
                output_position: position,
                activity: bcode_session_models::ReasoningActivity {
                    activity_id: "reasoning-1".to_owned(),
                    order: 0,
                    status: bcode_session_models::ReasoningActivityStatus::Completed,
                    parts: Vec::new(),
                    opaque: true,
                },
            },
            SessionEventKind::PositionedToolCallRequested {
                turn_id: "turn-1".to_owned(),
                output_position: position,
                tool_call_id: "call-1".to_owned(),
                producer_plugin_id: Some("bcode.filesystem".to_owned()),
                tool_name: "filesystem.read".to_owned(),
                arguments_json: r#"{"path":"src/lib.rs"}"#.to_owned(),
                working_directory: Some(PathBuf::from("/tmp/project")),
            },
        ];
        for (sequence, kind) in variants.into_iter().enumerate() {
            let event = SessionEvent {
                schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence: u64::try_from(sequence).unwrap_or(u64::MAX),
                timestamp_ms: 11,
                session_id: SessionId::new(),
                provenance: None,
                kind,
            };
            let encoded = encode_session_event(&event).expect("encode positioned output");
            let decoded = decode_session_event(&encoded).expect("decode positioned output");
            assert_eq!(decoded, event);
        }
    }

    #[test]
    fn structured_reasoning_activity_round_trips_through_persistence() {
        let kind = SessionEventKind::AssistantReasoningActivity {
            turn_id: "turn-1".to_owned(),
            activity: bcode_session_models::ReasoningActivity {
                activity_id: "reasoning-1".to_owned(),
                order: 0,
                status: bcode_session_models::ReasoningActivityStatus::Completed,
                parts: vec![bcode_session_models::ReasoningPart {
                    part_id: "raw-0".to_owned(),
                    kind: bcode_session_models::ReasoningContentKind::Raw,
                    role: bcode_session_models::ReasoningContentRole::Detail,
                    order: 0,
                    text: "raw detail".to_owned(),
                }],
                opaque: true,
            },
        };

        let persisted = PersistedSessionEventKind::from(&kind);
        let decoded = persisted.into_domain();

        assert_eq!(decoded, kind);
        assert!(
            !serde_json::to_string(&decoded)
                .expect("reasoning event should encode")
                .contains("encrypted_content")
        );
    }

    #[test]
    fn generic_invocation_result_record_round_trips_through_persistence() {
        let event = SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 7,
            timestamp_ms: 9,
            session_id: SessionId::new(),
            provenance: None,
            kind: SessionEventKind::ToolInvocationResultRecorded {
                record: bcode_session_models::ToolInvocationResultRecord {
                    invocation_id: "call-1".to_owned(),
                    model_output: "done".to_owned(),
                    is_error: false,
                    presentation: None,
                    result: Some(ToolInvocationResult::Json {
                        value: r#"{"ok":true}"#.to_owned(),
                    }),
                },
            },
        };
        let encoded = encode_session_event(&event).expect("encode generic result record");
        let decoded = decode_session_event(&encoded).expect("decode generic result record");

        assert_eq!(decoded, event);
    }

    #[test]
    fn user_message_turn_origin_round_trips_through_persistence() {
        let event = SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 1,
            timestamp_ms: 1,
            session_id: SessionId::new(),
            provenance: None,
            kind: SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: "background prompt".to_string(),
                admission: TurnAdmissionMetadata {
                    origin: Some(bcode_session_models::TurnOrigin {
                        producer: "test.producer".to_string(),
                        correlation_id: Some("operation-1".to_string()),
                        display_label: Some("Background pass 1".to_string()),
                    }),
                    ..TurnAdmissionMetadata::default()
                },
            },
        };

        let encoded = encode_session_event(&event).expect("event should encode");
        let decoded = decode_session_event(&encoded).expect("event should decode");
        assert_eq!(decoded, event);
    }

    #[test]
    fn rejects_corrupt_current_result_payload() {
        let payload = serde_json::json!({
            "schema_version": CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            "sequence": 1,
            "session_id": SessionId::new(),
            "kind": {
                "tool_invocation_result_recorded": {
                    "record": { "model_output": "missing invocation id" }
                }
            }
        })
        .to_string();

        let error = decode_session_event(&payload).expect_err("corrupt event should fail");

        assert!(matches!(error, PersistedSessionEventError::Json(_)));
    }
}
