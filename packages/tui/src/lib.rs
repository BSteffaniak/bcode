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
    const fn id(self) -> &'static str {
        match self {
            Self::InputSubmit => "tui.input.submit",
            Self::InputNewLine => "tui.input.newLine",
            Self::DeleteCharBackward => "tui.editor.deleteCharBackward",
            Self::AppInterrupt => "app.interrupt",
            Self::AppExit => "app.exit",
            Self::AppClear => "app.clear",
            Self::SearchStart => "app.search",
            Self::SearchNext => "app.search.next",
            Self::SearchPrevious => "app.search.previous",
            Self::PermissionApprove => "app.permission.approve",
            Self::PermissionDeny => "app.permission.deny",
            Self::PermissionAlwaysAllow => "app.permission.alwaysAllow",
            Self::PermissionAlwaysDeny => "app.permission.alwaysDeny",
            Self::TranscriptPageUp => "transcript.pageUp",
            Self::TranscriptPageDown => "transcript.pageDown",
            Self::TranscriptTop => "transcript.top",
            Self::TranscriptBottom => "transcript.bottom",
            Self::TranscriptLineUp => "transcript.lineUp",
            Self::TranscriptLineDown => "transcript.lineDown",
            Self::SelectUp => "tui.select.up",
            Self::SelectDown => "tui.select.down",
            Self::SelectConfirm => "tui.select.confirm",
            Self::SelectCancel => "tui.select.cancel",
        }
    }

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
            "tui.select.up" => Self::SelectUp,
            "tui.select.down" => Self::SelectDown,
            "tui.select.confirm" => Self::SelectConfirm,
            "tui.select.cancel" => Self::SelectCancel,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct KeyBinding {
    code: KeyCode,
    modifiers: KeyModifiers,
}

impl KeyBinding {
    fn matches(&self, key: &KeyEvent) -> bool {
        let (code, modifiers) = normalized_key(key);
        self.code == code && self.modifiers == modifiers
    }
}

#[derive(Debug, Clone)]
struct KeyMap {
    bindings: BTreeMap<TuiAction, Vec<KeyBinding>>,
    warnings: Vec<String>,
}

impl KeyMap {
    fn from_config(config: &bcode_config::TuiConfig) -> Self {
        let mut warnings = Vec::new();
        let mut bindings = default_keybindings()
            .into_iter()
            .map(|(action, keys)| (action, parse_default_keybindings(action, keys)))
            .collect::<BTreeMap<_, _>>();
        for (id, keys) in &config.keybindings {
            let Some(action) = TuiAction::from_id(id) else {
                warnings.push(format!("unknown keybinding action: {id}"));
                continue;
            };
            let mut parsed = Vec::new();
            for key in keys {
                match parse_key_binding(key) {
                    Ok(binding) => parsed.push(binding),
                    Err(error) => warnings.push(format!("invalid keybinding {id}={key}: {error}")),
                }
            }
            bindings.insert(action, parsed);
        }
        Self { bindings, warnings }
    }

    fn action_for_key(&self, key: &KeyEvent) -> Option<TuiAction> {
        self.bindings.iter().find_map(|(action, bindings)| {
            bindings
                .iter()
                .any(|binding| binding.matches(key))
                .then_some(*action)
        })
    }

    fn primary(&self, action: TuiAction) -> String {
        self.bindings
            .get(&action)
            .and_then(|bindings| bindings.first())
            .map_or_else(|| "unbound".to_string(), KeyBinding::display)
    }

    fn chat_hints(&self) -> String {
        format!(
            "{} send · {} interrupt · {} exit · {} search · {} approve · {} deny",
            self.primary(TuiAction::InputSubmit),
            self.primary(TuiAction::AppInterrupt),
            self.primary(TuiAction::AppExit),
            self.primary(TuiAction::SearchStart),
            self.primary(TuiAction::PermissionApprove),
            self.primary(TuiAction::PermissionDeny),
        )
    }
}

impl KeyBinding {
    fn display(&self) -> String {
        let mut parts = Vec::new();
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            parts.push("ctrl".to_string());
        }
        if self.modifiers.contains(KeyModifiers::ALT) {
            parts.push("alt".to_string());
        }
        if self.modifiers.contains(KeyModifiers::SHIFT) {
            parts.push("shift".to_string());
        }
        parts.push(key_code_name(self.code));
        parts.join("+")
    }
}

