//! Terminal user interface for Bcode.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

pub(crate) mod activity;
pub(crate) mod app;
pub(crate) mod artifact_stream;
pub(crate) mod chat_loop;
pub(crate) mod clipboard_image;
pub mod code_review_launcher;
pub(crate) mod command_palette;
pub(crate) mod command_palette_render;
pub(crate) mod composer_flow;
pub(crate) mod cursor_blink;
pub(crate) mod daemon_host;
pub(crate) mod daemon_issue;
pub(crate) mod effects;
pub mod eval_launcher;
pub(crate) mod exit_state;
pub(crate) mod filtered_list;
pub(crate) mod helpers;
pub(crate) mod history_flow;
pub(crate) mod input;
pub(crate) mod input_history;
pub(crate) mod interactive_surface;
pub(crate) mod invalidation;
pub(crate) mod keymap;
pub mod metrics_launcher;
pub(crate) mod model_flow;
pub(crate) mod model_picker;
pub(crate) mod model_picker_render;
pub(crate) mod mouse_flow;
pub(crate) mod older_history;
pub mod onboarding;
pub(crate) mod onboarding_render;
pub(crate) mod palette_flow;
pub(crate) mod pending_submission;
pub(crate) mod pending_submissions;
pub(crate) mod permission_dialog;
pub(crate) mod permission_dialog_render;
pub(crate) mod permission_flow;
pub(crate) mod permission_present;
pub(crate) mod picker_mouse;
pub(crate) mod picker_render;
#[cfg(test)]
mod plugin_command_architecture_tests;
pub(crate) mod plugin_surface_host;
pub mod plugin_tui;
pub(crate) mod provider_picker;
pub(crate) mod provider_picker_render;
pub(crate) mod ralph_flow;
pub mod ralph_launcher;
pub(crate) mod ralph_start_dialog;
pub(crate) mod ralph_start_dialog_render;
pub(crate) mod render;
pub(crate) mod runtime;
mod runtime_context;
pub(crate) mod session_flow;
pub(crate) mod session_fork_dialog;
pub(crate) mod session_fork_dialog_render;
pub(crate) mod session_fork_flow;
pub(crate) mod session_picker;
pub(crate) mod session_picker_render;
pub(crate) mod setup_board;
pub(crate) mod skill_flow;
pub(crate) mod skill_picker;
pub(crate) mod skill_picker_render;
pub(crate) mod slash_commands;
pub(crate) mod slash_flow;
pub(crate) mod slash_palette;
pub(crate) mod slash_palette_render;
pub(crate) mod slash_registry;
pub(crate) mod startup_action;
pub(crate) mod temporal;
pub(crate) mod terminal_events;
#[cfg(test)]
pub(crate) mod tests;
pub(crate) mod text_input_flow;
pub(crate) mod theme;
mod thinking_dialog;
pub(crate) mod thinking_dialog_render;
pub(crate) mod thinking_flow;
pub(crate) mod time_format;
pub(crate) mod timeline_dialog;
pub(crate) mod timeline_dialog_render;
pub(crate) mod timeline_flow;
pub(crate) mod tool_render_projection;
#[cfg(test)]
mod tool_render_projection_tests;
pub(crate) mod transcript;
pub(crate) mod transcript_document;
pub(crate) mod transcript_layout;
pub(crate) mod transcript_projection;
pub(crate) mod transcript_resident_window;
pub(crate) mod transcript_viewport;
pub(crate) mod worktree_flow;
pub(crate) mod wt_create_dialog;
pub(crate) mod wt_create_dialog_render;

