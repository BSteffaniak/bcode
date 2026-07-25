//! Frozen historical persistence codecs grouped by released schema family.

use bcode_session_models::{
    LocalContextEstimate, ModelRequestIdentity, RequestContextObservation,
    RequestContextTokenCount, SessionEvent, SessionEventKind, SessionEventProvenance, SessionId,
    ToolArtifact, ToolArtifactRef, ToolInvocationResult, ToolInvocationResultRecord,
    current_unix_timestamp_ms,
};
use serde::Deserialize;

use crate::classification::{HistoricalDecode, HistoricalEventMetadata};
use crate::historical::HistoricalSessionEventError;

#[derive(Debug, Deserialize)]
pub struct HistoricalEnvelope {
    schema_version: u16,
    sequence: u64,
    #[serde(default = "current_unix_timestamp_ms")]
    timestamp_ms: u64,
    session_id: SessionId,
    #[serde(default)]
    provenance: Option<SessionEventProvenance>,
    kind: serde_json::Value,
}

impl HistoricalEnvelope {
    pub const fn schema_version(&self) -> u16 {
        self.schema_version
    }

    fn materialize(&self, kind: SessionEventKind) -> SessionEvent {
        SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: self.sequence,
            timestamp_ms: self.timestamp_ms,
            session_id: self.session_id,
            provenance: self.provenance.clone(),
            kind,
        }
    }
}

fn source_kind(
    envelope: &HistoricalEnvelope,
) -> Result<(&str, &serde_json::Value), HistoricalSessionEventError> {
    let Some(kind) = envelope.kind.as_object() else {
        return Err(HistoricalSessionEventError::InvalidEvent {
            schema_version: envelope.schema_version,
            event_kind: "<invalid>".to_owned(),
            reason: "kind is not an object".to_owned(),
        });
    };
    if kind.len() != 1 {
        return Err(HistoricalSessionEventError::InvalidEvent {
            schema_version: envelope.schema_version,
            event_kind: "<invalid>".to_owned(),
            reason: "kind must contain exactly one event variant".to_owned(),
        });
    }
    let (event_kind, payload) = kind.iter().next().expect("kind length was validated");
    Ok((event_kind, payload))
}

