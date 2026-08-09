//! Terminal user interface for Bcode.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

pub(crate) mod activity;
pub(crate) mod app;
pub(crate) mod artifact_stream;
pub(crate) mod auth_pool_picker;
pub(crate) mod auth_pool_picker_render;
pub(crate) mod chat_loop;
pub(crate) mod clipboard_image;
pub mod code_review_launcher;
pub(crate) mod command_palette;
pub(crate) mod command_palette_render;
pub(crate) mod composer_flow;
pub(crate) mod cursor_blink;
pub(crate) mod daemon_issue;
pub(crate) mod effects;
pub mod eval_launcher;
pub(crate) mod exit_state;
pub(crate) mod filtered_list;
#[cfg(test)]
mod frame_sequence_harness;
pub(crate) mod helpers;
pub(crate) mod history_flow;
pub(crate) mod indexed_transcript_layout;
pub(crate) mod input;
pub(crate) mod input_history;
pub(crate) mod interactive_surface;
pub(crate) mod invalidation;
pub(crate) mod keymap;
pub(crate) mod markdown_activation;
pub mod markdown_image;
pub(crate) mod markdown_interaction;
pub mod markdown_mermaid;
pub(crate) mod markdown_projection_coordinator;
pub mod metrics_launcher;
pub(crate) mod model_flow;
pub(crate) mod model_picker;
pub(crate) mod model_picker_render;
pub(crate) mod mouse_flow;
pub(crate) mod older_history;
pub mod onboarding;
mod onboarding_program;
pub(crate) mod onboarding_render;
pub(crate) mod palette_flow;
pub(crate) mod pending_submission;
pub(crate) mod pending_submissions;
pub(crate) mod permission_dialog;
pub(crate) mod permission_dialog_render;
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
#[cfg(test)]
mod renderer_fixtures;
mod root_program;
pub(crate) mod runtime;
mod runtime_adapter;
pub(crate) mod session_flow;
pub(crate) mod session_fork_dialog;
pub(crate) mod session_fork_dialog_render;
pub(crate) mod session_fork_flow;
pub(crate) mod session_picker;
pub(crate) mod session_picker_render;
pub mod session_search_effect;
pub(crate) mod setup_board;
pub(crate) mod skill_flow;
pub(crate) mod skill_picker;
pub(crate) mod skill_picker_render;
pub(crate) mod slash_commands;
pub(crate) mod slash_palette;
pub(crate) mod slash_palette_render;
pub(crate) mod slash_registry;
pub(crate) mod startup_action;
pub(crate) mod telemetry;
pub(crate) mod temporal;
#[cfg(test)]
pub(crate) mod tests;
pub(crate) mod text_input_flow;
pub mod theme;
mod theme_picker;
pub(crate) mod theme_picker_render;
mod thinking_dialog;
pub(crate) mod thinking_dialog_render;
pub(crate) mod thinking_flow;
pub(crate) mod time_format;
pub(crate) mod timeline_dialog;
pub(crate) mod timeline_dialog_render;
pub(crate) mod tool_render_projection;
#[cfg(test)]
mod tool_render_projection_tests;
pub(crate) mod transcript;
pub(crate) mod transcript_document;
pub(crate) mod transcript_layout;
pub(crate) mod transcript_markdown_cache;
pub(crate) mod transcript_projection;
pub(crate) mod transcript_resident_window;
pub(crate) mod transcript_viewport;
pub(crate) mod wt_create_dialog;
pub(crate) mod wt_create_dialog_render;

static BUILD_INFO: std::sync::OnceLock<bcode_build_info::BuildInfo> = std::sync::OnceLock::new();

/// Initialize immutable build information for all TUI entry points.
///
/// # Panics
///
/// Panics if a different build identity was already installed in this process.
pub fn initialize_build_info(build_info: bcode_build_info::BuildInfo) {
    set_build_info(build_info);
}

fn set_build_info(build_info: bcode_build_info::BuildInfo) {
    if let Some(current) = BUILD_INFO.get() {
        assert_eq!(
            current, &build_info,
            "TUI build information changed at runtime"
        );
    } else {
        BUILD_INFO
            .set(build_info)
            .expect("TUI build information initialized more than once");
    }
}

