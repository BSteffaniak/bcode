#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
// Small TUI state mutation helpers are clearer as regular functions even when
// clippy can technically const-qualify them.
#![allow(clippy::missing_const_for_fn)]
// Complex palette + slash logic in TUI (UI contribution) intentionally uses patterns
// that trigger pedantic lints; kept for readability and future plugin invoke.
#![allow(
    clippy::too_many_lines,
    clippy::uninlined_format_args,
    clippy::if_same_then_else,
    clippy::map_unwrap_or,
    clippy::unused_enumerate_index,
    clippy::unused_async
)]

//! Terminal user interface for Bcode.

use bcode_client::{BcodeClient, ClientError};
use bcode_command::CommandInfo;
use bcode_ipc::Event;
use bcode_model::{ModelList, ReasoningEffort};
use bcode_session_models::{
    ModelTurnOutcome, SessionEvent, SessionEventKind, SessionHistoryCursor,
    SessionHistoryDirection, SessionHistoryQuery, SessionId, SessionSummary, SessionTokenUsage,
    SessionTracePayload, SessionTracePhase,
};
use bmux_text_edit::{TextDelete, TextEditBuffer, TextMotion};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event as CrosstermEvent, KeyCode, KeyEvent,
    KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, StatefulWidget,
    Widget, Wrap,
};
use ratatui::{Frame, Terminal};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{self, Stdout};
use std::rc::Rc;
use std::time::{Duration, Instant};
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

