//! Independent foreground outcomes, associated work, and native continuations.
//!
//! Adapter identities identify observations, never native execution authority. A terminal
//! foreground outcome does not imply that associated work or continuations have ended.

use crate::{HarnessContractError, HarnessTerminalDisposition, MAX_HARNESS_ITEMS, id, text};
use serde::{Deserialize, Serialize};

/// Exact retained Bcode submission attempt; not a native turn identifier.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessAttemptIdentity {
    /// Canonical submission identity.
    pub submission_id: String,
    /// Canonical delivery-attempt identity.
    pub attempt_id: String,
}

impl HarnessAttemptIdentity {
    /// Check identity shape, without granting delivery or execution authority.
    /// # Errors
    /// Returns an error for malformed or oversized identities.
    pub fn validate(&self) -> Result<(), HarnessContractError> {
        id(&self.submission_id)?;
        id(&self.attempt_id)
    }
}

/// State of one adapter-observed background task or automatic continuation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessWorkState {
    /// Known work awaiting execution.
    Pending,
    /// Work has started and has not ended.
    Running,
    /// Authoritative successful end of this work item.
    Completed,
    /// Authoritative failed end of this work item.
    Failed,
    /// Authoritative interrupted end of this work item.
    Interrupted,
    /// Observation loss or unsupported semantics prevent a current conclusion.
    Unknown,
}

impl HarnessWorkState {
    /// Whether this individual work item has an authoritative terminal outcome.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Interrupted)
    }
}

/// Native activity classification; none of these authorize Bcode tool dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessWorkKind {
    /// Native command or managed process.
    Command,
    /// Native delegated agent.
    Subagent,
    /// Persistent native tool activity.
    Tool,
    /// Activity whose lifecycle cannot be interpreted more specifically.
    Other,
}

/// Notification delivery is independent of task completion and follow-up execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessNotificationState {
    /// Native completion may still produce a notification or automatic follow-up.
    Pending,
    /// Completion notification was observed; any resulting continuation is separate.
    Observed,
    /// Verified protocol semantics establish that no notification is owed.
    NotExpected,
    /// Notification coverage is unavailable.
    Unknown,
}

/// Scope and completeness of a tracked lifecycle dimension, not a future-silence promise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessTrackingCoverage {
    /// Complete supported tracking through this observation on the current connection.
    Complete,
    /// Missing or unsupported evidence; an empty inventory is not proof of completion.
    Unknown,
}

/// Required boundary for a caller consuming managed execution evidence.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessCompletionRequirement {
    /// Exact foreground terminal outcome and complete relevant output only.
    Foreground,
    /// Foreground plus associated background work and required continuations.
    #[default]
    AssociatedWork,
    /// Foreground completion only after a canonical workflow destination durably adopts
    /// observation, cancellation and all retained resource responsibility.
    AsyncHandoff,
    /// Associated work plus separately established host workspace-release authority.
    /// Tracking coverage alone must never satisfy this boundary.
    WorkspaceRelease,
}