/// Frozen codec for schema 28, the released format affected by the invocation/context incident.
pub mod schema_28 {
    use super::{
        HistoricalDecode, HistoricalEnvelope, HistoricalEventMetadata, HistoricalSessionEventError,
        LocalContextEstimate, ModelRequestIdentity, RequestContextObservation,
        RequestContextTokenCount, SessionEventKind, ToolArtifact, ToolArtifactRef,
        ToolInvocationResult, ToolInvocationResultRecord, source_kind,
    };
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct ToolCallFinished {
        tool_call_id: String,
        result: String,
        #[serde(default)]
        is_error: bool,
        #[serde(default)]
        semantic_result: Option<ToolInvocationResultDto>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum ToolInvocationResultDto {
        Text { text: String },
        Json { value: String },
        Artifact { artifact: Box<ToolArtifactDto> },
    }

    #[derive(Debug, Deserialize)]
    struct ToolArtifactDto {
        artifact_id: String,
        producer_plugin_id: String,
        schema: String,
        schema_version: u32,
        #[serde(default)]
        tool_call_id: Option<String>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        metadata: serde_json::Value,
        #[serde(default)]
        refs: Vec<ToolArtifactRefDto>,
    }

    impl From<ToolArtifactDto> for ToolArtifact {
        fn from(value: ToolArtifactDto) -> Self {
            Self {
                artifact_id: value.artifact_id,
                producer_plugin_id: value.producer_plugin_id,
                schema: value.schema,
                schema_version: value.schema_version,
                tool_call_id: value.tool_call_id,
                title: value.title,
                metadata: value.metadata,
                refs: value.refs.into_iter().map(Into::into).collect(),
            }
        }
    }

    #[derive(Debug, Deserialize)]
    struct ToolArtifactRefDto {
        key: String,
        #[serde(default)]
        content_type: Option<String>,
        #[serde(default)]
        storage_uri: Option<String>,
        #[serde(default)]
        byte_len: Option<u64>,
        #[serde(default)]
        metadata: Option<serde_json::Value>,
    }

    impl From<ToolArtifactRefDto> for ToolArtifactRef {
        fn from(value: ToolArtifactRefDto) -> Self {
            Self {
                key: value.key,
                content_type: value.content_type,
                storage_uri: value.storage_uri,
                byte_len: value.byte_len,
                metadata: value.metadata,
            }
        }
    }

    impl ToolInvocationResultDto {
        fn into_current(self) -> ToolInvocationResult {
            match self {
                Self::Text { text } => ToolInvocationResult::Text { text },
                Self::Json { value } => ToolInvocationResult::Json { value },
                Self::Artifact { artifact } => ToolInvocationResult::Artifact {
                    artifact: Box::new((*artifact).into()),
                },
            }
        }
    }

    #[derive(Debug, Deserialize)]
    struct FlatContextUsageSnapshot {
        provider_plugin_id: String,
        model_id: String,
        input_tokens: u64,
        context_through_sequence: u64,
        request_id: String,
        model_turn_id: String,
        round: u32,
        request_fingerprint: String,
        #[serde(default)]
        auth_profile: Option<String>,
        #[serde(default)]
        context_format_version: Option<u16>,
        #[serde(default)]
        compatibility_key: Option<String>,
        #[serde(default)]
        context_epoch: u64,
        estimated_input_tokens: u64,
        source: ContextUsageSource,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum ContextUsageSource {
        Provider,
        Estimated,
    }

    impl FlatContextUsageSnapshot {
        fn into_current(self) -> RequestContextObservation {
            let context_tokens = match self.source {
                ContextUsageSource::Provider => {
                    RequestContextTokenCount::ProviderExact(self.input_tokens)
                }
                ContextUsageSource::Estimated => {
                    RequestContextTokenCount::Estimated(self.input_tokens)
                }
            };
            RequestContextObservation {
                request: ModelRequestIdentity {
                    provider_plugin_id: self.provider_plugin_id,
                    requested_model_id: Some(self.model_id.clone()),
                    effective_model_id: self.model_id,
                    request_id: self.request_id,
                    model_turn_id: self.model_turn_id,
                    round: self.round,
                    request_fingerprint: self.request_fingerprint,
                    effective_auth_profile: self.auth_profile,
                    context_format_version: self.context_format_version,
                    compatibility_key: self.compatibility_key,
                    context_epoch: self.context_epoch,
                },
                context_through_sequence: self.context_through_sequence,
                context_tokens,
                local_estimate: LocalContextEstimate {
                    tokens: self.estimated_input_tokens,
                    algorithm_version: 1,
                },
            }
        }
    }

    pub fn decode(
        envelope: &HistoricalEnvelope,
    ) -> Result<HistoricalDecode, HistoricalSessionEventError> {
        let (event_kind, event_payload) = source_kind(envelope)?;
        let metadata = HistoricalEventMetadata {
            source_schema: envelope.schema_version,
            source_kind: event_kind.to_owned(),
        };
        match event_kind {
            "tool_call_finished" => {
                let source = serde_json::from_value::<ToolCallFinished>(event_payload.clone())
                    .map_err(|error| HistoricalSessionEventError::InvalidEvent {
                        schema_version: envelope.schema_version,
                        event_kind: event_kind.to_owned(),
                        reason: error.to_string(),
                    })?;
                Ok(HistoricalDecode::Converted {
                    event: envelope.materialize(SessionEventKind::ToolInvocationResultRecorded {
                        record: ToolInvocationResultRecord {
                            invocation_id: source.tool_call_id,
                            model_output: source.result.clone(),
                            is_error: source.is_error,
                            presentation: None,
                            result: source
                                .semantic_result
                                .map(ToolInvocationResultDto::into_current),
                        },
                    }),
                    metadata,
                })
            }
            "context_usage_observed" => {
                let snapshot = event_payload.get("snapshot").cloned().ok_or_else(|| {
                    HistoricalSessionEventError::InvalidEvent {
                        schema_version: envelope.schema_version,
                        event_kind: event_kind.to_owned(),
                        reason: "missing snapshot".to_owned(),
                    }
                })?;
                let snapshot = serde_json::from_value::<FlatContextUsageSnapshot>(snapshot)
                    .map_err(|error| HistoricalSessionEventError::InvalidEvent {
                        schema_version: envelope.schema_version,
                        event_kind: event_kind.to_owned(),
                        reason: error.to_string(),
                    })?;
                Ok(HistoricalDecode::Converted {
                    event: envelope.materialize(SessionEventKind::RequestContextObserved {
                        observation: snapshot.into_current(),
                    }),
                    metadata,
                })
            }
            "tool_invocation_stream" => Ok(HistoricalDecode::RetiredKnown {
                event: envelope.materialize(SessionEventKind::OpaqueEvent {
                    event_type: event_kind.to_owned(),
                    payload: event_payload.clone(),
                }),
                metadata,
            }),
            _ => Err(HistoricalSessionEventError::UnsupportedEventKind {
                schema_version: envelope.schema_version,
                event_kind: event_kind.to_owned(),
            }),
        }
    }
}
