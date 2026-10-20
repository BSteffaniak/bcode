#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Serializable leaf-model types for neutral tool runtime contracts.

pub mod interaction;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Generic lifecycle stage for a tool invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolInvocationLifecycleStage {
    /// The prepared invocation has started.
    Started,
    /// The invocation is still running and published progress metadata.
    Progress,
    /// The invocation is waiting for an external response or resource.
    Waiting,
    /// The invocation completed successfully.
    Completed,
    /// The invocation stopped because its turn was cancelled.
    Cancelled,
    /// The invocation completed with an error.
    Failed,
}

/// Mutation applied to a generic renderer contribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolContributionOperation {
    /// Replace or create the identified contribution.
    Upsert,
    /// Append opaque payload data according to the owner schema.
    Append,
    /// Remove the identified contribution.
    Remove,
}

/// Retention policy for an invocation-owned presentation update.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPresentationRetention {
    /// Keep one bounded current value and checkpoint the latest accepted value at closure.
    #[default]
    RetainLatest,
    /// Keep the value only while the invocation update scope remains open.
    ActiveOnly,
}

/// Semantic identity of an invocation-owned presentation item.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolPresentationIdentity {
    /// Primary presentation for the invocation.
    Primary,
    /// Independently meaningful supporting presentation.
    Supplemental { item_id: String },
}

/// Renderer-neutral current presentation update for one tool invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolPresentationUpdate {
    /// Invocation that owns this update.
    pub invocation_id: String,
    /// Plugin, direct tool, or adapter that owns the payload schema.
    pub producer_id: String,
    /// Invocation-local update generation.
    pub generation: u64,
    /// Monotonic revision within the generation and presentation identity.
    pub revision: u64,
    /// Primary or independently keyed supplemental identity.
    pub identity: ToolPresentationIdentity,
    /// Retention policy independent from whether the invocation is currently active.
    pub retention: ToolPresentationRetention,
    /// Producer-owned payload schema.
    pub schema: String,
    /// Version of `schema` used by `payload`.
    pub schema_version: u32,
    /// Optional bounded artifact revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<ToolContributionArtifact>,
    /// Opaque producer-owned visual payload.
    pub payload: serde_json::Value,
}

/// State of an invocation presentation update scope.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPresentationScopeState {
    /// Updates may be accepted subject to identity, generation, revision, and host limits.
    #[default]
    Open,
    /// Terminal closure is absorbing and rejects every later update.
    Closed,
}

/// Error accepting an invocation presentation update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolPresentationUpdateError {
    MissingInvocationId,
    MissingProducerId,
    MissingSupplementalId,
    MissingSchema,
    InvocationMismatch,
    ProducerMismatch,
    Unavailable,
    Closed,
    StaleGeneration,
    FutureGeneration,
    StaleRevision,
    TooLarge { actual: usize, maximum: usize },
    Encode(String),
}

/// Authoritative acceptance state for one invocation-owned presentation identity.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolPresentationUpdateScope {
    state: ToolPresentationScopeState,
    generation: u64,
    highest_revisions: BTreeMap<ToolPresentationIdentity, u64>,
}

#[cfg(test)]
mod retained_revision_iterator_tests {
    use super::*;

    #[test]
    fn bounded_accept_counts_escaped_json_and_rolls_back_generation_on_denial() {
        let update = ToolPresentationUpdate {
            invocation_id: "call".into(),
            producer_id: "producer".into(),
            generation: 1,
            revision: 1,
            identity: ToolPresentationIdentity::Primary,
            retention: ToolPresentationRetention::RetainLatest,
            schema: "schema".into(),
            schema_version: 1,
            artifact: None,
            payload: serde_json::json!({"text": "\"\\\n界"}),
        };
        let encoded_len = serde_json::to_vec(&update).unwrap().len();
        let mut scope = ToolPresentationUpdateScope::default();
        let before = scope.clone();
        assert!(matches!(
            scope.accept_bounded(&update, encoded_len - 1),
            Err(ToolPresentationUpdateError::TooLarge { .. })
        ));
        assert_eq!(scope, before);
        scope.accept_bounded(&update, encoded_len).unwrap();
        let accepted = scope.clone();
        assert_eq!(
            scope.accept_bounded(&update, encoded_len),
            Err(ToolPresentationUpdateError::StaleRevision)
        );
        assert_eq!(scope, accepted);
        let mut normal = before;
        normal.accept(&update, encoded_len).unwrap();
        assert_eq!(normal, scope);
        scope.close();
        assert_eq!(
            scope.accept_bounded(&update, encoded_len),
            Err(ToolPresentationUpdateError::Closed)
        );
    }

