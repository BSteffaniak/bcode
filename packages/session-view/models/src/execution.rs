//! Renderer-neutral evidence of foreground outcomes and independently continuing work.
use serde::{Deserialize, Serialize};

/// Exact submission attempt within a session generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionAttemptView {
    /// Bcode submission identity, never a native turn ID.
    pub submission_id: String,
    /// Delivery attempt identity.
    pub attempt_id: String,
}

/// Orthogonal execution state. Unknown never invents a successful outcome.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStateView {
    /// Awaiting execution.
    Pending,
    /// Native activity observed.
    Running,
    /// Authoritative successful end.
    Completed,
    /// Authoritative failed end.
    Failed,
    /// Authoritative interrupted end.
    Interrupted,
    /// Evidence unavailable or inconsistent.
    #[default]
    Unknown,
}
impl ExecutionStateView {
    /// Whether an authoritative terminal disposition has been recorded.
    #[must_use]
    pub const fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Interrupted)
    }
}

/// Completion notification, independent of work and continuation state.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionNotificationView {
    /// Notification remains owed.
    Pending,
    /// Notification observed; follow-up execution may still be pending.
    Observed,
    /// Native semantics establish that no notification is owed.
    NotExpected,
    /// Notification tracking unavailable.
    #[default]
    Unknown,
}

/// One bounded projected lifecycle entity. Its native identity is never fabricated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionItemView {
    /// Enclosing ownership generation.
    pub generation: u64,
    /// Adapter-owned identity, not native execution authority.
    pub id: String,
    /// Optional native identifier supplied by canonical evidence.
    pub native_id: Option<String>,
    /// Verified origin; absence fences the whole session.
    pub origin: Option<ExecutionAttemptView>,
    /// Optional adapter-owned parent/source work identity.
    pub source_work_id: Option<String>,
    /// Sanitized bounded display label.
    pub label: String,
    /// Current lifecycle state; authoritative terminal outcomes cannot reopen.
    pub state: ExecutionStateView,
    /// Notification lifecycle remains independent.
    pub notification: ExecutionNotificationView,
}

/// Tracking coverage through a canonical observation, not a promise of future silence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionCoverageView {
    /// Enclosing ownership generation.
    pub generation: u64,
    /// Attempt whose tracking was observed.
    pub attempt: ExecutionAttemptView,
    /// Supported background tracking is complete through this observation only.
    pub background_complete: bool,
    /// Supported continuation tracking is complete through this observation only.
    pub continuations_complete: bool,
}

/// Bounded portable projection. This is display evidence, not a workflow completion verdict.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionLifecycleView {
    /// Session identity prevents cross-session observation mixing.
    pub session_id: Option<String>,
    /// Latest observed ownership generation; older observations cannot refresh current coverage.
    pub generation: Option<u64>,
    /// Foreground outcomes in canonical first-observed order.
    pub submissions: Vec<ExecutionItemView>,
    /// Independently observed background tasks.
    pub background: Vec<ExecutionItemView>,
    /// Independently observed automatic follow-ups.
    pub continuations: Vec<ExecutionItemView>,
    /// Explicit per-attempt coverage, independent of pagination.
    pub coverage: Vec<ExecutionCoverageView>,
    /// Rejected conflicts, gaps or bounded omission leave a visible safety fence.
    pub uncertain: bool,
    /// Entries were omitted; empty visible inventory is not settlement.
    pub truncated: bool,
}
impl ExecutionLifecycleView {
    /// Portable display budget, independent of lifetime execution capacity.
    pub const MAX_ITEMS: usize = 32;

