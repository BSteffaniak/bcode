//! Session picker event flow for the TUI.

use std::collections::BTreeMap;
use std::io::Write;

use bcode_agent_profile::AgentInfo;
use bcode_client::{AttachedSessionHistory, BcodeClient, SessionCatalogWatcher, SessionList};
use bcode_ipc::{Event as BcodeEvent, SessionCatalogSourceStatus, SessionCatalogStatus};
use bcode_session_models::SessionId;
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::event::{Event, FocusEvent};
use bmux_tui::geometry::Rect;
use bmux_tui::terminal::Terminal;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::app::BmuxApp;
use super::daemon_issue;
use super::helpers;
use super::keymap::{BmuxAction, BmuxKeyMap, BmuxScope};
use super::picker_mouse::picker_row_from_mouse;
use super::runtime_context::{TuiIo, TuiServices};
use super::text_input_flow;
use super::{TuiError, history_flow};
use super::{session_picker, session_picker_render};

/// Active chat session state shared by TUI flows.
pub struct ActiveChat {
    pub app: BmuxApp,
    pub agents: AgentCatalog,
    pub session_id: Option<SessionId>,
    pub event_sender: mpsc::UnboundedSender<BcodeEvent>,
    pub event_receiver: mpsc::UnboundedReceiver<BcodeEvent>,
    pub event_task: Option<JoinHandle<()>>,
    pub opening_session_id: Option<SessionId>,
    pub pending_effects: super::effects::TuiEffectQueue,
}

impl ActiveChat {
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
    if let Some(event_task) = chat.event_task.take() {
        event_task.abort();
    }
    while chat.event_receiver.try_recv().is_ok() {}
    let tui_config = chat.app.tui_config().clone();
    let agent_metadata_hydrated = chat.app.is_agent_metadata_hydrated();
    let draft_text = chat.app.composer().text().to_owned();
    chat.opening_session_id = Some(next_session_id);
    chat.session_id = None;
    let previous_app = std::mem::replace(
        &mut chat.app,
        BmuxApp::new_with_history(Some(next_session_id), &[], &[], false),
    );
    chat.app.apply_tui_config(tui_config);
    chat.app
        .set_agent_metadata_hydrated(agent_metadata_hydrated);
    chat.app.take_theme_transition_state_from(&previous_app);
    chat.agents.refresh_app_agent_metadata(&mut chat.app);
    if !draft_text.is_empty() {
        chat.app.replace_composer_with(&draft_text);
    }
    chat.app.set_status("Opening session…".to_owned());
    chat.replace_effect(super::effects::TuiEffect::OpenSession {
        session_id: next_session_id,
        initial_window_request,
        event_sender: chat.event_sender.clone(),
    });
}

