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
    Box::pin(run_surface(
        terminal,
        repo_path,
        EVAL_RUN_PICKER_SURFACE_KIND,
        "eval-run-picker".to_string(),
        serde_json::json!({ "runs_root": DEFAULT_RUNS_ROOT }),
    ))
    .await
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
    Box::pin(run_surface(
        terminal,
        repo_path,
        surface_kind,
        instance_id,
        options,
    ))
    .await
}

#[allow(clippy::future_not_send)]
async fn run_surface<W: Write>(
    terminal: &mut Terminal<&mut W>,
    repo_path: PathBuf,
    surface_kind: &str,
    instance_id: String,
    options: serde_json::Value,
) -> Result<(), TuiError> {
    let runtime = load_eval_tui_runtime()?;
    let surface = crate::plugin_tui::open_plugin_tui_surface(
        &runtime,
        EVAL_PLUGIN_ID,
        surface_kind,
        bcode_plugin_sdk::tui::PluginTuiSurfaceOpenRequest {
            instance_id,
            repo_path: Some(repo_path),
            session_id: None,
            target: None,
            options,
        },
    )
    .await
    .map_err(|error| TuiError::PluginService {
        code: "tui_surface_open_failed".to_string(),
        message: error.to_string(),
    })?;
    let _outcome = Box::pin(crate::runtime::run_standalone_plugin_surface(
        terminal,
        EVAL_PLUGIN_ID,
        surface,
    ))
    .await?;
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
