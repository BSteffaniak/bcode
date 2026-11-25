//! Native Bcode child-session delegation contracts.
//!
//! A tool invocation in a parent session may delegate a self-contained task to a fresh native
//! child session served by the same daemon and state location. The child is an ordinary child
//! (see [`crate::SessionChildCreateRequest`]); delegation additionally retains an immutable
//! [`SessionDelegationIntent`] in the child's canonical creation event. That intent governs
//! how the child's turns are authorized, where its interactions are surfaced and whether
//! interrupting the delegating turn interrupts the child.
//!
//! The host interface [`NATIVE_DELEGATION_INTERFACE_ID`] carries typed requests from an
//! authenticated tool invocation of the parent. Delegation grants control only to active tool
//! invocations of the child's canonical parent; it never grants decision authority over the
//! child's permission requests or questions. Those are surfaced in the parent session's view
//! and remain answerable only by the user.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{SessionActivityState, SessionId};

/// Host interface for native child-session delegation.
pub const NATIVE_DELEGATION_INTERFACE_ID: &str = "bcode.native-delegation/v1";
/// Create a delegated child and admit its first turn.
pub const NATIVE_DELEGATION_DELEGATE_OPERATION: &str = "delegate";
/// Admit the next turn of a delegated child.
pub const NATIVE_DELEGATION_CONTINUE_OPERATION: &str = "continue";
/// Summarize delegated children.
pub const NATIVE_DELEGATION_RESULT_OPERATION: &str = "result";
/// Wait until delegated children settle, need attention or a deadline passes.
pub const NATIVE_DELEGATION_WAIT_OPERATION: &str = "wait";
/// Interrupt a delegated child's active turn.
pub const NATIVE_DELEGATION_INTERRUPT_OPERATION: &str = "interrupt";
/// Describe the calling session's delegation context.
pub const NATIVE_DELEGATION_CONTEXT_OPERATION: &str = "context";
/// Every operation offered on [`NATIVE_DELEGATION_INTERFACE_ID`].
pub const NATIVE_DELEGATION_OPERATIONS: [&str; 6] = [
    NATIVE_DELEGATION_DELEGATE_OPERATION,
    NATIVE_DELEGATION_CONTINUE_OPERATION,
    NATIVE_DELEGATION_RESULT_OPERATION,
    NATIVE_DELEGATION_WAIT_OPERATION,
    NATIVE_DELEGATION_INTERRUPT_OPERATION,
    NATIVE_DELEGATION_CONTEXT_OPERATION,
];

/// Current [`SessionDelegationIntent`] version.
pub const SESSION_DELEGATION_INTENT_VERSION: u32 = 1;
/// Event schema that first retains a delegation intent in child creation.
pub const DELEGATION_EVENT_SCHEMA_VERSION: u16 = 60;
/// Storage writer epoch that first maintains delegation intents.
pub const DELEGATION_STORAGE_WRITER_EPOCH: u32 = 29;

/// Largest prompt accepted by delegation operations, in bytes.
pub const MAX_DELEGATION_PROMPT_BYTES: usize = 256 * 1024;
/// Largest identifier (agent, model, producer, invocation, request) in bytes.
pub const MAX_DELEGATION_ID_BYTES: usize = 256;
/// Most children one wait or result request may name.
pub const MAX_DELEGATION_BATCH: usize = 32;
/// Longest wait a single request may hold, in milliseconds.
pub const MAX_DELEGATION_WAIT_MS: u64 = 10 * 60 * 1000;
/// Largest result preview returned per child, in bytes.
pub const MAX_DELEGATION_RESULT_PREVIEW_BYTES: usize = 8 * 1024;
/// Most pending interactions reported per child.
pub const MAX_DELEGATION_PENDING: usize = 8;
/// Default bound on a parent's simultaneously active delegated children.
pub const DEFAULT_MAX_ACTIVE_DELEGATED_CHILDREN: u32 = 32;

