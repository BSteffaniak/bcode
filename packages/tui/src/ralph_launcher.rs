//! Ralph native TUI plugin launcher.

use std::io::Write;
use std::path::PathBuf;

use bmux_tui::terminal::Terminal;

use crate::TuiError;

/// Ralph home outcome returned by the Ralph plugin surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RalphHomeOutcome {
    /// Run a slash command selected in the plugin surface.
    RunCommand(String),
    /// Exit without running a command.
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
    let runtime = load_ralph_tui_runtime()?;
    let mut surface = crate::plugin_tui::open_plugin_tui_surface(
        &runtime,
        "bcode.ralph",
        "ralph-home",
        bcode_plugin_sdk::tui::PluginTuiSurfaceOpenRequest {
            instance_id: "ralph-home".to_string(),
            repo_path: Some(repo_path),
            target: None,
            options: serde_json::Value::Null,
        },
    )
    .await
    .map_err(|error| TuiError::PluginService {
        code: "tui_surface_open_failed".to_string(),
        message: error.to_string(),
    })?;
    let close_outcome =
        crate::plugin_surface_host::run_plugin_surface(terminal, surface.as_mut()).await?;
    Ok(parse_ralph_home_outcome(close_outcome))
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
        .and_then(|value| value.get("run_command").cloned())
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .map_or(RalphHomeOutcome::Exit, RalphHomeOutcome::RunCommand)
}