/// Normalized adapter evidence. Collection sizes, identities, and text are bounded.
///
/// Events inherit session/generation/observation identity from their enclosing observation.
/// Persistence validates retained attempts, immutable terminals, predecessor order and
/// association consistency; structural validation here cannot establish those facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum HarnessExecutionEvent {
    /// Foreground outcome correlated directly to an exact retained attempt.
    SubmissionTerminal {
        /// Exact canonical attempt, never reconstructed from display output.
        attempt: HarnessAttemptIdentity,
        /// Genuine native turn identity when supplied by the protocol.
        native_turn_id: Option<String>,
        /// Genuine native result-frame identity when supplied by the protocol.
        native_frame_id: Option<String>,
        /// Authoritative foreground outcome, independent of background activity.
        disposition: HarnessTerminalDisposition,
        /// Complete attributed output, absent when no complete output is available.
        output: Option<String>,
    },
    /// Current state of one stable adapter-owned native background work identity.
    BackgroundWork {
        /// Stable within the enclosing session generation; not necessarily a native ID.
        work_id: String,
        /// Genuine native task identifier, when available.
        native_task_id: Option<String>,
        /// Exact origin, or absent when uncorrelated (a session-wide safety fence).
        origin: Option<HarnessAttemptIdentity>,
        /// Parent work identity, when the native protocol establishes the relationship.
        parent_work_id: Option<String>,
        /// Semantic activity kind.
        work_kind: HarnessWorkKind,
        /// Sanitized bounded label, not raw protocol input.
        label: String,
        /// Lifecycle outcome of this work item.
        state: HarnessWorkState,
        /// Completion notification coverage, retained even after an empty task snapshot.
        notification: HarnessNotificationState,
    },
    /// Automatic native follow-up; never a Bcode dispatch authorization.
    Continuation {
        /// Stable adapter-owned follow-up execution identity.
        continuation_id: String,
        /// Exact originating attempt when known.
        origin: Option<HarnessAttemptIdentity>,
        /// Background work responsible for this continuation, when known.
        source_work_id: Option<String>,
        /// Genuine native turn identifier, not synthesized from this continuation ID.
        native_turn_id: Option<String>,
        /// Pending, running or terminal follow-up state.
        state: HarnessWorkState,
        /// Complete correlated follow-up output, when supplied.
        output: Option<String>,
    },
    /// Supported tracking coverage through this observation for an exact attempt.
    /// Does not erase retained work, outstanding notifications, or continuations.
    Coverage {
        /// Attempt whose associated-work boundary is being observed.
        attempt: HarnessAttemptIdentity,
        /// Whether relevant background lifecycle evidence is completely tracked.
        background: HarnessTrackingCoverage,
        /// Whether relevant notifications and automatic follow-ups are completely tracked.
        continuations: HarnessTrackingCoverage,
    },
}

impl HarnessExecutionEvent {
    /// Validate portable bounds and required native evidence shape.
    /// # Errors
    /// Rejects invalid identities, excessive text, missing native terminal references,
    /// and self-referential work relationships.
    pub fn validate(&self) -> Result<(), HarnessContractError> {
        match self {
            Self::SubmissionTerminal {
                attempt,
                native_turn_id,
                native_frame_id,
                output,
                ..
            } => {
                attempt.validate()?;
                if native_turn_id.is_none() && native_frame_id.is_none() {
                    return Err(HarnessContractError::InvalidShape);
                }
                optional_id(native_turn_id.as_deref())?;
                optional_id(native_frame_id.as_deref())?;
                optional_text(output.as_deref())
            }
            Self::BackgroundWork {
                work_id,
                native_task_id,
                origin,
                parent_work_id,
                label,
                ..
            } => {
                id(work_id)?;
                optional_id(native_task_id.as_deref())?;
                optional_attempt(origin.as_ref())?;
                optional_id(parent_work_id.as_deref())?;
                if parent_work_id.as_ref() == Some(work_id) {
                    return Err(HarnessContractError::InvalidShape);
                }
                text(label)
            }
            Self::Continuation {
                continuation_id,
                origin,
                source_work_id,
                native_turn_id,
                output,
                ..
            } => {
                id(continuation_id)?;
                optional_attempt(origin.as_ref())?;
                optional_id(source_work_id.as_deref())?;
                optional_id(native_turn_id.as_deref())?;
                optional_text(output.as_deref())
            }
            Self::Coverage { attempt, .. } => attempt.validate(),
        }
    }
}

/// Maximum retained lifecycle entries per indexed execution snapshot.
///
/// This limits a bounded read or outstanding responsibility set, not conversation
/// lifetime. Terminal entries must remain incrementally accessible through canonical
/// indexes without forcing adapters to retain every historical task in memory.
pub const MAX_HARNESS_EXECUTION_ITEMS: usize = MAX_HARNESS_ITEMS;

fn optional_id(value: Option<&str>) -> Result<(), HarnessContractError> {
    value.map_or(Ok(()), id)
}
fn optional_text(value: Option<&str>) -> Result<(), HarnessContractError> {
    value.map_or(Ok(()), text)
}
fn optional_attempt(value: Option<&HarnessAttemptIdentity>) -> Result<(), HarnessContractError> {
    value.map_or(Ok(()), HarnessAttemptIdentity::validate)
}
