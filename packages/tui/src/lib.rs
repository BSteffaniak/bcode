//! Terminal user interface for Bcode.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

pub(crate) mod activity;
pub(crate) mod app;
pub(crate) mod chat_loop;
pub(crate) mod clipboard_image;
pub mod code_review_launcher;
pub(crate) mod command_palette;
pub(crate) mod command_palette_render;
pub(crate) mod composer_flow;
pub(crate) mod cursor_blink;
pub(crate) mod daemon_issue;
pub(crate) mod diff_extract;
pub(crate) mod diff_panel;
pub(crate) mod effects;
pub(crate) mod exit_state;
pub(crate) mod filtered_list;
pub(crate) mod helpers;
pub(crate) mod history_flow;
pub(crate) mod input;
pub(crate) mod input_history;
pub(crate) mod invalidation;
pub(crate) mod keymap;
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
pub(crate) mod runtime_work_view;
pub(crate) mod session_flow;
pub(crate) mod session_fork_dialog;
pub(crate) mod session_fork_dialog_render;
pub(crate) mod session_fork_flow;
pub(crate) mod session_picker;
pub(crate) mod session_picker_render;
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
pub(crate) mod tool_invocation_view;
pub(crate) mod tool_present;
pub(crate) mod transcript;
pub(crate) mod transcript_document;
pub(crate) mod transcript_layout;
pub(crate) mod transcript_projection;
pub(crate) mod transcript_resident_window;
pub(crate) mod transcript_viewport;
pub(crate) mod worktree_create_dialog;
pub(crate) mod worktree_create_dialog_render;
pub(crate) mod worktree_flow;
pub(crate) mod worktree_picker;
pub(crate) mod worktree_picker_render;

use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use bcode_session_models::SessionId;
use bmux_tui::crossterm::CrosstermTerminalGuard;
use bmux_tui::geometry::Rect;
use bmux_tui::terminal::Terminal;
use crossterm::event::{
    self as crossterm_event, Event as CrosstermEvent, KeyCode as CrosstermKeyCode,
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
            CrosstermEvent::Key(key) => match key.code {
                CrosstermKeyCode::Esc | CrosstermKeyCode::Char('q') => return Ok(()),
                CrosstermKeyCode::Right | CrosstermKeyCode::Down | CrosstermKeyCode::Char('j') => {
                    shell.focus_next();
                }
                CrosstermKeyCode::Left | CrosstermKeyCode::Up | CrosstermKeyCode::Char('k') => {
                    shell.focus_previous();
                }
                CrosstermKeyCode::Enter => {
                    shell.handle_action(
                        onboarding::OnboardingInputAction::Select,
                        store,
                        current_time_ms(),
                    )?;
                }
                CrosstermKeyCode::Char('c') => {
                    shell.handle_action(
                        onboarding::OnboardingInputAction::Complete,
                        store,
                        current_time_ms(),
                    )?;
                }
                CrosstermKeyCode::Char('s') => {
                    shell.handle_action(
                        onboarding::OnboardingInputAction::Skip,
                        store,
                        current_time_ms(),
                    )?;
                }
                CrosstermKeyCode::Char('l') => {
                    shell.handle_action(
                        onboarding::OnboardingInputAction::Launch,
                        store,
                        current_time_ms(),
                    )?;
                }
                _ => {}
            },
            CrosstermEvent::FocusGained
            | CrosstermEvent::FocusLost
            | CrosstermEvent::Mouse(_)
            | CrosstermEvent::Paste(_) => {}
        }
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

/// Return statically bundled plugin registrations compiled into `bcode_tui`.
#[must_use]
pub fn static_bundled_plugins() -> Vec<bcode_plugin::StaticBundledPlugin> {
    vec![
        #[cfg(feature = "static-bundled-code-review-plugin")]
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/code-review-plugin/bcode-plugin.toml"),
            bcode_code_review_plugin::static_plugin(),
        ),
        #[cfg(feature = "static-bundled-ralph-plugin")]
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/ralph-plugin/bcode-plugin.toml"),
            bcode_ralph_plugin::static_plugin(),
        ),
    ]
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
        runtime::run_event_loop_with_startup(
            &mut terminal,
            None,
            startup_action::StartupTuiAction::OpenRalphHome,
        )
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
pub async fn run(session_id: Option<SessionId>) -> Result<(), TuiError> {
    let stdout = io::stdout();
    let mut guard = CrosstermTerminalGuard::enter(stdout)?;
    let result = {
        let mut terminal = Terminal::new(
            guard.writer_mut().ok_or_else(|| {
                std::io::Error::other("terminal guard writer unavailable after entering terminal")
            })?,
            helpers::terminal_area()?,
        );
        runtime::run_event_loop(&mut terminal, session_id).await
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
            run_code_review_workspace(workspace, build_mode).await
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
                run(Some(session_id)).await
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
                run(Some(session_id)).await
            } else {
                Ok(())
            }
        }
        Err(error) => Err(error),
    }
}
