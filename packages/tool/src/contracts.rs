//! Transport-neutral contracts used to prepare, execute, authorize, and present tool invocations.
//!
//! These types deliberately describe mechanism rather than tool domains. Authorization fact kinds,
//! preparation descriptors, and contribution payloads are opaque to the runtime and are
//! interpreted only by the tool owner and host adapters that understand them.

use serde::{Deserialize, Serialize};
use std::num::{NonZeroU64, NonZeroUsize};

/// Runtime options controlling neutral tool scheduling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolExecutionOptions {
    /// Whether approved invocations from one provider batch may overlap.
    pub parallel: bool,
    /// Optional maximum number of invocations that may execute concurrently.
    ///
    /// `None` allows every approved call in the provider batch to overlap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrency: Option<NonZeroUsize>,
    /// Maximum duration of one side-effect-free preparation operation, in milliseconds.
    pub preparation_timeout_ms: NonZeroU64,
}

impl Default for ToolExecutionOptions {
    fn default() -> Self {
        Self {
            parallel: true,
            max_concurrency: None,
            preparation_timeout_ms: NonZeroU64::new(30_000).expect("thirty thousand is non-zero"),
        }
    }
}

/// Tool-owner-produced fact routed to an authorization coordinator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolAuthorizationFact {
    /// Tool-owner-defined fact namespace.
    pub namespace: String,
    /// Version of the fact schema named by `namespace`.
    pub schema_version: u32,
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

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_tool_models::{
        ToolContributionEvent, ToolContributionOperation, ToolContributionPersistence,
    };

    #[test]
    fn execution_options_omit_unlimited_concurrency_and_preserve_explicit_limit() {
        let unlimited = serde_json::to_value(ToolExecutionOptions::default())
            .expect("unlimited execution options should encode");
        assert!(
            !unlimited
                .as_object()
                .expect("options object")
                .contains_key("max_concurrency")
        );

        let limited = serde_json::to_value(ToolExecutionOptions {
            max_concurrency: NonZeroUsize::new(8),
            ..ToolExecutionOptions::default()
        })
        .expect("limited execution options should encode");
        assert_eq!(limited["max_concurrency"], 8);
        let decoded: ToolExecutionOptions = serde_json::from_value(unlimited)
            .expect("omitted concurrency should decode as unlimited");
        assert_eq!(decoded.max_concurrency, None);
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
            artifact: None,
            payload: serde_json::json!({"opaque": [1, 2, 3]}),
        };

        let encoded = serde_json::to_value(&contribution).expect("contribution should serialize");
        let decoded: ToolContributionEvent =
            serde_json::from_value(encoded).expect("contribution should deserialize");

        assert_eq!(decoded, contribution);
    }
}
