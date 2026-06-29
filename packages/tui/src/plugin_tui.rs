//! Native plugin TUI surface opening helpers.

use bcode_plugin::{PluginHost, PluginLoadError, PluginRuntimeHost, StaticBundledPlugin};
use bcode_plugin_sdk::tui::{BoxedPluginTuiSurface, PluginTuiSurfaceOpenRequest};

/// Load the default local plugin host for TUI client-side services.
///
/// # Errors
///
/// Returns plugin loading errors from discovery, loading, or activation.
pub fn load_default_host_with_static_bundled(
    static_plugins: &[StaticBundledPlugin],
) -> Result<PluginHost, PluginLoadError> {
    let selection = bcode_plugin::PluginSelection::all_enabled();
    let host = PluginHost::load_defaults_with_static_bundled(&selection, static_plugins);
    match host {
        Ok(host) if host.visual_adapter_count() > 0 || static_plugins.is_empty() => Ok(host),
        Ok(_) | Err(_) => {
            let selected =
                bcode_plugin::filter_selected_static_plugins(static_plugins, &selection)?;
            let adapter_plugins = selected
                .into_iter()
                .filter(|(manifest, _)| !manifest.visual_adapters.is_empty())
                .collect::<Vec<_>>();
            Ok(PluginHost::load_static_plugins_best_effort(
                &adapter_plugins,
            ))
        }
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

/// Open a native TUI surface from a loaded plugin registry.
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
    let registry = runtime
        .tui_registry(plugin_id)
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