pub(crate) fn build_info() -> bcode_build_info::BuildInfo {
    BUILD_INFO.get().cloned().unwrap_or_else(|| {
        bcode_build_info::BuildInfo::new(
            env!("CARGO_PKG_VERSION"),
            bcode_build_info::BuildMode::Developer,
            bcode_build_info::GitState::Unavailable,
            "00000000",
        )
        .expect("fallback TUI build information")
    })
}

use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use bcode_session_models::SessionId;
use bmux_tui::crossterm::CrosstermTerminalGuard;
use bmux_tui::terminal::Terminal;

const CURSOR_BLINK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
const OLDER_HISTORY_EVENT_LIMIT: usize = 256;

fn markdown_activation_adapter_linkage() {
    let activate: fn(
        &bcode_markdown_render::MarkdownDestination,
    ) -> Result<
        markdown_activation::MarkdownActivation,
        markdown_activation::MarkdownActivationError,
    > = markdown_activation::activate_markdown_destination;
    let copy: fn(&bcode_markdown_render::MarkdownDestination) -> Result<bool, arboard::Error> =
        markdown_activation::copy_markdown_destination;
    let _ = (activate, copy);
}

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
    /// BMUX image compositor failure.
    #[error("image compositor error: {0}")]
    ImageCompositor(#[from] bmux_image::tui::TuiImageError),
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
    /// Session search result could not become canonical navigation.
    #[error("session search navigation unavailable: {0}")]
    SessionSearchNavigation(String),
    /// Session selection was canceled.
    #[error("session selection canceled")]
    Canceled,
}

/// Run the first-run onboarding setup-map interface.
///
/// # Errors
///
/// Returns I/O, settings, or config errors.
pub async fn run_onboarding() -> Result<(), TuiError> {
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
    let shell = onboarding::OnboardingShell::load(&store, &summary)?;
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
        run_onboarding_runtime(&mut terminal, store, shell, &config.tui).await
    };
    let _writer = guard.leave()?;
    result
}

async fn run_onboarding_runtime<W: io::Write>(
    terminal: &mut Terminal<&mut W>,
    store: bcode_settings::SettingsStore,
    shell: onboarding::OnboardingShell,
    tui_config: &bcode_config::TuiConfig,
) -> Result<(), TuiError> {
    let area = terminal.area();
    let theme = theme::resolve_configured_theme(tui_config, std::path::Path::new("."));
    let program = onboarding_program::OnboardingProgram::new(store, shell, &theme, area)?;
    let presenter = onboarding_program::OnboardingPresenter::new(terminal);
    let (runtime, handle) = bmux_tui_runtime::Runtime::new(
        program,
        presenter,
        bmux_tui_runtime::RuntimeConfig::default(),
    );
    let input = bmux_tui_runtime::TerminalInput::start::<onboarding_program::OnboardingProgram>(
        handle,
        onboarding_program::OnboardingMessage::InputFailed,
    );
    let result = runtime.run().await;
    input.request_shutdown();
    match result {
        Ok(_output) => Ok(()),
        Err(bmux_tui_runtime::RuntimeError::Program { error, .. }) => Err(error),
        Err(bmux_tui_runtime::RuntimeError::Presenter { error, .. }) => Err(error.into()),
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
#[cfg(any(
    feature = "static-bundled-code-review-plugin",
    feature = "static-bundled-filesystem-plugin",
    feature = "static-bundled-plugins",
    feature = "static-bundled-ralph-plugin",
    feature = "static-bundled-workflow-plugin"
))]
#[must_use]
#[cfg(any(
    feature = "static-bundled-code-review-plugin",
    feature = "static-bundled-filesystem-plugin",
    feature = "static-bundled-plugins",
    feature = "static-bundled-ralph-plugin",
    feature = "static-bundled-workflow-plugin"
))]
pub fn static_bundled_plugins() -> Vec<bcode_plugin::StaticBundledPlugin> {
    bcode_bundled_plugins::static_bundled_plugins()
}

/// Return no static plugin registrations when static bundling is disabled.
#[cfg(not(any(
    feature = "static-bundled-code-review-plugin",
    feature = "static-bundled-filesystem-plugin",
    feature = "static-bundled-plugins",
    feature = "static-bundled-ralph-plugin",
    feature = "static-bundled-workflow-plugin"
)))]
#[must_use]
pub const fn static_bundled_plugins() -> Vec<bcode_plugin::StaticBundledPlugin> {
    Vec::new()
}

