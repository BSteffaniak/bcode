#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
// Small TUI state mutation helpers are clearer as regular functions even when
// clippy can technically const-qualify them.
#![allow(clippy::missing_const_for_fn)]

//! Terminal user interface for Bcode.

use bcode_client::{BcodeClient, ClientError};
use bcode_ipc::Event;
use bcode_session_models::{SessionEvent, SessionEventKind, SessionId, SessionSummary};
use crossterm::event::{
    self, Event as CrosstermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, List, ListItem, ListState, Paragraph, StatefulWidget, Wrap,
};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{self, Stdout};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::mpsc;

/// Errors returned by the TUI.
#[derive(Debug, Error)]
pub enum TuiError {
    #[error("client error: {0}")]
    Client(#[from] ClientError),
    #[error("config error: {0}")]
    Config(#[from] bcode_config::ConfigError),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("session selection canceled")]
    Canceled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum TuiAction {
    InputSubmit,
    InputNewLine,
    DeleteCharBackward,
    AppInterrupt,
    AppExit,
    AppClear,
    SearchStart,
    SearchNext,
    SearchPrevious,
    PermissionApprove,
    PermissionDeny,
    PermissionAlwaysAllow,
    PermissionAlwaysDeny,
    TranscriptPageUp,
    TranscriptPageDown,
    TranscriptTop,
    TranscriptBottom,
    TranscriptLineUp,
    TranscriptLineDown,
    SelectUp,
    SelectDown,
    SelectConfirm,
    SelectCancel,
}

impl TuiAction {
    fn from_id(id: &str) -> Option<Self> {
        Some(match id {
            "tui.input.submit" => Self::InputSubmit,
            "tui.input.newLine" => Self::InputNewLine,
            "tui.editor.deleteCharBackward" => Self::DeleteCharBackward,
            "app.interrupt" => Self::AppInterrupt,
            "app.exit" => Self::AppExit,
            "app.clear" => Self::AppClear,
            "app.search" => Self::SearchStart,
            "app.search.next" => Self::SearchNext,
            "app.search.previous" => Self::SearchPrevious,
            "app.permission.approve" => Self::PermissionApprove,
            "app.permission.deny" => Self::PermissionDeny,
            "app.permission.alwaysAllow" => Self::PermissionAlwaysAllow,
            "app.permission.alwaysDeny" => Self::PermissionAlwaysDeny,
            "transcript.pageUp" => Self::TranscriptPageUp,
            "transcript.pageDown" => Self::TranscriptPageDown,
            "transcript.top" => Self::TranscriptTop,
            "transcript.bottom" => Self::TranscriptBottom,
            "transcript.lineUp" => Self::TranscriptLineUp,
            "transcript.lineDown" => Self::TranscriptLineDown,
            "tui.select.up" | "tui.select.previous" => Self::SelectUp,
            "tui.select.down" | "tui.select.next" => Self::SelectDown,
            "tui.select.confirm" => Self::SelectConfirm,
            "tui.select.cancel" => Self::SelectCancel,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum TuiScope {
    Chat,
    Permission,
    SessionPicker,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct KeyBinding {
    code: KeyCode,
    modifiers: KeyModifiers,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct KeyMapEntry {
    key: String,
    binding: KeyBinding,
    action: TuiAction,
}

impl KeyBinding {
    fn matches(&self, key: &KeyEvent) -> bool {
        let (code, modifiers) = normalized_key(key);
        self.code == code && self.modifiers == modifiers
    }
}

#[derive(Debug, Clone)]
struct KeyMap {
    bindings: BTreeMap<TuiScope, Vec<KeyMapEntry>>,
    warnings: Vec<String>,
}

impl KeyMap {
    fn from_config(config: &bcode_config::TuiConfig) -> Self {
        let mut warnings = Vec::new();
        let mut bindings = default_keybindings();
        apply_legacy_keybindings(
            &mut bindings,
            &mut warnings,
            &config.keybindings.legacy_actions,
        );
        apply_scoped_keybindings(
            &mut bindings,
            &mut warnings,
            TuiScope::Chat,
            &config.keybindings.chat,
        );
        apply_scoped_keybindings(
            &mut bindings,
            &mut warnings,
            TuiScope::Permission,
            &config.keybindings.permission,
        );
        apply_scoped_keybindings(
            &mut bindings,
            &mut warnings,
            TuiScope::SessionPicker,
            &config.keybindings.session_picker,
        );
        let bindings = bindings
            .into_iter()
            .map(|(scope, bindings)| {
                (
                    scope,
                    compile_keybinding_scope(scope, bindings, &mut warnings),
                )
            })
            .collect();
        Self { bindings, warnings }
    }

    fn action_for_key(&self, scope: TuiScope, key: &KeyEvent) -> Option<TuiAction> {
        self.bindings.get(&scope).and_then(|bindings| {
            bindings
                .iter()
                .find(|entry| entry.binding.matches(key))
                .map(|entry| entry.action)
        })
    }

    fn primary(&self, scope: TuiScope, action: TuiAction) -> String {
        self.bindings
            .get(&scope)
            .and_then(|bindings| bindings.iter().find(|entry| entry.action == action))
            .map_or_else(|| "unbound".to_string(), |entry| entry.key.clone())
    }

    fn chat_hints(&self) -> String {
        format!(
            "{} send · {} interrupt · {} exit · {} search",
            self.primary(TuiScope::Chat, TuiAction::InputSubmit),
            self.primary(TuiScope::Chat, TuiAction::AppInterrupt),
            self.primary(TuiScope::Chat, TuiAction::AppExit),
            self.primary(TuiScope::Chat, TuiAction::SearchStart),
        )
    }

    fn permission_hints(&self) -> String {
        format!(
            "{} allow once · {} deny · {} always allow · {} always deny · {}/{} choose · {} confirm · {} cancel",
            self.primary(TuiScope::Permission, TuiAction::PermissionApprove),
            self.primary(TuiScope::Permission, TuiAction::PermissionDeny),
            self.primary(TuiScope::Permission, TuiAction::PermissionAlwaysAllow),
            self.primary(TuiScope::Permission, TuiAction::PermissionAlwaysDeny),
            self.primary(TuiScope::Permission, TuiAction::SelectUp),
            self.primary(TuiScope::Permission, TuiAction::SelectDown),
            self.primary(TuiScope::Permission, TuiAction::SelectConfirm),
            self.primary(TuiScope::Permission, TuiAction::SelectCancel),
        )
    }
}

fn default_keybindings() -> BTreeMap<TuiScope, BTreeMap<String, TuiAction>> {
    BTreeMap::from([
        (
            TuiScope::Chat,
            action_bindings(&[
                ("enter", TuiAction::InputSubmit),
                ("shift+enter", TuiAction::InputNewLine),
                ("backspace", TuiAction::DeleteCharBackward),
                ("escape", TuiAction::AppInterrupt),
                ("ctrl+d", TuiAction::AppExit),
                ("ctrl+c", TuiAction::AppClear),
                ("ctrl+f", TuiAction::SearchStart),
                ("ctrl+g", TuiAction::SearchNext),
                ("ctrl+r", TuiAction::SearchPrevious),
                ("pageUp", TuiAction::TranscriptPageUp),
                ("pageDown", TuiAction::TranscriptPageDown),
                ("home", TuiAction::TranscriptTop),
                ("end", TuiAction::TranscriptBottom),
                ("alt+up", TuiAction::TranscriptLineUp),
                ("alt+down", TuiAction::TranscriptLineDown),
            ]),
        ),
        (
            TuiScope::Permission,
            action_bindings(&[
                ("y", TuiAction::PermissionApprove),
                ("n", TuiAction::PermissionDeny),
                ("a", TuiAction::PermissionAlwaysAllow),
                ("d", TuiAction::PermissionAlwaysDeny),
                ("left", TuiAction::SelectUp),
                ("up", TuiAction::SelectUp),
                ("right", TuiAction::SelectDown),
                ("down", TuiAction::SelectDown),
                ("tab", TuiAction::SelectDown),
                ("enter", TuiAction::SelectConfirm),
                ("escape", TuiAction::SelectCancel),
                ("ctrl+c", TuiAction::SelectCancel),
            ]),
        ),
        (
            TuiScope::SessionPicker,
            action_bindings(&[
                ("up", TuiAction::SelectUp),
                ("k", TuiAction::SelectUp),
                ("down", TuiAction::SelectDown),
                ("j", TuiAction::SelectDown),
                ("enter", TuiAction::SelectConfirm),
                ("escape", TuiAction::SelectCancel),
                ("ctrl+c", TuiAction::SelectCancel),
            ]),
        ),
    ])
}

fn action_bindings(bindings: &[(&str, TuiAction)]) -> BTreeMap<String, TuiAction> {
    bindings
        .iter()
        .map(|(key, action)| ((*key).to_string(), *action))
        .collect()
}

fn apply_legacy_keybindings(
    bindings: &mut BTreeMap<TuiScope, BTreeMap<String, TuiAction>>,
    warnings: &mut Vec<String>,
    legacy_actions: &BTreeMap<String, Vec<String>>,
) {
    for (id, keys) in legacy_actions {
        let Some(action) = TuiAction::from_id(id) else {
            warnings.push(format!("unknown legacy keybinding action: {id}"));
            continue;
        };
        for scope in legacy_scopes_for_action(action) {
            let scope_bindings = bindings.entry(*scope).or_default();
            scope_bindings.retain(|_, existing_action| *existing_action != action);
            for key in keys {
                scope_bindings.insert(key.clone(), action);
            }
        }
    }
}

fn legacy_scopes_for_action(action: TuiAction) -> &'static [TuiScope] {
    match action {
        TuiAction::PermissionApprove
        | TuiAction::PermissionDeny
        | TuiAction::PermissionAlwaysAllow
        | TuiAction::PermissionAlwaysDeny => &[TuiScope::Permission],
        TuiAction::SelectUp
        | TuiAction::SelectDown
        | TuiAction::SelectConfirm
        | TuiAction::SelectCancel => &[TuiScope::Permission, TuiScope::SessionPicker],
        _ => &[TuiScope::Chat],
    }
}

fn apply_scoped_keybindings(
    bindings: &mut BTreeMap<TuiScope, BTreeMap<String, TuiAction>>,
    warnings: &mut Vec<String>,
    scope: TuiScope,
    configured: &BTreeMap<String, String>,
) {
    let scope_bindings = bindings.entry(scope).or_default();
    for (key, action_id) in configured {
        if action_id.trim().is_empty()
            || action_id.eq_ignore_ascii_case("none")
            || action_id.eq_ignore_ascii_case("unbind")
        {
            scope_bindings.remove(key);
            continue;
        }
        let Some(action) = TuiAction::from_id(action_id) else {
            warnings.push(format!(
                "unknown keybinding action in {scope:?}: {key} -> {action_id}"
            ));
            scope_bindings.remove(key);
            continue;
        };
        scope_bindings.insert(key.clone(), action);
    }
}

fn compile_keybinding_scope(
    scope: TuiScope,
    bindings: BTreeMap<String, TuiAction>,
    warnings: &mut Vec<String>,
) -> Vec<KeyMapEntry> {
    bindings
        .into_iter()
        .filter_map(|(key, action)| match parse_key_binding(&key) {
            Ok(binding) => Some(KeyMapEntry {
                key,
                binding,
                action,
            }),
            Err(error) => {
                warnings.push(format!("invalid keybinding in {scope:?}: {key}: {error}"));
                None
            }
        })
        .collect()
}

fn parse_key_binding(input: &str) -> Result<KeyBinding, String> {
    let normalized = input.trim();
    if normalized.is_empty() {
        return Err("empty keybinding".to_string());
    }
    let parts = normalized.split('+').collect::<Vec<_>>();
    let key_part = if normalized.ends_with('+') {
        "+"
    } else {
        parts.last().copied().unwrap_or(normalized)
    };
    let modifier_parts = if normalized.ends_with('+') {
        &parts[..parts.len().saturating_sub(2)]
    } else {
        &parts[..parts.len().saturating_sub(1)]
    };
    let mut modifiers = KeyModifiers::empty();
    for part in modifier_parts {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => modifiers.insert(KeyModifiers::CONTROL),
            "alt" | "option" => modifiers.insert(KeyModifiers::ALT),
            "shift" => modifiers.insert(KeyModifiers::SHIFT),
            "" => {}
            unknown => return Err(format!("unknown modifier '{unknown}'")),
        }
    }
    Ok(KeyBinding {
        code: parse_key_code(key_part)?,
        modifiers,
    })
}

fn parse_key_code(key: &str) -> Result<KeyCode, String> {
    let lower = key.to_ascii_lowercase();
    Ok(match lower.as_str() {
        "esc" | "escape" => KeyCode::Esc,
        "enter" | "return" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "backspace" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "page_up" | "page-up" => KeyCode::PageUp,
        "pagedown" | "page_down" | "page-down" => KeyCode::PageDown,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "space" => KeyCode::Char(' '),
        function if function.starts_with('f') && function.len() > 1 && function.len() <= 3 => {
            let number = function[1..]
                .parse::<u8>()
                .map_err(|_| format!("unknown key '{key}'"))?;
            if !(1..=12).contains(&number) {
                return Err(format!("function key out of range: {key}"));
            }
            KeyCode::F(number)
        }
        single if single.chars().count() == 1 => {
            KeyCode::Char(single.chars().next().expect("single char should exist"))
        }
        _ => return Err(format!("unknown key '{key}'")),
    })
}

fn normalized_key(key: &KeyEvent) -> (KeyCode, KeyModifiers) {
    let mut modifiers = normalized_modifiers(key.modifiers);
    let code = match key.code {
        KeyCode::Char(character) if character.is_ascii_uppercase() => {
            modifiers.insert(KeyModifiers::SHIFT);
            KeyCode::Char(character.to_ascii_lowercase())
        }
        KeyCode::Char(character) => KeyCode::Char(character),
        KeyCode::BackTab => {
            modifiers.insert(KeyModifiers::SHIFT);
            KeyCode::Tab
        }
        code => code,
    };
    (code, modifiers)
}

fn normalized_modifiers(modifiers: KeyModifiers) -> KeyModifiers {
    let mut normalized = KeyModifiers::empty();
    for modifier in [
        KeyModifiers::CONTROL,
        KeyModifiers::ALT,
        KeyModifiers::SHIFT,
    ] {
        if modifiers.contains(modifier) {
            normalized.insert(modifier);
        }
    }
    normalized
}

fn key_is_text_input(key: &KeyEvent) -> Option<char> {
    let (code, modifiers) = normalized_key(key);
    match code {
        KeyCode::Char(character) if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT => {
            Some(character)
        }
        _ => None,
    }
}

/// Run the interactive terminal UI.
///
/// # Errors
///
/// Returns an error when terminal setup, daemon communication, or rendering fails.
pub async fn run(session_id: Option<SessionId>) -> Result<(), TuiError> {
    let client = BcodeClient::default_endpoint();
    let config = bcode_config::load_config()?;
    let keymap = KeyMap::from_config(&config.tui);
    let session_id = resolve_session(&client, session_id, &keymap).await?;
    run_chat(client, session_id, keymap).await
}

#[allow(clippy::too_many_lines)]
async fn run_chat(
    client: BcodeClient,
    session_id: SessionId,
    keymap: KeyMap,
) -> Result<(), TuiError> {
    let mut connection = client.connect("bcode-tui").await?;
    let history = connection.attach_session(session_id).await?;

    let (event_sender, mut event_receiver) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            match connection.recv_event().await {
                Ok(event) => {
                    if event_sender.send(event).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    eprintln!("TUI event stream ended: {error}");
                    break;
                }
            }
        }
    });

    let status = client.server_status().await.ok();
    let mut terminal = TerminalGuard::enter()?;
    let mut app = ChatApp::new(session_id, &history, &keymap);
    if let Some(status) = status {
        app.selected_provider_plugin_id = status.selected_provider_plugin_id;
        app.selected_model_id = status.selected_model_id;
    }

    loop {
        while let Ok(event) = event_receiver.try_recv() {
            app.push_event(event);
        }

        terminal.draw(&app)?;

        if event::poll(Duration::from_millis(50))? {
            let CrosstermEvent::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            let scope = app.current_scope();
            if let Some(action) = keymap.action_for_key(scope, &key) {
                if handle_tui_action(&client, &mut app, session_id, scope, action).await {
                    break;
                }
                continue;
            }
            if scope == TuiScope::Permission {
                app.status = "permission prompt active; use configured prompt bindings".to_string();
                continue;
            }
            if let Some(character) = key_is_text_input(&key) {
                app.input.push(character);
                if app.search_mode {
                    app.update_search();
                }
            }
        }
    }

    Ok(())
}

async fn execute_selected_permission_choice(client: &BcodeClient, app: &mut ChatApp) {
    execute_permission_choice(client, app, app.selected_permission_choice).await;
}

async fn execute_permission_choice(
    client: &BcodeClient,
    app: &mut ChatApp,
    choice: PermissionChoice,
) {
    match choice {
        PermissionChoice::AllowOnce => resolve_first_permission(client, app, true).await,
        PermissionChoice::DenyOnce => resolve_first_permission(client, app, false).await,
        PermissionChoice::AlwaysAllow => persist_first_permission_rule(client, app, true).await,
        PermissionChoice::AlwaysDeny => persist_first_permission_rule(client, app, false).await,
    }
}

async fn handle_tui_action(
    client: &BcodeClient,
    app: &mut ChatApp,
    session_id: SessionId,
    scope: TuiScope,
    action: TuiAction,
) -> bool {
    match action {
        TuiAction::AppExit => {
            if app.input.is_empty() {
                return true;
            }
            app.input.clear();
            app.status = "input cleared; press exit again to quit".to_string();
        }
        TuiAction::AppInterrupt => {
            if app.search_mode {
                app.cancel_search();
            } else {
                match client.cancel_session_turn(session_id).await {
                    Ok(true) => app.status = "turn cancellation requested".to_string(),
                    Ok(false) => app.status = "no active turn".to_string(),
                    Err(error) => app.status = format!("cancel failed: {error}"),
                }
            }
        }
        TuiAction::AppClear => {
            app.input.clear();
            app.status = "input cleared".to_string();
        }
        TuiAction::PermissionApprove => resolve_first_permission(client, app, true).await,
        TuiAction::PermissionDeny => resolve_first_permission(client, app, false).await,
        TuiAction::PermissionAlwaysAllow => persist_first_permission_rule(client, app, true).await,
        TuiAction::PermissionAlwaysDeny => persist_first_permission_rule(client, app, false).await,
        TuiAction::SearchStart => app.start_search(),
        TuiAction::SearchNext => app.find_next(),
        TuiAction::SearchPrevious => app.find_previous(),
        TuiAction::TranscriptPageUp => app.scroll_page_up(),
        TuiAction::TranscriptPageDown => app.scroll_page_down(),
        TuiAction::TranscriptTop => app.scroll_top(),
        TuiAction::TranscriptBottom => app.scroll_bottom(),
        TuiAction::TranscriptLineUp => app.scroll_line_up(),
        TuiAction::TranscriptLineDown => app.scroll_line_down(),
        TuiAction::InputSubmit => {
            if app.search_mode {
                app.finish_search();
            } else if let Some(message) = app.take_input()
                && let Err(error) = client.send_user_message(session_id, message).await
            {
                app.status = format!("send failed: {error}");
            }
        }
        TuiAction::InputNewLine => {
            app.input.push('\n');
            if app.search_mode {
                app.update_search();
            }
        }
        TuiAction::DeleteCharBackward => {
            app.input.pop();
            if app.search_mode {
                app.update_search();
            }
        }
        TuiAction::SelectUp => {
            if scope == TuiScope::Permission {
                app.previous_permission_choice();
            }
        }
        TuiAction::SelectDown => {
            if scope == TuiScope::Permission {
                app.next_permission_choice();
            }
        }
        TuiAction::SelectConfirm => {
            if scope == TuiScope::Permission {
                execute_selected_permission_choice(client, app).await;
            }
        }
        TuiAction::SelectCancel => {
            if scope == TuiScope::Permission {
                execute_permission_choice(client, app, PermissionChoice::DenyOnce).await;
            }
        }
    }
    false
}

async fn resolve_first_permission(client: &BcodeClient, app: &mut ChatApp, approved: bool) {
    let Some(permission_id) = app.first_pending_permission_id() else {
        app.status = "no pending permission".to_string();
        return;
    };
    match client
        .resolve_permission(permission_id.clone(), approved)
        .await
    {
        Ok(true) => {
            let action = if approved { "approved" } else { "denied" };
            app.status = format!("permission {permission_id} {action}");
            app.remove_pending_permission(&permission_id);
        }
        Ok(false) => app.status = format!("permission {permission_id} was not pending"),
        Err(error) => app.status = format!("permission resolve failed: {error}"),
    }
}

async fn persist_first_permission_rule(client: &BcodeClient, app: &mut ChatApp, approved: bool) {
    let Some(permission) = app.first_pending_permission().cloned() else {
        app.status = "no pending permission".to_string();
        return;
    };
    let (kind, value) = permission.policy_rule(approved);
    match client
        .add_permission_rule(kind.to_string(), value.clone())
        .await
    {
        Ok(_) => {
            resolve_first_permission(client, app, approved).await;
            let action = if approved { "allow" } else { "deny" };
            app.status = format!("persisted {action} rule {kind}={value}");
        }
        Err(error) => app.status = format!("persist rule failed: {error}"),
    }
}

async fn resolve_session(
    client: &BcodeClient,
    session_id: Option<SessionId>,
    keymap: &KeyMap,
) -> Result<SessionId, TuiError> {
    if let Some(session_id) = session_id {
        return Ok(session_id);
    }
    let sessions = client.list_sessions().await?;
    match sessions.len() {
        0 => Ok(client.create_session(Some("default".to_string())).await?.id),
        1 => Ok(sessions[0].id),
        _ => pick_session(&sessions, keymap),
    }
}

fn pick_session(sessions: &[SessionSummary], keymap: &KeyMap) -> Result<SessionId, TuiError> {
    let mut terminal = TerminalGuard::enter()?;
    let mut app = SessionPickerApp::new(sessions);
    loop {
        terminal.draw(&app)?;
        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        let CrosstermEvent::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match keymap.action_for_key(TuiScope::SessionPicker, &key) {
            Some(TuiAction::SelectCancel) => return Err(TuiError::Canceled),
            Some(TuiAction::SelectUp) => app.previous(),
            Some(TuiAction::SelectDown) => app.next(),
            Some(TuiAction::SelectConfirm) => return Ok(app.selected_session_id()),
            _ => {}
        }
    }
}

#[derive(Debug)]
struct SessionPickerApp {
    sessions: Vec<SessionSummary>,
    selected: usize,
}

impl SessionPickerApp {
    fn new(sessions: &[SessionSummary]) -> Self {
        Self {
            sessions: sessions.to_vec(),
            selected: 0,
        }
    }

    fn next(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.sessions.len();
    }

    fn previous(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        self.selected = self
            .selected
            .checked_sub(1)
            .unwrap_or_else(|| self.sessions.len() - 1);
    }

    fn selected_session_id(&self) -> SessionId {
        self.sessions[self.selected].id
    }
}

impl ratatui::widgets::Widget for &SessionPickerApp {
    fn render(self, area: ratatui::layout::Rect, buf: &mut ratatui::buffer::Buffer) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(3),
                Constraint::Length(1),
            ])
            .split(area);

        Paragraph::new("Select a Bcode session").render(chunks[0], buf);

        let items = self.sessions.iter().map(|session| {
            let name = session.name.as_deref().unwrap_or("<unnamed>");
            ListItem::new(format!(
                "{}  {}  ({} clients)",
                session.id, name, session.client_count
            ))
        });
        let list = List::new(items)
            .block(Block::new().title("Sessions").borders(Borders::ALL))
            .highlight_symbol("> ");
        let mut state = ListState::default().with_selected(Some(self.selected));
        StatefulWidget::render(list, chunks[1], buf, &mut state);

        Paragraph::new("enter selects, up/down or j/k moves, esc quits").render(chunks[2], buf);
    }
}

#[derive(Debug, Clone)]
enum TranscriptBlock {
    User {
        text: String,
    },
    Assistant {
        text: String,
        streaming: bool,
    },
    ToolCall {
        id: String,
        name: String,
        arguments_json: String,
    },
    ToolResult {
        id: String,
        result: String,
        is_error: bool,
    },
    PermissionRequest {
        id: String,
        tool_call_id: String,
        name: String,
        arguments_json: String,
    },
    PermissionResult {
        approved: bool,
    },
    Meta {
        text: String,
    },
    System {
        text: String,
    },
}

#[derive(Debug)]
struct ChatApp {
    session_id: SessionId,
    blocks: Vec<TranscriptBlock>,
    input: String,
    status: String,
    pending_permissions: BTreeMap<String, PendingPermissionView>,
    selected_provider_plugin_id: Option<String>,
    selected_model_id: Option<String>,
    scroll_from_bottom: usize,
    search_mode: bool,
    search_query: String,
    key_hints: String,
    permission_hints: String,
    selected_permission_choice: PermissionChoice,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermissionChoice {
    AllowOnce,
    DenyOnce,
    AlwaysAllow,
    AlwaysDeny,
}

impl PermissionChoice {
    const fn label(self) -> &'static str {
        match self {
            Self::AllowOnce => "allow once",
            Self::DenyOnce => "deny",
            Self::AlwaysAllow => "always allow",
            Self::AlwaysDeny => "always deny",
        }
    }

