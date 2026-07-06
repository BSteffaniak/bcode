//! Eval plugin TUI surface registry.

use crate::eval_viewer::{EvalRunPickerSurface, EvalRunViewerSurface};
use bcode_plugin_sdk::tui::{
    BoxedPluginTuiSurface, PluginTuiRegistry, PluginTuiSurfaceFactory, PluginTuiSurfaceFuture,
    PluginTuiSurfaceOpenRequest,
};
use std::path::PathBuf;

/// Eval run picker surface kind.
pub const EVAL_RUN_PICKER_SURFACE_KIND: &str = "eval-run-picker";
/// Eval run viewer surface kind.
pub const EVAL_RUN_VIEWER_SURFACE_KIND: &str = "eval-run-viewer";

/// Register eval TUI surfaces.
#[must_use]
pub fn tui_registry() -> PluginTuiRegistry {
    let mut registry = PluginTuiRegistry::default();
    registry.register_factory(Box::new(EvalRunPickerSurfaceFactory));
    registry.register_factory(Box::new(EvalRunViewerSurfaceFactory));
    registry
}

#[derive(Debug, Default)]
struct EvalRunPickerSurfaceFactory;

impl PluginTuiSurfaceFactory for EvalRunPickerSurfaceFactory {
    fn surface_kind(&self) -> &'static str {
        EVAL_RUN_PICKER_SURFACE_KIND
    }

    fn open(&self, request: PluginTuiSurfaceOpenRequest) -> PluginTuiSurfaceFuture {
        Box::pin(async move {
            let root = request
                .options
                .get("runs_root")
                .and_then(serde_json::Value::as_str)
                .map_or_else(|| PathBuf::from("target/bcode-evals/runs"), PathBuf::from);
            Ok(Box::new(EvalRunPickerSurface::load(root)) as BoxedPluginTuiSurface)
        })
    }
}

#[derive(Debug, Default)]
struct EvalRunViewerSurfaceFactory;

impl PluginTuiSurfaceFactory for EvalRunViewerSurfaceFactory {
    fn surface_kind(&self) -> &'static str {
        EVAL_RUN_VIEWER_SURFACE_KIND
    }

    fn open(&self, request: PluginTuiSurfaceOpenRequest) -> PluginTuiSurfaceFuture {
        Box::pin(async move {
            let run_path = request
                .options
                .get("run_path")
                .and_then(serde_json::Value::as_str)
                .map(PathBuf::from)
                .ok_or("eval run viewer requires run_path option")?;
            let surface = EvalRunViewerSurface::load(run_path)?;
            Ok(Box::new(surface) as BoxedPluginTuiSurface)
        })
    }
}
