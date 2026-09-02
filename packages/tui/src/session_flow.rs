//! Session picker event flow for the TUI.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use bcode_agent_profile::AgentInfo;
use bcode_client::{AttachedSessionHistory, BcodeClient};
use bcode_session_models::SessionId;
use bmux_tui::geometry::Rect;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::app::BmuxApp;
use super::{TuiError, history_flow};

static PRESENTATION_NOTE_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Canonical attachment lifecycle for the chat's session view.
///
/// The renderer keeps exactly one representation of session attachment so identity and readiness
/// cannot contradict each other. Canonical session state remains owned by the application and
/// session layers; this only records how the current view is bound to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatSessionAttachment {
    /// No persisted session; the composer targets an unpersisted draft.
    Draft,
    /// Open in flight; identity is known but session-scoped work must not dispatch yet.
    Opening {
        session_id: SessionId,
        anchor_sequence: Option<u64>,
    },
    /// Attached with a live event stream.
    Attached { session_id: SessionId },
    /// Identity remains valid while the event stream reconnects.
    ///
    /// Commands still dispatch in this state: they travel over independent client requests rather
    /// than the event stream, so only the view is stale.
    Detached { session_id: SessionId },
}

impl ChatSessionAttachment {
    /// Return the session this view represents, including while opening or reconnecting.
    #[must_use]
    pub const fn viewing_session_id(&self) -> Option<SessionId> {
        match self {
            Self::Draft => None,
            Self::Opening { session_id, .. }
            | Self::Attached { session_id }
            | Self::Detached { session_id } => Some(*session_id),
        }
    }

    /// Return the session identity only when session-scoped work may dispatch.
    #[must_use]
    pub const fn attached_session_id(&self) -> Option<SessionId> {
        match self {
            Self::Draft | Self::Opening { .. } => None,
            Self::Attached { session_id } | Self::Detached { session_id } => Some(*session_id),
        }
    }

    /// Return the session currently being opened, if any.
    #[must_use]
    pub const fn opening_session_id(&self) -> Option<SessionId> {
        match self {
            Self::Opening { session_id, .. } => Some(*session_id),
            Self::Draft | Self::Attached { .. } | Self::Detached { .. } => None,
        }
    }

    /// Return the pending transcript anchor requested for an in-flight open.
    #[must_use]
    pub const fn opening_anchor_sequence(&self) -> Option<u64> {
        match self {
            Self::Opening {
                anchor_sequence, ..
            } => *anchor_sequence,
            Self::Draft | Self::Attached { .. } | Self::Detached { .. } => None,
        }
    }
}

/// Active chat session state shared by TUI flows.
pub struct ActiveChat {
    pub app: BmuxApp,
    pub agents: AgentCatalog,
    pub attachment: ChatSessionAttachment,
    pub event_sender: mpsc::Sender<history_flow::SessionStreamUpdate>,
    pub event_receiver: mpsc::Receiver<history_flow::SessionStreamUpdate>,
    pub event_task: Option<JoinHandle<()>>,
    pub opening_session_progress: Option<bcode_session_models::SessionOpenOperationSnapshot>,
    pub pending_effects: super::effects::TuiEffectQueue,
}

impl ActiveChat {
    /// Return the session this view represents, including while opening or reconnecting.
    #[must_use]
    pub const fn viewing_session_id(&self) -> Option<SessionId> {
        self.attachment.viewing_session_id()
    }

    /// Return the session identity only when session-scoped work may dispatch.
    #[must_use]
    pub const fn attached_session_id(&self) -> Option<SessionId> {
        self.attachment.attached_session_id()
    }

    /// Return the session currently being opened, if any.
    #[must_use]
    pub const fn opening_session_id(&self) -> Option<SessionId> {
        self.attachment.opening_session_id()
    }

    /// Record that the session view is attached with a live event stream.
    pub const fn mark_attached(&mut self, session_id: SessionId) {
        self.attachment = ChatSessionAttachment::Attached { session_id };
    }

    /// Record that the event stream was lost while identity remains valid.
    pub const fn mark_stream_detached(&mut self, session_id: SessionId) {
        self.attachment = ChatSessionAttachment::Detached { session_id };
    }

