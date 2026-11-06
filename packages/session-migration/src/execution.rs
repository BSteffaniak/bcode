//! Historical canonical normalization and conversion into current session events.

use crate::classification::{HistoricalDecode, HistoricalEventMetadata};
use crate::codec::{HistoricalEnvelope, historical_event_families};
use crate::{
    SessionMigrationCanonicalReceiptEvidence, SessionMigrationReceiptRequest,
    build_session_migration_receipt,
};
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

/// Build one durable target receipt from policy-free replay facts.
///
/// # Errors
///
/// Returns an error when the source writer epoch has no released migration plan or receipt
/// invariants are inconsistent.
pub fn build_target_receipt(
    facts: bcode_session_migration_target::MigrationReceiptFacts,
) -> Result<bcode_session_migration_target::MigrationReceipt, String> {
    let source_writer_epoch = u32::try_from(facts.source_writer_epoch).map_err(|_| {
        format!(
            "source writer epoch {} is out of range",
            facts.source_writer_epoch
        )
    })?;
    let receipt = build_session_migration_receipt(SessionMigrationReceiptRequest {
        operation_id: facts
            .operation_id
            .unwrap_or_else(|| "explicit-maintenance".to_owned()),
        source_writer_epoch,
        source: SessionMigrationCanonicalReceiptEvidence {
            event_count: facts.event_count,
            event_tail: facts.event_tail,
            event_digest_sha256: facts.replay.source_payload_digest_sha256,
        },
        target: SessionMigrationCanonicalReceiptEvidence {
            event_count: facts.event_count,
            event_tail: facts.event_tail,
            event_digest_sha256: facts.target_payload_digest_sha256,
        },
        converted_events: facts.replay.converted_events,
        retired_known_events: facts.replay.retired_known_events,
        completed_at_ms: facts.completed_at_ms,
    })
    .map_err(|error| error.to_string())?;
    Ok(bcode_session_migration_target::MigrationReceipt {
        operation_id: receipt.operation_id,
        session_id: facts.session_id,
        source_writer_epoch: receipt.source_writer_epoch,
        target_writer_epoch: receipt.target_writer_epoch,
        migration_step_ids: receipt.migration_step_ids,
        source_event_count: receipt.source_event_count,
        source_event_tail: receipt.source_event_tail,
        source_payload_digest_sha256: receipt.source_event_digest_sha256,
        target_event_count: receipt.target_event_count,
        target_event_tail: receipt.target_event_tail,
        target_payload_digest_sha256: receipt.target_event_digest_sha256,
        converted_events: receipt.converted_events,
        retired_known_events: receipt.retired_known_events,
        completed_at_ms: receipt.completed_at_ms,
    })
}

/// Normalize one source row using migration-owned historical policy.
///
/// # Errors
///
/// Returns an error when the payload is not a strict current event or a recognized released
/// historical event.
pub fn normalize_canonical_row(
    row: &bcode_session_migration_target::CanonicalRow,
) -> Result<bcode_session_migration_target::NormalizedCanonicalRow, HistoricalSessionEventError> {
    let normalized = normalize_canonical_event(
        &row.payload,
        bcode_session_migration_target::CURRENT_EVENT_SCHEMA,
        |payload| serde_json::from_str::<SessionEvent>(payload).map_err(|error| error.to_string()),
    )?;
    Ok(bcode_session_migration_target::NormalizedCanonicalRow {
        event: normalized.event,
        metric_counter: normalized.metric_counter.map(str::to_owned),
        historical_source: normalized
            .historical
            .map(|metadata| format!("{}:{}", metadata.source_schema, metadata.source_kind)),
        retired_known: normalized.retired_known,
    })
}

/// Normalize a row under the compatibility plan for an inspected durable writer epoch.
///
/// Epochs through 20 retain schema-55 observation shapes but require regenerated host
/// commitments. Only this migration-owned boundary removes those derived commitments;
/// current-epoch repair and ordinary normalization retain exact commitment verification.
/// The caller must prepare the returned event against the ingested prefix, persist it,
/// then ingest the canonical projectors once before normalizing the next row.
///
/// # Errors
/// Returns an error for an unsupported writer, malformed source event, or invalid
/// typed harness operation. It never retries using a different source epoch.
pub fn normalize_canonical_row_for_writer_epoch(
    row: &bcode_session_migration_target::CanonicalRow,
    source_writer_epoch: u32,
) -> Result<bcode_session_migration_target::NormalizedCanonicalRow, HistoricalSessionEventError> {
    crate::plan_writer_epoch_migration(source_writer_epoch).map_err(|error| {
        HistoricalSessionEventError::InvalidEvent {
            schema_version: row.schema_version,
            event_kind: row.event_kind.clone(),
            reason: format!(
                "unsupported migration source writer epoch {source_writer_epoch}: {error}"
            ),
        }
    })?;
    let mut normalized = normalize_canonical_row(row)?;
    if let SessionEventKind::ExternalHarnessOperation { operation } = &normalized.event.kind
        && (operation.version == 2 || operation.is_selected_open())
        && (source_writer_epoch
            != bcode_session_models::external_harness::SELECTED_OPEN_STORAGE_WRITER_EPOCH
            || row.schema_version
                != bcode_session_models::external_harness::SELECTED_OPEN_EVENT_SCHEMA_VERSION)
    {
        return Err(HistoricalSessionEventError::InvalidEvent {
            schema_version: row.schema_version,
            event_kind: row.event_kind.clone(),
            reason: "selected-model operations require writer epoch 25 and event schema 57".into(),
        });
    }
    validate_recovery_writer(&normalized.event, source_writer_epoch)?;
    if source_writer_epoch <= 21
        && let SessionEventKind::ToolInvocationLifecycle { event } = &normalized.event.kind
        && event
            .metadata
            .get(bcode_session_models::MANAGED_TOOL_CLAIM_KEY)
            .is_some()
    {
        return Err(HistoricalSessionEventError::InvalidEvent {
            schema_version: row.schema_version,
            event_kind: row.event_kind.clone(),
            reason: "managed tool claim metadata requires source writer epoch 22".into(),
        });
    }
    if source_writer_epoch <= 20
        && let SessionEventKind::ExternalHarnessOperation { operation } = &mut normalized.event.kind
    {
        operation
            .validate()
            .map_err(|reason| HistoricalSessionEventError::InvalidEvent {
                schema_version: row.schema_version,
                event_kind: row.event_kind.clone(),
                reason,
            })?;
        operation.inventory_commitments.clear();
    }
    Ok(normalized)
}

fn validate_recovery_writer(
    event: &SessionEvent,
    _source_writer_epoch: u32,
) -> Result<(), HistoricalSessionEventError> {
    use bcode_session_models::ExternalHarnessOperationRecord;
    if let SessionEventKind::ExternalHarnessOperation { operation } = &event.kind
        && matches!(
            operation.record,
            ExternalHarnessOperationRecord::TransportReleased { .. }
                | ExternalHarnessOperationRecord::RecoveryPrepared { .. }
        )
    {
        return Err(HistoricalSessionEventError::InvalidEvent {
            schema_version: event.schema_version,
            event_kind: "external_harness_operation".into(),
            reason: "recovery operations are unpublished and unsupported".into(),
        });
    }
    Ok(())
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
    if current_schema == 56 {
        return Err(HistoricalSessionEventError::UnsupportedSchema {
            schema_version: current_schema,
        });
    }
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
    hex::encode(digest.finalize())
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
    let envelope = serde_json::from_str::<HistoricalEnvelope>(payload)?;
    envelope.validate_observation_compatibility()?;
    let current = decode_current(payload);
    if envelope.schema_version() == 52 {
        return envelope.decode_schema_52(current);
    }
    if envelope.schema_version() == crate::CURRENT_EVENT_SCHEMA {
        return match current {
            Ok(event) => Ok(HistoricalDecode::Current(event)),
            Err(reason) => Err(HistoricalSessionEventError::InvalidEvent {
                schema_version: envelope.schema_version(),
                event_kind: envelope.source_kind_name()?.to_owned(),
                reason,
            }),
        };
    }
    if !crate::is_released_historical_event_schema(envelope.schema_version()) {
        return Err(HistoricalSessionEventError::UnsupportedSchema {
            schema_version: envelope.schema_version(),
        });
    }
    let source_kind = envelope.source_kind_name()?;
    if source_kind == "child_session_created"
        && envelope.schema_version() < 52
        && crate::is_released_event_kind_schema(source_kind, envelope.schema_version())
    {
        return envelope.decode_child_session_created();
    }
    // Schemas 51 through 55 are representation-compatible except for child creation above.
    // Schema 53 external observations retain their original evidence class: replay must never
    // promote a result, acceptance or settlement into a newly introduced terminal disposition.
    // Preserve recorded usage and all admission identities rather than applying older policy.
    if matches!(envelope.schema_version(), 51..=55)
        && crate::is_released_event_kind_schema(source_kind, envelope.schema_version())
        && crate::RELEASED_EVENT_VARIANTS.iter().any(|variant| {
            variant.kind == source_kind
                && (variant.treatment == crate::ReleasedEventTreatment::CurrentEquivalent
                    || (matches!(envelope.schema_version(), 53..=55)
                        && variant.treatment == crate::ReleasedEventTreatment::ExplicitConversion))
        })
    {
        return current
            .map(|event| HistoricalDecode::Converted {
                event,
                metadata: HistoricalEventMetadata {
                    source_schema: envelope.schema_version(),
                    source_kind: source_kind.to_owned(),
                },
            })
            .map_err(|reason| HistoricalSessionEventError::InvalidEvent {
                schema_version: envelope.schema_version(),
                event_kind: source_kind.to_owned(),
                reason,
            });
    }
    // Epoch-11 schema 47 is representation-compatible. Preserve its semantics, including
    // historical user-message visibility and receipt identities; create no submissions.
    if envelope.schema_version() == 47
        && crate::is_released_event_kind_schema(source_kind, 47)
        && current.is_ok()
    {
        return current.map(HistoricalDecode::Current).map_err(|reason| {
            HistoricalSessionEventError::InvalidEvent {
                schema_version: 47,
                event_kind: source_kind.to_owned(),
                reason,
            }
        });
    }
    decode_released_event(&envelope, source_kind, current)
}

