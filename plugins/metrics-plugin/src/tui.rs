//! Metrics plugin TUI surface registry.

use crate::metrics_dashboard::MetricsDashboardSurface;
use bcode_plugin_sdk::tui::{
    BoxedPluginTuiSurface, PluginTuiRegistry, PluginTuiSurfaceFactory, PluginTuiSurfaceFuture,
    PluginTuiSurfaceOpenRequest,
};
use std::path::PathBuf;

/// Metrics dashboard surface kind.
pub const METRICS_DASHBOARD_SURFACE_KIND: &str = "metrics-dashboard";

/// Register metrics TUI surfaces.
#[must_use]
pub fn tui_registry() -> PluginTuiRegistry {
    let mut registry = PluginTuiRegistry::default();
    registry.register_factory(Box::new(MetricsDashboardSurfaceFactory));
    registry
}

#[derive(Debug, Default)]
struct MetricsDashboardSurfaceFactory;

impl PluginTuiSurfaceFactory for MetricsDashboardSurfaceFactory {
    fn surface_kind(&self) -> &'static str {
        METRICS_DASHBOARD_SURFACE_KIND
    }

    fn open(&self, request: PluginTuiSurfaceOpenRequest) -> PluginTuiSurfaceFuture {
        Box::pin(async move {
            let path = request
                .options
                .get("metrics_path")
                .and_then(serde_json::Value::as_str)
                .map(PathBuf::from)
                .unwrap_or_else(default_metrics_path);
            let path = resolve_repo_relative(path, request.repo_path.as_deref());
            Ok(Box::new(MetricsDashboardSurface::load(path)) as BoxedPluginTuiSurface)
        })
    }
}

fn default_metrics_path() -> PathBuf {
    std::env::var_os("HOME").map_or_else(
        || PathBuf::from(".local/state/bcode/metrics/events.jsonl"),
        |home| PathBuf::from(home).join(".local/state/bcode/metrics/events.jsonl"),
    )
}

fn resolve_repo_relative(path: PathBuf, repo_path: Option<&std::path::Path>) -> PathBuf {
    if path.is_absolute() || path.starts_with(".local") {
        return path;
    }
    match repo_path {
        Some(repo_path) => repo_path.join(path),
        None => path,
    }
}
