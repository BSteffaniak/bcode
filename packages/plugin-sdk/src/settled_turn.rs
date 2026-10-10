//! Versioned request-only notification of a canonically settled turn.
use bcode_session_models::{ModelTurnOutcome, SessionId, TurnPriority};
use serde::{Deserialize, Serialize};

/// Multi-provider observer interface. Missing observers are a no-op.
pub const INTERFACE_ID: &str = "bcode.settled-turn-observer/v1";
/// Observe one persisted settlement; delivery is not a durable replay promise.
pub const OP_OBSERVE: &str = "observe";

/// Canonical facts only: no prompts, tools, reasoning, or provider-private data.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettledTurn {
    /// Canonical owning session.
    pub session_id: SessionId,
    /// Canonical activated turn identifier.
    pub turn_id: String,
    /// Canonical activation event sequence.
    pub trigger_sequence: u64,
    /// Normalized authoritative outcome, persisted before dispatch.
    pub outcome: ModelTurnOutcome,
    /// Admission provenance for plugin-owned applicability policy.
    pub priority: TurnPriority,
    /// Whether a producer supplied an origin.
    pub has_origin: bool,
    /// Whether execution carries workflow or application correlation.
    pub correlated: bool,
    /// Whether execution was resumed by recovery.
    pub continuity: ExecutionContinuity,
    /// Cancellation observed before terminal cleanup; cleanup alone never sets this fact.
    pub cancellation_requested: bool,
}

/// Bounded declarative observer response; the host authorizes any requested action.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ObserverResponse {
    /// No application action requested.
    NoAction,
    /// One typed application service request, executed under the observer cancellation scope.
    Application {
        /// Normal application request; declarations do not confer authority.
        request: bcode_tool::ToolInvocationServiceRequest,
    },
}

/// Whether the host began this execution normally or resumed unresolved work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionContinuity {
    /// Ordinary admitted execution.
    Fresh,
    /// Recovery execution; notification does not authorize replay.
    Recovery,
}