// Historical family dispatch stays in the migration domain after schema
// classification and representation-compatible exceptions have been resolved.
fn decode_released_event(
    envelope: &HistoricalEnvelope,
    source_kind: &str,
    current: Result<SessionEvent, String>,
) -> Result<HistoricalDecode, HistoricalSessionEventError> {
    if source_kind == "model_usage" {
        return match current {
            Ok(mut event) => {
                let SessionEventKind::ModelUsage { usage, .. } = &mut event.kind else {
                    unreachable!("model_usage kind decoded to a different current event");
                };
                if usage.cost.is_none() {
                    usage.cost = Some(bcode_session_models::SessionCostEstimate::Unavailable {
                        reason: bcode_session_models::SessionCostUnavailableReason::RequestIdentityUnavailable,
                    });
                }
                Ok(HistoricalDecode::Converted {
                    event,
                    metadata: HistoricalEventMetadata {
                        source_schema: envelope.schema_version(),
                        source_kind: source_kind.to_owned(),
                    },
                })
            }
            Err(reason) => Err(HistoricalSessionEventError::InvalidEvent {
                schema_version: envelope.schema_version(),
                event_kind: source_kind.to_owned(),
                reason,
            }),
        };
    }
    if source_kind == "execution_session_created"
        && envelope.schema_version() <= crate::LATEST_HISTORICAL_EVENT_SCHEMA
    {
        return historical_event_families::decode_execution_session_created(envelope);
    }
    if source_kind == "tool_call_finished" && envelope.schema_version() <= 39 {
        return historical_event_families::decode_tool_call_finished(envelope);
    }
    if source_kind == "context_usage_observed" && envelope.schema_version() <= 31 {
        return historical_event_families::decode_context_usage_observed(envelope);
    }
    if envelope.schema_version() == 28 && source_kind == "tool_invocation_stream" {
        return historical_event_families::decode_schema_28(envelope);
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
        Err(reason) => Err(HistoricalSessionEventError::InvalidEvent {
            schema_version: envelope.schema_version(),
            event_kind: envelope.source_kind_name()?.to_owned(),
            reason,
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
    fn released_writer_23_schema_55_normalization_is_lossless() {
        let fixture = include_str!("../fixtures/stores/epoch-23-schema-55-events.jsonl");
        for payload in fixture.lines() {
            let source: SessionEvent = serde_json::from_str(payload).unwrap();
            let value: serde_json::Value = serde_json::from_str(payload).unwrap();
            let kind = value["kind"].as_object().unwrap().keys().next().unwrap();
            let row = bcode_session_migration_target::CanonicalRow {
                sequence: source.sequence,
                event_kind: kind.clone(),
                schema_version: 55,
                payload: payload.to_owned(),
            };
            let normalized = normalize_canonical_row_for_writer_epoch(&row, 23)
                .expect("released writer-23 representation");
            let mut expected = source;
            expected.schema_version = crate::CURRENT_EVENT_SCHEMA;
            assert_eq!(normalized.event, expected, "preserve complete event {kind}");
            assert!(!normalized.retired_known);
            assert_eq!(normalized.historical_source, Some(format!("55:{kind}")));
            assert!(normalized.metric_counter.is_none());
            assert!(normalize_canonical_row_for_writer_epoch(&row, 24).is_err());
        }
    }

    #[test]
    fn historical_harness_version_cannot_acquire_selected_open_semantics() {
        let mut payload: serde_json::Value = serde_json::from_str(
            include_str!("../fixtures/stores/epoch-23-schema-55-events.jsonl")
                .lines()
                .find(|line| line.contains("\"external_harness_operation\""))
                .unwrap(),
        )
        .unwrap();
        payload["kind"]["external_harness_operation"]["operation"]["version"] = 2.into();
        assert!(
            decode_for_migration(&payload.to_string(), |_| {
                panic!("historical version 2 must not reach the current decoder")
            })
            .is_err()
        );
    }

    #[test]
    fn selected_open_cannot_be_smuggled_into_released_harness_payloads() {
        for record in [
            serde_json::json!({"kind": "configured", "open_request": {
                "kind": {"operation": "open_selected", "request": {}}
            }}),
            serde_json::json!({"kind": "delivery_claimed", "request": {
                "kind": {"operation": "open_selected", "request": {}}
            }}),
            serde_json::json!({"kind": "delivery_completed", "response": {
                "kind": {"kind": "selected", "response": {}}
            }}),
        ] {
            let payload = serde_json::json!({
                "schema_version": 55,
                "sequence": 1,
                "timestamp_ms": 2,
                "session_id": SESSION_ID,
                "kind": {"external_harness_operation": {"operation": {
                    "version": 1,
                    "record": record
                }}}
            })
            .to_string();
            assert!(
                decode_for_migration(&payload, |_| {
                    panic!("selected-open historical payload must fail before decoding")
                })
                .is_err()
            );
        }
    }

    #[test]
    fn reserved_schema_cannot_be_a_normalization_target() {
        assert!(matches!(
            normalize_canonical_event("{}", 56, |_| {
                panic!("reserved target cannot invoke decoding")
            }),
            Err(HistoricalSessionEventError::UnsupportedSchema { schema_version: 56 })
        ));
    }

    #[test]
    fn reserved_schema_and_disabled_families_cannot_bypass_decoder() {
        for schema in [55, 56, 57] {
            for kind in [
                "durable_interaction_committed",
                "turn_continuation_committed",
            ] {
                let payload = serde_json::json!({
                    "schema_version": schema,
                    "sequence": 1,
                    "timestamp_ms": 2,
                    "session_id": SESSION_ID,
                    "kind": {kind: {}}
                })
                .to_string();
                assert!(
                    decode_for_migration(&payload, |_| {
                        panic!("disabled family must be rejected before current decoding")
                    })
                    .is_err()
                );
            }
        }
        let payload = serde_json::json!({
            "schema_version": 56,
            "sequence": 1,
            "timestamp_ms": 2,
            "session_id": SESSION_ID,
            "kind": {"session_renamed": {"name": null}}
        })
        .to_string();
        assert!(matches!(
            decode_for_migration(&payload, |_| {
                panic!("reserved schema must be rejected before current decoding")
            }),
            Err(HistoricalSessionEventError::UnsupportedSchema { schema_version: 56 })
        ));
    }

    #[test]
    fn schema_52_fixture_migrates_losslessly_without_creating_checkpoints() {
        let plan = crate::plan_writer_epoch_migration(16).expect("known lossless upgrade");
        assert_eq!(plan.target_writer_epoch, crate::CURRENT_WRITER_EPOCH);
        assert_eq!(plan.steps.len(), 8);
        assert_eq!(plan.steps[0].id, "session-writer-epoch-16-to-17");
        assert_eq!(plan.steps[1].id, "session-writer-epoch-17-to-18");
        assert_eq!(plan.steps[2].id, "session-writer-epoch-18-to-19");
        assert_eq!(plan.steps[3].id, "session-writer-epoch-19-to-20");
        assert_eq!(plan.steps[4].id, "session-writer-epoch-20-to-21");
        assert_eq!(plan.steps[5].id, "session-writer-epoch-21-to-22");
        assert_eq!(plan.steps[6].id, "session-writer-epoch-22-to-23");
        assert_eq!(plan.steps[7].id, "session-writer-epoch-23-to-25");
        let fixture = include_str!("../fixtures/stores/epoch-16-schema-52-events.jsonl");
        let mut seen = BTreeSet::new();
        for payload in fixture.lines() {
            let envelope: serde_json::Value = serde_json::from_str(payload).unwrap();
            let kind = envelope["kind"].as_object().unwrap().keys().next().unwrap();
            let row = bcode_session_migration_target::CanonicalRow {
                sequence: envelope["sequence"].as_u64().unwrap(),
                event_kind: kind.clone(),
                schema_version: 52,
                payload: payload.to_owned(),
            };
            let normalized = normalize_canonical_row(&row).expect("released schema-52 fixture");
            assert_eq!(normalized.event.schema_version, crate::CURRENT_EVENT_SCHEMA);
            assert_eq!(normalized.event.sequence, row.sequence);
            assert_eq!(normalized.historical_source, Some(format!("52:{kind}")));
            assert!(normalized.metric_counter.is_none());
            let actual = serde_json::to_value(&normalized.event).unwrap();
            for field in ["sequence", "timestamp_ms", "session_id", "provenance"] {
                assert_eq!(
                    actual[field], envelope[field],
                    "preserve {field} for {kind}"
                );
            }
            if kind == "session_forked" {
                assert!(normalized.retired_known);
                assert_eq!(
                    actual["kind"]["inert_history"]["payload"],
                    envelope["kind"][kind]
                );
            } else {
                let source: SessionEvent = serde_json::from_str(payload).unwrap();
                assert_eq!(normalized.event.kind, source.kind, "preserve {kind}");
                if let SessionEventKind::ChildSessionCreated { spawning, .. } =
                    &normalized.event.kind
                {
                    let spawning = spawning
                        .as_ref()
                        .expect("recorded composed spawn is retained");
                    assert_eq!(spawning.input, "sanitized original child input");
                }
                assert!(!normalized.retired_known);
            }
            assert!(
                !actual["kind"]
                    .as_object()
                    .unwrap()
                    .contains_key("durable_interaction_committed")
            );
            seen.insert(kind.clone());
        }
        for kind in [
            "model_usage",
            "execution_session_created",
            "child_session_created",
            "submission_recovery_authorized",
            "submission_pending_cancelled",
            "tool_exchange_requested",
            "tool_exchange_resolved",
            "tool_invocation_result_recorded",
        ] {
            assert!(seen.contains(kind), "fixture covers {kind}");
        }
    }

    #[test]
    fn schema_52_valid_current_equivalent_payloads_cannot_bypass_decoder_rejection() {
        let fixture = include_str!("../fixtures/stores/epoch-16-schema-52-events.jsonl");
        let mut checked = BTreeSet::new();
        for payload in fixture.lines() {
            let envelope: serde_json::Value = serde_json::from_str(payload).unwrap();
            let kind = envelope["kind"].as_object().unwrap().keys().next().unwrap();
            if !crate::RELEASED_EVENT_VARIANTS.iter().any(|descriptor| {
                descriptor.kind == kind
                    && descriptor.treatment == crate::ReleasedEventTreatment::CurrentEquivalent
            }) {
                continue;
            }
            serde_json::from_str::<SessionEvent>(payload)
                .expect("fixture contains a valid current-equivalent representation");
            let decoded =
                decode_for_migration(
                    payload,
                    |_| Err("current compatibility rejected".to_owned()),
                );
            assert!(
                matches!(
                    &decoded,
                    Err(HistoricalSessionEventError::InvalidEvent {
                        schema_version: 52,
                        event_kind,
                        reason,
                    }) if event_kind == kind && reason == "current compatibility rejected"
                ),
                "valid current-equivalent 52:{kind} must retain decoder rejection: {decoded:?}"
            );
            checked.insert(kind.clone());
        }
        let expected = crate::RELEASED_EVENT_VARIANTS
            .iter()
            .filter(|descriptor| {
                descriptor.treatment == crate::ReleasedEventTreatment::CurrentEquivalent
                    && descriptor.supports_schema(52)
            })
            .map(|descriptor| descriptor.kind.to_owned())
            .collect::<BTreeSet<_>>();
        assert_eq!(checked, expected);
    }

    #[test]
    fn schema_52_explicit_conversions_preserve_recorded_values_when_current_rejects() {
        let fixture = include_str!("../fixtures/stores/epoch-16-schema-52-events.jsonl");
        let mut checked = BTreeSet::new();
        for payload in fixture.lines() {
            let envelope: serde_json::Value = serde_json::from_str(payload).unwrap();
            let kind = envelope["kind"].as_object().unwrap().keys().next().unwrap();
            if !crate::RELEASED_EVENT_VARIANTS.iter().any(|descriptor| {
                descriptor.kind == kind
                    && descriptor.treatment == crate::ReleasedEventTreatment::ExplicitConversion
            }) {
                continue;
            }
            let source: SessionEvent = serde_json::from_str(payload).unwrap();
            let normalized = normalize_canonical_event(payload, 53, reject_current)
                .expect("schema-52 explicit conversions are migration-owned");
            assert_eq!(normalized.event.kind, source.kind, "preserve 52:{kind}");
            assert_eq!(normalized.historical.as_ref().unwrap().source_schema, 52);
            assert!(!normalized.retired_known);
            checked.insert(kind.clone());
        }
        let expected = crate::RELEASED_EVENT_VARIANTS
            .iter()
            .filter(|descriptor| {
                descriptor.treatment == crate::ReleasedEventTreatment::ExplicitConversion
                    && descriptor.supports_schema(52)
            })
            .map(|descriptor| descriptor.kind.to_owned())
            .collect::<BTreeSet<_>>();
        assert_eq!(checked, expected);
    }

    #[test]
    fn schema_52_pending_exchange_stays_historical_not_recoverable() {
        let fixture = include_str!("../fixtures/stores/epoch-16-schema-52-events.jsonl");
        let pending = fixture.lines().filter(|payload| {
            let envelope: serde_json::Value = serde_json::from_str(payload).unwrap();
            !envelope["kind"]
                .as_object()
                .unwrap()
                .contains_key("tool_exchange_resolved")
        });
        let mut requests = Vec::new();
        for payload in pending {
            let source: serde_json::Value = serde_json::from_str(payload).unwrap();
            let normalized = normalize_canonical_event(payload, 53, |payload| {
                serde_json::from_str(payload).map_err(|error| error.to_string())
            })
            .expect("migration-owned schema-52 decoder");
            if let SessionEventKind::ToolExchangeRequested { request } = &normalized.event.kind {
                requests.push(serde_json::to_value(request).unwrap());
                assert_eq!(
                    requests.last().unwrap(),
                    &source["kind"]["tool_exchange_requested"]["request"]
                );
            }
            assert!(!matches!(
                normalized.event.kind,
                SessionEventKind::DurableInteractionCommitted { .. }
            ));
        }
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["invocation_id"], "call");
        assert_eq!(requests[0]["exchange_id"], "exchange");
    }

    #[test]
    fn schema_52_rejects_durable_event_impersonation_even_if_current_decoder_accepts() {
        let payload = serde_json::json!({
            "schema_version": 52, "sequence": 7, "timestamp_ms": 13,
            "session_id": SESSION_ID,
            "kind": { "durable_interaction_committed": { "operation": {
                "version": 1,
                "key": { "turn_id": "original-turn", "invocation_id": "call", "interaction_id": "exchange" },
                "operation_id": "impersonated", "request_digest": "0".repeat(64),
                "producer_plugin_id": "fixture.plugin", "request_event_sequence": 5,
                "exchange_request_event_sequence": 6, "expected_revision": null,
                "transition": { "checkpoint": { "payload": {
                    "contract_id": "fixture", "contract_version": 1, "json": "{}"
                } } }
            } } }
        }).to_string();
        let current: SessionEvent =
            serde_json::from_str(&payload).expect("new variant is parseable");
        assert!(matches!(
            current.kind,
            SessionEventKind::DurableInteractionCommitted { .. }
        ));
        assert!(matches!(
            normalize_canonical_event(&payload, 53, |_| Ok(current)),
            Err(HistoricalSessionEventError::UnsupportedEventKind { schema_version: 52, event_kind })
                if event_kind == "durable_interaction_committed"
        ));
        assert!(matches!(
            normalize_canonical_event(&payload, 53, |_| Err("current-only".to_owned())),
            Err(HistoricalSessionEventError::UnsupportedEventKind {
                schema_version: 52,
                ..
            })
        ));
    }

    #[test]
    fn schema_47_upgrade_preserves_user_admission_and_turn_identity() {
        let payload = format!(
            r#"{{"schema_version":47,"sequence":17,"timestamp_ms":2,"session_id":"{SESSION_ID}","kind":{{"user_message":{{"client_id":"00000000-0000-0000-0000-000000000002","text":"historical pending is not rescheduled","admission":{{}}}}}}}}"#
        );
        let before: SessionEvent = serde_json::from_str(&payload).unwrap();
        let normalized =
            normalize_canonical_event(&payload, crate::CURRENT_EVENT_SCHEMA, |value| {
                serde_json::from_str(value).map_err(|error| error.to_string())
            })
            .unwrap();
        assert_eq!(before.kind, normalized.event.kind);
        assert_eq!(before.sequence, normalized.event.sequence);
        assert_eq!(before.session_id, normalized.event.session_id);
        assert_eq!(
            bcode_session_models::TurnReceipt::from_accepted_event(
                before.session_id,
                before.sequence
            ),
            bcode_session_models::TurnReceipt::from_accepted_event(
                normalized.event.session_id,
                normalized.event.sequence
            )
        );
        assert!(!normalized.retired_known);
    }

    #[test]
    fn schema_47_usage_and_execution_rows_preserve_recorded_authority() {
        let fixture = include_str!("../fixtures/stores/epoch-11-schema-47-events.jsonl");
        let mut checked = BTreeSet::new();
        for payload in fixture.lines() {
            let value: serde_json::Value = serde_json::from_str(payload).unwrap();
            let kind = value["kind"].as_object().unwrap().keys().next().unwrap();
            if !matches!(kind.as_str(), "model_usage" | "execution_session_created") {
                continue;
            }
            let source: SessionEvent = serde_json::from_str(payload).unwrap();
            let decoded = decode_for_migration(payload, |payload| {
                serde_json::from_str(payload).map_err(|error| error.to_string())
            })
            .expect("released schema-47 representation");
            assert!(matches!(&decoded, HistoricalDecode::Current(_)));
            assert_eq!(decoded.event(), &source);
            assert_eq!(decoded.metadata(), None);
            let normalized =
                normalize_canonical_event(payload, crate::CURRENT_EVENT_SCHEMA, |payload| {
                    serde_json::from_str(payload).map_err(|error| error.to_string())
                })
                .unwrap();
            let mut expected = source;
            expected.schema_version = crate::CURRENT_EVENT_SCHEMA;
            assert_eq!(normalized.event, expected);
            assert!(normalized.historical.is_none());
            assert!(!normalized.retired_known);
            match &normalized.event.kind {
                SessionEventKind::ModelUsage { usage, .. } => {
                    assert_eq!(usage.cost, None);
                    assert_eq!(usage.request_id, None);
                    assert_eq!(usage.request, None);
                    assert_eq!(usage.total_tokens, Some(3));
                }
                SessionEventKind::ExecutionSessionCreated {
                    provenance,
                    visibility,
                } => {
                    assert_eq!(provenance.version, 2);
                    assert_eq!(provenance.owner, "fixture");
                    assert_eq!(
                        *visibility,
                        bcode_session_models::SessionVisibility::Background
                    );
                }
                _ => unreachable!(),
            }
            checked.insert(kind.clone());
        }
        assert_eq!(
            checked,
            BTreeSet::from([
                "model_usage".to_owned(),
                "execution_session_created".to_owned()
            ])
        );
    }

    #[test]
    fn historical_usage_without_trustworthy_attribution_is_explicitly_unavailable() {
        let payload = format!(
            r#"{{"schema_version":46,"sequence":1,"timestamp_ms":2,"session_id":"{SESSION_ID}","provenance":null,"kind":{{"model_usage":{{"turn_id":"turn","usage":{{"input_tokens":1,"output_tokens":2,"total_tokens":3}}}}}}}}"#
        );
        let decoded = decode_for_migration(&payload, |payload| {
            serde_json::from_str(payload).map_err(|error| error.to_string())
        })
        .expect("historical usage should convert");

        let HistoricalDecode::Converted { event, metadata } = decoded else {
            panic!("historical usage should be migration-owned conversion");
        };
        assert_eq!(crate::CURRENT_EVENT_SCHEMA, 57);
        assert_eq!(crate::LATEST_HISTORICAL_EVENT_SCHEMA, 55);
        // Classification preserves the source envelope; canonical normalization alone
        // advances its schema after historical semantics have been interpreted.
        assert_eq!(event.schema_version, metadata.source_schema);
        assert!(crate::is_released_historical_event_schema(
            metadata.source_schema
        ));
        assert_eq!(metadata.source_schema, 46);
        let SessionEventKind::ModelUsage { usage, .. } = event.kind else {
            panic!("converted event should remain model usage");
        };
        assert!(usage.request.is_none());
        assert!(usage.request_id.is_none());
        assert_eq!(
            usage.cost,
            Some(bcode_session_models::SessionCostEstimate::Unavailable {
                reason:
                    bcode_session_models::SessionCostUnavailableReason::RequestIdentityUnavailable,
            })
        );
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
                selection_source: bcode_session_models::ModelSelectionSource::UserExplicit,
            },
        };
        state.ingest(&reset);
        assert_eq!(state.context_epoch, 2);
        assert!(state.context_occupancy.is_none());
    }

    #[test]
    fn schema_48_submission_fixture_matches_domain_owned_models() {
        use bcode_session_models::{
            ClientId, PromptSubmission, SubmissionContextIssue, SubmissionId, SubmissionPlacement,
            SubmissionTarget, TurnAdmissionMetadata, TurnId,
        };
        let client_id = ClientId("00000000-0000-0000-0000-000000000048".parse().unwrap());
        let submission_id = SubmissionId("fixture-submission".into());
        let turn_id = TurnId("fixture-submission-turn".into());
        let text = "sanitized pending prompt".to_owned();
        let expected = [
            SessionEventKind::SubmissionAccepted {
                submission: PromptSubmission {
                    id: submission_id.clone(),
                    client_id,
                    text: text.clone(),
                    admission: TurnAdmissionMetadata::default(),
                    requested_placement: SubmissionPlacement::FollowUp,
                    target: SubmissionTarget::OwnTurn {
                        turn_id: turn_id.clone(),
                    },
                    execution_descriptor: None,
                },
            },
            SessionEventKind::SubmissionContextObserved {
                submission_id: submission_id.clone(),
                issue: SubmissionContextIssue::Unavailable,
            },
            SessionEventKind::SubmissionActivated {
                submission_id,
                turn_id,
                client_id,
                text,
                admission: TurnAdmissionMetadata::default(),
            },
            SessionEventKind::SubmissionCancelled {
                submission_id: SubmissionId("fixture-cancelled-submission".into()),
            },
        ];
        let fixture = include_str!("../fixtures/stores/epoch-12-schema-48-events.jsonl");
        let events = fixture
            .lines()
            .skip(54)
            .map(|line| serde_json::from_str::<SessionEvent>(line).expect("strict schema-48 model"))
            .collect::<Vec<_>>();
        assert_eq!(events.len(), expected.len());
        for (event, kind) in events.into_iter().zip(expected) {
            assert_eq!(event.schema_version, 48);
            assert_eq!(event.kind, kind);
        }
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
                assert!(
                    !fixture.source_writer_epochs.is_empty()
                        || !fixture.lifecycle_matrix.source_writer_epochs.is_empty()
                );
            } else {
                assert_eq!(fixture.source_writer_epochs, [] as [u32; 0]);
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
                    let treatment = crate::RELEASED_EVENT_VARIANTS
                        .iter()
                        .find(|variant| variant.kind == pair.event_kind)
                        .expect("declared event kind treatment")
                        .treatment;
                    if treatment == crate::ReleasedEventTreatment::CurrentEquivalent {
                        let event = serde_json::from_str::<SessionEvent>(&payload).unwrap_or_else(
                            |error| {
                                panic!(
                                    "declared current-equivalent pair {}:{} must strictly decode: {error}",
                                    pair.event_schema, pair.event_kind
                                )
                            },
                        );
                        let actual_kind = serde_json::to_value(&event.kind)
                            .expect("strictly decoded event kind must serialize")
                            .as_object()
                            .and_then(|kind| kind.keys().next())
                            .cloned()
                            .expect("strictly decoded event kind");
                        assert_eq!(actual_kind, pair.event_kind);
                    } else {
                        decode_for_migration(&payload, reject_current).unwrap_or_else(|error| {
                            panic!(
                                "declared classification pair {}:{} must decode: {error}",
                                pair.event_schema, pair.event_kind
                            )
                        });
                    }
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

            let decoded = payloads
                .iter()
                .map(|payload| {
                    decode_for_migration(payload, |payload| {
                        serde_json::from_str::<SessionEvent>(payload)
                            .map_err(|error| error.to_string())
                    })
                })
                .collect::<Result<Vec<_>, _>>()
                .expect("fixture migration classifications");
            for (event, envelope) in decoded.iter().zip(&envelopes) {
                let actual_kind = serde_json::to_value(&event.event().kind)
                    .expect("normalized fixture event kind")
                    .as_object()
                    .and_then(|kind| kind.keys().next())
                    .cloned()
                    .expect("normalized event kind");
                assert!(
                    !matches!(event, HistoricalDecode::Current(_))
                        || envelope.kind.contains_key(&actual_kind),
                    "current-equivalent decoding changed fixture event kind {actual_kind}"
                );
                assert_eq!(event.event().sequence, envelope.sequence);
            }
            assert_eq!(
                decoded
                    .iter()
                    .filter(|event| matches!(event, HistoricalDecode::Current(_)))
                    .count(),
                fixture.expected_classifications.current_passthrough,
                "fixture {}",
                fixture.path.display()
            );
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
            ("session_forked", 45),
            ("session_forked", 52),
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
                SessionEventKind::InertHistory { ref event_type, ref payload }
                    if event_type == event_kind && payload["preserved"] == true
            ));
        }
    }

    #[test]
    fn schema_53_rejects_unpublished_and_already_retired_families() {
        for event_kind in [
            "durable_interaction_committed",
            "turn_continuation_committed",
            "session_forked",
        ] {
            let payload = format!(
                r#"{{"schema_version":53,"sequence":1,"session_id":"{SESSION_ID}","kind":{{"{event_kind}":{{}}}}}}"#
            );
            assert!(matches!(
                decode_for_migration(&payload, reject_current),
                Err(HistoricalSessionEventError::UnsupportedEventKind { .. })
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
        let SessionEventKind::InertHistory {
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
                        SessionEventKind::InertHistory { event_type, payload }
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
    fn epoch_18_fixtures_preserve_exact_typed_events_and_harness_evidence() {
        use bcode_session_models::external_backend::ExternalBackendEnvelope;
        use bcode_session_models::external_harness::ExternalHarnessOperationRecord;

        for (fixture, count) in [
            (
                include_str!("../fixtures/stores/epoch-18-schema-53-events.jsonl"),
                75,
            ),
            (
                include_str!("../fixtures/stores/epoch-18-schema-53-harness.jsonl"),
                9,
            ),
        ] {
            assert_eq!(fixture.lines().count(), count);
            for (sequence, payload) in fixture.lines().enumerate() {
                let source: SessionEvent = serde_json::from_str(payload).unwrap();
                assert_eq!(source.schema_version, 53);
                assert_eq!(source.sequence, u64::try_from(sequence).unwrap());
                let value: serde_json::Value = serde_json::from_str(payload).unwrap();
                let event_kind = value["kind"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .next()
                    .unwrap()
                    .clone();
                let normalized =
                    normalize_canonical_row(&bcode_session_migration_target::CanonicalRow {
                        sequence: source.sequence,
                        event_kind: event_kind.clone(),
                        schema_version: 53,
                        payload: payload.to_owned(),
                    })
                    .unwrap();
                assert_eq!(
                    normalized.historical_source,
                    Some(format!("53:{event_kind}"))
                );
                assert!(!normalized.retired_known);
                assert_eq!(normalized.metric_counter, None);
                let mut expected = source;
                expected.schema_version = crate::CURRENT_EVENT_SCHEMA;
                assert_eq!(normalized.event, expected);
                let mut future = value;
                future["schema_version"] = serde_json::json!(crate::CURRENT_EVENT_SCHEMA + 1);
                assert!(matches!(
                    normalize_canonical_row(&bcode_session_migration_target::CanonicalRow {
                        sequence: expected.sequence,
                        event_kind: event_kind.clone(),
                        schema_version: crate::CURRENT_EVENT_SCHEMA + 1,
                        payload: future.to_string(),
                    }),
                    Err(HistoricalSessionEventError::UnsupportedSchema { .. })
                ));
                if let SessionEventKind::ExternalHarnessOperation { operation } = &expected.kind {
                    operation.validate().unwrap();
                    if let ExternalHarnessOperationRecord::ObservationPage { page, .. } =
                        &operation.record
                    {
                        // Schema-53 replay retains only the recorded evidence. A settlement or
                        // zero queued work is never promoted into a new native terminal outcome.
                        assert!(page.observations.iter().all(|observation| {
                            let kind = serde_json::to_value(&observation.kind).unwrap();
                            !matches!(
                                kind["kind"].as_str(),
                                Some(
                                    "native_turn_terminal"
                                        | "submission_consumed"
                                        | "submission_result_observed"
                                )
                            )
                        }));
                    }
                } else {
                    let _ = ExternalBackendEnvelope::from_event(&expected).unwrap();
                }
            }
        }
    }

    #[test]
    fn epoch_18_harness_replay_preserves_exact_page_cursors_and_loss() {
        use bcode_session_models::external_harness::ExternalHarnessOperationRecord;
        let pages = include_str!("../fixtures/stores/epoch-18-schema-53-harness.jsonl")
            .lines()
            .filter_map(|payload| {
                let event: SessionEvent = serde_json::from_str(payload).unwrap();
                let SessionEventKind::ExternalHarnessOperation { operation } = event.kind else {
                    return None;
                };
                let ExternalHarnessOperationRecord::ObservationPage { after, page, .. } =
                    operation.record
                else {
                    return None;
                };
                let kinds = page
                    .observations
                    .iter()
                    .map(|observation| {
                        serde_json::to_value(&observation.kind).unwrap()["kind"]
                            .as_str()
                            .unwrap()
                            .to_owned()
                    })
                    .collect::<Vec<_>>();
                Some((
                    event.sequence,
                    after,
                    page.next_after,
                    kinds,
                    serde_json::to_value(page.completeness).unwrap(),
                ))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            pages,
            vec![
                (
                    5,
                    None,
                    Some("accepted".into()),
                    vec!["submission_accepted".into()],
                    serde_json::json!("complete"),
                ),
                (
                    6,
                    Some("accepted".into()),
                    Some("result".into()),
                    vec!["result".into()],
                    serde_json::json!("complete"),
                ),
                (
                    7,
                    Some("result".into()),
                    Some("settled".into()),
                    vec!["settlement".into()],
                    serde_json::json!("complete"),
                ),
                (
                    8,
                    Some("settled".into()),
                    Some("gap".into()),
                    vec![],
                    serde_json::json!("gap"),
                ),
            ]
        );
    }

    #[test]
    fn schema_53_harness_pages_reject_new_nested_evidence() {
        for kind in [
            "submission_consumed",
            "submission_result_observed",
            "native_turn_terminal",
        ] {
            let payload = serde_json::json!({
                "schema_version": 53,
                "sequence": 0,
                "timestamp_ms": 1,
                "session_id": "00000000-0000-0000-0000-000000000001",
                "kind": {
                    "external_harness_operation": {
                        "operation": {
                            "record": {
                                "kind": "observation_page",
                                "page": { "observations": [{ "kind": { "kind": kind } }] }
                            }
                        }
                    }
                }
            });
            let envelope: HistoricalEnvelope = serde_json::from_value(payload).unwrap();
            assert!(matches!(
                envelope.validate_observation_compatibility(),
                Err(HistoricalSessionEventError::InvalidEvent {
                    schema_version: 53,
                    ..
                })
            ));
        }
    }

    #[test]
    fn schema_53_observations_preserve_evidence_class_without_inventing_terminals() {
        let base: serde_json::Value = serde_json::from_str(
            include_str!("../fixtures/stores/epoch-16-schema-52-events.jsonl")
                .lines()
                .next()
                .unwrap(),
        )
        .unwrap();
        let session_id = base["session_id"].as_str().unwrap();
        for kind in [
            serde_json::json!({"kind": "text", "text": "historical progress"}),
            serde_json::json!({"kind": "result", "text": "historical result"}),
            serde_json::json!({
                "kind": "submission_accepted", "submission_id": "submission",
                "attempt_id": "attempt", "native_turn_id": "turn"
            }),
            serde_json::json!({"kind": "settlement", "native_turn_id": "turn"}),
        ] {
            let mut source = base.clone();
            source["schema_version"] = serde_json::json!(53);
            source["kind"] = serde_json::json!({
                "external_observation_recorded": {
                    "version": 1,
                    "observation": {
                        "observation_id": "old-observation", "session_id": session_id,
                        "generation": 1, "native_correlations": ["turn"],
                        "completeness": "complete", "kind": kind
                    }
                }
            });
            let normalized =
                normalize_canonical_row(&bcode_session_migration_target::CanonicalRow {
                    sequence: source["sequence"].as_u64().unwrap(),
                    event_kind: "external_observation_recorded".into(),
                    schema_version: 53,
                    payload: source.to_string(),
                })
                .unwrap();
            assert_eq!(normalized.event.schema_version, crate::CURRENT_EVENT_SCHEMA);
            assert_eq!(
                normalized.historical_source.as_deref(),
                Some("53:external_observation_recorded")
            );
            assert!(!normalized.retired_known);
            let converted = serde_json::to_value(normalized.event).unwrap();
            assert_eq!(converted["kind"], source["kind"]);
            assert_eq!(converted["sequence"], source["sequence"]);
            assert_eq!(converted["timestamp_ms"], source["timestamp_ms"]);
            assert_eq!(converted["session_id"], source["session_id"]);
            assert_eq!(converted["provenance"], source["provenance"]);

            source["schema_version"] = serde_json::json!(crate::CURRENT_EVENT_SCHEMA + 1);
            assert!(matches!(
                normalize_canonical_row(&bcode_session_migration_target::CanonicalRow {
                    sequence: source["sequence"].as_u64().unwrap(),
                    event_kind: "external_observation_recorded".into(),
                    schema_version: crate::CURRENT_EVENT_SCHEMA + 1,
                    payload: source.to_string(),
                }),
                Err(HistoricalSessionEventError::UnsupportedSchema { .. })
            ));
        }
    }

    #[test]
    fn schema_53_rejects_new_closed_observation_variants() {
        let mut source: serde_json::Value = serde_json::from_str(
            include_str!("../fixtures/stores/epoch-16-schema-52-events.jsonl")
                .lines()
                .next()
                .unwrap(),
        )
        .unwrap();
        for kind in [
            serde_json::json!({
                "kind": "submission_consumed", "submission_id": "submission",
                "attempt_id": "attempt", "native_frame_id": "frame"
            }),
            serde_json::json!({
                "kind": "submission_result_observed", "submission_id": "submission",
                "attempt_id": "attempt", "native_frame_id": "frame",
                "is_error": false, "queued_user_sends": 0
            }),
            serde_json::json!({
                "kind": "native_turn_terminal", "native_turn_id": "turn",
                "disposition": "completed"
            }),
        ] {
            source["schema_version"] = serde_json::json!(53);
            source["kind"] = serde_json::json!({
                "external_observation_recorded": {
                    "version": 1,
                    "observation": {
                        "observation_id": "new-observation", "session_id": source["session_id"],
                        "generation": 1, "native_correlations": ["turn"],
                        "completeness": "complete", "kind": kind
                    }
                }
            });
            let mut row = bcode_session_migration_target::CanonicalRow {
                sequence: source["sequence"].as_u64().unwrap(),
                event_kind: "external_observation_recorded".into(),
                schema_version: 53,
                payload: source.to_string(),
            };
            assert!(matches!(
                normalize_canonical_row(&row),
                Err(HistoricalSessionEventError::InvalidEvent {
                    schema_version: 53,
                    ..
                })
            ));
            source["schema_version"] = serde_json::json!(crate::CURRENT_EVENT_SCHEMA);
            row.schema_version = crate::CURRENT_EVENT_SCHEMA;
            row.payload = source.to_string();
            assert!(normalize_canonical_row(&row).is_ok());
        }
    }

    #[test]
    fn epoch_17_replay_preserves_normalized_external_evidence_without_vendor_runtime() {
        use bcode_session_models::external_backend::ExternalBackendEnvelope;
        let mut event = serde_json::from_str::<SessionEvent>(
            include_str!("../fixtures/stores/epoch-16-schema-52-events.jsonl")
                .lines()
                .next()
                .unwrap(),
        )
        .unwrap();
        let record = serde_json::from_value(serde_json::json!({
            "kind": "observation",
            "observation": {
                "observation_id": "external-output", "session_id": event.session_id.to_string(),
                "generation": 1, "native_correlations": ["native-turn"], "completeness": "partial",
                "kind": { "kind": "text", "text": "retained offline: 🦀" }
            }
        }))
        .unwrap();
        let envelope = ExternalBackendEnvelope { version: 1, record };
        event.schema_version = crate::CURRENT_EVENT_SCHEMA;
        event.kind = envelope.clone().into_event_kind().unwrap();
        let payload = serde_json::to_string(&event).unwrap();
        // Typed external evidence was introduced at schema 53. Never accept it as
        // a fabricated schema-52 event merely because serde recognizes its shape.
        let mut historical = event.clone();
        historical.schema_version = 52;
        assert!(matches!(
            normalize_canonical_row(&bcode_session_migration_target::CanonicalRow {
                sequence: historical.sequence,
                event_kind: "external_observation_recorded".into(),
                schema_version: 52,
                payload: serde_json::to_string(&historical).unwrap(),
            }),
            Err(HistoricalSessionEventError::UnsupportedEventKind {
                schema_version: 52,
                ..
            })
        ));
        let normalized = normalize_canonical_row(&bcode_session_migration_target::CanonicalRow {
            sequence: event.sequence,
            event_kind: "external_observation_recorded".into(),
            schema_version: event.schema_version,
            payload,
        })
        .unwrap();
        assert_eq!(normalized.event, event);
        assert_eq!(
            ExternalBackendEnvelope::from_event(&normalized.event).unwrap(),
            Some(envelope)
        );
        assert!(normalized.metric_counter.is_none());
        let plan = crate::plan_writer_epoch_migration(17).unwrap();
        assert_eq!(plan.target_writer_epoch, crate::CURRENT_WRITER_EPOCH);
        assert_eq!(plan.steps.len(), 7);
    }

    #[test]
    fn epoch_16_replay_preserves_identity_through_historical_conversion() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let manifest = crate::load_released_fixture_manifest(&root).expect("fixture inventory");
        let source_schema = crate::RELEASED_WRITER_SCHEMA_COMBINATIONS
            .iter()
            .find(|source| source.writer_epoch == 16)
            .expect("epoch 16 source contract")
            .event_schema;
        let fixtures = manifest.fixtures.iter().filter(|fixture| {
            fixture.lifecycle_matrix.source_writer_epochs.contains(&16)
                && fixture.path == std::path::Path::new("stores/epoch-16-schema-52-events.jsonl")
                && fixture.event_schemas == [source_schema]
        });
        let mut replayed = 0;
        for fixture in fixtures {
            let plan = crate::plan_writer_epoch_migration(16).expect("registered lifecycle plan");
            assert_eq!(plan.target_writer_epoch, crate::CURRENT_WRITER_EPOCH);
            let payloads =
                std::fs::read_to_string(root.join(&fixture.path)).expect("fixture payloads");
            for payload in payloads.lines() {
                let source: serde_json::Value =
                    serde_json::from_str(payload).expect("epoch 16 envelope");
                let kind = source["kind"].as_object().unwrap().keys().next().unwrap();
                let normalized =
                    normalize_canonical_row(&bcode_session_migration_target::CanonicalRow {
                        sequence: source["sequence"].as_u64().unwrap(),
                        event_kind: kind.clone(),
                        schema_version: source_schema,
                        payload: payload.to_owned(),
                    })
                    .expect("lossless projection rebuild input");
                assert_eq!(normalized.event.schema_version, crate::CURRENT_EVENT_SCHEMA);
                assert_eq!(
                    serde_json::to_value(normalized.event.session_id).unwrap(),
                    source["session_id"]
                );
                assert_eq!(
                    normalized.event.sequence,
                    source["sequence"].as_u64().unwrap()
                );
                assert_eq!(
                    normalized.event.timestamp_ms,
                    source["timestamp_ms"].as_u64().unwrap()
                );
                assert_eq!(
                    serde_json::to_value(&normalized.event.provenance).unwrap(),
                    source["provenance"]
                );
                assert_eq!(normalized.historical_source, Some(format!("52:{kind}")));
                let historical = decode_for_migration(payload, |payload| {
                    serde_json::from_str::<SessionEvent>(payload).map_err(|error| error.to_string())
                })
                .expect("registered historical conversion");
                assert_eq!(normalized.retired_known, historical.is_retired_known());
                let mut expected = historical.into_event();
                expected.schema_version = crate::CURRENT_EVENT_SCHEMA;
                assert_eq!(normalized.event, expected);
                replayed += 1;
            }
        }
        assert!(
            replayed > 0,
            "registered epoch 16 lifecycle input must be replayed"
        );
        let fixture = include_str!("../fixtures/stores/epoch-16-schema-52-events.jsonl");
        // A future schema is not made current even if its event shape is familiar.
        let mut future: serde_json::Value =
            serde_json::from_str(fixture.lines().next().unwrap()).unwrap();
        future["schema_version"] = serde_json::json!(crate::CURRENT_EVENT_SCHEMA + 1);
        let payload = future.to_string();
        assert!(matches!(
            normalize_canonical_row(&bcode_session_migration_target::CanonicalRow {
                sequence: future["sequence"].as_u64().expect("future event sequence"),
                event_kind: future["kind"]
                    .as_object()
                    .expect("future event kind")
                    .keys()
                    .next()
                    .expect("one event kind")
                    .clone(),
                schema_version: u16::try_from(
                    future["schema_version"].as_u64().expect("future schema")
                )
                .expect("schema fits u16"),
                payload: payload.clone(),
            }),
            Err(HistoricalSessionEventError::UnsupportedSchema { .. })
        ));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&payload).unwrap(),
            future
        );
    }

    #[test]
    fn compaction_checkpoint_replay_preserves_operational_evidence() {
        use bcode_session_models::CompactionSynthesisCheckpoint;

        for host_payload in [Some("accepted synthesis: 🦀".to_owned()), None] {
            let checkpoint = CompactionSynthesisCheckpoint {
                version: 1,
                operation_key: "host-v1/source/policy".to_owned(),
                host_payload,
            };
            let mut event = serde_json::from_str::<SessionEvent>(
                include_str!("../fixtures/stores/epoch-16-schema-52-events.jsonl")
                    .lines()
                    .next()
                    .expect("session fixture"),
            )
            .expect("fixture event");
            event.schema_version = crate::CURRENT_EVENT_SCHEMA;
            event.kind = checkpoint
                .clone()
                .into_event_kind(1)
                .expect("checkpoint event");
            let payload = serde_json::to_string(&event).expect("canonical payload");
            let normalized =
                normalize_canonical_row(&bcode_session_migration_target::CanonicalRow {
                    sequence: event.sequence,
                    event_kind: bcode_session_models::persisted_session_event_kind_name(
                        &event.kind,
                    )
                    .to_owned(),
                    schema_version: event.schema_version,
                    payload: payload.clone(),
                })
                .expect("replay operational evidence");
            assert_eq!(normalized.event, event);
            assert_eq!(serde_json::to_string(&normalized.event).unwrap(), payload);
            assert_eq!(
                CompactionSynthesisCheckpoint::from_event(&normalized.event).unwrap(),
                Some(checkpoint)
            );
            assert!(normalized.metric_counter.is_none());
        }
    }

    #[test]
    fn unknown_compaction_envelopes_are_preserved_not_reinterpreted() {
        use bcode_session_models::{CompactionSynthesisCheckpoint, SessionTracePayload};

        let checkpoint = CompactionSynthesisCheckpoint {
            version: 1,
            operation_key: "host-v1/source/policy".to_owned(),
            host_payload: Some("accepted synthesis".to_owned()),
        };
        let kind = checkpoint
            .clone()
            .into_event_kind(1)
            .expect("checkpoint event");
        for envelope in [
            serde_json::json!({"version": 2, "operation_key": checkpoint.operation_key,
                "host_payload": "future payload"}),
            serde_json::json!({"version": 1, "operation_key": checkpoint.operation_key,
                "host_payload": "payload", "future_field": true}),
        ] {
            let mut event = serde_json::from_str::<SessionEvent>(
                include_str!("../fixtures/stores/epoch-16-schema-52-events.jsonl")
                    .lines()
                    .next()
                    .expect("session fixture"),
            )
            .expect("fixture event");
            event.schema_version = crate::CURRENT_EVENT_SCHEMA;
            event.kind = kind.clone();
            let SessionEventKind::TraceEvent { trace } = &mut event.kind else {
                panic!("checkpoint must be a trace event");
            };
            let SessionTracePayload::ProviderEvent { detail, .. } = &mut trace.payload else {
                panic!("checkpoint must be provider evidence");
            };
            *detail = Some(envelope.to_string());
            let payload = serde_json::to_string(&event).unwrap();
            let normalized =
                normalize_canonical_row(&bcode_session_migration_target::CanonicalRow {
                    sequence: event.sequence,
                    event_kind: bcode_session_models::persisted_session_event_kind_name(
                        &event.kind,
                    )
                    .to_owned(),
                    schema_version: event.schema_version,
                    payload: payload.clone(),
                })
                .expect("normalization must not rewrite opaque provider details");
            assert_eq!(serde_json::to_string(&normalized.event).unwrap(), payload);
            // The current projector must reject rather than accept or downgrade this envelope.
            assert!(CompactionSynthesisCheckpoint::from_event(&normalized.event).is_err());
        }
    }

    #[test]
    fn canonical_normalization_always_materializes_the_requested_current_schema() {
        let current_payload = format!(
            r#"{{"schema_version":39,"sequence":1,"session_id":"{SESSION_ID}","kind":{{"session_created":{{"summary":{{"id":"{SESSION_ID}","title":"fixture","cwd":"/tmp","created_at_ms":1,"updated_at_ms":1}}}}}}}}"#
        );
        let normalized = normalize_canonical_event(&current_payload, 42, |payload| {
            serde_json::from_str(payload).map_err(|error| error.to_string())
        })
        .expect("normalize current-compatible event");
        assert_eq!(normalized.event.schema_version, 42);
        assert!(normalized.historical.is_none());
        assert!(!normalized.retired_known);
        assert!(normalized.metric_counter.is_none());

        let historical_payload = format!(
            r#"{{"schema_version":28,"sequence":8,"timestamp_ms":9,"session_id":"{SESSION_ID}","kind":{{"context_usage_observed":{{"snapshot":{{"provider_plugin_id":"provider","model_id":"model","input_tokens":123,"context_through_sequence":4,"request_id":"request","model_turn_id":"turn","round":0,"request_fingerprint":"fingerprint","auth_profile":"profile","estimated_input_tokens":120,"context_format_version":null,"compatibility_key":null,"source":"estimated"}}}}}}}}"#
        );
        let normalized = normalize_canonical_event(&historical_payload, 42, reject_current)
            .expect("normalize historical event");
        assert_eq!(normalized.event.schema_version, 42);
        assert_eq!(normalized.historical.expect("metadata").source_schema, 28);
        assert!(!normalized.retired_known);
        assert_eq!(
            normalized.metric_counter,
            Some(metric::CONVERTED_CONTEXT_USAGE_OBSERVED)
        );
    }

    #[test]
    fn epoch_5_schema_40_fixture_normalizes_to_current_without_reinterpreting_kinds() {
        let payloads = include_str!("../fixtures/stores/epoch-5-schema-40-events.jsonl");
        for (event_seq, payload) in payloads.lines().enumerate() {
            let source = serde_json::from_str::<FixtureEnvelope>(payload).expect("schema-40 event");
            let source_kind = source.kind.keys().next().expect("schema-40 event kind");
            let normalized =
                normalize_canonical_row(&bcode_session_migration_target::CanonicalRow {
                    sequence: u64::try_from(event_seq).expect("fixture sequence"),
                    event_kind: source_kind.clone(),
                    schema_version: 40,
                    payload: payload.to_owned(),
                })
                .expect("schema-40 event should normalize");

            assert_eq!(normalized.event.schema_version, crate::CURRENT_EVENT_SCHEMA);
            assert_eq!(normalized.event.sequence, source.sequence);
            let retired = crate::RELEASED_EVENT_VARIANTS
                .iter()
                .find(|variant| variant.kind == source_kind)
                .expect("inventoried fixture kind")
                .treatment
                == crate::ReleasedEventTreatment::RetiredKnown;
            assert_eq!(normalized.retired_known, retired, "{source_kind}");
            if retired {
                assert!(matches!(
                    normalized.event.kind,
                    SessionEventKind::InertHistory { ref event_type, .. }
                        if event_type == source_kind
                ));
                continue;
            }
            assert_eq!(
                serde_json::to_value(&normalized.event.kind)
                    .expect("normalized kind")
                    .as_object()
                    .and_then(|kind| kind.keys().next()),
                Some(source_kind)
            );
        }
    }

    #[test]
    fn historical_codec_only_applies_family_rules_to_released_schema_ranges() {
        let unreleased = crate::NEVER_RELEASED_EVENT_SCHEMAS[2];
        let payload = format!(
            r#"{{"schema_version":{unreleased},"sequence":1,"session_id":"{SESSION_ID}","kind":{{"tool_call_finished":{{"tool_call_id":"call","result":"done"}}}}}}"#
        );
        assert!(matches!(
            decode_for_migration(&payload, reject_current),
            Err(HistoricalSessionEventError::UnsupportedSchema { schema_version })
                if schema_version == unreleased
        ));

        let future = crate::CURRENT_EVENT_SCHEMA + 1;
        let payload = format!(
            r#"{{"schema_version":{future},"sequence":1,"session_id":"{SESSION_ID}","kind":{{"user_message":{{"client_id":"00000000-0000-0000-0000-0000000000ff","text":"hello"}}}}}}"#
        );
        assert!(matches!(
            decode_for_migration(&payload, reject_current),
            Err(HistoricalSessionEventError::UnsupportedSchema { schema_version })
                if schema_version == future
        ));

        let payload = format!(
            r#"{{"schema_version":44,"sequence":1,"session_id":"{SESSION_ID}","kind":{{"tool_call_finished":{{"tool_call_id":"call","result":"done"}}}}}}"#
        );
        assert!(matches!(
            decode_for_migration(&payload, reject_current),
            Err(HistoricalSessionEventError::UnsupportedEventKind {
                schema_version: 44,
                ..
            })
        ));

        let payload = format!(
            r#"{{"schema_version":40,"sequence":1,"session_id":"{SESSION_ID}","kind":{{"tool_call_finished":{{"tool_call_id":"call","result":"done"}}}}}}"#
        );
        assert!(matches!(
            decode_for_migration(&payload, reject_current),
            Err(HistoricalSessionEventError::UnsupportedEventKind {
                schema_version: 40,
                ..
            })
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
    fn schema_51_child_creation_upgrade_retains_identity_without_inventing_spawning() {
        let payload = serde_json::json!({
            "schema_version": 51, "sequence": 1, "timestamp_ms": 13,
            "session_id": SESSION_ID,
            "kind": { "child_session_created": { "request": {
                "child_id": SESSION_ID,
                "parent_id": "00000000-0000-0000-0000-000000000002",
                "name": "historical child", "working_directory": "/tmp"
            } } }
        });
        let normalized = normalize_canonical_row(&bcode_session_migration_target::CanonicalRow {
            sequence: 1,
            event_kind: "child_session_created".to_owned(),
            schema_version: 51,
            payload: payload.to_string(),
        })
        .expect("supported schema-51 creation");
        assert_eq!(normalized.event.schema_version, crate::CURRENT_EVENT_SCHEMA);
        assert_eq!(normalized.event.sequence, 1);
        assert_eq!(normalized.event.timestamp_ms, 13);
        let SessionEventKind::ChildSessionCreated { request, spawning } = normalized.event.kind
        else {
            panic!("creation identity must stay active");
        };
        assert_eq!(
            serde_json::to_value(request).unwrap(),
            payload["kind"]["child_session_created"]["request"]
        );
        assert_eq!(spawning, None);
        assert_eq!(
            normalized.historical_source.as_deref(),
            Some("51:child_session_created")
        );
        assert!(!normalized.retired_known);
    }

    #[test]
    fn schema_51_submission_preserves_null_and_v1_execution_descriptors() {
        use bcode_session_models::{
            ClientId, PromptSubmission, SubmissionId, SubmissionPlacement, SubmissionTarget,
            TurnAdmissionMetadata, TurnId,
        };
        let descriptor = bcode_session_models::SubmissionExecutionDescriptor {
            version: 1,
            provider: "provider".into(),
            model: "model".into(),
            reasoning: None,
            model_profile: Some("coding".into()),
            auth_profile: Some("work".into()),
            config_identity: "config-digest".into(),
            recovery_recipe: None,
        };
        for execution_descriptor in [None, Some(descriptor)] {
            let source = SessionEvent {
                schema_version: 51,
                session_id: SESSION_ID.parse().unwrap(),
                sequence: 1,
                timestamp_ms: 2,
                provenance: None,
                kind: SessionEventKind::SubmissionAccepted {
                    submission: PromptSubmission {
                        id: SubmissionId("retained-submission".into()),
                        client_id: ClientId::new(),
                        text: "preserve exact input".into(),
                        admission: TurnAdmissionMetadata::default(),
                        requested_placement: SubmissionPlacement::FollowUp,
                        target: SubmissionTarget::OwnTurn {
                            turn_id: TurnId("retained-turn".into()),
                        },
                        execution_descriptor,
                    },
                },
            };
            let payload = serde_json::to_string(&source).unwrap();
            let normalized =
                normalize_canonical_row(&bcode_session_migration_target::CanonicalRow {
                    sequence: 1,
                    event_kind: "submission_accepted".into(),
                    schema_version: 51,
                    payload,
                })
                .unwrap();
            let mut expected = source;
            expected.schema_version = crate::CURRENT_EVENT_SCHEMA;
            assert_eq!(normalized.event, expected);
            assert_eq!(
                normalized.historical_source.as_deref(),
                Some("51:submission_accepted")
            );
            assert!(!normalized.retired_known);
        }
    }

    #[test]
    fn schema_51_upgrade_preserves_other_events_and_rejects_fabricated_composition() {
        let payload = serde_json::json!({
            "schema_version": 51, "sequence": 7, "timestamp_ms": 13,
            "session_id": SESSION_ID,
            "kind": { "assistant_message": { "text": "preserved" } }
        });
        let normalized = normalize_canonical_row(&bcode_session_migration_target::CanonicalRow {
            sequence: 7,
            event_kind: "assistant_message".to_owned(),
            schema_version: 51,
            payload: payload.to_string(),
        })
        .unwrap();
        let mut expected = payload.clone();
        expected["schema_version"] = crate::CURRENT_EVENT_SCHEMA.into();
        expected["provenance"] = serde_json::Value::Null;
        assert_eq!(serde_json::to_value(normalized.event).unwrap(), expected);
        assert_eq!(
            normalized.historical_source.as_deref(),
            Some("51:assistant_message")
        );
        let invalid = serde_json::json!({
            "schema_version": 51, "sequence": 1, "session_id": SESSION_ID,
            "kind": { "child_session_created": { "request": {
                "child_id": SESSION_ID,
                "parent_id": "00000000-0000-0000-0000-000000000002",
                "name": null, "working_directory": "/tmp"
            }, "spawning": null } }
        });
        assert!(matches!(
            decode_for_migration(&invalid.to_string(), reject_current),
            Err(HistoricalSessionEventError::InvalidEvent {
                schema_version: 51,
                ..
            })
        ));
        let mut future = payload;
        future["schema_version"] = (crate::CURRENT_EVENT_SCHEMA + 1).into();
        assert!(matches!(
            decode_for_migration(&future.to_string(), reject_current),
            Err(HistoricalSessionEventError::UnsupportedSchema { .. })
        ));
    }

    #[test]
    fn historical_harness_operations_prepare_empty_inventory_commitments_without_execution() {
        for fixture in [
            include_str!("../fixtures/stores/epoch-18-schema-53-harness.jsonl"),
            include_str!("../fixtures/stores/epoch-19-schema-54-harness.jsonl"),
        ] {
            let mut operations = 0;
            for payload in fixture.lines() {
                let source: serde_json::Value = serde_json::from_str(payload).unwrap();
                let event_kind = source["kind"].as_object().unwrap().keys().next().unwrap();
                let normalized =
                    normalize_canonical_row(&bcode_session_migration_target::CanonicalRow {
                        sequence: source["sequence"].as_u64().unwrap(),
                        event_kind: event_kind.clone(),
                        schema_version: u16::try_from(source["schema_version"].as_u64().unwrap())
                            .unwrap(),
                        payload: payload.into(),
                    })
                    .expect("historical prepared operation");
                if let SessionEventKind::ExternalHarnessOperation { operation } =
                    &normalized.event.kind
                {
                    operations += 1;
                    assert!(
                        operation.inventory_commitments.is_empty(),
                        "normalization cannot invent stateful projector roots"
                    );
                    let mut expected =
                        source["kind"]["external_harness_operation"]["operation"].clone();
                    expected["inventory_commitments"] = serde_json::json!([]);
                    assert_eq!(
                        serde_json::to_value(operation).unwrap(),
                        expected,
                        "all source evidence and identities survive preparation"
                    );
                    operation.validate().unwrap();
                }
            }
            assert!(operations > 0);
        }
    }

    #[test]
    fn schema_54_retired_fork_fixture_preserves_original_payload_as_inert_history() {
        let payload = include_str!("../fixtures/stores/epoch-19-schema-54-retired-fork.jsonl");
        let source: serde_json::Value = serde_json::from_str(payload).unwrap();
        let normalized = normalize_canonical_row(&bcode_session_migration_target::CanonicalRow {
            sequence: 0,
            event_kind: "session_forked".into(),
            schema_version: 54,
            payload: payload.into(),
        })
        .expect("released retired fork normalization");
        assert!(normalized.retired_known);
        assert_eq!(
            normalized.historical_source.as_deref(),
            Some("54:session_forked")
        );
        let mut expected = source.clone();
        expected["schema_version"] = crate::CURRENT_EVENT_SCHEMA.into();
        expected["kind"] = serde_json::json!({"inert_history":{
            "event_type":"session_forked", "payload":source["kind"]["session_forked"]
        }});
        assert_eq!(serde_json::to_value(normalized.event).unwrap(), expected);
    }

    #[test]
    fn schema_54_fixture_preserves_every_released_kind_and_evidence() {
        let mut kinds = std::collections::BTreeSet::new();
        for (fixture, count) in [
            (
                include_str!("../fixtures/stores/epoch-19-schema-54-events.jsonl"),
                75,
            ),
            (
                include_str!("../fixtures/stores/epoch-19-schema-54-harness.jsonl"),
                12,
            ),
        ] {
            assert_eq!(fixture.lines().count(), count);
            for (sequence, payload) in fixture.lines().enumerate() {
                let source: SessionEvent = serde_json::from_str(payload).unwrap();
                let kind = bcode_session_models::persisted_session_event_kind_name(&source.kind);
                kinds.insert(kind);
                let normalized =
                    normalize_canonical_row(&bcode_session_migration_target::CanonicalRow {
                        sequence: source.sequence,
                        event_kind: kind.into(),
                        schema_version: 54,
                        payload: payload.into(),
                    })
                    .expect("released schema-54 event");
                let mut expected = source;
                expected.schema_version = crate::CURRENT_EVENT_SCHEMA;
                assert_eq!(expected.sequence, u64::try_from(sequence).unwrap());
                assert_eq!(normalized.event, expected);
                assert!(!normalized.retired_known);
                if let SessionEventKind::ExternalHarnessOperation { operation } =
                    &normalized.event.kind
                {
                    operation.validate().unwrap();
                    let value = serde_json::to_value(operation).unwrap();
                    if let Some(observations) = value
                        .pointer("/record/page/observations")
                        .and_then(serde_json::Value::as_array)
                    {
                        assert!(
                            observations
                                .iter()
                                .all(|observation| observation["kind"]["kind"] != "execution")
                        );
                    }
                }
            }
        }
        let expected: std::collections::BTreeSet<_> = crate::RELEASED_EVENT_VARIANTS
            .iter()
            .filter(|variant| crate::is_released_event_kind_schema(variant.kind, 54))
            .filter(|variant| variant.treatment != crate::ReleasedEventTreatment::RetiredKnown)
            .map(|variant| variant.kind)
            .collect();
        assert_eq!(kinds, expected);
    }

    #[test]
    fn schema_54_retains_v2_observations_in_both_envelopes() {
        let mut checked = 0;
        for line in include_str!("../fixtures/stores/epoch-19-schema-54-harness.jsonl").lines() {
            let mut source: serde_json::Value = serde_json::from_str(line).unwrap();
            let Some(observation) = source
                .pointer("/kind/external_harness_operation/operation/record/page/observations/0")
                .cloned()
            else {
                continue;
            };
            if !matches!(
                observation["kind"]["kind"].as_str(),
                Some("submission_consumed" | "submission_result_observed" | "native_turn_terminal")
            ) {
                continue;
            }
            checked += 1;
            source["kind"] = serde_json::json!({"external_observation_recorded":{"version":1,"observation":observation}});
            let row = bcode_session_migration_target::CanonicalRow {
                sequence: source["sequence"].as_u64().unwrap(),
                event_kind: "external_observation_recorded".into(),
                schema_version: 54,
                payload: source.to_string(),
            };
            let converted =
                normalize_canonical_row(&row).expect("v2 evidence remains supported at schema54");
            source["schema_version"] = crate::CURRENT_EVENT_SCHEMA.into();
            assert_eq!(serde_json::to_value(converted.event).unwrap(), source);
            source["schema_version"] = 53.into();
            assert!(matches!(
                normalize_canonical_row(&bcode_session_migration_target::CanonicalRow {
                    schema_version: 53,
                    payload: source.to_string(),
                    ..row
                }),
                Err(HistoricalSessionEventError::InvalidEvent { .. })
            ));
        }
        assert_eq!(checked, 3);
    }

    #[test]
    fn execution_wrapper_is_schema_55_only_in_both_observation_envelopes() {
        let source: serde_json::Value = serde_json::from_str(
            include_str!("../fixtures/stores/epoch-19-schema-54-harness.jsonl")
                .lines()
                .nth(6)
                .unwrap(),
        )
        .unwrap();
        let attempt = serde_json::json!({"submission_id":"submission", "attempt_id":"attempt"});
        for event in [
            serde_json::json!({"event":"submission_terminal", "attempt":attempt,
                "native_turn_id":null, "native_frame_id":"native-frame", "disposition":"completed", "output":"done"}),
            serde_json::json!({"event":"background_work", "work_id":"owned-work", "native_task_id":null,
                "origin":null, "parent_work_id":null, "work_kind":"command", "label":"background",
                "state":"completed", "notification":"pending"}),
            serde_json::json!({"event":"continuation", "continuation_id":"owned-continuation", "origin":attempt,
                "source_work_id":"owned-work", "native_turn_id":null, "state":"running", "output":null}),
            serde_json::json!({"event":"coverage", "attempt":attempt, "background":"unknown", "continuations":"complete"}),
        ] {
            for page_envelope in [false, true] {
                let mut payload = source.clone();
                let mut observation = payload
                    .pointer(
                        "/kind/external_harness_operation/operation/record/page/observations/0",
                    )
                    .unwrap()
                    .clone();
                observation["kind"] = serde_json::json!({"kind":"execution", "event":event});
                let kind = if page_envelope {
                    payload["kind"]["external_harness_operation"]["operation"]["record"]["page"]
                        ["observations"] = serde_json::json!([observation]);
                    "external_harness_operation"
                } else {
                    payload["kind"] = serde_json::json!({"external_observation_recorded":{"version":1,"observation":observation}});
                    "external_observation_recorded"
                };
                for schema in [53, 54] {
                    payload["schema_version"] = schema.into();
                    assert!(matches!(
                        normalize_canonical_row(&bcode_session_migration_target::CanonicalRow {
                            sequence: 6,
                            event_kind: kind.into(),
                            schema_version: schema,
                            payload: payload.to_string(),
                        }),
                        Err(HistoricalSessionEventError::InvalidEvent { .. })
                    ));
                }
                payload["schema_version"] = crate::CURRENT_EVENT_SCHEMA.into();
                let normalized =
                    normalize_canonical_row(&bcode_session_migration_target::CanonicalRow {
                        sequence: 6,
                        event_kind: kind.into(),
                        schema_version: crate::CURRENT_EVENT_SCHEMA,
                        payload: payload.to_string(),
                    })
                    .expect("current execution event");
                if let SessionEventKind::ExternalHarnessOperation { operation } =
                    &normalized.event.kind
                {
                    operation
                        .validate()
                        .expect("valid current execution observation");
                }
                if page_envelope {
                    payload["kind"]["external_harness_operation"]["operation"]["inventory_commitments"] =
                        serde_json::json!([]);
                }
                assert_eq!(serde_json::to_value(normalized.event).unwrap(), payload);
                assert!(!normalized.retired_known);
                assert!(normalized.historical_source.is_none());
            }
        }
    }

    #[test]
    fn managed_claim_metadata_requires_writer_22_even_with_schema_55() {
        use bcode_session_models::{
            ExternalHarnessOwner, ManagedToolClaimEvent, ManagedToolIdentity,
            ToolInvocationLifecycleEvent, ToolInvocationLifecycleStage,
        };
        // Synthetic decoder input only; this is not historical producer evidence.
        let source = include_str!("../fixtures/stores/epoch-19-schema-54-harness.jsonl")
            .lines()
            .nth(6)
            .unwrap();
        let mut event = normalize_canonical_row(&bcode_session_migration_target::CanonicalRow {
            sequence: 6,
            event_kind: "external_harness_operation".into(),
            schema_version: 54,
            payload: source.into(),
        })
        .unwrap()
        .event;
        let identity = ManagedToolIdentity {
            owner: ExternalHarnessOwner {
                session_id: event.session_id,
                generation: 1,
                artifact_id: "artifact".into(),
                daemon_id: "daemon".into(),
            },
            grant_id: "grant".into(),
            invocation_id: "invocation".into(),
            producer_plugin_id: "producer".into(),
            tool_name: "tool".into(),
            arguments_sha256: "a".repeat(64),
            preparation_sha256: "b".repeat(64),
            working_directory: "/workspace".into(),
        };
        event.kind = SessionEventKind::ToolInvocationLifecycle {
            event: ToolInvocationLifecycleEvent {
                invocation_id: identity.invocation_id.clone(),
                sequence: 0,
                stage: ToolInvocationLifecycleStage::Waiting,
                message: None,
                metadata: serde_json::json!({
                    (bcode_session_models::MANAGED_TOOL_CLAIM_KEY):
                        ManagedToolClaimEvent::Claimed { identity }
                }),
            },
        };
        let mut row = bcode_session_migration_target::CanonicalRow {
            sequence: event.sequence,
            event_kind: "tool_invocation_lifecycle".into(),
            schema_version: 55,
            payload: serde_json::to_string(&event).unwrap(),
        };
        for writer in [18, 19, 20, 21] {
            assert!(normalize_canonical_row_for_writer_epoch(&row, writer).is_err());
        }
        for writer in [22, 23] {
            assert_eq!(
                normalize_canonical_row_for_writer_epoch(&row, writer)
                    .unwrap()
                    .event,
                event
            );
        }
        if let SessionEventKind::ToolInvocationLifecycle { event } = &mut event.kind {
            event.metadata = serde_json::json!({});
        }
        row.payload = serde_json::to_string(&event).unwrap();
        for writer in [18, 19, 20, 21, 22] {
            assert_eq!(
                normalize_canonical_row_for_writer_epoch(&row, writer)
                    .unwrap()
                    .event,
                event
            );
        }
    }

    #[test]
    fn writer_scoped_normalization_replaces_only_old_host_commitments() {
        let source: serde_json::Value = serde_json::from_str(
            include_str!("../fixtures/stores/epoch-19-schema-54-harness.jsonl")
                .lines()
                .nth(6)
                .unwrap(),
        )
        .unwrap();
        let mut payload = source;
        payload["schema_version"] = 55.into();
        // This is a normalization unit input, not the historical database fixture.
        let operation = payload
            .pointer_mut("/kind/external_harness_operation/operation")
            .unwrap();
        operation["inventory_commitments"] = serde_json::json!([{
            "snapshot_key": "execution_v3:1:inventory:unknown:work", "revision": 6,
            "root": "a".repeat(64), "summary": {
                "identities": 1, "unfinished": 0, "obligations": 1,
                "failed": 1, "interrupted": 0
            }
        }]);
        let row = bcode_session_migration_target::CanonicalRow {
            sequence: 6,
            event_kind: "external_harness_operation".into(),
            schema_version: 55,
            payload: payload.to_string(),
        };
        let current = normalize_canonical_row(&row).unwrap();
        for writer in [21, 22, 23, 25] {
            let repaired = normalize_canonical_row_for_writer_epoch(&row, writer).unwrap();
            assert_eq!(repaired.event, current.event);
            assert_eq!(
                repaired.historical_source.as_deref(),
                Some("55:external_harness_operation")
            );
        }
        let mut current_payload = payload.clone();
        current_payload["schema_version"] = crate::CURRENT_EVENT_SCHEMA.into();
        assert_eq!(
            serde_json::to_value(&current.event).unwrap(),
            current_payload
        );
        for writer in [18, 19, 20] {
            let migrated = normalize_canonical_row_for_writer_epoch(&row, writer).unwrap();
            let mut expected = current_payload.clone();
            expected["kind"]["external_harness_operation"]["operation"]["inventory_commitments"] =
                serde_json::json!([]);
            assert_eq!(serde_json::to_value(migrated.event).unwrap(), expected);
            assert_eq!(
                migrated.historical_source.as_deref(),
                Some("55:external_harness_operation")
            );
            assert!(!migrated.retired_known);
        }
        let current_row = bcode_session_migration_target::CanonicalRow {
            schema_version: crate::CURRENT_EVENT_SCHEMA,
            payload: current_payload.to_string(),
            ..row.clone()
        };
        for writer in [21, 22, 23, 25] {
            let normalized =
                normalize_canonical_row_for_writer_epoch(&current_row, writer).unwrap();
            assert_eq!(normalized.event, current.event);
            assert!(normalized.historical_source.is_none());
            assert!(!normalized.retired_known);
        }
        for writer in [0, 24] {
            assert!(matches!(
                normalize_canonical_row_for_writer_epoch(&row, writer),
                Err(HistoricalSessionEventError::InvalidEvent { .. })
            ));
        }
        let mut malformed = payload;
        malformed["kind"]["external_harness_operation"]["operation"]["inventory_commitments"][0]
            ["root"] = "invalid".into();
        assert!(
            normalize_canonical_row_for_writer_epoch(
                &bcode_session_migration_target::CanonicalRow {
                    payload: malformed.to_string(),
                    ..row
                },
                20
            )
            .is_err()
        );
    }

    #[test]
    fn recovery_schema_gate_precedes_even_a_successful_current_decoder() {
        for record in ["transport_released", "recovery_prepared"] {
            for schema in [53, 54, 55, 57] {
                let payload = serde_json::json!({
                    "schema_version": schema,
                    "sequence": 0,
                    "timestamp_ms": 0,
                    "session_id": SESSION_ID,
                    "kind": { "external_harness_operation": {
                        "operation": { "record": { "kind": record } }
                    } }
                })
                .to_string();
                // Decoder stub intentionally accepts the input: migration's historical
                // representation gate must reject before adopting that decoder's result.
                let result = decode_for_migration(&payload, |_| {
                    Ok(SessionEvent {
                        schema_version: crate::CURRENT_EVENT_SCHEMA,
                        sequence: 0,
                        timestamp_ms: 0,
                        session_id: SESSION_ID.parse().unwrap(),
                        provenance: None,
                        kind: SessionEventKind::SessionRenamed { name: None },
                    })
                });
                assert!(matches!(result,
                    Err(HistoricalSessionEventError::InvalidEvent { reason, .. })
                        if reason == "recovery operations are unpublished and unsupported"
                ));
            }
        }
    }

    #[test]
    fn recovery_schema_gate_does_not_promote_or_rewrite_older_records() {
        for schema in [53, 54, 55, 56, 57] {
            for kind in ["configured", "transport_released", "recovery_prepared"] {
                let envelope: HistoricalEnvelope = serde_json::from_value(serde_json::json!({
                    "schema_version": schema,
                    "sequence": 0,
                    "session_id": SESSION_ID,
                    "kind": { "external_harness_operation": {
                        "operation": { "record": { "kind": kind } }
                    } }
                }))
                .unwrap();
                assert_eq!(
                    envelope.validate_observation_compatibility().is_ok(),
                    schema != 56 && kind == "configured"
                );
            }
        }
    }

    #[test]
    fn recovery_writer_gate_uses_actual_typed_records_without_rewriting_evidence() {
        // Synthetic type/compatibility inputs, never historical release evidence.
        let payload: serde_json::Value = serde_json::from_str(
            include_str!("../fixtures/stores/epoch-19-schema-54-harness.jsonl")
                .lines()
                .nth(1)
                .unwrap(),
        )
        .unwrap();
        let operation = &payload["kind"]["external_harness_operation"]["operation"];
        let owner = operation["owner"].clone();
        let instance = operation["record"]["instance"].clone();
        let capabilities = operation["record"]["capabilities"].clone();
        let binding = serde_json::json!({
            "schema_version": 1,
            "session_id": owner["session_id"],
            "instance_id": "fixture",
            "native_session_id": "native",
            "native_version": "exact",
            "generation": 1
        });
        let release = serde_json::json!({
            "close_request_id": "close",
            "instance": instance,
            "binding": binding,
            "lifetime": {
                "daemon_instance_id": owner["daemon_id"],
                "artifact_id": "exact-artifact",
                "state_location_id": "isolated-location"
            },
            "reap_id": "synthetic-reap"
        });
        let mut next_binding = binding;
        next_binding["generation"] = 2.into();
        let recovery = serde_json::json!({
            "previous_owner": owner,
            "release_event_sequence": 7,
            "release": release,
            "capabilities": capabilities,
            "resume_request": {
                "schema_version": 1,
                "request_id": "recover",
                "instance": instance,
                "kind": { "operation": "open", "request": {
                    "open_id": "recover",
                    "session_id": owner["session_id"],
                    "generation": 2,
                    "working_directory": "/tmp",
                    "resume": next_binding,
                    "parent_assistance": null
                } }
            }
        });
        let release: bcode_session_models::external_harness::ExternalHarnessTransportRelease =
            serde_json::from_value(release).unwrap();
        let mut event: SessionEvent = serde_json::from_value(payload.clone()).unwrap();
        let SessionEventKind::ExternalHarnessOperation { operation } = &mut event.kind else {
            panic!("operation fixture")
        };
        operation.record =
            bcode_session_models::ExternalHarnessOperationRecord::TransportReleased { release };
        for writer in [1, 20, 21, 22, 23] {
            assert!(validate_recovery_writer(&event, writer).is_err());
        }
        let before = event.clone();
        assert!(validate_recovery_writer(&event, 24).is_err());
        assert!(validate_recovery_writer(&event, 25).is_err());
        assert_eq!(event, before);
        let recovery: bcode_session_models::external_harness::ExternalHarnessRecovery =
            serde_json::from_value(recovery).unwrap();
        let SessionEventKind::ExternalHarnessOperation { operation } = &mut event.kind else {
            panic!("operation fixture")
        };
        operation.owner.generation = 2;
        operation.owner.daemon_id = "new-daemon".into();
        recovery.validate_for(&operation.owner).unwrap();
        operation.record = bcode_session_models::ExternalHarnessOperationRecord::RecoveryPrepared {
            recovery: Box::new(recovery),
        };
        for writer in [1, 20, 21, 22, 23] {
            assert!(validate_recovery_writer(&event, writer).is_err());
        }
        let before = event.clone();
        assert!(validate_recovery_writer(&event, 24).is_err());
        assert!(validate_recovery_writer(&event, 25).is_err());
        assert_eq!(event, before);
        let configured = serde_json::from_value::<SessionEvent>(payload).unwrap();
        for writer in [21, 22, 23, 24] {
            validate_recovery_writer(&configured, writer).unwrap();
        }
    }

    #[test]
    fn execution_epoch_preserves_the_released_upgrade_chain() {
        let latest = crate::plan_writer_epoch_migration(20).unwrap();
        assert_eq!(latest.target_writer_epoch, crate::CURRENT_WRITER_EPOCH);
        assert_eq!(
            latest.steps,
            [
                crate::MigrationStepDescriptor {
                    id: "session-writer-epoch-20-to-21",
                    source_writer_epoch: 20,
                    target_writer_epoch: 21,
                },
                crate::MigrationStepDescriptor {
                    id: "session-writer-epoch-21-to-22",
                    source_writer_epoch: 21,
                    target_writer_epoch: 22,
                },
                crate::MigrationStepDescriptor {
                    id: "session-writer-epoch-22-to-23",
                    source_writer_epoch: 22,
                    target_writer_epoch: 23,
                },
                crate::MigrationStepDescriptor {
                    id: "session-writer-epoch-23-to-25",
                    source_writer_epoch: 23,
                    target_writer_epoch: 25,
                }
            ]
        );
        let plan = crate::plan_writer_epoch_migration(19).unwrap();
        assert_eq!(plan.target_writer_epoch, crate::CURRENT_WRITER_EPOCH);
        assert_eq!(plan.steps.len(), 5);
        assert_eq!(
            plan.steps[0],
            crate::MigrationStepDescriptor {
                id: "session-writer-epoch-19-to-20",
                source_writer_epoch: 19,
                target_writer_epoch: 20,
            }
        );
        assert_eq!(plan.steps[1..], latest.steps);
        let old = crate::plan_writer_epoch_migration(18).unwrap();
        assert_eq!(old.steps.len(), 6);
        assert_eq!(old.steps[0].id, "session-writer-epoch-18-to-19");
        assert_eq!(old.steps[1..], plan.steps);
        assert_eq!(crate::CURRENT_EVENT_SCHEMA, 57);
        assert!(matches!(
            crate::plan_writer_epoch_migration_with_registry(21, 20, &crate::MIGRATION_STEPS),
            Err(crate::MigrationPlanError::FutureWriter { .. })
        ));
    }

    #[test]
    fn strict_current_payload_bypasses_historical_inventory() {
        let payload = format!(
            r#"{{"schema_version":42,"sequence":1,"timestamp_ms":2,"session_id":"{SESSION_ID}","kind":{{"assistant_response_segment":{{"turn_id":"turn-1","segment_id":"segment-0","segment_order":0,"text":"current"}}}}}}"#
        );
        assert!(matches!(
            decode_for_migration(&payload, |payload| {
                serde_json::from_str(payload).map_err(|error| error.to_string())
            }),
            Ok(HistoricalDecode::Current(SessionEvent {
                schema_version: 42,
                sequence: 1,
                kind: SessionEventKind::AssistantResponseSegment { ref text, .. },
                ..
            })) if text == "current"
        ));
    }

    #[test]
    fn every_current_equivalent_pair_requires_strict_current_compatibility() {
        for descriptor in crate::RELEASED_EVENT_VARIANTS.iter().filter(|descriptor| {
            descriptor.treatment == crate::ReleasedEventTreatment::CurrentEquivalent
        }) {
            for schema_version in crate::RELEASED_HISTORICAL_EVENT_SCHEMAS
                .iter()
                .copied()
                .filter(|schema_version| descriptor.supports_schema(*schema_version))
            {
                let payload = format!(
                    r#"{{"schema_version":{schema_version},"sequence":1,"session_id":"{SESSION_ID}","kind":{{"{}":{{}}}}}}"#,
                    descriptor.kind
                );
                let decoded = decode_for_migration(&payload, reject_current);
                assert!(
                    matches!(
                        &decoded,
                        Err(HistoricalSessionEventError::InvalidEvent {
                            schema_version: actual,
                            event_kind,
                            ..
                        }) if *actual == schema_version && event_kind == descriptor.kind
                    ),
                    "current-equivalent {schema_version}:{} must retain current decoder rejection: {decoded:?}",
                    descriptor.kind
                );
            }
        }
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
                        SessionEventKind::InertHistory { event_type, payload }
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