    /// Validate bounded projection shape before rendering or recovery.
    #[must_use]
    pub fn is_bounded(&self) -> bool {
        self.submissions
            .len()
            .saturating_add(self.background.len())
            .saturating_add(self.continuations.len())
            .saturating_add(self.coverage.len())
            <= Self::MAX_ITEMS
            && self.session_id.as_ref().is_none_or(|id| id.len() <= 512)
            && self
                .submissions
                .iter()
                .chain(&self.background)
                .chain(&self.continuations)
                .all(|item| {
                    item.id.len() <= 1024
                        && item.label.len() <= 1024
                        && item.native_id.as_ref().is_none_or(|id| id.len() <= 512)
                        && item
                            .source_work_id
                            .as_ref()
                            .is_none_or(|id| id.len() <= 512)
                        && item.origin.as_ref().is_none_or(|origin| {
                            origin.submission_id.len() <= 512 && origin.attempt_id.len() <= 512
                        })
                })
            && self.coverage.iter().all(|item| {
                item.attempt.submission_id.len() <= 512 && item.attempt.attempt_id.len() <= 512
            })
    }

    /// Shared summary used unchanged by all renderers. Never claims workflow success.
    #[must_use]
    pub fn summary(&self) -> String {
        let reply = self
            .submissions
            .last()
            .map_or("Reply unknown", |item| match item.state {
                ExecutionStateView::Completed => "Reply complete",
                ExecutionStateView::Failed => "Reply failed",
                ExecutionStateView::Interrupted => "Reply interrupted",
                ExecutionStateView::Pending => "Reply pending",
                ExecutionStateView::Running => "Reply running",
                ExecutionStateView::Unknown => "Reply unknown",
            });
        let reply = if self.truncated {
            "Reply state unavailable: lifecycle details truncated"
        } else {
            reply
        };
        let active = self
            .background
            .iter()
            .filter(|item| !item.state.terminal())
            .count();
        let notifications = self
            .background
            .iter()
            .filter(|item| {
                matches!(
                    item.notification,
                    ExecutionNotificationView::Pending | ExecutionNotificationView::Unknown
                )
            })
            .count();
        let continuations = self
            .continuations
            .iter()
            .filter(|item| !item.state.terminal())
            .count();
        let unknown_origin = self
            .background
            .iter()
            .chain(&self.continuations)
            .any(|item| item.origin.is_none());
        let origin = self
            .submissions
            .last()
            .and_then(|item| item.origin.as_ref());
        let complete = origin.is_some_and(|origin| {
            self.coverage.iter().any(|item| {
                Some(item.generation) == self.generation
                    && &item.attempt == origin
                    && item.background_complete
                    && item.continuations_complete
            })
        });
        format!(
            "{reply} + {active} background · {notifications} notification pending · {continuations} continuation pending{}{} · not workflow completion",
            if self.uncertain
                || !complete
                || self
                    .background
                    .iter()
                    .chain(&self.continuations)
                    .any(|item| item.state == ExecutionStateView::Unknown)
            {
                " · coverage unknown"
            } else {
                " · tracking complete through observation"
            },
            if unknown_origin {
                " · unknown origin: session-wide fence"
            } else {
                ""
            }
        )
    }

    /// Bounded details, preserving foreground, task and continuation identities separately.
    #[must_use]
    pub fn details(&self) -> String {
        let mut text = self.summary();
        for (kind, items) in [
            ("Reply", &self.submissions),
            ("Task", &self.background),
            ("Continuation", &self.continuations),
        ] {
            for item in items {
                use std::fmt::Write as _;
                let origin = item.origin.as_ref().map_or_else(
                    || "unknown origin: session-wide fence".into(),
                    |origin| {
                        format!(
                            "submission {} · attempt {}",
                            origin.submission_id, origin.attempt_id
                        )
                    },
                );
                let _ = write!(
                    text,
                    "\n{kind} {} · generation {} · {} · {:?} · notification {:?} · {origin}",
                    item.id, item.generation, item.label, item.state, item.notification
                );
                if let Some(native_id) = &item.native_id {
                    let _ = write!(text, " · native {native_id}");
                }
                if let Some(source) = &item.source_work_id {
                    let _ = write!(text, " · source work {source}");
                }
            }
        }
        if self.truncated {
            text.push_str("\nLifecycle details truncated; unknown coverage remains fenced");
        }
        text
    }
}
