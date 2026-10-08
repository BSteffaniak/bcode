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
    for action in dashboard_keymap_actions() {
        registry.register_keymap_action(action);
    }
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
            let query = crate::metrics_dashboard::dashboard_query_from_options(&request.options);
            Ok(Box::new(MetricsDashboardSurface::load(path, query)) as BoxedPluginTuiSurface)
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

/// Contributor-owned dashboard actions and default shortcuts.
pub(crate) fn dashboard_keymap_actions() -> Vec<bcode_plugin_sdk::tui::keymap::KeymapAction> {
    [
        ("previous_row", "Previous row", &["up", "k"] as &[&str]),
        ("next_row", "Next row", &["down", "j"] as &[&str]),
        ("first_row", "First row", &["home"] as &[&str]),
        ("last_row", "Last row", &["end"] as &[&str]),
        ("close", "Close dashboard", &["q", "escape"] as &[&str]),
        ("refresh", "Refresh report", &["r"] as &[&str]),
        ("sort", "Cycle sort", &["s"] as &[&str]),
        ("sort_direction", "Reverse sort", &["d"] as &[&str]),
        ("group", "Cycle grouping", &["g"] as &[&str]),
        ("filter", "Filter selected row", &["f"] as &[&str]),
        ("clear_filters", "Clear filters", &["c"] as &[&str]),
        ("tab.1", "Select tab 1", &["1"] as &[&str]),
        ("tab.2", "Select tab 2", &["2"] as &[&str]),
        ("tab.3", "Select tab 3", &["3"] as &[&str]),
        ("tab.4", "Select tab 4", &["4"] as &[&str]),
        ("tab.5", "Select tab 5", &["5"] as &[&str]),
        ("tab.6", "Select tab 6", &["6"] as &[&str]),
        ("tab.7", "Select tab 7", &["7"] as &[&str]),
        ("tab.8", "Select tab 8", &["8"] as &[&str]),
    ]
    .into_iter()
    .map(
        |(id, description, keys)| bcode_plugin_sdk::tui::keymap::KeymapAction {
            id: format!("bcode.metrics.{id}"),
            scope: "metrics-dashboard".into(),
            description: description.into(),
            defaults: keys.iter().map(|key| (*key).into()).collect(),
        },
    )
    .collect()
}
