//! Permission bypass for the review's agent sessions.
//!
//! * **The mode is each session's.** Bcode keeps a runtime permission mode per session; the
//!   app steps its sessions in and out through the host. Cards and the assistant panel show
//!   the mode of the sessions they follow, wherever it was changed.
//! * **Review-wide.** While review-wide bypass is on, the review's agent sessions are stepped in
//!   (the ones it started before, found by their bindings to the review) and every session it
//!   starts during this launch starts in bypass ([`start_session`]). Guide agents are included
//!   unless the reviewer turned that off; it can change at any time.
//! * **Kinds.** A session's kind comes from the origin the app created it with: an inline
//!   question, the assistant panel, or a guide writer; a child session (a guide fan-out writer
//!   or verifier) has no origin and is a guide agent.

use bcode_config::permissions::BypassWaitingPrompts;
use bcode_session_models::TurnPermissionMode;

use crate::services::ReviewServices;

/// What one of the review's agent sessions is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AgentKind {
    /// An inline question (an AI card).
    Question,
    /// The assistant panel's conversation.
    Assistant,
    /// A guide writer or verifier.
    Guide,
}

impl AgentKind {
    /// The kind of a session created with origin purpose `purpose` (none for child sessions).
    #[must_use]
    pub fn of_purpose(purpose: Option<&str>) -> Self {
        match purpose {
            Some(purpose) if purpose == crate::host::ReviewSessionPurpose::Question.as_str() => {
                Self::Question
            }
            Some(purpose) if purpose == crate::host::ReviewSessionPurpose::Assistant.as_str() => {
                Self::Assistant
            }
            _ => Self::Guide,
        }
    }
}

/// Which of the review's sessions run in bypass during this launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BypassPolicy {
    /// Review-wide bypass is on.
    pub review_wide: bool,
    /// Guide agents are included in review-wide bypass.
    pub include_guides: bool,
}

impl Default for BypassPolicy {
    fn default() -> Self {
        Self {
            review_wide: false,
            include_guides: true,
        }
    }
}

impl BypassPolicy {
    /// Whether review-wide bypass covers a session of `kind`.
    #[must_use]
    pub fn covers(self, kind: AgentKind) -> bool {
        self.review_wide && (kind != AgentKind::Guide || self.include_guides)
    }
}

/// Stepping into bypass is confirmed once per launch, unless the reviewer chose not to be asked
/// again (remembered in Bcode's state directory, shared with the TUI's `/yolo always`).
#[derive(Debug, Default)]
pub struct Confirmation {
    /// Stepping in was confirmed during this launch.
    confirmed: bool,
    /// The confirmation was shown; stepping in again confirms it.
    asked: bool,
    /// Where the choice not to be asked again is kept; the state directory's when `None`.
    path: Option<std::path::PathBuf>,
}

impl Confirmation {
    /// Keep the choice not to be asked again at `path`.
    #[must_use]
    pub fn at(path: std::path::PathBuf) -> Self {
        Self {
            path: Some(path),
            ..Self::default()
        }
    }

    fn path(&self) -> std::path::PathBuf {
        self.path
            .clone()
            .unwrap_or_else(bcode_config::bypass_confirmation::default_path)
    }

    /// Count stepping in as confirmed for this launch (the launch asked for bypass).
    pub const fn confirm(&mut self) {
        self.confirmed = true;
    }

    /// Whether stepping in may go ahead now; `dont_ask_again` also remembers not to ask.
    /// Returns `false` the first time in a launch (the caller shows [`CONFIRMATION`]), `true`
    /// once confirmed. A failure to remember is returned with `true`.
    pub fn step_in(&mut self, dont_ask_again: bool) -> (bool, Option<String>) {
        let mut problem = None;
        if dont_ask_again {
            if let Err(error) = bcode_config::bypass_confirmation::dismiss_at(&self.path()) {
                problem = Some(format!("could not remember not to ask again: {error}"));
            }
            self.confirmed = true;
        }
        if !self.confirmed
            && !bcode_config::bypass_confirmation::load_from(&self.path())
                .is_ok_and(|state| state.dismissed)
        {
            if !self.asked {
                self.asked = true;
                return (false, problem);
            }
            self.confirmed = true;
        }
        self.asked = false;
        (true, problem)
    }
}

/// What the confirmation says.
pub const CONFIRMATION: &str = "Permission bypass lets these agents' tools run without asking \
(commands, file writes and edits, and every other tool their agent offers) until you step out. \
Do it again to step in, or use “Permission bypass: step in and don't ask again”.";

/// Step a session the review just created into bypass when review-wide bypass covers its kind;
/// call before its first turn.
pub async fn start_session(services: &ReviewServices, session_id: &str, kind: AgentKind) {
    if services.bypass_policy().covers(kind) {
        // A host or daemon without runtime modes leaves the session as it is.
        drop(
            services
                .host()
                .set_session_permission_mode(session_id.to_owned(), TurnPermissionMode::Bypass)
                .await,
        );
    }
}

/// Step a new question's session into bypass when the asker chose it (`chosen`) or review-wide
/// bypass covers questions; call before its first turn.
pub async fn start_question(services: &ReviewServices, session_id: &str, chosen: bool) {
    if chosen {
        // A host or daemon without runtime modes leaves the session as it is.
        drop(
            services
                .host()
                .set_session_permission_mode(session_id.to_owned(), TurnPermissionMode::Bypass)
                .await,
        );
    } else {
        start_session(services, session_id, AgentKind::Question).await;
    }
}