    #[cfg(test)]
    pub fn queued_effect_count(&self) -> usize {
        self.pending_effects.queued_effect_count()
    }

    /// Append a renderer-neutral durable presentation note when a session is active.
    ///
    /// Without an active canonical session, this falls back to an explicitly ephemeral
    /// notice owned by the current TUI presentation context.
    pub fn append_durable_presentation_note(
        &mut self,
        source_id: impl Into<String>,
        text: String,
        format: bcode_command::CommandTextFormat,
    ) {
        let source_id = source_id.into();
        if let Some(session_id) = self.attached_session_id() {
            self.pending_effects
                .start_ordered(super::effects::TuiEffect::AppendPresentationNote {
                    session_id,
                    source_id,
                    note_id: format!(
                        "{:020}-{}",
                        PRESENTATION_NOTE_SEQUENCE.fetch_add(1, Ordering::Relaxed),
                        uuid::Uuid::new_v4()
                    ),
                    text,
                    format,
                });
            return;
        }
        match format {
            bcode_command::CommandTextFormat::PlainText => {
                self.app.push_ephemeral_system_plain(text);
            }
            bcode_command::CommandTextFormat::Markdown => {
                self.app.push_ephemeral_system_markdown(text);
            }
            bcode_command::CommandTextFormat::Json => {
                self.app.push_ephemeral_system_json(text);
            }
        }
    }

    pub fn push_presentation_markdown(&mut self, source_id: impl Into<String>, text: String) {
        self.append_durable_presentation_note(
            source_id,
            text,
            bcode_command::CommandTextFormat::Markdown,
        );
    }

    /// Queue a background effect to start when the chat loop effect runner is available.
    pub fn start_effect(&mut self, effect: super::effects::TuiEffect) {
        self.pending_effects.start(effect);
    }

    /// Queue a background effect that should replace stale in-flight work with the same key.
    pub fn replace_effect(&mut self, effect: super::effects::TuiEffect) {
        self.pending_effects.replace(effect);
    }

    /// Queue the latest background effect to run after in-flight work with the same key.
    pub fn queue_latest_effect(&mut self, effect: super::effects::TuiEffect) {
        self.pending_effects.queue_latest(effect);
    }
}

/// TUI-side catalog of agent profile metadata.
#[derive(Debug, Clone, Default)]
pub struct AgentCatalog {
    agents: Vec<AgentInfo>,
    by_id: BTreeMap<String, AgentInfo>,
}

impl AgentCatalog {
    /// Load agent metadata from the daemon.
    ///
    /// # Errors
    ///
    /// Returns an error when the client cannot fetch agent profiles.
    pub async fn load(client: &BcodeClient) -> Result<Self, TuiError> {
        Ok(Self::from_agents(client.list_agents().await?))
    }

    /// Build a catalog from ordered agent metadata.
    #[must_use]
    pub fn from_agents(agents: Vec<AgentInfo>) -> Self {
        let by_id = agents
            .iter()
            .map(|agent| (agent.id.clone(), agent.clone()))
            .collect();
        Self { agents, by_id }
    }

    /// Apply an agent id plus any known metadata to app state.
    pub fn apply_agent_to_app(&self, app: &mut BmuxApp, agent_id: impl Into<String>) {
        let agent_id = agent_id.into();
        let accent = self
            .by_id
            .get(&agent_id)
            .and_then(|agent| agent.accent.clone());
        app.set_current_agent(agent_id, accent);
    }

    /// Apply metadata for the app's current agent id without changing the id.
    pub fn refresh_app_agent_metadata(&self, app: &mut BmuxApp) {
        let agent_id = app.current_agent_id().to_owned();
        self.apply_agent_to_app(app, agent_id);
    }

    /// Return true when the catalog has no agent profiles.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }

    /// Return the next agent after the current one in catalog order.
    #[must_use]
    pub fn next_agent(&self, current_agent_id: &str) -> Option<&AgentInfo> {
        next_agent(&self.agents, current_agent_id)
    }
}

