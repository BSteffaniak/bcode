//! Native plugin TUI presentation and surface helpers.

use bcode_plugin::{PluginHost, PluginLoadError, PluginRuntimeHost, StaticBundledPlugin};
use bcode_plugin_sdk::tui::{
    BoxedPluginTuiSurface, PluginTuiArtifactChunk, PluginTuiRegistry, PluginTuiSurfaceOpenRequest,
};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// Process-local presentation state for one TUI instance.
///
/// Registries are retained because visual adapters may accumulate incremental artifact state that
/// later render passes consume.
#[derive(Debug)]
pub struct PluginTuiPresentation {
    host: Arc<PluginHost>,
    registries: Mutex<BTreeMap<String, Arc<PluginTuiRegistry>>>,
}

impl PluginTuiPresentation {
    /// Create presentation state around a loaded plugin host.
    #[must_use]
    pub fn new(host: PluginHost) -> Self {
        Self::from_shared(Arc::new(host))
    }

    /// Create presentation state around a shared loaded plugin host.
    #[must_use]
    pub const fn from_shared(host: Arc<PluginHost>) -> Self {
        Self {
            host,
            registries: Mutex::new(BTreeMap::new()),
        }
    }

    /// Return the routing host.
    #[must_use]
    pub fn host(&self) -> &PluginHost {
        &self.host
    }

    /// Return one retained native TUI registry.
    #[must_use]
    pub fn registry(&self, plugin_id: &str) -> Option<Arc<PluginTuiRegistry>> {
        let mut registries = self.registries.lock().ok()?;
        if let Some(registry) = registries.get(plugin_id).cloned() {
            return Some(registry);
        }
        let registry = Arc::new(tui_registry(plugin_id)?);
        registries.insert(plugin_id.to_owned(), Arc::clone(&registry));
        drop(registries);
        Some(registry)
    }

    /// Deliver opaque artifact bytes to the retained adapter selected by generic routing metadata.
    ///
    /// # Errors
    ///
    /// Returns an error when the owning adapter rejects the chunk.
    pub fn deliver_artifact_chunk(&self, chunk: &PluginTuiArtifactChunk) -> Result<bool, String> {
        let producer = Some(chunk.producer_plugin_id.as_str());
        let Some(route) =
            self.host
                .visual_adapter(&chunk.schema, chunk.schema_version, "tui", producer)
        else {
            return Ok(false);
        };
        let Some(registry) = self.registry(&route.plugin_id) else {
            return Ok(false);
        };
        registry.visual_artifact_chunk(chunk)
    }
}

/// Return a newly constructed platform-owned TUI registry for an enabled bundled plugin.
///
/// Long-lived visual rendering must acquire registries through [`PluginTuiPresentation`]. Fresh
/// registries remain appropriate for opening independent interactive surface instances.
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

/// Load persistent presentation state for TUI client-side visual adapters.
///
/// # Errors
///
/// Returns plugin loading errors from discovery, loading, or activation.
pub fn load_default_presentation_with_static_bundled(
    static_plugins: &[StaticBundledPlugin],
) -> Result<PluginTuiPresentation, PluginLoadError> {
    load_default_host_with_static_bundled(static_plugins).map(PluginTuiPresentation::new)
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

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tui::prelude::Line;

    #[derive(Default)]
    struct StatefulTestAdapter {
        bytes: Mutex<Vec<u8>>,
    }

    impl bcode_plugin_sdk::tui::PluginTuiVisualAdapter for StatefulTestAdapter {
        fn supports(&self, kind: &str) -> bool {
            kind == "test.artifact"
        }

        fn artifact_chunk(&self, chunk: &PluginTuiArtifactChunk) -> Result<(), String> {
            self.bytes
                .lock()
                .map_err(|_| "test adapter state poisoned".to_owned())?
                .extend_from_slice(&chunk.bytes);
            Ok(())
        }

        fn rows(
            &self,
            _kind: &str,
            _payload: &serde_json::Value,
            _context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
        ) -> Vec<Line> {
            let text = self
                .bytes
                .lock()
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_default();
            vec![Line::from(text)]
        }
    }

    fn test_presentation() -> PluginTuiPresentation {
        let bundled = [StaticBundledPlugin::new(
            include_str!("../../../plugins/shell-plugin/bcode-plugin.toml"),
            bcode_shell_plugin::static_plugin(),
        )];
        let selected = bcode_plugin::filter_selected_static_plugins(
            &bundled,
            &bcode_plugin::PluginSelection::all_enabled(),
        )
        .expect("select test plugin");
        let host = PluginHost::load_static_plugins(&selected).expect("load test plugin");
        PluginTuiPresentation::new(host)
    }

    #[test]
    fn presentation_retains_one_registry_for_delivery_and_rendering() {
        let presentation = test_presentation();
        let mut registry = PluginTuiRegistry::default();
        registry.register_visual_adapter(Box::new(StatefulTestAdapter::default()));
        presentation
            .registries
            .lock()
            .expect("presentation registries")
            .insert("test.plugin".to_owned(), Arc::new(registry));

        let first = presentation.registry("test.plugin").expect("registry");
        first
            .visual_artifact_chunk(&PluginTuiArtifactChunk {
                tool_call_id: "call".to_owned(),
                artifact_id: "artifact".to_owned(),
                reference_key: "reference".to_owned(),
                producer_plugin_id: "test.plugin".to_owned(),
                schema: "test.artifact".to_owned(),
                schema_version: 1,
                content_type: None,
                offset: 0,
                total_bytes: 3,
                revision: 1,
                finalized: false,
                bytes: b"abc".to_vec(),
            })
            .expect("deliver artifact chunk");

        let second = presentation.registry("test.plugin").expect("registry");
        assert!(Arc::ptr_eq(&first, &second));
        let rows = second
            .visual_rows(
                "test.artifact",
                &serde_json::Value::Null,
                &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
                    80,
                    bcode_plugin_sdk::tui::PluginTuiDiffLayout::Unified,
                    None,
                ),
            )
            .expect("stateful adapter rows");
        assert_eq!(rows[0].spans[0].content, "abc");
    }
}
