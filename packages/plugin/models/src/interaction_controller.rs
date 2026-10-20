//! Versioned renderer-neutral dynamic interaction-controller contracts.
//!
//! The plugin owns transitions. The host retains opaque state only for the lifetime
//! of a pending exchange; neither state nor snapshots imply durable resumability.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use bcode_tool_models::interaction::{
    InteractionControlId, InteractionInput, InteractionNavigation, InteractionOutput,
    InteractionValue,
};

/// Secret-safe controller application failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionControllerError {
    /// No compatible exact producer/schema controller.
    Unsupported,
    /// Unsupported version, identity, route or transition.
    InvalidContract,
    /// Owner/revision or conflicting duplicate mismatch.
    Conflict,
    /// No longer pending.
    NotPending,
    /// Encoded size or ephemeral lifecycle limit exceeded.
    LimitExceeded,
    /// Plugin or application unavailable, cancelled or timed out.
    Unavailable,
    /// Caller lacks consumer/session authority.
    Unauthorized,
}
impl InteractionControllerError {
    /// Stable secret-safe diagnostic.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::Unsupported => "no compatible interaction controller",
            Self::InvalidContract => "invalid or unsupported interaction controller contract",
            Self::Conflict => "interaction controller owner or revision conflict",
            Self::NotPending => "interaction is no longer pending",
            Self::LimitExceeded => "interaction controller resource limit exceeded",
            Self::Unavailable => "interaction controller unavailable",
            Self::Unauthorized => "interaction consumer is not authorized",
        }
    }
}

/// Independently versioned plugin service interface.
pub const INTERACTION_CONTROLLER_INTERFACE: &str = "bcode.interaction-controller/v1";
/// Current controller contract version.
pub const INTERACTION_CONTROLLER_VERSION: u32 = 1;
/// Maximum encoded request, state, snapshot or transition size.
pub const MAX_CONTROLLER_BYTES: usize = 256 * 1024;
/// Maximum routes in one producer's discovery response.
pub const MAX_CONTROLLER_ROUTES: usize = 64;

/// Producer-owned controller route; no renderer or transport identity is involved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionControllerRoute {
    /// Exact producer owning the pending exchange.
    pub producer_id: String,
    /// Exact opaque exchange request schema.
    pub exchange_schema: String,
    /// First supported request schema version.
    pub min_schema_version: u32,
    /// Last supported request schema version, inclusive.
    pub max_schema_version: u32,
    /// Plugin-owned semantic kind.
    pub interaction_kind: String,
}

impl InteractionControllerRoute {
    /// Return whether this route accepts the exact producer/schema/version.
    #[must_use]
    pub fn supports(&self, producer: &str, schema: &str, version: u32) -> bool {
        self.producer_id == producer
            && self.exchange_schema == schema
            && self.min_schema_version <= version
            && version <= self.max_schema_version
    }
}

/// Discovery request for a versioned controller interface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionControllerDiscoveryRequest {
    /// Required contract version; future versions fail closed.
    pub version: u32,
}

/// Dynamically discovered routes from one loaded producer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionControllerDiscovery {
    /// Contract version.
    pub version: u32,
    /// Bounded producer-owned routes.
    pub routes: Vec<InteractionControllerRoute>,
}

/// Host-to-plugin transition request. State is host-owned ephemeral data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum InteractionControllerRequest {
    /// Initialize from a pending exchange without interpreting its opaque payload.
    Open {
        /// Required contract version.
        version: u32,
        /// Pinned producer route.
        route: InteractionControllerRoute,
        /// Request schema version.
        schema_version: u32,
        /// Original exchange payload.
        payload: Value,
    },
    /// Apply semantic input to the previous ephemeral plugin state.
    Input {
        /// Required contract version.
        version: u32,
        /// Pinned producer route.
        route: InteractionControllerRoute,
        /// Opaque state from the previous transition.
        state: Value,
        /// Renderer-neutral semantic input.
        input: InteractionInput,
    },
}

/// Plugin-owned transition. No side effect is committed merely by returning it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionControllerTransition {
    /// Contract version.
    pub version: u32,
    /// Exact semantic kind from the pinned route.
    pub interaction_kind: String,
    /// Opaque ephemeral state; never exposed in public snapshots or persisted.
    pub state: Value,
    /// Renderer-neutral plugin-owned presentation snapshot.
    pub snapshot: Value,
    /// Semantic effect for the application owner to consume.
    pub output: InteractionOutput,
}

/// Public bounded semantic view, without opaque controller state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionControllerSnapshot {
    /// Contract version.
    pub version: u32,
    /// Canonical pending exchange identity.
    pub exchange_id: String,
    /// Pinned producer and schema identity.
    pub route: InteractionControllerRoute,
    /// Exact request schema version.
    pub schema_version: u32,
    /// Monotonic host revision used for conflict-safe input.
    pub revision: u64,
    /// Plugin-owned semantic snapshot.
    pub snapshot: Value,
    /// Last semantic effect; terminal effects are stable until lifecycle cleanup.
    pub output: InteractionOutput,
}

/// Input admission uses optimistic revision checks; it never silently reapplies input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractionControllerInput {
    /// Required contract version.
    pub version: u32,
    /// Canonical exchange identity.
    pub exchange_id: String,
    /// Expected host revision.
    pub expected_revision: u64,
    /// Semantic input, independent of renderer gestures.
    pub input: InteractionInput,
}