#[cfg(any(
    feature = "static-bundled-code-review-plugin",
    feature = "static-bundled-filesystem-plugin",
    feature = "static-bundled-plugins",
    feature = "static-bundled-ralph-plugin",
    feature = "static-bundled-workflow-plugin"
))]
fn bundled_interaction_adapters(
    platform_id: &str,
) -> Vec<bcode_plugin_sdk::interaction::PluginInteractionAdapterCapability> {
    bcode_bundled_plugins::interaction_adapters(platform_id)
}

#[cfg(not(any(
    feature = "static-bundled-code-review-plugin",
    feature = "static-bundled-filesystem-plugin",
    feature = "static-bundled-plugins",
    feature = "static-bundled-ralph-plugin",
    feature = "static-bundled-workflow-plugin"
)))]
const fn bundled_interaction_adapters(
    _platform_id: &str,
) -> Vec<bcode_plugin_sdk::interaction::PluginInteractionAdapterCapability> {
    Vec::new()
}

#[cfg(any(
    feature = "static-bundled-code-review-plugin",
    feature = "static-bundled-filesystem-plugin",
    feature = "static-bundled-plugins",
    feature = "static-bundled-ralph-plugin",
    feature = "static-bundled-workflow-plugin"
))]
fn bundled_interaction_adapter(
    producer_id: &str,
    schema: &str,
    schema_version: u32,
    platform_id: &str,
) -> Option<bcode_plugin_sdk::interaction::PluginInteractionAdapterCapability> {
    bcode_bundled_plugins::interaction_adapter(producer_id, schema, schema_version, platform_id)
}

#[cfg(any(
    feature = "static-bundled-code-review-plugin",
    feature = "static-bundled-filesystem-plugin",
    feature = "static-bundled-plugins",
    feature = "static-bundled-ralph-plugin",
    feature = "static-bundled-workflow-plugin"
))]
fn bundled_tui_extensions() -> Vec<bcode_plugin_sdk::tui::StaticPluginTuiExtension> {
    bcode_bundled_plugins::static_tui_extensions()
}

#[cfg(not(any(
    feature = "static-bundled-code-review-plugin",
    feature = "static-bundled-filesystem-plugin",
    feature = "static-bundled-plugins",
    feature = "static-bundled-ralph-plugin",
    feature = "static-bundled-workflow-plugin"
)))]
const fn bundled_tui_extensions() -> Vec<bcode_plugin_sdk::tui::StaticPluginTuiExtension> {
    Vec::new()
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
pub async fn run(
    session_id: Option<SessionId>,
    build_info: bcode_build_info::BuildInfo,
) -> Result<(), TuiError> {
    markdown_activation_adapter_linkage();
    Box::pin(run_with_static_bundled(
        session_id,
        &static_bundled_plugins(),
        build_info,
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
    build_info: bcode_build_info::BuildInfo,
) -> Result<(), TuiError> {
    set_build_info(build_info);
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
        Box::pin(code_review_launcher::run_home(&mut terminal, repo_path)).await
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
        Box::pin(code_review_launcher::run_workspace(
            &mut terminal,
            workspace,
            build_mode,
        ))
        .await
    };

    match result {
        Ok(session_id) => {
            let _writer = guard.leave()?;
            if let Some(session_id) = session_id {
                Box::pin(run(Some(session_id), build_info())).await
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
        Box::pin(code_review_launcher::run(&mut terminal, repo_path, target)).await
    };

    match result {
        Ok(session_id) => {
            let _writer = guard.leave()?;
            if let Some(session_id) = session_id {
                Box::pin(run(Some(session_id), build_info())).await
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
        Box::pin(eval_launcher::run_picker(&mut terminal, repo_path)).await
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
        Box::pin(metrics_launcher::run_dashboard(
            &mut terminal,
            repo_path,
            metrics_path,
        ))
        .await
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
        Box::pin(eval_launcher::run_viewer(&mut terminal, repo_path, run)).await
    };
    let _writer = guard.leave()?;
    result
}