const TRANSCRIPT_WRAP: Wrap = Wrap { trim: false };
const DEFAULT_TRANSCRIPT_WIDTH: u16 = 80;
const DEFAULT_TRANSCRIPT_HEIGHT: u16 = 20;
const TRANSCRIPT_WINDOW_OVERSCAN_LINES: usize = 2;
const MOUSE_SCROLL_ROWS: usize = 3;
const INITIAL_HISTORY_EVENT_LIMIT: usize = 500;
const MAX_COMPOSER_ROWS: u16 = 6;
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const MODAL_MARGIN_X: u16 = 4;
const MODAL_MARGIN_Y: u16 = 2;
const COLOR_TEXT: Color = Color::Gray;
const COLOR_MUTED: Color = Color::DarkGray;
const COLOR_BORDER: Color = Color::DarkGray;
const COLOR_ACCENT: Color = Color::Cyan;
const COLOR_SUCCESS: Color = Color::Green;
const COLOR_WARNING: Color = Color::Yellow;
const COLOR_DANGER: Color = Color::Red;
const COLOR_SELECTED_BG: Color = Color::Rgb(38, 52, 64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum TuiAction {
    InputSubmit,
    InputNewLine,
    InputHistoryPrevious,
    InputHistoryNext,
    DeleteCharBackward,
    DeleteCharForward,
    DeleteWordBackward,
    DeleteWordForward,
    DeleteToStart,
    DeleteToEnd,
    MoveCursorLeft,
    MoveCursorRight,
    MoveCursorWordLeft,
    MoveCursorWordRight,
    MoveCursorStart,
    MoveCursorEnd,
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
    CommandPaletteOpen,
    CommandPaletteClose,
    CommandPaletteUp,
    CommandPaletteDown,
    CommandPaletteConfirm,
    CommandPaletteFilter,
    SessionNew,
    SessionRename,
    SessionDelete,
}

impl TuiAction {
    fn from_id(id: &str) -> Option<Self> {
        Some(match id {
            "tui.input.submit" => Self::InputSubmit,
            "tui.input.newLine" => Self::InputNewLine,
            "tui.input.historyPrevious" => Self::InputHistoryPrevious,
            "tui.input.historyNext" => Self::InputHistoryNext,
            "tui.editor.deleteCharBackward" => Self::DeleteCharBackward,
            "tui.editor.deleteCharForward" => Self::DeleteCharForward,
            "tui.editor.deleteWordBackward" => Self::DeleteWordBackward,
            "tui.editor.deleteWordForward" => Self::DeleteWordForward,
            "tui.editor.deleteToStart" => Self::DeleteToStart,
            "tui.editor.deleteToEnd" => Self::DeleteToEnd,
            "tui.editor.moveCursorLeft" => Self::MoveCursorLeft,
            "tui.editor.moveCursorRight" => Self::MoveCursorRight,
            "tui.editor.moveCursorWordLeft" => Self::MoveCursorWordLeft,
            "tui.editor.moveCursorWordRight" => Self::MoveCursorWordRight,
            "tui.editor.moveCursorStart" => Self::MoveCursorStart,
            "tui.editor.moveCursorEnd" => Self::MoveCursorEnd,
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
            "app.command_palette" => Self::CommandPaletteOpen,
            "app.command_palette.close" => Self::CommandPaletteClose,
            "tui.palette.up" | "tui.palette.previous" => Self::CommandPaletteUp,
            "tui.palette.down" | "tui.palette.next" => Self::CommandPaletteDown,
            "app.command_palette.confirm" => Self::CommandPaletteConfirm,
            "app.command_palette.filter" => Self::CommandPaletteFilter,
            "tui.session.new" => Self::SessionNew,
            "tui.session.rename" => Self::SessionRename,
            "tui.session.delete" => Self::SessionDelete,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum TuiScope {
    Chat,
    Permission,
    SessionPicker,
    CommandPalette,
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
            "{} send · {} interrupt · {} exit · {} search · {} palette",
            self.primary(TuiScope::Chat, TuiAction::InputSubmit),
            self.primary(TuiScope::Chat, TuiAction::AppInterrupt),
            self.primary(TuiScope::Chat, TuiAction::AppExit),
            self.primary(TuiScope::Chat, TuiAction::SearchStart),
            self.primary(TuiScope::Chat, TuiAction::CommandPaletteOpen),
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
                ("up", TuiAction::InputHistoryPrevious),
                ("down", TuiAction::InputHistoryNext),
                ("backspace", TuiAction::DeleteCharBackward),
                ("delete", TuiAction::DeleteCharForward),
                ("alt+backspace", TuiAction::DeleteWordBackward),
                ("ctrl+w", TuiAction::DeleteWordBackward),
                ("alt+delete", TuiAction::DeleteWordForward),
                ("ctrl+delete", TuiAction::DeleteWordForward),
                ("ctrl+u", TuiAction::DeleteToStart),
                ("ctrl+k", TuiAction::DeleteToEnd),
                ("left", TuiAction::MoveCursorLeft),
                ("right", TuiAction::MoveCursorRight),
                ("alt+left", TuiAction::MoveCursorWordLeft),
                ("alt+right", TuiAction::MoveCursorWordRight),
                ("ctrl+left", TuiAction::MoveCursorWordLeft),
                ("ctrl+right", TuiAction::MoveCursorWordRight),
                ("ctrl+a", TuiAction::MoveCursorStart),
                ("ctrl+e", TuiAction::MoveCursorEnd),
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
                ("ctrl+p", TuiAction::CommandPaletteOpen),
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
                ("n", TuiAction::SessionNew),
                ("r", TuiAction::SessionRename),
                ("d", TuiAction::SessionDelete),
                ("escape", TuiAction::SelectCancel),
                ("ctrl+c", TuiAction::SelectCancel),
            ]),
        ),
        (
            TuiScope::CommandPalette,
            action_bindings(&[
                ("up", TuiAction::CommandPaletteUp),
                ("k", TuiAction::CommandPaletteUp),
                ("down", TuiAction::CommandPaletteDown),
                ("j", TuiAction::CommandPaletteDown),
                ("left", TuiAction::MoveCursorLeft),
                ("right", TuiAction::MoveCursorRight),
                ("alt+left", TuiAction::MoveCursorWordLeft),
                ("alt+right", TuiAction::MoveCursorWordRight),
                ("ctrl+left", TuiAction::MoveCursorWordLeft),
                ("ctrl+right", TuiAction::MoveCursorWordRight),
                ("ctrl+a", TuiAction::MoveCursorStart),
                ("ctrl+e", TuiAction::MoveCursorEnd),
                ("backspace", TuiAction::DeleteCharBackward),
                ("delete", TuiAction::DeleteCharForward),
                ("alt+backspace", TuiAction::DeleteWordBackward),
                ("ctrl+w", TuiAction::DeleteWordBackward),
                ("alt+delete", TuiAction::DeleteWordForward),
                ("ctrl+delete", TuiAction::DeleteWordForward),
                ("ctrl+u", TuiAction::DeleteToStart),
                ("ctrl+k", TuiAction::DeleteToEnd),
                ("enter", TuiAction::CommandPaletteConfirm),
                ("escape", TuiAction::CommandPaletteClose),
                ("ctrl+c", TuiAction::CommandPaletteClose),
                ("ctrl+p", TuiAction::CommandPaletteClose),
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
        TuiAction::SessionNew | TuiAction::SessionRename | TuiAction::SessionDelete => {
            &[TuiScope::SessionPicker]
        }
        TuiAction::CommandPaletteOpen
        | TuiAction::CommandPaletteClose
        | TuiAction::CommandPaletteUp
        | TuiAction::CommandPaletteDown
        | TuiAction::CommandPaletteConfirm
        | TuiAction::CommandPaletteFilter => &[TuiScope::CommandPalette, TuiScope::Chat],
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
    match key.code {
        KeyCode::Char(character)
            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
        {
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
    let history = connection
        .attach_session_recent(session_id, INITIAL_HISTORY_EVENT_LIMIT)
        .await?;

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
    let model_status = client.session_model_status(session_id).await.ok();
    let mut terminal = TerminalGuard::enter()?;
    let mut app = ChatApp::new(session_id, &history, &keymap);
    if let Some(status) = status {
        app.selected_provider_plugin_id = status.selected_provider_plugin_id;
        app.selected_model_id = status.selected_model_id;
        // thinking loaded via events or future status extension
    }
    if let Some(model_status) = model_status {
        app.apply_model_status(model_status);
    }

    loop {
        while let Ok(event) = event_receiver.try_recv() {
            app.push_event(event);
        }
        if app.take_model_status_refresh_needed()
            && let Ok(model_status) = client.session_model_status(session_id).await
        {
            app.apply_model_status(model_status);
        }
        if let Some(cursor) = app.take_older_history_cursor() {
            match client
                .session_history_page(
                    session_id,
                    SessionHistoryQuery {
                        cursor: Some(cursor),
                        limit: INITIAL_HISTORY_EVENT_LIMIT,
                        direction: SessionHistoryDirection::Backward,
                    },
                )
                .await
            {
                Ok(page) => app.prepend_older_history(&page.events, page.has_more),
                Err(error) => app.status = format!("older history load failed: {error}"),
            }
        }

        terminal.draw_frame(|frame| render_chat_frame(frame, &app))?;

        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                CrosstermEvent::Key(key) => {
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
                        app.status =
                            "permission prompt active; use configured prompt bindings".to_string();
                        continue;
                    }
                    if let Some(character) = key_is_text_input(&key) {
                        if let Some(palette) = &mut app.command_palette {
                            palette.filter.insert_char(character);
                            palette.selected = 0;
                        } else {
                            app.reset_input_history_navigation();
                            app.input.insert_char(character);
                            if app.search_mode {
                                app.update_search();
                            }
                        }
                    }
                }
                CrosstermEvent::Mouse(mouse) => handle_mouse_event(&mut app, mouse),
                _ => {}
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
            app.reset_input_history_navigation();
            app.input.clear();
            app.status = "input cleared; press exit again to quit".to_string();
        }
        TuiAction::AppInterrupt => {
            if app.search_mode {
                app.cancel_search();
            } else {
                match client.cancel_session_turn(session_id).await {
                    Ok(true) => {
                        app.set_activity(ActivityState::Cancelling);
                        app.status = "turn cancellation requested".to_string();
                    }
                    Ok(false) => {
                        app.set_activity(ActivityState::Idle);
                        app.status = "no active turn".to_string();
                    }
                    Err(error) => {
                        app.set_activity(ActivityState::Idle);
                        app.status = format!("cancel failed: {error}");
                    }
                }
            }
        }
        TuiAction::AppClear => {
            app.reset_input_history_navigation();
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
            } else if let Some(message) = app.take_input() {
                if message.starts_with('/') {
                    if !handle_slash_command(client, app, session_id, &message).await {
                        app.status = format!("unknown slash command: {}", message);
                    }
                } else if let Err(error) = client.send_user_message(session_id, message).await {
                    app.set_activity(ActivityState::Idle);
                    app.status = format!("send failed: {error}");
                } else {
                    app.set_activity(ActivityState::Thinking);
                    app.status = "sent".to_string();
                }
            }
        }
        TuiAction::InputNewLine => {
            app.reset_input_history_navigation();
            app.input.insert_newline();
            if app.search_mode {
                app.update_search();
            }
        }
        TuiAction::InputHistoryPrevious => app.previous_input_history(),
        TuiAction::InputHistoryNext => app.next_input_history(),
        TuiAction::DeleteCharBackward => {
            if let Some(palette) = &mut app.command_palette {
                palette.filter.delete_backward();
                palette.selected = 0;
            } else {
                app.reset_input_history_navigation();
                app.input.delete_backward();
                if app.search_mode {
                    app.update_search();
                }
            }
        }
        TuiAction::DeleteCharForward => {
            if let Some(palette) = &mut app.command_palette {
                palette.filter.delete_forward();
                palette.selected = 0;
            } else {
                app.reset_input_history_navigation();
                app.input.delete(TextDelete::Forward);
                if app.search_mode {
                    app.update_search();
                }
            }
        }
        TuiAction::DeleteWordBackward => {
            if let Some(palette) = &mut app.command_palette {
                palette.filter.delete(TextDelete::WordBackward);
                palette.selected = 0;
            } else {
                app.reset_input_history_navigation();
                app.input.delete(TextDelete::WordBackward);
                if app.search_mode {
                    app.update_search();
                }
            }
        }
        TuiAction::DeleteWordForward => {
            if let Some(palette) = &mut app.command_palette {
                palette.filter.delete(TextDelete::WordForward);
                palette.selected = 0;
            } else {
                app.reset_input_history_navigation();
                app.input.delete(TextDelete::WordForward);
                if app.search_mode {
                    app.update_search();
                }
            }
        }
        TuiAction::DeleteToStart => {
            if let Some(palette) = &mut app.command_palette {
                palette.filter.delete(TextDelete::ToStart);
                palette.selected = 0;
            } else {
                app.reset_input_history_navigation();
                app.input.delete(TextDelete::ToStart);
                if app.search_mode {
                    app.update_search();
                }
            }
        }
        TuiAction::DeleteToEnd => {
            if let Some(palette) = &mut app.command_palette {
                palette.filter.delete(TextDelete::ToEnd);
                palette.selected = 0;
            } else {
                app.reset_input_history_navigation();
                app.input.delete(TextDelete::ToEnd);
                if app.search_mode {
                    app.update_search();
                }
            }
        }
        TuiAction::MoveCursorLeft => {
            if let Some(palette) = &mut app.command_palette {
                palette.filter.move_cursor(TextMotion::Left);
            } else {
                app.input.move_cursor(TextMotion::Left);
            }
        }
        TuiAction::MoveCursorRight => {
            if let Some(palette) = &mut app.command_palette {
                palette.filter.move_cursor(TextMotion::Right);
            } else {
                app.input.move_cursor(TextMotion::Right);
            }
        }
        TuiAction::MoveCursorWordLeft => {
            if let Some(palette) = &mut app.command_palette {
                palette.filter.move_cursor(TextMotion::WordLeft);
            } else {
                app.input.move_cursor(TextMotion::WordLeft);
            }
        }
        TuiAction::MoveCursorWordRight => {
            if let Some(palette) = &mut app.command_palette {
                palette.filter.move_cursor(TextMotion::WordRight);
            } else {
                app.input.move_cursor(TextMotion::WordRight);
            }
        }
        TuiAction::MoveCursorStart => {
            if let Some(palette) = &mut app.command_palette {
                palette.filter.move_cursor(TextMotion::Start);
            } else {
                app.input.move_cursor(TextMotion::Start);
            }
        }
        TuiAction::MoveCursorEnd => {
            if let Some(palette) = &mut app.command_palette {
                palette.filter.move_cursor(TextMotion::End);
            } else {
                app.input.move_cursor(TextMotion::End);
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
        TuiAction::CommandPaletteOpen => app.open_command_palette(),
        TuiAction::CommandPaletteClose => app.close_command_palette(),
        TuiAction::CommandPaletteUp => {
            if let Some(p) = &mut app.command_palette
                && p.selected > 0
            {
                p.selected -= 1;
            }
        }
        TuiAction::CommandPaletteDown => {
            if let Some(p) = &mut app.command_palette {
                let len = p.filtered_commands().len();
                if len > 0 && p.selected + 1 < len {
                    p.selected += 1;
                }
            }
        }
        TuiAction::CommandPaletteConfirm => {
            if let Some(cmd) = app
                .command_palette
                .as_ref()
                .and_then(|p| p.selected_command().cloned())
            {
                app.execute_command(client, &cmd).await;
            }
            app.close_command_palette();
        }
        TuiAction::CommandPaletteFilter
        | TuiAction::SessionNew
        | TuiAction::SessionRename
        | TuiAction::SessionDelete => {
            // filter/session-picker-only actions are handled outside this chat action path
        }
    }
    false
}

fn handle_mouse_event(app: &mut ChatApp, mouse: MouseEvent) {
    if app.command_palette.is_some() || app.first_pending_permission().is_some() {
        return;
    }
    if !rect_contains(app.last_transcript_area.get(), mouse.column, mouse.row) {
        return;
    }
    match mouse.kind {
        MouseEventKind::ScrollUp => app.scroll_rows_up(MOUSE_SCROLL_ROWS),
        MouseEventKind::ScrollDown => app.scroll_rows_down(MOUSE_SCROLL_ROWS),
        _ => {}
    }
}

fn rect_contains(rect: Rect, column: u16, row: u16) -> bool {
    column >= rect.x
        && column < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

async fn handle_slash_command(
    client: &BcodeClient,
    app: &mut ChatApp,
    session_id: SessionId,
    message: &str,
) -> bool {
    let parts = message.split_whitespace().collect::<Vec<_>>();
    let Some(command) = parts.first().map(|part| part.trim_start_matches('/')) else {
        return false;
    };
    match command {
        "plan" => set_session_agent_from_tui(client, app, session_id, "plan").await,
        "build" => set_session_agent_from_tui(client, app, session_id, "build").await,
        "compact" => compact_session_from_tui(client, app, session_id).await,
        "models" | "model" if parts.len() == 1 => {
            list_models_from_tui(client, app, session_id).await
        }
        "model" | "set-model" if parts.len() > 1 => {
            set_session_model_from_tui(client, app, session_id, &parts).await
        }
        "provider" | "set-provider" if parts.len() > 1 => {
            set_session_provider_from_tui(client, app, session_id, parts[1]).await
        }
        "provider" => {
            app.status = format!(
                "current provider: {}",
                app.selected_provider_plugin_id.as_deref().unwrap_or("auto")
            );
            true
        }
        "agent" if parts.len() > 1 => {
            set_session_agent_from_tui(client, app, session_id, parts[1]).await
        }
        "agent" => {
            match client.list_agents().await {
                Ok(agents) => {
                    let names = agents
                        .iter()
                        .map(|agent| agent.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let policy = client.agent_policy_status().await.map_or_else(
                        |_| "policy: unavailable".to_string(),
                        |status| format!("policy: {}", status.source),
                    );
                    app.status = format!(
                        "current agent: {}; available: {names}; {policy}",
                        app.current_agent_id
                    );
                }
                Err(error) => app.status = format!("agent list failed: {error}"),
            }
            true
        }
        _ => app.parse_and_execute_slash(message, client),
    }
}

async fn list_models_from_tui(
    client: &BcodeClient,
    app: &mut ChatApp,
    session_id: SessionId,
) -> bool {
    let provider_plugin_id = if app.selected_provider_plugin_id.is_some() {
        app.selected_provider_plugin_id.clone()
    } else {
        client
            .session_model_status(session_id)
            .await
            .ok()
            .and_then(|status| status.provider_plugin_id)
    };
    let response = if let Some(provider_plugin_id) = provider_plugin_id.clone() {
        client
            .invoke_plugin_service(
                provider_plugin_id,
                bcode_model::MODEL_PROVIDER_INTERFACE_ID.to_string(),
                bcode_model::OP_MODELS.to_string(),
                Vec::new(),
            )
            .await
    } else {
        client
            .call_plugin_service(
                bcode_model::MODEL_PROVIDER_INTERFACE_ID.to_string(),
                bcode_model::OP_MODELS.to_string(),
                Vec::new(),
            )
            .await
    };
    match response {
        Ok(response) => {
            if let Some(error) = response.error {
                app.status = format!("model list failed: {}", error.message);
                return true;
            }
            match serde_json::from_slice::<ModelList>(&response.payload) {
                Ok(models) => app.show_model_list(&models.models),
                Err(error) => app.status = format!("model list decode failed: {error}"),
            }
        }
        Err(error) => app.status = format!("model list failed: {error}"),
    }
    true
}

async fn set_session_model_from_tui(
    client: &BcodeClient,
    app: &mut ChatApp,
    session_id: SessionId,
    parts: &[&str],
) -> bool {
    let model_id = parts[1];
    let provider = slash_option_value(parts, "--provider").map(ToString::to_string);
    match client
        .set_session_model(session_id, provider.clone(), model_id.to_string())
        .await
    {
        Ok(()) => {
            if let Some(provider) = provider {
                app.selected_provider_plugin_id = Some(provider);
            }
            app.selected_model_id = (model_id != "<default>").then(|| model_id.to_string());
            app.model_status_refresh_needed = true;
            app.status = format!("model set to {model_id}");
        }
        Err(error) => app.status = format!("model switch failed: {error}"),
    }
    true
}

async fn set_session_provider_from_tui(
    client: &BcodeClient,
    app: &mut ChatApp,
    session_id: SessionId,
    provider_plugin_id: &str,
) -> bool {
    match client
        .set_session_model(
            session_id,
            Some(provider_plugin_id.to_string()),
            "<default>".to_string(),
        )
        .await
    {
        Ok(()) => {
            app.selected_provider_plugin_id = Some(provider_plugin_id.to_string());
            app.selected_model_id = None;
            app.model_status_refresh_needed = true;
            app.status =
                format!("provider set to {provider_plugin_id}; model uses provider default");
        }
        Err(error) => app.status = format!("provider switch failed: {error}"),
    }
    true
}

fn slash_option_value<'a>(parts: &'a [&str], option: &str) -> Option<&'a str> {
    parts
        .windows(2)
        .find(|window| window[0] == option)
        .map(|window| window[1])
}

async fn set_session_agent_from_tui(
    client: &BcodeClient,
    app: &mut ChatApp,
    session_id: SessionId,
    agent_id: &str,
) -> bool {
    match client
        .set_session_agent(session_id, agent_id.to_string())
        .await
    {
        Ok(()) => {
            app.current_agent_id = agent_id.to_string();
            app.status = format!("agent set to {agent_id}");
        }
        Err(error) => app.status = format!("agent switch failed: {error}"),
    }
    true
}

async fn compact_session_from_tui(
    client: &BcodeClient,
    app: &mut ChatApp,
    session_id: SessionId,
) -> bool {
    app.status = "compacting context...".to_string();
    match client.compact_session(session_id).await {
        Ok(message) => app.status = message,
        Err(error) => app.status = format!("compaction failed: {error}"),
    }
    true
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
    let agent_id = app.current_agent_id.clone();
    let rules = permission.policy_rules();
    let action = if approved { "allow" } else { "deny" };
    let mut persisted = 0usize;
    let mut last_error: Option<String> = None;
    for (category, pattern) in &rules {
        match client
            .add_permission_rule(
                agent_id.clone(),
                (*category).to_string(),
                pattern.clone(),
                action.to_string(),
            )
            .await
        {
            Ok(_) => persisted += 1,
            Err(error) => {
                last_error = Some(error.to_string());
                break;
            }
        }
    }
    if let Some(error) = last_error {
        app.status = format!("persist rule failed after {persisted} rule(s): {error}");
        return;
    }
    resolve_first_permission(client, app, approved).await;
    let rule_count = rules.len();
    let summary = rules
        .iter()
        .map(|(category, pattern)| format!("{category} {pattern:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    app.status = format!("persisted {action} rule agent={agent_id} ({rule_count}): {summary}");
}

async fn resolve_session(
    client: &BcodeClient,
    session_id: Option<SessionId>,
    keymap: &KeyMap,
) -> Result<SessionId, TuiError> {
    if let Some(session_id) = session_id {
        return Ok(session_id);
    }
    pick_session(client, keymap).await
}

async fn pick_session(client: &BcodeClient, keymap: &KeyMap) -> Result<SessionId, TuiError> {
    let sessions = client.list_sessions().await?;
    let mut terminal = TerminalGuard::enter()?;
    let mut app = SessionPickerApp::new(&sessions);
    loop {
        terminal.draw_frame(|frame| {
            frame.render_widget(&app, frame.area());
            if let Some(position) = app.cursor_position(frame.area()) {
                frame.set_cursor_position(position);
            }
        })?;
        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        let CrosstermEvent::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if handle_session_picker_text_key(client, &mut app, &key).await? {
            continue;
        }
        match keymap.action_for_key(TuiScope::SessionPicker, &key) {
            Some(TuiAction::SelectCancel) => return Err(TuiError::Canceled),
            Some(TuiAction::SelectUp) => app.previous(),
            Some(TuiAction::SelectDown) => app.next(),
            Some(TuiAction::SelectConfirm) => {
                if let Some(session_id) = app.selected_session_id() {
                    return Ok(session_id);
                }
                return Ok(client.create_session(None).await?.id);
            }
            Some(TuiAction::SessionNew) => return Ok(client.create_session(None).await?.id),
            Some(TuiAction::SessionRename) => app.start_rename(),
            Some(TuiAction::SessionDelete) => app.start_delete_confirmation(),
            _ => {}
        }
    }
}

async fn handle_session_picker_text_key(
    client: &BcodeClient,
    app: &mut SessionPickerApp,
    key: &KeyEvent,
) -> Result<bool, TuiError> {
    match app.mode.clone() {
        SessionPickerMode::Browsing => Ok(false),
        SessionPickerMode::Renaming { mut input } => match key.code {
            KeyCode::Enter => {
                let Some(session_id) = app.selected_session_id() else {
                    app.mode = SessionPickerMode::Browsing;
                    return Ok(true);
                };
                let name = input.text().trim().to_string();
                match client.rename_session(session_id, Some(name)).await {
                    Ok(session) => {
                        app.upsert_session(session);
                        app.mode = SessionPickerMode::Browsing;
                        app.status = "session renamed".to_string();
                    }
                    Err(error) => app.status = format!("rename failed: {error}"),
                }
                Ok(true)
            }
            KeyCode::Esc => {
                app.mode = SessionPickerMode::Browsing;
                app.status = "rename canceled".to_string();
                Ok(true)
            }
            KeyCode::Backspace => {
                if key.modifiers.contains(KeyModifiers::ALT) {
                    input.delete(TextDelete::WordBackward);
                } else {
                    input.delete_backward();
                }
                app.mode = SessionPickerMode::Renaming { input };
                Ok(true)
            }
            KeyCode::Delete => {
                if key.modifiers.contains(KeyModifiers::ALT)
                    || key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    input.delete(TextDelete::WordForward);
                } else {
                    input.delete_forward();
                }
                app.mode = SessionPickerMode::Renaming { input };
                Ok(true)
            }
            KeyCode::Left => {
                input.move_cursor(
                    if key.modifiers.contains(KeyModifiers::ALT)
                        || key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        TextMotion::WordLeft
                    } else {
                        TextMotion::Left
                    },
                );
                app.mode = SessionPickerMode::Renaming { input };
                Ok(true)
            }
            KeyCode::Right => {
                input.move_cursor(
                    if key.modifiers.contains(KeyModifiers::ALT)
                        || key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        TextMotion::WordRight
                    } else {
                        TextMotion::Right
                    },
                );
                app.mode = SessionPickerMode::Renaming { input };
                Ok(true)
            }
            KeyCode::Home => {
                input.move_cursor(TextMotion::Start);
                app.mode = SessionPickerMode::Renaming { input };
                Ok(true)
            }
            KeyCode::End => {
                input.move_cursor(TextMotion::End);
                app.mode = SessionPickerMode::Renaming { input };
                Ok(true)
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.move_cursor(TextMotion::Start);
                app.mode = SessionPickerMode::Renaming { input };
                Ok(true)
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.move_cursor(TextMotion::End);
                app.mode = SessionPickerMode::Renaming { input };
                Ok(true)
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.delete(TextDelete::WordBackward);
                app.mode = SessionPickerMode::Renaming { input };
                Ok(true)
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.delete(TextDelete::ToStart);
                app.mode = SessionPickerMode::Renaming { input };
                Ok(true)
            }
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.delete(TextDelete::ToEnd);
                app.mode = SessionPickerMode::Renaming { input };
                Ok(true)
            }
            _ => {
                if let Some(character) = key_is_text_input(key) {
                    input.insert_char(character);
                    app.mode = SessionPickerMode::Renaming { input };
                }
                Ok(true)
            }
        },
        SessionPickerMode::ConfirmDelete => match key.code {
            KeyCode::Enter | KeyCode::Char('y' | 'Y') => {
                let Some(session_id) = app.selected_session_id() else {
                    app.mode = SessionPickerMode::Browsing;
                    return Ok(true);
                };
                match client.delete_session(session_id).await {
                    Ok(session) => {
                        app.remove_session(session.id);
                        app.mode = SessionPickerMode::Browsing;
                        app.status = "session deleted".to_string();
                    }
                    Err(error) => {
                        app.mode = SessionPickerMode::Browsing;
                        app.status = format!("delete failed: {error}");
                    }
                }
                Ok(true)
            }
            KeyCode::Esc | KeyCode::Char('n' | 'N') => {
                app.mode = SessionPickerMode::Browsing;
                app.status = "delete canceled".to_string();
                Ok(true)
            }
            _ => Ok(true),
        },
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionPickerMode {
    Browsing,
    Renaming { input: TextEditBuffer },
    ConfirmDelete,
}

#[derive(Debug)]
struct SessionPickerApp {
    sessions: Vec<SessionSummary>,
    selected: usize,
    mode: SessionPickerMode,
    status: String,
}

impl SessionPickerApp {
    fn new(sessions: &[SessionSummary]) -> Self {
        Self {
            sessions: sessions.to_vec(),
            selected: 0,
            mode: SessionPickerMode::Browsing,
            status: "choose a session or create a new one".to_string(),
        }
    }

    fn total_rows(&self) -> usize {
        self.sessions.len() + 1
    }

    fn next(&mut self) {
        self.selected = (self.selected + 1) % self.total_rows();
    }

    fn previous(&mut self) {
        self.selected = self
            .selected
            .checked_sub(1)
            .unwrap_or_else(|| self.total_rows() - 1);
    }

    fn selected_existing_index(&self) -> Option<usize> {
        self.selected.checked_sub(1)
    }

    fn selected_session(&self) -> Option<&SessionSummary> {
        self.selected_existing_index()
            .and_then(|index| self.sessions.get(index))
    }

    fn selected_session_id(&self) -> Option<SessionId> {
        self.selected_session().map(|session| session.id)
    }

    fn start_rename(&mut self) {
        let Some(session) = self.selected_session() else {
            self.status = "select an existing session to rename".to_string();
            return;
        };
        self.mode = SessionPickerMode::Renaming {
            input: TextEditBuffer::from_text(session.name.clone().unwrap_or_default()),
        };
        self.status = "type a new title and press enter".to_string();
    }

    fn cursor_position(&self, area: Rect) -> Option<Position> {
        let SessionPickerMode::Renaming { input } = &self.mode else {
            return None;
        };
        let chunks = session_picker_layout(
            area,
            matches!(self.mode, SessionPickerMode::Renaming { .. }),
        );
        let input_area = chunks[2];
        if input_area.width == 0 || input_area.height == 0 {
            return None;
        }
        let column = line_width(&input.text()[..input.cursor_byte_index()]);
        Some(Position::new(
            input_area.x + usize_to_u16_saturating(column).min(input_area.width.saturating_sub(1)),
            input_area.y,
        ))
    }

    fn start_delete_confirmation(&mut self) {
        if self.selected_session().is_none() {
            self.status = "select an existing session to delete".to_string();
            return;
        }
        self.mode = SessionPickerMode::ConfirmDelete;
        self.status =
            "delete selected session? press y/enter to confirm, esc/n to cancel".to_string();
    }

    fn upsert_session(&mut self, session: SessionSummary) {
        if let Some(existing) = self
            .sessions
            .iter_mut()
            .find(|existing| existing.id == session.id)
        {
            *existing = session;
        } else {
            self.sessions.push(session);
        }
    }

    fn remove_session(&mut self, session_id: SessionId) {
        self.sessions.retain(|session| session.id != session_id);
        if self.selected >= self.total_rows() {
            self.selected = self.total_rows().saturating_sub(1);
        }
    }
}

fn session_picker_layout(area: Rect, renaming: bool) -> Rc<[Rect]> {
    let panel = centered_rect(area, 70, 70);
    let inner = inset(panel, 2, 1);
    let rename_rows = if renaming { 2 } else { 0 };
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(rename_rows),
            Constraint::Length(1),
        ])
        .split(inner)
}

impl ratatui::widgets::Widget for &SessionPickerApp {
    fn render(self, area: Rect, buf: &mut ratatui::buffer::Buffer) {
        let panel = centered_rect(area, area.width.min(96), area.height.min(26));
        ratatui::widgets::Widget::render(Clear, panel, buf);
        let block = Block::new()
            .title(Line::from(vec![
                Span::styled(" bcode ", accent_bold_style()),
                Span::styled("sessions ", muted_style()),
            ]))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(border_style());
        ratatui::widgets::Widget::render(block, panel, buf);

        let chunks = session_picker_layout(
            area,
            matches!(self.mode, SessionPickerMode::Renaming { .. }),
        );

        Paragraph::new(Text::from(vec![
            Line::from(vec![Span::styled("Select a session", title_style())]),
            Line::from(vec![Span::styled(
                "Friendly titles are shown first; UUIDs remain available for raw identification",
                muted_style(),
            )]),
        ]))
        .render(chunks[0], buf);

        let mut items = vec![ListItem::new(Line::from(vec![
            Span::styled("+ ", accent_style()),
            Span::styled("New session", normal_style()),
            Span::styled("  starts untitled; first prompt names it", muted_style()),
        ]))];
        items.extend(self.sessions.iter().map(|session| {
            let title = session.name.as_deref().unwrap_or("Untitled session");
            let id = truncate_middle(&session.id.to_string(), 12);
            ListItem::new(Line::from(vec![
                Span::styled(truncate_end(title, 46), normal_style()),
                Span::raw("  "),
                Span::styled(id, muted_style()),
                Span::raw("  "),
                Span::styled(format!("{} clients", session.client_count), muted_style()),
            ]))
        }));
        let list = List::new(items)
            .highlight_symbol("› ")
            .highlight_style(selected_style());
        let mut state = ListState::default().with_selected(Some(self.selected));
        StatefulWidget::render(list, chunks[1], buf, &mut state);

        if let SessionPickerMode::Renaming { input } = &self.mode {
            let rename_block = Block::new()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(border_style())
                .title(Span::styled(" rename ", muted_style()));
            Paragraph::new(input.text().to_string())
                .style(normal_style())
                .block(rename_block)
                .render(chunks[2], buf);
        }

        let hint = match self.mode {
            SessionPickerMode::Browsing => Line::from(vec![
                Span::styled("enter", key_style()),
                Span::styled(" open · ", muted_style()),
                Span::styled("n", key_style()),
                Span::styled(" new · ", muted_style()),
                Span::styled("r", key_style()),
                Span::styled(" rename · ", muted_style()),
                Span::styled("d", key_style()),
                Span::styled(" delete · ", muted_style()),
                Span::styled("esc", key_style()),
                Span::styled(" quit", muted_style()),
                Span::styled(format!("  ·  {}", self.status), status_style(&self.status)),
            ]),
            SessionPickerMode::Renaming { .. } => Line::from(vec![
                Span::styled("enter", key_style()),
                Span::styled(" save · ", muted_style()),
                Span::styled("backspace", key_style()),
                Span::styled(" edit · ", muted_style()),
                Span::styled("esc", key_style()),
                Span::styled(" cancel", muted_style()),
            ]),
            SessionPickerMode::ConfirmDelete => Line::from(vec![
                Span::styled("y/enter", key_style()),
                Span::styled(" delete · ", muted_style()),
                Span::styled("n/esc", key_style()),
                Span::styled(" cancel", muted_style()),
                Span::styled(format!("  ·  {}", self.status), status_style(&self.status)),
            ]),
        };
        Paragraph::new(hint).render(chunks[3], buf);
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum ActivityState {
    Idle,
    Thinking,
    Compacting { detail: String },
    Streaming { chars: usize },
    RunningTool { name: String },
    WaitingPermission { name: String },
    Cancelling,
}

impl ActivityState {
    const fn is_idle(&self) -> bool {
        matches!(self, Self::Idle)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OlderHistoryState {
    More,
    LoadRequested { reveal_rows: usize },
    Exhausted,
}

#[derive(Debug)]
struct ChatApp {
    session_id: SessionId,
    session_title: Option<String>,
    blocks: Vec<TranscriptBlock>,
    input: TextEditBuffer,
    input_history: Vec<String>,
    input_history_index: Option<usize>,
    input_history_draft: Option<String>,
    status: String,
    pending_permissions: BTreeMap<String, PendingPermissionView>,
    selected_provider_plugin_id: Option<String>,
    selected_model_id: Option<String>,
    token_usage: TokenUsageMeter,
    model_status_refresh_needed: bool,
    current_agent_id: String,
    current_thinking_level: Option<ReasoningEffort>,
    activity: ActivityState,
    activity_started_at: Option<Instant>,
    render_tick: Cell<u64>,
    transcript_revision: Cell<u64>,
    transcript_cache: RefCell<TranscriptRenderCache>,
    scroll_rows_from_bottom: usize,
    last_transcript_width: Cell<u16>,
    last_transcript_height: Cell<u16>,
    last_transcript_area: Cell<Rect>,
    search_mode: bool,
    search_query: String,
    key_hints: String,
    permission_hints: String,
    selected_permission_choice: PermissionChoice,
    command_palette: Option<CommandPaletteState>,
    oldest_loaded_sequence: Option<u64>,
    older_history_state: OlderHistoryState,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TokenUsageMeter {
    session_tokens: u64,
    latest_context_input_tokens: Option<u32>,
    latest_cached_input_tokens: Option<u32>,
    latest_cache_write_input_tokens: Option<u32>,
    context_window: Option<u32>,
    max_output_tokens: Option<u32>,
}

impl TokenUsageMeter {
    fn absorb(&mut self, usage: &SessionTokenUsage) {
        if let Some(tokens) = usage.metered_total_tokens() {
            self.session_tokens = self.session_tokens.saturating_add(u64::from(tokens));
        }
        if let Some(input_tokens) = usage.context_input_tokens() {
            self.latest_context_input_tokens = Some(input_tokens);
        }
        if usage.cached_input_tokens.is_some() {
            self.latest_cached_input_tokens = usage.cached_input_tokens;
        }
        if usage.cache_write_input_tokens.is_some() {
            self.latest_cache_write_input_tokens = usage.cache_write_input_tokens;
        }
    }

    fn apply_model_info(&mut self, model: Option<&bcode_model::ModelInfo>) {
        if let Some(model) = model {
            self.context_window = model.context_window;
            self.max_output_tokens = model.max_output_tokens;
        }
    }

    fn footer_summary(&self) -> String {
        let mut parts = vec![self.context_summary()];
        if let Some(cached) = self.latest_cached_input_tokens
            && cached > 0
        {
            parts.push(format!("cached {} tok", compact_u64(u64::from(cached))));
        }
        if let Some(written) = self.latest_cache_write_input_tokens
            && written > 0
        {
            parts.push(format!(
                "cache write {} tok",
                compact_u64(u64::from(written))
            ));
        }
        parts.push(format!("spent {} tok", compact_u64(self.session_tokens)));
        parts.join(" · ")
    }

    fn context_summary(&self) -> String {
        if let Some(window) = self.context_window
            && window > 0
        {
            let input = self.latest_context_input_tokens.unwrap_or_default();
            return format!(
                "ctx {}/{} {}%",
                compact_u64(u64::from(input)),
                compact_u64(u64::from(window)),
                context_window_percentage(input, window)
            );
        }
        "ctx unknown".to_string()
    }
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
struct CommandPaletteState {
    filter: TextEditBuffer,
    selected: usize,
    commands: Vec<CommandInfo>,
    is_loading: bool,
}

impl CommandPaletteState {
    fn new() -> Self {
        Self {
            filter: TextEditBuffer::new(),
            selected: 0,
            commands: Vec::new(),
            is_loading: true,
        }
    }

    fn filtered_commands(&self) -> Vec<&CommandInfo> {
        let filter = self.filter.text();
        if filter.is_empty() {
            return self.commands.iter().collect();
        }
        let filter = filter.to_lowercase();
        self.commands
            .iter()
            .filter(|c| {
                c.name.to_lowercase().contains(&filter)
                    || c.id.to_lowercase().contains(&filter)
                    || c.description
                        .as_deref()
                        .unwrap_or("")
                        .to_lowercase()
                        .contains(&filter)
            })
            .collect()
    }

    fn selected_command(&self) -> Option<&CommandInfo> {
        let filtered = self.filtered_commands();
        filtered.get(self.selected).copied()
    }
}

#[derive(Debug, Clone)]
struct PendingPermissionView {
    permission_id: String,
    tool_call_id: String,
    tool_name: String,
    arguments_json: String,
}

/// Return a broadened glob prefix for a shell command.
///
/// Extracts the leading command word (typically the program name) so
/// `persist_first_permission_rule` can persist an additional rule like
/// `<prefix> *` that covers variations of the same command with different
/// arguments. Returns `None` for empty inputs.
fn shell_command_broadened_glob(command: &str) -> Option<String> {
    let trimmed = command.trim_start();
    if trimmed.is_empty() {
        return None;
    }
    let first_word: String = trimmed
        .chars()
        .take_while(|char| !char.is_whitespace())
        .collect();
    if first_word.is_empty() {
        None
    } else {
        Some(first_word)
    }
}

impl PendingPermissionView {
    /// Return `(category, pattern)` pairs to persist as rules for this permission.
    ///
    /// The action (`allow` / `deny`) is supplied separately by the caller based on
    /// whether the user approved the prompt.
    ///
    /// For `shell.run`, this returns both the literal bash command and a
    /// broadened `<first-word> *` glob so that variations of the same command
    /// (for example `echo hi` after approving `echo hello`) match without
    /// re-prompting. The literal form covers the bare-command case (for
    /// example `ls` with no arguments) that a trailing-`*` glob would not
    /// match on its own.
    ///
    /// For filesystem tools, this returns a single rule with the literal path
    /// argument. Broadening paths is left to the user editing `bcode.toml`
    /// because implicit directory globs can grant unintended access.
    fn policy_rules(&self) -> Vec<(&'static str, String)> {
        if self.tool_name == "shell.run"
            && let Some(command) = self.string_argument("command")
        {
            let trimmed = command.trim();
            if !trimmed.is_empty() {
                let mut rules = vec![("bash", trimmed.to_string())];
                if let Some(prefix) = shell_command_broadened_glob(trimmed) {
                    let broadened = format!("{prefix} *");
                    if broadened != trimmed {
                        rules.push(("bash", broadened));
                    }
                }
                return rules;
            }
        }
        match self.tool_name.as_str() {
            "filesystem.write" => {
                if let Some(path) = self.string_argument("path") {
                    return vec![("write", path)];
                }
            }
            "filesystem.edit" => {
                if let Some(path) = self.string_argument("path") {
                    return vec![("edit", path)];
                }
            }
            "filesystem.read" | "filesystem.list" | "filesystem.find" | "filesystem.grep"
            | "filesystem.stat" | "filesystem.exists" => {
                if let Some(path) = self.string_argument("path") {
                    return vec![("read", path)];
                }
            }
            _ => {}
        }
        vec![("bash", self.tool_name.clone())]
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
            session_title: None,
            blocks: Vec::new(),
            input: TextEditBuffer::new(),
            input_history: Vec::new(),
            input_history_index: None,
            input_history_draft: None,
            status: keymap
                .warnings
                .first()
                .cloned()
                .unwrap_or_else(|| "ready".to_string()),
            pending_permissions: BTreeMap::new(),
            selected_provider_plugin_id: None,
            selected_model_id: None,
            token_usage: TokenUsageMeter::default(),
            model_status_refresh_needed: false,
            current_agent_id: "build".to_string(),
            current_thinking_level: None,
            activity: ActivityState::Idle,
            activity_started_at: None,
            render_tick: Cell::new(0),
            transcript_revision: Cell::new(0),
            transcript_cache: RefCell::new(TranscriptRenderCache::default()),
            scroll_rows_from_bottom: 0,
            last_transcript_width: Cell::new(DEFAULT_TRANSCRIPT_WIDTH),
            last_transcript_height: Cell::new(DEFAULT_TRANSCRIPT_HEIGHT),
            last_transcript_area: Cell::new(Rect::new(
                0,
                0,
                DEFAULT_TRANSCRIPT_WIDTH,
                DEFAULT_TRANSCRIPT_HEIGHT,
            )),
            search_mode: false,
            search_query: String::new(),
            key_hints: keymap.chat_hints(),
            permission_hints: keymap.permission_hints(),
            selected_permission_choice: PermissionChoice::AllowOnce,
            command_palette: None,
            oldest_loaded_sequence: history.iter().map(|event| event.sequence).min(),
            older_history_state: if history.len() >= INITIAL_HISTORY_EVENT_LIMIT {
                OlderHistoryState::More
            } else {
                OlderHistoryState::Exhausted
            },
        };
        app.absorb_history(history);
        app
    }

    fn push_event(&mut self, event: Event) {
        match event {
            Event::Session(event) => {
                self.update_activity_from_live_event(&event);
                self.absorb_session_event(&event);
            }
        }
    }

    fn apply_model_status(&mut self, status: bcode_ipc::SessionModelStatus) {
        if status.provider_plugin_id.is_some() {
            self.selected_provider_plugin_id = status.provider_plugin_id;
        }
        if status.model_id.is_some() {
            self.selected_model_id = status.model_id;
        }
        self.token_usage.apply_model_info(status.model.as_ref());
        self.model_status_refresh_needed = false;
    }

    fn show_model_list(&mut self, models: &[bcode_model::ModelInfo]) {
        if models.is_empty() {
            self.status = "no models returned by provider".to_string();
            return;
        }
        let mut lines = vec!["Available models:".to_string()];
        lines.extend(models.iter().take(40).map(|model| {
            let marker = if model.is_default { "*" } else { " " };
            format!("{marker} {}", model.model_id)
        }));
        if models.len() > 40 {
            lines.push(format!("… {} more", models.len() - 40));
        }
        lines.push("Use /model <id> [--provider <plugin-id>] to switch.".to_string());
        self.finish_streaming_block_if_needed();
        self.blocks.push(TranscriptBlock::System {
            text: lines.join("\n"),
        });
        self.mark_transcript_dirty();
        self.scroll_rows_from_bottom = 0;
        self.status = format!("listed {} models", models.len());
    }

    fn take_model_status_refresh_needed(&mut self) -> bool {
        let needed = self.model_status_refresh_needed;
        self.model_status_refresh_needed = false;
        needed
    }

    fn set_activity(&mut self, activity: ActivityState) {
        if self.activity == activity {
            return;
        }
        self.activity = activity;
        self.activity_started_at = (!self.activity.is_idle()).then(Instant::now);
    }

    fn update_activity_from_live_event(&mut self, event: &SessionEvent) {
        match &event.kind {
            SessionEventKind::UserMessage { .. }
            | SessionEventKind::ToolCallFinished { .. }
            | SessionEventKind::ModelTurnStarted { .. } => {
                self.set_activity(ActivityState::Thinking);
            }
            SessionEventKind::AssistantDelta { text } => {
                let delta_chars = text.chars().count();
                if let ActivityState::Streaming { chars } = &mut self.activity {
                    *chars = chars.saturating_add(delta_chars);
                } else {
                    self.set_activity(ActivityState::Streaming { chars: delta_chars });
                }
            }
            SessionEventKind::ToolCallRequested { tool_name, .. } => {
                self.set_activity(ActivityState::RunningTool {
                    name: tool_name.clone(),
                });
            }
            SessionEventKind::PermissionRequested { tool_name, .. } => {
                self.set_activity(ActivityState::WaitingPermission {
                    name: tool_name.clone(),
                });
            }
            SessionEventKind::PermissionResolved {
                permission_id,
                approved,
            } => {
                let tool_name = self
                    .pending_permissions
                    .get(permission_id)
                    .map(|permission| permission.tool_name.clone());
                if *approved {
                    self.set_activity(ActivityState::RunningTool {
                        name: tool_name.unwrap_or_else(|| "tool".to_string()),
                    });
                } else {
                    self.set_activity(ActivityState::Thinking);
                }
            }
            SessionEventKind::ModelTurnFinished {
                outcome, message, ..
            } => {
                self.set_activity(ActivityState::Idle);
                self.status = message
                    .clone()
                    .unwrap_or_else(|| model_turn_outcome_label(*outcome).to_string());
            }
            SessionEventKind::TraceEvent { trace } => {
                self.update_activity_from_trace(trace);
            }
            SessionEventKind::ContextCompacted { .. } => {
                if matches!(self.activity, ActivityState::Compacting { .. }) {
                    self.set_activity(ActivityState::Thinking);
                    self.status = "compaction complete; retrying model turn".to_string();
                }
            }
            SessionEventKind::SessionCreated { name }
            | SessionEventKind::SessionRenamed { name } => {
                self.session_title.clone_from(name);
            }
            SessionEventKind::ClientAttached { .. }
            | SessionEventKind::ClientDetached { .. }
            | SessionEventKind::AssistantMessage { .. }
            | SessionEventKind::ModelChanged { .. }
            | SessionEventKind::AgentChanged { .. }
            | SessionEventKind::SystemMessage { .. }
            | SessionEventKind::ModelUsage { .. } => {}
        }
    }

    fn update_activity_from_trace(&mut self, trace: &bcode_session_models::SessionTraceEvent) {
        let SessionTracePayload::ContextCompaction {
            reason,
            compacted,
            message,
            ..
        } = &trace.payload
        else {
            return;
        };

        match trace.phase {
            SessionTracePhase::ContextCompactionStarted => {
                let detail = message
                    .clone()
                    .unwrap_or_else(|| format!("context compaction · {reason}"));
                self.set_activity(ActivityState::Compacting {
                    detail: detail.clone(),
                });
                self.status = detail;
            }
            SessionTracePhase::ContextCompactionFinished => {
                let detail = message
                    .clone()
                    .unwrap_or_else(|| "context compaction finished".to_string());
                self.status = detail;
                if *compacted {
                    self.set_activity(ActivityState::Thinking);
                }
            }
            SessionTracePhase::ContextCompactionSkipped => {
                if matches!(self.activity, ActivityState::Compacting { .. }) {
                    self.set_activity(ActivityState::Thinking);
                }
                if let Some(message) = message {
                    self.status.clone_from(message);
                }
            }
            _ => {}
        }
    }

    fn start_search(&mut self) {
        self.reset_input_history_navigation();
        self.search_mode = true;
        self.search_query.clear();
        self.input.clear();
        self.status = "search: type query, submit accepts, next/previous jump".to_string();
    }

    fn finish_search(&mut self) {
        self.reset_input_history_navigation();
        self.search_mode = false;
        self.search_query = self.input.text().to_string();
        self.input.clear();
        self.find_next();
    }

    fn cancel_search(&mut self) {
        self.reset_input_history_navigation();
        self.search_mode = false;
        self.input.clear();
        self.status = "search canceled".to_string();
    }

    fn update_search(&mut self) {
        self.search_query = self.input.text().to_string();
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
        self.with_transcript_snapshot(self.last_transcript_width.get(), |snapshot| {
            let Some(last_metric) = snapshot.metrics.last() else {
                return 0;
            };
            let viewport_rows = usize::from(self.last_transcript_height.get());
            let max_scroll_rows_from_bottom = snapshot.total_rows.saturating_sub(viewport_rows);
            let effective_scroll_rows_from_bottom = self
                .scroll_rows_from_bottom
                .min(max_scroll_rows_from_bottom);
            let top_visual_row =
                max_scroll_rows_from_bottom.saturating_sub(effective_scroll_rows_from_bottom);
            snapshot
                .metrics
                .iter()
                .find(|metric| metric.end_row > top_visual_row)
                .map_or(last_metric.logical_index, |metric| metric.logical_index)
        })
    }

    fn scroll_to_line(&mut self, index: usize) {
        let scroll_rows_from_bottom =
            self.with_transcript_snapshot(self.last_transcript_width.get(), |snapshot| {
                snapshot
                    .metrics
                    .get(index)
                    .map(|metric| snapshot.total_rows.saturating_sub(metric.end_row))
            });
        if let Some(scroll_rows_from_bottom) = scroll_rows_from_bottom {
            self.scroll_rows_from_bottom = scroll_rows_from_bottom;
        }
        self.clamp_scroll();
    }

    fn scroll_rows_up(&mut self, rows: usize) {
        let max_scroll = self.max_scroll_rows_from_bottom();
        let desired = self.scroll_rows_from_bottom.saturating_add(rows);
        let reveal_rows = desired.saturating_sub(max_scroll);
        self.scroll_rows_from_bottom = desired.min(max_scroll);
        if reveal_rows > 0 && self.older_history_state == OlderHistoryState::More {
            self.older_history_state = OlderHistoryState::LoadRequested { reveal_rows };
        }
    }

    fn scroll_rows_down(&mut self, rows: usize) {
        self.scroll_rows_from_bottom = self.scroll_rows_from_bottom.saturating_sub(rows);
    }

    fn scroll_line_up(&mut self) {
        self.scroll_rows_up(1);
    }

    fn scroll_line_down(&mut self) {
        self.scroll_rows_down(1);
    }

    fn scroll_page_up(&mut self) {
        self.scroll_rows_up(self.page_scroll_rows());
    }

    fn scroll_page_down(&mut self) {
        self.scroll_rows_down(self.page_scroll_rows());
    }

    fn scroll_top(&mut self) {
        let max_scroll = self.max_scroll_rows_from_bottom();
        let reveal_rows = self.page_scroll_rows();
        self.scroll_rows_from_bottom = max_scroll;
        if self.older_history_state == OlderHistoryState::More {
            self.older_history_state = OlderHistoryState::LoadRequested { reveal_rows };
        }
    }

    fn scroll_bottom(&mut self) {
        self.scroll_rows_from_bottom = 0;
    }

    fn page_scroll_rows(&self) -> usize {
        usize::from(self.last_transcript_height.get())
            .saturating_sub(1)
            .max(1)
    }

    fn max_scroll_rows_from_bottom(&self) -> usize {
        self.with_transcript_snapshot(self.last_transcript_width.get(), |snapshot| {
            snapshot
                .total_rows
                .saturating_sub(usize::from(self.last_transcript_height.get()))
        })
    }

    fn absorb_history(&mut self, history: &[SessionEvent]) {
        // Large sessions can contain thousands of tiny AssistantDelta events.
        // Recomputing wrapped transcript metrics after each replayed event makes
        // initial TUI load effectively quadratic, so replay all durable state
        // first and clamp the final scroll position once.
        for event in history {
            self.absorb_session_event_with_scroll_clamp(event, false);
        }
        self.clamp_scroll();
    }

    fn take_older_history_cursor(&mut self) -> Option<SessionHistoryCursor> {
        let OlderHistoryState::LoadRequested { .. } = self.older_history_state else {
            return None;
        };
        let oldest = self.oldest_loaded_sequence?;
        if oldest == 0 {
            self.older_history_state = OlderHistoryState::Exhausted;
            return None;
        }
        Some(SessionHistoryCursor {
            sequence: oldest.saturating_sub(1),
        })
    }

    fn prepend_older_history(&mut self, history: &[SessionEvent], has_more: bool) {
        if history.is_empty() {
            self.older_history_state = OlderHistoryState::Exhausted;
            self.status = "start of session history".to_string();
            return;
        }
        let reveal_rows = match self.older_history_state {
            OlderHistoryState::LoadRequested { reveal_rows } => reveal_rows,
            OlderHistoryState::More | OlderHistoryState::Exhausted => 0,
        };
        let max_scroll_before = self.max_scroll_rows_from_bottom();
        let mut blocks = Vec::new();
        let mut input_messages = Vec::new();
        for event in history {
            self.oldest_loaded_sequence = Some(
                self.oldest_loaded_sequence
                    .map_or(event.sequence, |oldest| oldest.min(event.sequence)),
            );
            if let SessionEventKind::UserMessage { text, .. } = &event.kind {
                input_messages.push(text.clone());
            }
            if let SessionEventKind::ModelUsage { usage, .. } = &event.kind {
                self.token_usage.absorb(usage);
            }
            if !matches!(event.kind, SessionEventKind::AssistantDelta { .. }) {
                blocks.extend(transcript_blocks_from_event(event));
            }
        }
        self.input_history.splice(0..0, input_messages);
        if !blocks.is_empty() {
            self.blocks.splice(0..0, blocks);
            self.mark_transcript_dirty();
            let max_scroll_after = self.max_scroll_rows_from_bottom();
            let inserted_rows = max_scroll_after.saturating_sub(max_scroll_before);
            self.scroll_rows_from_bottom = self
                .scroll_rows_from_bottom
                .saturating_add(reveal_rows.min(inserted_rows));
            self.clamp_scroll();
        }
        self.older_history_state = if has_more {
            OlderHistoryState::More
        } else {
            OlderHistoryState::Exhausted
        };
        self.status = if has_more {
            "loaded older history".to_string()
        } else {
            "start of session history".to_string()
        };
    }

    fn absorb_session_event(&mut self, event: &SessionEvent) {
        self.absorb_session_event_with_scroll_clamp(event, false);
    }

    fn absorb_session_event_with_scroll_clamp(&mut self, event: &SessionEvent, clamp_scroll: bool) {
        match &event.kind {
            SessionEventKind::AssistantDelta { text } => {
                self.push_assistant_delta_with_scroll_clamp(text, clamp_scroll);
                return;
            }
            SessionEventKind::AssistantMessage { text } => {
                self.finish_assistant_message_with_scroll_clamp(text, clamp_scroll);
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
                self.token_usage.context_window = None;
                self.token_usage.max_output_tokens = None;
                self.model_status_refresh_needed = true;
            }
            SessionEventKind::ModelUsage { usage, .. } => {
                self.token_usage.absorb(usage);
            }
            SessionEventKind::AgentChanged { agent_id } => {
                self.current_agent_id.clone_from(agent_id);
            }
            SessionEventKind::UserMessage { text, .. } => {
                self.push_input_history_message(text);
            }
            SessionEventKind::SessionCreated { name }
            | SessionEventKind::SessionRenamed { name } => {
                self.session_title.clone_from(name);
            }
            _ => {}
        }
        self.push_session_event(event);
        if clamp_scroll {
            self.clamp_scroll();
        }
    }

    fn push_session_event(&mut self, event: &SessionEvent) {
        let blocks = transcript_blocks_from_event(event);
        if blocks.is_empty() {
            return;
        }
        self.finish_streaming_block_if_needed();
        self.blocks.extend(blocks);
        self.mark_transcript_dirty();
    }

    fn push_assistant_delta_with_scroll_clamp(&mut self, text: &str, clamp_scroll: bool) {
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
        self.mark_transcript_dirty();
        if clamp_scroll {
            self.clamp_scroll();
        }
    }

    fn finish_assistant_message_with_scroll_clamp(&mut self, text: &str, clamp_scroll: bool) {
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
        self.mark_transcript_dirty();
        if clamp_scroll {
            self.clamp_scroll();
        }
    }

    fn finish_streaming_block_if_needed(&mut self) {
        if let Some(TranscriptBlock::Assistant { streaming, .. }) = self.blocks.last_mut() {
            *streaming = false;
        }
    }

    fn clamp_scroll(&mut self) {
        self.scroll_rows_from_bottom = self
            .scroll_rows_from_bottom
            .min(self.max_scroll_rows_from_bottom());
    }

    fn mark_transcript_dirty(&self) {
        self.transcript_revision
            .set(self.transcript_revision.get().wrapping_add(1));
    }

    fn with_transcript_snapshot<R>(
        &self,
        width: u16,
        f: impl FnOnce(&TranscriptRenderSnapshot) -> R,
    ) -> R {
        let width = width.max(1);
        let revision = self.transcript_revision.get();
        let needs_refresh =
            self.transcript_cache
                .borrow()
                .snapshot
                .as_ref()
                .is_none_or(|snapshot| {
                    snapshot.revision != revision
                        || snapshot.width != width
                        || snapshot.block_count != self.blocks.len()
                        || snapshot.search_query != self.search_query
                });
        if needs_refresh {
            let lines = self.rendered_transcript_lines_uncached();
            let (metrics, total_rows) = visual_line_metrics(&lines, width);
            self.transcript_cache.borrow_mut().snapshot = Some(TranscriptRenderSnapshot {
                revision,
                width,
                block_count: self.blocks.len(),
                search_query: self.search_query.clone(),
                lines,
                metrics,
                total_rows,
            });
        }

        let cache = self.transcript_cache.borrow();
        let snapshot = cache
            .snapshot
            .as_ref()
            .expect("transcript snapshot should exist after refresh");
        f(snapshot)
    }

    fn rendered_line_texts(&self) -> Vec<String> {
        self.with_transcript_snapshot(self.last_transcript_width.get(), |snapshot| {
            snapshot.lines.iter().map(line_plain_text).collect()
        })
    }

    #[cfg(test)]
    fn rendered_transcript_lines(&self) -> Vec<Line<'static>> {
        self.with_transcript_snapshot(self.last_transcript_width.get(), |snapshot| {
            snapshot.lines.clone()
        })
    }

    fn rendered_transcript_lines_uncached(&self) -> Vec<Line<'static>> {
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
        if self.command_palette.is_some() {
            TuiScope::CommandPalette
        } else if self.first_pending_permission().is_some() && !self.search_mode {
            TuiScope::Permission
        } else {
            TuiScope::Chat
        }
    }

    fn cursor_position(&self, area: Rect) -> Option<Position> {
        match self.current_scope() {
            TuiScope::Chat => {
                let chunks = chat_layout(area, &self.input, self.search_mode);
                composer_layout(chunks[2], &self.input, self.search_mode).cursor_position
            }
            TuiScope::CommandPalette => self
                .command_palette
                .as_ref()
                .and_then(|palette| command_palette_cursor_position(area, palette)),
            TuiScope::Permission | TuiScope::SessionPicker => None,
        }
    }

    fn push_input_history_message(&mut self, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        self.input_history.push(text.to_string());
    }

    fn previous_input_history(&mut self) {
        if self.search_mode {
            self.status = "input history unavailable while searching".to_string();
            return;
        }
        if self.input_history.is_empty() {
            self.status = "no input history in this session".to_string();
            return;
        }
        let index = match self.input_history_index {
            Some(0) => 0,
            Some(index) => index.saturating_sub(1),
            None => {
                self.input_history_draft = Some(self.input.text().to_string());
                self.input_history.len().saturating_sub(1)
            }
        };
        self.set_input_history_index(index);
    }

    fn next_input_history(&mut self) {
        if self.search_mode {
            self.status = "input history unavailable while searching".to_string();
            return;
        }
        let Some(current_index) = self.input_history_index else {
            self.status = "not browsing input history".to_string();
            return;
        };
        let next_index = current_index.saturating_add(1);
        if next_index < self.input_history.len() {
            self.set_input_history_index(next_index);
            return;
        }
        let draft = self.input_history_draft.take().unwrap_or_default();
        self.input_history_index = None;
        self.set_input_text(draft);
        self.status = "draft restored".to_string();
    }

    fn set_input_history_index(&mut self, index: usize) {
        self.input_history_index = Some(index);
        let text = self.input_history[index].clone();
        self.set_input_text(text);
        self.status = format!(
            "input history {}/{}",
            index.saturating_add(1),
            self.input_history.len()
        );
    }

    fn reset_input_history_navigation(&mut self) {
        self.input_history_index = None;
        self.input_history_draft = None;
    }

    fn set_input_text(&mut self, text: impl Into<String>) {
        self.input = TextEditBuffer::from_text(text);
    }

    fn take_input(&mut self) -> Option<String> {
        self.reset_input_history_navigation();
        let input = self.input.text().trim().to_string();
        if input.is_empty() {
            return None;
        }
        self.input.clear();
        Some(input)
    }

    fn open_command_palette(&mut self) {
        let mut p = CommandPaletteState::new();
        p.commands = vec![
            CommandInfo {
                id: "switch-model".into(),
                name: "Switch Model".into(),
                description: Some("Change active model ID".into()),
                requires_args: true,
                category: Some("model".into()),
            },
            CommandInfo {
                id: "switch-provider".into(),
                name: "Switch Provider".into(),
                description: Some("Change model provider plugin".into()),
                requires_args: true,
                category: Some("model".into()),
            },
            CommandInfo {
                id: "set-thinking".into(),
                name: "Set Thinking Level".into(),
                description: Some("low | medium | high (for reasoning models)".into()),
                requires_args: true,
                category: Some("model".into()),
            },
            CommandInfo {
                id: "help".into(),
                name: "Help".into(),
                description: Some("Show slash command reference".into()),
                requires_args: false,
                category: Some("general".into()),
            },
            CommandInfo {
                id: "compact".into(),
                name: "Compact Context".into(),
                description: Some("Summarize older context for future model turns".into()),
                requires_args: false,
                category: Some("general".into()),
            },
            CommandInfo {
                id: "clear".into(),
                name: "Clear Transcript".into(),
                description: Some("Clear chat transcript display in TUI".into()),
                requires_args: false,
                category: Some("general".into()),
            },
        ];
        p.is_loading = false;
        self.command_palette = Some(p);
        self.status = "command palette: type to filter, enter to run, esc close".to_string();
    }

    fn close_command_palette(&mut self) {
        self.command_palette = None;
        // do not overwrite status here — command execution sets its own message
        // (status reset to "ready" only happens on manual close or new actions)
    }

    async fn execute_command(&mut self, _client: &BcodeClient, cmd: &CommandInfo) {
        match cmd.id.as_str() {
            "switch-model" | "set-model" => {
                self.status = "use /models to list, then /model <id> [--provider <p>]".to_string();
            }
            "switch-provider" | "set-provider" => {
                self.status = "use slash /provider <plugin-id>".to_string();
            }
            "set-thinking" | "thinking" => {
                // cycle levels on repeated selection
                let next = match self.current_thinking_level {
                    Some(ReasoningEffort::Low) => ReasoningEffort::Medium,
                    Some(ReasoningEffort::Medium) => ReasoningEffort::High,
                    _ => ReasoningEffort::Low,
                };
                self.current_thinking_level = Some(next);
                self.status = format!("thinking level set to {:?}", next);
            }
            "help" => {
                self.status =
                    "Slash: /models, /model <id>, /provider <id>, /thinking low|medium|high, /compact, /clear, /help"
                        .to_string();
            }
            "compact" => {
                self.status = "use slash /compact".to_string();
            }
            "clear" => {
                self.blocks.clear();
                self.scroll_rows_from_bottom = 0;
                self.mark_transcript_dirty();
                self.status = "transcript cleared".to_string();
            }
            _ => {
                self.status = format!("executing {}", cmd.name);
            }
        }
        // clear filter/input after action
        self.input.clear();
        if let Some(p) = &mut self.command_palette {
            p.filter.clear();
            p.selected = 0;
        }
    }

    #[allow(clippy::too_many_lines)]
    fn parse_and_execute_slash(&mut self, input: &str, _client: &BcodeClient) -> bool {
        let parts: Vec<&str> = input.split_whitespace().collect();
        if parts.is_empty() {
            return false;
        }
        let cmd = parts[0].trim_start_matches('/');
        match cmd {
            "model" | "set-model" if parts.len() > 1 => {
                let model = parts[1].to_string();
                let provider = if parts.len() > 3 && parts[2] == "--provider" {
                    Some(parts[3].to_string())
                } else {
                    None
                };
                // fire and forget async in real; for sync here status
                self.status = format!("switching model to {} (provider {:?})", model, provider);
                // In full impl: client.set_session_model(...).await
                true
            }
            "provider" | "set-provider" if parts.len() > 1 => {
                self.status = format!("switching provider to {}", parts[1]);
                true
            }
            "thinking" | "set-thinking" if parts.len() > 1 => {
                let level_str = parts[1].to_lowercase();
                let level = match level_str.as_str() {
                    "low" => Some(ReasoningEffort::Low),
                    "medium" => Some(ReasoningEffort::Medium),
                    "high" => Some(ReasoningEffort::High),
                    _ => None,
                };
                if let Some(l) = level {
                    self.current_thinking_level = Some(l);
                    self.status = format!("thinking set to {:?}", l);
                } else {
                    self.status = "invalid thinking level (low|medium|high)".to_string();
                }
                true
            }
            "help" => {
                self.status =
                    "Commands: /models, /model <id>, /provider <id>, /thinking <level>, /compact, /clear, /help"
                        .to_string();
                true
            }
            "clear" => {
                self.blocks.clear();
                self.scroll_rows_from_bottom = 0;
                self.mark_transcript_dirty();
                self.status = "cleared".to_string();
                true
            }
            _ => false,
        }
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

fn model_turn_outcome_label(outcome: ModelTurnOutcome) -> &'static str {
    match outcome {
        ModelTurnOutcome::Completed => "ready",
        ModelTurnOutcome::Cancelled => "model turn cancelled",
        ModelTurnOutcome::Error => "model turn failed",
        ModelTurnOutcome::IdleTimeout => "model provider idle timeout",
        ModelTurnOutcome::ToolRoundLimitReached => "model tool-call round limit reached",
        ModelTurnOutcome::ProviderUnavailable => "model provider unavailable",
    }
}

fn render_chat_frame(frame: &mut Frame<'_>, app: &ChatApp) {
    let area = frame.area();
    frame.render_widget(app, area);
    if let Some(position) = app.cursor_position(area) {
        frame.set_cursor_position(position);
    }
}

fn chat_layout(area: Rect, input: &TextEditBuffer, search_mode: bool) -> [Rect; 4] {
    let composer_height = composer_height(
        input,
        search_mode,
        area.height,
        area.width.saturating_sub(2),
    );
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(composer_height),
            Constraint::Length(1),
        ])
        .areas(area)
}

impl ratatui::widgets::Widget for &ChatApp {
    fn render(self, area: Rect, buf: &mut ratatui::buffer::Buffer) {
        let chunks = chat_layout(area, &self.input, self.search_mode);

        render_chat_header(self, chunks[0], buf);

        let transcript_width = chunks[1].width;
        let transcript_height = chunks[1].height;
        self.last_transcript_width.set(transcript_width);
        self.last_transcript_height.set(transcript_height);
        self.last_transcript_area.set(chunks[1]);

        let viewport = self.with_transcript_snapshot(transcript_width, |snapshot| {
            transcript_viewport_from_metrics(
                &snapshot.lines,
                &snapshot.metrics,
                snapshot.total_rows,
                transcript_width,
                transcript_height,
                self.scroll_rows_from_bottom,
            )
        });
        let transcript = if viewport.lines.is_empty() {
            Paragraph::new(Text::from(vec![Line::from(vec![Span::styled(
                "Start a conversation with Bcode.",
                muted_style(),
            )])]))
        } else {
            Paragraph::new(Text::from(viewport.lines))
                .wrap(TRANSCRIPT_WRAP)
                .scroll((usize_to_u16_saturating(viewport.local_scroll_y), 0))
        };
        transcript.render(chunks[1], buf);

        let input_title = if self.search_mode {
            " Search "
        } else {
            " Message "
        };
        let composer = composer_layout(chunks[2], &self.input, self.search_mode);
        let input = Paragraph::new(composer.text).block(
            Block::new()
                .title(input_title)
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(if self.search_mode {
                    accent_style()
                } else {
                    border_style()
                }),
        );
        input.render(chunks[2], buf);

        let tick = self.render_tick.get().wrapping_add(1);
        self.render_tick.set(tick);
        render_chat_status(
            self,
            chunks[3],
            buf,
            viewport.effective_scroll_rows_from_bottom,
            tick,
        );

        if let Some(permission) = self.first_pending_permission() {
            render_permission_modal(
                area,
                buf,
                permission,
                &self.permission_hints,
                self.selected_permission_choice,
                self.pending_permissions.len(),
            );
        }
        if let Some(palette) = &self.command_palette {
            render_command_palette(area, buf, palette);
        }
    }
}

fn render_chat_header(app: &ChatApp, area: Rect, buf: &mut ratatui::buffer::Buffer) {
    let provider = truncate_middle(
        app.selected_provider_plugin_id.as_deref().unwrap_or("auto"),
        22,
    );
    let model = truncate_middle(app.selected_model_id.as_deref().unwrap_or("default"), 30);
    let thinking = app
        .current_thinking_level
        .map(|level| format!("{:?}", level))
        .unwrap_or_else(|| "default".to_string());
    let agent = truncate_middle(&app.current_agent_id, 18);
    let session_id = truncate_middle(&app.session_id.to_string(), 12);
    let session_title = app.session_title.as_deref().map_or_else(
        || "Untitled session".to_string(),
        |title| truncate_end(title, 28),
    );
    let mut spans = vec![
        Span::styled(" bcode ", title_style()),
        Span::styled("session ", muted_style()),
        Span::styled(session_title, normal_style()),
        Span::styled(format!(" ({session_id})"), muted_style()),
        Span::raw("  "),
    ];
    push_label_value(&mut spans, "provider", &provider, accent_style());
    spans.push(Span::raw("  "));
    push_label_value(&mut spans, "model", &model, normal_style());
    spans.push(Span::raw("  "));
    push_label_value(&mut spans, "agent", &agent, accent_style());
    spans.push(Span::raw("  "));
    push_label_value(&mut spans, "thinking", &thinking, muted_style());
    Paragraph::new(Line::from(spans)).render(area, buf);
}

fn render_chat_status(
    app: &ChatApp,
    area: Rect,
    buf: &mut ratatui::buffer::Buffer,
    scroll_rows_from_bottom: usize,
    tick: u64,
) {
    let mut spans = if app.activity.is_idle() {
        vec![Span::styled(app.status.clone(), status_style(&app.status))]
    } else {
        activity_spans(&app.activity, tick, app.activity_started_at)
    };
    if scroll_rows_from_bottom > 0 {
        spans.push(Span::styled(
            format!("  ·  {scroll_rows_from_bottom} rows from bottom"),
            muted_style(),
        ));
    }
    let summary = app.token_usage.footer_summary();
    spans.push(Span::styled(format!("  ·  {summary}"), muted_style()));
    spans.push(Span::styled("  ·  ", muted_style()));
    spans.extend(key_hint_spans(&app.key_hints));
    Paragraph::new(Line::from(spans)).render(area, buf);
}

fn activity_spans(
    activity: &ActivityState,
    tick: u64,
    started_at: Option<Instant>,
) -> Vec<Span<'static>> {
    if activity.is_idle() {
        return vec![Span::styled("ready", status_style("ready"))];
    }
    let spinner =
        SPINNER_FRAMES[usize::try_from(tick / 2).unwrap_or_default() % SPINNER_FRAMES.len()];
    let elapsed = started_at.map_or(Duration::ZERO, |started| started.elapsed());
    let label = activity_label(activity);
    vec![
        Span::styled(spinner.to_string(), accent_bold_style()),
        Span::raw(" "),
        Span::styled(label, accent_style()),
        Span::styled(format!(" · {}", format_elapsed(elapsed)), muted_style()),
    ]
}

fn activity_label(activity: &ActivityState) -> String {
    match activity {
        ActivityState::Idle => "ready".to_string(),
        ActivityState::Thinking => "thinking".to_string(),
        ActivityState::Compacting { detail } => {
            format!("compacting context · {}", truncate_middle(detail, 36))
        }
        ActivityState::Streaming { chars } => {
            format!("streaming · {} chars", compact_count(*chars))
        }
        ActivityState::RunningTool { name } => {
            format!("running tool · {}", truncate_middle(name, 24))
        }
        ActivityState::WaitingPermission { name } => {
            format!("permission required · {}", truncate_middle(name, 24))
        }
        ActivityState::Cancelling => "cancelling".to_string(),
    }
}

fn compact_count(value: usize) -> String {
    compact_u64(u64::try_from(value).unwrap_or(u64::MAX))
}

fn compact_u64(value: u64) -> String {
    if value >= 1_000_000 {
        let whole = value / 1_000_000;
        let decimal = (value % 1_000_000) / 100_000;
        format!("{whole}.{decimal}m")
    } else if value >= 1_000 {
        let whole = value / 1_000;
        let decimal = (value % 1_000) / 100;
        format!("{whole}.{decimal}k")
    } else {
        value.to_string()
    }
}

fn context_window_percentage(input_tokens: u32, context_window: u32) -> u32 {
    let numerator = u64::from(input_tokens).saturating_mul(100);
    let denominator = u64::from(context_window).max(1);
    u32::try_from(numerator / denominator).unwrap_or(u32::MAX)
}

fn format_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 1 {
        "<1s".to_string()
    } else if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m{}s", seconds / 60, seconds % 60)
    }
}

fn command_palette_layout(area: Rect) -> (Rect, [Rect; 3]) {
    let modal = centered_rect(area, area.width.min(86), area.height.min(18));
    let inner = inset(modal, 2, 1);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .areas(inner);
    (modal, chunks)
}

fn command_palette_cursor_position(area: Rect, palette: &CommandPaletteState) -> Option<Position> {
    let (_, chunks) = command_palette_layout(area);
    let search_area = chunks[0];
    if search_area.width == 0 || search_area.height == 0 {
        return None;
    }
    let column = line_width("› ").saturating_add(line_width(
        &palette.filter.text()[..palette.filter.cursor_byte_index()],
    ));
    Some(Position::new(
        search_area.x + usize_to_u16_saturating(column).min(search_area.width.saturating_sub(1)),
        search_area.y,
    ))
}

fn render_command_palette(
    area: Rect,
    buf: &mut ratatui::buffer::Buffer,
    palette: &CommandPaletteState,
) {
    let (modal, chunks) = command_palette_layout(area);
    ratatui::widgets::Widget::render(Clear, modal, buf);
    let block = Block::new()
        .title(Line::from(vec![
            Span::styled(" Command Palette ", title_style()),
            Span::styled("ctrl+p closes ", muted_style()),
        ]))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(accent_style());
    ratatui::widgets::Widget::render(block, modal, buf);

    let search = if palette.filter.is_empty() {
        Line::from(vec![
            Span::styled("› ", accent_style()),
            Span::styled("Type to filter commands", muted_style()),
        ])
    } else {
        Line::from(vec![
            Span::styled("› ", accent_style()),
            Span::styled(palette.filter.text().to_string(), normal_style()),
        ])
    };
    Paragraph::new(Text::from(vec![
        search,
        Line::from(vec![Span::styled(
            "Run common actions without leaving the keyboard",
            muted_style(),
        )]),
    ]))
    .render(chunks[0], buf);

    let filtered = palette.filtered_commands();
    if filtered.is_empty() {
        Paragraph::new(Line::from(vec![Span::styled(
            "No commands match",
            muted_style(),
        )]))
        .alignment(Alignment::Center)
        .render(chunks[1], buf);
    } else {
        let items = filtered
            .iter()
            .copied()
            .map(command_palette_item)
            .collect::<Vec<_>>();
        let selected = palette.selected.min(filtered.len().saturating_sub(1));
        let list = List::new(items)
            .highlight_symbol("  ")
            .highlight_style(selected_style());
        let mut state = ListState::default().with_selected(Some(selected));
        StatefulWidget::render(list, chunks[1], buf, &mut state);
    }

    Paragraph::new(Line::from(vec![
        Span::styled("enter", key_style()),
        Span::styled(" run · ", muted_style()),
        Span::styled("esc", key_style()),
        Span::styled(" close · ", muted_style()),
        Span::styled("type", key_style()),
        Span::styled(" filter", muted_style()),
    ]))
    .render(chunks[2], buf);
}

fn render_permission_modal(
    area: Rect,
    buf: &mut ratatui::buffer::Buffer,
    permission: &PendingPermissionView,
    key_hints: &str,
    selected: PermissionChoice,
    pending_count: usize,
) {
    let modal = centered_rect(area, area.width.min(84), area.height.min(16));
    ratatui::widgets::Widget::render(Clear, modal, buf);
    let count_label = if pending_count > 1 {
        format!("{pending_count} pending ")
    } else {
        String::new()
    };
    let block = Block::new()
        .title(Line::from(vec![
            Span::styled(" Permission required ", danger_bold_style()),
            Span::styled(count_label, muted_style()),
        ]))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(danger_style());
    let inner = inset(modal, 2, 1);
    ratatui::widgets::Widget::render(block, modal, buf);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(4),
            Constraint::Length(2),
            Constraint::Length(1),
        ])
        .split(inner);

    Paragraph::new(Text::from(vec![
        Line::from(vec![
            Span::styled(permission.tool_name.clone(), title_style()),
            Span::styled(" wants to run", normal_style()),
        ]),
        Line::from(vec![
            Span::styled("tool call ", muted_style()),
            Span::styled(truncate_middle(&permission.tool_call_id, 18), muted_style()),
            Span::styled(" · permission ", muted_style()),
            Span::styled(
                truncate_middle(&permission.permission_id, 18),
                muted_style(),
            ),
        ]),
    ]))
    .render(chunks[0], buf);

    Paragraph::new(Text::from(permission_summary_lines(permission)))
        .wrap(Wrap { trim: false })
        .render(chunks[1], buf);

    Paragraph::new(Text::from(vec![
        permission_choice_line(selected),
        Line::from(key_hint_spans(key_hints)),
    ]))
    .render(chunks[2], buf);

    Paragraph::new(Line::from(vec![Span::styled(
        "Selected choice is highlighted. Enter confirms; escape denies once.",
        muted_style(),
    )]))
    .render(chunks[3], buf);
}

fn command_palette_item(command: &CommandInfo) -> ListItem<'static> {
    let category = command.category.as_deref().unwrap_or("general");
    let description = command.description.as_deref().unwrap_or("");
    ListItem::new(Line::from(vec![
        Span::styled(format!(" {:<8} ", truncate_end(category, 8)), badge_style()),
        Span::raw("  "),
        Span::styled(truncate_end(&command.name, 24), normal_bold_style()),
        Span::raw("  "),
        Span::styled(truncate_end(description, 42), muted_style()),
    ]))
}