#[must_use]
pub fn next_agent<'a>(agents: &'a [AgentInfo], current_agent_id: &str) -> Option<&'a AgentInfo> {
    if agents.is_empty() {
        return None;
    }
    if let Some(index) = agents.iter().position(|agent| agent.id == current_agent_id) {
        return agents.get((index + 1) % agents.len());
    }
    agents
        .iter()
        .find(|agent| agent.is_default)
        .or_else(|| agents.first())
}

/// Compute the semantic initial transcript-window request from the visible transcript area.
#[must_use]
pub fn initial_transcript_window_request(
    transcript_area: Rect,
) -> bcode_session_models::ProjectionWindowRequest {
    history_flow::initial_transcript_window_request(transcript_area)
}

/// Start asynchronously opening a session without blocking the chat input loop.
pub fn start_switch_session(
    chat: &mut ActiveChat,
    next_session_id: SessionId,
    initial_window_request: bcode_session_models::ProjectionWindowRequest,
) {
    start_switch_session_at_sequence(chat, next_session_id, initial_window_request, None);
}

/// Start asynchronously opening the canonical transcript window around a hydrated search hit.
#[cfg_attr(not(test), allow(dead_code))]
pub fn start_switch_session_from_search_hit(
    chat: &mut ActiveChat,
    hydrated: &bcode_session_search::HydratedSessionSearchHit,
    mut initial_window_request: bcode_session_models::ProjectionWindowRequest,
) -> Result<(), super::session_search_effect::SessionSearchNavigationUnavailable> {
    let target = super::session_search_effect::canonical_navigation_target(hydrated)?;
    initial_window_request.anchor =
        bcode_session_models::ProjectionWindowAnchor::AroundSequence(target.sequence);
    start_switch_session_at_sequence(
        chat,
        target.session_id,
        initial_window_request,
        Some(target.sequence),
    );
    Ok(())
}

fn start_switch_session_at_sequence(
    chat: &mut ActiveChat,
    next_session_id: SessionId,
    initial_window_request: bcode_session_models::ProjectionWindowRequest,
    anchor_sequence: Option<u64>,
) {
    if let Some(event_task) = chat.event_task.take() {
        event_task.abort();
    }
    while chat.event_receiver.try_recv().is_ok() {}
    let draft_text = chat.app.composer().text().to_owned();
    chat.attachment = ChatSessionAttachment::Opening {
        session_id: next_session_id,
        anchor_sequence,
    };
    chat.opening_session_progress = None;
    let previous_app = std::mem::replace(
        &mut chat.app,
        BmuxApp::new_with_history(Some(next_session_id), &[], &[], false),
    );
    chat.app.take_cross_session_state_from(&previous_app);
    // Reopening the session that is already attached is an in-process reconstruction, so its
    // process-local notices must survive this intermediate app as well as the final one.
    chat.app
        .take_same_session_transcript_state_from(&previous_app);
    chat.app
        .take_same_session_reasoning_state_from(&previous_app);
    chat.agents.refresh_app_agent_metadata(&mut chat.app);
    if !draft_text.is_empty() {
        chat.app.replace_composer_with(&draft_text);
    }
    chat.app.set_status("Opening session…".to_owned());
    chat.replace_effect(super::effects::TuiEffect::OpenSession {
        session_id: next_session_id,
        initial_window_request,
        event_sender: chat.event_sender.clone(),
        allow_daemon_start: true,
    });
}