    const fn next(self) -> Self {
        match self {
            Self::AllowOnce => Self::DenyOnce,
            Self::DenyOnce => Self::AlwaysAllow,
            Self::AlwaysAllow => Self::AlwaysDeny,
            Self::AlwaysDeny => Self::AllowOnce,
        }
    }

    const fn previous(self) -> Self {
        match self {
            Self::AllowOnce => Self::AlwaysDeny,
            Self::DenyOnce => Self::AllowOnce,
            Self::AlwaysAllow => Self::DenyOnce,
            Self::AlwaysDeny => Self::AlwaysAllow,
        }
    }
}

#[derive(Debug, Clone)]
struct PendingPermissionView {
    permission_id: String,
    tool_call_id: String,
    tool_name: String,
    arguments_json: String,
}

impl PendingPermissionView {
    fn render_text(&self, key_hints: &str, selected: PermissionChoice) -> String {
        format!(
            "Tool wants permission\n{} ({})\npermission {}\n{}\n{}\nselected: {}",
            self.tool_name,
            self.tool_call_id,
            self.permission_id,
            pretty_jsonish(&self.arguments_json),
            key_hints,
            selected.label()
        )
    }

    fn policy_rule(&self, approved: bool) -> (&'static str, String) {
        if self.tool_name == "shell.run"
            && let Some(command) = self.string_argument("command")
        {
            return if approved {
                ("allow_shell_command_prefix", command)
            } else {
                ("deny_shell_command_prefix", command)
            };
        }
        if self.tool_name.starts_with("filesystem.")
            && let Some(path) = self.string_argument("path")
        {
            return if approved {
                ("allow_path_prefix", path)
            } else {
                ("deny_path_prefix", path)
            };
        }
        if approved {
            ("allow_tool", self.tool_name.clone())
        } else {
            ("deny_tool", self.tool_name.clone())
        }
    }