    #[test]
    fn bounded_writer_rejects_without_retaining_encoded_bytes() {
        use std::io::Write;
        let mut writer = BoundedJsonSizeWriter {
            written: 0,
            limit: 4,
            exceeded: false,
        };
        writer.write_all(b"1234").unwrap();
        assert!(writer.write_all(b"5").is_err());
        assert!(writer.exceeded);
        assert_eq!(writer.written, 5);
    }

    #[test]
    fn iteration_borrows_fences_without_changing_scope() {
        let mut scope = ToolPresentationUpdateScope::default();
        scope
            .highest_revisions
            .insert(ToolPresentationIdentity::Primary, 3);
        scope.highest_revisions.insert(
            ToolPresentationIdentity::Supplemental {
                item_id: "retired".into(),
            },
            8,
        );
        let before = scope.clone();
        let retained: Vec<_> = scope.iter_retained_revisions().collect();
        assert_eq!(retained.len(), 2);
        assert!(retained.iter().any(|(identity, revision)| **identity
            == ToolPresentationIdentity::Primary
            && *revision == 3));
        assert_eq!(scope, before);
        assert_eq!(
            scope.highest_revision(&ToolPresentationIdentity::Supplemental {
                item_id: "retired".into()
            }),
            Some(8)
        );
    }
}

