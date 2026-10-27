//! Secret-free account-first routing contracts. Account identities never contain credentials.

use serde::{Deserialize, Serialize};

/// Configuration-local credential profile identity, deduplicated across pools.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AccountId {
    /// Owning provider plugin ID.
    pub provider_id: String,
    /// Configured authentication profile name.
    pub profile: String,
}

/// Explicit account selection; an empty fallback list never authorizes implicit fallback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum AccountRoute {
    /// Follow declarative configuration and its configured fallback policy.
    FollowConfiguration,
    /// Use this account exclusively.
    Only { account: AccountId },
    /// Prefer this account, then only the explicitly listed alternatives, in order.
    Prefer {
        /// First account to attempt.
        account: AccountId,
        /// Explicit alternatives; empty means no fallback.
        alternatives: Vec<AccountId>,
    },
}

/// Which routing state is changed. New-session defaults never edit existing sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountRoutingScope {
    /// This session, starting with its next turn.
    Session,
    /// New sessions using the selected model preset in this configuration.
    NewSessions,
    /// Both the current session and new-session default.
    Both,
}

/// Set or reset routing for the session's selected model preset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetAccountRoutingRequest {
    /// State to update.
    pub scope: AccountRoutingScope,
    /// Route to install; follow-configuration clears the override.
    pub route: AccountRoute,
}

/// Configuration-wide pause duration, independent of pool membership.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountPause {
    /// Eligible again (subject to all other policy).
    Resume,
    /// Paused until explicitly resumed.
    Manual,
    /// Pause for one hour from application time.
    OneHour,
    /// Pause for four hours from application time.
    FourHours,
}

/// Pause/resume one account throughout the current configuration, including account-only routes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetAccountPauseRequest {
    /// Account to change.
    pub account: AccountId,
    /// Configuration-wide pause policy.
    pub pause: AccountPause,
}

/// Rename display metadata only; never renames credential storage or profile identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenameAccountRequest {
    /// Account to change.
    pub account: AccountId,
    /// Friendly label; `None` restores the configured profile name.
    pub label: Option<String>,
}

/// Effective pause state; expired timed pauses are reported as eligible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AccountPauseState {
    /// No active user pause.
    Eligible,
    /// Paused until manually resumed.
    Manual,
    /// Paused until this Unix timestamp (seconds).
    Until { unix_seconds: u64 },
}

/// Provider readiness evidence; absence never claims credentials are usable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AccountReadiness {
    /// No current bounded readiness proof.
    Unknown,
    /// Provider cooldown observed for at least one pool membership.
    Cooldown { until_unix: u64 },
}

/// Secret-free, deduplicated account details.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountSummary {
    /// Stable configuration-local identity.
    pub account: AccountId,
    /// Effective friendly display label.
    pub label: String,
    /// All configured pools containing this account.
    pub pools: Vec<String>,
    /// Configuration-wide pause state.
    pub pause: AccountPauseState,
    /// Provider readiness independent of manual pause.
    pub readiness: AccountReadiness,
    /// Last successful dispatch, not a prediction of next selection.
    pub last_used_unix: Option<u64>,
}

/// Next-turn routing state, distinct from historical usage and any frozen running turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountRouting {
    /// Deduplicated accounts visible in the configured context/custody boundary.
    pub accounts: Vec<AccountSummary>,
    /// Secret-free effective pool membership, strategy, and preference details.
    pub pools: Vec<crate::AuthPoolSummary>,
    /// Selected model preset key, if one is selected.
    pub model_profile: Option<String>,
    /// Effective route for the session's next turn.
    pub session_route: AccountRoute,
    /// Default for future sessions using this model preset.
    pub new_session_route: AccountRoute,
    /// Last account actually used by this session, if known.
    pub last_used: Option<AccountId>,
    /// Whether a turn is running with its already-frozen routing snapshot.
    pub running_turn: bool,
}
