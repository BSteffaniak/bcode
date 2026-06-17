//! Ralph native TUI plugin launcher.

use std::io::Write;
use std::path::PathBuf;

use bmux_tui::terminal::Terminal;
use serde::Deserialize;

use crate::TuiError;
use crate::terminal_events::TuiInput;

/// Typed Ralph actions selected in the plugin surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RalphHomeAction {
    /// Open guided setup flow.
    #[serde(alias = "plan")]
    Plan,
    /// Save latest assistant planning output into the setup draft.
    #[serde(alias = "save-draft")]
    SaveDraft,
    /// View saved setup draft contents.
    #[serde(alias = "view-draft")]
    ViewDraft,
    /// Build a focused revision prompt for the saved setup draft.
    #[serde(alias = "revise-draft")]
    ReviseDraft,
    /// Approve the saved setup draft.
    #[serde(alias = "approve-draft")]
    ApproveDraft,
    /// Create loop from approved setup draft.
    #[serde(alias = "create-from-draft")]
    CreateFromDraft,
    /// Open quick setup flow.
    #[serde(alias = "start")]
    Start,
    /// Prepare/run autonomous loop.
    Run,
    /// Approve prepared run.
    Approve,
    /// Stop active run.
    Stop,
    /// Resume interrupted run.
    Resume,
    /// Show status.
    Status,
    /// List runs.
    Runs,
    /// List iterations.
    Iterations,
    /// Open progress document.
    Open,
    /// Build audit prompt.
    Audit,
    /// Build replan prompt.
    Replan,
    /// Open goal workflow.
    Goal,
}

/// Ralph home outcome returned by the Ralph plugin surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RalphHomeOutcome {
    /// Dispatch a typed Ralph action selected in the plugin surface.
    Action(RalphHomeAction),
    /// Exit without running an action.
    Exit,
}

/// Run the Ralph plugin home surface.
///
/// # Errors
///
/// Returns an error when the Ralph plugin cannot be loaded/opened or terminal I/O fails.
#[allow(clippy::future_not_send)]
pub async fn run_home<W: Write>(
    terminal: &mut Terminal<&mut W>,
    repo_path: PathBuf,
) -> Result<RalphHomeOutcome, TuiError> {
    let mut surface = open_ralph_home_surface(repo_path, None).await?;
    let close_outcome =
        crate::plugin_surface_host::run_plugin_surface(terminal, surface.as_mut()).await?;
    Ok(parse_ralph_home_outcome(close_outcome))
}

/// Run the Ralph plugin home surface with the caller-owned input stream.
///
/// # Errors
///
/// Returns an error when the Ralph plugin cannot be loaded/opened or terminal I/O fails.
#[allow(clippy::future_not_send)]
pub async fn run_home_with_input<W: Write>(
    terminal: &mut Terminal<&mut W>,
    input: &mut TuiInput,
    repo_path: PathBuf,
    flash_message: Option<&str>,
) -> Result<RalphHomeOutcome, TuiError> {
    let mut surface = open_ralph_home_surface(repo_path, flash_message).await?;
    let close_outcome = crate::plugin_surface_host::run_plugin_surface_with_input(
        terminal,
        input,
        surface.as_mut(),
    )
    .await?;
    Ok(parse_ralph_home_outcome(close_outcome))
}

async fn open_ralph_home_surface(
    repo_path: PathBuf,
    flash_message: Option<&str>,
) -> Result<bcode_plugin_sdk::tui::BoxedPluginTuiSurface, TuiError> {
    let runtime = load_ralph_tui_runtime()?;
    let options = flash_message.map_or(
        serde_json::Value::Null,
        |message| serde_json::json!({ "flash_message": message }),
    );
    let surface = crate::plugin_tui::open_plugin_tui_surface(
        &runtime,
        "bcode.ralph",
        "ralph-home",
        bcode_plugin_sdk::tui::PluginTuiSurfaceOpenRequest {
            instance_id: "ralph-home".to_string(),
            repo_path: Some(repo_path),
            target: None,
            options,
        },
    )
    .await
    .map_err(|error| TuiError::PluginService {
        code: "tui_surface_open_failed".to_string(),
        message: error.to_string(),
    })?;
    Ok(surface)
}

fn load_ralph_tui_runtime() -> Result<bcode_plugin::PluginRuntimeHost, TuiError> {
    bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
        &bcode_plugin::PluginSelection::all_enabled(),
        &crate::static_bundled_plugins(),
    )
    .map_err(|error| TuiError::PluginService {
        code: "plugin_runtime_load_failed".to_string(),
        message: error.to_string(),
    })
}

fn parse_ralph_home_outcome(outcome: Option<serde_json::Value>) -> RalphHomeOutcome {
    outcome
        .and_then(|value| value.get("ralph_action").cloned())
        .and_then(|value| serde_json::from_value(value).ok())
        .map_or(RalphHomeOutcome::Exit, RalphHomeOutcome::Action)
}
