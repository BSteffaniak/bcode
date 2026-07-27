//! Frozen historical persistence codecs grouped by released schema family.

use bcode_session_models::{
    LocalContextEstimate, ModelRequestIdentity, RequestContextObservation,
    RequestContextTokenCount, SessionEvent, SessionEventKind, SessionEventProvenance, SessionId,
    ToolArtifact, ToolArtifactRef, ToolInvocationResult, ToolInvocationResultRecord,
    current_unix_timestamp_ms,
};
use serde::Deserialize;

use crate::classification::{HistoricalDecode, HistoricalEventMetadata};
use crate::execution::HistoricalSessionEventError;

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

    pub fn source_kind_name(&self) -> Result<&str, HistoricalSessionEventError> {
        source_kind(self).map(|(kind, _)| kind)
    }

    pub fn decode_retired_known(&self) -> Result<HistoricalDecode, HistoricalSessionEventError> {
        let (event_kind, payload) = source_kind(self)?;
        Ok(HistoricalDecode::RetiredKnown {
            event: self.materialize(SessionEventKind::InertHistory {
                event_type: event_kind.to_owned(),
                payload: payload.clone(),
            }),
            metadata: HistoricalEventMetadata {
                source_schema: self.schema_version,
                source_kind: event_kind.to_owned(),
            },
        })
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

/// Frozen codecs for released tool-result and context-usage event families.
pub mod historical_event_families {
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
        ShellRun { result: ShellRunResultDto },
        FileChange { result: FileChangeResultDto },
    }

    #[derive(Debug, serde::Serialize, Deserialize)]
    #[serde(tag = "mode", rename_all = "snake_case")]
    enum ShellRunResultDto {
        Terminal {
            #[serde(default)]
            exit_code: Option<i32>,
            #[serde(default)]
            timed_out: bool,
            #[serde(default)]
            cancelled: bool,
            #[serde(default)]
            duration_ms: Option<u64>,
            #[serde(default)]
            output_tail: String,
            #[serde(default)]
            output_truncated: bool,
            #[serde(default)]
            output_bytes: Option<u64>,
            #[serde(default)]
            retained_output_bytes: Option<u64>,
            #[serde(default = "default_terminal_columns")]
            columns: u16,
            #[serde(default = "default_terminal_rows")]
            rows: u16,
        },
        Captured {
            #[serde(default)]
            exit_code: Option<i32>,
            #[serde(default)]
            timed_out: bool,
            #[serde(default)]
            cancelled: bool,
            #[serde(default)]
            duration_ms: Option<u64>,
            #[serde(default)]
            stdout: String,
            #[serde(default)]
            stderr: String,
            #[serde(default)]
            stdout_truncated: bool,
            #[serde(default)]
            stderr_truncated: bool,
            #[serde(default)]
            stdout_bytes: Option<u64>,
            #[serde(default)]
            stderr_bytes: Option<u64>,
        },
    }

    const fn default_terminal_columns() -> u16 {
        80
    }

    const fn default_terminal_rows() -> u16 {
        24
    }

    #[derive(Debug, serde::Serialize, Deserialize)]
    struct FileChangeResultDto {
        tool_name: String,
        summary: String,
        #[serde(default)]
        path: Option<String>,
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
                Self::ShellRun { result } => ToolInvocationResult::Json {
                    value: serde_json::json!({
                        "type": "shell_run",
                        "result": result,
                    })
                    .to_string(),
                },
                Self::FileChange { result } => ToolInvocationResult::Json {
                    value: serde_json::json!({
                        "type": "file_change",
                        "result": result,
                    })
                    .to_string(),
                },
            }
        }
    }

    #[derive(Debug, Deserialize)]
    struct HistoricalModelInvocationIdentity {
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

    impl From<HistoricalModelInvocationIdentity> for ModelRequestIdentity {
        fn from(value: HistoricalModelInvocationIdentity) -> Self {
            Self {
                provider_plugin_id: value.provider_plugin_id,
                requested_model_id: value.requested_model_id,
                effective_model_id: value.effective_model_id,
                request_id: value.request_id,
                model_turn_id: value.model_turn_id,
                round: value.round,
                request_fingerprint: value.request_fingerprint,
                effective_auth_profile: value.effective_auth_profile,
                context_format_version: value.context_format_version,
                compatibility_key: value.compatibility_key,
                context_epoch: value.context_epoch,
            }
        }
    }

    #[derive(Debug, Deserialize)]
    struct HistoricalContextUsageSnapshot {
        #[serde(default)]
        invocation: Option<HistoricalModelInvocationIdentity>,
        #[serde(default)]
        provider_plugin_id: Option<String>,
        #[serde(default)]
        model_id: Option<String>,
        #[serde(default)]
        input_tokens: Option<u64>,
        context_through_sequence: u64,
        #[serde(default)]
        request_id: Option<String>,
        #[serde(default)]
        model_turn_id: Option<String>,
        #[serde(default)]
        round: Option<u32>,
        #[serde(default)]
        request_fingerprint: Option<String>,
        #[serde(default)]
        auth_profile: Option<String>,
        #[serde(default)]
        context_format_version: Option<u16>,
        #[serde(default)]
        compatibility_key: Option<String>,
        #[serde(default)]
        context_epoch: u64,
        #[serde(default)]
        estimated_input_tokens: Option<u64>,
        #[serde(default)]
        context_input_tokens: Option<u64>,
        #[serde(default)]
        local_request_estimate_tokens: Option<u64>,
        source: ContextUsageSource,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum ContextUsageSource {
        Provider,
        Estimated,
    }

    enum ContextUsageConversion {
        Active(Box<RequestContextObservation>),
        Inert,
    }

    impl HistoricalContextUsageSnapshot {
        fn into_current(self) -> ContextUsageConversion {
            let Some(context_input_tokens) = self.context_input_tokens.or(self.input_tokens) else {
                return ContextUsageConversion::Inert;
            };
            let local_estimate_tokens = self
                .local_request_estimate_tokens
                .or(self.estimated_input_tokens)
                .unwrap_or(context_input_tokens);
            let request = if let Some(invocation) = self.invocation {
                invocation.into()
            } else {
                let (
                    Some(provider_plugin_id),
                    Some(effective_model_id),
                    Some(request_id),
                    Some(model_turn_id),
                    Some(round),
                    Some(request_fingerprint),
                ) = (
                    self.provider_plugin_id,
                    self.model_id,
                    self.request_id,
                    self.model_turn_id,
                    self.round,
                    self.request_fingerprint,
                )
                else {
                    return ContextUsageConversion::Inert;
                };
                ModelRequestIdentity {
                    provider_plugin_id,
                    requested_model_id: Some(effective_model_id.clone()),
                    effective_model_id,
                    request_id,
                    model_turn_id,
                    round,
                    request_fingerprint,
                    effective_auth_profile: self.auth_profile,
                    context_format_version: self.context_format_version,
                    compatibility_key: self.compatibility_key,
                    context_epoch: self.context_epoch,
                }
            };
            let context_tokens = match self.source {
                ContextUsageSource::Provider => {
                    RequestContextTokenCount::ProviderExact(context_input_tokens)
                }
                ContextUsageSource::Estimated => {
                    RequestContextTokenCount::Estimated(context_input_tokens)
                }
            };
            ContextUsageConversion::Active(Box::new(RequestContextObservation {
                request,
                context_through_sequence: self.context_through_sequence,
                context_tokens,
                local_estimate: LocalContextEstimate {
                    tokens: local_estimate_tokens,
                    algorithm_version: 1,
                },
            }))
        }
    }

    pub fn decode_tool_call_finished(
        envelope: &HistoricalEnvelope,
    ) -> Result<HistoricalDecode, HistoricalSessionEventError> {
        let (event_kind, event_payload) = source_kind(envelope)?;
        if event_kind != "tool_call_finished" {
            return Err(HistoricalSessionEventError::UnsupportedEventKind {
                schema_version: envelope.schema_version,
                event_kind: event_kind.to_owned(),
            });
        }
        let metadata = HistoricalEventMetadata {
            source_schema: envelope.schema_version,
            source_kind: event_kind.to_owned(),
        };
        let source =
            serde_json::from_value::<ToolCallFinished>(event_payload.clone()).map_err(|error| {
                HistoricalSessionEventError::InvalidEvent {
                    schema_version: envelope.schema_version,
                    event_kind: event_kind.to_owned(),
                    reason: error.to_string(),
                }
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

    pub fn decode_context_usage_observed(
        envelope: &HistoricalEnvelope,
    ) -> Result<HistoricalDecode, HistoricalSessionEventError> {
        let (event_kind, event_payload) = source_kind(envelope)?;
        if event_kind != "context_usage_observed" {
            return Err(HistoricalSessionEventError::UnsupportedEventKind {
                schema_version: envelope.schema_version,
                event_kind: event_kind.to_owned(),
            });
        }
        let metadata = HistoricalEventMetadata {
            source_schema: envelope.schema_version,
            source_kind: event_kind.to_owned(),
        };
        let snapshot = event_payload.get("snapshot").cloned().ok_or_else(|| {
            HistoricalSessionEventError::InvalidEvent {
                schema_version: envelope.schema_version,
                event_kind: event_kind.to_owned(),
                reason: "missing snapshot".to_owned(),
            }
        })?;
        let snapshot = serde_json::from_value::<HistoricalContextUsageSnapshot>(snapshot).map_err(
            |error| HistoricalSessionEventError::InvalidEvent {
                schema_version: envelope.schema_version,
                event_kind: event_kind.to_owned(),
                reason: error.to_string(),
            },
        )?;
        let observation = match snapshot.into_current() {
            ContextUsageConversion::Active(observation) => *observation,
            ContextUsageConversion::Inert => {
                return Ok(HistoricalDecode::RetiredKnown {
                    event: envelope.materialize(SessionEventKind::InertHistory {
                        event_type: event_kind.to_owned(),
                        payload: event_payload.clone(),
                    }),
                    metadata,
                });
            }
        };
        Ok(HistoricalDecode::Converted {
            event: envelope.materialize(SessionEventKind::RequestContextObserved { observation }),
            metadata,
        })
    }

    pub fn decode_schema_28(
        envelope: &HistoricalEnvelope,
    ) -> Result<HistoricalDecode, HistoricalSessionEventError> {
        let (event_kind, event_payload) = source_kind(envelope)?;
        let metadata = HistoricalEventMetadata {
            source_schema: envelope.schema_version,
            source_kind: event_kind.to_owned(),
        };
        match event_kind {
            "tool_call_finished" => decode_tool_call_finished(envelope),
            "context_usage_observed" => decode_context_usage_observed(envelope),
            "tool_invocation_stream" => Ok(HistoricalDecode::RetiredKnown {
                event: envelope.materialize(SessionEventKind::InertHistory {
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
