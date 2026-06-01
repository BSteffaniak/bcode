//! Session picker event flow for the TUI.

use std::io::Write;

use bcode_client::{AttachedSessionHistory, BcodeClient, SessionList};
use bcode_ipc::{
    Event as BcodeEvent, RuntimeWorkSnapshot, SessionCatalogSourceStatus, SessionCatalogStatus,
};
use bcode_session_models::SessionId;
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::event::{Event, FocusEvent};
use bmux_tui::geometry::Rect;
use bmux_tui::terminal::Terminal;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::app::BmuxApp;
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
    pub session_id: Option<SessionId>,
    pub event_sender: mpsc::UnboundedSender<BcodeEvent>,
    pub event_receiver: mpsc::UnboundedReceiver<BcodeEvent>,
    pub event_task: Option<JoinHandle<()>>,
    pub async_event_sender: mpsc::UnboundedSender<ChatAsyncEvent>,
    pub async_event_receiver: mpsc::UnboundedReceiver<ChatAsyncEvent>,
    pub session_open_task: Option<JoinHandle<()>>,
    pub status_hydration_task: Option<JoinHandle<()>>,
    pub opening_session_id: Option<SessionId>,
}

/// Async TUI work completion event.
pub enum ChatAsyncEvent {
    SessionOpened(SessionOpenResult),
    StatusHydrated(StatusHydrationResult),
}

/// Result from asynchronously opening a session.
pub struct SessionOpenResult {
    pub session_id: SessionId,
    pub has_older_history: bool,
    pub result: Result<(AttachedSessionHistory, JoinHandle<()>), TuiError>,
}

/// Result from asynchronously hydrating non-critical session status.
pub struct StatusHydrationResult {
    pub session_id: SessionId,
    pub model: Option<bcode_ipc::SessionModelStatus>,
    pub active_skill_count: Option<usize>,
    pub runtime_work: Option<Vec<RuntimeWorkSnapshot>>,
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
    client: &BcodeClient,
    chat: &mut ActiveChat,
    next_session_id: SessionId,
    initial_window_request: bcode_session_models::ProjectionWindowRequest,
) {
    if let Some(open_task) = chat.session_open_task.take() {
        open_task.abort();
    }
    if let Some(hydration_task) = chat.status_hydration_task.take() {
        hydration_task.abort();
    }
    if let Some(event_task) = chat.event_task.take() {
        event_task.abort();
    }
    while chat.event_receiver.try_recv().is_ok() {}
    let tui_config = chat.app.tui_config().clone();
    let draft_text = chat.app.composer().text().to_owned();
    chat.opening_session_id = Some(next_session_id);
    chat.session_id = None;
    chat.app = BmuxApp::new_with_history(Some(next_session_id), &[], &[], false);
    chat.app.apply_tui_config(tui_config);
    if !draft_text.is_empty() {
        chat.app.replace_composer_with(&draft_text);
    }
    chat.app.set_status("Opening session…".to_owned());
    let client = client.clone();
    let event_sender = chat.event_sender.clone();
    let async_event_sender = chat.async_event_sender.clone();
    chat.session_open_task = Some(tokio::spawn(async move {
        let result = history_flow::attach_session_event_stream_with_window_request(
            &client,
            next_session_id,
            event_sender,
            initial_window_request,
        )
        .await;
        let _ = async_event_sender.send(ChatAsyncEvent::SessionOpened(SessionOpenResult {
            session_id: next_session_id,
            has_older_history: true,
            result,
        }));
    }));
}