/// Apply a completed asynchronous session-open result.
pub fn complete_switch_session(
    chat: &mut ActiveChat,
    session_id: SessionId,
    has_older_history: bool,
    result: Result<(AttachedSessionHistory, JoinHandle<()>), TuiError>,
) {
    if chat.opening_session_id != Some(session_id) {
        if let Ok((_, event_task)) = result {
            event_task.abort();
        }
        return;
    }
    chat.opening_session_id = None;
    match result {
        Ok((attached, next_task)) => {
            let draft_text = chat.app.composer().text().to_owned();
            chat.event_task = Some(next_task);
            chat.session_id = Some(session_id);
            let tui_config = chat.app.tui_config().clone();
            let agent_metadata_hydrated = chat.app.is_agent_metadata_hydrated();
            let previous_app = std::mem::replace(
                &mut chat.app,
                BmuxApp::new_with_history(
                    Some(session_id),
                    &attached.history,
                    &attached.input_history,
                    has_older_history,
                ),
            );
            chat.app.apply_tui_config(tui_config);
            chat.app
                .set_agent_metadata_hydrated(agent_metadata_hydrated);
            chat.app.take_theme_transition_state_from(&previous_app);
            chat.agents.refresh_app_agent_metadata(&mut chat.app);
            if !draft_text.is_empty() {
                chat.app.replace_composer_with(&draft_text);
            } else if let Some(draft) = attached.draft {
                chat.app.replace_composer_with(&draft);
            }
            chat.app.apply_session_summary(&attached.session);
            chat.app.set_status("session opened".to_owned());
            chat.replace_effect(super::effects::TuiEffect::LoadSessionStatus { session_id });
        }
        Err(error) => {
            chat.app.set_status(format!("session open failed: {error}"));
            chat.app
                .push_system_note(format!("session open failed: {error}"));
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
    let policy = bcode_provider_auth::security::device_seal_policy_for_auth_profile(auth_profile);
    let report = bcode_provider_auth::security::reconcile_auth_vault_security_report(
        &vault,
        profile,
        policy,
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

/// Hydrate model and skill status for the active session.
pub async fn hydrate_status(client: &BcodeClient, app: &mut BmuxApp) {
    let Some(session_id) = app.session_id() else {
        return;
    };
    let model = client.session_model_status(session_id).await.ok();
    let active_skills = client.active_skills(session_id).await.ok();
    let model_text = model.as_ref().map_or_else(
        || "model unknown".to_owned(),
        |status| {
            let provider = status.provider_plugin_id.as_deref().unwrap_or("auto");
            let model = status.model_id.as_deref().unwrap_or("default");
            format!("{provider}/{model}")
        },
    );
    if let Some(model) = model {
        app.apply_model_status(model);
    }
    if let Ok(work) = client.list_runtime_work(session_id).await {
        app.apply_runtime_work_snapshots(&work);
    }
    let skill_count = active_skills.as_ref().map_or(0, Vec::len);
    app.set_status(format!("model: {model_text}; active skills: {skill_count}"));
}

/// Switch the active chat to another session.
pub fn switch_session<W: Write>(
    terminal: &mut Terminal<&mut W>,
    chat: &mut ActiveChat,
    next_session_id: SessionId,
) -> Result<(), TuiError> {
    chat.app.set_status("Opening session…".to_owned());
    terminal.draw(|frame| super::render::render(&mut chat.app, frame))?;
    start_switch_session(
        chat,
        next_session_id,
        initial_transcript_window_request(super::render::transcript_area_for_frame(
            &chat.app,
            terminal.area(),
        )),
    );
    Ok(())
}

/// Reset the active chat to an unpersisted draft session.
pub fn switch_to_draft_session(chat: &mut ActiveChat) {
    if let Some(event_task) = chat.event_task.take() {
        event_task.abort();
    }
    while chat.event_receiver.try_recv().is_ok() {}
    chat.opening_session_id = None;
    chat.session_id = None;
    let tui_config = chat.app.tui_config().clone();
    let agent_metadata_hydrated = chat.app.is_agent_metadata_hydrated();
    let current_agent_id = chat.app.current_agent_id().to_owned();
    let previous_app = std::mem::replace(
        &mut chat.app,
        BmuxApp::new_with_history(None, &[], &[], false),
    );
    chat.app.apply_tui_config(tui_config);
    chat.app
        .set_agent_metadata_hydrated(agent_metadata_hydrated);
    chat.app.take_theme_transition_state_from(&previous_app);
    chat.agents
        .apply_agent_to_app(&mut chat.app, current_agent_id);
    chat.app.set_status("New draft".to_owned());
}

/// Create and attach a persisted session for the active draft chat.
pub async fn persist_draft_session<W: Write>(
    terminal: &mut Terminal<&mut W>,
    client: &BcodeClient,
    chat: &mut ActiveChat,
) -> Result<SessionId, TuiError> {
    if let Some(session_id) = chat.session_id {
        return Ok(session_id);
    }
    chat.app.set_status("Creating session…".to_owned());
    terminal.draw(|frame| super::render::render(&mut chat.app, frame))?;
    let draft_agent_id = chat.app.current_agent_id().to_owned();
    let session = match client.create_session(None).await {
        Ok(session) => session,
        Err(error) => {
            helpers::report_client_issue(&mut chat.app, "session creation unavailable", &error);
            return Err(error.into());
        }
    };
    let _ = client
        .set_composer_draft(
            bcode_ipc::ComposerDraftScope::DraftSession {
                launch_working_directory: std::env::current_dir()?,
            },
            String::new(),
        )
        .await;
    if draft_agent_id != "build" {
        if let Err(error) = client
            .set_session_agent(session.id, draft_agent_id.clone())
            .await
        {
            helpers::report_client_issue(&mut chat.app, "session agent unavailable", &error);
            return Err(error.into());
        }
        chat.agents
            .apply_agent_to_app(&mut chat.app, draft_agent_id);
    }
    let (attached, event_task) = match history_flow::attach_session_event_stream(
        client,
        session.id,
        chat.event_sender.clone(),
    )
    .await
    {
        Ok(attached) => attached,
        Err(TuiError::Client(error)) => {
            helpers::report_client_issue(&mut chat.app, "session event stream unavailable", &error);
            return Err(error.into());
        }
        Err(error) => return Err(error),
    };
    chat.session_id = Some(session.id);
    chat.event_task = Some(event_task);
    chat.app.apply_session_summary(&attached.session);
    if let Err(error) = commit_draft_reasoning(client, &chat.app, session.id).await {
        chat.app
            .set_status(format!("thinking settings failed: {error}"));
    }
    hydrate_status(client, &mut chat.app).await;
    Ok(session.id)
}

async fn commit_draft_reasoning(
    client: &BcodeClient,
    app: &BmuxApp,
    session_id: SessionId,
) -> Result<(), TuiError> {
    let effort = app.reasoning_effort().map(ToOwned::to_owned);
    let summary = app.reasoning_summary().map(ToOwned::to_owned);
    client
        .set_session_reasoning(session_id, effort, summary)
        .await?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionPickerStartMode {
    /// Start in rename mode.
    Rename,
    /// Start in delete-confirmation mode.
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerKeyOutcome {
    Continue,
    Create,
    Rename,
    Delete,
    Selected,
    Canceled,
}

fn apply_session_list(picker: &mut session_picker::SessionPickerApp, session_list: SessionList) {
    let is_loading = catalog_still_loading(&session_list.catalog_status);
    let session_count = session_list.sessions.len();
    let status = catalog_status_text(&session_list, session_count);
    picker.replace_sessions(session_list.sessions);
    if is_loading {
        picker.set_loading_status(status);
    } else {
        picker.set_status(status);
        picker.set_idle_empty_message();
    }
}

fn catalog_status_text(session_list: &SessionList, session_count: usize) -> String {
    if session_list.catalog_sources.is_empty()
        && catalog_still_loading(&session_list.catalog_status)
    {
        return format!(
            "Loading sessions: discovering sources; {session_count} found so far; press Ctrl-N to create one"
        );
    }

    let loaded_sources = status_source_ids(&session_list.catalog_sources, |status| {
        matches!(status, SessionCatalogStatus::Loaded)
    });
    let loading_sources = status_source_ids(&session_list.catalog_sources, catalog_still_loading);
    let failed_sources = status_source_ids(&session_list.catalog_sources, |status| {
        matches!(status, SessionCatalogStatus::Failed(_))
    });
    let degraded_sources = status_source_ids(&session_list.catalog_sources, |status| {
        matches!(status, SessionCatalogStatus::Degraded(_))
    });

    if catalog_still_loading(&session_list.catalog_status) {
        let mut phases = Vec::new();
        if !loaded_sources.is_empty() {
            phases.push(format!("loaded {}", loaded_sources.join(", ")));
        }
        if loading_sources.is_empty() {
            phases.push("discovering sources".to_owned());
        } else {
            phases.push(format!("loading {}", loading_sources.join(", ")));
        }
        if !failed_sources.is_empty() {
            phases.push(format!("failed {}", failed_sources.join(", ")));
        }
        if !degraded_sources.is_empty() {
            phases.push(format!("needs repair {}", degraded_sources.join(", ")));
        }
        return format!(
            "Loading sessions: {}; {session_count} found so far; press Ctrl-N to create one",
            phases.join("; ")
        );
    }

    let mut phases = Vec::new();
    if !loaded_sources.is_empty() {
        phases.push(format!("loaded {}", loaded_sources.join(", ")));
    }
    if !failed_sources.is_empty() {
        phases.push(format!("failed {}", failed_sources.join(", ")));
    }
    if !degraded_sources.is_empty() {
        phases.push(format!("needs repair {}", degraded_sources.join(", ")));
    }
    if phases.is_empty() {
        format!("Select a session ({session_count} found) or press Ctrl-N to create one")
    } else {
        format!(
            "{}; {session_count} found; press Ctrl-N to create one",
            phases.join("; ")
        )
    }
}

fn status_source_ids(
    sources: &[SessionCatalogSourceStatus],
    matches_status: impl Fn(&SessionCatalogStatus) -> bool,
) -> Vec<&str> {
    sources
        .iter()
        .filter(|source| matches_status(&source.status))
        .map(|source| source.source_id.as_str())
        .collect()
}

const fn catalog_still_loading(status: &SessionCatalogStatus) -> bool {
    matches!(
        status,
        SessionCatalogStatus::NotStarted | SessionCatalogStatus::Loading
    )
}

fn draw_session_picker<W: Write>(
    terminal: &mut Terminal<&mut W>,
    picker: &mut session_picker::SessionPickerApp,
    theme: super::render::TuiTheme,
) -> Result<(), TuiError> {
    terminal.resize(helpers::terminal_area()?);
    terminal.draw(|frame| session_picker_render::render_picker(picker, frame, theme))?;
    Ok(())
}

async fn import_selected_session<W: Write>(
    terminal: &mut Terminal<&mut W>,
    client: &BcodeClient,
    picker: &mut session_picker::SessionPickerApp,
    theme: super::render::TuiTheme,
) -> Result<Option<SessionId>, TuiError> {
    let selected_import = picker
        .selected_import()
        .filter(|import| import.imported_at_ms == 0)
        .cloned();
    if let Some(import) = selected_import {
        picker.set_status(format!("Importing [{}] session...", import.source_id));
        terminal.draw(|frame| {
            session_picker_render::render_picker(picker, frame, theme);
        })?;
        match client
            .import_external_session(import.source_id.clone(), import.external_session_id)
            .await
        {
            Ok((session, warnings)) => {
                let status = if warnings.is_empty() {
                    format!("Imported [{}] session", import.source_id)
                } else {
                    format!(
                        "Imported [{}] with {} warnings; opening session",
                        import.source_id,
                        warnings.len()
                    )
                };
                picker.set_status(status);
                picker.set_last_import(Some((session.clone(), warnings)));
                Ok(Some(session.id))
            }
            Err(error) => {
                picker.set_status(format!("Import failed: {error}"));
                Ok(None)
            }
        }
    } else if let Some(session_id) = picker.selected_session_id() {
        picker.set_status("Opening session…".to_owned());
        terminal.draw(|frame| {
            session_picker_render::render_picker(picker, frame, theme);
        })?;
        Ok(Some(session_id))
    } else {
        picker.set_status("No session selected; press Ctrl-N to create one".to_owned());
        Ok(None)
    }
}

/// Session picker result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickSessionOutcome {
    /// An existing session was selected.
    Existing(SessionId),
    /// A new unpersisted draft session was requested.
    Draft,
}

async fn initialize_session_catalog_watcher(
    client: &BcodeClient,
    picker: &mut session_picker::SessionPickerApp,
    create_hint: &str,
) -> Option<SessionCatalogWatcher> {
    match client.watch_session_catalog().await {
        Ok(mut watcher) => {
            picker.set_loading_status(format!(
                "Loading sessions: discovering sources{create_hint}"
            ));
            match watcher.initial_snapshot().await {
                Ok(snapshot) => apply_session_list(picker, snapshot),
                Err(error) => {
                    set_picker_client_issue(picker, "session catalog unavailable", &error);
                    return None;
                }
            }
            Some(watcher)
        }
        Err(error) => {
            set_picker_client_issue(picker, "session catalog unavailable", &error);
            None
        }
    }
}

async fn apply_next_catalog_snapshot(
    watcher: &mut Option<SessionCatalogWatcher>,
    picker: &mut session_picker::SessionPickerApp,
) {
    let Some(active_watcher) = watcher else {
        return;
    };
    match active_watcher.next_snapshot().await {
        Ok(snapshot) => apply_session_list(picker, snapshot),
        Err(error) => {
            set_picker_client_issue(picker, "session catalog unavailable", &error);
            *watcher = None;
        }
    }
}

fn set_picker_client_issue(
    picker: &mut session_picker::SessionPickerApp,
    label: &str,
    error: &bcode_client::ClientError,
) {
    let issue = daemon_issue::classify_client_error(error);
    picker.set_status(issue.message(label).status);
    picker.set_idle_empty_message();
}

/// Pick an existing session or request a new draft.
#[allow(clippy::too_many_lines)]
pub async fn pick_session<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    services: &TuiServices<'_>,
) -> Result<PickSessionOutcome, TuiError> {
    let mut picker = session_picker::SessionPickerApp::new(Vec::new());
    picker.set_loading_status(
        "Loading sessions: connecting to catalog; press Ctrl-N to create one".to_owned(),
    );
    draw_session_picker(io.terminal, &mut picker, services.theme)?;
    let mut watcher = initialize_session_catalog_watcher(
        services.client,
        &mut picker,
        "; press Ctrl-N to create one",
    )
    .await;
    draw_session_picker(io.terminal, &mut picker, services.theme)?;
    loop {
        draw_session_picker(io.terminal, &mut picker, services.theme)?;
        let event = if watcher.is_some() {
            tokio::select! {
                () = apply_next_catalog_snapshot(&mut watcher, &mut picker) => {
                    continue;
                }
                event = io.input.recv() => {
                    event?
                }
            }
        } else {
            io.input.recv().await?
        };
        let Some(event) = event else {
            continue;
        };
        match event {
            Event::Resize(size) => io.terminal.resize(Rect::new(0, 0, size.width, size.height)),
            Event::Paste(text) => {
                let _ = text_input_flow::handle_paste(picker.filter_mut(), &text);
                picker.refresh_filter();
            }
            Event::Key(stroke) => match handle_picker_key(&mut picker, services.keymap, stroke) {
                PickerKeyOutcome::Continue => {}
                PickerKeyOutcome::Create => {
                    return Ok(PickSessionOutcome::Draft);
                }
                PickerKeyOutcome::Rename => {
                    if let Err(error) = rename_picker_session(services.client, &mut picker).await {
                        if let TuiError::Client(error) = error {
                            set_picker_client_issue(&mut picker, "session rename failed", &error);
                        } else {
                            return Err(error);
                        }
                    }
                }
                PickerKeyOutcome::Delete => {
                    if let Err(error) = delete_picker_session(services.client, &mut picker).await {
                        if let TuiError::Client(error) = error {
                            set_picker_client_issue(&mut picker, "session delete failed", &error);
                        } else {
                            return Err(error);
                        }
                    }
                }
                PickerKeyOutcome::Selected => {
                    if let Some(session_id) = import_selected_session(
                        io.terminal,
                        services.client,
                        &mut picker,
                        services.theme,
                    )
                    .await?
                    {
                        return Ok(PickSessionOutcome::Existing(session_id));
                    }
                }
                PickerKeyOutcome::Canceled => {
                    return Err(TuiError::Canceled);
                }
            },
            Event::Mouse(mouse) => {
                if let Some(row) = picker_row_from_mouse(mouse)
                    && picker.select_visible(row)
                    && let Some(session_id) = import_selected_session(
                        io.terminal,
                        services.client,
                        &mut picker,
                        services.theme,
                    )
                    .await?
                {
                    return Ok(PickSessionOutcome::Existing(session_id));
                }
            }
            Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick | Event::User(_) => {}
        }
    }
}

/// Pick a session to rename or delete.
pub async fn pick_session_for_mutation<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    services: &TuiServices<'_>,
    start_mode: SessionPickerStartMode,
) -> Result<(), TuiError> {
    let mut picker = session_picker::SessionPickerApp::new(Vec::new());
    picker.set_loading_status("Loading sessions: connecting to catalog".to_owned());
    draw_session_picker(io.terminal, &mut picker, services.theme)?;
    let mut watcher = initialize_session_catalog_watcher(services.client, &mut picker, "").await;
    draw_session_picker(io.terminal, &mut picker, services.theme)?;
    let mut pending_start_mode = Some(start_mode);
    loop {
        if let Some(start_mode) = pending_start_mode.take() {
            match start_mode {
                SessionPickerStartMode::Rename => {
                    picker.start_rename();
                }
                SessionPickerStartMode::Delete => {
                    picker.start_delete_confirmation();
                }
            }
        }
        draw_session_picker(io.terminal, &mut picker, services.theme)?;
        let event = if watcher.is_some() {
            tokio::select! {
                () = apply_next_catalog_snapshot(&mut watcher, &mut picker) => {
                    continue;
                }
                event = io.input.recv() => {
                    event?
                }
            }
        } else {
            io.input.recv().await?
        };
        let Some(event) = event else {
            continue;
        };
        match event {
            Event::Resize(size) => io.terminal.resize(Rect::new(0, 0, size.width, size.height)),
            Event::Paste(text) => match picker.mode() {
                session_picker::SessionPickerMode::Rename => {
                    let _ = text_input_flow::handle_paste(picker.rename_mut(), &text);
                }
                session_picker::SessionPickerMode::Filter
                | session_picker::SessionPickerMode::DeleteConfirm => {
                    let _ = text_input_flow::handle_paste(picker.filter_mut(), &text);
                    picker.refresh_filter();
                }
            },
            Event::Key(stroke) => match handle_picker_key(&mut picker, services.keymap, stroke) {
                PickerKeyOutcome::Continue
                | PickerKeyOutcome::Create
                | PickerKeyOutcome::Selected => {}
                PickerKeyOutcome::Rename => {
                    if let Err(error) = rename_picker_session(services.client, &mut picker).await {
                        if let TuiError::Client(error) = error {
                            set_picker_client_issue(&mut picker, "session rename failed", &error);
                        } else {
                            return Err(error);
                        }
                    }
                }
                PickerKeyOutcome::Delete => {
                    if let Err(error) = delete_picker_session(services.client, &mut picker).await {
                        if let TuiError::Client(error) = error {
                            set_picker_client_issue(&mut picker, "session delete failed", &error);
                        } else {
                            return Err(error);
                        }
                    }
                }
                PickerKeyOutcome::Canceled => {
                    return Ok(());
                }
            },
            Event::Mouse(mouse) => {
                if let Some(row) = picker_row_from_mouse(mouse) {
                    let _selected = picker.select_visible(row);
                }
            }
            Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick | Event::User(_) => {}
        }
        if matches!(picker.mode(), session_picker::SessionPickerMode::Filter) {
            return Ok(());
        }
    }
}

fn handle_picker_key(
    picker: &mut session_picker::SessionPickerApp,
    keymap: &BmuxKeyMap,
    stroke: KeyStroke,
) -> PickerKeyOutcome {
    match picker.mode() {
        session_picker::SessionPickerMode::Filter => {
            if picker.last_import().is_some() && stroke.key == KeyCode::Escape {
                picker.clear_last_import();
                return PickerKeyOutcome::Continue;
            }
            handle_picker_filter_key(picker, keymap, stroke)
        }
        session_picker::SessionPickerMode::Rename => {
            handle_picker_rename_key(picker, keymap, stroke)
        }
        session_picker::SessionPickerMode::DeleteConfirm => {
            handle_picker_delete_key(picker, stroke)
        }
    }
}

fn handle_picker_filter_key(
    picker: &mut session_picker::SessionPickerApp,
    keymap: &BmuxKeyMap,
    stroke: KeyStroke,
) -> PickerKeyOutcome {
    if let Some(action) = keymap.action_for_key(BmuxScope::SessionPicker, stroke) {
        return match action {
            BmuxAction::SelectCancel => PickerKeyOutcome::Canceled,
            BmuxAction::SessionNew => PickerKeyOutcome::Create,
            BmuxAction::SessionRename => {
                picker.start_rename();
                PickerKeyOutcome::Continue
            }
            BmuxAction::SessionDelete => {
                picker.start_delete_confirmation();
                PickerKeyOutcome::Continue
            }
            BmuxAction::SelectConfirm => PickerKeyOutcome::Selected,
            BmuxAction::SelectUp => {
                picker.select_previous();
                PickerKeyOutcome::Continue
            }
            BmuxAction::SelectDown => {
                picker.select_next();
                PickerKeyOutcome::Continue
            }
            BmuxAction::InputSubmitSteering
            | BmuxAction::InputSubmitFollowUp
            | BmuxAction::InputHistoryPrevious
            | BmuxAction::InputHistoryNext
            | BmuxAction::AppExit
            | BmuxAction::AppInterrupt
            | BmuxAction::ClipboardPasteImage
            | BmuxAction::CommandPaletteOpen
            | BmuxAction::AgentCycle
            | BmuxAction::ThinkingEffortCycle
            | BmuxAction::TranscriptPageUp
            | BmuxAction::TranscriptPageDown
            | BmuxAction::TranscriptTop
            | BmuxAction::TranscriptBottom
            | BmuxAction::TranscriptLineUp
            | BmuxAction::TranscriptLineDown
            | BmuxAction::PermissionApprove
            | BmuxAction::PermissionDeny
            | BmuxAction::InputNewLine
            | BmuxAction::EditorMoveLeft
            | BmuxAction::EditorMoveRight
            | BmuxAction::EditorMoveWordLeft
            | BmuxAction::EditorMoveWordRight
            | BmuxAction::EditorMoveStart
            | BmuxAction::EditorMoveEnd
            | BmuxAction::EditorSelectLeft
            | BmuxAction::EditorSelectRight
            | BmuxAction::EditorSelectWordLeft
            | BmuxAction::EditorSelectWordRight
            | BmuxAction::EditorSelectUp
            | BmuxAction::EditorSelectDown
            | BmuxAction::EditorDeleteBackward
            | BmuxAction::EditorDeleteForward
            | BmuxAction::EditorDeleteWordBackward
            | BmuxAction::EditorDeleteWordForward
            | BmuxAction::EditorDeleteToStart
            | BmuxAction::EditorDeleteToEnd
            | BmuxAction::SkillInvoke
            | BmuxAction::SkillActivate
            | BmuxAction::SkillDeactivate
            | BmuxAction::SkillHelp => PickerKeyOutcome::Continue,
        };
    }
    match stroke.key {
        KeyCode::Enter => PickerKeyOutcome::Selected,
        KeyCode::Up if stroke.modifiers.is_empty() => {
            picker.select_previous();
            PickerKeyOutcome::Continue
        }
        KeyCode::Down if stroke.modifiers.is_empty() => {
            picker.select_next();
            PickerKeyOutcome::Continue
        }
        _ => {
            if text_input_flow::handle_key(picker.filter_mut(), keymap, stroke)
                != bmux_tui_components::text_input::TextInputOutcome::Ignored
            {
                picker.refresh_filter();
            }
            PickerKeyOutcome::Continue
        }
    }
}

fn handle_picker_rename_key(
    picker: &mut session_picker::SessionPickerApp,
    keymap: &BmuxKeyMap,
    stroke: KeyStroke,
) -> PickerKeyOutcome {
    if stroke.key == KeyCode::Escape {
        picker.cancel_rename();
        return PickerKeyOutcome::Continue;
    }
    if stroke.key == KeyCode::Enter {
        return PickerKeyOutcome::Rename;
    }
    if text_input_flow::handle_key(picker.rename_mut(), keymap, stroke)
        == bmux_tui_components::text_input::TextInputOutcome::Submitted
    {
        PickerKeyOutcome::Rename
    } else {
        PickerKeyOutcome::Continue
    }
}

fn handle_picker_delete_key(
    picker: &mut session_picker::SessionPickerApp,
    stroke: KeyStroke,
) -> PickerKeyOutcome {
    match stroke.key {
        KeyCode::Escape | KeyCode::Char('n' | 'N') => {
            picker.cancel_delete();
            PickerKeyOutcome::Continue
        }
        KeyCode::Char('y' | 'Y') => PickerKeyOutcome::Delete,
        _ => PickerKeyOutcome::Continue,
    }
}

async fn rename_picker_session(
    client: &BcodeClient,
    picker: &mut session_picker::SessionPickerApp,
) -> Result<(), TuiError> {
    let Some(session_id) = picker.selected_session_id() else {
        picker.finish_mutation("No session selected to rename".to_owned());
        return Ok(());
    };
    let name = picker.rename().buffer().text().trim();
    let name = (!name.is_empty()).then(|| name.to_owned());
    match client.rename_session(session_id, name).await {
        Ok(_) => match client.list_sessions().await {
            Ok(sessions) => {
                picker.replace_sessions(sessions);
                picker.finish_mutation("Session renamed".to_owned());
            }
            Err(error) => set_picker_client_issue(picker, "session refresh failed", &error),
        },
        Err(error) => set_picker_client_issue(picker, "session rename failed", &error),
    }
    Ok(())
}

async fn delete_picker_session(
    client: &BcodeClient,
    picker: &mut session_picker::SessionPickerApp,
) -> Result<(), TuiError> {
    let Some(session_id) = picker.selected_session_id() else {
        picker.finish_mutation("No session selected to delete".to_owned());
        return Ok(());
    };
    match client.delete_session(session_id).await {
        Ok(_) => match client.list_sessions().await {
            Ok(sessions) => {
                picker.replace_sessions(sessions);
                picker.finish_mutation("Session deleted".to_owned());
            }
            Err(error) => set_picker_client_issue(picker, "session refresh failed", &error),
        },
        Err(error) => set_picker_client_issue(picker, "session delete failed", &error),
    }
    Ok(())
}
