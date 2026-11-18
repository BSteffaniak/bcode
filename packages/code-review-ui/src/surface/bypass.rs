//! Stepping the review's agents into and out of permission bypass (see [`crate::bypass`]).
//!
//! * **Review-wide** (`bcode.code_review.permission_bypass`): every agent session of the
//!   review (and every one it starts during this launch), guide agents included unless
//!   `bcode.code_review.permission_bypass_guides` turned them off.
//! * **One card** (`bcode.code_review.permission_bypass_card`): the AI card at the cursor.
//! * Stepping in asks first once per launch; `…_always` steps in and stops asking. Stepping in
//!   then does what `permissions.bypass_waiting_prompts` says with prompts already waiting.

use bcode_session_models::TurnPermissionMode;
use bmux_tui_components::status_bar::StatusSeverity;
use bmux_tui_components::toast_stack::ToastSeverity;

use super::super::loader::Message;
use super::ReviewSurface;
use crate::bypass::{self, AgentKind, Waiting, waiting};
use crate::context::UiContext;

/// What a change of the review's agents' modes did.
#[derive(Debug)]
pub struct BypassDone {
    /// What was asked: review-wide, guide agents, or one card's session.
    pub scope: BypassScope,
    /// The mode set.
    pub mode: TurnPermissionMode,
    /// Each session's outcome.
    pub outcomes: Vec<(String, Result<TurnPermissionMode, String>)>,
    /// Prompts already waiting on the sessions stepped in: approved, or left waiting.
    pub waiting: Waiting,
    /// Why the review's sessions could not be found, when they could not.
    pub error: Option<String>,
}

/// Which sessions a change was for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BypassScope {
    /// Every agent of the review the policy covers.
    Review,
    /// The review's guide agents.
    Guides,
    /// One AI card's session.
    Card,
}

impl ReviewSurface {
    /// Step the review's agents into bypass (asking first once per launch), or out of it.
    /// `dont_ask_again` steps in and stops asking.
    pub(super) fn toggle_review_bypass(&mut self, dont_ask_again: bool, host: &UiContext) {
        let mut policy = self.services.bypass_policy();
        let on = dont_ask_again || !policy.review_wide;
        if on && !self.confirm_bypass(dont_ask_again) {
            return;
        }
        policy.review_wide = on;
        self.services.set_bypass_policy(policy);
        self.apply_review_bypass(BypassScope::Review, host);
    }

    /// Include the review's guide agents in review-wide bypass, or leave them out.
    pub(super) fn toggle_guides_bypass(&mut self, host: &UiContext) {
        let mut policy = self.services.bypass_policy();
        policy.include_guides = !policy.include_guides;
        self.services.set_bypass_policy(policy);
        if !policy.review_wide {
            self.set_status(
                if policy.include_guides {
                    "Review-wide permission bypass will include guide agents"
                } else {
                    "Review-wide permission bypass will leave guide agents out"
                },
                StatusSeverity::Info,
            );
            return;
        }
        self.apply_review_bypass(BypassScope::Guides, host);
    }

    /// Step the AI card at the cursor into bypass (asking first once per launch), or out of it.
    pub(super) fn toggle_card_bypass(&mut self, host: &UiContext) {
        let Some(session) = self
            .agent_at_cursor()
            .and_then(|exchange| self.notes.exchange(&exchange))
            .and_then(|exchange| exchange.session_id.clone())
        else {
            self.set_status(
                "move to an AI card with a session first",
                StatusSeverity::Warning,
            );
            return;
        };
        let on = !self
            .agents
            .state(&session)
            .is_some_and(crate::agent::AgentState::bypass);
        if on && !self.confirm_bypass(false) {
            return;
        }
        let mode = if on {
            TurnPermissionMode::Bypass
        } else {
            TurnPermissionMode::Enforce
        };
        let services = self.services.clone();
        let inbox = self.inbox.clone();
        host.spawn(Box::pin(async move {
            let sessions = vec![session];
            let waiting = waiting(&services, &sessions, mode).await;
            let outcomes = bypass::set_modes(&services, sessions, mode).await;
            inbox.push(Message::Bypass(Box::new(BypassDone {
                scope: BypassScope::Card,
                mode,
                outcomes,
                waiting,
                error: None,
            })));
        }));
    }