/// How a delegated child's turns are authorized.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionDelegationPermissions {
    /// Follow the child's agent policy, bypassing it only while the parent's own delegating
    /// turn and session are in bypass. A child never runs with more authority than its parent.
    #[default]
    Inherit,
    /// Follow the child's agent policy; never inherit the parent's bypass.
    AgentProfile,
    /// Ask the user before every action that requires permission, ignoring discretionary
    /// allowances (such as agent preauthorizations); denials still apply.
    AlwaysAsk,
}

/// Immutable delegation facts retained in a delegated child's canonical creation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionDelegationIntent {
    /// Intent schema; see [`SESSION_DELEGATION_INTENT_VERSION`].
    pub version: u32,
    /// Plugin whose tool invocation delegated the child.
    pub producer_plugin_id: String,
    /// Delegating tool invocation in the parent session.
    pub invocation_id: String,
    /// Parent turn whose invocation delegated the child, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegating_turn_id: Option<String>,
    /// Authorization policy for every turn of the child.
    pub permissions: SessionDelegationPermissions,
    /// Whether interrupting the delegating parent turn interrupts the child.
    pub cancel_with_turn: bool,
}

impl SessionDelegationIntent {
    /// Validate the intent's shape.
    ///
    /// # Errors
    ///
    /// * Unknown versions.
    /// * Empty, oversized or control-character identities.
    pub fn validate(&self) -> Result<(), String> {
        if self.version != SESSION_DELEGATION_INTENT_VERSION {
            return Err("unsupported delegation intent version".into());
        }
        identity(&self.producer_plugin_id, "producer_plugin_id")?;
        identity(&self.invocation_id, "invocation_id")?;
        if let Some(turn) = &self.delegating_turn_id {
            identity(turn, "delegating_turn_id")?;
        }
        Ok(())
    }
}

/// Validate a bounded, trimmed, control-free identity.
///
/// # Errors
///
/// * When `value` is empty, longer than [`MAX_DELEGATION_ID_BYTES`], untrimmed or contains
///   control characters.
pub fn identity(value: &str, field: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > MAX_DELEGATION_ID_BYTES
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(format!("invalid {field}"));
    }
    Ok(())
}

fn prompt(value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err("prompt must not be empty".into());
    }
    if value.len() > MAX_DELEGATION_PROMPT_BYTES {
        return Err("prompt exceeds bound".into());
    }
    Ok(())
}

/// Parent-configured limits enforced when delegating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeDelegationLimits {
    /// Greatest nesting depth of a delegated child below its root session; unlimited when
    /// absent. A root session's direct child has depth one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_depth: Option<u32>,
    /// Most simultaneously active children of one parent.
    pub max_active_children: u32,
}

impl Default for NativeDelegationLimits {
    fn default() -> Self {
        Self {
            max_depth: None,
            max_active_children: DEFAULT_MAX_ACTIVE_DELEGATED_CHILDREN,
        }
    }
}

/// Create a delegated child of the calling session and admit `prompt` as its first turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeDelegateRequest {
    /// Caller-derived child identity; an identical retry reconciles instead of duplicating.
    pub child_id: SessionId,
    /// Initial child title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// First turn.
    pub prompt: String,
    /// Agent profile for the child; the session default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Model reference resolved through the model catalog; the parent's selection when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Absolute working directory; the parent's workspace when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<PathBuf>,
    /// Whether `working_directory` must be a registered worktree of the parent's repository.
    #[serde(default)]
    pub worktree: bool,
    /// Authorization policy for the child.
    #[serde(default)]
    pub permissions: SessionDelegationPermissions,
    /// Whether interrupting the delegating parent turn interrupts the child.
    #[serde(default)]
    pub cancel_with_turn: bool,
    /// Limits enforced before creation.
    #[serde(default)]
    pub limits: NativeDelegationLimits,
}

