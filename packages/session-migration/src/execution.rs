//! Historical canonical normalization and conversion into current session events.

use crate::classification::{HistoricalDecode, HistoricalEventMetadata};
use crate::codec::{HistoricalEnvelope, schema_28};
use bcode_session_models::SessionEvent;
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use thiserror::Error;

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

/// Stable migration metric counters selected by migration-owned historical policy.
pub mod metric {
    /// Historical tool completion converted into a current result record.
    pub const CONVERTED_TOOL_CALL_FINISHED: &str =
        "session.migration.converted_tool_call_finished_events_total";
    /// Historical context usage converted into a current request-context observation.
    pub const CONVERTED_CONTEXT_USAGE_OBSERVED: &str =
        "session.migration.converted_context_usage_observed_events_total";
    /// Historical tool invocation stream retained as inert current history.
    pub const RETIRED_TOOL_INVOCATION_STREAM: &str =
        "session.migration.retired_tool_stream_events_total";
}

/// Audit totals accumulated while normalizing canonical migration events.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CanonicalNormalizationSummary {
    converted_events: BTreeMap<String, u64>,
    retired_known_events: BTreeMap<String, u64>,
}

impl CanonicalNormalizationSummary {
    /// Record one normalized event without exposing historical classification policy to callers.
    pub fn record(&mut self, normalized: &NormalizedCanonicalEvent) {
        let Some(metadata) = normalized.historical.as_ref() else {
            return;
        };
        let counts = if normalized.retired_known {
            &mut self.retired_known_events
        } else {
            &mut self.converted_events
        };
        let count = counts
            .entry(format!(
                "{}:{}",
                metadata.source_schema, metadata.source_kind
            ))
            .or_insert(0_u64);
        *count = count.saturating_add(1);
    }

    /// Return converted event counts keyed by stable historical source identity.
    #[must_use]
    pub const fn converted_events(&self) -> &BTreeMap<String, u64> {
        &self.converted_events
    }

    /// Return retired-known event counts keyed by stable historical source identity.
    #[must_use]
    pub const fn retired_known_events(&self) -> &BTreeMap<String, u64> {
        &self.retired_known_events
    }

    /// Consume the summary into converted and retired-known count maps.
    #[must_use]
    pub fn into_counts(self) -> (BTreeMap<String, u64>, BTreeMap<String, u64>) {
        (self.converted_events, self.retired_known_events)
    }
}

/// Result of normalizing one canonical payload for migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedCanonicalEvent {
    /// Current event ready for canonical replacement and projector ingestion.
    pub event: SessionEvent,
    /// Historical source metadata when explicit conversion or inert preservation occurred.
    pub historical: Option<HistoricalEventMetadata>,
    /// Whether the historical event is intentionally inert current history.
    pub retired_known: bool,
    /// Stable metric counter selected by migration-owned classification policy, when applicable.
    pub metric_counter: Option<&'static str>,
}

/// Normalize one canonical payload into the final current representation.
///
/// `decode_current` must be the strict current persistence decoder. Historical adapters are used
/// only after that decoder rejects the payload. The resulting event schema is always the supplied
/// current schema.
///
/// # Errors
///
/// Returns an error when historical decoding fails.
pub fn normalize_canonical_event(
    payload: &str,
    current_schema: u16,
    decode_current: impl FnOnce(&str) -> Result<SessionEvent, String>,
) -> Result<NormalizedCanonicalEvent, HistoricalSessionEventError> {
    let decoded = decode_for_migration(payload, decode_current)?;
    let historical = decoded.metadata().cloned();
    let retired_known = decoded.is_retired_known();
    let metric_counter = historical.as_ref().and_then(|metadata| {
        match (metadata.source_kind.as_str(), retired_known) {
            ("tool_call_finished", false) => Some(metric::CONVERTED_TOOL_CALL_FINISHED),
            ("context_usage_observed", false) => Some(metric::CONVERTED_CONTEXT_USAGE_OBSERVED),
            ("tool_invocation_stream", true) => Some(metric::RETIRED_TOOL_INVOCATION_STREAM),
            _ => None,
        }
    });
    let mut event = decoded.into_event();
    event.schema_version = current_schema;
    Ok(NormalizedCanonicalEvent {
        event,
        historical,
        retired_known,
        metric_counter,
    })
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
#[cfg(test)]
#[must_use]
fn historical_conversion_counts<'a>(
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
fn decode_for_migration(
    payload: &str,
    decode_current: impl FnOnce(&str) -> Result<SessionEvent, String>,
) -> Result<HistoricalDecode, HistoricalSessionEventError> {
    if let Ok(event) = decode_current(payload) {
        return Ok(HistoricalDecode::Current(event));
    }

    let envelope = serde_json::from_str::<HistoricalEnvelope>(payload)?;
    match envelope.schema_version() {
        28 => schema_28::decode(&envelope),
        schema_version => Err(HistoricalSessionEventError::UnsupportedSchema { schema_version }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_models::{RequestContextTokenCount, SessionEventKind, ToolInvocationResult};
    use serde::Deserialize;
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;

    const SESSION_ID: &str = "00000000-0000-0000-0000-000000000001";

    #[derive(Debug, Deserialize)]
    struct FixtureEnvelope {
        schema_version: u16,
        sequence: u64,
        kind: BTreeMap<String, serde_json::Value>,
    }

    #[test]
    fn fixture_manifest_enforces_complete_sanitized_inventory() {
        let fixture_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let manifest = crate::load_released_fixture_manifest(&fixture_root)
            .expect("fixture manifest and disk inventory");
        assert_eq!(manifest.format_version, 1);

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
            assert!(!fixture.covered_authoritative_records.is_empty());

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
    fn canonical_normalization_always_materializes_the_requested_current_schema() {
        let current_payload = format!(
            r#"{{"schema_version":39,"sequence":1,"session_id":"{SESSION_ID}","kind":{{"session_created":{{"summary":{{"id":"{SESSION_ID}","title":"fixture","cwd":"/tmp","created_at_ms":1,"updated_at_ms":1}}}}}}}}"#
        );
        let normalized = normalize_canonical_event(&current_payload, 40, |payload| {
            serde_json::from_str(payload).map_err(|error| error.to_string())
        })
        .expect("normalize current-compatible event");
        assert_eq!(normalized.event.schema_version, 40);
        assert!(normalized.historical.is_none());
        assert!(!normalized.retired_known);
        assert!(normalized.metric_counter.is_none());

        let historical_payload = format!(
            r#"{{"schema_version":28,"sequence":8,"timestamp_ms":9,"session_id":"{SESSION_ID}","kind":{{"context_usage_observed":{{"snapshot":{{"provider_plugin_id":"provider","model_id":"model","input_tokens":123,"context_through_sequence":4,"request_id":"request","model_turn_id":"turn","round":0,"request_fingerprint":"fingerprint","auth_profile":"profile","estimated_input_tokens":120,"context_format_version":null,"compatibility_key":null,"source":"estimated"}}}}}}}}"#
        );
        let normalized = normalize_canonical_event(&historical_payload, 40, reject_current)
            .expect("normalize historical event");
        assert_eq!(normalized.event.schema_version, 40);
        assert_eq!(normalized.historical.expect("metadata").source_schema, 28);
        assert!(!normalized.retired_known);
        assert_eq!(
            normalized.metric_counter,
            Some(metric::CONVERTED_CONTEXT_USAGE_OBSERVED)
        );
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