    /// Whether stepping in may go ahead; the first time in a launch, say what it does instead.
    fn confirm_bypass(&mut self, dont_ask_again: bool) -> bool {
        let (go, problem) = self.services.confirm_bypass(dont_ask_again);
        if let Some(problem) = problem {
            self.toasts.push(ToastSeverity::Warning, problem, None);
        }
        if !go {
            self.toasts.push_sticky(
                ToastSeverity::Warning,
                "Permission bypass is not on yet",
                Some(bypass::CONFIRMATION.to_owned()),
            );
        }
        go
    }

    /// Bring the review's agent sessions in `scope` to what the policy says now.
    pub(super) fn apply_review_bypass(&self, scope: BypassScope, host: &UiContext) {
        let Some(workspace_id) = self.workspace().map(|workspace| workspace.id.clone()) else {
            return;
        };
        let policy = self.services.bypass_policy();
        let services = self.services.clone();
        let inbox = self.inbox.clone();
        let repo = self.repo_path.clone();
        host.spawn(Box::pin(async move {
            let sessions = match bypass::review_sessions(&services, repo, &workspace_id).await {
                Ok(sessions) => sessions,
                Err(error) => {
                    inbox.push(Message::Bypass(Box::new(BypassDone {
                        scope,
                        mode: TurnPermissionMode::Enforce,
                        outcomes: Vec::new(),
                        waiting: Waiting::None,
                        error: Some(error),
                    })));
                    return;
                }
            };
            let in_scope =
                |kind: AgentKind| scope == BypassScope::Review || kind == AgentKind::Guide;
            let (bypassed, enforced): (Vec<_>, Vec<_>) = sessions
                .into_iter()
                .filter(|(_, kind)| in_scope(*kind))
                .partition(|(_, kind)| policy.covers(*kind));
            let (sessions, mode) = if bypassed.is_empty() {
                (enforced, TurnPermissionMode::Enforce)
            } else {
                (bypassed, TurnPermissionMode::Bypass)
            };
            let sessions: Vec<String> = sessions.into_iter().map(|(id, _)| id).collect();
            let waiting = waiting(&services, &sessions, mode).await;
            let outcomes = bypass::set_modes(&services, sessions, mode).await;
            inbox.push(Message::Bypass(Box::new(BypassDone {
                scope,
                mode,
                outcomes,
                waiting,
                error: None,
            })));
        }));
    }

    /// Say what a change of the review's agents' modes did.
    pub(super) fn apply_bypass(&mut self, done: BypassDone) {
        if let Some(error) = done.error {
            self.toasts.push(
                ToastSeverity::Error,
                "Could not find the review's agents",
                Some(error),
            );
            return;
        }
        let failed: Vec<&str> = done
            .outcomes
            .iter()
            .filter_map(|(_, outcome)| outcome.as_ref().err().map(String::as_str))
            .collect();
        let changed = done.outcomes.len() - failed.len();
        let on = done.mode == TurnPermissionMode::Bypass;
        let whom = match done.scope {
            BypassScope::Review => "the review's agents",
            BypassScope::Guides => "the review's guide agents",
            BypassScope::Card => "this card's agent",
        };
        let message = match (on, done.scope) {
            (true, BypassScope::Card) => {
                "Permission bypass on for this card: its tools run without asking".to_owned()
            }
            (false, BypassScope::Card) => "Permission bypass off for this card".to_owned(),
            (true, _) => {
                format!("Permission bypass on for {whom} ({changed} now; new ones start in it)")
            }
            (false, _) => format!("Permission bypass off for {whom}"),
        };
        let message = format!("{message}{}", done.waiting.suffix("on their cards"));
        if let Some(first) = failed.first() {
            self.toasts.push(
                ToastSeverity::Error,
                format!("Permission bypass not set for {} agent(s)", failed.len()),
                Some((*first).to_owned()),
            );
        }
        self.set_status(message, StatusSeverity::Info);
    }

    /// Whether review-wide bypass is on (the header says so).
    #[must_use]
    pub fn review_bypass(&self) -> bool {
        self.services.bypass_policy().review_wide
    }
}
