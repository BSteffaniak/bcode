//! Durable JSON compatibility DTOs for persisted session events.
//!
//! These types intentionally live in the session package instead of the IPC
//! package. Persistence DTOs may use JSON-oriented serde behavior for
//! compatibility and migration, while IPC DTOs must remain safe for the
//! non-self-describing `bmux_codec` wire format.

use bcode_session_models::{
    CURRENT_SESSION_EVENT_SCHEMA_VERSION, ClientId, LegacyToolRequestPresentationMetadata,
    ModelRequestIdentity, ModelTurnOutcome, ProviderContextSnapshot, RequestContextObservation,
    RequestContextTokenCount, RuntimeWorkKind, RuntimeWorkStatus, SessionEvent,
    SessionEventCompatibilityIssue, SessionEventCompatibilityKind, SessionEventKind,
    SessionEventProvenance, SessionForkKind, SessionId, SessionTokenUsage, SessionTraceEvent,
    ToolArtifact, ToolInvocationResult, ToolInvocationStreamEvent, TraceBlobRef,
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
    let mut value = serde_json::from_str::<serde_json::Value>(payload)?;
    preserve_retired_event_kind_as_legacy(&mut value)?;
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

const RETIRED_INTERACTIVE_TOOL_EVENT_KINDS: [&str; 2] = [
    "interactive_tool_request_created",
    "interactive_tool_request_resolved",
];

fn preserve_retired_event_kind_as_legacy(
    value: &mut serde_json::Value,
) -> Result<(), PersistedSessionEventError> {
    let Some(kind) = value
        .get_mut("kind")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return Ok(());
    };
    let Some(event_type) = RETIRED_INTERACTIVE_TOOL_EVENT_KINDS
        .iter()
        .find(|event_type| kind.contains_key(**event_type))
        .copied()
    else {
        return Ok(());
    };
    let Some(payload) = kind.get(event_type) else {
        return Ok(());
    };
    validate_retired_event_kind(event_type, payload)?;
    let payload = kind
        .remove(event_type)
        .expect("validated retired event payload must remain present");
    kind.clear();
    kind.insert(
        "legacy_event".to_owned(),
        serde_json::json!({
            "event_type": event_type,
            "payload": payload,
        }),
    );
    Ok(())
}

fn validate_retired_event_kind(
    event_type: &str,
    payload: &serde_json::Value,
) -> Result<(), PersistedSessionEventError> {
    let required_fields: &[&str] = match event_type {
        "interactive_tool_request_created" => &[
            "interaction_id",
            "tool_call_id",
            "tool_name",
            "surface_kind",
            "request_json",
        ],
        "interactive_tool_request_resolved" => {
            &["interaction_id", "tool_call_id", "resolution_json"]
        }
        _ => return Ok(()),
    };
    let Some(payload) = payload.as_object() else {
        return Err(PersistedSessionEventError::InvalidLegacyEvent {
            kind: event_type.to_owned(),
            reason: "payload is not an object".to_owned(),
        });
    };
    for field in required_fields {
        if !payload
            .get(*field)
            .is_some_and(serde_json::Value::is_string)
        {
            return Err(PersistedSessionEventError::InvalidLegacyEvent {
                kind: event_type.to_owned(),
                reason: format!("missing or non-string field {field}"),
            });
        }
    }
    Ok(())
}

/// Result of compatibility decoding one persisted canonical event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompatibleSessionEvent {
    /// Event semantics are understood by the current persistence layer.
    Known(SessionEvent),
    /// The stable envelope is trustworthy, but event semantics are unsupported.
    Opaque {
        /// Inert history representation retaining the canonical sequence and payload.
        event: SessionEvent,
        /// Structured reason this event is opaque.
        issue: SessionEventCompatibilityIssue,
    },
}

impl CompatibleSessionEvent {
    /// Return the replayable event representation.
    #[must_use]
    pub const fn event(&self) -> &SessionEvent {
        match self {
            Self::Known(event) | Self::Opaque { event, .. } => event,
        }
    }