    fn string_argument(&self, key: &str) -> Option<String> {
        serde_json::from_str::<serde_json::Value>(&self.arguments_json)
            .ok()
            .and_then(|arguments| {
                arguments
                    .get(key)
                    .and_then(serde_json::Value::as_str)
                    .map(ToString::to_string)
            })
    }
}

impl ChatApp {
    fn new(session_id: SessionId, history: &[SessionEvent], keymap: &KeyMap) -> Self {
        let mut app = Self {
            session_id,
            blocks: Vec::new(),
            input: String::new(),
            status: keymap
                .warnings
                .first()
                .cloned()
                .unwrap_or_else(|| "ready".to_string()),
            pending_permissions: BTreeMap::new(),
            selected_provider_plugin_id: None,
            selected_model_id: None,
            scroll_from_bottom: 0,
            search_mode: false,
            search_query: String::new(),
            key_hints: keymap.chat_hints(),
            permission_hints: keymap.permission_hints(),
            selected_permission_choice: PermissionChoice::AllowOnce,
        };
        for event in history {
            app.absorb_session_event(event);
        }
        app
    }

    fn push_event(&mut self, event: Event) {
        match event {
            Event::Session(event) => self.absorb_session_event(&event),
        }
    }

    fn start_search(&mut self) {
        self.search_mode = true;
        self.search_query.clear();
        self.input.clear();
        self.status = "search: type query, submit accepts, next/previous jump".to_string();
    }