impl NativeDelegateRequest {
    /// Validate the request's shape.
    ///
    /// # Errors
    ///
    /// * Empty or oversized prompts, invalid identities or titles.
    /// * Relative working directories, or a worktree request without a directory.
    /// * A zero active-children limit.
    pub fn validate(&self) -> Result<(), String> {
        prompt(&self.prompt)?;
        if let Some(title) = &self.title {
            identity(title, "title")?;
        }
        if let Some(agent) = &self.agent {
            identity(agent, "agent")?;
        }
        if let Some(model) = &self.model {
            identity(model, "model")?;
        }
        match &self.working_directory {
            Some(directory) if !directory.is_absolute() => {
                return Err("working_directory must be absolute".into());
            }
            None if self.worktree => {
                return Err("worktree requires a working_directory".into());
            }
            _ => {}
        }
        if self.limits.max_active_children == 0 || self.limits.max_depth == Some(0) {
            return Err("delegation limits must be positive".into());
        }
        Ok(())
    }
}

/// Admit the next turn of a delegated child once it is idle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeContinueRequest {
    /// Delegated child.
    pub session_id: SessionId,
    /// Next turn.
    pub prompt: String,
    /// Caller-derived admission identity; an identical retry reconciles.
    pub idempotency_key: String,
}

impl NativeContinueRequest {
    /// Validate the request's shape.
    ///
    /// # Errors
    ///
    /// * Empty or oversized prompts and invalid idempotency keys.
    pub fn validate(&self) -> Result<(), String> {
        prompt(&self.prompt)?;
        identity(&self.idempotency_key, "idempotency_key")
    }
}

/// Summarize delegated children of the calling session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeResultRequest {
    /// Children to summarize, at most [`MAX_DELEGATION_BATCH`].
    pub session_ids: Vec<SessionId>,
}

/// Condition a wait completes on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeWaitUntil {
    /// Return when any child settles, fails or needs attention.
    #[default]
    Any,
    /// Return when every child has settled, failed or needs attention.
    All,
}

/// Wait for delegated children.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeWaitRequest {
    /// Children to wait for, at most [`MAX_DELEGATION_BATCH`].
    pub session_ids: Vec<SessionId>,
    /// Completion condition.
    #[serde(default)]
    pub until: NativeWaitUntil,
    /// Deadline, at most [`MAX_DELEGATION_WAIT_MS`].
    pub timeout_ms: u64,
}

/// Validate a bounded, non-empty, duplicate-free child batch.
///
/// # Errors
///
/// * Empty or oversized batches and duplicate children.
pub fn validate_batch(session_ids: &[SessionId]) -> Result<(), String> {
    if session_ids.is_empty() || session_ids.len() > MAX_DELEGATION_BATCH {
        return Err(format!(
            "name between 1 and {MAX_DELEGATION_BATCH} delegated children"
        ));
    }
    let unique = session_ids
        .iter()
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    if unique != session_ids.len() {
        return Err("duplicate delegated child".into());
    }
    Ok(())
}

impl NativeWaitRequest {
    /// Validate the request's shape.
    ///
    /// # Errors
    ///
    /// * Invalid batches and timeouts beyond [`MAX_DELEGATION_WAIT_MS`].
    pub fn validate(&self) -> Result<(), String> {
        validate_batch(&self.session_ids)?;
        if self.timeout_ms > MAX_DELEGATION_WAIT_MS {
            return Err("timeout exceeds bound".into());
        }
        Ok(())
    }
}

/// Interrupt a delegated child's active turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeInterruptRequest {
    /// Delegated child.
    pub session_id: SessionId,
}

/// Describe the calling session's delegation context.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeContextRequest {
    /// Limits the plugin enforces, reported back for the model.
    #[serde(default)]
    pub limits: NativeDelegationLimits,
}

/// Normalized delegated-child status, derived from observed activity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeChildStatus {
    /// A turn is executing or queued.
    Running,
    /// A permission decision or question is pending.
    NeedsAttention,
    /// No active work; the latest turn finished. Settled is not accepted or complete.
    Settled,
    /// The latest turn failed.
    Failed,
    /// The latest turn was interrupted.
    Interrupted,
    /// State cannot be verified.
    Unknown,
}

