//! Explicit live diagnostic DTOs, not replay or a second session semantic reducer.
//!
//! Canonical outcomes and stream positions are retained as supplied by their owning
//! contracts. We never serialize `SessionEvent`, provider context, tool presentation,
//! typed plugin results, original usage, or opaque provider metadata. Free text is
//! sensitive and uses the existing credential-shaped diagnostic sanitizer, with an
//! explicit lossy-projection marker. Domain-private objects require owner projections.
use bcode_model::{ProviderOutputEvent, ProviderTurnEvent};
use bcode_session_models::{SessionEvent, SessionEventKind, SessionId};
use bcode_xray::CaptureContext;
use bcode_xray_models::XrayCorrelation;
use serde_json::{Value, json};

use crate::ServerState;

const MAX_SOURCE_TEXT_BYTES: usize = 16 * 1024;

fn session_context(state: &ServerState, session_id: SessionId) -> CaptureContext {
    let session_id = session_id.to_string();
    if let Some(context) = CaptureContext::current()
        && context.correlation().session_id == session_id
    {
        return context;
    }
    state.xray.context(XrayCorrelation {
        session_id,
        ..Default::default()
    })
}

/// Content is deliberately projected, never described as lossless raw input/output.
/// Build nothing and retain no bytes when deep capture is disabled. Large source text
/// is omitted rather than scanned, cloned, or silently truncated on the producer path.
fn protected_text(context: &CaptureContext, field: &str, text: &str) {
    protected_text_at(context, field, text, None);
}

fn protected_text_at(
    context: &CaptureContext,
    field: &str,
    text: &str,
    canonical_sequence: Option<u64>,
) {
    if !context.deep_capture_active() {
        return;
    }
    if text.len() > MAX_SOURCE_TEXT_BYTES {
        context.emit(
            "session",
            "content_omitted",
            json!({
                "field": field, "canonical_sequence": canonical_sequence,
                "reason": "source_byte_budget_exceeded",
                "source_bytes": text.len(), "max_source_bytes": MAX_SOURCE_TEXT_BYTES,
                "complete": false,
            }),
            false,
        );
        return;
    }
    let sanitized = bcode_model_provider_runtime::sanitize_provider_diagnostic(text);
    // A domain-specific diagnostic text DTO: never an unrestricted canonical object.
    // It remains sensitive even after known credential shapes have been removed.
    let projected = json!({
        "schema_version": 1, "field": field, "canonical_sequence": canonical_sequence,
        "text": sanitized,
        "representation": "credential_filtered_diagnostic_text",
        "source_bytes": text.len(), "complete_original": false,
        "redaction": "known_credential_shapes_only",
    });
    let Ok(bytes) = serde_json::to_vec(&projected) else {
        context.emit(
            "session",
            "content_omitted",
            json!({
                "field": field, "reason": "encoding_unavailable", "complete": false,
            }),
            false,
        );
        return;
    };
    match context.capture_payload("application/vnd.bcode.xray.diagnostic-text+json", &bytes) {
        Ok(Some(reference)) => context.emit(
            "session",
            "content_projected",
            json!({
                "field": field, "canonical_sequence": canonical_sequence,
                "payload": reference, "complete_original": false,
            }),
            true,
        ),
        Ok(None) => {} // Observer detached between gating and payload admission.
        Err(_) => context.emit(
            "session",
            "content_omitted",
            json!({
                "field": field, "reason": "payload_admission_unavailable", "complete": false,
            }),
            false,
        ),
    }
}

fn canonical_projection(context: &CaptureContext, event: &SessionEvent) {
    let mut correlation = context.correlation().clone();
    let mut data =
        json!({"canonical_sequence": event.sequence, "timestamp_ms": event.timestamp_ms});
    // Generic execution correlation is owner-defined, not proof of workflow identity.
    // Preserve it in domain data without guessing an activation from a unit ID.
    let admission = match &event.kind {
        SessionEventKind::UserMessage { admission, .. }
        | SessionEventKind::SubmissionActivated { admission, .. } => Some(admission),
        SessionEventKind::SubmissionAccepted { submission } => Some(&submission.admission),
        _ => None,
    };
    if let Some(execution) =
        admission.and_then(|admission| admission.execution.correlation.as_ref())
    {
        data["execution_correlation"] = json!({
            "execution_id": execution.execution_id,
            "unit_id": execution.unit_id,
            "attempt": execution.attempt,
        });
    }
    let text = canonical_content(event, &mut correlation, &mut data);
    let context = context.with_correlation(correlation);
    context.emit(
        "session",
        super::session_event_kind_name(&event.kind),
        data,
        false,
    );
    if let Some((field, value)) = text {
        protected_text_at(&context, field, value, Some(event.sequence));
    }
}

