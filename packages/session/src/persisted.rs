//! Durable JSON compatibility DTOs for persisted session events.
//!
//! These types intentionally live in the session package instead of the IPC
//! package. Persistence DTOs may use JSON-oriented serde behavior for
//! compatibility and migration, while IPC DTOs must remain safe for the
//! non-self-describing `bmux_codec` wire format.

use bcode_session_models::{
    CURRENT_SESSION_EVENT_SCHEMA_VERSION, ClientId, ContextUsageSnapshot,
    LegacyToolRequestPresentationMetadata, ModelTurnOutcome, ProviderContextSnapshot,
    RuntimeWorkKind, RuntimeWorkStatus, SessionEvent, SessionEventKind, SessionEventProvenance,
    SessionForkKind, SessionId, SessionTokenUsage, SessionTraceEvent, ToolArtifact,
    ToolInvocationResult, ToolInvocationStreamEvent, TraceBlobRef, TurnOrigin, WorkId,
    current_unix_timestamp_ms,
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
        if schema_version > CURRENT_SESSION_EVENT_SCHEMA_VERSION {
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

/// Decode a persisted session event from durable JSON, returning `None` for
/// unsupported or corrupt records that should not block normal catalog/open/
/// attach/history paths.
///
/// This is intentionally lossy and must not be used by repair, doctor, reindex,
/// or migration code that needs to report exact damage. Normal user-facing reads
/// use it to degrade safely without implicitly repairing or mutating damaged
/// logs.
#[must_use]
pub fn decode_session_event_degraded(payload: &str) -> Option<SessionEvent> {
    decode_session_event(payload).ok()
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
        if self.schema_version > CURRENT_SESSION_EVENT_SCHEMA_VERSION {
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<TurnOrigin>,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_visual: Option<bcode_session_models::PluginVisualDescriptor>,
        #[serde(
            default,
            rename = "request_presentation",
            skip_serializing_if = "Option::is_none"
        )]
        legacy_request_presentation: Option<LegacyToolRequestPresentationMetadata>,
    },
    ToolCallFinished {
        tool_call_id: String,
        result: String,
        #[serde(default)]
        is_error: bool,
        #[serde(default)]
        output: Option<TraceBlobRef>,
        #[serde(default)]
        semantic_result: Option<PersistedToolInvocationResult>,
    },
    InteractiveToolRequestCreated {
        interaction_id: String,
        tool_call_id: String,
        tool_name: String,
        #[serde(default)]
        interaction_kind: Option<String>,
        surface_kind: String,
        request_json: String,
        #[serde(default)]
        required: bool,
        #[serde(default)]
        turn_behavior: bcode_session_models::InteractiveToolTurnBehavior,
        #[serde(default)]
        render_target: bcode_session_models::InteractiveToolRenderTarget,
    },
    InteractiveToolRequestResolved {
        interaction_id: String,
        tool_call_id: String,
        resolution_json: String,
    },
    PermissionRequested {
        permission_id: String,
        tool_call_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        producer_plugin_id: Option<String>,
        tool_name: String,
        arguments_json: String,
        #[serde(
            default,
            rename = "request_presentation",
            skip_serializing_if = "Option::is_none"
        )]
        legacy_request_presentation: Option<LegacyToolRequestPresentationMetadata>,
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
    /// Incremental tool invocation event emitted while a tool is running.
    ToolInvocationStream {
        event: ToolInvocationStreamEvent,
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
    /// Legacy durable bounded presentation state for a completed tool invocation.
    #[serde(rename = "tool_invocation_presentation")]
    LegacyToolInvocationPresentation {
        tool_call_id: String,
        #[serde(default)]
        started_at_ms: Option<u64>,
        #[serde(default)]
        finished_at_ms: Option<u64>,
        is_error: bool,
        presentation: crate::persisted_legacy::ToolInvocationPresentation,
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
    },
    ProviderContextCompacted {
        snapshot: ProviderContextSnapshot,
        compacted_through_sequence: u64,
    },
    ContextUsageObserved {
        snapshot: ContextUsageSnapshot,
    },
    PluginStatusNote {
        plugin_id: String,
        note_id: String,
        text: String,
        #[serde(default)]
        metadata: BTreeMap<String, serde_json::Value>,
    },
    PluginAutomationTurnStarted {
        plugin_id: String,
        run_id: String,
        operation_id: String,
        display_label: String,
        turn_id: String,
        user_event_sequence: u64,
        read_only: bool,
    },
    PluginAutomationTurnFinished {
        plugin_id: String,
        operation_id: String,
        turn_id: String,
        outcome: ModelTurnOutcome,
        #[serde(default)]
        message: Option<String>,
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
                origin,
            } => Self::UserMessage {
                client_id: *client_id,
                text: text.clone(),
                origin: origin.clone(),
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
                request_visual,
                legacy_request_presentation,
            } => Self::ToolCallRequested {
                tool_call_id: tool_call_id.clone(),
                producer_plugin_id: producer_plugin_id.clone(),
                tool_name: tool_name.clone(),
                arguments_json: arguments_json.clone(),
                working_directory: working_directory.clone(),
                request_visual: request_visual.clone(),
                legacy_request_presentation: legacy_request_presentation.clone(),
            },
            SessionEventKind::ToolCallFinished {
                tool_call_id,
                result,
                is_error,
                output,
                semantic_result,
            } => Self::ToolCallFinished {
                tool_call_id: tool_call_id.clone(),
                result: result.clone(),
                is_error: *is_error,
                output: output.clone(),
                semantic_result: semantic_result
                    .as_ref()
                    .map(PersistedToolInvocationResult::from),
            },
            SessionEventKind::InteractiveToolRequestCreated {
                interaction_id,
                tool_call_id,
                tool_name,
                interaction_kind,
                surface_kind,
                request_json,
                required,
                turn_behavior,
                render_target,
            } => Self::InteractiveToolRequestCreated {
                interaction_id: interaction_id.clone(),
                tool_call_id: tool_call_id.clone(),
                tool_name: tool_name.clone(),
                interaction_kind: interaction_kind.clone(),
                surface_kind: surface_kind.clone(),
                request_json: request_json.clone(),
                required: *required,
                turn_behavior: *turn_behavior,
                render_target: *render_target,
            },
            SessionEventKind::InteractiveToolRequestResolved {
                interaction_id,
                tool_call_id,
                resolution_json,
            } => Self::InteractiveToolRequestResolved {
                interaction_id: interaction_id.clone(),
                tool_call_id: tool_call_id.clone(),
                resolution_json: resolution_json.clone(),
            },
            SessionEventKind::PermissionRequested {
                permission_id,
                tool_call_id,
                producer_plugin_id,
                tool_name,
                arguments_json,
                legacy_request_presentation,
                policy_source,
                policy_reason,
            } => Self::PermissionRequested {
                permission_id: permission_id.clone(),
                tool_call_id: tool_call_id.clone(),
                producer_plugin_id: producer_plugin_id.clone(),
                tool_name: tool_name.clone(),
                arguments_json: arguments_json.clone(),
                legacy_request_presentation: legacy_request_presentation.clone(),
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
            SessionEventKind::ModelChanged { provider, model } => Self::ModelChanged {
                provider: provider.clone(),
                model: model.clone(),
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
            SessionEventKind::ToolInvocationStream { event } => Self::ToolInvocationStream {
                event: event.clone(),
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
            SessionEventKind::ReasoningChanged { effort, summary } => Self::ReasoningChanged {
                effort: effort.clone(),
                summary: summary.clone(),
            },
            SessionEventKind::ProviderContextCompacted {
                snapshot,
                compacted_through_sequence,
            } => Self::ProviderContextCompacted {
                snapshot: snapshot.clone(),
                compacted_through_sequence: *compacted_through_sequence,
            },
            SessionEventKind::ContextUsageObserved { snapshot } => Self::ContextUsageObserved {
                snapshot: snapshot.clone(),
            },
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
            SessionEventKind::PluginAutomationTurnStarted {
                plugin_id,
                run_id,
                operation_id,
                display_label,
                turn_id,
                user_event_sequence,
                read_only,
            } => Self::PluginAutomationTurnStarted {
                plugin_id: plugin_id.clone(),
                run_id: run_id.clone(),
                operation_id: operation_id.clone(),
                display_label: display_label.clone(),
                turn_id: turn_id.clone(),
                user_event_sequence: *user_event_sequence,
                read_only: *read_only,
            },
            SessionEventKind::PluginAutomationTurnFinished {
                plugin_id,
                operation_id,
                turn_id,
                outcome,
                message,
            } => Self::PluginAutomationTurnFinished {
                plugin_id: plugin_id.clone(),
                operation_id: operation_id.clone(),
                turn_id: turn_id.clone(),
                outcome: *outcome,
                message: message.clone(),
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
                origin,
            } => SessionEventKind::UserMessage {
                client_id,
                text,
                origin,
            },
            Self::AssistantDelta { text } => SessionEventKind::AssistantDelta { text },
            Self::AssistantMessage { text } => SessionEventKind::AssistantMessage { text },
            Self::ToolCallRequested {
                tool_call_id,
                producer_plugin_id,
                tool_name,
                arguments_json,
                working_directory,
                request_visual,
                legacy_request_presentation,
            } => SessionEventKind::ToolCallRequested {
                tool_call_id,
                producer_plugin_id,
                tool_name,
                arguments_json,
                working_directory,
                request_visual,
                legacy_request_presentation,
            },
            Self::ToolCallFinished {
                tool_call_id,
                result,
                is_error,
                output,
                semantic_result,
            } => SessionEventKind::ToolCallFinished {
                tool_call_id,
                result,
                is_error,
                output,
                semantic_result: semantic_result.map(PersistedToolInvocationResult::into_domain),
            },
            Self::InteractiveToolRequestCreated {
                interaction_id,
                tool_call_id,
                tool_name,
                interaction_kind,
                surface_kind,
                request_json,
                required,
                turn_behavior,
                render_target,
            } => SessionEventKind::InteractiveToolRequestCreated {
                interaction_id,
                tool_call_id,
                tool_name,
                interaction_kind,
                surface_kind,
                request_json,
                required,
                turn_behavior,
                render_target,
            },
            Self::InteractiveToolRequestResolved {
                interaction_id,
                tool_call_id,
                resolution_json,
            } => SessionEventKind::InteractiveToolRequestResolved {
                interaction_id,
                tool_call_id,
                resolution_json,
            },
            Self::PermissionRequested {
                permission_id,
                tool_call_id,
                producer_plugin_id,
                tool_name,
                arguments_json,
                legacy_request_presentation,
                policy_source,
                policy_reason,
            } => SessionEventKind::PermissionRequested {
                permission_id,
                tool_call_id,
                producer_plugin_id,
                tool_name,
                arguments_json,
                legacy_request_presentation,
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
            Self::ModelChanged { provider, model } => {
                SessionEventKind::ModelChanged { provider, model }
            }
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
            Self::ToolInvocationStream { event } => {
                SessionEventKind::ToolInvocationStream { event }
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
            Self::LegacyToolInvocationPresentation {
                tool_call_id,
                is_error,
                presentation,
                ..
            } => SessionEventKind::ToolCallFinished {
                tool_call_id,
                result: crate::persisted_legacy::presentation_result_text(&presentation),
                is_error,
                output: None,
                semantic_result: Some(crate::persisted_legacy::semantic_from_presentation(
                    &presentation,
                )),
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
            Self::ReasoningChanged { effort, summary } => {
                SessionEventKind::ReasoningChanged { effort, summary }
            }
            Self::ProviderContextCompacted {
                snapshot,
                compacted_through_sequence,
            } => SessionEventKind::ProviderContextCompacted {
                snapshot,
                compacted_through_sequence,
            },
            Self::ContextUsageObserved { snapshot } => {
                SessionEventKind::ContextUsageObserved { snapshot }
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
            Self::PluginAutomationTurnStarted {
                plugin_id,
                run_id,
                operation_id,
                display_label,
                turn_id,
                user_event_sequence,
                read_only,
            } => SessionEventKind::PluginAutomationTurnStarted {
                plugin_id,
                run_id,
                operation_id,
                display_label,
                turn_id,
                user_event_sequence,
                read_only,
            },
            Self::PluginAutomationTurnFinished {
                plugin_id,
                operation_id,
                turn_id,
                outcome,
                message,
            } => SessionEventKind::PluginAutomationTurnFinished {
                plugin_id,
                operation_id,
                turn_id,
                outcome,
                message,
            },
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum PersistedToolInvocationResult {
    Current(CurrentPersistedToolInvocationResult),
    Legacy(crate::persisted_legacy::ToolInvocationResultCompat),
}

impl From<&ToolInvocationResult> for PersistedToolInvocationResult {
    fn from(value: &ToolInvocationResult) -> Self {
        Self::Current(CurrentPersistedToolInvocationResult::from(value))
    }
}

impl PersistedToolInvocationResult {
    fn into_domain(self) -> ToolInvocationResult {
        match self {
            Self::Current(value) => value.into_domain(),
            Self::Legacy(value) => value.into_domain(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CurrentPersistedToolInvocationResult {
    Text { text: String },
    Json { value: String },
    Artifact { artifact: Box<ToolArtifact> },
}

impl From<&ToolInvocationResult> for CurrentPersistedToolInvocationResult {
    fn from(value: &ToolInvocationResult) -> Self {
        match value {
            ToolInvocationResult::Text { text } => Self::Text { text: text.clone() },
            ToolInvocationResult::Json { value } => Self::Json {
                value: value.clone(),
            },
            ToolInvocationResult::Artifact { artifact } => Self::Artifact {
                artifact: artifact.clone(),
            },
        }
    }
}

impl CurrentPersistedToolInvocationResult {
    fn into_domain(self) -> ToolInvocationResult {
        match self {
            Self::Text { text } => ToolInvocationResult::Text { text },
            Self::Json { value } => ToolInvocationResult::Json { value },
            Self::Artifact { artifact } => ToolInvocationResult::Artifact { artifact },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_user_message_defaults_missing_turn_origin() {
        let payload = r#"{"schema_version":29,"sequence":1,"timestamp_ms":1,"session_id":"00000000-0000-0000-0000-000000000001","kind":{"user_message":{"client_id":"00000000-0000-0000-0000-000000000002","text":"hello"}}}"#;

        let event = decode_session_event(payload).expect("legacy user message should decode");
        assert!(matches!(
            event.kind,
            SessionEventKind::UserMessage { origin: None, .. }
        ));
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
                origin: Some(TurnOrigin {
                    producer: "test.producer".to_string(),
                    correlation_id: Some("operation-1".to_string()),
                    display_label: Some("Background pass 1".to_string()),
                }),
            },
        };

        let encoded = encode_session_event(&event).expect("event should encode");
        let decoded = decode_session_event(&encoded).expect("event should decode");
        assert_eq!(decoded, event);
    }

    #[test]
    fn decodes_schema_29_automation_compatibility_fixtures() {
        let cases = [
            (
                include_str!("../fixtures/migrations/plugin-automation-turn-started-v29.json"),
                "plugin_automation_turn_started",
            ),
            (
                include_str!("../fixtures/migrations/plugin-automation-turn-finished-v29.json"),
                "plugin_automation_turn_finished",
            ),
            (
                include_str!("../fixtures/migrations/plugin-status-note-v29.json"),
                "plugin_status_note",
            ),
        ];

        for (payload, expected_kind) in cases {
            let event = decode_session_event(payload).expect("schema-29 fixture should decode");
            let actual_kind = match event.kind {
                SessionEventKind::PluginAutomationTurnStarted { .. } => {
                    "plugin_automation_turn_started"
                }
                SessionEventKind::PluginAutomationTurnFinished { .. } => {
                    "plugin_automation_turn_finished"
                }
                SessionEventKind::PluginStatusNote { .. } => "plugin_status_note",
                other => panic!("unexpected compatibility event: {other:?}"),
            };
            assert_eq!(actual_kind, expected_kind);
        }
    }

    #[test]
    fn decodes_current_and_legacy_persisted_tool_results() {
        for (semantic_result, assertion) in semantic_result_cases() {
            let event = decode_session_event(&event_payload(&semantic_result))
                .expect("persisted event should decode");

            assertion(event);
        }
    }

    #[test]
    fn encodes_and_decodes_persisted_tool_results_through_dto_layer() {
        for (semantic_result, assertion) in semantic_result_cases() {
            let mut original = decode_session_event(&event_payload(&semantic_result))
                .expect("fixture should decode through persisted DTOs");
            original.sequence = 42;

            let encoded = encode_session_event(&original).expect("event should encode");
            let decoded = decode_session_event(&encoded).expect("event should decode");

            assert_eq!(decoded.sequence, 42);
            assertion(decoded);
        }
    }

    #[test]
    fn persisted_event_kind_dto_covers_all_current_domain_event_kinds() {
        let session_id = SessionId::new();
        for kind in all_event_kind_samples(session_id) {
            let original = SessionEvent {
                schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence: 1,
                timestamp_ms: 1,
                session_id,
                provenance: None,
                kind: kind.clone(),
            };

            let encoded = encode_session_event(&original).expect("event should encode");
            let decoded = decode_session_event(&encoded).expect("event should decode");

            assert_eq!(decoded.kind, kind);
        }
    }

    #[allow(clippy::too_many_lines)]
    fn all_event_kind_samples(session_id: SessionId) -> Vec<SessionEventKind> {
        vec![
            SessionEventKind::SessionCreated {
                name: Some("session".to_string()),
                working_directory: PathBuf::from("/tmp/session"),
            },
            SessionEventKind::ClientAttached {
                client_id: ClientId::new(),
            },
            SessionEventKind::ClientDetached {
                client_id: ClientId::new(),
            },
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: "hello".to_string(),
                origin: None,
            },
            SessionEventKind::AssistantDelta {
                text: "delta".to_string(),
            },
            SessionEventKind::AssistantMessage {
                text: "message".to_string(),
            },
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call".to_string(),
                producer_plugin_id: None,
                tool_name: "tool".to_string(),
                arguments_json: "{}".to_string(),
                working_directory: None,
                request_visual: None,
                legacy_request_presentation: None,
            },
            SessionEventKind::ToolCallFinished {
                tool_call_id: "call".to_string(),
                result: "done".to_string(),
                is_error: false,
                output: None,
                semantic_result: Some(ToolInvocationResult::Text {
                    text: "semantic".to_string(),
                }),
            },
            SessionEventKind::PermissionRequested {
                permission_id: "perm".to_string(),
                tool_call_id: "call".to_string(),
                producer_plugin_id: None,
                tool_name: "tool".to_string(),
                arguments_json: "{}".to_string(),
                legacy_request_presentation: None,
                policy_source: None,
                policy_reason: None,
            },
            SessionEventKind::PermissionResolved {
                permission_id: "perm".to_string(),
                approved: true,
            },
            SessionEventKind::ModelChanged {
                provider: "provider".to_string(),
                model: "model".to_string(),
            },
            SessionEventKind::SystemMessage {
                text: "system".to_string(),
            },
            SessionEventKind::AgentChanged {
                agent_id: "agent".to_string(),
            },
            SessionEventKind::ModelTurnStarted {
                turn_id: "turn".to_string(),
            },
            SessionEventKind::ModelTurnFinished {
                turn_id: "turn".to_string(),
                outcome: ModelTurnOutcome::Completed,
                message: Some("done".to_string()),
            },
            SessionEventKind::ModelUsage {
                turn_id: "turn".to_string(),
                usage: SessionTokenUsage {
                    input_tokens: Some(1),
                    cached_input_tokens: Some(2),
                    cache_write_input_tokens: Some(3),
                    output_tokens: Some(4),
                    reasoning_tokens: Some(5),
                    total_tokens: Some(6),
                },
            },
            SessionEventKind::ContextCompacted {
                summary: "summary".to_string(),
                compacted_through_sequence: 1,
            },
            SessionEventKind::SessionRenamed {
                name: Some("renamed".to_string()),
            },
            SessionEventKind::TraceEvent {
                trace: Box::new(SessionTraceEvent {
                    timestamp_ms: 1,
                    turn_id: Some("turn".to_string()),
                    phase: bcode_session_models::SessionTracePhase::ModelProviderEvent,
                    payload: bcode_session_models::SessionTracePayload::ProviderEvent {
                        event_type: "event".to_string(),
                        detail: Some("detail".to_string()),
                    },
                }),
            },
            SessionEventKind::SkillInvoked {
                skill_id: SkillId::new("skill"),
                arguments: "{}".to_string(),
                source: None,
                invoked_at_ms: 1,
            },
            SessionEventKind::SkillSuggested {
                skill_id: SkillId::new("skill"),
                reason: Some("reason".to_string()),
                suggested_at_ms: 2,
            },
            SessionEventKind::SkillActivated {
                skill_id: SkillId::new("skill"),
                source: None,
                mode: SkillActivationMode::Automatic,
                activated_at_ms: 3,
            },
            SessionEventKind::SkillDeactivated {
                skill_id: SkillId::new("skill"),
                deactivated_at_ms: 4,
            },
            SessionEventKind::SkillContextLoaded {
                skill_id: SkillId::new("skill"),
                bytes_loaded: 12,
                truncated: false,
                loaded_at_ms: 5,
                source: None,
                preview: None,
            },
            SessionEventKind::SkillInvocationFailed {
                skill_id: SkillId::new("skill"),
                error: "error".to_string(),
                failed_at_ms: 6,
            },
            SessionEventKind::AssistantReasoningDelta {
                text: "reasoning delta".to_string(),
            },
            SessionEventKind::AssistantReasoningMessage {
                text: "reasoning".to_string(),
            },
            SessionEventKind::RuntimeWorkStarted {
                work_id: WorkId::new("work-started"),
                kind: RuntimeWorkKind::Tool,
                label: "work".to_string(),
                tool_call_id: Some("call".to_string()),
                plugin_id: None,
                service_interface: None,
                operation: None,
                parent_work_id: None,
                started_at_ms: Some(7),
                cancellable: true,
            },
            SessionEventKind::RuntimeWorkCancelRequested {
                work_id: WorkId::new("work-cancel"),
                requested_at_ms: Some(8),
                client_id: None,
            },
            SessionEventKind::RuntimeWorkFinished {
                work_id: WorkId::new("work-finished"),
                status: RuntimeWorkStatus::Completed,
                finished_at_ms: Some(9),
                message: Some("finished".to_string()),
            },
            SessionEventKind::RuntimeWorkProgress {
                work_id: WorkId::new("work-progress"),
                message: "progress".to_string(),
                progress_at_ms: Some(10),
                completed_units: Some(1),
                total_units: Some(2),
            },
            SessionEventKind::ModelTurnCancelRequested {
                turn_id: "turn".to_string(),
                requested_at_ms: Some(11),
                client_id: None,
            },
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::Finished {
                    tool_call_id: "call".to_string(),
                    sequence: 1,
                    is_error: false,
                    finished_at_ms: Some(14),
                },
            },
            SessionEventKind::WorkingDirectoryChanged {
                old_working_directory: PathBuf::from("/tmp/old"),
                new_working_directory: PathBuf::from("/tmp/new"),
            },
            SessionEventKind::SessionImported {
                source_id: "source".to_string(),
                source_display_name: "Source".to_string(),
                external_session_id: "external".to_string(),
                imported_at_ms: 12,
            },
            SessionEventKind::SessionForked {
                source_session_id: session_id,
                source_title: Some("source".to_string()),
                source_cutoff_sequence: Some(1),
                source_prompt_sequence: Some(0),
                forked_at_ms: 15,
                kind: SessionForkKind::Fork,
            },
            SessionEventKind::ReasoningChanged {
                effort: Some("none".to_string()),
                summary: Some("auto".to_string()),
            },
        ]
    }

    #[test]
    fn rejects_future_schema_version() {
        let payload = serde_json::json!({
            "schema_version": CURRENT_SESSION_EVENT_SCHEMA_VERSION + 1,
            "sequence": 1,
            "session_id": SessionId::new(),
            "kind": { "assistant_message": { "text": "future" } }
        })
        .to_string();

        let error = decode_session_event(&payload).expect_err("future schema should fail");

        assert!(matches!(
            error,
            PersistedSessionEventError::UnsupportedSchemaVersion { .. }
        ));
    }

    #[test]
    fn rejects_unknown_future_event_kind() {
        let payload = serde_json::json!({
            "schema_version": CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            "sequence": 1,
            "session_id": SessionId::new(),
            "kind": { "future_event_kind": { "value": true } }
        })
        .to_string();

        let error = decode_session_event(&payload).expect_err("future event kind should fail");

        assert!(matches!(
            error,
            PersistedSessionEventError::UnsupportedEventKind { .. }
        ));
    }

    #[test]
    fn rejects_corrupt_event_payload() {
        let payload = serde_json::json!({
            "schema_version": CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            "sequence": 1,
            "session_id": SessionId::new(),
            "kind": { "tool_call_finished": { "result": "missing id" } }
        })
        .to_string();

        let error = decode_session_event(&payload).expect_err("corrupt event should fail");

        assert!(matches!(error, PersistedSessionEventError::Json(_)));
    }

    type PersistedAssertion = fn(SessionEvent);

    fn semantic_result_cases() -> Vec<(serde_json::Value, PersistedAssertion)> {
        vec![
            (
                serde_json::json!({ "type": "text", "text": "plain" }),
                assert_text_result,
            ),
            (
                serde_json::json!({ "type": "json", "value": "{\"ok\":true}" }),
                assert_json_result,
            ),
            (
                serde_json::json!({
                    "type": "file_change",
                    "result": {
                        "tool_name": "filesystem.write",
                        "summary": "wrote bytes",
                        "path": "file.txt",
                        "future_field": "ignored"
                    },
                    "future_top_level": "ignored"
                }),
                assert_file_change_result,
            ),
            (
                serde_json::json!({
                    "type": "shell_run",
                    "result": {
                        "mode": "terminal",
                        "output": "legacy tail"
                    }
                }),
                assert_legacy_terminal_result,
            ),
            (
                serde_json::json!({
                    "type": "shell_run",
                    "result": {
                        "mode": "captured",
                        "stdout": "hello\n",
                        "stderr": ""
                    }
                }),
                assert_captured_result,
            ),
        ]
    }

    fn event_payload(semantic_result: &serde_json::Value) -> String {
        serde_json::json!({
            "schema_version": CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            "sequence": 1,
            "session_id": SessionId::new(),
            "provenance": null,
            "kind": {
                "tool_call_finished": {
                    "tool_call_id": "call-1",
                    "result": "legacy",
                    "semantic_result": semantic_result.clone()
                }
            }
        })
        .to_string()
    }

    fn tool_result(event: SessionEvent) -> ToolInvocationResult {
        let SessionEventKind::ToolCallFinished {
            semantic_result: Some(result),
            ..
        } = event.kind
        else {
            panic!("expected semantic tool result");
        };
        result
    }

    fn assert_text_result(event: SessionEvent) {
        assert_eq!(
            tool_result(event),
            ToolInvocationResult::Text {
                text: "plain".to_string(),
            }
        );
    }

    fn assert_json_result(event: SessionEvent) {
        assert_eq!(
            tool_result(event),
            ToolInvocationResult::Json {
                value: r#"{"ok":true}"#.to_string(),
            }
        );
    }

    fn assert_file_change_result(event: SessionEvent) {
        let ToolInvocationResult::Json { value } = tool_result(event) else {
            panic!("expected generic file-change json");
        };
        let value: serde_json::Value =
            serde_json::from_str(&value).expect("file change should decode");
        assert_eq!(value["tool_name"], "filesystem.write");
        assert_eq!(value["summary"], "wrote bytes");
        assert_eq!(value["path"], "file.txt");
    }

    fn assert_legacy_terminal_result(event: SessionEvent) {
        let ToolInvocationResult::Json { value } = tool_result(event) else {
            panic!("expected generic shell-run json");
        };
        let value: serde_json::Value =
            serde_json::from_str(&value).expect("shell result should decode");
        assert_eq!(value["mode"], "terminal");
        assert_eq!(value["output_tail"], "legacy tail");
        assert_eq!(value["columns"], 80);
        assert_eq!(value["rows"], 24);
    }

    fn assert_captured_result(event: SessionEvent) {
        let ToolInvocationResult::Json { value } = tool_result(event) else {
            panic!("expected generic shell-run json");
        };
        let value: serde_json::Value =
            serde_json::from_str(&value).expect("shell result should decode");
        assert_eq!(value["mode"], "captured");
        assert_eq!(value["stdout"], "hello\n");
        assert_eq!(value["stderr"], "");
    }
}