impl ToolPresentationUpdateScope {
    #[must_use]
    pub const fn state(&self) -> ToolPresentationScopeState {
        self.state
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub fn highest_revision(&self, identity: &ToolPresentationIdentity) -> Option<u64> {
        self.highest_revisions.get(identity).copied()
    }

    /// Borrow retained revision fences without cloning identities or exposing storage.
    ///
    /// Includes fences for presentations no longer retained by a consumer. Iteration
    /// does not change scope state, generation, or duplicate acceptance semantics.
    #[must_use]
    pub fn iter_retained_revisions(
        &self,
    ) -> impl ExactSizeIterator<Item = (&ToolPresentationIdentity, u64)> {
        self.highest_revisions
            .iter()
            .map(|(identity, revision)| (identity, *revision))
    }

    /// Accept a bounded update and advance authoritative revision state.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid identity/schema, closed scope, stale or skipped generation,
    /// stale revision, serialization failure, or encoded payload larger than `max_encoded_bytes`.
    pub fn accept(
        &mut self,
        update: &ToolPresentationUpdate,
        max_encoded_bytes: usize,
    ) -> Result<(), ToolPresentationUpdateError> {
        self.accept_with_writer_limit(update, max_encoded_bytes, usize::MAX, false)
    }

    /// Accept an update without allocating its encoded representation.
    ///
    /// Serialization stops as soon as the encoded size exceeds `max_encoded_bytes`.
    /// `TooLarge::actual` is therefore a lower bound, not necessarily the complete size.
    /// Callers retaining the update must separately admit the identity fence and payload clone.
    ///
    /// # Errors
    /// Returns the same validation errors as [`Self::accept`]. Size and encoding failures
    /// preserve the revision fences, including the current generation.
    pub fn accept_bounded(
        &mut self,
        update: &ToolPresentationUpdate,
        max_encoded_bytes: usize,
    ) -> Result<(), ToolPresentationUpdateError> {
        self.accept_with_writer_limit(update, max_encoded_bytes, max_encoded_bytes, true)
    }

    fn accept_with_writer_limit(
        &mut self,
        update: &ToolPresentationUpdate,
        max_encoded_bytes: usize,
        writer_limit: usize,
        atomic: bool,
    ) -> Result<(), ToolPresentationUpdateError> {
        if self.state == ToolPresentationScopeState::Closed {
            return Err(ToolPresentationUpdateError::Closed);
        }
        if update.invocation_id.trim().is_empty() {
            return Err(ToolPresentationUpdateError::MissingInvocationId);
        }
        if update.producer_id.trim().is_empty() {
            return Err(ToolPresentationUpdateError::MissingProducerId);
        }
        if update.schema.trim().is_empty() {
            return Err(ToolPresentationUpdateError::MissingSchema);
        }
        if matches!(
            &update.identity,
            ToolPresentationIdentity::Supplemental { item_id } if item_id.trim().is_empty()
        ) {
            return Err(ToolPresentationUpdateError::MissingSupplementalId);
        }
        if update.generation < self.generation {
            return Err(ToolPresentationUpdateError::StaleGeneration);
        }
        if update.generation > self.generation.saturating_add(1) {
            return Err(ToolPresentationUpdateError::FutureGeneration);
        }
        // The unrestricted API historically advances generation before payload validation.
        // Bounded admission is atomic and advances only after successful serialization.
        if !atomic && update.generation > self.generation {
            self.generation = update.generation;
            self.highest_revisions.clear();
        }
        if update.generation == self.generation
            && self
                .highest_revisions
                .get(&update.identity)
                .is_some_and(|revision| update.revision <= *revision)
        {
            return Err(ToolPresentationUpdateError::StaleRevision);
        }
        let mut encoded = BoundedJsonSizeWriter {
            written: 0,
            limit: writer_limit,
            exceeded: false,
        };
        if let Err(error) = serde_json::to_writer(&mut encoded, update) {
            if encoded.exceeded {
                return Err(ToolPresentationUpdateError::TooLarge {
                    actual: encoded.written,
                    maximum: max_encoded_bytes,
                });
            }
            return Err(ToolPresentationUpdateError::Encode(error.to_string()));
        }
        if encoded.written > max_encoded_bytes {
            return Err(ToolPresentationUpdateError::TooLarge {
                actual: encoded.written,
                maximum: max_encoded_bytes,
            });
        }
        if update.generation > self.generation {
            self.generation = update.generation;
            self.highest_revisions.clear();
        }
        self.highest_revisions
            .insert(update.identity.clone(), update.revision);
        Ok(())
    }

    /// Close the scope. Repeated closure is idempotent.
    pub const fn close(&mut self) {
        self.state = ToolPresentationScopeState::Closed;
    }
}

/// Allocation-free JSON byte-counting sink with an explicit write limit.
struct BoundedJsonSizeWriter {
    written: usize,
    limit: usize,
    exceeded: bool,
}

impl std::io::Write for BoundedJsonSizeWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.written = self.written.saturating_add(bytes.len());
        if self.written > self.limit {
            self.exceeded = true;
            return Err(std::io::Error::other("encoded size limit exceeded"));
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Renderer-neutral placement for one tool contribution.
///
/// Placement controls semantic transcript composition without encoding renderer-specific layout or
/// styling. Renderers must not infer placement from tool names, schemas, or contribution IDs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolContributionPlacement {
    /// Primary request presentation for the invocation.
    Request,
    /// Current progress presentation for the invocation.
    Progress,
    /// Primary final result presentation for the invocation.
    Result,
    /// Independently visible supporting presentation.
    Supplemental,
    /// Semantic contribution retained without normal transcript presentation.
    #[default]
    Hidden,
}

/// Versioned contribution envelope carrying host composition semantics separately from the opaque
/// producer payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolContributionEnvelope {
    /// Envelope schema version.
    pub schema_version: u16,
    /// Renderer-neutral placement of the contribution.
    pub placement: ToolContributionPlacement,
    /// Opaque producer-owned contribution.
    pub contribution: ToolContributionEvent,
}

impl ToolContributionEnvelope {
    /// Current envelope schema version.
    pub const SCHEMA_VERSION: u16 = 1;