/// Select protected text separately from scalar metadata; never serialize the event itself.
fn canonical_content<'a>(
    event: &'a SessionEvent,
    correlation: &mut XrayCorrelation,
    data: &mut Value,
) -> Option<(&'static str, &'a str)> {
    let mut text = None;
    match &event.kind {
        SessionEventKind::UserMessage { text: value, .. } => {
            correlation.turn_id = Some(super::execution_turn_id(event));
            text = Some(("user_message", value.as_str()));
        }
        SessionEventKind::SubmissionActivated {
            turn_id,
            text: value,
            ..
        } => {
            correlation.turn_id = Some(turn_id.0.clone());
            text = Some(("user_message", value.as_str()));
        }
        SessionEventKind::SubmissionAccepted { submission } => {
            correlation.turn_id = Some(super::execution_turn_id(event));
            data["submission_id"] = json!(submission.id);
            text = Some(("accepted_user_message", submission.text.as_str()));
        }
        SessionEventKind::AssistantMessage { text: value }
        | SessionEventKind::AssistantDelta { text: value } => {
            text = Some(("assistant_text", value.as_str()));
        }
        SessionEventKind::AssistantResponseSegment {
            turn_id,
            segment_id,
            segment_order,
            text: value,
        }
        | SessionEventKind::PositionedAssistantResponseSegment {
            turn_id,
            segment_id,
            segment_order,
            text: value,
            ..
        } => {
            correlation.turn_id = Some(turn_id.clone());
            data["segment_id"] = json!(segment_id);
            data["segment_order"] = json!(segment_order);
            if let SessionEventKind::PositionedAssistantResponseSegment {
                output_position, ..
            } = &event.kind
            {
                data["output_position"] = json!(output_position);
            }
            text = Some(("assistant_segment", value.as_str()));
        }
        SessionEventKind::ToolInvocationResultRecorded { record } => {
            correlation.tool_call_id = Some(record.invocation_id.clone());
            data["is_error"] = json!(record.is_error);
            data["content_items"] = json!(record.content.len());
            data["structured_content"] = json!("omitted_requires_tool_owned_projection");
            text = Some(("tool_model_output", record.model_output.as_str()));
        }
        _ => canonical_scalars(event, correlation, data),
    }
    text
}

/// Project only explicitly approved scalar fields and owner-supplied identities.
fn canonical_scalars(event: &SessionEvent, correlation: &mut XrayCorrelation, data: &mut Value) {
    match &event.kind {
        SessionEventKind::ModelTurnStarted { turn_id }
        | SessionEventKind::ModelTurnCancelRequested { turn_id, .. } => {
            correlation.turn_id = Some(turn_id.clone());
        }
        SessionEventKind::ModelTurnFinished {
            turn_id,
            outcome,
            message,
        } => {
            correlation.turn_id = Some(turn_id.clone());
            data["outcome"] = json!(outcome);
            // Outcome is authoritative; arbitrary diagnostic messages are not public errors.
            data["diagnostic_present"] = json!(message.is_some());
        }
        SessionEventKind::ToolCallRequested {
            tool_call_id,
            arguments_json,
            ..
        }
        | SessionEventKind::PositionedToolCallRequested {
            tool_call_id,
            arguments_json,
            ..
        } => {
            correlation.tool_call_id = Some(tool_call_id.clone());
            if let SessionEventKind::PositionedToolCallRequested {
                turn_id,
                output_position,
                ..
            } = &event.kind
            {
                correlation.turn_id = Some(turn_id.clone());
                data["output_position"] = json!(output_position);
            }
            // Arbitrary arguments may contain auth objects. Do not serialize or parse them
            // here; only the owning tool can supply a credential-free argument DTO.
            data["argument_bytes"] = json!(arguments_json.len());
            data["arguments"] = json!("omitted_requires_tool_owned_projection");
        }
        SessionEventKind::ModelUsage { turn_id, usage } => {
            correlation.turn_id = Some(turn_id.clone());
            correlation.request_id.clone_from(&usage.request_id);
            data["usage"] = json!({
                "input_tokens": usage.input_tokens, "output_tokens": usage.output_tokens,
                "total_tokens": usage.total_tokens, "cached_input_tokens": usage.cached_input_tokens,
                "cache_write_input_tokens": usage.cache_write_input_tokens,
                "terminal": usage.terminal, "observation_ordinal": usage.observation_ordinal,
            });
        }
        SessionEventKind::AssistantReasoningActivity { turn_id, activity }
        | SessionEventKind::PositionedAssistantReasoningActivity {
            turn_id, activity, ..
        } => {
            correlation.turn_id = Some(turn_id.clone());
            data["activity_id"] = json!(activity.activity_id);
            data["order"] = json!(activity.order);
            data["status"] = json!(activity.status);
            data["parts"] = json!(activity.parts.len());
            data["opaque_observed"] = json!(activity.opaque);
            // An unbounded vector of reasoning parts must not fan out on publication.
            data["content"] = json!("readable_parts_available_in_provider_stream_projection");
        }
        SessionEventKind::ExecutionSessionCreated { provenance, .. } => {
            data["owner"] = json!(provenance.owner);
            data["execution_id"] = json!(provenance.run_id);
            data["unit_id"] = json!(provenance.node_id);
            data["attempt"] = json!(provenance.attempt);
            // The owner-defined execution identity is not universally a workflow run.
            data["activation_id"] = json!(provenance.activation_id);
        }
        _ => canonical_invocation_scalars(event, correlation, data),
    }
}

