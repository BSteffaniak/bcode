//! Transport-neutral contracts used to prepare, schedule, authorize, and present tool invocations.
//!
//! These types deliberately describe mechanism rather than tool domains. Resource namespaces,
//! authorization fact kinds, preparation descriptors, and contribution payloads are opaque to the
//! runtime and are interpreted only by the tool owner and host adapters that understand them.

use serde::{Deserialize, Serialize};
use std::num::NonZeroUsize;

/// Runtime options controlling neutral tool scheduling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolExecutionOptions {
    /// Whether compatible prepared invocations may overlap.
    pub parallel: bool,
    /// Maximum number of invocations that may execute concurrently.
    pub max_concurrency: NonZeroUsize,
}

impl Default for ToolExecutionOptions {
    fn default() -> Self {
        Self {
            parallel: true,
            max_concurrency: NonZeroUsize::new(4).expect("four is non-zero"),
        }
    }
}

/// Access requested for one opaque scheduling resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResourceAccess {
    /// The invocation may coexist with other shared claims for the same resource.
    Shared,
    /// The invocation conflicts with every other claim for the same resource.
    Exclusive,
}

/// One tool-owner-produced claim over an opaque scheduling resource.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ToolResourceClaim {
    /// Tool-owner-defined namespace. Core code compares this value but never interprets it.
    pub namespace: String,
    /// Tool-owner-defined resource identity within `namespace`.
    pub resource: String,
    /// Requested access to the resource.
    pub access: ToolResourceAccess,
}

/// Scheduling contract returned while preparing an invocation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ToolSchedulingContract {
    /// Run without overlapping another invocation.
    ///
    /// This is the safe default for tools and adapters that do not provide scheduling metadata.
    #[default]
    Isolated,
    /// Permit overlap when none of the opaque resource claims conflict.
    Concurrent {
        /// Complete resource claims for this prepared invocation.
        #[serde(default)]
        claims: Vec<ToolResourceClaim>,
    },
}

impl ToolSchedulingContract {
    /// Return whether this contract conflicts with another prepared invocation.
    #[must_use]
    pub fn conflicts_with(&self, other: &Self) -> bool {
        let (
            Self::Concurrent {
                claims: left_claims,
            },
            Self::Concurrent {
                claims: right_claims,
            },
        ) = (self, other)
        else {
            return true;
        };

        left_claims.iter().any(|left| {
            right_claims.iter().any(|right| {
                left.namespace == right.namespace
                    && left.resource == right.resource
                    && (left.access == ToolResourceAccess::Exclusive
                        || right.access == ToolResourceAccess::Exclusive)
            })
        })
    }
}

/// Tool-owner-produced fact routed to an authorization coordinator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolAuthorizationFact {
    /// Tool-owner-defined fact namespace.
    pub namespace: String,
    /// Tool-owner-defined action within `namespace`.
    pub action: String,
    /// Optional opaque resource identity relevant to the action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    /// Opaque structured metadata for authorization adapters that understand this fact.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: serde_json::Value,
}

/// Transport-free identity and arguments for one requested tool invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInvocationDescriptor {
    /// Provider-assigned call identifier.
    pub invocation_id: String,
    /// Registered tool name.
    pub tool_name: String,
    /// Opaque JSON arguments supplied by the provider.
    pub arguments: serde_json::Value,
}

/// Opaque host context made available during side-effect-free preparation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolHostContextEntry {
    /// Host-owned context schema.
    pub schema: String,
    /// Version of `schema` used by `payload`.
    pub schema_version: u32,
    /// Opaque context interpreted only by adapters that understand `schema`.
    pub payload: serde_json::Value,
}

/// Request to prepare one invocation without performing its side effects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolPreparationRequest {
    /// Transport-free invocation identity and arguments.
    pub invocation: ToolInvocationDescriptor,
    /// Opaque host context available to the tool owner.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub host_context: Vec<ToolHostContextEntry>,
}

/// Neutral preparation data returned by the tool owner.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolPreparationResponse {
    /// Scheduling behavior and opaque resource claims for this invocation.
    #[serde(default)]
    pub scheduling: ToolSchedulingContract,
    /// Facts that a host authorization coordinator must evaluate before invocation.
    #[serde(default)]
    pub authorization: Vec<ToolAuthorizationFact>,
    /// Opaque tool-owner-defined descriptor passed back during invocation.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub descriptor: serde_json::Value,
}

/// Fully prepared invocation passed from neutral orchestration to an invoker adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedToolInvocation {
    /// Transport-free invocation identity and arguments.
    pub invocation: ToolInvocationDescriptor,
    /// Tool-owner-produced preparation data.
    pub preparation: ToolPreparationResponse,
}