    /// Wrap a contribution with explicit renderer-neutral placement.
    #[must_use]
    pub const fn new(
        placement: ToolContributionPlacement,
        contribution: ToolContributionEvent,
    ) -> Self {
        Self {
            schema_version: Self::SCHEMA_VERSION,
            placement,
            contribution,
        }
    }
}

/// Persistence requested for a renderer contribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolContributionPersistence {
    /// Publish only to currently attached observers.
    Transient,
    /// Preserve the opaque envelope for replay.
    Durable,
}

/// Generic revision metadata for one artifact attached to a renderer contribution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolContributionArtifact {
    /// Producer-assigned artifact identifier unique within the invocation.
    pub artifact_id: String,
    /// Stable reference key within the artifact.
    pub reference_key: String,
    /// Optional media type for the referenced bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// Opaque host-readable storage reference.
    pub storage_uri: String,
    /// Byte prefix committed and safe for bounded range reads.
    pub committed_bytes: u64,
    /// Monotonic artifact revision.
    pub revision: u64,
    /// Whether no later bytes will be appended.
    #[serde(default)]
    pub finalized: bool,
    /// Optional generic availability state such as `active`, `complete`, or `incomplete`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub availability: Option<String>,
}

/// Schema-versioned renderer contribution emitted by a tool owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolContributionEvent {
    /// Invocation that owns this contribution.
    pub invocation_id: String,
    /// Stable identity of the contribution within the invocation.
    pub contribution_id: String,
    /// Monotonic sequence assigned by the producer for this contribution.
    pub sequence: u64,
    /// Plugin, direct tool, or adapter identity that owns the payload schema.
    pub producer_id: String,
    /// Tool-owner-defined payload schema.
    pub schema: String,
    /// Version of `schema` used by `payload`.
    pub schema_version: u32,
    /// Generic contribution mutation.
    pub operation: ToolContributionOperation,
    /// Whether the opaque envelope is transient or durable.
    pub persistence: ToolContributionPersistence,
    /// Optional generic artifact revision consumed through the host artifact capability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<ToolContributionArtifact>,
    /// Opaque renderer payload interpreted outside core orchestration.
    pub payload: serde_json::Value,
}

/// Required versus optional exchange response policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExchangeResponsePolicy {
    /// The invocation cannot complete without a compatible response.
    Required,
    /// The host may decline the exchange and allow the invocation to continue.
    Optional,
}

/// Correlated renderer-neutral request for external input while an invocation remains active.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolExchangeRequest {
    /// Invocation that owns the exchange.
    pub invocation_id: String,
    /// Producer-assigned exchange identifier unique within the invocation.
    pub exchange_id: String,
    /// Plugin, direct tool, or adapter that owns the payload schema.
    pub producer_id: String,
    /// Producer-owned request/response schema.
    pub schema: String,
    /// Version of `schema` used by `payload`.
    pub schema_version: u32,
    /// Opaque request payload.
    pub payload: serde_json::Value,
    /// Required versus optional response behavior.
    pub response_policy: ToolExchangeResponsePolicy,
}

/// Terminal resolution of one invocation exchange.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolExchangeResolution {
    /// A compatible consumer supplied an opaque response payload.
    Responded { payload: serde_json::Value },
    /// The owning invocation or turn was cancelled.
    Cancelled,
    /// The exchange deadline elapsed.
    TimedOut,
    /// No attached consumer supports the exchange schema/version.
    NoCompatibleConsumer,
    /// The selected consumer detached before responding.
    ConsumerDetached,
    /// The host failed to route or resolve the exchange.
    Failed { code: String, message: String },
}

/// Correlated terminal record for one invocation exchange.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolExchangeResolutionEvent {
    /// Invocation that owns the exchange.
    pub invocation_id: String,
    /// Exchange resolved by this event.
    pub exchange_id: String,
    /// Terminal resolution, including any opaque response payload.
    pub resolution: ToolExchangeResolution,
}