/// Project invocation lifecycle, permission outcomes, and runtime-work identities without
/// exposing plugin messages, metadata, permission descriptions, or tool payloads.
fn canonical_invocation_scalars(
    event: &SessionEvent,
    correlation: &mut XrayCorrelation,
    data: &mut Value,
) {
    match &event.kind {
        SessionEventKind::ToolInvocationLifecycle { event } => {
            correlation.tool_call_id = Some(event.invocation_id.clone());
            data["stage"] = json!(event.stage);
            data["producer_sequence"] = json!(event.sequence);
            // message and metadata intentionally not serialized.
        }
        SessionEventKind::PermissionRequested {
            permission_id,
            tool_call_id,
            ..
        } => {
            correlation.tool_call_id = Some(tool_call_id.clone());
            data["permission_id"] = json!(permission_id);
            data["outcome"] = json!("pending");
        }
        SessionEventKind::PermissionResolved {
            permission_id,
            approved,
        } => {
            data["permission_id"] = json!(permission_id);
            data["approved"] = json!(approved);
        }
        SessionEventKind::RuntimeWorkStarted {
            work_id,
            tool_call_id,
            kind,
            ..
        } => {
            correlation.tool_call_id.clone_from(tool_call_id);
            data["work_id"] = json!(work_id);
            data["kind"] = json!(kind);
        }
        SessionEventKind::RuntimeWorkFinished {
            work_id, status, ..
        } => {
            data["work_id"] = json!(work_id);
            data["status"] = json!(status);
        }
        // No arbitrary schema fallback: opaque/new canonical data remains unavailable.
        _ => {
            data["detail"] = json!("no_safe_domain_projection");
        }
    }
}

pub fn canonical_event(state: &ServerState, event: &SessionEvent) {
    if state.xray.capture_active(&event.session_id.to_string()) {
        canonical_projection(&session_context(state, event.session_id), event);
    }
}

fn output_projection(context: &CaptureContext, event: &ProviderOutputEvent) -> Value {
    match event {
        ProviderOutputEvent::TextDelta { text } => {
            protected_text(context, "provider_text_delta", text);
            json!({"text_bytes": text.len()})
        }
        ProviderOutputEvent::ToolCallStarted { call_id, .. } => json!({"tool_call_id": call_id}),
        ProviderOutputEvent::ToolCallDelta { call_id, delta } => json!({
            "tool_call_id": call_id, "argument_delta_bytes": delta.len(),
            "arguments": "omitted_requires_tool_owned_projection",
        }),
        ProviderOutputEvent::ToolCallFinished { call } => json!({
            "tool_call_id": call.id, "arguments": "omitted_requires_tool_owned_projection",
        }),
        ProviderOutputEvent::ReasoningActivity { event } => reasoning_projection(context, event),
    }
}

