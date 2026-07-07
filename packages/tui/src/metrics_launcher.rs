//! Metrics TUI plugin launcher.

use std::io::Write;
use std::path::PathBuf;

use bmux_tui::terminal::Terminal;

use crate::TuiError;

const METRICS_PLUGIN_ID: &str = "bcode.metrics";
const METRICS_DASHBOARD_SURFACE_KIND: &str = "metrics-dashboard";

/// Run the persisted metrics dashboard surface.
///
/// # Errors
///
/// Returns an error when the metrics plugin cannot be loaded/opened or terminal I/O fails.
#[allow(clippy::future_not_send)]
pub async fn run_dashboard<W: Write>(
    terminal: &mut Terminal<&mut W>,
    repo_path: PathBuf,
    metrics_path: Option<PathBuf>,
) -> Result<(), TuiError> {
    let runtime = load_metrics_tui_runtime()?;
    let metrics_path = metrics_path.unwrap_or_else(default_metrics_path);
    let mut surface = crate::plugin_tui::open_plugin_tui_surface(
        &runtime,
        METRICS_PLUGIN_ID,
        METRICS_DASHBOARD_SURFACE_KIND,
        bcode_plugin_sdk::tui::PluginTuiSurfaceOpenRequest {
            instance_id: "metrics-dashboard".to_string(),
            repo_path: Some(repo_path),
            target: None,
            options: serde_json::json!({ "metrics_path": metrics_path }),
        },
    )
    .await
    .map_err(|error| TuiError::PluginService {
        code: "tui_surface_open_failed".to_string(),
        message: error.to_string(),
    })?;
    let _outcome =
        crate::plugin_surface_host::run_plugin_surface(terminal, surface.as_mut()).await?;
    Ok(())
}

fn default_metrics_path() -> PathBuf {
    bcode_config::default_state_dir()
        .join("metrics")
        .join("events.jsonl")
}

fn load_metrics_tui_runtime() -> Result<bcode_plugin::PluginRuntimeHost, TuiError> {
    bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
        &bcode_plugin::PluginSelection::all_enabled(),
        &crate::static_bundled_plugins(),
    )
    .map_err(|error| TuiError::PluginService {
        code: "plugin_runtime_load_failed".to_string(),
        message: error.to_string(),
    })
}