fn permission_summary_lines(permission: &PendingPermissionView) -> Vec<Line<'static>> {
    if permission.tool_name == "shell.run"
        && let Some(command) = permission.string_argument("command")
    {
        return vec![
            Line::from(vec![Span::styled("command", muted_style())]),
            Line::from(vec![Span::styled(command, code_style())]),
        ];
    }
    if permission.tool_name.starts_with("filesystem.")
        && let Some(path) = permission.string_argument("path")
    {
        return vec![
            Line::from(vec![Span::styled("path", muted_style())]),
            Line::from(vec![Span::styled(path, code_style())]),
        ];
    }
    let mut lines = vec![Line::from(vec![Span::styled("arguments", muted_style())])];
    lines.extend(
        pretty_jsonish(&permission.arguments_json)
            .lines()
            .take(8)
            .map(|line| Line::from(vec![Span::styled(line.to_string(), code_style())])),
    );
    lines
}

fn permission_choice_line(selected: PermissionChoice) -> Line<'static> {
    Line::from(vec![
        permission_choice_span(PermissionChoice::AllowOnce, selected),
        Span::raw("  "),
        permission_choice_span(PermissionChoice::DenyOnce, selected),
        Span::raw("  "),
        permission_choice_span(PermissionChoice::AlwaysAllow, selected),
        Span::raw("  "),
        permission_choice_span(PermissionChoice::AlwaysDeny, selected),
    ])
}