fn reasoning_projection(
    context: &CaptureContext,
    event: &bcode_session_models::ReasoningActivityEvent,
) -> Value {
    use bcode_session_models::ReasoningActivityEvent as R;
    let mut data =
        json!({"activity_id": event.activity_id(), "activity_order": event.activity_order()});
    match event {
        R::PartDelta {
            text,
            part_id,
            part_order,
            kind,
            role,
            ..
        }
        | R::PartCompleted {
            text,
            part_id,
            part_order,
            kind,
            role,
            ..
        } => {
            data["part_id"] = json!(part_id);
            data["part_order"] = json!(part_order);
            data["kind"] = json!(kind);
            data["role"] = json!(role);
            data["operation"] = json!(if matches!(event, R::PartDelta { .. }) {
                "delta"
            } else {
                "completed"
            });
            protected_text(context, "reasoning_text", text);
        }
        R::Started { .. } => data["operation"] = json!("started"),
        R::OpaqueObserved { .. } => data["operation"] = json!("opaque_observed"),
        R::Finished { status, .. } => {
            data["operation"] = json!("finished");
            data["status"] = json!(status);
        }
    }
    data
}

fn provider_projection(context: &CaptureContext, event: &ProviderTurnEvent) {
    let mut correlation = context.correlation().clone();
    let call_id = match event {
        ProviderTurnEvent::ToolCallStarted { call_id, .. }
        | ProviderTurnEvent::ToolCallDelta { call_id, .. } => Some(call_id),
        ProviderTurnEvent::ToolCallFinished { call } => Some(&call.id),
        ProviderTurnEvent::Output { event, .. } => match event {
            ProviderOutputEvent::ToolCallStarted { call_id, .. }
            | ProviderOutputEvent::ToolCallDelta { call_id, .. } => Some(call_id),
            ProviderOutputEvent::ToolCallFinished { call } => Some(&call.id),
            _ => None,
        },
        _ => None,
    };
    correlation.tool_call_id = call_id.cloned();
    let context = context.with_correlation(correlation);
    let data = match event {
        ProviderTurnEvent::TextDelta { text } | ProviderTurnEvent::ReasoningDelta { text } => {
            protected_text(&context, "provider_text", text);
            json!({"text_bytes": text.len()})
        }
        ProviderTurnEvent::Output { position, event } => json!({
            "position": position, "output": output_projection(&context, event),
        }),
        ProviderTurnEvent::ReasoningActivity { event } => reasoning_projection(&context, event),
        ProviderTurnEvent::ToolCallStarted { call_id, .. } => json!({"tool_call_id": call_id}),
        ProviderTurnEvent::ToolCallDelta { call_id, delta } => json!({
            "tool_call_id": call_id, "argument_delta_bytes": delta.len(),
            "arguments": "omitted_requires_tool_owned_projection",
        }),
        ProviderTurnEvent::ToolCallFinished { call } => json!({
            "tool_call_id": call.id, "arguments": "omitted_requires_tool_owned_projection",
        }),
        ProviderTurnEvent::Usage { usage } => json!({
            "input_tokens": usage.input_tokens, "output_tokens": usage.output_tokens,
            "total_tokens": usage.total_tokens, "cached_input_tokens": usage.cached_input_tokens,
            "cache_write_input_tokens": usage.cache_write_input_tokens,
            "reasoning_tokens": usage.reasoning_tokens,
        }),
        ProviderTurnEvent::Error { error } => {
            json!({"outcome": "error", "category": error.category})
        }
        ProviderTurnEvent::TurnStarted => json!({"outcome": "started"}),
        ProviderTurnEvent::TurnFinished { stop_reason } => json!({"stop_reason": stop_reason}),
        ProviderTurnEvent::Cancelled => json!({"outcome": "cancelled"}),
        ProviderTurnEvent::RetryScheduled { retry_at_unix, .. } => {
            json!({"retry_at_unix": retry_at_unix})
        }
        ProviderTurnEvent::Warning { .. } => json!({"outcome": "warning", "message": "omitted"}),
        ProviderTurnEvent::RequestProjection { projection } => json!({
            "input_item_count": projection.input_item_count,
            "message_count": projection.message_count,
            "sent_message_count": projection.sent_message_count,
            "omitted_message_count": projection.omitted_message_count,
            "serialized_body_bytes": projection.serialized_body_bytes,
        }),
        // In particular ContextCompacted contains ModelMessage/provider_context. Never
        // serialize it, even when a deep observer exists. Provider adapters own it.
        _ => json!({"detail": "requires_domain_owned_projection"}),
    };
    context.emit("model", super::provider_turn_event_kind(event), data, false);
}