fn default_keybindings() -> Vec<(TuiAction, &'static [&'static str])> {
    vec![
        (TuiAction::InputSubmit, &["enter"]),
        (TuiAction::InputNewLine, &["shift+enter"]),
        (TuiAction::DeleteCharBackward, &["backspace"]),
        (TuiAction::AppInterrupt, &["escape"]),
        (TuiAction::AppExit, &["ctrl+d"]),
        (TuiAction::AppClear, &["ctrl+c"]),
        (TuiAction::SearchStart, &["ctrl+f"]),
        (TuiAction::SearchNext, &["ctrl+g"]),
        (TuiAction::SearchPrevious, &["ctrl+r"]),
        (TuiAction::PermissionApprove, &["alt+y"]),
        (TuiAction::PermissionDeny, &["alt+n"]),
        (TuiAction::PermissionAlwaysAllow, &["alt+shift+y"]),
        (TuiAction::PermissionAlwaysDeny, &["alt+shift+n"]),
        (TuiAction::TranscriptPageUp, &["pageUp"]),
        (TuiAction::TranscriptPageDown, &["pageDown"]),
        (TuiAction::TranscriptTop, &["home"]),
        (TuiAction::TranscriptBottom, &["end"]),
        (TuiAction::TranscriptLineUp, &["alt+up"]),
        (TuiAction::TranscriptLineDown, &["alt+down"]),
        (TuiAction::SelectUp, &["up", "k"]),
        (TuiAction::SelectDown, &["down", "j"]),
        (TuiAction::SelectConfirm, &["enter"]),
        (TuiAction::SelectCancel, &["escape", "ctrl+c"]),
    ]
}