/// Apply a completed asynchronous session-open result.
pub fn complete_switch_session(
    chat: &mut ActiveChat,
    session_id: SessionId,
    has_older_history: bool,
    result: Result<(AttachedSessionHistory, JoinHandle<()>), TuiError>,
) {
    if chat.opening_session_id() != Some(session_id) {
        if let Ok((_, event_task)) = result {
            event_task.abort();
        }
        return;
    }
    chat.opening_session_progress = None;
    let anchor_sequence = chat.attachment.opening_anchor_sequence();
    match result {
        Ok((attached, next_task)) => {
            let draft_text = chat.app.composer().text().to_owned();
            chat.event_task = Some(next_task);
            chat.mark_attached(session_id);
            let previous_app = std::mem::replace(
                &mut chat.app,
                BmuxApp::new_with_history(
                    Some(session_id),
                    &attached.history,
                    &attached.input_history,
                    has_older_history,
                ),
            );
            chat.app.take_cross_session_state_from(&previous_app);
            chat.app
                .take_same_session_transcript_state_from(&previous_app);
            chat.app
                .take_same_session_reasoning_state_from(&previous_app);
            chat.agents.refresh_app_agent_metadata(&mut chat.app);
            if !draft_text.is_empty() {
                chat.app.replace_composer_with(&draft_text);
            } else if let Some(draft) = attached.draft {
                chat.app.replace_composer_with(&draft);
            }
            chat.app.apply_session_summary(&attached.session);
            chat.app.apply_usage_summary(&attached.usage_summary);
            chat.app
                .apply_runtime_selection(attached.runtime_selection.clone());
            chat.app
                .set_status("session writable and attached".to_owned());
            if let Some(sequence) = anchor_sequence {
                if chat.app.transcript_index_for_sequence(sequence).is_some() {
                    chat.app.request_transcript_top_anchor_sequence(sequence);
                    chat.app.set_status("jumped to search result".to_owned());
                } else {
                    chat.app.set_status(format!(
                        "search result seq {sequence} was not in the loaded canonical window"
                    ));
                }
            }
            chat.replace_effect(super::effects::TuiEffect::LoadSessionStatus { session_id });
            chat.start_effect(super::effects::TuiEffect::ListPermissions);
        }
        Err(error) => {
            // A failed open must not leave the view holding an identity it cannot serve. Returning
            // to Draft keeps attachment state and session-scoped dispatch consistent.
            chat.attachment = ChatSessionAttachment::Draft;
            chat.app.set_status(format!("session open failed: {error}"));
            chat.app
                .push_ephemeral_system_plain(format!("session open failed: {error}"));
        }
    }
}

pub fn auth_security_status(config: &bcode_config::BcodeConfig) -> Option<String> {
    let selection = config.resolved_model_selection();
    let auth_profile_name = std::env::var(bcode_config::BCODE_AUTH_PROFILE_ENV)
        .ok()
        .filter(|profile| !profile.trim().is_empty())
        .or(selection.auth_profile)?;
    let auth_profile = config.auth.profiles.get(&auth_profile_name)?;
    if auth_profile.backend != "sshenv" {
        return None;
    }
    let vault = auth_profile.settings.get("vault").map_or_else(
        bcode_config::default_auth_vault_path,
        std::path::PathBuf::from,
    );
    let profile = auth_profile
        .settings
        .get("profile")
        .map_or(auth_profile_name.as_str(), String::as_str);
    let options = bcode_provider_auth::security::device_seal_options_for_auth_profile(auth_profile);
    let report = bcode_provider_auth::security::reconcile_auth_vault_security_report_with_options(
        &vault,
        profile,
        options,
        auth_profile
            .settings
            .get("recipient_key")
            .map(String::as_str),
    );
    report
        .diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic.severity
                == bcode_provider_auth::security::AuthSecurityDiagnosticSeverity::Error
        })
        .or_else(|| {
            report.diagnostics.iter().find(|diagnostic| {
                diagnostic.severity
                    == bcode_provider_auth::security::AuthSecurityDiagnosticSeverity::Warning
            })
        })
        .map(|diagnostic| format!("⚠ {} Run `bcode auth status`.", diagnostic.message))
}
/// Reset the active chat to an unpersisted draft session.
pub fn switch_to_draft_session(chat: &mut ActiveChat) {
    if let Some(event_task) = chat.event_task.take() {
        event_task.abort();
    }
    while chat.event_receiver.try_recv().is_ok() {}
    chat.attachment = ChatSessionAttachment::Draft;
    chat.opening_session_progress = None;
    let current_agent_id = chat.app.current_agent_id().to_owned();
    let previous_app = std::mem::replace(
        &mut chat.app,
        BmuxApp::new_with_history(None, &[], &[], false),
    );
    chat.app.take_cross_session_state_from(&previous_app);
    chat.app.clear_pending_reasoning_effort_for_session_change();
    chat.agents
        .apply_agent_to_app(&mut chat.app, current_agent_id);
    chat.app.set_status("New draft".to_owned());
}