/// The review's agent sessions: the sessions bound to workspace `workspace_id`, with their
/// kinds.
///
/// # Errors
///
/// Returns the store's or host's message when the bindings or sessions cannot be read.
pub async fn review_sessions(
    services: &ReviewServices,
    repo_path: std::path::PathBuf,
    workspace_id: &str,
) -> Result<Vec<(String, AgentKind)>, String> {
    let bindings = services
        .call(bcode_code_review_models::ListReviewAssistantsRequest {
            repo_path,
            limit: None,
        })
        .await
        .map_err(|error| error.message)?
        .bindings;
    let ids: Vec<String> = bindings
        .into_iter()
        .filter(|binding| binding.workspace.id == workspace_id)
        .map(|binding| binding.session_id)
        .collect();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let summaries = services
        .host()
        .session_summaries(ids.clone())
        .await
        .map_err(|error| error.message)?;
    Ok(ids
        .into_iter()
        .zip(summaries)
        // A session Bcode no longer has is left out.
        .filter_map(|(id, summary)| {
            let summary = summary?;
            let purpose = summary
                .origin_context
                .get(crate::host::ORIGIN_PURPOSE)
                .map(String::as_str);
            Some((id, AgentKind::of_purpose(purpose)))
        })
        .collect())
}

/// Set `mode` on each of `sessions`; returns each session's outcome.
pub async fn set_modes(
    services: &ReviewServices,
    sessions: Vec<String>,
    mode: TurnPermissionMode,
) -> Vec<(String, Result<TurnPermissionMode, String>)> {
    let mut outcomes = Vec::with_capacity(sessions.len());
    for session in sessions {
        let outcome = services
            .host()
            .set_session_permission_mode(session.clone(), mode)
            .await
            .map_err(|error| error.message);
        outcomes.push((session, outcome));
    }
    outcomes
}

/// Approve, once each, the permission prompts `sessions` are waiting on; returns how many.
pub async fn approve_waiting(services: &ReviewServices, sessions: &[String]) -> usize {
    let Ok(pending) = services.host().pending_permissions().await else {
        return 0;
    };
    let mut approved = 0;
    for permission in pending
        .into_iter()
        .filter(|permission| sessions.contains(&permission.session_id))
    {
        if services
            .host()
            .resolve_permission(permission.permission_id, true, false)
            .await
            .is_ok()
        {
            approved += 1;
        }
    }
    approved
}

/// How many permission prompts `sessions` are waiting on.
pub async fn count_waiting(services: &ReviewServices, sessions: &[String]) -> usize {
    services
        .host()
        .pending_permissions()
        .await
        .map_or(0, |pending| {
            pending
                .iter()
                .filter(|permission| sessions.contains(&permission.session_id))
                .count()
        })
}

/// Prompts already waiting when sessions stepped in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Waiting {
    /// None waited, or stepping out.
    #[default]
    None,
    /// This many were approved.
    Approved(usize),
    /// This many wait to be answered on their cards.
    Left(usize),
}

impl Waiting {
    /// What a status line adds about the waiting prompts: approved, or left waiting `where_`.
    #[must_use]
    pub fn suffix(self, where_: &str) -> String {
        match self {
            Self::Approved(count) if count > 0 => format!("; approved {count} waiting prompt(s)"),
            Self::Left(count) if count > 0 => {
                format!("; {count} prompt(s) already waiting {where_}")
            }
            _ => String::new(),
        }
    }
}

/// Handle the prompts `sessions` already wait on, when stepping them in.
pub async fn waiting(
    services: &ReviewServices,
    sessions: &[String],
    mode: TurnPermissionMode,
) -> Waiting {
    if mode != TurnPermissionMode::Bypass {
        return Waiting::None;
    }
    match services.permissions().bypass_waiting_prompts {
        BypassWaitingPrompts::Approve => {
            Waiting::Approved(approve_waiting(services, sessions).await)
        }
        BypassWaitingPrompts::Ask => Waiting::Left(count_waiting(services, sessions).await),
        BypassWaitingPrompts::Leave => Waiting::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stepping_in_is_confirmed_once_per_launch_unless_dismissed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("permission-bypass.toml");
        let mut confirmation = Confirmation::at(path.clone());
        assert_eq!(confirmation.step_in(false), (false, None));
        assert_eq!(confirmation.step_in(false), (true, None));
        assert_eq!(
            confirmation.step_in(false),
            (true, None),
            "confirmed for the launch"
        );

        let mut next_launch = Confirmation::at(path.clone());
        assert_eq!(next_launch.step_in(true), (true, None));
        let mut later = Confirmation::at(path);
        assert_eq!(later.step_in(false), (true, None), "remembered");
    }

    #[test]
    fn review_wide_bypass_covers_guides_only_when_they_are_included() {
        let off = BypassPolicy::default();
        assert!(!off.covers(AgentKind::Question));
        let on = BypassPolicy {
            review_wide: true,
            ..off
        };
        for kind in [AgentKind::Question, AgentKind::Assistant, AgentKind::Guide] {
            assert!(on.covers(kind), "{kind:?}");
        }
        let without_guides = BypassPolicy {
            include_guides: false,
            ..on
        };
        assert!(without_guides.covers(AgentKind::Assistant));
        assert!(!without_guides.covers(AgentKind::Guide));
    }

    #[test]
    fn a_session_without_a_review_purpose_is_a_guide_agent() {
        assert_eq!(AgentKind::of_purpose(Some("question")), AgentKind::Question);
        assert_eq!(
            AgentKind::of_purpose(Some("assistant")),
            AgentKind::Assistant
        );
        assert_eq!(AgentKind::of_purpose(Some("guide")), AgentKind::Guide);
        assert_eq!(AgentKind::of_purpose(None), AgentKind::Guide);
    }
}