/// Apply a completed asynchronous session-open result.
pub fn complete_switch_session(
    client: &BcodeClient,
    chat: &mut ActiveChat,
    opened: SessionOpenResult,
) {
    if chat.opening_session_id != Some(opened.session_id) {
        if let Ok((_, event_task)) = opened.result {
            event_task.abort();
        }
        return;
    }
    chat.opening_session_id = None;
    match opened.result {
        Ok((attached, next_task)) => {
            let draft_text = chat.app.composer().text().to_owned();
            chat.event_task = Some(next_task);
            chat.session_id = Some(opened.session_id);
            let tui_config = chat.app.tui_config().clone();
            let has_older_history = opened.has_older_history;
            chat.app = BmuxApp::new_with_history(
                Some(opened.session_id),
                &attached.history,
                &attached.input_history,
                has_older_history,
            );
            chat.app.apply_tui_config(tui_config);
            if !draft_text.is_empty() {
                chat.app.replace_composer_with(&draft_text);
            }
            chat.app.apply_session_summary(&attached.session);
            chat.app.set_status("session opened".to_owned());
            start_status_hydration(client, chat, opened.session_id);
        }
        Err(error) => {
            chat.app.set_status(format!("session open failed: {error}"));
            chat.app
                .push_system_note(format!("session open failed: {error}"));
        }
    }
}

/// Start non-critical session status hydration in the background.
pub fn start_status_hydration(client: &BcodeClient, chat: &mut ActiveChat, session_id: SessionId) {
    if let Some(hydration_task) = chat.status_hydration_task.take() {
        hydration_task.abort();
    }
    let client = client.clone();
    let async_event_sender = chat.async_event_sender.clone();
    chat.status_hydration_task = Some(tokio::spawn(async move {
        let model = client.session_model_status(session_id).await.ok();
        let (active_skill_count, runtime_work) = tokio::join!(
            async {
                client
                    .active_skills(session_id)
                    .await
                    .ok()
                    .map(|skills| skills.len())
            },
            async { client.list_runtime_work(session_id).await.ok() },
        );
        let _ = async_event_sender.send(ChatAsyncEvent::StatusHydrated(StatusHydrationResult {
            session_id,
            model,
            active_skill_count,
            runtime_work,
        }));
    }));
}