impl NativeChildStatus {
    /// Normalize concurrent observed activity states.
    #[must_use]
    pub fn from_states(states: &[SessionActivityState]) -> Self {
        let has = |state| states.contains(&state);
        if has(SessionActivityState::PermissionWait) || has(SessionActivityState::InputWait) {
            Self::NeedsAttention
        } else if has(SessionActivityState::Running) || has(SessionActivityState::Queued) {
            Self::Running
        } else if has(SessionActivityState::Failed) {
            Self::Failed
        } else if has(SessionActivityState::Interrupted) {
            Self::Interrupted
        } else if has(SessionActivityState::Idle) {
            Self::Settled
        } else {
            Self::Unknown
        }
    }

    /// Whether a waiter should stop waiting for this child.
    ///
    /// Unverifiable state (for example a child whose scheduler is not yet observed) keeps
    /// waiting; a wait still returns at its deadline.
    #[must_use]
    pub const fn is_terminal_for_wait(self) -> bool {
        !matches!(self, Self::Running | Self::Unknown)
    }
}

/// Kind of pending child interaction; every kind is answered by the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativePendingKind {
    /// A permission decision.
    Permission,
    /// A question or other tool exchange.
    Question,
}

/// One pending child interaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativePendingInteraction {
    /// Request identity.
    pub request_id: String,
    /// Interaction kind.
    pub kind: NativePendingKind,
    /// Tool that requested permission, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// Bounded prompt preview; untrusted child output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

/// Bounded summary of one delegated child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeChildSummary {
    /// Delegated child.
    pub session_id: SessionId,
    /// Current title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Normalized status.
    pub status: NativeChildStatus,
    /// Latest assistant text, bounded; untrusted child output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_result: Option<String>,
    /// Whether `latest_result` was truncated.
    #[serde(default)]
    pub latest_result_truncated: bool,
    /// Pending interactions, at most [`MAX_DELEGATION_PENDING`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending: Vec<NativePendingInteraction>,
    /// Bounded explanation when the summary is degraded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<String>,
}

/// Outcome of delegate or continue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeAdmissionResponse {
    /// Child summary after admission.
    pub child: NativeChildSummary,
    /// Whether this request reused an earlier identical admission.
    pub reused: bool,
}

/// Outcome of result or wait.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeSummariesResponse {
    /// Summaries in request order.
    pub children: Vec<NativeChildSummary>,
    /// Whether a wait returned at its deadline rather than its condition.
    #[serde(default)]
    pub timed_out: bool,
}

/// Outcome of interrupt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeControlResponse {
    /// Child summary after the control request.
    pub child: NativeChildSummary,
}

/// Agent profile offered for delegation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeDelegationAgent {
    /// Agent identity accepted by delegate.
    pub id: String,
    /// Whether the agent is the session default.
    #[serde(default)]
    pub default: bool,
}

