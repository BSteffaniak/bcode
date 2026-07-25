use bcode_session_models::{
    LocalContextEstimate, ModelRequestIdentity, RequestContextObservation,
    RequestContextTokenCount, SessionEvent, SessionEventKind, SessionEventProvenance, SessionId,
    ToolArtifact, ToolArtifactRef, ToolInvocationResult, ToolInvocationResultRecord,
    current_unix_timestamp_ms,
};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use thiserror::Error;

/// Source identity retained for one converted historical event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoricalEventMetadata {
    /// Durable event schema that produced the source payload.
    pub source_schema: u16,
    /// Durable event-kind name in the source payload.
    pub source_kind: String,
}

/// Migration classification for one recognized historical event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoricalDecode {
    /// The payload already uses the current durable representation.
    Current(SessionEvent),
    /// Historical semantics were converted into an active current event.
    Converted {
        /// Current event produced from the historical payload.
        event: SessionEvent,
        /// Source schema and kind used for audit accounting.
        metadata: HistoricalEventMetadata,
    },
    /// Historical semantics are recognized but intentionally inert now.
    RetiredKnown {
        /// Current inert event retaining the historical payload.
        event: SessionEvent,
        /// Source schema and kind used for audit accounting.
        metadata: HistoricalEventMetadata,
    },
}

impl HistoricalDecode {
    /// Return the current event materialized by this classification.
    #[must_use]
    pub const fn event(&self) -> &SessionEvent {
        match self {
            Self::Current(event)
            | Self::Converted { event, .. }
            | Self::RetiredKnown { event, .. } => event,
        }
    }

    /// Return source metadata when conversion handled a historical event.
    #[must_use]
    pub const fn metadata(&self) -> Option<&HistoricalEventMetadata> {
        match self {
            Self::Current(_) => None,
            Self::Converted { metadata, .. } | Self::RetiredKnown { metadata, .. } => {
                Some(metadata)
            }
        }
    }

    /// Return whether this event has active current semantics.
    #[must_use]
    pub const fn is_retired_known(&self) -> bool {
        matches!(self, Self::RetiredKnown { .. })
    }

    /// Consume the classification and return its current event.
    #[must_use]
    pub fn into_event(self) -> SessionEvent {
        match self {
            Self::Current(event)
            | Self::Converted { event, .. }
            | Self::RetiredKnown { event, .. } => event,
        }
    }
}

/// Failure to classify a historical canonical event for writable migration.
#[derive(Debug, Error)]
pub enum HistoricalSessionEventError {
    /// The durable JSON envelope is malformed.
    #[error("historical session event JSON is malformed: {0}")]
    Json(#[from] serde_json::Error),
    /// The durable schema is not a released historical schema supported by this build.
    #[error("unsupported historical session event schema {schema_version}")]
    UnsupportedSchema {
        /// Durable schema declared by the source event.
        schema_version: u16,
    },
    /// The event kind is not a released historical shape supported by this build.
    #[error("unsupported historical session event kind {event_kind:?} at schema {schema_version}")]
    UnsupportedEventKind {
        /// Durable schema declared by the source event.
        schema_version: u16,
        /// Durable source event-kind name.
        event_kind: String,
    },
    /// A historical event could not be converted without inventing required semantics.
    #[error(
        "invalid historical session event kind {event_kind:?} at schema {schema_version}: {reason}"
    )]
    InvalidEvent {
        /// Durable schema declared by the source event.
        schema_version: u16,
        /// Durable source event-kind name.
        event_kind: String,
        /// Exact conversion failure.
        reason: String,
    },
}

