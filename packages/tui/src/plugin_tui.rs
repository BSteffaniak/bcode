//! Native plugin TUI surface opening helpers.

use bcode_plugin::{
    PluginHost, PluginLoadError, PluginManifest, PluginRuntimeHost, StaticBundledPlugin,
};
use bcode_plugin_sdk::StaticPluginVtable;
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
    let selected_static_plugins =
        bcode_plugin::filter_selected_static_plugins(static_plugins, &selection)?;
    let host = PluginHost::load_defaults_with_static_bundled(&selection, static_plugins);
    match host {
        Ok(host)
            if static_plugins.is_empty()
                || host_supports_static_native_visual_routes(&host, &selected_static_plugins) =>
        {
            Ok(host)
        }
        Ok(_) | Err(_) => {
            let adapter_plugins = selected_static_plugins
                .into_iter()
                .filter(|(manifest, vtable)| {
                    manifest_has_static_native_visual_route(manifest, vtable)
                })
                .collect::<Vec<_>>();
            Ok(PluginHost::load_static_plugins_best_effort(
                &adapter_plugins,
            ))
        }
    }
}

fn host_supports_static_native_visual_routes(
    host: &PluginHost,
    static_plugins: &[(PluginManifest, StaticPluginVtable)],
) -> bool {
    static_plugins.iter().all(|(manifest, vtable)| {
        host_supports_static_plugin_native_visual_routes(host, manifest, vtable)
    })
}

fn host_supports_static_plugin_native_visual_routes(
    host: &PluginHost,
    manifest: &PluginManifest,
    vtable: &StaticPluginVtable,
) -> bool {
    let Some(static_registry) = vtable.tui_registry.map(|registry| registry()) else {
        return true;
    };
    let routes = native_visual_route_schemas(manifest, &static_registry);
    routes.into_iter().all(|schema| {
        host.tui_registry(&manifest.id)
            .is_some_and(|registry| registry.supports_visual(schema))
    })
}

fn manifest_has_static_native_visual_route(
    manifest: &PluginManifest,
    vtable: &StaticPluginVtable,
) -> bool {
    vtable
        .tui_registry
        .is_some_and(|registry| !native_visual_route_schemas(manifest, &registry()).is_empty())
}

fn native_visual_route_schemas<'a>(
    manifest: &'a PluginManifest,
    registry: &bcode_plugin_sdk::tui::PluginTuiRegistry,
) -> Vec<&'a str> {
    manifest
        .visual_adapters
        .iter()
        .filter(|adapter| {
            adapter.surfaces.is_empty() || adapter.surfaces.iter().any(|surface| surface == "tui")
        })
        .filter_map(|adapter| {
            registry
                .supports_visual(&adapter.schema)
                .then_some(adapter.schema.as_str())
        })
        .collect()
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