/// The calling session's delegation context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeContextResponse {
    /// Calling session.
    pub session_id: SessionId,
    /// Its canonical parent, when it is a child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<SessionId>,
    /// Depth below its root session; a root session has depth zero.
    pub depth: u32,
    /// Canonical working directory.
    pub working_directory: PathBuf,
    /// Current model selection, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Agent profiles available to delegated children.
    pub agents: Vec<NativeDelegationAgent>,
    /// Direct children observed in one bounded page.
    pub children: Vec<NativeChildSummary>,
    /// Whether more children exist than were listed.
    #[serde(default)]
    pub children_truncated: bool,
    /// Limits the plugin enforces.
    pub limits: NativeDelegationLimits,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent() -> SessionDelegationIntent {
        SessionDelegationIntent {
            version: SESSION_DELEGATION_INTENT_VERSION,
            producer_plugin_id: "bcode.harness".into(),
            invocation_id: "call-1".into(),
            delegating_turn_id: Some("turn-1".into()),
            permissions: SessionDelegationPermissions::Inherit,
            cancel_with_turn: true,
        }
    }

    #[test]
    fn intents_validate_versions_and_identities() {
        assert!(intent().validate().is_ok());
        let mut future = intent();
        future.version = 2;
        assert!(future.validate().is_err());
        let mut blank = intent();
        blank.invocation_id = " ".into();
        assert!(blank.validate().is_err());
    }

    #[test]
    fn intents_reject_unknown_fields() {
        let mut value = serde_json::to_value(intent()).unwrap();
        value["grant"] = serde_json::json!(true);
        assert!(serde_json::from_value::<SessionDelegationIntent>(value).is_err());
    }

    fn delegate() -> NativeDelegateRequest {
        NativeDelegateRequest {
            child_id: SessionId::new(),
            title: None,
            prompt: "do the task".into(),
            agent: None,
            model: None,
            working_directory: None,
            worktree: false,
            permissions: SessionDelegationPermissions::default(),
            cancel_with_turn: false,
            limits: NativeDelegationLimits::default(),
        }
    }

    #[test]
    fn delegate_requests_validate_prompts_directories_and_limits() {
        assert!(delegate().validate().is_ok());
        assert!(
            NativeDelegateRequest {
                prompt: "  ".into(),
                ..delegate()
            }
            .validate()
            .is_err()
        );
        assert!(
            NativeDelegateRequest {
                working_directory: Some("relative".into()),
                ..delegate()
            }
            .validate()
            .is_err()
        );
        assert!(
            NativeDelegateRequest {
                worktree: true,
                ..delegate()
            }
            .validate()
            .is_err()
        );
        assert!(
            NativeDelegateRequest {
                limits: NativeDelegationLimits {
                    max_depth: Some(0),
                    max_active_children: 1,
                },
                ..delegate()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn defaults_inherit_without_cancellation_and_unlimited_depth() {
        let decoded: NativeDelegateRequest = serde_json::from_value(serde_json::json!({
            "child_id": SessionId::new(),
            "prompt": "task",
        }))
        .unwrap();
        assert_eq!(decoded.permissions, SessionDelegationPermissions::Inherit);
        assert!(!decoded.cancel_with_turn);
        assert_eq!(decoded.limits.max_depth, None);
        assert_eq!(
            decoded.limits.max_active_children,
            DEFAULT_MAX_ACTIVE_DELEGATED_CHILDREN
        );
    }

    #[test]
    fn batches_are_bounded_and_unique() {
        let id = SessionId::new();
        assert!(validate_batch(&[]).is_err());
        assert!(validate_batch(&[id, id]).is_err());
        assert!(validate_batch(&[id]).is_ok());
        let many = (0..=MAX_DELEGATION_BATCH)
            .map(|_| SessionId::new())
            .collect::<Vec<_>>();
        assert!(validate_batch(&many).is_err());
        assert!(
            NativeWaitRequest {
                session_ids: vec![id],
                until: NativeWaitUntil::All,
                timeout_ms: MAX_DELEGATION_WAIT_MS + 1,
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn attention_outranks_running_and_idle_is_settled() {
        use SessionActivityState as S;
        assert_eq!(
            NativeChildStatus::from_states(&[S::Running, S::PermissionWait]),
            NativeChildStatus::NeedsAttention
        );
        assert_eq!(
            NativeChildStatus::from_states(&[S::Queued]),
            NativeChildStatus::Running
        );
        assert_eq!(
            NativeChildStatus::from_states(&[S::Idle]),
            NativeChildStatus::Settled
        );
        assert_eq!(
            NativeChildStatus::from_states(&[S::Idle, S::Failed]),
            NativeChildStatus::Failed
        );
        assert_eq!(
            NativeChildStatus::from_states(&[]),
            NativeChildStatus::Unknown
        );
        assert!(!NativeChildStatus::Running.is_terminal_for_wait());
        assert!(!NativeChildStatus::Unknown.is_terminal_for_wait());
        assert!(NativeChildStatus::NeedsAttention.is_terminal_for_wait());
    }
}
