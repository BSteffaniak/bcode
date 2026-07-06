//! Eval TUI plugin launcher.

use std::io::Write;
use std::path::PathBuf;

use bmux_tui::terminal::Terminal;

use crate::TuiError;

const EVAL_PLUGIN_ID: &str = "bcode.eval";
const EVAL_RUN_PICKER_SURFACE_KIND: &str = "eval-run-picker";
const EVAL_RUN_VIEWER_SURFACE_KIND: &str = "eval-run-viewer";
const DEFAULT_RUNS_ROOT: &str = "target/bcode-evals/runs";

/// Run the eval run picker surface.
///
/// # Errors
///
/// Returns an error when the eval plugin cannot be loaded/opened or terminal I/O fails.
#[allow(clippy::future_not_send)]
pub async fn run_picker<W: Write>(
    terminal: &mut Terminal<&mut W>,
    repo_path: PathBuf,
) -> Result<(), TuiError> {
    let runtime = load_eval_tui_runtime()?;
    let mut surface = crate::plugin_tui::open_plugin_tui_surface(
        &runtime,
        EVAL_PLUGIN_ID,
        EVAL_RUN_PICKER_SURFACE_KIND,
        bcode_plugin_sdk::tui::PluginTuiSurfaceOpenRequest {
            instance_id: "eval-run-picker".to_string(),
            repo_path: Some(repo_path),
            target: None,
            options: serde_json::json!({ "runs_root": DEFAULT_RUNS_ROOT }),
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

/// Run the eval run viewer surface for an optional run path.
///
/// When `run` is `None`, the picker is opened instead.
///
/// # Errors
///
/// Returns an error when the eval plugin cannot be loaded/opened or terminal I/O fails.
#[allow(clippy::future_not_send)]
pub async fn run_viewer<W: Write>(
    terminal: &mut Terminal<&mut W>,
    repo_path: PathBuf,
    run: Option<PathBuf>,
) -> Result<(), TuiError> {
    let runtime = load_eval_tui_runtime()?;
    let (surface_kind, instance_id, options) = run.map_or_else(
        || {
            (
                EVAL_RUN_PICKER_SURFACE_KIND,
                "eval-run-picker".to_string(),
                serde_json::json!({ "runs_root": DEFAULT_RUNS_ROOT }),
            )
        },
        |run_path| {
            (
                EVAL_RUN_VIEWER_SURFACE_KIND,
                format!("eval-run-viewer:{}", run_path.display()),
                serde_json::json!({ "run_path": run_path, "runs_root": DEFAULT_RUNS_ROOT }),
            )
        },
    );
    let mut surface = crate::plugin_tui::open_plugin_tui_surface(
        &runtime,
        EVAL_PLUGIN_ID,
        surface_kind,
        bcode_plugin_sdk::tui::PluginTuiSurfaceOpenRequest {
            instance_id,
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
    let _outcome =
        crate::plugin_surface_host::run_plugin_surface(terminal, surface.as_mut()).await?;
    Ok(())
}

fn load_eval_tui_runtime() -> Result<bcode_plugin::PluginRuntimeHost, TuiError> {
    bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
        &bcode_plugin::PluginSelection::all_enabled(),
        &crate::static_bundled_plugins(),
    )
    .map_err(|error| TuiError::PluginService {
        code: "plugin_runtime_load_failed".to_string(),
        message: error.to_string(),
    })
}