    fn finish_search(&mut self) {
        self.search_mode = false;
        self.search_query = self.input.clone();
        self.input.clear();
        self.find_next();
    }

    fn cancel_search(&mut self) {
        self.search_mode = false;
        self.input.clear();
        self.status = "search canceled".to_string();
    }

    fn update_search(&mut self) {
        self.search_query = self.input.clone();
        if self.search_query.is_empty() {
            self.status = "search: type query, submit accepts, next/previous jump".to_string();
        } else {
            self.find_previous_match();
        }
    }

    fn find_next(&mut self) {
        if self.search_query.is_empty() {
            self.status = "no search query".to_string();
            return;
        }
        let Some(index) = self.next_match_index() else {
            self.status = format!("no match: {}", self.search_query);
            return;
        };
        self.scroll_to_line(index);
        self.status = format!("match: {}", self.search_query);
    }

    fn find_previous(&mut self) {
        if self.search_query.is_empty() {
            self.status = "no search query".to_string();
            return;
        }
        self.find_previous_match();
    }

    fn find_previous_match(&mut self) {
        let Some(index) = self.previous_match_index() else {
            self.status = format!("no match: {}", self.search_query);
            return;
        };
        self.scroll_to_line(index);
        self.status = format!("match: {}", self.search_query);
    }

