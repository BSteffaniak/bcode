//! Native code review TUI surface contribution.

use crate::code_review_tui::CodeReviewSurface;
use bcode_plugin_sdk::tui::{
    PluginTuiRegistry, PluginTuiSurfaceFactory, PluginTuiSurfaceFuture, PluginTuiSurfaceOpenRequest,
};

/// Code review native TUI surface kind.
pub const CODE_REVIEW_SURFACE_KIND: &str = "code-review";

/// Register native TUI surfaces contributed by the code review plugin.
#[must_use]
pub fn tui_registry() -> PluginTuiRegistry {
    let mut registry = PluginTuiRegistry::default();
    registry.register_factory(Box::new(CodeReviewSurfaceFactory));
    registry
}

#[derive(Debug, Default)]
struct CodeReviewSurfaceFactory;

impl PluginTuiSurfaceFactory for CodeReviewSurfaceFactory {
    fn surface_kind(&self) -> &'static str {
        CODE_REVIEW_SURFACE_KIND
    }

    fn open(&self, request: PluginTuiSurfaceOpenRequest) -> PluginTuiSurfaceFuture {
        Box::pin(async move {
            let repo_path = request
                .repo_path
                .ok_or("code review surface requires repo_path")?;
            let target = serde_json::from_value(
                request
                    .options
                    .get("target")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
            )
            .unwrap_or(bcode_code_review_models::ReviewTarget::Repository);
            let build_mode = request
                .options
                .get("build_mode")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let workspace = request
                .options
                .get("workspace")
                .cloned()
                .and_then(|value| serde_json::from_value(value).ok());
            let surface = CodeReviewSurface::load(repo_path, target, workspace, build_mode).await?;
            Ok(Box::new(surface) as bcode_plugin_sdk::tui::BoxedPluginTuiSurface)
        })
    }
}