/// Apply completed non-critical session status hydration.
pub fn complete_status_hydration(chat: &mut ActiveChat, hydrated: StatusHydrationResult) {
    if chat.session_id != Some(hydrated.session_id) {
        return;
    }
    let model_text = hydrated.model.as_ref().map_or_else(
        || "model unknown".to_owned(),
        |status| {
            let provider = status.provider_plugin_id.as_deref().unwrap_or("auto");
            let model = status.model_id.as_deref().unwrap_or("default");
            format!("{provider}/{model}")
        },
    );
    if let Some(model) = hydrated.model {
        chat.app.apply_model_status(model);
    }
    if let Some(work) = hydrated.runtime_work {
        chat.app.apply_runtime_work_snapshots(&work);
    }
    let skill_count = hydrated.active_skill_count.unwrap_or(0);
    chat.app
        .set_status(format!("model: {model_text}; active skills: {skill_count}"));
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
    client: &BcodeClient,
    chat: &mut ActiveChat,
    next_session_id: SessionId,
) -> Result<(), TuiError> {
    chat.app.set_status("Opening session…".to_owned());
    terminal.draw(|frame| super::render::render(&mut chat.app, frame))?;
    start_switch_session(
        client,
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
    if let Some(open_task) = chat.session_open_task.take() {
        open_task.abort();
    }
    if let Some(hydration_task) = chat.status_hydration_task.take() {
        hydration_task.abort();
    }
    while chat.event_receiver.try_recv().is_ok() {}
    chat.opening_session_id = None;
    chat.session_id = None;
    let tui_config = chat.app.tui_config().clone();
    let current_agent_id = chat.app.current_agent_id().to_owned();
    chat.app = BmuxApp::new_with_history(None, &[], &[], false);
    chat.app.apply_tui_config(tui_config);
    chat.app.set_current_agent_id(current_agent_id);
    chat.app
        .set_status("New draft session; send a message to save it".to_owned());
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
    let session = client.create_session(None).await?;
    if draft_agent_id != "build" {
        client.set_session_agent(session.id, draft_agent_id).await?;
    }
    let (attached, event_task) =
        history_flow::attach_session_event_stream(client, session.id, chat.event_sender.clone())
            .await?;
    chat.session_id = Some(session.id);
    chat.event_task = Some(event_task);
    chat.app.apply_session_summary(&attached.session);
    hydrate_status(client, &mut chat.app).await;
    Ok(session.id)
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
) -> Result<(), TuiError> {
    terminal.resize(helpers::terminal_area()?);
    terminal.draw(|frame| session_picker_render::render_picker(picker, frame))?;
    Ok(())
}

async fn import_selected_session<W: Write>(
    terminal: &mut Terminal<&mut W>,
    client: &BcodeClient,
    picker: &mut session_picker::SessionPickerApp,
) -> Result<Option<SessionId>, TuiError> {
    let selected_import = picker
        .selected_import()
        .filter(|import| import.imported_at_ms == 0)
        .cloned();
    if let Some(import) = selected_import {
        picker.set_status(format!("Importing [{}] session...", import.source_id));
        terminal.draw(|frame| {
            session_picker_render::render_picker(picker, frame);
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
            session_picker_render::render_picker(picker, frame);
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
    draw_session_picker(io.terminal, &mut picker)?;
    let mut watcher = services.client.watch_session_catalog().await?;
    picker.set_loading_status(
        "Loading sessions: discovering sources; press Ctrl-N to create one".to_owned(),
    );
    draw_session_picker(io.terminal, &mut picker)?;
    apply_session_list(&mut picker, watcher.initial_snapshot().await?);
    loop {
        draw_session_picker(io.terminal, &mut picker)?;
        let event = tokio::select! {
            snapshot = watcher.next_snapshot() => {
                apply_session_list(&mut picker, snapshot?);
                continue;
            }
            event = io.input.recv() => {
                event?
            }
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
                    rename_picker_session(services.client, &mut picker).await?;
                }
                PickerKeyOutcome::Delete => {
                    delete_picker_session(services.client, &mut picker).await?;
                }
                PickerKeyOutcome::Selected => {
                    if let Some(session_id) =
                        import_selected_session(io.terminal, services.client, &mut picker).await?
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
                    && let Some(session_id) =
                        import_selected_session(io.terminal, services.client, &mut picker).await?
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
    draw_session_picker(io.terminal, &mut picker)?;
    let mut watcher = services.client.watch_session_catalog().await?;
    picker.set_loading_status("Loading sessions: discovering sources".to_owned());
    draw_session_picker(io.terminal, &mut picker)?;
    apply_session_list(&mut picker, watcher.initial_snapshot().await?);
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
        draw_session_picker(io.terminal, &mut picker)?;
        let event = tokio::select! {
            snapshot = watcher.next_snapshot() => {
                apply_session_list(&mut picker, snapshot?);
                continue;
            }
            event = io.input.recv() => {
                event?
            }
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
                    rename_picker_session(services.client, &mut picker).await?;
                }
                PickerKeyOutcome::Delete => {
                    delete_picker_session(services.client, &mut picker).await?;
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
            BmuxAction::InputSubmit
            | BmuxAction::InputHistoryPrevious
            | BmuxAction::InputHistoryNext
            | BmuxAction::AppExit
            | BmuxAction::AppInterrupt
            | BmuxAction::ClipboardPasteImage
            | BmuxAction::CommandPaletteOpen
            | BmuxAction::AgentCycle
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
        Ok(_) => {
            picker.replace_sessions(client.list_sessions().await?);
            picker.finish_mutation("Session renamed".to_owned());
        }
        Err(error) => picker.finish_mutation(format!("rename failed: {error}")),
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
        Ok(_) => {
            picker.replace_sessions(client.list_sessions().await?);
            picker.finish_mutation("Session deleted".to_owned());
        }
        Err(error) => picker.finish_mutation(format!("delete failed: {error}")),
    }
    Ok(())
}