#[derive(Debug, Deserialize)]
struct HistoricalEnvelope {
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

#[derive(Debug, Deserialize)]
struct LegacyToolCallFinished {
    tool_call_id: String,
    result: String,
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    semantic_result: Option<LegacyToolInvocationResult>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum LegacyToolInvocationResult {
    Text { text: String },
    Json { value: String },
    Artifact { artifact: Box<Schema28ToolArtifact> },
}

#[derive(Debug, Deserialize)]
struct Schema28ToolArtifact {
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
    refs: Vec<Schema28ToolArtifactRef>,
}

impl From<Schema28ToolArtifact> for ToolArtifact {
    fn from(value: Schema28ToolArtifact) -> Self {
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
struct Schema28ToolArtifactRef {
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

impl From<Schema28ToolArtifactRef> for ToolArtifactRef {
    fn from(value: Schema28ToolArtifactRef) -> Self {
        Self {
            key: value.key,
            content_type: value.content_type,
            storage_uri: value.storage_uri,
            byte_len: value.byte_len,
            metadata: value.metadata,
        }
    }
}

impl LegacyToolInvocationResult {
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
struct LegacyFlatContextUsageSnapshot {
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
    source: LegacyContextUsageSource,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LegacyContextUsageSource {
    Provider,
    Estimated,
}

impl LegacyFlatContextUsageSnapshot {
    fn into_current(self) -> RequestContextObservation {
        let context_tokens = match self.source {
            LegacyContextUsageSource::Provider => {
                RequestContextTokenCount::ProviderExact(self.input_tokens)
            }
            LegacyContextUsageSource::Estimated => {
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

/// Compute the stable digest used for ordered canonical migration audit.
#[must_use]
pub fn ordered_payload_digest<'a>(payloads: impl IntoIterator<Item = &'a str>) -> String {
    let mut digest = Sha256::new();
    for payload in payloads {
        let length = u64::try_from(payload.len()).unwrap_or(u64::MAX);
        digest.update(length.to_le_bytes());
        digest.update(payload.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

/// Accumulate converted and retired-known audit counts.
#[must_use]
pub fn historical_conversion_counts<'a>(
    decoded: impl IntoIterator<Item = &'a HistoricalDecode>,
) -> (BTreeMap<String, u64>, BTreeMap<String, u64>) {
    let mut converted = BTreeMap::new();
    let mut retired_known = BTreeMap::new();
    for decoded in decoded {
        let Some(metadata) = decoded.metadata() else {
            continue;
        };
        let key = format!("{}:{}", metadata.source_schema, metadata.source_kind);
        let counts = if decoded.is_retired_known() {
            &mut retired_known
        } else {
            &mut converted
        };
        let count = counts.entry(key).or_insert(0_u64);
        *count = (*count).saturating_add(1);
    }
    (converted, retired_known)
}

/// Decode one canonical payload for writable migration.
///
/// `decode_current` must be the strict current persistence decoder. Historical
/// adapters are consulted only after that decoder rejects the payload.
///
/// # Errors
///
/// Returns an error when the stable envelope is malformed, the source kind was
/// never released by a supported Bcode writer, or conversion would require
/// inventing semantics absent from the source payload.
pub fn decode_for_migration(
    payload: &str,
    decode_current: impl FnOnce(&str) -> Result<SessionEvent, String>,
) -> Result<HistoricalDecode, HistoricalSessionEventError> {
    if let Ok(event) = decode_current(payload) {
        return Ok(HistoricalDecode::Current(event));
    }

    let envelope = serde_json::from_str::<HistoricalEnvelope>(payload)?;
    match envelope.schema_version {
        28 => schema_28::decode(&envelope),
        schema_version => Err(HistoricalSessionEventError::UnsupportedSchema { schema_version }),
    }
}

mod schema_28 {
    use super::{
        HistoricalDecode, HistoricalEnvelope, HistoricalSessionEventError, decode_released_event,
    };

    pub(super) fn decode(
        envelope: &HistoricalEnvelope,
    ) -> Result<HistoricalDecode, HistoricalSessionEventError> {
        decode_released_event(envelope)
    }
}

fn decode_released_event(
    envelope: &HistoricalEnvelope,
) -> Result<HistoricalDecode, HistoricalSessionEventError> {
    let (event_kind, event_payload) = source_kind(envelope)?;
    let metadata = HistoricalEventMetadata {
        source_schema: envelope.schema_version,
        source_kind: event_kind.to_owned(),
    };
    match event_kind {
        "tool_call_finished" => {
            let source = serde_json::from_value::<LegacyToolCallFinished>(event_payload.clone())
                .map_err(|error| HistoricalSessionEventError::InvalidEvent {
                    schema_version: envelope.schema_version,
                    event_kind: event_kind.to_owned(),
                    reason: error.to_string(),
                })?;
            Ok(HistoricalDecode::Converted {
                event: envelope.materialize(SessionEventKind::ToolInvocationResultRecorded {
                    record: ToolInvocationResultRecord {
                        invocation_id: source.tool_call_id,
                        model_output: source.result,
                        is_error: source.is_error,
                        presentation: None,
                        result: source
                            .semantic_result
                            .map(LegacyToolInvocationResult::into_current),
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
            let snapshot = serde_json::from_value::<LegacyFlatContextUsageSnapshot>(snapshot)
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    const SESSION_ID: &str = "00000000-0000-0000-0000-000000000001";

    #[derive(Debug, Deserialize)]
    struct FixtureManifest {
        format_version: u32,
        fixtures: Vec<FixtureManifestEntry>,
    }

    #[derive(Debug, Deserialize)]
    struct FixtureManifestEntry {
        path: PathBuf,
        source_writer_epochs: Vec<u64>,
        event_schemas: Vec<u16>,
        expected_event_count: usize,
        expected_classifications: FixtureClassificationCounts,
        covered_event_kinds: Vec<String>,
    }

    #[derive(Debug, Deserialize)]
    struct FixtureClassificationCounts {
        converted: usize,
        retired_known: usize,
        current_passthrough: usize,
    }

    #[derive(Debug, Deserialize)]
    struct FixtureEnvelope {
        schema_version: u16,
        sequence: u64,
        kind: BTreeMap<String, serde_json::Value>,
    }

    #[test]
    fn fixture_manifest_enforces_complete_sanitized_inventory() {
        let manifest: FixtureManifest =
            serde_json::from_str(include_str!("../fixtures/manifest.json"))
                .expect("fixture manifest");
        assert_eq!(manifest.format_version, 1);
        assert!(!manifest.fixtures.is_empty());

        let fixture_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let listed_paths = manifest
            .fixtures
            .iter()
            .map(|fixture| fixture.path.clone())
            .collect::<BTreeSet<_>>();
        let actual_paths = std::fs::read_dir(fixture_root.join("stores"))
            .expect("fixture directory")
            .map(|entry| PathBuf::from("stores").join(entry.expect("fixture entry").file_name()))
            .collect::<BTreeSet<_>>();
        assert_eq!(
            listed_paths, actual_paths,
            "fixture manifest must be exhaustive"
        );

        for fixture in manifest.fixtures {
            assert!(!fixture.source_writer_epochs.is_empty());
            let contents =
                std::fs::read_to_string(fixture_root.join(&fixture.path)).expect("listed fixture");
            let payloads = contents.lines().collect::<Vec<_>>();
            assert_eq!(payloads.len(), fixture.expected_event_count);
            let envelopes = payloads
                .iter()
                .map(|payload| serde_json::from_str::<FixtureEnvelope>(payload))
                .collect::<Result<Vec<_>, _>>()
                .expect("fixture envelopes");
            assert_eq!(
                envelopes
                    .iter()
                    .map(|event| event.sequence)
                    .collect::<Vec<_>>(),
                (0..u64::try_from(envelopes.len()).expect("fixture length")).collect::<Vec<_>>()
            );
            assert_eq!(
                envelopes
                    .iter()
                    .map(|event| event.schema_version)
                    .collect::<BTreeSet<_>>(),
                fixture.event_schemas.into_iter().collect()
            );
            assert_eq!(
                envelopes
                    .iter()
                    .flat_map(|event| event.kind.keys().cloned())
                    .collect::<BTreeSet<_>>(),
                fixture.covered_event_kinds.into_iter().collect()
            );

            let historical_payloads = payloads
                .iter()
                .zip(&envelopes)
                .filter(|(_, event)| {
                    event.kind.keys().any(|kind| {
                        matches!(
                            kind.as_str(),
                            "tool_invocation_stream"
                                | "tool_call_finished"
                                | "context_usage_observed"
                        )
                    })
                })
                .map(|(payload, _)| *payload)
                .collect::<Vec<_>>();
            assert_eq!(
                payloads.len() - historical_payloads.len(),
                fixture.expected_classifications.current_passthrough
            );
            let decoded = historical_payloads
                .iter()
                .map(|payload| decode_for_migration(payload, reject_current))
                .collect::<Result<Vec<_>, _>>()
                .expect("fixture migration classifications");
            let converted = decoded
                .iter()
                .filter(|event| matches!(event, HistoricalDecode::Converted { .. }))
                .count();
            let retired_known = decoded
                .iter()
                .filter(|event| matches!(event, HistoricalDecode::RetiredKnown { .. }))
                .count();
            assert_eq!(converted, fixture.expected_classifications.converted);
            assert_eq!(
                retired_known,
                fixture.expected_classifications.retired_known
            );
        }
    }

    fn reject_current(_: &str) -> Result<SessionEvent, String> {
        Err("not current".to_owned())
    }

    #[test]
    fn schema_28_artifact_result_decodes_through_frozen_dto() {
        let payload = format!(
            r#"{{"schema_version":28,"sequence":8,"timestamp_ms":9,"session_id":"{SESSION_ID}","kind":{{"tool_call_finished":{{"tool_call_id":"call-artifact","result":"artifact result","semantic_result":{{"type":"artifact","artifact":{{"artifact_id":"artifact-1","producer_plugin_id":"fixture.plugin","schema":"fixture.artifact","schema_version":2,"tool_call_id":"call-artifact","title":"Fixture","metadata":{{"safe":true}},"refs":[{{"key":"body","content_type":"text/plain","storage_uri":"file:///tmp/fixture","byte_len":7,"metadata":{{"encoding":"utf8"}}}}]}}}}}}}}}}"#
        );
        let decoded = decode_for_migration(&payload, reject_current).expect("historical decode");
        let HistoricalDecode::Converted { event, .. } = decoded else {
            panic!("expected converted event");
        };
        let SessionEventKind::ToolInvocationResultRecorded { record } = event.kind else {
            panic!("expected current invocation result");
        };
        let Some(ToolInvocationResult::Artifact { artifact }) = record.result else {
            panic!("expected artifact result");
        };
        assert_eq!(artifact.artifact_id, "artifact-1");
        assert_eq!(artifact.producer_plugin_id, "fixture.plugin");
        assert_eq!(artifact.refs.len(), 1);
        assert_eq!(artifact.refs[0].key, "body");
        assert_eq!(artifact.refs[0].byte_len, Some(7));
    }

    #[test]
    fn schema_28_tool_completion_converts_to_current_result_record() {
        let payload = format!(
            r#"{{"schema_version":28,"sequence":7,"timestamp_ms":9,"session_id":"{SESSION_ID}","kind":{{"tool_call_finished":{{"tool_call_id":"call-1","result":"done","is_error":false,"semantic_result":{{"type":"json","value":"{{\"ok\":true}}"}}}}}}}}"#
        );
        let decoded = decode_for_migration(&payload, reject_current).expect("historical decode");
        let HistoricalDecode::Converted { event, metadata } = decoded else {
            panic!("expected converted event");
        };
        assert_eq!(metadata.source_schema, 28);
        assert_eq!(metadata.source_kind, "tool_call_finished");
        let SessionEventKind::ToolInvocationResultRecorded { record } = event.kind else {
            panic!("expected current invocation result");
        };
        assert_eq!(record.invocation_id, "call-1");
        assert_eq!(record.model_output, "done");
        assert_eq!(
            record.result,
            Some(ToolInvocationResult::Json {
                value: r#"{"ok":true}"#.to_owned()
            })
        );
    }

    #[test]
    fn schema_28_flat_context_usage_converts_to_current_observation() {
        let payload = format!(
            r#"{{"schema_version":28,"sequence":8,"timestamp_ms":9,"session_id":"{SESSION_ID}","kind":{{"context_usage_observed":{{"snapshot":{{"provider_plugin_id":"provider","model_id":"model","input_tokens":123,"context_through_sequence":4,"request_id":"request","model_turn_id":"turn","round":0,"request_fingerprint":"fingerprint","auth_profile":"profile","estimated_input_tokens":120,"context_format_version":null,"compatibility_key":null,"source":"estimated"}}}}}}}}"#
        );
        let decoded = decode_for_migration(&payload, reject_current).expect("historical decode");
        let HistoricalDecode::Converted { event, .. } = decoded else {
            panic!("expected converted event");
        };
        let SessionEventKind::RequestContextObserved { observation } = event.kind else {
            panic!("expected current context observation");
        };
        assert_eq!(observation.request.effective_model_id, "model");
        assert_eq!(
            observation.request.requested_model_id.as_deref(),
            Some("model")
        );
        assert_eq!(
            observation.request.effective_auth_profile.as_deref(),
            Some("profile")
        );
        assert_eq!(observation.context_through_sequence, 4);
        assert_eq!(
            observation.context_tokens,
            RequestContextTokenCount::Estimated(123)
        );
        assert_eq!(observation.local_estimate.tokens, 120);
    }

    #[test]
    fn schema_28_stream_status_is_recognized_inert_history() {
        let payload = format!(
            r#"{{"schema_version":28,"sequence":44,"timestamp_ms":9,"session_id":"{SESSION_ID}","kind":{{"tool_invocation_stream":{{"event":{{"status":{{"tool_call_id":"call-1","sequence":1,"message":"working"}}}}}}}}}}"#
        );
        let decoded = decode_for_migration(&payload, reject_current).expect("historical decode");
        let HistoricalDecode::RetiredKnown { event, metadata } = decoded else {
            panic!("expected retired known event");
        };
        assert_eq!(metadata.source_kind, "tool_invocation_stream");
        let SessionEventKind::OpaqueEvent {
            event_type,
            payload,
        } = event.kind
        else {
            panic!("expected current inert event");
        };
        assert_eq!(event_type, "tool_invocation_stream");
        assert_eq!(payload["event"]["status"]["message"], "working");
    }

    #[test]
    fn schema_28_store_fixture_classifies_affected_historical_events() {
        let fixture = include_str!("../fixtures/stores/schema-28-tool-context.jsonl");
        let decoded = fixture
            .lines()
            .skip(2)
            .map(|payload| decode_for_migration(payload, reject_current))
            .collect::<Result<Vec<_>, _>>()
            .expect("affected schema-28 fixture events should classify");
        assert_eq!(decoded.len(), 3);
        assert!(matches!(decoded[0], HistoricalDecode::RetiredKnown { .. }));
        assert!(matches!(decoded[1], HistoricalDecode::Converted { .. }));
        assert!(matches!(decoded[2], HistoricalDecode::Converted { .. }));
        for (expected_sequence, decoded) in (2_u64..).zip(&decoded) {
            assert_eq!(decoded.event().sequence, expected_sequence);
            assert_eq!(
                decoded.event().session_id.to_string(),
                "00000000-0000-0000-0000-000000000001"
            );
        }
    }

    #[test]
    fn exact_schema_28_codec_conversions_preserve_identity_and_semantics() {
        let fixture = include_str!("../fixtures/stores/schema-28-tool-context.jsonl");
        let decoded = fixture
            .lines()
            .skip(2)
            .map(|payload| decode_for_migration(payload, reject_current))
            .collect::<Result<Vec<_>, _>>()
            .expect("schema-28 fixture should decode");
        for (expected_sequence, decoded) in (2_u64..).zip(&decoded) {
            let event = decoded.event();
            assert_eq!(event.sequence, expected_sequence);
            assert_eq!(event.timestamp_ms, expected_sequence + 1);
            assert_eq!(event.session_id.to_string(), SESSION_ID);
            assert!(event.provenance.is_none());
        }
        assert!(matches!(
            &decoded[0],
            HistoricalDecode::RetiredKnown { event, metadata }
                if metadata.source_schema == 28
                    && metadata.source_kind == "tool_invocation_stream"
                    && matches!(
                        &event.kind,
                        SessionEventKind::OpaqueEvent { event_type, payload }
                            if event_type == "tool_invocation_stream"
                                && payload["event"]["status"]["tool_call_id"] == "call-1"
                                && payload["event"]["status"]["sequence"] == 1
                                && payload["event"]["status"]["message"]
                                    == "read: loading README.md"
                    )
        ));
        assert!(matches!(
            &decoded[1],
            HistoricalDecode::Converted { event, metadata }
                if metadata.source_schema == 28
                    && metadata.source_kind == "tool_call_finished"
                    && matches!(
                        &event.kind,
                        SessionEventKind::ToolInvocationResultRecorded { record }
                            if record.invocation_id == "call-1"
                                && record.model_output == "fixture result"
                                && !record.is_error
                                && matches!(
                                    &record.result,
                                    Some(ToolInvocationResult::Text { text })
                                        if text == "fixture result"
                                )
                    )
        ));
        assert!(matches!(
            &decoded[2],
            HistoricalDecode::Converted { event, metadata }
                if metadata.source_schema == 28
                    && metadata.source_kind == "context_usage_observed"
                    && matches!(
                        &event.kind,
                        SessionEventKind::RequestContextObserved { observation }
                            if observation.request.provider_plugin_id
                                == "bcode.openai-compatible"
                                && observation.request.effective_model_id == "fixture-model"
                                && observation.context_through_sequence == 3
                                && observation.context_tokens
                                    == RequestContextTokenCount::Estimated(123)
                                && observation.local_estimate.tokens == 120
                    )
        ));
    }

    #[test]
    fn ordered_digest_and_conversion_counts_are_deterministic() {
        let fixture = include_str!("../fixtures/stores/schema-28-tool-context.jsonl");
        let payloads = fixture.lines().skip(2).collect::<Vec<_>>();
        let decoded = payloads
            .iter()
            .map(|payload| decode_for_migration(payload, reject_current))
            .collect::<Result<Vec<_>, _>>()
            .expect("fixture decode");
        let digest = ordered_payload_digest(payloads.iter().copied());
        assert_eq!(digest.len(), 64);
        assert_eq!(digest, ordered_payload_digest(payloads.iter().copied()));
        let (converted, retired) = historical_conversion_counts(&decoded);
        assert_eq!(converted.get("28:tool_call_finished"), Some(&1));
        assert_eq!(converted.get("28:context_usage_observed"), Some(&1));
        assert_eq!(retired.get("28:tool_invocation_stream"), Some(&1));
    }

    #[test]
    fn historical_codec_never_applies_schema_28_rules_to_other_schemas() {
        let payload = format!(
            r#"{{"schema_version":27,"sequence":1,"session_id":"{SESSION_ID}","kind":{{"tool_call_finished":{{"tool_call_id":"call","result":"done"}}}}}}"#
        );
        assert!(matches!(
            decode_for_migration(&payload, reject_current),
            Err(HistoricalSessionEventError::UnsupportedSchema { schema_version: 27 })
        ));
    }

    #[test]
    fn unknown_historical_kind_fails_writable_migration() {
        let payload = format!(
            r#"{{"schema_version":28,"sequence":1,"session_id":"{SESSION_ID}","kind":{{"unknown_released_shape":{{}}}}}}"#
        );
        assert!(matches!(
            decode_for_migration(&payload, reject_current),
            Err(HistoricalSessionEventError::UnsupportedEventKind { .. })
        ));
    }
}