    /// Consume the outcome and return its replayable event representation.
    #[must_use]
    pub fn into_event(self) -> SessionEvent {
        match self {
            Self::Known(event) | Self::Opaque { event, .. } => event,
        }
    }

    /// Return the compatibility issue when this event is opaque.
    #[must_use]
    pub const fn issue(&self) -> Option<&SessionEventCompatibilityIssue> {
        match self {
            Self::Known(_) => None,
            Self::Opaque { issue, .. } => Some(issue),
        }
    }
}

/// Decode a persisted session event for normal user-facing reads.
///
/// Unknown event kinds and future-schema events with trustworthy envelopes are
/// returned explicitly as [`CompatibleSessionEvent::Opaque`]. Structurally
/// malformed records return an error. This must not be used by repair, doctor,
/// reindex, or migration code, which requires strict semantic compatibility as
/// well as envelope validity.
///
/// # Errors
///
/// Returns an error when the persisted envelope is malformed or cannot be
/// trusted.
pub fn decode_session_event_compatible(
    payload: &str,
) -> Result<CompatibleSessionEvent, PersistedSessionEventError> {
    match decode_session_event(payload) {
        Ok(event) => Ok(CompatibleSessionEvent::Known(event)),
        Err(PersistedSessionEventError::UnsupportedSchemaVersion { actual, .. }) => {
            decode_opaque_session_event(payload).map(|(event, event_kind)| {
                CompatibleSessionEvent::Opaque {
                    issue: compatibility_issue(
                        &event,
                        event_kind,
                        SessionEventCompatibilityKind::FutureSchema,
                        format!(
                            "open this session with a Bcode build supporting event schema {actual}"
                        ),
                    ),
                    event,
                }
            })
        }
        Err(PersistedSessionEventError::UnsupportedEventKind { kind }) => {
            decode_opaque_session_event(payload).map(|(event, event_kind)| {
                CompatibleSessionEvent::Opaque {
                    issue: compatibility_issue(
                        &event,
                        event_kind,
                        SessionEventCompatibilityKind::UnknownEventKind,
                        format!(
                            "open this session with a Bcode build supporting event kind {kind}"
                        ),
                    ),
                    event,
                }
            })
        }
        Err(error) => Err(error),
    }
}

const fn compatibility_issue(
    event: &SessionEvent,
    event_kind: String,
    compatibility: SessionEventCompatibilityKind,
    remediation: String,
) -> SessionEventCompatibilityIssue {
    SessionEventCompatibilityIssue {
        sequence: event.sequence,
        event_kind,
        schema_version: event.schema_version,
        compatibility,
        remediation,
    }
}

/// Decode a persisted event best-effort for bounded metadata discovery.
///
/// Unlike [`decode_session_event_compatible`], this helper discards structural
/// errors. It is reserved for best-effort catalog/metadata scans where one
/// damaged row must not hide the canonical session directory.
#[must_use]
pub fn decode_session_event_degraded(payload: &str) -> Option<SessionEvent> {
    decode_session_event_compatible(payload)
        .ok()
        .map(CompatibleSessionEvent::into_event)
}

fn decode_opaque_session_event(
    payload: &str,
) -> Result<(SessionEvent, String), PersistedSessionEventError> {
    let persisted = serde_json::from_str::<OpaquePersistedSessionEvent>(payload)?;
    let Some(kind) = persisted.kind.as_object() else {
        return Err(PersistedSessionEventError::InvalidOpaqueEvent {
            reason: "kind is not an object".to_owned(),
        });
    };
    if kind.len() != 1 {
        return Err(PersistedSessionEventError::InvalidOpaqueEvent {
            reason: "kind must contain exactly one event variant".to_owned(),
        });
    }
    let (event_type, payload) = kind.iter().next().expect("kind length was validated");
    let event_type = event_type.clone();
    Ok((
        SessionEvent {
            schema_version: persisted.schema_version,
            sequence: persisted.sequence,
            timestamp_ms: persisted.timestamp_ms,
            session_id: persisted.session_id,
            provenance: persisted.provenance,
            kind: SessionEventKind::LegacyEvent {
                event_type: event_type.clone(),
                payload: payload.clone(),
            },
        },
        event_type,
    ))
}

