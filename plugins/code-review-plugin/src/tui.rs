//! Native code review TUI surface contribution.

use bcode_plugin_sdk::tui::{
    PluginTuiError, PluginTuiRegistry, PluginTuiSurfaceFactory, PluginTuiSurfaceFuture,
    PluginTuiSurfaceOpenRequest,
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

    fn open(&self, _request: PluginTuiSurfaceOpenRequest) -> PluginTuiSurfaceFuture {
        Box::pin(async move {
            Err::<_, PluginTuiError>("code review TUI surface implementation is supplied by bcode_tui until UI modules move into this plugin".into())
        })
    }
}