    fn next_match_index(&self) -> Option<usize> {
        let lines = self.rendered_line_texts();
        let current = self.top_visible_line_index();
        lines
            .iter()
            .enumerate()
            .skip(current.saturating_add(1))
            .chain(lines.iter().enumerate().take(current.saturating_add(1)))
            .find_map(|(index, line)| line.contains(&self.search_query).then_some(index))
    }

    fn previous_match_index(&self) -> Option<usize> {
        let lines = self.rendered_line_texts();
        let current = self.top_visible_line_index();
        lines
            .iter()
            .enumerate()
            .take(current)
            .rev()
            .chain(lines.iter().enumerate().skip(current).rev())
            .find_map(|(index, line)| line.contains(&self.search_query).then_some(index))
    }

    fn top_visible_line_index(&self) -> usize {
        self.rendered_line_count()
            .saturating_sub(self.scroll_from_bottom)
            .saturating_sub(1)
    }

    fn scroll_to_line(&mut self, index: usize) {
        self.scroll_from_bottom = self
            .rendered_line_count()
            .saturating_sub(index.saturating_add(1));
    }

    fn scroll_line_up(&mut self) {
        self.scroll_from_bottom = self.scroll_from_bottom.saturating_add(1);
    }

    fn scroll_line_down(&mut self) {
        self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(1);
    }

    fn scroll_page_up(&mut self) {
        self.scroll_from_bottom = self.scroll_from_bottom.saturating_add(10);
    }

    fn scroll_page_down(&mut self) {
        self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(10);
    }

    fn scroll_top(&mut self) {
        self.scroll_from_bottom = self.rendered_line_count();
    }

    fn scroll_bottom(&mut self) {
        self.scroll_from_bottom = 0;
    }