/// Unsolicited schema-versioned input delivered to an active invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInvocationInput {
    /// Invocation that owns the input.
    pub invocation_id: String,
    /// Producer-assigned input identifier.
    pub input_id: String,
    /// Plugin, host adapter, or client that owns the payload schema.
    pub producer_id: String,
    /// Producer-owned input schema.
    pub schema: String,
    /// Version of `schema` used by `payload`.
    pub schema_version: u32,
    /// Opaque input payload.
    pub payload: serde_json::Value,
}

/// Result of waiting for the next input addressed to an invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolInvocationInputResolution {
    /// One input is available.
    Received { input: ToolInvocationInput },
    /// The owning invocation or turn was cancelled.
    Cancelled,
    /// The bounded input wait elapsed without an input.
    TimedOut,
    /// The host closed input delivery for this invocation.
    Closed,
    /// Input routing failed.
    Failed { code: String, message: String },
}

/// Renderer-independent invocation lifecycle event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInvocationLifecycleEvent {
    /// Invocation correlated with this lifecycle event.
    pub invocation_id: String,
    /// Producer-local monotonic sequence.
    pub sequence: u64,
    /// Current lifecycle stage.
    pub stage: ToolInvocationLifecycleStage,
    /// Optional human-readable progress summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Optional structured producer metadata.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presentation_scope_close_retains_highest_accepted_revision() {
        let mut scope = ToolPresentationUpdateScope::default();
        let update = ToolPresentationUpdate {
            invocation_id: "call-1".to_owned(),
            producer_id: "example.plugin".to_owned(),
            generation: 0,
            revision: 7,
            identity: ToolPresentationIdentity::Primary,
            retention: ToolPresentationRetention::RetainLatest,
            schema: "example.presentation".to_owned(),
            schema_version: 1,
            artifact: None,
            payload: serde_json::json!({"value": 1}),
        };
        scope.accept(&update, 1024).expect("accepted update");
        scope.close();

        assert_eq!(scope.state(), ToolPresentationScopeState::Closed);
        assert_eq!(
            scope.highest_revision(&ToolPresentationIdentity::Primary),
            Some(7)
        );
        assert_eq!(
            scope.accept(
                &ToolPresentationUpdate {
                    revision: 8,
                    ..update
                },
                1024
            ),
            Err(ToolPresentationUpdateError::Closed)
        );
    }

    #[test]
    fn contribution_envelope_round_trips_all_placements() {
        for placement in [
            ToolContributionPlacement::Request,
            ToolContributionPlacement::Progress,
            ToolContributionPlacement::Result,
            ToolContributionPlacement::Supplemental,
            ToolContributionPlacement::Hidden,
        ] {
            let contribution = ToolContributionEvent {
                invocation_id: "call-1".to_owned(),
                contribution_id: "surface".to_owned(),
                sequence: 1,
                producer_id: "example.plugin".to_owned(),
                schema: "example.surface".to_owned(),
                schema_version: 1,
                operation: ToolContributionOperation::Upsert,
                persistence: ToolContributionPersistence::Durable,
                artifact: None,
                payload: serde_json::json!({"opaque": true}),
            };
            let envelope = ToolContributionEnvelope::new(placement, contribution);
            let encoded = serde_json::to_vec(&envelope).expect("envelope encodes");
            let decoded: ToolContributionEnvelope =
                serde_json::from_slice(&encoded).expect("envelope decodes");
            assert_eq!(decoded, envelope);
        }
    }

    #[test]
    fn unknown_future_placement_is_rejected_without_payload_fallback() {
        let contribution = serde_json::json!({
            "schema_version": ToolContributionEnvelope::SCHEMA_VERSION,
            "placement": "future_surface",
            "contribution": {
                "invocation_id": "call-1",
                "contribution_id": "surface",
                "sequence": 1,
                "producer_id": "example.plugin",
                "schema": "example.surface",
                "schema_version": 1,
                "operation": "upsert",
                "persistence": "durable",
                "payload": {"opaque": true}
            }
        });
        let error = serde_json::from_value::<ToolContributionEnvelope>(contribution)
            .expect_err("unknown placement must not silently become visible");
        assert!(error.to_string().contains("unknown variant"));
    }

    #[test]
    fn unsupported_envelope_version_remains_explicit() {
        let contribution = ToolContributionEvent {
            invocation_id: "call-1".to_owned(),
            contribution_id: "surface".to_owned(),
            sequence: 1,
            producer_id: "example.plugin".to_owned(),
            schema: "example.surface".to_owned(),
            schema_version: 1,
            operation: ToolContributionOperation::Upsert,
            persistence: ToolContributionPersistence::Durable,
            artifact: None,
            payload: serde_json::json!({"opaque": true}),
        };
        let mut envelope =
            ToolContributionEnvelope::new(ToolContributionPlacement::Hidden, contribution);
        envelope.schema_version = ToolContributionEnvelope::SCHEMA_VERSION.saturating_add(1);
        assert_ne!(
            envelope.schema_version,
            ToolContributionEnvelope::SCHEMA_VERSION
        );
    }

    #[test]
    fn contribution_without_artifact_remains_decode_compatible() {
        let contribution: ToolContributionEvent = serde_json::from_value(serde_json::json!({
            "invocation_id": "call-1",
            "contribution_id": "surface",
            "sequence": 1,
            "producer_id": "example.plugin",
            "schema": "example.surface",
            "schema_version": 1,
            "operation": "upsert",
            "persistence": "transient",
            "payload": {"opaque": true}
        }))
        .expect("legacy contribution envelope");

        assert!(contribution.artifact.is_none());
    }

    #[test]
    fn contribution_artifact_revision_round_trips() {
        let contribution = ToolContributionEvent {
            invocation_id: "call-1".to_owned(),
            contribution_id: "recording".to_owned(),
            sequence: 2,
            producer_id: "example.plugin".to_owned(),
            schema: "example.recording".to_owned(),
            schema_version: 1,
            operation: ToolContributionOperation::Upsert,
            persistence: ToolContributionPersistence::Transient,
            artifact: Some(ToolContributionArtifact {
                artifact_id: "recording-1".to_owned(),
                reference_key: "bytes".to_owned(),
                content_type: Some("application/octet-stream".to_owned()),
                storage_uri: "file:///tmp/recording".to_owned(),
                committed_bytes: 42,
                revision: 2,
                finalized: false,
                availability: None,
            }),
            payload: serde_json::json!({"opaque": [1, 2]}),
        };
        let encoded = serde_json::to_vec(&contribution).expect("encode contribution");
        let decoded: ToolContributionEvent =
            serde_json::from_slice(&encoded).expect("decode contribution");

        assert_eq!(decoded, contribution);
        assert_eq!(
            decoded
                .artifact
                .as_ref()
                .and_then(|artifact| artifact.availability.as_deref()),
            None
        );
    }

    #[test]
    fn contribution_artifact_availability_round_trips() {
        let mut contribution = ToolContributionEvent {
            invocation_id: "call-1".to_owned(),
            contribution_id: "recording".to_owned(),
            sequence: 3,
            producer_id: "example.plugin".to_owned(),
            schema: "example.recording".to_owned(),
            schema_version: 1,
            operation: ToolContributionOperation::Upsert,
            persistence: ToolContributionPersistence::Transient,
            artifact: Some(ToolContributionArtifact {
                artifact_id: "recording-1".to_owned(),
                reference_key: "bytes".to_owned(),
                content_type: None,
                storage_uri: "file:///tmp/recording".to_owned(),
                committed_bytes: 42,
                revision: 3,
                finalized: false,
                availability: Some("incomplete".to_owned()),
            }),
            payload: serde_json::Value::Null,
        };
        let encoded = serde_json::to_vec(&contribution).expect("encode contribution");
        contribution = serde_json::from_slice(&encoded).expect("decode contribution");

        assert_eq!(
            contribution
                .artifact
                .as_ref()
                .and_then(|artifact| artifact.availability.as_deref()),
            Some("incomplete")
        );
    }
}