fn parse_default_keybindings(action: TuiAction, keys: &[&'static str]) -> Vec<KeyBinding> {
    keys.iter()
        .map(|key| {
            parse_key_binding(key).unwrap_or_else(|error| {
                panic!("invalid default keybinding {}={key}: {error}", action.id())
            })
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

fn key_code_name(code: KeyCode) -> String {
    match code {
        KeyCode::Backspace => "backspace".to_string(),
        KeyCode::Enter => "enter".to_string(),
        KeyCode::Left => "left".to_string(),
        KeyCode::Right => "right".to_string(),
        KeyCode::Up => "up".to_string(),
        KeyCode::Down => "down".to_string(),
        KeyCode::Home => "home".to_string(),
        KeyCode::End => "end".to_string(),
        KeyCode::PageUp => "pageUp".to_string(),
        KeyCode::PageDown => "pageDown".to_string(),
        KeyCode::Tab => "tab".to_string(),
        KeyCode::BackTab => "shift+tab".to_string(),
        KeyCode::Delete => "delete".to_string(),
        KeyCode::Insert => "insert".to_string(),
        KeyCode::F(number) => format!("f{number}"),
        KeyCode::Char(' ') => "space".to_string(),
        KeyCode::Char(character) => character.to_string(),
        KeyCode::Null => "null".to_string(),
        KeyCode::Esc => "escape".to_string(),
        KeyCode::CapsLock => "capsLock".to_string(),
        KeyCode::ScrollLock => "scrollLock".to_string(),
        KeyCode::NumLock => "numLock".to_string(),
        KeyCode::PrintScreen => "printScreen".to_string(),
        KeyCode::Pause => "pause".to_string(),
        KeyCode::Menu => "menu".to_string(),
        KeyCode::KeypadBegin => "keypadBegin".to_string(),
        KeyCode::Media(media_key) => format!("media:{media_key:?}"),
        KeyCode::Modifier(modifier_key) => format!("modifier:{modifier_key:?}"),
    }
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
            if let Some(action) = keymap.action_for_key(&key) {
                if handle_chat_action(&client, &mut app, session_id, action).await {
                    break;
                }
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

async fn handle_chat_action(
    client: &BcodeClient,
    app: &mut ChatApp,
    session_id: SessionId,
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
        TuiAction::SelectUp
        | TuiAction::SelectDown
        | TuiAction::SelectConfirm
        | TuiAction::SelectCancel => {}
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
        match keymap.action_for_key(&key) {
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

#[derive(Debug)]
struct ChatApp {
    session_id: SessionId,
    lines: Vec<String>,
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
}

#[derive(Debug, Clone)]
struct PendingPermissionView {
    permission_id: String,
    tool_call_id: String,
    tool_name: String,
    arguments_json: String,
}

impl PendingPermissionView {
    fn render_text(&self, key_hints: &str) -> String {
        format!(
            "permission: {}\ntool: {} ({})\narguments: {}\n{}",
            self.permission_id, self.tool_name, self.tool_call_id, self.arguments_json, key_hints
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

const STREAMING_ASSISTANT_PREFIX: &str = "assistant (streaming): ";
const FINAL_ASSISTANT_PREFIX: &str = "assistant: ";

impl ChatApp {
    fn new(session_id: SessionId, history: &[SessionEvent], keymap: &KeyMap) -> Self {
        let mut app = Self {
            session_id,
            lines: Vec::new(),
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
            permission_hints: format!(
                "{} approve once, {} deny once, {} always allow, {} always deny",
                keymap.primary(TuiAction::PermissionApprove),
                keymap.primary(TuiAction::PermissionDeny),
                keymap.primary(TuiAction::PermissionAlwaysAllow),
                keymap.primary(TuiAction::PermissionAlwaysDeny),
            ),
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
        let current = self.top_visible_line_index();
        self.lines
            .iter()
            .enumerate()
            .skip(current.saturating_add(1))
            .chain(
                self.lines
                    .iter()
                    .enumerate()
                    .take(current.saturating_add(1)),
            )
            .find_map(|(index, line)| line.contains(&self.search_query).then_some(index))
    }

    fn previous_match_index(&self) -> Option<usize> {
        let current = self.top_visible_line_index();
        self.lines
            .iter()
            .enumerate()
            .take(current)
            .rev()
            .chain(self.lines.iter().enumerate().skip(current).rev())
            .find_map(|(index, line)| line.contains(&self.search_query).then_some(index))
    }

    fn top_visible_line_index(&self) -> usize {
        self.lines
            .len()
            .saturating_sub(self.scroll_from_bottom)
            .saturating_sub(1)
    }

    fn scroll_to_line(&mut self, index: usize) {
        self.scroll_from_bottom = self.lines.len().saturating_sub(index.saturating_add(1));
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
        self.scroll_from_bottom = self.lines.len();
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
        self.lines.push(format_session_event(event));
        self.clamp_scroll();
    }

    fn push_assistant_delta(&mut self, text: &str) {
        if let Some(last) = self
            .lines
            .last_mut()
            .filter(|line| line.starts_with(STREAMING_ASSISTANT_PREFIX))
        {
            last.push_str(text);
        } else {
            self.lines
                .push(format!("{STREAMING_ASSISTANT_PREFIX}{text}"));
        }
        self.clamp_scroll();
    }

    fn finish_assistant_message(&mut self, text: &str) {
        let final_message = format!("{FINAL_ASSISTANT_PREFIX}{text}");
        if let Some(last) = self
            .lines
            .last_mut()
            .filter(|line| line.starts_with(STREAMING_ASSISTANT_PREFIX))
        {
            *last = final_message;
        } else {
            self.lines.push(final_message);
        }
        self.clamp_scroll();
    }

    fn clamp_scroll(&mut self) {
        self.scroll_from_bottom = self.scroll_from_bottom.min(self.lines.len());
    }

    fn remove_pending_permission(&mut self, permission_id: &str) {
        self.pending_permissions.remove(permission_id);
    }

    fn first_pending_permission_id(&self) -> Option<String> {
        self.pending_permissions.keys().next().cloned()
    }

    fn first_pending_permission(&self) -> Option<&PendingPermissionView> {
        self.pending_permissions.values().next()
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
                Constraint::Length(6),
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
            .unwrap_or("<auto>");
        let model = self.selected_model_id.as_deref().unwrap_or("<default>");
        let header = Paragraph::new(format!(
            "Bcode session {} | provider: {provider} | model: {model}",
            self.session_id
        ));
        header.render(chunks[0], buf);

        let transcript_height = usize::from(chunks[1].height.saturating_sub(2));
        let visible_end = self
            .lines
            .len()
            .saturating_sub(self.scroll_from_bottom)
            .max(transcript_height.min(self.lines.len()));
        let start = visible_end.saturating_sub(transcript_height);
        let title = if self.scroll_from_bottom == 0 {
            "Transcript".to_string()
        } else {
            format!("Transcript ({} from bottom)", self.scroll_from_bottom)
        };
        let transcript_lines = self.lines[start..visible_end]
            .iter()
            .map(|line| render_transcript_line(line, &self.search_query))
            .collect::<Vec<_>>();
        let transcript = Paragraph::new(Text::from(transcript_lines))
            .block(Block::new().title(title).borders(Borders::ALL))
            .wrap(Wrap { trim: false });
        transcript.render(chunks[1], buf);

        let input_index = self.first_pending_permission().map_or(2, |permission| {
            let permission = Paragraph::new(permission.render_text(&self.permission_hints))
                .block(
                    Block::new()
                        .title("Permission Required")
                        .borders(Borders::ALL),
                )
                .wrap(Wrap { trim: false });
            permission.render(chunks[2], buf);
            3
        });

        let input_title = if self.search_mode { "Search" } else { "Input" };
        let input = Paragraph::new(self.input.as_str())
            .block(Block::new().title(input_title).borders(Borders::ALL))
            .wrap(Wrap { trim: false });
        input.render(chunks[input_index], buf);

        let status = format!("{} | {}", self.status, self.key_hints);
        Paragraph::new(status).render(chunks[input_index + 1], buf);
    }
}

fn render_transcript_line(line: &str, search_query: &str) -> Line<'static> {
    let (prefix, body, prefix_style, body_style) = classify_transcript_line(line);
    let mut spans = Vec::new();
    if !prefix.is_empty() {
        push_highlighted_spans(&mut spans, prefix, search_query, prefix_style);
    }
    if !body.is_empty() {
        push_highlighted_spans(&mut spans, body, search_query, body_style);
    }
    Line::from(spans)
}

fn classify_transcript_line(line: &str) -> (&str, &str, Style, Style) {
    let assistant_style = Style::default()
        .fg(Color::Green)
        .add_modifier(Modifier::BOLD);
    let streaming_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let tool_style = Style::default().fg(Color::Yellow);
    let permission_style = Style::default().fg(Color::Red).add_modifier(Modifier::BOLD);
    let metadata_style = Style::default().fg(Color::DarkGray);
    let body_style = Style::default();

    for (prefix, style) in [
        (STREAMING_ASSISTANT_PREFIX, streaming_style),
        (FINAL_ASSISTANT_PREFIX, assistant_style),
        ("↳ tool requested: ", tool_style),
        ("↳ tool result", tool_style),
        ("⚠ permission requested: ", permission_style),
        ("permission resolved: ", permission_style),
    ] {
        if let Some(body) = line.strip_prefix(prefix) {
            return (prefix, body, style, body_style);
        }
    }

    if line.starts_with('#') {
        (line, "", metadata_style, body_style)
    } else {
        ("", line, body_style, body_style)
    }
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

fn format_session_event(event: &SessionEvent) -> String {
    match &event.kind {
        SessionEventKind::SessionCreated { name } => {
            let name = name.as_deref().unwrap_or("<unnamed>");
            format!("#{} session created: {name}", event.sequence)
        }
        SessionEventKind::ClientAttached { client_id } => {
            format!("#{} client attached: {client_id}", event.sequence)
        }
        SessionEventKind::ClientDetached { client_id } => {
            format!("#{} client detached: {client_id}", event.sequence)
        }
        SessionEventKind::UserMessage { client_id, text } => {
            format!("#{} {client_id}: {text}", event.sequence)
        }
        SessionEventKind::AssistantDelta { text } => {
            format!("{STREAMING_ASSISTANT_PREFIX}{text}")
        }
        SessionEventKind::AssistantMessage { text } => {
            format!("{FINAL_ASSISTANT_PREFIX}{text}")
        }
        SessionEventKind::ToolCallRequested {
            tool_call_id,
            tool_name,
            arguments_json,
        } => format!("↳ tool requested: {tool_name} ({tool_call_id}) {arguments_json}"),
        SessionEventKind::ToolCallFinished {
            tool_call_id,
            result,
            is_error,
        } => {
            let status = if *is_error { "error" } else { "ok" };
            format!("↳ tool result ({status}) for {tool_call_id}:\n{result}")
        }
        SessionEventKind::PermissionRequested {
            permission_id,
            tool_call_id,
            tool_name,
            arguments_json,
        } => format!(
            "⚠ permission requested: {permission_id} {tool_name} ({tool_call_id}) {arguments_json}"
        ),
        SessionEventKind::PermissionResolved {
            permission_id,
            approved,
        } => format!("permission resolved: {permission_id} approved={approved}"),
        SessionEventKind::ModelChanged { provider, model } => {
            format!("#{} model changed: {provider}/{model}", event.sequence)
        }
        SessionEventKind::SystemMessage { text } => format!("#{} system: {text}", event.sequence),
    }
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
            keybindings: [
                (
                    "app.permission.approve".to_string(),
                    vec!["ctrl+y".to_string()],
                ),
                ("app.exit".to_string(), Vec::new()),
            ]
            .into_iter()
            .collect(),
        };
        let keymap = KeyMap::from_config(&config);

        assert_eq!(
            keymap.action_for_key(&KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL)),
            Some(TuiAction::PermissionApprove)
        );
        assert_eq!(
            keymap.action_for_key(&KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            None
        );
    }
}