fn permission_choice_span(choice: PermissionChoice, selected: PermissionChoice) -> Span<'static> {
    let label = format!(" {} ", choice.label());
    if choice == selected {
        Span::styled(label, selected_button_style())
    } else {
        Span::styled(label, muted_style())
    }
}

struct ComposerLayout {
    text: Text<'static>,
    cursor_position: Option<Position>,
}

fn composer_height(
    input: &TextEditBuffer,
    search_mode: bool,
    terminal_height: u16,
    content_width: u16,
) -> u16 {
    if search_mode {
        return 3.min(terminal_height.max(1));
    }
    usize_to_u16_saturating(
        input
            .wrapped_layout(usize::from(content_width.max(1)))
            .lines
            .len(),
    )
    .min(MAX_COMPOSER_ROWS)
    .saturating_add(2)
    .min(terminal_height.saturating_sub(2).max(3))
}

fn composer_layout(area: Rect, input: &TextEditBuffer, search_mode: bool) -> ComposerLayout {
    let content_area = inset(area, 1, 1);
    if content_area.width == 0 || content_area.height == 0 {
        return ComposerLayout {
            text: Text::default(),
            cursor_position: None,
        };
    }

    let visible_row_count = usize::from(content_area.height);
    let layout = input.wrapped_layout(usize::from(content_area.width.max(1)));
    let wrapped_lines = layout.lines;
    let cursor_row = layout.cursor.row;
    let scroll = cursor_row
        .saturating_add(1)
        .saturating_sub(visible_row_count);
    let cursor_column =
        usize_to_u16_saturating(layout.cursor.col).min(content_area.width.saturating_sub(1));
    let visible_lines = if input.is_empty() {
        vec![Line::from(vec![Span::styled(
            composer_placeholder(search_mode),
            muted_style(),
        )])]
    } else {
        wrapped_lines
            .iter()
            .skip(scroll)
            .take(visible_row_count)
            .map(|line| Line::from(vec![Span::styled(line.clone(), normal_style())]))
            .collect::<Vec<_>>()
    };

    ComposerLayout {
        text: Text::from(visible_lines),
        cursor_position: Some(Position::new(
            content_area.x + cursor_column,
            content_area.y + usize_to_u16_saturating(cursor_row.saturating_sub(scroll)),
        )),
    }
}