pub fn provider_event(
    state: &ServerState,
    session_id: SessionId,
    provider_turn_id: &str,
    turn_id: &str,
    event: &ProviderTurnEvent,
) {
    if !state.xray.capture_active(&session_id.to_string()) {
        return;
    }
    let context = session_context(state, session_id);
    let mut correlation = context.correlation().clone();
    correlation.turn_id = Some(turn_id.into());
    correlation.request_id = Some(provider_turn_id.into());
    provider_projection(&context.with_correlation(correlation), event);
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_xray::XrayHub;
    use bcode_xray_models::XrayObserveRequest;

    fn event(kind: SessionEventKind) -> SessionEvent {
        SessionEvent {
            schema_version: 1,
            sequence: 7,
            timestamp_ms: 42,
            session_id: SessionId::new(),
            provenance: None,
            kind,
        }
    }

    #[tokio::test]
    async fn canonical_outcome_is_preserved_but_arbitrary_error_text_is_not() {
        let hub = XrayHub::default();
        let event = event(SessionEventKind::ModelTurnFinished {
            turn_id: "turn".into(),
            outcome: bcode_session_models::ModelTurnOutcome::Error,
            message: Some("Authorization: Bearer must-not-appear".into()),
        });
        let mut observation = hub
            .observe(XrayObserveRequest {
                schema_version: 1,
                session_id: event.session_id.to_string(),
                deep_capture: true,
            })
            .expect("observe");
        let _ready = observation.recv().await;
        let context = hub.context(XrayCorrelation {
            session_id: event.session_id.to_string(),
            ..Default::default()
        });
        canonical_projection(&context, &event);
        let emitted = observation.recv().await.expect("outcome").event;
        assert_eq!(emitted.correlation.turn_id.as_deref(), Some("turn"));
        assert_eq!(
            emitted.data["outcome"],
            json!(bcode_session_models::ModelTurnOutcome::Error)
        );
        assert!(!emitted.data.to_string().contains("must-not-appear"));
    }

    #[tokio::test]
    async fn shallow_stream_never_includes_text_or_tool_arguments() {
        let hub = XrayHub::default();
        let mut observation = hub
            .observe(XrayObserveRequest {
                schema_version: 1,
                session_id: "session".into(),
                deep_capture: false,
            })
            .expect("observe");
        let _ready = observation.recv().await;
        let context = hub.context(XrayCorrelation {
            session_id: "session".into(),
            ..Default::default()
        });
        provider_projection(
            &context,
            &ProviderTurnEvent::TextDelta {
                text: "private prompt".into(),
            },
        );
        provider_projection(
            &context,
            &ProviderTurnEvent::ToolCallFinished {
                call: bcode_model::ToolCall {
                    id: "call".into(),
                    name: "tool".into(),
                    arguments: json!({"password":"never-emit"}),
                },
            },
        );
        let text = observation.recv().await.expect("text metadata").event;
        assert_eq!(text.data["text_bytes"], 14);
        assert!(!text.data.to_string().contains("private prompt"));
        let call = observation.recv().await.expect("call metadata").event;
        assert_eq!(call.correlation.tool_call_id.as_deref(), Some("call"));
        assert!(!call.data.to_string().contains("never-emit"));
        assert!(!call.sensitive);
    }

    #[tokio::test]
    async fn deep_text_payload_is_referenced_filtered_and_never_inlined() {
        let hub = XrayHub::default();
        let mut observation = hub
            .observe(XrayObserveRequest {
                schema_version: 1,
                session_id: "session".into(),
                deep_capture: true,
            })
            .expect("observe");
        let _ready = observation.recv().await;
        let context = hub.context(XrayCorrelation {
            session_id: "session".into(),
            ..Default::default()
        });
        protected_text(
            &context,
            "tool_model_output",
            "status ok; password=credential-never-emit",
        );
        let emitted = observation.recv().await.expect("reference").event;
        assert!(emitted.sensitive);
        assert_eq!(emitted.name, "content_projected");
        assert!(!emitted.data.to_string().contains("status ok"));
        let reference: bcode_xray_models::XrayPayloadRef =
            serde_json::from_value(emitted.data["payload"].clone()).expect("typed reference");
        let retained = hub
            .read_payload(&bcode_xray_models::XrayReadPayloadRequest {
                schema_version: 1,
                observation_id: observation.id().into(),
                session_id: "session".into(),
                payload_id: reference.payload_id,
                offset: 0,
                length: 4096,
            })
            .expect("retained projection");
        let text = String::from_utf8(retained.bytes).expect("json utf8");
        assert!(text.contains("status ok"));
        assert!(!text.contains("credential-never-emit"));
        assert!(text.contains("REDACTED"));
        let dto: Value = serde_json::from_str(&text).expect("text dto");
        assert_eq!(dto["complete_original"], false);
    }

    #[tokio::test]
    async fn oversized_deep_content_is_explicitly_omitted_without_partial_payload() {
        let hub = XrayHub::default();
        let mut observation = hub
            .observe(XrayObserveRequest {
                schema_version: 1,
                session_id: "session".into(),
                deep_capture: true,
            })
            .expect("observe");
        let _ready = observation.recv().await;
        let context = hub.context(XrayCorrelation {
            session_id: "session".into(),
            ..Default::default()
        });
        protected_text(
            &context,
            "user_message",
            &"x".repeat(MAX_SOURCE_TEXT_BYTES + 1),
        );
        let emitted = observation.recv().await.expect("omission").event;
        assert_eq!(emitted.name, "content_omitted");
        assert_eq!(emitted.data["reason"], "source_byte_budget_exceeded");
        assert_eq!(emitted.data["complete"], false);
        assert!(emitted.data.get("payload").is_none());
    }

    #[tokio::test]
    async fn permission_and_tool_terminal_projection_retains_real_outcomes_without_private_objects()
    {
        let hub = XrayHub::default();
        let permission = event(SessionEventKind::PermissionResolved {
            permission_id: "permission".into(),
            approved: false,
        });
        let mut observation = hub
            .observe(XrayObserveRequest {
                schema_version: 1,
                session_id: permission.session_id.to_string(),
                deep_capture: false,
            })
            .expect("observe");
        let _ready = observation.recv().await;
        let context = hub.context(XrayCorrelation {
            session_id: permission.session_id.to_string(),
            tool_call_id: Some("call".into()),
            turn_id: Some("turn".into()),
            ..Default::default()
        });
        canonical_projection(&context, &permission);
        let emitted = observation.recv().await.expect("denial").event;
        assert_eq!(emitted.data["approved"], false);
        assert_eq!(emitted.data["permission_id"], "permission");
        assert_eq!(emitted.correlation.tool_call_id.as_deref(), Some("call"));
        let result = SessionEvent {
            kind: SessionEventKind::ToolInvocationResultRecorded {
                record: bcode_session_models::ToolInvocationResultRecord {
                    invocation_id: "call".into(),
                    model_output: "private result".into(),
                    is_error: true,
                    content: vec![],
                    presentation: None,
                    result: None,
                },
            },
            ..permission
        };
        canonical_projection(&context, &result);
        let emitted = observation.recv().await.expect("tool result").event;
        assert_eq!(emitted.data["is_error"], true);
        assert_eq!(emitted.correlation.turn_id.as_deref(), Some("turn"));
        assert!(!emitted.data.to_string().contains("private result"));
    }

    #[tokio::test]
    async fn even_deep_capture_never_serializes_opaque_provider_metadata() {
        let hub = XrayHub::default();
        let mut observation = hub
            .observe(XrayObserveRequest {
                schema_version: 1,
                session_id: "session".into(),
                deep_capture: true,
            })
            .expect("observe");
        let _ready = observation.recv().await;
        let context = hub.context(XrayCorrelation {
            session_id: "session".into(),
            ..Default::default()
        });
        provider_projection(
            &context,
            &ProviderTurnEvent::ProviderMetadata {
                key: "auth".into(),
                value: "credential-never-emit".into(),
            },
        );
        let emitted = observation.recv().await.expect("omission").event;
        assert!(!emitted.data.to_string().contains("credential-never-emit"));
        assert_eq!(emitted.data["detail"], "requires_domain_owned_projection");
    }
}