    fn absorb_session_event(&mut self, event: &SessionEvent) {
        match &event.kind {
            SessionEventKind::AssistantDelta { text } => {
                self.push_assistant_delta(text);
                return;
            }
            SessionEventKind::AssistantMessage { text } => {
                self.finish_assistant_message(text);
                return;
            }
            SessionEventKind::PermissionRequested {
                permission_id,
                tool_call_id,
                tool_name,
                arguments_json,
            } => {
                self.pending_permissions.insert(
                    permission_id.clone(),
                    PendingPermissionView {
                        permission_id: permission_id.clone(),
                        tool_call_id: tool_call_id.clone(),
                        tool_name: tool_name.clone(),
                        arguments_json: arguments_json.clone(),
                    },
                );
                self.selected_permission_choice = PermissionChoice::AllowOnce;
                self.status = format!("permission pending: {permission_id}");
            }
            SessionEventKind::PermissionResolved { permission_id, .. } => {
                self.remove_pending_permission(permission_id);
            }
            SessionEventKind::ModelChanged { provider, model } => {
                self.selected_provider_plugin_id = provider_to_display_selection(provider);
                self.selected_model_id = model_to_display_selection(model);
            }
            _ => {}
        }
        self.push_session_event(event);
        self.clamp_scroll();
    }

    fn push_session_event(&mut self, event: &SessionEvent) {
        self.finish_streaming_block_if_needed();
        self.blocks.extend(transcript_blocks_from_event(event));
    }

    fn push_assistant_delta(&mut self, text: &str) {
        match self.blocks.last_mut() {
            Some(TranscriptBlock::Assistant {
                text: current,
                streaming: true,
            }) => {
                current.push_str(text);
            }
            _ => self.blocks.push(TranscriptBlock::Assistant {
                text: text.to_string(),
                streaming: true,
            }),
        }
        self.clamp_scroll();
    }

    fn finish_assistant_message(&mut self, text: &str) {
        match self.blocks.last_mut() {
            Some(TranscriptBlock::Assistant {
                text: current,
                streaming,
            }) if *streaming => {
                *current = text.to_string();
                *streaming = false;
            }
            _ => self.blocks.push(TranscriptBlock::Assistant {
                text: text.to_string(),
                streaming: false,
            }),
        }
        self.clamp_scroll();
    }

    fn finish_streaming_block_if_needed(&mut self) {
        if let Some(TranscriptBlock::Assistant { streaming, .. }) = self.blocks.last_mut() {
            *streaming = false;
        }
    }

    fn clamp_scroll(&mut self) {
        self.scroll_from_bottom = self.scroll_from_bottom.min(self.rendered_line_count());
    }

    fn rendered_line_count(&self) -> usize {
        self.rendered_line_texts().len()
    }

    fn rendered_line_texts(&self) -> Vec<String> {
        self.blocks
            .iter()
            .flat_map(TranscriptBlock::plain_lines)
            .collect()
    }

    fn rendered_transcript_lines(&self) -> Vec<Line<'static>> {
        self.blocks
            .iter()
            .flat_map(|block| block.render_lines(&self.search_query))
            .collect()
    }

    fn remove_pending_permission(&mut self, permission_id: &str) {
        self.pending_permissions.remove(permission_id);
        self.selected_permission_choice = PermissionChoice::AllowOnce;
    }

    fn next_permission_choice(&mut self) {
        self.selected_permission_choice = self.selected_permission_choice.next();
        self.status = format!(
            "permission choice: {}",
            self.selected_permission_choice.label()
        );
    }

    fn previous_permission_choice(&mut self) {
        self.selected_permission_choice = self.selected_permission_choice.previous();
        self.status = format!(
            "permission choice: {}",
            self.selected_permission_choice.label()
        );
    }

    fn first_pending_permission_id(&self) -> Option<String> {
        self.pending_permissions.keys().next().cloned()
    }

    fn first_pending_permission(&self) -> Option<&PendingPermissionView> {
        self.pending_permissions.values().next()
    }

    fn current_scope(&self) -> TuiScope {
        if self.first_pending_permission().is_some() && !self.search_mode {
            TuiScope::Permission
        } else {
            TuiScope::Chat
        }
    }

    fn take_input(&mut self) -> Option<String> {
        let input = self.input.trim().to_string();
        if input.is_empty() {
            return None;
        }
        self.input.clear();
        Some(input)
    }
}

fn provider_to_display_selection(provider: &str) -> Option<String> {
    if provider == "<auto>" || provider.is_empty() {
        None
    } else {
        Some(provider.to_string())
    }
}

fn model_to_display_selection(model: &str) -> Option<String> {
    if model == "<default>" || model.is_empty() {
        None
    } else {
        Some(model.to_string())
    }
}

impl ratatui::widgets::Widget for &ChatApp {
    fn render(self, area: ratatui::layout::Rect, buf: &mut ratatui::buffer::Buffer) {
        let has_permission = self.first_pending_permission().is_some();
        let constraints = if has_permission {
            vec![
                Constraint::Length(1),
                Constraint::Min(3),
                Constraint::Length(8),
                Constraint::Length(3),
                Constraint::Length(1),
            ]
        } else {
            vec![
                Constraint::Length(1),
                Constraint::Min(3),
                Constraint::Length(3),
                Constraint::Length(1),
            ]
        };
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);

        let provider = self
            .selected_provider_plugin_id
            .as_deref()
            .unwrap_or("auto");
        let model = self.selected_model_id.as_deref().unwrap_or("default");
        let header = Paragraph::new(format!(
            "Bcode  {}  provider: {provider}  model: {model}",
            self.session_id
        ));
        header.render(chunks[0], buf);

        let transcript_height = usize::from(chunks[1].height.saturating_sub(2));
        let rendered_lines = self.rendered_transcript_lines();
        let visible_end = rendered_lines
            .len()
            .saturating_sub(self.scroll_from_bottom)
            .max(transcript_height.min(rendered_lines.len()));
        let start = visible_end.saturating_sub(transcript_height);
        let title = if self.scroll_from_bottom == 0 {
            "Chat".to_string()
        } else {
            format!("Chat ({} from bottom)", self.scroll_from_bottom)
        };
        let transcript_lines = rendered_lines[start..visible_end].to_vec();
        let transcript = Paragraph::new(Text::from(transcript_lines))
            .block(Block::new().title(title).borders(Borders::ALL))
            .wrap(Wrap { trim: false });
        transcript.render(chunks[1], buf);