/// Policy controlling how a host handles an exchange with no compatible consumer.
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
    /// The host closed input delivery for this invocation.
    Closed,
    /// Input routing failed.
    Failed { code: String, message: String },
}

/// Opaque nested service request issued by an active invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInvocationServiceRequest {
    /// Invocation that owns the request.
    pub invocation_id: String,
    /// Producer-assigned request identifier.
    pub request_id: String,
    /// Versioned service interface identifier.
    pub interface_id: String,
    /// Operation within `interface_id`.
    pub operation: String,
    /// Opaque service request payload.
    pub payload: serde_json::Value,
}

/// Terminal result of one nested invocation service request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolInvocationServiceResolution {
    /// The routed service returned an opaque response payload.
    Responded { payload: serde_json::Value },
    /// The owning invocation or turn was cancelled.
    Cancelled,
    /// No host service supports the interface and operation.
    Unsupported,
    /// The routed service failed.
    Failed { code: String, message: String },
}

/// Bounded artifact write requested by an active invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolArtifactWriteRequest {
    /// Invocation that owns the artifact.
    pub invocation_id: String,
    /// Producer-assigned artifact identifier unique within the invocation.
    pub artifact_id: String,
    /// Artifact content type.
    pub content_type: String,
    /// Complete artifact bytes. Host sinks enforce their configured bound.
    pub bytes: Vec<u8>,
    /// Opaque producer metadata.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: serde_json::Value,
}

/// Terminal result of a bounded host artifact write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolArtifactWriteResolution {
    /// The host persisted the complete artifact and returned an opaque reference.
    Written {
        artifact_id: String,
        byte_len: u64,
        reference: serde_json::Value,
    },
    /// The owning invocation or turn was cancelled.
    Cancelled,
    /// The artifact exceeded the host sink's configured bound.
    TooLarge { max_bytes: u64 },
    /// The artifact sink failed.
    Failed { code: String, message: String },
}

/// Mutation applied to a generic renderer contribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolContributionOperation {
    /// Replace or create the identified contribution.
    Upsert,
    /// Append payload data to the identified contribution.
    Append,
    /// Remove the identified contribution.
    Remove,
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
    /// Opaque renderer payload interpreted outside core orchestration.
    pub payload: serde_json::Value,
}

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

    fn claim(namespace: &str, resource: &str, access: ToolResourceAccess) -> ToolResourceClaim {
        ToolResourceClaim {
            namespace: namespace.to_string(),
            resource: resource.to_string(),
            access,
        }
    }

    #[test]
    fn unknown_contract_defaults_to_isolated() {
        let response: ToolPreparationResponse = serde_json::from_value(serde_json::json!({}))
            .expect("default preparation should parse");

        assert_eq!(response.scheduling, ToolSchedulingContract::Isolated);
    }

    #[test]
    fn opaque_claim_conflicts_are_domain_independent() {
        let shared = ToolSchedulingContract::Concurrent {
            claims: vec![claim("synthetic", "alpha", ToolResourceAccess::Shared)],
        };
        let other_shared = ToolSchedulingContract::Concurrent {
            claims: vec![claim("synthetic", "alpha", ToolResourceAccess::Shared)],
        };
        let exclusive = ToolSchedulingContract::Concurrent {
            claims: vec![claim("synthetic", "alpha", ToolResourceAccess::Exclusive)],
        };
        let unrelated = ToolSchedulingContract::Concurrent {
            claims: vec![claim(
                "another-namespace",
                "alpha",
                ToolResourceAccess::Exclusive,
            )],
        };

        assert!(!shared.conflicts_with(&other_shared));
        assert!(shared.conflicts_with(&exclusive));
        assert!(!shared.conflicts_with(&unrelated));
        assert!(ToolSchedulingContract::Isolated.conflicts_with(&shared));
    }

    #[test]
    fn contribution_payload_round_trips_without_renderer_knowledge() {
        let contribution = ToolContributionEvent {
            invocation_id: "invoke-1".to_string(),
            contribution_id: "view-1".to_string(),
            sequence: 3,
            producer_id: "synthetic-producer".to_string(),
            schema: "example.unknown/v7".to_string(),
            schema_version: 7,
            operation: ToolContributionOperation::Upsert,
            persistence: ToolContributionPersistence::Durable,
            payload: serde_json::json!({"opaque": [1, 2, 3]}),
        };

        let encoded = serde_json::to_value(&contribution).expect("contribution should serialize");
        let decoded: ToolContributionEvent =
            serde_json::from_value(encoded).expect("contribution should deserialize");

        assert_eq!(decoded, contribution);
    }
}