use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use bcode_session_models::SessionId;
use bmux_tui::crossterm::CrosstermTerminalGuard;
use bmux_tui::event::{
    Event as BmuxEvent, MouseButton as BmuxMouseButton, MouseEvent as BmuxMouseEvent,
    MouseEventKind as BmuxMouseEventKind, MouseModifiers as BmuxMouseModifiers,
};
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::terminal::Terminal;
use crossterm::event::{
    self as crossterm_event, Event as CrosstermEvent, KeyCode as CrosstermKeyCode,
    MouseButton as CrosstermMouseButton, MouseEvent as CrosstermMouseEvent,
    MouseEventKind as CrosstermMouseEventKind,
};

const CURSOR_BLINK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
const OLDER_HISTORY_EVENT_LIMIT: usize = 256;

/// Errors returned by the TUI.
#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    /// Client error.
    #[error("client error: {0}")]
    Client(#[from] bcode_client::ClientError),
    /// Config error.
    #[error("config error: {0}")]
    Config(#[from] bcode_config::ConfigError),
    /// Settings error.
    #[error("settings error: {0}")]
    Settings(#[from] bcode_settings::SettingsError),
    /// JSON error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Task join error.
    #[error("task join error: {0}")]
    Join(#[from] tokio::task::JoinError),
    /// Plugin service error.
    #[error("plugin service error {code}: {message}")]
    PluginService { code: String, message: String },
    /// Ralph state error.
    #[error("Ralph state error: {0}")]
    RalphState(#[from] bcode_ralph::RalphStateError),
    /// Session storage is unavailable for normal runtime access.
    #[error("session unavailable: {session_id}: {reason}")]
    SessionUnavailable {
        session_id: SessionId,
        reason: String,
    },
    /// Session selection was canceled.
    #[error("session selection canceled")]
    Canceled,
}

/// Run the first-run onboarding setup-map interface.
///
/// # Errors
///
/// Returns I/O, settings, or config errors.
pub fn run_onboarding() -> Result<(), TuiError> {
    let store = bcode_settings::SettingsStore::default();
    let detection = bcode_settings::detect_setup_environment(current_time_ms());
    store.save_setup_detection_snapshot(&detection)?;
    let config = bcode_config::load_config()?;
    let auth_detection = bcode_settings::detect_auth_security_from_config(&config);
    let secure_import_plans =
        bcode_settings::secure_import_plans_from_detection(&detection.entries);
    let secure_story =
        bcode_settings::secure_credential_story_panel(&secure_import_plans, &auth_detection);
    let draft = store.onboarding_draft_setup()?;
    let questionnaire = bcode_settings::deterministic_onboarding_questionnaire(&draft, &detection);
    store.put_control_state(
        "onboarding.questionnaire",
        &serde_json::to_value(&questionnaire)?,
        current_time_ms(),
    )?;
    store.put_control_state(
        "onboarding.secure_credential_story",
        &serde_json::to_value(&secure_story)?,
        current_time_ms(),
    )?;
    let summary = bcode_settings::SetupConfigSummary::from_config(&config);
    let mut shell = onboarding::OnboardingShell::load(&store, &summary)?;
    let recommendations = store.setup_recommendations()?;
    let readiness = bcode_settings::setup_readiness_report(shell.sections(), &recommendations);
    store.save_readiness_report(&readiness, current_time_ms())?;
    let stdout = io::stdout();
    let mut guard = CrosstermTerminalGuard::enter(stdout)?;
    let result = {
        let mut terminal = Terminal::new(
            guard.writer_mut().ok_or_else(|| {
                std::io::Error::other("terminal guard writer unavailable after entering terminal")
            })?,
            helpers::terminal_area()?,
        );
        run_onboarding_loop(&mut terminal, &store, &mut shell)
    };
    let _writer = guard.leave()?;
    result
}

fn run_onboarding_loop<W: io::Write>(
    terminal: &mut Terminal<&mut W>,
    store: &bcode_settings::SettingsStore,
    shell: &mut onboarding::OnboardingShell,
) -> Result<(), TuiError> {
    loop {
        terminal.resize(helpers::terminal_area()?);
        let health = store.health();
        let readiness = store.readiness_report()?;
        terminal.draw(|frame| {
            onboarding_render::render_onboarding(shell, frame, &health, readiness.clone());
        })?;
        match crossterm_event::read()? {
            CrosstermEvent::Resize(width, height) => {
                terminal.resize(Rect::new(0, 0, width, height));
            }
            CrosstermEvent::Key(key) => {
                if handle_onboarding_key(key.code, store, shell)? {
                    return Ok(());
                }
            }
            CrosstermEvent::Mouse(mouse) => {
                let board_area = onboarding_render::onboarding_board_area(terminal.area());
                let event = BmuxEvent::Mouse(convert_onboarding_mouse(mouse));
                let _outcome = shell.handle_board_event(board_area, &event);
            }
            CrosstermEvent::FocusGained | CrosstermEvent::FocusLost | CrosstermEvent::Paste(_) => {}
        }
    }
}

fn handle_onboarding_key(
    code: CrosstermKeyCode,
    store: &bcode_settings::SettingsStore,
    shell: &mut onboarding::OnboardingShell,
) -> Result<bool, TuiError> {
    match code {
        CrosstermKeyCode::Esc | CrosstermKeyCode::Char('q') => {
            shell.handle_action(
                onboarding::OnboardingInputAction::CancelConfirmation,
                store,
                current_time_ms(),
            )?;
            Ok(true)
        }
        CrosstermKeyCode::Right | CrosstermKeyCode::Down | CrosstermKeyCode::Char('j') => {
            shell.focus_next();
            Ok(false)
        }
        CrosstermKeyCode::Left | CrosstermKeyCode::Up | CrosstermKeyCode::Char('k') => {
            shell.focus_previous();
            Ok(false)
        }
        _ => {
            if let Some(action) = onboarding_action_for_key(code) {
                shell.handle_action(action, store, current_time_ms())?;
            }
            Ok(false)
        }
    }
}

const fn onboarding_action_for_key(
    code: CrosstermKeyCode,
) -> Option<onboarding::OnboardingInputAction> {
    match code {
        CrosstermKeyCode::Enter => Some(onboarding::OnboardingInputAction::Select),
        CrosstermKeyCode::Char('p') => Some(onboarding::OnboardingInputAction::ToggleProvider),
        CrosstermKeyCode::Char('a') => Some(onboarding::OnboardingInputAction::ToggleAuthProfile),
        CrosstermKeyCode::Char('m') => Some(onboarding::OnboardingInputAction::SelectModelProfile),
        CrosstermKeyCode::Char('r') => {
            Some(onboarding::OnboardingInputAction::CyclePermissionPreset)
        }
        CrosstermKeyCode::Char('i') => Some(onboarding::OnboardingInputAction::ReviewSessionImport),
        CrosstermKeyCode::Char('g') => Some(onboarding::OnboardingInputAction::ReviewPlugins),
        CrosstermKeyCode::Char('x') => Some(onboarding::OnboardingInputAction::ApplyPlan),
        CrosstermKeyCode::Char('y') => Some(onboarding::OnboardingInputAction::Confirm),
        CrosstermKeyCode::Char('n') => Some(onboarding::OnboardingInputAction::CancelConfirmation),
        CrosstermKeyCode::Char('c') => Some(onboarding::OnboardingInputAction::Complete),
        CrosstermKeyCode::Char('s') => Some(onboarding::OnboardingInputAction::Skip),
        CrosstermKeyCode::Char('l') => Some(onboarding::OnboardingInputAction::Launch),
        _ => None,
    }
}

const fn convert_onboarding_mouse(mouse: CrosstermMouseEvent) -> BmuxMouseEvent {
    BmuxMouseEvent {
        kind: convert_onboarding_mouse_kind(mouse.kind),
        position: Point::new(mouse.column, mouse.row),
        modifiers: BmuxMouseModifiers {
            shift: mouse
                .modifiers
                .contains(crossterm::event::KeyModifiers::SHIFT),
            alt: mouse
                .modifiers
                .contains(crossterm::event::KeyModifiers::ALT),
            ctrl: mouse
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL),
        },
    }
}

const fn convert_onboarding_mouse_kind(kind: CrosstermMouseEventKind) -> BmuxMouseEventKind {
    match kind {
        CrosstermMouseEventKind::Down(button) => {
            BmuxMouseEventKind::Down(convert_mouse_button(button))
        }
        CrosstermMouseEventKind::Up(button) => BmuxMouseEventKind::Up(convert_mouse_button(button)),
        CrosstermMouseEventKind::Drag(button) => {
            BmuxMouseEventKind::Drag(convert_mouse_button(button))
        }
        CrosstermMouseEventKind::Moved => BmuxMouseEventKind::Move,
        CrosstermMouseEventKind::ScrollUp => BmuxMouseEventKind::ScrollUp,
        CrosstermMouseEventKind::ScrollDown => BmuxMouseEventKind::ScrollDown,
        CrosstermMouseEventKind::ScrollLeft => BmuxMouseEventKind::ScrollLeft,
        CrosstermMouseEventKind::ScrollRight => BmuxMouseEventKind::ScrollRight,
    }
}

const fn convert_mouse_button(button: CrosstermMouseButton) -> BmuxMouseButton {
    match button {
        CrosstermMouseButton::Left => BmuxMouseButton::Left,
        CrosstermMouseButton::Right => BmuxMouseButton::Right,
        CrosstermMouseButton::Middle => BmuxMouseButton::Middle,
    }
}

fn current_time_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

/// Return statically bundled plugin registrations enabled through `bcode_bundled_plugins`.
#[must_use]
pub fn static_bundled_plugins() -> Vec<bcode_plugin::StaticBundledPlugin> {
    bcode_bundled_plugins::static_bundled_plugins()
}

/// Run the main terminal UI and open a plugin-owned surface on startup.
///
/// # Errors
///
/// Returns I/O or plugin service errors, or an error when the surface does not
/// yet have a full-screen startup flow.
#[allow(clippy::future_not_send)]
pub async fn run_plugin_surface(
    surface_kind: String,
    repo_path: Option<std::path::PathBuf>,
    options: std::collections::BTreeMap<String, String>,
) -> Result<(), TuiError> {
    if surface_kind == "ralph-home" {
        return run_ralph_home().await;
    }
    if surface_kind == "code-review" {
        let repo = repo_path.unwrap_or_else(|| std::path::PathBuf::from("."));
        if let Some(target) = options.get("target") {
            return run_code_review(repo, serde_json::from_str(target)?).await;
        }
        return run_code_review_home(repo).await;
    }
    if surface_kind == "eval-run-picker" {
        return run_eval_viewer_picker(repo_path.unwrap_or_else(|| std::path::PathBuf::from(".")))
            .await;
    }
    if surface_kind == "eval-run-viewer" {
        return run_eval_viewer(
            repo_path.unwrap_or_else(|| std::path::PathBuf::from(".")),
            options.get("run").map(std::path::PathBuf::from),
        )
        .await;
    }
    if surface_kind == "metrics-dashboard" {
        return run_metrics_dashboard(
            repo_path.unwrap_or_else(|| std::path::PathBuf::from(".")),
            options.get("metrics_path").map(std::path::PathBuf::from),
        )
        .await;
    }
    Err(TuiError::PluginService {
        code: "unsupported_startup_surface".to_owned(),
        message: format!("plugin surface `{surface_kind}` cannot be opened as a startup surface"),
    })
}

/// Run the main terminal user interface and open Ralph on startup.
///
/// # Errors
///
/// Returns I/O or plugin service errors.
#[allow(clippy::future_not_send)]
pub async fn run_ralph_home() -> Result<(), TuiError> {
    let stdout = io::stdout();
    let mut guard = CrosstermTerminalGuard::enter(stdout)?;
    let result = {
        let mut terminal = Terminal::new(
            guard.writer_mut().ok_or_else(|| {
                std::io::Error::other("terminal guard writer unavailable after entering terminal")
            })?,
            helpers::terminal_area()?,
        );
        Box::pin(runtime::run_event_loop_with_startup_and_static_bundled(
            &mut terminal,
            None,
            startup_action::StartupTuiAction::OpenRalphHome,
            &static_bundled_plugins(),
        ))
        .await
    };
    let _writer = guard.leave()?;
    result
}

/// Run the terminal user interface.
///
/// # Errors
///
/// Returns I/O errors from terminal setup, event polling, drawing, or Bcode
/// client operations.
#[allow(clippy::future_not_send)]
pub async fn run(session_id: Option<SessionId>) -> Result<(), TuiError> {
    Box::pin(run_with_static_bundled(
        session_id,
        &static_bundled_plugins(),
    ))
    .await
}

/// Run the terminal user interface with caller-provided static bundled plugins.
///
/// # Errors
///
/// Returns I/O errors from terminal setup, event polling, drawing, or Bcode
/// client/plugin operations.
#[allow(clippy::future_not_send)]
pub async fn run_with_static_bundled(
    session_id: Option<SessionId>,
    static_plugins: &[bcode_plugin::StaticBundledPlugin],
) -> Result<(), TuiError> {
    let stdout = io::stdout();
    let mut guard = CrosstermTerminalGuard::enter(stdout)?;
    let result = {
        let mut terminal = Terminal::new(
            guard.writer_mut().ok_or_else(|| {
                std::io::Error::other("terminal guard writer unavailable after entering terminal")
            })?,
            helpers::terminal_area()?,
        );
        Box::pin(runtime::run_event_loop_with_static_bundled(
            &mut terminal,
            session_id,
            static_plugins,
        ))
        .await
    };

    match result {
        Ok(()) => {
            let _writer = guard.leave()?;
            Ok(())
        }
        Err(error) => Err(error),
    }
}

/// Run the full-screen review home/picker.
///
/// # Errors
///
/// Returns I/O, client, or plugin service errors.
#[allow(clippy::future_not_send)]
pub async fn run_code_review_home(repo_path: std::path::PathBuf) -> Result<(), TuiError> {
    let stdout = io::stdout();
    let mut guard = CrosstermTerminalGuard::enter(stdout)?;
    let result = {
        let mut terminal = Terminal::new(
            guard.writer_mut().ok_or_else(|| {
                std::io::Error::other("terminal guard writer unavailable after entering terminal")
            })?,
            helpers::terminal_area()?,
        );
        code_review_launcher::run_home(&mut terminal, repo_path).await
    };

    match result {
        Ok(code_review_launcher::ReviewHomeOutcome::OpenWorkspace {
            workspace,
            build_mode,
        }) => {
            let _writer = guard.leave()?;
            Box::pin(run_code_review_workspace(workspace, build_mode)).await
        }
        Ok(code_review_launcher::ReviewHomeOutcome::Exit) => {
            let _writer = guard.leave()?;
            Ok(())
        }
        Err(error) => Err(error),
    }
}

/// Run the full-screen local code review interface for an existing workspace.
///
/// # Errors
///
/// Returns I/O, client, or plugin service errors.
#[allow(clippy::future_not_send)]
pub async fn run_code_review_workspace(
    workspace: bcode_code_review_models::ReviewWorkspace,
    build_mode: bool,
) -> Result<(), TuiError> {
    let stdout = io::stdout();
    let mut guard = CrosstermTerminalGuard::enter(stdout)?;
    let result = {
        let mut terminal = Terminal::new(
            guard.writer_mut().ok_or_else(|| {
                std::io::Error::other("terminal guard writer unavailable after entering terminal")
            })?,
            helpers::terminal_area()?,
        );
        code_review_launcher::run_workspace(&mut terminal, workspace, build_mode).await
    };

    match result {
        Ok(session_id) => {
            let _writer = guard.leave()?;
            if let Some(session_id) = session_id {
                Box::pin(run(Some(session_id))).await
            } else {
                Ok(())
            }
        }
        Err(error) => Err(error),
    }
}

/// Run the full-screen local code review interface.
///
/// # Errors
///
/// Returns I/O, client, or plugin service errors.
#[allow(clippy::future_not_send)]
pub async fn run_code_review(
    repo_path: std::path::PathBuf,
    target: bcode_code_review_models::ReviewTarget,
) -> Result<(), TuiError> {
    let stdout = io::stdout();
    let mut guard = CrosstermTerminalGuard::enter(stdout)?;
    let result = {
        let mut terminal = Terminal::new(
            guard.writer_mut().ok_or_else(|| {
                std::io::Error::other("terminal guard writer unavailable after entering terminal")
            })?,
            helpers::terminal_area()?,
        );
        code_review_launcher::run(&mut terminal, repo_path, target).await
    };

    match result {
        Ok(session_id) => {
            let _writer = guard.leave()?;
            if let Some(session_id) = session_id {
                Box::pin(run(Some(session_id))).await
            } else {
                Ok(())
            }
        }
        Err(error) => Err(error),
    }
}

/// Run the eval run picker TUI.
///
/// # Errors
///
/// Returns I/O or plugin service errors.
#[allow(clippy::future_not_send)]
pub async fn run_eval_viewer_picker(repo_path: std::path::PathBuf) -> Result<(), TuiError> {
    let stdout = io::stdout();
    let mut guard = CrosstermTerminalGuard::enter(stdout)?;
    let result = {
        let mut terminal = Terminal::new(
            guard.writer_mut().ok_or_else(|| {
                std::io::Error::other("terminal guard writer unavailable after entering terminal")
            })?,
            helpers::terminal_area()?,
        );
        eval_launcher::run_picker(&mut terminal, repo_path).await
    };
    let _writer = guard.leave()?;
    result
}

/// Run the persisted metrics dashboard TUI.
///
/// # Errors
///
/// Returns I/O or plugin service errors.
#[allow(clippy::future_not_send)]
pub async fn run_metrics_dashboard(
    repo_path: std::path::PathBuf,
    metrics_path: Option<std::path::PathBuf>,
) -> Result<(), TuiError> {
    let stdout = io::stdout();
    let mut guard = CrosstermTerminalGuard::enter(stdout)?;
    let result = {
        let mut terminal = Terminal::new(
            guard.writer_mut().ok_or_else(|| {
                std::io::Error::other("terminal guard writer unavailable after entering terminal")
            })?,
            helpers::terminal_area()?,
        );
        metrics_launcher::run_dashboard(&mut terminal, repo_path, metrics_path).await
    };
    let _writer = guard.leave()?;
    result
}

/// Run the eval run viewer TUI for an optional run path.
///
/// When `run` is `None`, the picker is opened instead.
///
/// # Errors
///
/// Returns I/O or plugin service errors.
#[allow(clippy::future_not_send)]
pub async fn run_eval_viewer(
    repo_path: std::path::PathBuf,
    run: Option<std::path::PathBuf>,
) -> Result<(), TuiError> {
    let stdout = io::stdout();
    let mut guard = CrosstermTerminalGuard::enter(stdout)?;
    let result = {
        let mut terminal = Terminal::new(
            guard.writer_mut().ok_or_else(|| {
                std::io::Error::other("terminal guard writer unavailable after entering terminal")
            })?,
            helpers::terminal_area()?,
        );
        eval_launcher::run_viewer(&mut terminal, repo_path, run).await
    };
    let _writer = guard.leave()?;
    result
}