        let input_index = self.first_pending_permission().map_or(2, |permission| {
            let permission = Paragraph::new(
                permission.render_text(&self.permission_hints, self.selected_permission_choice),
            )
            .block(
                Block::new()
                    .title("Permission Required")
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false });
            permission.render(chunks[2], buf);
            3
        });

        let input_title = if self.search_mode {
            "Search"
        } else {
            "Message"
        };
        let input = Paragraph::new(self.input.as_str())
            .block(Block::new().title(input_title).borders(Borders::ALL))
            .wrap(Wrap { trim: false });
        input.render(chunks[input_index], buf);

        let status = format!("{} | {}", self.status, self.key_hints);
        Paragraph::new(status).render(chunks[input_index + 1], buf);
    }
}

impl TranscriptBlock {
    fn plain_lines(&self) -> Vec<String> {
        match self {
            Self::User { text } => block_plain_lines("You", text),
            Self::Assistant { text, streaming } => {
                block_plain_lines(if *streaming { "Bcode…" } else { "Bcode" }, text)
            }
            Self::ToolCall {
                name,
                arguments_json,
                ..
            } => block_plain_lines(&format!("Tool · {name}"), &pretty_jsonish(arguments_json)),
            Self::ToolResult {
                id,
                result,
                is_error,
            } => block_plain_lines(
                &format!(
                    "Tool result · {id} · {}",
                    if *is_error { "failed" } else { "ok" }
                ),
                &tool_result_preview(result),
            ),
            Self::PermissionRequest {
                name,
                arguments_json,
                ..
            } => block_plain_lines(
                &format!("Permission required · {name}"),
                &pretty_jsonish(arguments_json),
            ),
            Self::PermissionResult { approved } => {
                block_plain_lines("Permission", if *approved { "allowed" } else { "denied" })
            }
            Self::Meta { text } => vec![format!("· {text}"), String::new()],
            Self::System { text } => block_plain_lines("System", text),
        }
    }

    fn render_lines(&self, search_query: &str) -> Vec<Line<'static>> {
        match self {
            Self::User { text } => render_message_block("You", text, Color::Blue, search_query),
            Self::Assistant { text, streaming } => render_message_block(
                if *streaming { "Bcode …" } else { "Bcode" },
                text,
                if *streaming {
                    Color::Cyan
                } else {
                    Color::Green
                },
                search_query,
            ),
            Self::ToolCall {
                id,
                name,
                arguments_json,
            } => render_detail_block(
                &format!("Tool · {name}"),
                &format!("id: {id}\n{}", pretty_jsonish(arguments_json)),
                Color::Yellow,
                search_query,
            ),
            Self::ToolResult {
                id,
                result,
                is_error,
            } => render_detail_block(
                &format!(
                    "Tool result · {id} · {}",
                    if *is_error { "failed" } else { "ok" }
                ),
                &tool_result_preview(result),
                if *is_error { Color::Red } else { Color::Yellow },
                search_query,
            ),
            Self::PermissionRequest {
                id,
                tool_call_id,
                name,
                arguments_json,
            } => render_detail_block(
                &format!("Permission required · {name}"),
                &format!(
                    "permission: {id}\ntool call: {tool_call_id}\n{}",
                    pretty_jsonish(arguments_json)
                ),
                Color::Red,
                search_query,
            ),
            Self::PermissionResult { approved } => render_detail_block(
                "Permission",
                if *approved { "allowed" } else { "denied" },
                if *approved { Color::Green } else { Color::Red },
                search_query,
            ),
            Self::Meta { text } => vec![highlighted_line(
                "· ",
                text,
                Style::default().fg(Color::DarkGray),
                Style::default().fg(Color::DarkGray),
                search_query,
            )],
            Self::System { text } => {
                render_detail_block("System", text, Color::DarkGray, search_query)
            }
        }
    }
}

fn block_plain_lines(title: &str, body: &str) -> Vec<String> {
    let mut lines = vec![format!("{title}:")];
    lines.extend(body.lines().map(|line| format!("  {line}")));
    lines.push(String::new());
    lines
}

fn render_message_block(
    title: &str,
    body: &str,
    color: Color,
    search_query: &str,
) -> Vec<Line<'static>> {
    render_block(title, body, color, true, search_query)
}

fn render_detail_block(
    title: &str,
    body: &str,
    color: Color,
    search_query: &str,
) -> Vec<Line<'static>> {
    render_block(title, body, color, false, search_query)
}

fn render_block(
    title: &str,
    body: &str,
    color: Color,
    prominent: bool,
    search_query: &str,
) -> Vec<Line<'static>> {
    let border_style = Style::default().fg(color);
    let title_style = if prominent {
        border_style.add_modifier(Modifier::BOLD)
    } else {
        border_style
    };
    let mut lines = vec![highlighted_line(
        "╭─ ",
        title,
        border_style,
        title_style,
        search_query,
    )];
    for line in body.lines() {
        lines.push(highlighted_line(
            "│ ",
            line,
            border_style,
            Style::default(),
            search_query,
        ));
    }
    if body.is_empty() {
        lines.push(Line::from(Span::styled("│", border_style)));
    }
    lines.push(Line::from(Span::styled("╰", border_style)));
    lines.push(Line::default());
    lines
}

fn highlighted_line(
    prefix: &str,
    body: &str,
    prefix_style: Style,
    body_style: Style,
    search_query: &str,
) -> Line<'static> {
    let mut spans = Vec::new();
    push_highlighted_spans(&mut spans, prefix, search_query, prefix_style);
    push_highlighted_spans(&mut spans, body, search_query, body_style);
    Line::from(spans)
}