#[derive(Debug, Deserialize)]
struct OpaquePersistedSessionEvent {
    schema_version: u16,
    sequence: u64,
    #[serde(default = "current_unix_timestamp_ms")]
    timestamp_ms: u64,
    session_id: SessionId,
    #[serde(default)]
    provenance: Option<SessionEventProvenance>,
    kind: serde_json::Value,
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
    /// A recognized retired event kind is malformed and cannot be preserved safely.
    #[error("invalid legacy persisted session event kind {kind}: {reason}")]
    InvalidLegacyEvent { kind: String, reason: String },
    /// A semantically opaque event did not contain a trustworthy persisted envelope.
    #[error("invalid opaque persisted session event: {reason}")]
    InvalidOpaqueEvent { reason: String },
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
        /// Schema-30 compatibility field used before complete admission metadata existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<bcode_session_models::TurnOrigin>,
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
    RequestContextObserved {
        observation: RequestContextObservation,
    },
    ContextUsageObserved {
        snapshot: LegacyContextUsageSnapshot,
    },
    PluginStatusNote {
        plugin_id: String,
        note_id: String,
        text: String,
        #[serde(default)]
        metadata: BTreeMap<String, serde_json::Value>,
    },
    LegacyEvent {
        event_type: String,
        payload: serde_json::Value,
    },
    #[serde(rename = "plugin_automation_turn_started")]
    LegacyTurnStarted {
        plugin_id: String,
        run_id: String,
        operation_id: String,
        display_label: String,
        turn_id: String,
        user_event_sequence: u64,
        read_only: bool,
    },
    #[serde(rename = "plugin_automation_turn_finished")]
    LegacyTurnFinished {
        plugin_id: String,
        operation_id: String,
        turn_id: String,
        outcome: ModelTurnOutcome,
        #[serde(default)]
        message: Option<String>,
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
}

#[derive(Debug, Serialize, Deserialize)]
struct LegacyContextUsageSnapshot {
    invocation: LegacyModelInvocationIdentity,
    context_through_sequence: u64,
    #[serde(alias = "input_tokens")]
    context_input_tokens: u64,
    #[serde(alias = "estimated_input_tokens")]
    local_request_estimate_tokens: u64,
    source: LegacyContextUsageSource,
}

#[derive(Debug, Serialize, Deserialize)]
struct LegacyModelInvocationIdentity {
    provider_plugin_id: String,
    #[serde(default)]
    requested_model_id: Option<String>,
    effective_model_id: String,
    request_id: String,
    model_turn_id: String,
    round: u32,
    request_fingerprint: String,
    #[serde(default)]
    effective_auth_profile: Option<String>,
    #[serde(default)]
    context_format_version: Option<u16>,
    #[serde(default)]
    compatibility_key: Option<String>,
    #[serde(default)]
    context_epoch: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LegacyContextUsageSource {
    Provider,
    Estimated,
}

impl LegacyContextUsageSnapshot {
    fn into_observation(self) -> RequestContextObservation {
        RequestContextObservation {
            request: ModelRequestIdentity {
                provider_plugin_id: self.invocation.provider_plugin_id,
                requested_model_id: self.invocation.requested_model_id,
                effective_model_id: self.invocation.effective_model_id,
                request_id: self.invocation.request_id,
                model_turn_id: self.invocation.model_turn_id,
                round: self.invocation.round,
                request_fingerprint: self.invocation.request_fingerprint,
                effective_auth_profile: self.invocation.effective_auth_profile,
                context_format_version: self.invocation.context_format_version,
                compatibility_key: self.invocation.compatibility_key,
                context_epoch: self.invocation.context_epoch,
            },
            context_through_sequence: self.context_through_sequence,
            context_tokens: match self.source {
                LegacyContextUsageSource::Provider => {
                    RequestContextTokenCount::ProviderExact(self.context_input_tokens)
                }
                LegacyContextUsageSource::Estimated => {
                    RequestContextTokenCount::Estimated(self.context_input_tokens)
                }
            },
            local_estimate: bcode_session_models::LocalContextEstimate {
                tokens: self.local_request_estimate_tokens,
                algorithm_version: 1,
            },
        }
    }
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
                origin: None,
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
            SessionEventKind::PermissionRequested {
                permission_id,
                tool_call_id,
                producer_plugin_id,
                tool_name,
                arguments_json,
                legacy_request_presentation,
                batch,
                policy_source,
                policy_reason,
            } => Self::PermissionRequested {
                permission_id: permission_id.clone(),
                tool_call_id: tool_call_id.clone(),
                producer_plugin_id: producer_plugin_id.clone(),
                tool_name: tool_name.clone(),
                arguments_json: arguments_json.clone(),
                legacy_request_presentation: legacy_request_presentation.clone(),
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
            SessionEventKind::LegacyEvent {
                event_type,
                payload,
            } => Self::LegacyEvent {
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
                origin,
                mut admission,
            } => {
                if admission.origin.is_none() {
                    admission.origin = origin;
                }
                SessionEventKind::UserMessage {
                    client_id,
                    text,
                    admission,
                }
            }
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
            Self::PermissionRequested {
                permission_id,
                tool_call_id,
                producer_plugin_id,
                tool_name,
                arguments_json,
                legacy_request_presentation,
                batch,
                policy_source,
                policy_reason,
            } => SessionEventKind::PermissionRequested {
                permission_id,
                tool_call_id,
                producer_plugin_id,
                tool_name,
                arguments_json,
                legacy_request_presentation,
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
            Self::ExecutionSessionCreated {
                provenance,
                visibility,
            } => SessionEventKind::ExecutionSessionCreated {
                provenance,
                visibility,
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
            Self::RequestContextObserved { observation } => {
                SessionEventKind::RequestContextObserved { observation }
            }
            Self::ContextUsageObserved { snapshot } => SessionEventKind::RequestContextObserved {
                observation: snapshot.into_observation(),
            },
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
            Self::LegacyEvent {
                event_type,
                payload,
            } => SessionEventKind::LegacyEvent {
                event_type,
                payload,
            },
            Self::LegacyTurnStarted {
                plugin_id,
                run_id,
                operation_id,
                display_label,
                turn_id,
                user_event_sequence,
                read_only,
            } => SessionEventKind::LegacyEvent {
                event_type: "plugin_automation_turn_started".to_owned(),
                payload: serde_json::json!({
                    "plugin_id": plugin_id,
                    "run_id": run_id,
                    "operation_id": operation_id,
                    "display_label": display_label,
                    "turn_id": turn_id,
                    "user_event_sequence": user_event_sequence,
                    "read_only": read_only,
                }),
            },
            Self::LegacyTurnFinished {
                plugin_id,
                operation_id,
                turn_id,
                outcome,
                message,
            } => SessionEventKind::LegacyEvent {
                event_type: "plugin_automation_turn_finished".to_owned(),
                payload: serde_json::json!({
                    "plugin_id": plugin_id,
                    "operation_id": operation_id,
                    "turn_id": turn_id,
                    "outcome": outcome,
                    "message": message,
                }),
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
    fn decodes_retired_interactive_tool_events_as_inert_legacy_history() {
        let created = r#"{"schema_version":32,"sequence":1158,"timestamp_ms":1784669781317,"session_id":"00000000-0000-0000-0000-000000000001","kind":{"interactive_tool_request_created":{"interaction_id":"call-1-question","tool_call_id":"call-1","tool_name":"question","interaction_kind":"bcode.question","surface_kind":"bcode.question.inline","request_json":"{\"questions\":[]}","required":true,"turn_behavior":"await_before_continuing","render_target":"transcript_tool_call"}}}"#;
        let resolved = r#"{"schema_version":32,"sequence":1159,"timestamp_ms":1784669784128,"session_id":"00000000-0000-0000-0000-000000000001","kind":{"interactive_tool_request_resolved":{"interaction_id":"call-1-question","tool_call_id":"call-1","resolution_json":"{\"type\":\"submitted\",\"payload\":{}}"}}}"#;

        let created = decode_session_event(created).expect("retired request should decode");
        let resolved = decode_session_event(resolved).expect("retired resolution should decode");

        let SessionEventKind::LegacyEvent {
            event_type,
            payload,
        } = created.kind
        else {
            panic!("retired request must remain inert legacy history");
        };
        assert_eq!(event_type, "interactive_tool_request_created");
        assert_eq!(payload["interaction_id"], "call-1-question");
        assert_eq!(payload["tool_call_id"], "call-1");
        assert_eq!(payload["tool_name"], "question");
        assert_eq!(payload["interaction_kind"], "bcode.question");
        assert_eq!(payload["surface_kind"], "bcode.question.inline");
        assert_eq!(payload["request_json"], r#"{"questions":[]}"#);
        assert_eq!(payload["required"], true);
        assert_eq!(payload["turn_behavior"], "await_before_continuing");
        assert_eq!(payload["render_target"], "transcript_tool_call");

        let SessionEventKind::LegacyEvent {
            event_type,
            payload,
        } = resolved.kind
        else {
            panic!("retired resolution must remain inert legacy history");
        };
        assert_eq!(event_type, "interactive_tool_request_resolved");
        assert_eq!(payload["interaction_id"], "call-1-question");
        assert_eq!(payload["tool_call_id"], "call-1");
        assert_eq!(
            payload["resolution_json"],
            r#"{"type":"submitted","payload":{}}"#
        );
    }

    #[test]
    fn retired_interactive_request_preserves_missing_optional_and_unknown_fields() {
        let payload = r#"{"schema_version":25,"sequence":1,"timestamp_ms":1,"session_id":"00000000-0000-0000-0000-000000000001","kind":{"interactive_tool_request_created":{"interaction_id":"interaction-1","tool_call_id":"call-1","tool_name":"question","surface_kind":"bcode.question.inline","request_json":"{}","future_historical_field":{"nested":true}}}}"#;

        let event = decode_session_event(payload).expect("minimal retired request should decode");
        let SessionEventKind::LegacyEvent { payload, .. } = event.kind else {
            panic!("retired request must remain inert legacy history");
        };
        assert!(payload.get("interaction_kind").is_none());
        assert!(payload.get("required").is_none());
        assert!(payload.get("turn_behavior").is_none());
        assert!(payload.get("render_target").is_none());
        assert_eq!(payload["future_historical_field"]["nested"], true);
    }

    #[test]
    fn retired_interactive_event_reencodes_only_as_legacy_event() {
        let payload = r#"{"schema_version":32,"sequence":1,"timestamp_ms":1,"session_id":"00000000-0000-0000-0000-000000000001","kind":{"interactive_tool_request_resolved":{"interaction_id":"interaction-1","tool_call_id":"call-1","resolution_json":"{\"type\":\"aborted\",\"reason\":\"turn_cancelled\"}"}}}"#;
        let event = decode_session_event(payload).expect("retired resolution should decode");

        let encoded = encode_session_event(&event).expect("legacy event should encode");
        assert!(encoded.contains("\"legacy_event\""));
        assert!(!encoded.contains("\"interactive_tool_request_resolved\":{"));
    }

    #[test]
    fn malformed_retired_interactive_event_is_rejected() {
        let payload = r#"{"schema_version":32,"sequence":1,"timestamp_ms":1,"session_id":"00000000-0000-0000-0000-000000000001","kind":{"interactive_tool_request_created":{"interaction_id":"interaction-1","tool_call_id":"call-1","tool_name":"question","surface_kind":"bcode.question.inline"}}}"#;

        assert!(matches!(
            decode_session_event(payload),
            Err(PersistedSessionEventError::InvalidLegacyEvent { .. })
        ));
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
    fn legacy_context_usage_observation_decodes_to_request_context_observation() {
        let payload = r#"{"schema_version":31,"sequence":7,"timestamp_ms":1,"session_id":"00000000-0000-0000-0000-000000000001","kind":{"context_usage_observed":{"snapshot":{"invocation":{"provider_plugin_id":"provider","requested_model_id":"requested","effective_model_id":"effective","request_id":"request","model_turn_id":"turn","round":2,"request_fingerprint":"fingerprint","effective_auth_profile":"profile","context_format_version":3,"compatibility_key":"key","context_epoch":5},"context_through_sequence":6,"context_input_tokens":123,"local_request_estimate_tokens":100,"source":"provider"}}}}"#;

        let event = decode_session_event(payload).expect("legacy context usage should decode");
        let SessionEventKind::RequestContextObserved { observation } = event.kind else {
            panic!("legacy context usage should map to request context observation");
        };
        assert_eq!(
            observation.request.requested_model_id.as_deref(),
            Some("requested")
        );
        assert_eq!(observation.request.context_epoch, 5);
        assert_eq!(observation.context_through_sequence, 6);
        assert_eq!(
            observation.context_tokens,
            RequestContextTokenCount::ProviderExact(123)
        );
        assert_eq!(observation.local_estimate.tokens, 100);
        assert_eq!(observation.local_estimate.algorithm_version, 1);
    }

    #[test]
    fn schema_30_user_message_origin_migrates_into_admission_metadata() {
        let payload = r#"{"schema_version":30,"sequence":1,"timestamp_ms":1,"session_id":"00000000-0000-0000-0000-000000000001","kind":{"user_message":{"client_id":"00000000-0000-0000-0000-000000000002","text":"hello","origin":{"producer":"test.producer","correlation_id":"operation-1","display_label":"Background pass 1"}}}}"#;

        let event = decode_session_event(payload).expect("schema-30 user message should decode");
        let SessionEventKind::UserMessage { admission, .. } = event.kind else {
            panic!("expected user message");
        };
        assert_eq!(
            admission.origin,
            Some(bcode_session_models::TurnOrigin {
                producer: "test.producer".to_string(),
                correlation_id: Some("operation-1".to_string()),
                display_label: Some("Background pass 1".to_string()),
            })
        );
    }

    #[test]
    fn legacy_user_message_defaults_missing_turn_origin() {
        let payload = r#"{"schema_version":29,"sequence":1,"timestamp_ms":1,"session_id":"00000000-0000-0000-0000-000000000001","kind":{"user_message":{"client_id":"00000000-0000-0000-0000-000000000002","text":"hello"}}}"#;

        let event = decode_session_event(payload).expect("legacy user message should decode");
        let SessionEventKind::UserMessage { admission, .. } = event.kind else {
            panic!("expected user message");
        };
        assert_eq!(
            admission,
            bcode_session_models::TurnAdmissionMetadata::default()
        );
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
                SessionEventKind::LegacyEvent { event_type, .. } => event_type,
                SessionEventKind::PluginStatusNote { .. } => "plugin_status_note".to_owned(),
                other => panic!("unexpected compatibility event: {other:?}"),
            };
            assert_eq!(actual_kind, expected_kind);
        }
    }

    #[test]
    fn decodes_retired_interactive_tool_compatibility_fixtures() {
        let cases = [
            (
                include_str!("../fixtures/migrations/interactive-tool-request-created-v32.json"),
                "interactive_tool_request_created",
            ),
            (
                include_str!("../fixtures/migrations/interactive-tool-request-resolved-v32.json"),
                "interactive_tool_request_resolved",
            ),
            (
                include_str!("../fixtures/migrations/interactive-tool-request-unresolved-v32.json"),
                "interactive_tool_request_created",
            ),
        ];

        for (payload, expected_kind) in cases {
            let event = decode_session_event(payload).expect("schema-32 fixture should decode");
            let SessionEventKind::LegacyEvent { event_type, .. } = event.kind else {
                panic!("retired interactive event must remain inert legacy history");
            };
            assert_eq!(event_type, expected_kind);
        }
    }

    #[test]
    fn compatibility_failure_fixtures_have_exact_strict_and_degraded_outcomes() {
        let opaque_cases = [
            (
                include_str!("../fixtures/migrations/unknown-old-event-kind-v32.json"),
                SessionEventCompatibilityKind::UnknownEventKind,
                "removed_unknown_event",
            ),
            (
                include_str!("../fixtures/migrations/unknown-future-event-kind-v38.json"),
                SessionEventCompatibilityKind::UnknownEventKind,
                "future_event_kind",
            ),
            (
                include_str!("../fixtures/migrations/future-schema-v40.json"),
                SessionEventCompatibilityKind::FutureSchema,
                "assistant_message",
            ),
        ];
        for (payload, expected_compatibility, expected_kind) in opaque_cases {
            assert!(decode_session_event(payload).is_err());
            let CompatibleSessionEvent::Opaque { event, issue } =
                decode_session_event_compatible(payload).expect("trustworthy envelope")
            else {
                panic!("fixture should decode opaquely");
            };
            assert_eq!(event.sequence, 0);
            assert_eq!(issue.event_kind, expected_kind);
            assert_eq!(issue.compatibility, expected_compatibility);
        }

        let malformed = include_str!("../fixtures/migrations/malformed-json-v38.json");
        assert!(matches!(
            decode_session_event_compatible(malformed),
            Err(PersistedSessionEventError::Json(_))
        ));
    }

    #[test]
    fn mixed_schema_32_35_fixture_decodes_contiguously_without_reviving_interactions() {
        let events = include_str!("../fixtures/migrations/mixed-interactive-history-v32-v35.jsonl")
            .lines()
            .map(|payload| {
                decode_session_event(payload).expect("mixed fixture event should decode")
            })
            .collect::<Vec<_>>();

        assert_eq!(
            events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(
            events
                .iter()
                .map(|event| event.schema_version)
                .collect::<Vec<_>>(),
            vec![32, 32, 35]
        );
        assert!(matches!(
            &events[0].kind,
            SessionEventKind::LegacyEvent { event_type, .. }
                if event_type == "interactive_tool_request_created"
        ));
        assert!(matches!(
            &events[1].kind,
            SessionEventKind::LegacyEvent { event_type, .. }
                if event_type == "interactive_tool_request_resolved"
        ));
        assert!(matches!(
            &events[2].kind,
            SessionEventKind::AssistantMessage { text }
                if text == "history continued under schema 35"
        ));
        assert!(events.iter().all(|event| !matches!(
            event.kind,
            SessionEventKind::ToolExchangeRequested { .. }
                | SessionEventKind::ToolExchangeResolved { .. }
        )));
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
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
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
                batch: Some(bcode_session_models::PermissionBatchCorrelation {
                    batch_id: "batch".to_owned(),
                    call_index: 0,
                    call_count: 2,
                }),
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
            SessionEventKind::ToolInvocationLifecycle {
                event: bcode_session_models::ToolInvocationLifecycleEvent {
                    invocation_id: "call".to_string(),
                    sequence: 1,
                    stage: bcode_session_models::ToolInvocationLifecycleStage::Started,
                    message: Some("started".to_string()),
                    metadata: serde_json::json!({"opaque": true}),
                },
            },
            SessionEventKind::ToolContribution {
                event: bcode_session_models::ToolContributionEvent {
                    invocation_id: "call".to_string(),
                    contribution_id: "surface".to_string(),
                    sequence: 1,
                    producer_id: "producer".to_string(),
                    schema: "example.unknown".to_string(),
                    schema_version: 7,
                    operation: bcode_session_models::ToolContributionOperation::Upsert,
                    persistence: bcode_session_models::ToolContributionPersistence::Durable,
                    artifact: None,
                    payload: serde_json::json!({"opaque": [1, 2, 3]}),
                },
            },
            SessionEventKind::ToolExchangeRequested {
                request: bcode_session_models::ToolExchangeRequest {
                    invocation_id: "call".to_string(),
                    exchange_id: "question".to_string(),
                    producer_id: "producer".to_string(),
                    schema: "example.question".to_string(),
                    schema_version: 4,
                    payload: serde_json::json!({"opaque": "request"}),
                    response_policy: bcode_session_models::ToolExchangeResponsePolicy::Required,
                },
            },
            SessionEventKind::ToolExchangeResolved {
                event: bcode_session_models::ToolExchangeResolutionEvent {
                    invocation_id: "call".to_string(),
                    exchange_id: "question".to_string(),
                    resolution: bcode_session_models::ToolExchangeResolution::Responded {
                        payload: serde_json::json!({"opaque": "response"}),
                    },
                },
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
    fn degraded_decode_preserves_unknown_and_future_events_as_opaque_history() {
        for schema_version in [
            CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            CURRENT_SESSION_EVENT_SCHEMA_VERSION + 1,
        ] {
            let payload = serde_json::json!({
                "schema_version": schema_version,
                "sequence": 17,
                "timestamp_ms": 23,
                "session_id": "00000000-0000-0000-0000-000000000001",
                "provenance": {
                    "source_event_id": "source-17",
                    "source_timestamp_ms": 22,
                    "source_locator": "bcode://session/source/event/17"
                },
                "kind": { "future_event_kind": { "value": true, "nested": [1, 2] } }
            })
            .to_string();

            let decoded = decode_session_event_compatible(&payload)
                .expect("trustworthy future envelope should remain inspectable");
            let CompatibleSessionEvent::Opaque { event, issue } = decoded else {
                panic!("unsupported event should have an explicit opaque outcome");
            };
            assert_eq!(event.schema_version, schema_version);
            assert_eq!(event.sequence, 17);
            assert_eq!(event.timestamp_ms, 23);
            assert!(event.provenance.is_some());
            assert_eq!(issue.sequence, 17);
            assert_eq!(issue.event_kind, "future_event_kind");
            assert_eq!(issue.schema_version, schema_version);
            assert_eq!(
                issue.compatibility,
                if schema_version == CURRENT_SESSION_EVENT_SCHEMA_VERSION {
                    SessionEventCompatibilityKind::UnknownEventKind
                } else {
                    SessionEventCompatibilityKind::FutureSchema
                }
            );
            assert!(!issue.remediation.is_empty());
            let SessionEventKind::LegacyEvent {
                event_type,
                payload,
            } = event.kind
            else {
                panic!("future event should be opaque legacy history");
            };
            assert_eq!(event_type, "future_event_kind");
            assert_eq!(payload["value"], true);
            assert_eq!(payload["nested"], serde_json::json!([1, 2]));
        }
    }

    #[test]
    fn degraded_decode_rejects_untrustworthy_opaque_envelopes() {
        for payload in [
            r#"{"schema_version":39,"sequence":1,"session_id":"not-a-session-id","kind":{"future":{}}}"#,
            r#"{"schema_version":39,"sequence":1,"session_id":"00000000-0000-0000-0000-000000000001","kind":{}}"#,
            r#"{"schema_version":39,"sequence":1,"session_id":"00000000-0000-0000-0000-000000000001","kind":{"one":{},"two":{}}}"#,
            "not json",
        ] {
            assert!(decode_session_event_degraded(payload).is_none());
        }
    }

    #[test]
    fn decodes_legacy_tool_presentation_diff_section() {
        let payload = include_str!("../fixtures/migrations/tool-presentation-diff-v25.json");
        let event = decode_session_event(payload).expect("legacy diff section should decode");
        let SessionEventKind::ToolInvocationStream {
            event:
                ToolInvocationStreamEvent::LegacyPresentation {
                    presentation: bcode_session_models::LegacyToolPresentationEvent::Card(card),
                    ..
                },
        } = event.kind
        else {
            panic!("expected a legacy presentation card");
        };
        assert!(matches!(
            card.sections.as_slice(),
            [bcode_session_models::LegacyToolPresentationSection::Diff {
                path: Some(path),
                old_text,
                new_text,
            }] if path == "/tmp/file.rs" && old_text == "before" && new_text == "after"
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