fn composer_placeholder(search_mode: bool) -> &'static str {
    if search_mode {
        "Search transcript…"
    } else {
        "Ask Bcode…"
    }
}

fn line_width(line: &str) -> usize {
    Line::from(line.to_string()).width()
}

fn push_label_value(spans: &mut Vec<Span<'static>>, label: &str, value: &str, value_style: Style) {
    spans.push(Span::styled(label.to_string(), muted_style()));
    spans.push(Span::styled(":", muted_style()));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(value.to_string(), value_style));
}

fn key_hint_spans(hints: &str) -> Vec<Span<'static>> {
    hints
        .split('·')
        .enumerate()
        .flat_map(|(index, segment)| {
            let trimmed = segment.trim();
            let mut parts = trimmed.splitn(2, ' ');
            let key = parts.next().unwrap_or_default();
            let label = parts.next().unwrap_or_default();
            let mut spans = Vec::new();
            if index > 0 {
                spans.push(Span::styled(" · ", muted_style()));
            }
            spans.push(Span::styled(key.to_string(), key_style()));
            if !label.is_empty() {
                spans.push(Span::styled(format!(" {label}"), muted_style()));
            }
            spans
        })
        .collect()
}

fn centered_rect(area: Rect, desired_width: u16, desired_height: u16) -> Rect {
    if area.width == 0 || area.height == 0 {
        return area;
    }
    let max_width = area
        .width
        .saturating_sub(MODAL_MARGIN_X.saturating_mul(2))
        .max(1)
        .min(area.width);
    let max_height = area
        .height
        .saturating_sub(MODAL_MARGIN_Y.saturating_mul(2))
        .max(1)
        .min(area.height);
    let width = desired_width.min(max_width).max(1).min(area.width);
    let height = desired_height.min(max_height).max(1).min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn inset(area: Rect, horizontal: u16, vertical: u16) -> Rect {
    let x_margin = horizontal.min(area.width / 2);
    let y_margin = vertical.min(area.height / 2);
    Rect::new(
        area.x + x_margin,
        area.y + y_margin,
        area.width.saturating_sub(x_margin.saturating_mul(2)),
        area.height.saturating_sub(y_margin.saturating_mul(2)),
    )
}

fn truncate_middle(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    if count <= max_chars {
        return value.to_string();
    }
    if max_chars <= 1 {
        return "…".to_string();
    }
    let left = max_chars / 2;
    let right = max_chars.saturating_sub(left).saturating_sub(1);
    let prefix = value.chars().take(left).collect::<String>();
    let suffix = value
        .chars()
        .rev()
        .take(right)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    format!("{prefix}…{suffix}")
}

fn truncate_end(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    if max_chars <= 1 {
        return "…".to_string();
    }
    let mut truncated = value
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    truncated.push('…');
    truncated
}

fn normal_style() -> Style {
    Style::default().fg(COLOR_TEXT)
}

fn normal_bold_style() -> Style {
    normal_style().add_modifier(Modifier::BOLD)
}

fn muted_style() -> Style {
    Style::default().fg(COLOR_MUTED)
}

fn accent_style() -> Style {
    Style::default().fg(COLOR_ACCENT)
}

fn accent_bold_style() -> Style {
    accent_style().add_modifier(Modifier::BOLD)
}

fn danger_style() -> Style {
    Style::default().fg(COLOR_DANGER)
}

fn danger_bold_style() -> Style {
    danger_style().add_modifier(Modifier::BOLD)
}

fn border_style() -> Style {
    Style::default().fg(COLOR_BORDER)
}

fn title_style() -> Style {
    accent_bold_style()
}

fn key_style() -> Style {
    accent_style().add_modifier(Modifier::BOLD)
}

fn code_style() -> Style {
    Style::default().fg(COLOR_WARNING)
}

fn status_style(status: &str) -> Style {
    let lower = status.to_lowercase();
    if lower.contains("failed") || lower.contains("error") || lower.contains("denied") {
        danger_style()
    } else if lower.contains("approved") || lower.contains("ready") || lower.contains("set") {
        Style::default().fg(COLOR_SUCCESS)
    } else {
        normal_style()
    }
}

fn selected_style() -> Style {
    Style::default()
        .fg(Color::White)
        .bg(COLOR_SELECTED_BG)
        .add_modifier(Modifier::BOLD)
}

fn selected_button_style() -> Style {
    Style::default()
        .fg(Color::Black)
        .bg(COLOR_ACCENT)
        .add_modifier(Modifier::BOLD)
}

fn badge_style() -> Style {
    Style::default().fg(Color::Black).bg(COLOR_ACCENT)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VisualLineMetric {
    logical_index: usize,
    start_row: usize,
    end_row: usize,
}

#[derive(Debug, Default)]
struct TranscriptRenderCache {
    snapshot: Option<TranscriptRenderSnapshot>,
}

#[derive(Debug, Clone)]
struct TranscriptRenderSnapshot {
    revision: u64,
    width: u16,
    block_count: usize,
    search_query: String,
    lines: Vec<Line<'static>>,
    metrics: Vec<VisualLineMetric>,
    total_rows: usize,
}

#[derive(Debug, Clone)]
struct TranscriptViewport {
    lines: Vec<Line<'static>>,
    effective_scroll_rows_from_bottom: usize,
    local_scroll_y: usize,
}

#[cfg(test)]
fn transcript_viewport(
    lines: &[Line<'static>],
    width: u16,
    height: u16,
    scroll_rows_from_bottom: usize,
) -> TranscriptViewport {
    let (metrics, total_rows) = visual_line_metrics(lines, width);
    transcript_viewport_from_metrics(
        lines,
        &metrics,
        total_rows,
        width,
        height,
        scroll_rows_from_bottom,
    )
}

fn transcript_viewport_from_metrics(
    lines: &[Line<'static>],
    metrics: &[VisualLineMetric],
    total_rows: usize,
    width: u16,
    height: u16,
    scroll_rows_from_bottom: usize,
) -> TranscriptViewport {
    if lines.is_empty() || metrics.is_empty() || width == 0 || height == 0 {
        return TranscriptViewport {
            lines: Vec::new(),
            effective_scroll_rows_from_bottom: 0,
            local_scroll_y: 0,
        };
    }

    let viewport_rows = usize::from(height);
    let max_scroll_rows_from_bottom = total_rows.saturating_sub(viewport_rows);
    let effective_scroll_rows_from_bottom =
        scroll_rows_from_bottom.min(max_scroll_rows_from_bottom);
    let target_start_row =
        max_scroll_rows_from_bottom.saturating_sub(effective_scroll_rows_from_bottom);
    let target_end_row = target_start_row.saturating_add(viewport_rows);

    let first_visible_index = metrics
        .iter()
        .position(|metric| metric.end_row > target_start_row)
        .unwrap_or(0);
    let last_visible_index = metrics
        .iter()
        .rposition(|metric| metric.start_row < target_end_row)
        .unwrap_or(first_visible_index);
    let window_start = first_visible_index.saturating_sub(TRANSCRIPT_WINDOW_OVERSCAN_LINES);
    let window_end = last_visible_index
        .saturating_add(1)
        .saturating_add(TRANSCRIPT_WINDOW_OVERSCAN_LINES)
        .min(lines.len());
    let window_start_row = metrics[window_start].start_row;

    TranscriptViewport {
        lines: lines[window_start..window_end].to_vec(),
        effective_scroll_rows_from_bottom,
        local_scroll_y: target_start_row.saturating_sub(window_start_row),
    }
}

fn visual_line_metrics(lines: &[Line<'static>], width: u16) -> (Vec<VisualLineMetric>, usize) {
    let width = width.max(1);
    let mut next_row = 0usize;
    let metrics = lines
        .iter()
        .enumerate()
        .map(|(logical_index, line)| {
            let start_row = next_row;
            next_row = next_row.saturating_add(visual_line_count(line, width));
            VisualLineMetric {
                logical_index,
                start_row,
                end_row: next_row,
            }
        })
        .collect();
    (metrics, next_row)
}

fn visual_line_count(line: &Line<'static>, width: u16) -> usize {
    Paragraph::new(Text::from(vec![line.clone()]))
        .wrap(TRANSCRIPT_WRAP)
        .line_count(width)
        .max(1)
}

fn line_plain_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

fn usize_to_u16_saturating(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

impl TranscriptBlock {
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
    let heading_style = if prominent {
        Style::default().fg(color).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(color)
    };
    let body_style = if prominent {
        normal_style()
    } else {
        muted_style()
    };
    let mut lines = vec![highlighted_line(
        "",
        title,
        Style::default(),
        heading_style,
        search_query,
    )];
    if body.is_empty() {
        lines.push(Line::from(vec![Span::styled("  ·", muted_style())]));
    } else {
        for line in body.lines() {
            lines.push(highlighted_line(
                "  ",
                line,
                muted_style(),
                body_style,
                search_query,
            ));
        }
    }
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
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        Ok(Self { terminal })
    }

    fn draw_frame<F>(&mut self, render: F) -> Result<(), io::Error>
    where
        F: FnOnce(&mut Frame<'_>),
    {
        self.terminal.draw(render)?;
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = self.terminal.show_cursor();
    }
}

fn transcript_blocks_from_event(event: &SessionEvent) -> Vec<TranscriptBlock> {
    match &event.kind {
        SessionEventKind::SessionCreated { name } => vec![TranscriptBlock::Meta {
            text: format!("session started: {}", name.as_deref().unwrap_or("untitled")),
        }],
        SessionEventKind::SessionRenamed { name } => vec![TranscriptBlock::Meta {
            text: format!("session renamed: {}", name.as_deref().unwrap_or("untitled")),
        }],
        SessionEventKind::ClientAttached { .. }
        | SessionEventKind::ClientDetached { .. }
        | SessionEventKind::ModelTurnStarted { .. }
        | SessionEventKind::ModelUsage { .. }
        | SessionEventKind::TraceEvent { .. } => Vec::new(),
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
        SessionEventKind::AgentChanged { agent_id } => vec![TranscriptBlock::Meta {
            text: format!("agent changed: {agent_id}"),
        }],
        SessionEventKind::SystemMessage { text } => {
            vec![TranscriptBlock::System { text: text.clone() }]
        }
        SessionEventKind::ContextCompacted {
            compacted_through_sequence,
            ..
        } => vec![TranscriptBlock::Meta {
            text: format!("context compacted through #{compacted_through_sequence}"),
        }],
        SessionEventKind::ModelTurnFinished {
            outcome, message, ..
        } => {
            if *outcome == ModelTurnOutcome::Completed {
                Vec::new()
            } else {
                vec![TranscriptBlock::System {
                    text: message
                        .clone()
                        .unwrap_or_else(|| model_turn_outcome_label(*outcome).to_string()),
                }]
            }
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
    fn text_input_preserves_printable_characters() {
        assert_eq!(
            key_is_text_input(&KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT)),
            Some('A')
        );
        assert_eq!(
            key_is_text_input(&KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE)),
            Some('A')
        );
        assert_eq!(
            key_is_text_input(&KeyEvent::new(KeyCode::Char('!'), KeyModifiers::SHIFT)),
            Some('!')
        );
        assert_eq!(
            key_is_text_input(&KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(
            key_is_text_input(&KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT)),
            None
        );
    }

    fn apply_chat_action(app: &mut ChatApp, action: TuiAction) {
        match action {
            TuiAction::DeleteCharBackward => app.input.delete(TextDelete::Backward),
            TuiAction::DeleteCharForward => app.input.delete(TextDelete::Forward),
            TuiAction::DeleteWordBackward => app.input.delete(TextDelete::WordBackward),
            TuiAction::DeleteWordForward => app.input.delete(TextDelete::WordForward),
            TuiAction::DeleteToStart => app.input.delete(TextDelete::ToStart),
            TuiAction::DeleteToEnd => app.input.delete(TextDelete::ToEnd),
            TuiAction::MoveCursorLeft => app.input.move_cursor(TextMotion::Left),
            TuiAction::MoveCursorRight => app.input.move_cursor(TextMotion::Right),
            TuiAction::MoveCursorWordLeft => app.input.move_cursor(TextMotion::WordLeft),
            TuiAction::MoveCursorWordRight => app.input.move_cursor(TextMotion::WordRight),
            TuiAction::MoveCursorStart => app.input.move_cursor(TextMotion::Start),
            TuiAction::MoveCursorEnd => app.input.move_cursor(TextMotion::End),
            _ => panic!("unsupported test editor action: {action:?}"),
        }
    }

    fn apply_command_palette_action(palette: &mut CommandPaletteState, action: TuiAction) {
        match action {
            TuiAction::DeleteCharBackward => palette.filter.delete(TextDelete::Backward),
            TuiAction::DeleteCharForward => palette.filter.delete(TextDelete::Forward),
            TuiAction::DeleteWordBackward => palette.filter.delete(TextDelete::WordBackward),
            TuiAction::DeleteWordForward => palette.filter.delete(TextDelete::WordForward),
            TuiAction::DeleteToStart => palette.filter.delete(TextDelete::ToStart),
            TuiAction::DeleteToEnd => palette.filter.delete(TextDelete::ToEnd),
            TuiAction::MoveCursorLeft => palette.filter.move_cursor(TextMotion::Left),
            TuiAction::MoveCursorRight => palette.filter.move_cursor(TextMotion::Right),
            TuiAction::MoveCursorWordLeft => palette.filter.move_cursor(TextMotion::WordLeft),
            TuiAction::MoveCursorWordRight => palette.filter.move_cursor(TextMotion::WordRight),
            TuiAction::MoveCursorStart => palette.filter.move_cursor(TextMotion::Start),
            TuiAction::MoveCursorEnd => palette.filter.move_cursor(TextMotion::End),
            _ => panic!("unsupported test palette action: {action:?}"),
        }
        palette.selected = 0;
    }

    fn chat_app_with_input(text: &str) -> ChatApp {
        let keymap = KeyMap::from_config(&bcode_config::TuiConfig::default());
        let mut app = ChatApp::new(SessionId::new(), &[], &keymap);
        app.input = TextEditBuffer::from_text(text);
        app
    }

    #[test]
    fn composer_editor_movement_actions_update_cursor() {
        let mut app = chat_app_with_input("hello brave world");

        apply_chat_action(&mut app, TuiAction::MoveCursorWordLeft);
        assert_eq!(app.input.cursor_byte_index(), "hello brave ".len());
        apply_chat_action(&mut app, TuiAction::MoveCursorLeft);
        assert_eq!(app.input.cursor_byte_index(), "hello brave".len());
        apply_chat_action(&mut app, TuiAction::MoveCursorStart);
        assert_eq!(app.input.cursor_byte_index(), 0);
        apply_chat_action(&mut app, TuiAction::MoveCursorRight);
        assert_eq!(app.input.cursor_byte_index(), "h".len());
        apply_chat_action(&mut app, TuiAction::MoveCursorWordRight);
        assert_eq!(app.input.cursor_byte_index(), "hello".len());
        apply_chat_action(&mut app, TuiAction::MoveCursorEnd);
        assert_eq!(app.input.cursor_byte_index(), app.input.text().len());
    }

    #[test]
    fn composer_editor_delete_actions_remove_expected_ranges() {
        let mut app = chat_app_with_input("hello brave world");

        apply_chat_action(&mut app, TuiAction::DeleteWordBackward);
        assert_eq!(app.input.text(), "hello brave ");
        apply_chat_action(&mut app, TuiAction::DeleteCharBackward);
        assert_eq!(app.input.text(), "hello brave");
        apply_chat_action(&mut app, TuiAction::MoveCursorStart);
        apply_chat_action(&mut app, TuiAction::DeleteWordForward);
        assert_eq!(app.input.text(), " brave");
        apply_chat_action(&mut app, TuiAction::DeleteCharForward);
        assert_eq!(app.input.text(), "brave");
    }

    #[test]
    fn composer_editor_delete_to_start_and_end_actions_remove_expected_ranges() {
        let mut app = chat_app_with_input("hello brave world");

        apply_chat_action(&mut app, TuiAction::MoveCursorWordLeft);
        apply_chat_action(&mut app, TuiAction::DeleteToStart);
        assert_eq!(app.input.text(), "world");
        assert_eq!(app.input.cursor_byte_index(), 0);
        apply_chat_action(&mut app, TuiAction::DeleteToEnd);
        assert!(app.input.is_empty());
    }

    #[test]
    fn composer_editor_inserts_in_middle() {
        let mut app = chat_app_with_input("helo");

        apply_chat_action(&mut app, TuiAction::MoveCursorStart);
        apply_chat_action(&mut app, TuiAction::MoveCursorRight);
        apply_chat_action(&mut app, TuiAction::MoveCursorRight);
        app.input.insert_char('l');

        assert_eq!(app.input.text(), "hello");
        assert_eq!(app.input.cursor_byte_index(), "hel".len());
    }

    #[test]
    fn composer_take_input_submits_edited_text_and_clears_buffer() {
        let mut app = chat_app_with_input("helo world");

        apply_chat_action(&mut app, TuiAction::MoveCursorStart);
        apply_chat_action(&mut app, TuiAction::MoveCursorWordRight);
        apply_chat_action(&mut app, TuiAction::MoveCursorLeft);
        app.input.insert_char('l');

        assert_eq!(app.take_input(), Some("hello world".to_string()));
        assert!(app.input.is_empty());
    }

    #[test]
    fn composer_take_input_trims_submitted_text_after_editing() {
        let mut app = chat_app_with_input("  hello  ");
        apply_chat_action(&mut app, TuiAction::MoveCursorEnd);
        app.input.insert_char('!');

        assert_eq!(app.take_input(), Some("hello  !".to_string()));
    }

    #[test]
    fn input_history_replays_session_user_messages() {
        let keymap = KeyMap::from_config(&bcode_config::TuiConfig::default());
        let session_id = SessionId::new();
        let history = vec![
            session_event(
                session_id,
                SessionEventKind::UserMessage {
                    client_id: bcode_session_models::ClientId::new(),
                    text: "first prompt".to_string(),
                },
            ),
            session_event(
                session_id,
                SessionEventKind::AssistantMessage {
                    text: "reply".to_string(),
                },
            ),
            session_event(
                session_id,
                SessionEventKind::UserMessage {
                    client_id: bcode_session_models::ClientId::new(),
                    text: "second prompt".to_string(),
                },
            ),
        ];
        let mut app = ChatApp::new(session_id, &history, &keymap);

        app.previous_input_history();
        assert_eq!(app.input.text(), "second prompt");
        app.previous_input_history();
        assert_eq!(app.input.text(), "first prompt");
    }

    #[test]
    fn input_history_next_restores_original_draft() {
        let keymap = KeyMap::from_config(&bcode_config::TuiConfig::default());
        let session_id = SessionId::new();
        let history = vec![
            session_event(
                session_id,
                SessionEventKind::UserMessage {
                    client_id: bcode_session_models::ClientId::new(),
                    text: "first prompt".to_string(),
                },
            ),
            session_event(
                session_id,
                SessionEventKind::UserMessage {
                    client_id: bcode_session_models::ClientId::new(),
                    text: "second prompt".to_string(),
                },
            ),
        ];
        let mut app = ChatApp::new(session_id, &history, &keymap);
        app.input = TextEditBuffer::from_text("draft prompt");

        app.previous_input_history();
        app.previous_input_history();
        assert_eq!(app.input.text(), "first prompt");
        app.next_input_history();
        assert_eq!(app.input.text(), "second prompt");
        app.next_input_history();

        assert_eq!(app.input.text(), "draft prompt");
        assert_eq!(app.input_history_index, None);
    }

    #[test]
    fn default_keymap_includes_standard_composer_editor_bindings() {
        let keymap = KeyMap::from_config(&bcode_config::TuiConfig::default());
        let cases = [
            (
                KeyCode::Up,
                KeyModifiers::NONE,
                TuiAction::InputHistoryPrevious,
            ),
            (
                KeyCode::Down,
                KeyModifiers::NONE,
                TuiAction::InputHistoryNext,
            ),
            (KeyCode::Left, KeyModifiers::NONE, TuiAction::MoveCursorLeft),
            (
                KeyCode::Right,
                KeyModifiers::NONE,
                TuiAction::MoveCursorRight,
            ),
            (
                KeyCode::Left,
                KeyModifiers::ALT,
                TuiAction::MoveCursorWordLeft,
            ),
            (
                KeyCode::Right,
                KeyModifiers::ALT,
                TuiAction::MoveCursorWordRight,
            ),
            (
                KeyCode::Backspace,
                KeyModifiers::ALT,
                TuiAction::DeleteWordBackward,
            ),
            (
                KeyCode::Char('w'),
                KeyModifiers::CONTROL,
                TuiAction::DeleteWordBackward,
            ),
            (
                KeyCode::Delete,
                KeyModifiers::ALT,
                TuiAction::DeleteWordForward,
            ),
            (
                KeyCode::Char('u'),
                KeyModifiers::CONTROL,
                TuiAction::DeleteToStart,
            ),
            (
                KeyCode::Char('k'),
                KeyModifiers::CONTROL,
                TuiAction::DeleteToEnd,
            ),
        ];

        for (code, modifiers, action) in cases {
            assert_eq!(
                keymap.action_for_key(TuiScope::Chat, &KeyEvent::new(code, modifiers)),
                Some(action)
            );
        }
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

    #[test]
    fn transcript_viewport_bottom_anchors_after_wrapped_lines() {
        let lines = vec![
            Line::from("header"),
            Line::from("wrapped ".repeat(20)),
            Line::from("FINAL"),
        ];

        let rows = render_viewport_rows(&lines, 10, 3, 0);

        assert!(
            rows.last().is_some_and(|row| row.contains("FINAL")),
            "expected final line at bottom after wrapping, got {rows:?}"
        );
    }

    #[test]
    fn scroll_to_line_uses_wrapped_visual_rows() {
        let keymap = KeyMap::from_config(&bcode_config::TuiConfig::default());
        let mut app = ChatApp::new(SessionId::new(), &[], &keymap);
        app.blocks.push(TranscriptBlock::System {
            text: format!("{}\nNEEDLE", "wrapped ".repeat(20)),
        });
        app.last_transcript_width.set(10);
        app.last_transcript_height.set(3);
        let needle_index = app
            .rendered_line_texts()
            .iter()
            .position(|line| line.contains("NEEDLE"))
            .expect("needle line should render");

        app.scroll_to_line(needle_index);
        let rows = render_viewport_rows(
            &app.rendered_transcript_lines(),
            10,
            3,
            app.scroll_rows_from_bottom,
        );

        assert!(
            rows.iter().any(|row| row.contains("NEEDLE")),
            "expected scroll target to be visible, got {rows:?}"
        );
    }

    #[test]
    fn history_replay_does_not_leave_activity_active() {
        let keymap = KeyMap::from_config(&bcode_config::TuiConfig::default());
        let session_id = SessionId::new();
        let history = vec![session_event(
            session_id,
            SessionEventKind::UserMessage {
                client_id: bcode_session_models::ClientId::new(),
                text: "hello".to_string(),
            },
        )];

        let app = ChatApp::new(session_id, &history, &keymap);

        assert_eq!(app.activity, ActivityState::Idle);
    }

    #[test]
    fn large_delta_history_replay_restores_final_assistant_message() {
        let keymap = KeyMap::from_config(&bcode_config::TuiConfig::default());
        let session_id = SessionId::new();
        let mut history = (0..2_000)
            .map(|index| {
                session_event(
                    session_id,
                    SessionEventKind::AssistantDelta {
                        text: format!("{index} "),
                    },
                )
            })
            .collect::<Vec<_>>();
        history.push(session_event(
            session_id,
            SessionEventKind::AssistantMessage {
                text: "complete".to_string(),
            },
        ));

        let app = ChatApp::new(session_id, &history, &keymap);

        assert_eq!(app.activity, ActivityState::Idle);
        assert!(matches!(
            app.blocks.as_slice(),
            [TranscriptBlock::Assistant { text, streaming: false }] if text == "complete"
        ));
    }

    #[test]
    fn prepended_older_history_drops_completed_assistant_deltas() {
        let keymap = KeyMap::from_config(&bcode_config::TuiConfig::default());
        let session_id = SessionId::new();
        let mut app = ChatApp::new(session_id, &[], &keymap);
        let history = vec![
            session_event(
                session_id,
                SessionEventKind::AssistantDelta {
                    text: "BM".to_string(),
                },
            ),
            session_event(
                session_id,
                SessionEventKind::AssistantDelta {
                    text: "UX".to_string(),
                },
            ),
            session_event(
                session_id,
                SessionEventKind::AssistantMessage {
                    text: "BMUX".to_string(),
                },
            ),
        ];

        app.prepend_older_history(&history, false);

        assert!(matches!(
            app.blocks.as_slice(),
            [TranscriptBlock::Assistant { text, streaming: false }] if text == "BMUX"
        ));
    }

    #[test]
    fn prepending_older_history_without_overscroll_preserves_scroll_position() {
        let keymap = KeyMap::from_config(&bcode_config::TuiConfig::default());
        let session_id = SessionId::new();
        let mut app = ChatApp::new(session_id, &[], &keymap);
        app.blocks.push(TranscriptBlock::System {
            text: "current\n".repeat(20),
        });
        app.last_transcript_width.set(80);
        app.last_transcript_height.set(5);
        app.scroll_rows_from_bottom = 3;
        app.older_history_state = OlderHistoryState::More;

        app.prepend_older_history(
            &[session_event(
                session_id,
                SessionEventKind::SystemMessage {
                    text: "older\n".repeat(10),
                },
            )],
            true,
        );

        assert_eq!(app.scroll_rows_from_bottom, 3);
    }

    #[test]
    fn prepending_older_history_reveals_only_requested_overscroll_rows() {
        let keymap = KeyMap::from_config(&bcode_config::TuiConfig::default());
        let session_id = SessionId::new();
        let mut app = ChatApp::new(session_id, &[], &keymap);
        app.blocks.push(TranscriptBlock::System {
            text: "current\n".repeat(20),
        });
        app.last_transcript_width.set(80);
        app.last_transcript_height.set(5);
        app.scroll_rows_from_bottom = app.max_scroll_rows_from_bottom();
        app.older_history_state = OlderHistoryState::LoadRequested { reveal_rows: 2 };
        let before = app.scroll_rows_from_bottom;

        app.prepend_older_history(
            &[session_event(
                session_id,
                SessionEventKind::SystemMessage {
                    text: "older\n".repeat(10),
                },
            )],
            true,
        );

        assert_eq!(app.scroll_rows_from_bottom, before.saturating_add(2));
    }

    #[test]
    fn live_user_message_starts_thinking_activity() {
        let keymap = KeyMap::from_config(&bcode_config::TuiConfig::default());
        let session_id = SessionId::new();
        let mut app = ChatApp::new(session_id, &[], &keymap);

        app.push_event(Event::Session(session_event(
            session_id,
            SessionEventKind::UserMessage {
                client_id: bcode_session_models::ClientId::new(),
                text: "hello".to_string(),
            },
        )));

        assert_eq!(app.activity, ActivityState::Thinking);
        app.previous_input_history();
        assert_eq!(app.input.text(), "hello");
    }

    #[test]
    fn assistant_delta_sets_streaming_and_counts_chars() {
        let keymap = KeyMap::from_config(&bcode_config::TuiConfig::default());
        let session_id = SessionId::new();
        let mut app = ChatApp::new(session_id, &[], &keymap);

        app.push_event(Event::Session(session_event(
            session_id,
            SessionEventKind::AssistantDelta {
                text: "hello".to_string(),
            },
        )));
        app.push_event(Event::Session(session_event(
            session_id,
            SessionEventKind::AssistantDelta {
                text: "世界".to_string(),
            },
        )));

        assert_eq!(app.activity, ActivityState::Streaming { chars: 7 });
    }

    #[test]
    fn live_assistant_final_message_replaces_stream_after_invisible_events() {
        let keymap = KeyMap::from_config(&bcode_config::TuiConfig::default());
        let session_id = SessionId::new();
        let mut app = ChatApp::new(session_id, &[], &keymap);

        app.push_event(Event::Session(session_event(
            session_id,
            SessionEventKind::AssistantDelta {
                text: "hel".to_string(),
            },
        )));
        app.push_event(Event::Session(session_event(
            session_id,
            SessionEventKind::ModelUsage {
                turn_id: "turn".to_string(),
                usage: SessionTokenUsage::default(),
            },
        )));
        app.push_event(Event::Session(session_event(
            session_id,
            SessionEventKind::AssistantMessage {
                text: "hello".to_string(),
            },
        )));

        assert!(matches!(
            app.blocks.as_slice(),
            [TranscriptBlock::Assistant { text, streaming: false }] if text == "hello"
        ));
    }

    #[test]
    fn activity_tracks_tool_permission_and_finish() {
        let keymap = KeyMap::from_config(&bcode_config::TuiConfig::default());
        let session_id = SessionId::new();
        let mut app = ChatApp::new(session_id, &[], &keymap);

        app.push_event(Event::Session(session_event(
            session_id,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "tool-1".to_string(),
                tool_name: "shell.run".to_string(),
                arguments_json: r#"{"command":"cargo check"}"#.to_string(),
            },
        )));
        assert_eq!(
            app.activity,
            ActivityState::RunningTool {
                name: "shell.run".to_string()
            }
        );

        app.push_event(Event::Session(session_event(
            session_id,
            SessionEventKind::PermissionRequested {
                permission_id: "permission-1".to_string(),
                tool_call_id: "tool-1".to_string(),
                tool_name: "shell.run".to_string(),
                arguments_json: r#"{"command":"cargo check"}"#.to_string(),
            },
        )));
        assert_eq!(
            app.activity,
            ActivityState::WaitingPermission {
                name: "shell.run".to_string()
            }
        );

        app.push_event(Event::Session(session_event(
            session_id,
            SessionEventKind::PermissionResolved {
                permission_id: "permission-1".to_string(),
                approved: true,
            },
        )));
        assert_eq!(
            app.activity,
            ActivityState::RunningTool {
                name: "shell.run".to_string()
            }
        );

        app.push_event(Event::Session(session_event(
            session_id,
            SessionEventKind::ToolCallFinished {
                tool_call_id: "tool-1".to_string(),
                result: "ok".to_string(),
                is_error: false,
            },
        )));
        assert_eq!(app.activity, ActivityState::Thinking);
    }

    #[test]
    fn model_turn_lifecycle_controls_activity() {
        let keymap = KeyMap::from_config(&bcode_config::TuiConfig::default());
        let session_id = SessionId::new();
        let mut app = ChatApp::new(session_id, &[], &keymap);

        app.push_event(Event::Session(session_event(
            session_id,
            SessionEventKind::ModelTurnStarted {
                turn_id: "turn-1".to_string(),
            },
        )));
        assert_eq!(app.activity, ActivityState::Thinking);

        app.push_event(Event::Session(session_event(
            session_id,
            SessionEventKind::SystemMessage {
                text: "model warning should not finish activity".to_string(),
            },
        )));
        assert_eq!(app.activity, ActivityState::Thinking);

        app.push_event(Event::Session(session_event(
            session_id,
            SessionEventKind::ModelTurnFinished {
                turn_id: "turn-1".to_string(),
                outcome: ModelTurnOutcome::Error,
                message: Some("provider failed".to_string()),
            },
        )));
        assert_eq!(app.activity, ActivityState::Idle);
        assert_eq!(app.status, "provider failed");
    }

    #[test]
    fn compaction_trace_events_show_progress_activity() {
        let keymap = KeyMap::from_config(&bcode_config::TuiConfig::default());
        let session_id = SessionId::new();
        let mut app = ChatApp::new(session_id, &[], &keymap);

        app.push_event(Event::Session(session_event(
            session_id,
            SessionEventKind::TraceEvent {
                trace: Box::new(bcode_session_models::SessionTraceEvent {
                    timestamp_ms: 0,
                    turn_id: Some("turn-1".to_string()),
                    phase: SessionTracePhase::ContextCompactionStarted,
                    payload: SessionTracePayload::ContextCompaction {
                        reason: "chunk".to_string(),
                        projected_context_chars: 0,
                        compacted: false,
                        message: Some("compacting context chunk 1/3".to_string()),
                    },
                }),
            },
        )));

        assert_eq!(
            app.activity,
            ActivityState::Compacting {
                detail: "compacting context chunk 1/3".to_string()
            }
        );
        assert_eq!(app.status, "compacting context chunk 1/3");

        app.push_event(Event::Session(session_event(
            session_id,
            SessionEventKind::ContextCompacted {
                summary: "summary".to_string(),
                compacted_through_sequence: 1,
            },
        )));

        assert_eq!(app.activity, ActivityState::Thinking);
    }

    #[test]
    fn active_status_line_includes_spinner_and_label() {
        let keymap = KeyMap::from_config(&bcode_config::TuiConfig::default());
        let session_id = SessionId::new();
        let mut app = ChatApp::new(session_id, &[], &keymap);
        app.set_activity(ActivityState::Thinking);
        let area = Rect::new(0, 0, 80, 1);
        let mut buffer = ratatui::buffer::Buffer::empty(area);

        render_chat_status(&app, area, &mut buffer, 0, 0);
        let rendered = buffer_rows(&buffer).join("\n");

        assert!(rendered.contains("thinking"));
    }

    #[test]
    fn token_usage_history_aggregates_session_spend_and_context_pressure() {
        let keymap = KeyMap::from_config(&bcode_config::TuiConfig::default());
        let session_id = SessionId::new();
        let history = vec![session_event(
            session_id,
            SessionEventKind::ModelUsage {
                turn_id: "turn-1".to_string(),
                usage: SessionTokenUsage {
                    input_tokens: Some(2_000),
                    output_tokens: Some(500),
                    total_tokens: Some(2_500),
                    cached_input_tokens: None,
                    cache_write_input_tokens: None,
                    reasoning_tokens: None,
                },
            },
        )];
        let mut app = ChatApp::new(session_id, &history, &keymap);
        app.apply_model_status(bcode_ipc::SessionModelStatus {
            provider_plugin_id: Some("provider".to_string()),
            model_id: Some("model".to_string()),
            model: Some(bcode_model::ModelInfo {
                model_id: "model".to_string(),
                display_name: "Model".to_string(),
                is_default: true,
                context_window: Some(8_000),
                max_output_tokens: Some(1_000),
                capabilities: std::collections::BTreeSet::new(),
            }),
        });

        assert_eq!(
            app.token_usage.footer_summary(),
            "ctx 2.0k/8.0k 25% · spent 2.5k tok"
        );
    }

    #[test]
    fn token_usage_footer_is_visible_before_usage_arrives() {
        let app = ChatApp::new(
            SessionId::new(),
            &[],
            &KeyMap::from_config(&bcode_config::TuiConfig::default()),
        );

        assert_eq!(
            app.token_usage.footer_summary(),
            "ctx unknown · spent 0 tok"
        );
    }

    #[test]
    fn transcript_messages_do_not_render_nested_box_art() {
        let lines = TranscriptBlock::User {
            text: "hello".to_string(),
        }
        .render_lines("");
        let rendered = lines
            .iter()
            .map(line_plain_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("You"));
        assert!(rendered.contains("hello"));
        assert!(!rendered.contains('╭'));
        assert!(!rendered.contains('│'));
        assert!(!rendered.contains('╰'));
    }

    #[test]
    fn search_highlighting_survives_polished_transcript_rendering() {
        let lines = TranscriptBlock::Assistant {
            text: "find the needle".to_string(),
            streaming: false,
        }
        .render_lines("needle");

        assert!(lines.iter().any(|line| {
            line.spans.iter().any(|span| {
                span.content.as_ref() == "needle" && span.style.bg == Some(Color::Yellow)
            })
        }));
    }

    #[test]
    fn command_palette_filter_is_separate_from_composer_input() {
        let keymap = KeyMap::from_config(&bcode_config::TuiConfig::default());
        let mut app = ChatApp::new(SessionId::new(), &[], &keymap);
        app.input = TextEditBuffer::from_text("draft message");
        app.open_command_palette();
        if let Some(palette) = &mut app.command_palette {
            palette.filter.insert_char('m');
        }

        assert_eq!(app.input.text(), "draft message");
        assert_eq!(
            app.command_palette
                .as_ref()
                .map(|palette| palette.filter.text()),
            Some("m")
        );
    }

    #[test]
    fn command_palette_filter_supports_cursor_editing() {
        let mut palette = CommandPaletteState::new();
        palette.filter = TextEditBuffer::from_text("model switch");

        apply_command_palette_action(&mut palette, TuiAction::MoveCursorWordLeft);
        apply_command_palette_action(&mut palette, TuiAction::MoveCursorLeft);
        apply_command_palette_action(&mut palette, TuiAction::DeleteCharForward);
        palette.filter.insert_char('-');

        assert_eq!(palette.filter.text(), "model-switch");
        assert_eq!(palette.selected, 0);
    }

    #[test]
    fn command_palette_filter_supports_word_and_range_deletion() {
        let mut palette = CommandPaletteState::new();
        palette.filter = TextEditBuffer::from_text("switch provider bedrock");

        apply_command_palette_action(&mut palette, TuiAction::MoveCursorWordLeft);
        apply_command_palette_action(&mut palette, TuiAction::DeleteWordBackward);
        assert_eq!(palette.filter.text(), "switch bedrock");

        apply_command_palette_action(&mut palette, TuiAction::MoveCursorStart);
        apply_command_palette_action(&mut palette, TuiAction::MoveCursorWordRight);
        apply_command_palette_action(&mut palette, TuiAction::DeleteToEnd);
        assert_eq!(palette.filter.text(), "switch");
    }

    #[test]
    fn command_palette_cursor_tracks_mid_filter_cursor() {
        let keymap = KeyMap::from_config(&bcode_config::TuiConfig::default());
        let mut app = ChatApp::new(SessionId::new(), &[], &keymap);
        app.open_command_palette();
        if let Some(palette) = &mut app.command_palette {
            palette.filter = TextEditBuffer::from_text("model");
            palette.filter.move_cursor(TextMotion::Left);
            palette.filter.move_cursor(TextMotion::Left);
        }

        assert_eq!(
            app.cursor_position(Rect::new(0, 0, 100, 30)),
            Some(Position::new(14, 7))
        );
    }

    #[test]
    fn command_palette_unicode_cursor_uses_display_width() {
        let keymap = KeyMap::from_config(&bcode_config::TuiConfig::default());
        let mut app = ChatApp::new(SessionId::new(), &[], &keymap);
        app.open_command_palette();
        if let Some(palette) = &mut app.command_palette {
            palette.filter = TextEditBuffer::from_text("a界👋🏽e\u{301}");
            palette.filter.move_cursor(TextMotion::Left);
        }

        assert_eq!(
            app.cursor_position(Rect::new(0, 0, 100, 30)),
            Some(Position::new(16, 7))
        );
    }

    #[test]
    fn chat_frame_sets_cursor_at_composer_insert_position() {
        let keymap = KeyMap::from_config(&bcode_config::TuiConfig::default());
        let mut app = ChatApp::new(SessionId::new(), &[], &keymap);
        let backend = ratatui::backend::TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");

        terminal
            .draw(|frame| render_chat_frame(frame, &app))
            .expect("frame should render");
        terminal
            .backend_mut()
            .assert_cursor_position(Position::new(1, 17));

        app.input = TextEditBuffer::from_text("Hello");
        terminal
            .draw(|frame| render_chat_frame(frame, &app))
            .expect("frame should render");
        terminal
            .backend_mut()
            .assert_cursor_position(Position::new(6, 17));
    }

    #[test]
    fn composer_unicode_cursor_uses_display_width() {
        let layout = composer_layout(
            Rect::new(0, 0, 20, 4),
            &TextEditBuffer::from_text("a界👋🏽e\u{301}"),
            false,
        );

        assert_eq!(layout.cursor_position, Some(Position::new(7, 1)));
    }

    #[test]
    fn composer_unicode_wrapping_keeps_cursor_visible() {
        let layout = composer_layout(
            Rect::new(0, 0, 6, 5),
            &TextEditBuffer::from_text("a界👋🏽e\u{301}b"),
            false,
        );
        let lines = layout
            .text
            .lines
            .iter()
            .map(line_plain_text)
            .collect::<Vec<_>>();

        assert_eq!(lines, vec!["a界", "👋🏽e\u{301}b"]);
        assert_eq!(layout.cursor_position, Some(Position::new(4, 2)));
    }

    #[test]
    fn composer_cursor_tracks_multiline_input() {
        let keymap = KeyMap::from_config(&bcode_config::TuiConfig::default());
        let mut app = ChatApp::new(SessionId::new(), &[], &keymap);
        app.input = TextEditBuffer::from_text("one\ntwo");

        assert_eq!(
            app.cursor_position(Rect::new(0, 0, 80, 20)),
            Some(Position::new(4, 17))
        );
    }

    #[test]
    fn composer_cursor_moves_to_blank_row_at_exact_wrap_boundary() {
        let layout = composer_layout(
            Rect::new(0, 0, 12, 4),
            &TextEditBuffer::from_text("abcdefghij"),
            false,
        );
        let lines = layout
            .text
            .lines
            .iter()
            .map(line_plain_text)
            .collect::<Vec<_>>();

        assert_eq!(lines, vec!["abcdefghij"]);
        assert_eq!(layout.cursor_position, Some(Position::new(10, 1)));
    }

    #[test]
    fn composer_layout_scrolls_to_keep_cursor_visible() {
        let layout = composer_layout(
            Rect::new(0, 0, 8, 5),
            &TextEditBuffer::from_text("one\ntwo\nthree\nfour"),
            false,
        );
        let lines = layout
            .text
            .lines
            .iter()
            .map(line_plain_text)
            .collect::<Vec<_>>();

        assert_eq!(lines, vec!["two", "three", "four"]);
        assert_eq!(layout.cursor_position, Some(Position::new(5, 3)));
    }

    #[test]
    fn command_palette_cursor_tracks_filter_input() {
        let keymap = KeyMap::from_config(&bcode_config::TuiConfig::default());
        let mut app = ChatApp::new(SessionId::new(), &[], &keymap);
        app.open_command_palette();
        if let Some(palette) = &mut app.command_palette {
            palette.filter = TextEditBuffer::from_text("mo");
        }

        assert_eq!(
            app.cursor_position(Rect::new(0, 0, 100, 30)),
            Some(Position::new(13, 7))
        );
    }

    #[test]
    fn permission_prompt_does_not_show_text_cursor() {
        let keymap = KeyMap::from_config(&bcode_config::TuiConfig::default());
        let mut app = ChatApp::new(SessionId::new(), &[], &keymap);
        app.pending_permissions.insert(
            "permission-123".to_string(),
            PendingPermissionView {
                permission_id: "permission-123".to_string(),
                tool_call_id: "tool-call-456".to_string(),
                tool_name: "shell.run".to_string(),
                arguments_json: r#"{"command":"cargo check -p bcode_tui"}"#.to_string(),
            },
        );

        assert_eq!(app.cursor_position(Rect::new(0, 0, 80, 20)), None);
    }

    #[test]
    fn command_palette_renders_empty_state() {
        let mut palette = CommandPaletteState::new();
        palette.filter = TextEditBuffer::from_text("missing");
        palette.commands = Vec::new();
        palette.is_loading = false;
        let area = Rect::new(0, 0, 80, 20);
        let mut buffer = ratatui::buffer::Buffer::empty(area);

        render_command_palette(area, &mut buffer, &palette);
        let rendered = buffer_rows(&buffer).join("\n");

        assert!(rendered.contains("Command Palette"));
        assert!(rendered.contains("No commands match"));
    }

    #[test]
    fn rect_contains_excludes_right_and_bottom_edges() {
        let rect = Rect::new(4, 3, 10, 5);

        assert!(rect_contains(rect, 4, 3));
        assert!(rect_contains(rect, 13, 7));
        assert!(!rect_contains(rect, 14, 7));
        assert!(!rect_contains(rect, 13, 8));
        assert!(!rect_contains(rect, 3, 3));
    }

    #[test]
    fn mouse_wheel_scrolls_transcript_inside_transcript_area() {
        let keymap = KeyMap::from_config(&bcode_config::TuiConfig::default());
        let mut app = ChatApp::new(SessionId::new(), &[], &keymap);
        app.blocks.push(TranscriptBlock::Assistant {
            text: (0..20)
                .map(|index| format!("line {index}"))
                .collect::<Vec<_>>()
                .join("\n"),
            streaming: false,
        });
        app.last_transcript_width.set(40);
        app.last_transcript_height.set(4);
        app.last_transcript_area.set(Rect::new(0, 1, 40, 10));

        handle_mouse_event(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 10,
                row: 5,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(app.scroll_rows_from_bottom, MOUSE_SCROLL_ROWS);

        handle_mouse_event(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 10,
                row: 5,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(app.scroll_rows_from_bottom, 0);
    }

    #[test]
    fn mouse_wheel_ignores_outside_transcript_and_open_modals() {
        let keymap = KeyMap::from_config(&bcode_config::TuiConfig::default());
        let mut app = ChatApp::new(SessionId::new(), &[], &keymap);
        app.blocks.push(TranscriptBlock::Assistant {
            text: "line\n".repeat(20),
            streaming: false,
        });
        app.last_transcript_width.set(40);
        app.last_transcript_height.set(4);
        app.last_transcript_area.set(Rect::new(0, 1, 40, 10));

        handle_mouse_event(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 41,
                row: 5,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(app.scroll_rows_from_bottom, 0);

        app.open_command_palette();
        handle_mouse_event(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 10,
                row: 5,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(app.scroll_rows_from_bottom, 0);
    }

    fn session_summary_with_name(name: &str) -> SessionSummary {
        SessionSummary {
            id: SessionId::new(),
            name: Some(name.to_string()),
            client_count: 0,
        }
    }

    fn rename_input_text(app: &SessionPickerApp) -> Option<&str> {
        match &app.mode {
            SessionPickerMode::Renaming { input } => Some(input.text()),
            SessionPickerMode::Browsing | SessionPickerMode::ConfirmDelete => None,
        }
    }

    fn apply_session_rename_key(app: &mut SessionPickerApp, key: KeyEvent) {
        let client = BcodeClient::new(bcode_ipc::IpcEndpoint::unix_socket(
            "/tmp/bcode-unused-test.sock",
        ));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime should build");
        runtime
            .block_on(handle_session_picker_text_key(&client, app, &key))
            .expect("rename key should be handled");
    }

    #[test]
    fn session_rename_input_supports_cursor_editing() {
        let mut app = SessionPickerApp::new(&[session_summary_with_name("hello world")]);
        app.selected = 1;
        app.start_rename();

        apply_session_rename_key(&mut app, KeyEvent::new(KeyCode::Left, KeyModifiers::ALT));
        apply_session_rename_key(
            &mut app,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT),
        );
        apply_session_rename_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE),
        );

        assert_eq!(rename_input_text(&app), Some("bworld"));
    }

    #[test]
    fn session_rename_cursor_position_tracks_editor_cursor() {
        let mut app = SessionPickerApp::new(&[session_summary_with_name("model")]);
        app.selected = 1;
        app.start_rename();
        if let SessionPickerMode::Renaming { input } = &mut app.mode {
            input.move_cursor(TextMotion::Left);
            input.move_cursor(TextMotion::Left);
        }

        assert_eq!(
            app.cursor_position(Rect::new(0, 0, 100, 40)),
            Some(Position::new(20, 34))
        );
    }

    #[test]
    fn session_rename_unicode_cursor_uses_display_width() {
        let mut app = SessionPickerApp::new(&[session_summary_with_name("a界👋🏽e\u{301}")]);
        app.selected = 1;
        app.start_rename();
        if let SessionPickerMode::Renaming { input } = &mut app.mode {
            input.move_cursor(TextMotion::Left);
        }

        assert_eq!(
            app.cursor_position(Rect::new(0, 0, 100, 40)),
            Some(Position::new(22, 34))
        );
    }

    #[test]
    fn permission_modal_renders_action_summary_and_selected_choice() {
        let permission = PendingPermissionView {
            permission_id: "permission-123".to_string(),
            tool_call_id: "tool-call-456".to_string(),
            tool_name: "shell.run".to_string(),
            arguments_json: r#"{"command":"cargo check -p bcode_tui"}"#.to_string(),
        };
        let area = Rect::new(0, 0, 96, 24);
        let mut buffer = ratatui::buffer::Buffer::empty(area);

        render_permission_modal(
            area,
            &mut buffer,
            &permission,
            "y allow once · n deny",
            PermissionChoice::AllowOnce,
            1,
        );
        let rendered = buffer_rows(&buffer).join("\n");

        assert!(rendered.contains("Permission required"));
        assert!(rendered.contains("shell.run"));
        assert!(rendered.contains("cargo check -p bcode_tui"));
        assert!(rendered.contains("allow once"));
    }

    fn session_event(session_id: SessionId, kind: SessionEventKind) -> SessionEvent {
        SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 1,
            session_id,
            kind,
        }
    }

    fn render_viewport_rows(
        lines: &[Line<'static>],
        width: u16,
        height: u16,
        scroll_rows_from_bottom: usize,
    ) -> Vec<String> {
        let viewport = transcript_viewport(lines, width, height, scroll_rows_from_bottom);
        let area = ratatui::layout::Rect::new(0, 0, width, height);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        let paragraph = Paragraph::new(Text::from(viewport.lines))
            .wrap(TRANSCRIPT_WRAP)
            .scroll((usize_to_u16_saturating(viewport.local_scroll_y), 0));
        ratatui::widgets::Widget::render(paragraph, area, &mut buffer);
        buffer_rows(&buffer)
    }

    fn buffer_rows(buffer: &ratatui::buffer::Buffer) -> Vec<String> {
        (0..buffer.area.height)
            .map(|y| {
                let mut row = String::new();
                for x in 0..buffer.area.width {
                    row.push_str(buffer[(x, y)].symbol());
                }
                row
            })
            .collect()
    }

    fn view(tool_name: &str, arguments_json: &str) -> PendingPermissionView {
        PendingPermissionView {
            permission_id: "perm-1".to_string(),
            tool_call_id: "call-1".to_string(),
            tool_name: tool_name.to_string(),
            arguments_json: arguments_json.to_string(),
        }
    }

    #[test]
    fn shell_policy_rules_persist_literal_and_broadened_glob() {
        let rules = view("shell.run", r#"{"command":"echo hello"}"#).policy_rules();

        assert_eq!(
            rules,
            vec![
                ("bash", "echo hello".to_string()),
                ("bash", "echo *".to_string()),
            ]
        );
    }

    #[test]
    fn shell_policy_rules_handle_single_word_command() {
        let rules = view("shell.run", r#"{"command":"ls"}"#).policy_rules();

        assert_eq!(
            rules,
            vec![("bash", "ls".to_string()), ("bash", "ls *".to_string()),]
        );
    }

    #[test]
    fn shell_policy_rules_collapse_when_broadening_matches_literal() {
        // Literal already ends in `*`; broadened form would be identical, so skip it.
        let rules = view("shell.run", r#"{"command":"cargo *"}"#).policy_rules();

        assert_eq!(rules, vec![("bash", "cargo *".to_string())]);
    }

    #[test]
    fn shell_policy_rules_preserve_leading_path_command() {
        let rules = view("shell.run", r#"{"command":"./scripts/build.sh --prod"}"#).policy_rules();

        assert_eq!(
            rules,
            vec![
                ("bash", "./scripts/build.sh --prod".to_string()),
                ("bash", "./scripts/build.sh *".to_string()),
            ]
        );
    }

    #[test]
    fn filesystem_write_policy_rule_stays_literal() {
        let rules = view(
            "filesystem.write",
            r#"{"path":"src/foo.rs","content":"..."}"#,
        )
        .policy_rules();

        assert_eq!(rules, vec![("write", "src/foo.rs".to_string())]);
    }

    #[test]
    fn filesystem_edit_policy_rule_stays_literal() {
        let rules = view("filesystem.edit", r#"{"path":"docs/readme.md"}"#).policy_rules();

        assert_eq!(rules, vec![("edit", "docs/readme.md".to_string())]);
    }

    #[test]
    fn filesystem_read_policy_rule_stays_literal() {
        let rules = view("filesystem.read", r#"{"path":"Cargo.toml"}"#).policy_rules();

        assert_eq!(rules, vec![("read", "Cargo.toml".to_string())]);
    }

    #[test]
    fn empty_shell_command_falls_back_to_literal_tool_name() {
        let rules = view("shell.run", r#"{"command":""}"#).policy_rules();

        assert_eq!(rules, vec![("bash", "shell.run".to_string())]);
    }
}
