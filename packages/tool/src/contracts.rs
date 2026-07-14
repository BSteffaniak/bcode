//! Transport-neutral contracts used to prepare, schedule, authorize, and present tool invocations.
//!
//! These types deliberately describe mechanism rather than tool domains. Resource namespaces,
//! authorization fact kinds, preparation descriptors, and contribution payloads are opaque to the
//! runtime and are interpreted only by the tool owner and host adapters that understand them.

use serde::{Deserialize, Serialize};
use std::num::NonZeroUsize;

use crate::ToolInvocationRequest;

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

/// Request to prepare one invocation without performing its side effects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolPreparationRequest {
    /// Complete invocation request that will be executed if preparation and authorization succeed.
    pub invocation: ToolInvocationRequest,
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
    /// Original invocation request.
    pub request: ToolInvocationRequest,
    /// Tool-owner-produced preparation data.
    pub preparation: ToolPreparationResponse,
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
    /// The invocation has stopped running.
    Finished,
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
