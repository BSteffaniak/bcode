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