fn push_highlighted_spans(
    spans: &mut Vec<Span<'static>>,
    text: &str,
    search_query: &str,
    style: Style,
) {
    if search_query.is_empty() {
        spans.push(Span::styled(text.to_string(), style));
        return;
    }

    let mut remainder = text;
    let highlight_style = Style::default()
        .fg(Color::Black)
        .bg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    while let Some(match_start) = remainder.find(search_query) {
        let (before, matched_and_after) = remainder.split_at(match_start);
        if !before.is_empty() {
            spans.push(Span::styled(before.to_string(), style));
        }
        let (matched, after) = matched_and_after.split_at(search_query.len());
        spans.push(Span::styled(matched.to_string(), highlight_style));
        remainder = after;
    }
    if !remainder.is_empty() {
        spans.push(Span::styled(remainder.to_string(), style));
    }
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    fn enter() -> Result<Self, io::Error> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        Ok(Self { terminal })
    }

    fn draw<W>(&mut self, widget: W) -> Result<(), io::Error>
    where
        W: ratatui::widgets::Widget,
    {
        self.terminal
            .draw(|frame| frame.render_widget(widget, frame.area()))?;
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

fn transcript_blocks_from_event(event: &SessionEvent) -> Vec<TranscriptBlock> {
    match &event.kind {
        SessionEventKind::SessionCreated { name } => vec![TranscriptBlock::Meta {
            text: format!("session started: {}", name.as_deref().unwrap_or("untitled")),
        }],
        SessionEventKind::ClientAttached { .. } | SessionEventKind::ClientDetached { .. } => {
            Vec::new()
        }
        SessionEventKind::UserMessage { text, .. } => {
            vec![TranscriptBlock::User { text: text.clone() }]
        }
        SessionEventKind::AssistantDelta { text } => vec![TranscriptBlock::Assistant {
            text: text.clone(),
            streaming: true,
        }],
        SessionEventKind::AssistantMessage { text } => vec![TranscriptBlock::Assistant {
            text: text.clone(),
            streaming: false,
        }],
        SessionEventKind::ToolCallRequested {
            tool_call_id,
            tool_name,
            arguments_json,
        } => vec![TranscriptBlock::ToolCall {
            id: tool_call_id.clone(),
            name: tool_name.clone(),
            arguments_json: arguments_json.clone(),
        }],
        SessionEventKind::ToolCallFinished {
            tool_call_id,
            result,
            is_error,
        } => vec![TranscriptBlock::ToolResult {
            id: tool_call_id.clone(),
            result: result.clone(),
            is_error: *is_error,
        }],
        SessionEventKind::PermissionRequested {
            permission_id,
            tool_call_id,
            tool_name,
            arguments_json,
        } => vec![TranscriptBlock::PermissionRequest {
            id: permission_id.clone(),
            tool_call_id: tool_call_id.clone(),
            name: tool_name.clone(),
            arguments_json: arguments_json.clone(),
        }],
        SessionEventKind::PermissionResolved { approved, .. } => {
            vec![TranscriptBlock::PermissionResult {
                approved: *approved,
            }]
        }
        SessionEventKind::ModelChanged { provider, model } => vec![TranscriptBlock::Meta {
            text: format!("model changed: {provider}/{model}"),
        }],
        SessionEventKind::SystemMessage { text } => {
            vec![TranscriptBlock::System { text: text.clone() }]
        }
    }
}

fn tool_result_preview(result: &str) -> String {
    let lines = result.lines().collect::<Vec<_>>();
    if lines.len() <= 24 {
        return result.to_string();
    }
    let mut preview = lines
        .iter()
        .take(20)
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    write!(preview, "\n… {} more lines", lines.len().saturating_sub(20))
        .expect("writing to string should not fail");
    preview
}

fn pretty_jsonish(value: &str) -> String {
    serde_json::from_str::<serde_json::Value>(value)
        .ok()
        .and_then(|json| serde_json::to_string_pretty(&json).ok())
        .unwrap_or_else(|| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keybinding_parser_handles_modifiers_and_named_keys() {
        let binding = parse_key_binding("alt+shift+y").expect("keybinding should parse");

        assert_eq!(binding.code, KeyCode::Char('y'));
        assert!(binding.modifiers.contains(KeyModifiers::ALT));
        assert!(binding.modifiers.contains(KeyModifiers::SHIFT));
    }

    #[test]
    fn keymap_user_config_overrides_and_unbinds_defaults() {
        let config = bcode_config::TuiConfig {
            keybindings: bcode_config::TuiKeyBindingConfig {
                chat: [
                    ("ctrl+s".to_string(), "app.search".to_string()),
                    ("ctrl+d".to_string(), String::new()),
                ]
                .into_iter()
                .collect(),
                ..bcode_config::TuiKeyBindingConfig::default()
            },
        };
        let keymap = KeyMap::from_config(&config);

        assert_eq!(
            keymap.action_for_key(
                TuiScope::Chat,
                &KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)
            ),
            Some(TuiAction::SearchStart)
        );
        assert_eq!(
            keymap.action_for_key(
                TuiScope::Chat,
                &KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)
            ),
            None
        );
    }

    #[test]
    fn permission_defaults_are_scoped_not_global() {
        let keymap = KeyMap::from_config(&bcode_config::TuiConfig::default());

        assert_eq!(
            keymap.action_for_key(
                TuiScope::Permission,
                &KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)
            ),
            Some(TuiAction::PermissionApprove)
        );
        assert_eq!(
            keymap.action_for_key(
                TuiScope::Chat,
                &KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)
            ),
            None
        );
    }

    #[test]
    fn permission_prompt_uses_configured_keys() {
        let config = bcode_config::TuiConfig {
            keybindings: bcode_config::TuiKeyBindingConfig {
                permission: [
                    ("y".to_string(), String::new()),
                    ("ctrl+y".to_string(), "app.permission.approve".to_string()),
                    ("escape".to_string(), String::new()),
                    ("ctrl+x".to_string(), "tui.select.cancel".to_string()),
                ]
                .into_iter()
                .collect(),
                ..bcode_config::TuiKeyBindingConfig::default()
            },
        };
        let keymap = KeyMap::from_config(&config);

        assert_eq!(
            keymap.action_for_key(
                TuiScope::Permission,
                &KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE)
            ),
            None
        );
        assert_eq!(
            keymap.action_for_key(
                TuiScope::Permission,
                &KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL)
            ),
            Some(TuiAction::PermissionApprove)
        );
        assert_eq!(
            keymap.action_for_key(
                TuiScope::Permission,
                &KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
            ),
            None
        );
        assert_eq!(
            keymap.action_for_key(
                TuiScope::Permission,
                &KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)
            ),
            Some(TuiAction::SelectCancel)
        );
    }

    #[test]
    fn legacy_action_array_config_still_applies() {
        let config = bcode_config::TuiConfig {
            keybindings: bcode_config::TuiKeyBindingConfig {
                legacy_actions: [
                    ("app.search".to_string(), vec!["ctrl+s".to_string()]),
                    ("app.exit".to_string(), Vec::new()),
                ]
                .into_iter()
                .collect(),
                ..bcode_config::TuiKeyBindingConfig::default()
            },
        };
        let keymap = KeyMap::from_config(&config);

        assert_eq!(
            keymap.action_for_key(
                TuiScope::Chat,
                &KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)
            ),
            Some(TuiAction::SearchStart)
        );
        assert_eq!(
            keymap.action_for_key(
                TuiScope::Chat,
                &KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)
            ),
            None
        );
    }
}
