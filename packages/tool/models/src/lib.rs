#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Serializable leaf-model types for neutral tool runtime contracts.

use serde::{Deserialize, Serialize};

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
            }),
            payload: serde_json::json!({"opaque": [1, 2]}),
        };
        let encoded = serde_json::to_vec(&contribution).expect("encode contribution");
        let decoded: ToolContributionEvent =
            serde_json::from_slice(&encoded).expect("decode contribution");

        assert_eq!(decoded, contribution);
    }
}
