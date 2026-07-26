//! Historical canonical normalization and conversion into current session events.

use crate::classification::{HistoricalDecode, HistoricalEventMetadata};
use crate::codec::{HistoricalEnvelope, historical_event_families};
use bcode_session_models::{RequestContextOccupancy, SessionEvent, SessionEventKind};
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

/// Migration-owned authoritative state derived while traversing normalized canonical events.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthoritativeMigrationState {
    /// Current context generation.
    pub context_epoch: u64,
    /// Reconciled current request-context occupancy.
    pub context_occupancy: Option<RequestContextOccupancy>,
}

impl AuthoritativeMigrationState {
    /// Ingest one normalized current event into authoritative migration state.
    pub fn ingest(&mut self, event: &SessionEvent) {
        let (context_epoch, occupancy) = match &event.kind {
            SessionEventKind::ModelChanged { .. }
            | SessionEventKind::ContextCompacted { .. }
            | SessionEventKind::ProviderContextCompacted { .. } => (event.sequence, None),
            SessionEventKind::RequestContextObserved { observation } => (
                self.context_epoch,
                RequestContextOccupancy::reconcile(
                    self.context_occupancy.as_ref(),
                    self.context_epoch,
                    event.sequence,
                    observation.clone(),
                ),
            ),
            _ => (self.context_epoch, self.context_occupancy.clone()),
        };
        self.context_epoch = context_epoch;
        self.context_occupancy = occupancy;
    }
}

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
    let current = decode_current(payload);
    let envelope = serde_json::from_str::<HistoricalEnvelope>(payload)?;
    if !crate::is_released_historical_event_schema(envelope.schema_version()) {
        return Err(HistoricalSessionEventError::UnsupportedSchema {
            schema_version: envelope.schema_version(),
        });
    }
    let source_kind = envelope.source_kind_name()?;
    if source_kind == "tool_call_finished" && envelope.schema_version() <= 39 {
        return historical_event_families::decode_tool_call_finished(&envelope);
    }
    if source_kind == "context_usage_observed" && envelope.schema_version() <= 31 {
        return historical_event_families::decode_context_usage_observed(&envelope);
    }
    if envelope.schema_version() == 28 && source_kind == "tool_invocation_stream" {
        return historical_event_families::decode_schema_28(&envelope);
    }
    let descriptor = crate::RELEASED_EVENT_VARIANTS
        .iter()
        .find(|variant| variant.kind == source_kind)
        .copied()
        .ok_or_else(|| HistoricalSessionEventError::UnsupportedEventKind {
            schema_version: envelope.schema_version(),
            event_kind: source_kind.to_owned(),
        })?;
    if !descriptor.supports_schema(envelope.schema_version()) {
        return Err(HistoricalSessionEventError::UnsupportedEventKind {
            schema_version: envelope.schema_version(),
            event_kind: source_kind.to_owned(),
        });
    }
    if descriptor.treatment == crate::ReleasedEventTreatment::RetiredKnown {
        return envelope.decode_retired_known();
    }
    match current {
        Ok(event) => Ok(HistoricalDecode::Current(event)),
        Err(reason) if crate::is_released_historical_event_schema(envelope.schema_version()) => {
            Err(HistoricalSessionEventError::InvalidEvent {
                schema_version: envelope.schema_version(),
                event_kind: envelope.source_kind_name()?.to_owned(),
                reason,
            })
        }
        Err(_) => Err(HistoricalSessionEventError::UnsupportedSchema {
            schema_version: envelope.schema_version(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_models::{
        LocalContextEstimate, ModelRequestIdentity, RequestContextObservation,
        RequestContextTokenCount, SessionEventKind, ToolInvocationResult,
    };
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
    fn authoritative_state_conversion_tracks_resets_and_reconciled_observations() {
        let session_id = SESSION_ID.parse().expect("session ID");
        let mut state = AuthoritativeMigrationState::default();
        let observed = SessionEvent {
            schema_version: crate::CURRENT_EVENT_SCHEMA,
            sequence: 1,
            timestamp_ms: 9,
            session_id,
            provenance: None,
            kind: SessionEventKind::RequestContextObserved {
                observation: RequestContextObservation {
                    request: ModelRequestIdentity {
                        provider_plugin_id: "provider".to_owned(),
                        requested_model_id: Some("model".to_owned()),
                        effective_model_id: "model".to_owned(),
                        request_id: "request".to_owned(),
                        model_turn_id: "turn".to_owned(),
                        round: 0,
                        request_fingerprint: "fingerprint".to_owned(),
                        effective_auth_profile: None,
                        context_format_version: None,
                        compatibility_key: None,
                        context_epoch: 0,
                    },
                    context_through_sequence: 0,
                    context_tokens: RequestContextTokenCount::Estimated(10),
                    local_estimate: LocalContextEstimate {
                        tokens: 10,
                        algorithm_version: 1,
                    },
                },
            },
        };
        state.ingest(&observed);
        assert!(state.context_occupancy.is_some());

        let reset = SessionEvent {
            schema_version: crate::CURRENT_EVENT_SCHEMA,
            sequence: 2,
            timestamp_ms: 10,
            session_id,
            provenance: None,
            kind: SessionEventKind::ModelChanged {
                provider: "provider".to_owned(),
                model: "next-model".to_owned(),
            },
        };
        state.ingest(&reset);
        assert_eq!(state.context_epoch, 2);
        assert!(state.context_occupancy.is_none());
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn fixture_manifest_enforces_complete_sanitized_inventory() {
        let fixture_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let manifest = crate::load_released_fixture_manifest(&fixture_root)
            .expect("fixture manifest and disk inventory");
        assert_eq!(manifest.format_version, 1);

        for fixture in manifest.fixtures {
            if fixture.migratable_store {
                assert!(!fixture.source_writer_epochs.is_empty());
            } else {
                assert!(fixture.source_writer_epochs.is_empty());
            }
            assert!(!fixture.historical_payloads || fixture.migratable_store);
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
                fixture.event_schemas.iter().copied().collect()
            );
            assert_eq!(
                envelopes
                    .iter()
                    .flat_map(|event| event.kind.keys().cloned())
                    .collect::<BTreeSet<_>>(),
                fixture.covered_event_kinds.iter().cloned().collect()
            );
            let actual_pairs = envelopes
                .iter()
                .flat_map(|event| {
                    event
                        .kind
                        .keys()
                        .map(|kind| crate::ReleasedFixtureSchemaEventPair {
                            event_schema: event.schema_version,
                            event_kind: kind.clone(),
                        })
                })
                .collect::<BTreeSet<_>>();
            let declared_pairs = fixture
                .covered_schema_event_pairs
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>();
            if fixture.migratable_store {
                assert_eq!(actual_pairs, declared_pairs);
            } else {
                assert!(actual_pairs.is_subset(&declared_pairs));
                for pair in declared_pairs.difference(&actual_pairs) {
                    let template_index = envelopes
                        .iter()
                        .position(|event| event.kind.contains_key(&pair.event_kind))
                        .expect("declared schema/kind pair must have a template payload");
                    let mut value =
                        serde_json::from_str::<serde_json::Value>(payloads[template_index])
                            .expect("fixture payload JSON");
                    value["schema_version"] = serde_json::json!(pair.event_schema);
                    let payload = serde_json::to_string(&value).expect("fixture payload JSON");
                    decode_for_migration(&payload, reject_current).unwrap_or_else(|error| {
                        panic!(
                            "declared classification pair {}:{} must decode: {error}",
                            pair.event_schema, pair.event_kind
                        )
                    });
                }
            }
            assert!(
                !fixture.covered_authoritative_records.is_empty() || !fixture.migratable_store,
                "migratable fixture must cover authoritative records"
            );
            if fixture.migratable_store {
                assert_eq!(
                    fixture.covered_tables.len(),
                    crate::RELEASED_RECORD_TREATMENTS.len(),
                    "migratable fixture must exercise every table treatment"
                );
                assert_eq!(
                    fixture.covered_table_treatments.len(),
                    crate::RELEASED_RECORD_TREATMENTS.len(),
                    "migratable fixture must declare every exact table treatment"
                );
            }

            let historical_payloads = payloads
                .iter()
                .zip(&envelopes)
                .filter(|(_, event)| {
                    event.kind.keys().any(|kind| {
                        crate::RELEASED_EVENT_VARIANTS.iter().any(|variant| {
                            variant.kind == kind
                                && variant.treatment
                                    != crate::ReleasedEventTreatment::CurrentEquivalent
                        })
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
    fn retired_semantic_tool_results_are_preserved_as_structured_json() {
        let shell = format!(
            r#"{{"schema_version":28,"sequence":1,"session_id":"{SESSION_ID}","kind":{{"tool_call_finished":{{"tool_call_id":"call","result":"done","semantic_result":{{"type":"shell_run","result":{{"mode":"terminal","exit_code":0,"output_tail":"done","columns":120,"rows":30}}}}}}}}}}"#
        );
        let HistoricalDecode::Converted { event, .. } =
            decode_for_migration(&shell, reject_current).expect("shell result")
        else {
            panic!("expected converted shell result");
        };
        let SessionEventKind::ToolInvocationResultRecorded { record } = event.kind else {
            panic!("expected result record");
        };
        let Some(ToolInvocationResult::Json { value }) = record.result else {
            panic!("expected structured JSON preservation");
        };
        let value: serde_json::Value = serde_json::from_str(&value).expect("preserved JSON");
        assert_eq!(value["type"], "shell_run");
        assert_eq!(value["result"]["mode"], "terminal");
        assert_eq!(value["result"]["output_tail"], "done");
        assert_eq!(value["result"]["columns"], 120);

        let file_change = format!(
            r#"{{"schema_version":28,"sequence":1,"session_id":"{SESSION_ID}","kind":{{"tool_call_finished":{{"tool_call_id":"call","result":"changed","semantic_result":{{"type":"file_change","result":{{"tool_name":"filesystem.write","summary":"wrote file","path":"README.md"}}}}}}}}}}"#
        );
        let HistoricalDecode::Converted { event, .. } =
            decode_for_migration(&file_change, reject_current).expect("file result")
        else {
            panic!("expected converted file result");
        };
        let SessionEventKind::ToolInvocationResultRecorded { record } = event.kind else {
            panic!("expected result record");
        };
        let Some(ToolInvocationResult::Json { value }) = record.result else {
            panic!("expected structured JSON preservation");
        };
        let value: serde_json::Value = serde_json::from_str(&value).expect("preserved JSON");
        assert_eq!(value["type"], "file_change");
        assert_eq!(value["result"]["tool_name"], "filesystem.write");
        assert_eq!(value["result"]["path"], "README.md");
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
    fn inventoried_retired_families_materialize_as_inert_current_history() {
        for (event_kind, schema_version) in [
            ("interactive_tool_request_created", 29),
            ("interactive_tool_request_resolved", 29),
            ("legacy_event", 32),
            ("legacy_tool_invocation_presentation", 29),
            ("legacy_turn_finished", 32),
            ("legacy_turn_started", 32),
            ("plugin_automation_turn_finished", 29),
            ("plugin_automation_turn_started", 29),
            ("tool_invocation_presentation", 21),
        ] {
            let payload = format!(
                r#"{{"schema_version":{schema_version},"sequence":1,"session_id":"{SESSION_ID}","kind":{{"{event_kind}":{{"preserved":true}}}}}}"#
            );
            let HistoricalDecode::RetiredKnown { event, metadata } =
                decode_for_migration(&payload, reject_current).expect("retired family")
            else {
                panic!("expected retired-known {event_kind}");
            };
            assert_eq!(metadata.source_kind, event_kind);
            assert!(matches!(
                event.kind,
                SessionEventKind::OpaqueEvent { ref event_type, ref payload }
                    if event_type == event_kind && payload["preserved"] == true
            ));
        }
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
    fn historical_codec_only_applies_family_rules_to_released_schema_ranges() {
        let payload = format!(
            r#"{{"schema_version":40,"sequence":1,"session_id":"{SESSION_ID}","kind":{{"tool_call_finished":{{"tool_call_id":"call","result":"done"}}}}}}"#
        );
        assert!(matches!(
            decode_for_migration(&payload, reject_current),
            Err(HistoricalSessionEventError::UnsupportedSchema { schema_version: 40 })
        ));

        let payload = format!(
            r#"{{"schema_version":32,"sequence":1,"session_id":"{SESSION_ID}","kind":{{"context_usage_observed":{{"snapshot":{{"invocation":{{"provider_plugin_id":"provider","effective_model_id":"model","request_id":"request","model_turn_id":"turn","round":0,"request_fingerprint":"fingerprint"}},"context_through_sequence":0,"context_input_tokens":1,"local_request_estimate_tokens":1,"source":"estimated"}}}}}}}}"#
        );
        assert!(matches!(
            decode_for_migration(&payload, reject_current),
            Err(HistoricalSessionEventError::UnsupportedEventKind {
                schema_version: 32,
                ..
            })
        ));

        let payload = format!(
            r#"{{"schema_version":1,"sequence":1,"session_id":"{SESSION_ID}","kind":{{"tool_invocation_stream":{{"event":{{"status":{{"tool_call_id":"call","sequence":1,"message":"working"}}}}}}}}}}"#
        );
        assert!(matches!(
            decode_for_migration(&payload, reject_current),
            Err(HistoricalSessionEventError::UnsupportedEventKind {
                schema_version: 1,
                ..
            })
        ));
    }

    #[test]
    fn all_released_flat_tool_completion_schemas_use_the_frozen_codec() {
        for schema_version in crate::RELEASED_HISTORICAL_EVENT_SCHEMAS
            .iter()
            .copied()
            .filter(|schema_version| *schema_version <= 39)
        {
            let payload = format!(
                r#"{{"schema_version":{schema_version},"sequence":1,"session_id":"{SESSION_ID}","kind":{{"tool_call_finished":{{"tool_call_id":"call","result":"done"}}}}}}"#
            );
            assert!(matches!(
                decode_for_migration(&payload, reject_current),
                Ok(HistoricalDecode::Converted { event, metadata })
                    if metadata.source_schema == schema_version
                        && metadata.source_kind == "tool_call_finished"
                        && matches!(
                            &event.kind,
                            SessionEventKind::ToolInvocationResultRecorded { record }
                                if record.invocation_id == "call"
                                    && record.model_output == "done"
                                    && !record.is_error
                                    && record.result.is_none()
                        )
            ));
        }
    }

    #[test]
    fn all_released_context_usage_shapes_use_the_frozen_codec() {
        for schema_version in crate::RELEASED_HISTORICAL_EVENT_SCHEMAS
            .iter()
            .copied()
            .filter(|schema_version| (26..=29).contains(schema_version))
        {
            let payload = format!(
                r#"{{"schema_version":{schema_version},"sequence":1,"session_id":"{SESSION_ID}","kind":{{"context_usage_observed":{{"snapshot":{{"provider_plugin_id":"provider","model_id":"model","input_tokens":123,"context_through_sequence":0,"request_id":"request","model_turn_id":"turn","round":0,"request_fingerprint":"fingerprint","estimated_input_tokens":120,"source":"estimated"}}}}}}}}"#
            );
            assert!(matches!(
                decode_for_migration(&payload, reject_current),
                Ok(HistoricalDecode::Converted { event, metadata })
                    if metadata.source_schema == schema_version
                        && metadata.source_kind == "context_usage_observed"
                        && matches!(
                            &event.kind,
                            SessionEventKind::RequestContextObserved { observation }
                                if observation.request.request_id == "request"
                                    && observation.context_tokens
                                        == RequestContextTokenCount::Estimated(123)
                                    && observation.local_estimate.tokens == 120
                        )
            ));
        }

        for schema_version in [30_u16, 31] {
            let payload = format!(
                r#"{{"schema_version":{schema_version},"sequence":1,"session_id":"{SESSION_ID}","kind":{{"context_usage_observed":{{"snapshot":{{"invocation":{{"provider_plugin_id":"provider","requested_model_id":"alias","effective_model_id":"model","request_id":"request","model_turn_id":"turn","round":2,"request_fingerprint":"fingerprint","provider_turn_id":"provider-turn","effective_auth_profile":"profile","context_epoch":3}},"context_through_sequence":0,"context_input_tokens":123,"local_request_estimate_tokens":120,"source":"provider"}}}}}}}}"#
            );
            assert!(matches!(
                decode_for_migration(&payload, reject_current),
                Ok(HistoricalDecode::Converted { event, metadata })
                    if metadata.source_schema == schema_version
                        && metadata.source_kind == "context_usage_observed"
                        && matches!(
                            &event.kind,
                            SessionEventKind::RequestContextObserved { observation }
                                if observation.request.request_id == "request"
                                    && observation.request.requested_model_id.as_deref()
                                        == Some("alias")
                                    && observation.request.context_epoch == 3
                                    && observation.context_tokens
                                        == RequestContextTokenCount::ProviderExact(123)
                                    && observation.local_estimate.tokens == 120
                        )
            ));
        }
    }

    #[test]
    fn context_usage_without_required_request_identity_is_preserved_inert() {
        let payload = format!(
            r#"{{"schema_version":26,"sequence":1,"session_id":"{SESSION_ID}","kind":{{"context_usage_observed":{{"snapshot":{{"provider_plugin_id":"provider","model_id":"model","input_tokens":123,"context_through_sequence":0,"source":"provider"}}}}}}}}"#
        );
        assert!(matches!(
            decode_for_migration(&payload, reject_current),
            Ok(HistoricalDecode::RetiredKnown { event, metadata })
                if metadata.source_schema == 26
                    && metadata.source_kind == "context_usage_observed"
                    && matches!(
                        &event.kind,
                        SessionEventKind::OpaqueEvent { event_type, payload }
                            if event_type == "context_usage_observed"
                                && payload["snapshot"]["input_tokens"] == 123
                    )
        ));
    }

    #[test]
    fn early_context_usage_defaults_missing_local_estimate_to_observed_tokens() {
        let payload = format!(
            r#"{{"schema_version":26,"sequence":1,"session_id":"{SESSION_ID}","kind":{{"context_usage_observed":{{"snapshot":{{"provider_plugin_id":"provider","model_id":"model","input_tokens":123,"context_through_sequence":0,"request_id":"request","model_turn_id":"turn","round":0,"request_fingerprint":"fingerprint","source":"estimated"}}}}}}}}"#
        );
        assert!(matches!(
            decode_for_migration(&payload, reject_current),
            Ok(HistoricalDecode::Converted { event, metadata })
                if metadata.source_schema == 26
                    && matches!(
                        &event.kind,
                        SessionEventKind::RequestContextObserved { observation }
                            if observation.context_tokens
                                == RequestContextTokenCount::Estimated(123)
                                && observation.local_estimate.tokens == 123
                    )
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
