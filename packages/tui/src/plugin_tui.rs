//! Native plugin TUI surface opening helpers.

use bcode_plugin::{PluginHost, PluginLoadError, PluginRuntimeHost, StaticBundledPlugin};
use bcode_plugin_sdk::tui::{
    BoxedPluginTuiSurface, PluginTuiRegistry, PluginTuiSurfaceOpenRequest,
};

/// Return the platform-owned TUI registry for an enabled bundled plugin.
#[must_use]
pub fn tui_registry(plugin_id: &str) -> Option<PluginTuiRegistry> {
    let registry = bcode_bundled_plugins::tui_registry(plugin_id);
    #[cfg(test)]
    let registry = registry.or_else(|| match plugin_id {
        "bcode.filesystem" => Some(bcode_filesystem_plugin::filesystem_tui_registry()),
        "bcode.question" => Some(bcode_question_plugin::question_tui_registry()),
        "bcode.shell" => Some(bcode_shell_plugin::shell_tui_registry()),
        _ => None,
    });
    registry
}

/// Load the default local plugin host for TUI client-side services.
///
/// # Errors
///
/// Returns plugin loading errors from discovery, loading, or activation.
pub fn load_default_host_with_static_bundled(
    static_plugins: &[StaticBundledPlugin],
) -> Result<PluginHost, PluginLoadError> {
    let selection = bcode_plugin::PluginSelection::all_enabled();
    if let Ok(host) = PluginHost::load_defaults_with_static_bundled(&selection, static_plugins) {
        Ok(host)
    } else {
        let selected = bcode_plugin::filter_selected_static_plugins(static_plugins, &selection)?;
        let visual_plugins = selected
            .into_iter()
            .filter(|(manifest, _)| {
                tui_registry(&manifest.id).is_some_and(|registry| {
                    manifest.visual_adapters.iter().any(|adapter| {
                        (adapter.surfaces.is_empty()
                            || adapter.surfaces.iter().any(|surface| surface == "tui"))
                            && registry.supports_visual(&adapter.schema)
                    })
                })
            })
            .collect::<Vec<_>>();
        Ok(PluginHost::load_static_plugins_best_effort(&visual_plugins))
    }
}

/// Load the default local plugin runtime for TUI client-side services.
///
/// # Errors
///
/// Returns plugin loading errors from discovery, loading, or activation.
pub fn load_default_runtime_with_static_bundled(
    static_plugins: &[StaticBundledPlugin],
) -> Result<PluginRuntimeHost, PluginLoadError> {
    PluginRuntimeHost::load_defaults_with_static_bundled(
        &bcode_plugin::PluginSelection::all_enabled(),
        static_plugins,
    )
}

/// Open a native TUI surface from a platform-owned registry.
///
/// # Errors
///
/// Returns an error when the plugin is not loaded, has no native TUI registry, or the surface
/// factory fails to open the surface.
pub async fn open_plugin_tui_surface(
    runtime: &PluginRuntimeHost,
    plugin_id: &str,
    surface_kind: &str,
    request: PluginTuiSurfaceOpenRequest,
) -> Result<BoxedPluginTuiSurface, PluginLoadError> {
    if !runtime
        .plugin_ids()
        .iter()
        .any(|loaded| loaded == plugin_id)
    {
        return Err(PluginLoadError::PluginNotLoaded(plugin_id.to_string()));
    }
    let registry = tui_registry(plugin_id)
        .ok_or_else(|| PluginLoadError::PluginNotLoaded(plugin_id.to_string()))?;
    if runtime
        .registry()
        .tui_surface(plugin_id, surface_kind)
        .is_none()
    {
        return Err(PluginLoadError::TuiSurfaceOpen {
            plugin_id: plugin_id.to_string(),
            message: format!("plugin does not declare TUI surface kind '{surface_kind}'"),
        });
    }
    registry
        .open(surface_kind, request)
        .await
        .map_err(|error| PluginLoadError::TuiSurfaceOpen {
            plugin_id: plugin_id.to_string(),
            message: error.to_string(),
        })
}
