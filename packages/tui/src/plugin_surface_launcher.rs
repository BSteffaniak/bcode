//! Generic standalone startup for registered plugin TUI surfaces.
//!
//! Any plugin that declares a TUI surface kind in its manifest and registers a factory for it
//! can be opened as a full-screen startup surface: the host loads the plugin runtime once,
//! finds the plugin that owns the surface kind, opens the surface with the caller's options,
//! and runs it until it closes. Nothing here knows what a surface does.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;

use bmux_tui::terminal::Terminal;

use crate::TuiError;

/// An opened plugin surface and the runtime that must outlive its interactive execution.
pub struct PreparedPluginSurface {
    runtime: bcode_plugin::PluginRuntimeHost,
    plugin_id: String,
    surface: bcode_plugin_sdk::tui::BoxedPluginTuiSurface,
    presentation: crate::runtime::PreparedPresentation,
}

impl PreparedPluginSurface {
    /// Plugin that owns the opened surface.
    #[must_use]
    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }
}

/// The plugin among `plugin_ids` that declares `surface_kind` (per `declares`), when exactly
/// one does.
///
/// # Errors
///
/// * `unsupported_startup_surface` when no loaded plugin declares the kind.
/// * `ambiguous_startup_surface` when more than one loaded plugin declares it.
pub fn owning_plugin(
    plugin_ids: &[String],
    surface_kind: &str,
    declares: impl Fn(&str) -> bool,
) -> Result<String, TuiError> {
    let mut owners = plugin_ids.iter().filter(|plugin_id| declares(plugin_id));
    let Some(owner) = owners.next() else {
        return Err(TuiError::PluginService {
            code: "unsupported_startup_surface".to_owned(),
            message: format!(
                "plugin surface `{surface_kind}` cannot be opened as a startup surface"
            ),
        });
    };
    if let Some(other) = owners.next() {
        return Err(TuiError::PluginService {
            code: "ambiguous_startup_surface".to_owned(),
            message: format!(
                "plugin surface `{surface_kind}` is declared by both `{owner}` and `{other}`"
            ),
        });
    }
    Ok(owner.clone())
}

/// Plugin-defined options as the JSON object handed to the surface factory. Values stay
/// strings; the surface interprets them.
#[must_use]
pub fn open_options(options: BTreeMap<String, String>) -> serde_json::Value {
    serde_json::Value::Object(
        options
            .into_iter()
            .map(|(key, value)| (key, serde_json::Value::String(value)))
            .collect(),
    )
}

/// Find the plugin owning `surface_kind` in `runtime` and open the surface through that
/// plugin's TUI registry (`registry_for`), handing the factory the repository and options.
///
/// # Errors
///
/// Returns ownership lookup or surface-open errors.
pub(crate) async fn open_startup_surface(
    runtime: &bcode_plugin::PluginRuntimeHost,
    registry_for: impl Fn(&str) -> Option<bcode_plugin_sdk::tui::PluginTuiRegistry>,
    surface_kind: &str,
    repo_path: PathBuf,
    options: BTreeMap<String, String>,
) -> Result<(String, bcode_plugin_sdk::tui::BoxedPluginTuiSurface), TuiError> {
    let plugin_id = owning_plugin(&runtime.plugin_ids(), surface_kind, |plugin_id| {
        runtime
            .registry()
            .tui_surface(plugin_id, surface_kind)
            .is_some()
    })?;
    let surface = crate::plugin_tui::open_plugin_tui_surface_from(
        runtime,
        registry_for(&plugin_id),
        &plugin_id,
        surface_kind,
        bcode_plugin_sdk::tui::PluginTuiSurfaceOpenRequest {
            instance_id: surface_kind.to_owned(),
            repo_path: Some(repo_path),
            session_id: None,
            target: None,
            options: open_options(options),
        },
    )
    .await
    .map_err(|error| TuiError::PluginService {
        code: "tui_surface_open_failed".to_owned(),
        message: error.to_string(),
    })?;
    Ok((plugin_id, surface))
}

/// Load the plugin runtime once, find the plugin owning `surface_kind`, and open the surface
/// without taking terminal or input ownership.
///
/// # Errors
///
/// Returns plugin load, ownership lookup, worker, or surface-open errors.
pub async fn prepare(
    surface_kind: String,
    repo_path: PathBuf,
    options: BTreeMap<String, String>,
) -> Result<PreparedPluginSurface, TuiError> {
    let handle = tokio::runtime::Handle::current();
    let (runtime, plugin_id, surface) = tokio::task::spawn_blocking(move || {
        let runtime = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
            &bcode_plugin::PluginSelection::all_enabled(),
            &crate::static_bundled_plugins(),
        )
        .map_err(|error| TuiError::PluginService {
            code: "plugin_runtime_load_failed".to_owned(),
            message: error.to_string(),
        })?;
        // Factories may perform synchronous native initialization before yielding; keep it off
        // the terminal scheduler.
        let (plugin_id, surface) = handle.block_on(open_startup_surface(
            &runtime,
            crate::plugin_tui::tui_registry,
            &surface_kind,
            repo_path,
            options,
        ))?;
        Ok::<_, TuiError>((runtime, plugin_id, surface))
    })
    .await??;
    let presentation =
        crate::runtime::prepare_presentation(&crate::static_bundled_plugins()).await?;
    Ok(PreparedPluginSurface {
        runtime,
        plugin_id,
        surface,
        presentation,
    })
}

/// Run an opened surface until it closes and return its close outcome.
///
/// # Errors
///
/// Returns interactive runtime errors.
#[allow(clippy::future_not_send)]
pub async fn run_prepared<W: Write>(
    terminal: &mut Terminal<&mut W>,
    prepared: PreparedPluginSurface,
) -> Result<Option<serde_json::Value>, TuiError> {
    let PreparedPluginSurface {
        runtime,
        plugin_id,
        surface,
        presentation,
    } = prepared;
    let result = Box::pin(crate::runtime::run_prepared_plugin_surface(
        terminal,
        plugin_id,
        surface,
        presentation,
    ))
    .await;
    drop(runtime);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(result: Result<String, TuiError>) -> String {
        match result {
            Ok(owner) => owner,
            Err(TuiError::PluginService { code, .. }) => code,
            Err(other) => panic!("unexpected error {other}"),
        }
    }

    #[test]
    fn owner_lookup_requires_exactly_one_declaring_plugin() {
        let ids = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        assert_eq!(code(owning_plugin(&ids, "kind", |id| id == "b")), "b");
        assert_eq!(
            code(owning_plugin(&ids, "kind", |_| false)),
            "unsupported_startup_surface"
        );
        assert_eq!(
            code(owning_plugin(&ids, "kind", |id| id != "a")),
            "ambiguous_startup_surface"
        );
    }

    const STARTUP_SURFACE_MANIFEST: &str = r#"
id = "test.startup-surface"
name = "Startup surface fixture"
version = "0.0.1"
[[tui_surfaces]]
description = "Any plugin-defined full-screen surface"
kind = "test.anything"
title = "Anything"
[runtime]
type = "native"
abi_version = 5
library = "libstartup_surface_fixture.dylib"
"#;

    #[derive(Default)]
    struct FixturePlugin;
    impl bcode_plugin_sdk::RustPlugin for FixturePlugin {}

    type OpenedRequests =
        std::sync::Arc<std::sync::Mutex<Vec<bcode_plugin_sdk::tui::PluginTuiSurfaceOpenRequest>>>;

    struct FixtureSurface;
    impl bcode_plugin_sdk::tui::PluginTuiSurface for FixtureSurface {
        fn id(&self) -> &'static str {
            "fixture-surface"
        }
        fn title(&self) -> &'static str {
            "Fixture"
        }
        fn render(
            &mut self,
            _area: bmux_tui::geometry::Rect,
            _frame: &mut bmux_tui::paint::PaintCx<'_, '_>,
        ) {
        }
        fn handle_event(
            &mut self,
            _event: &bmux_tui::event::Event,
            _host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
        ) -> bcode_plugin_sdk::tui::PluginTuiAction {
            bcode_plugin_sdk::tui::PluginTuiAction::None
        }
    }

    struct FixtureFactory(OpenedRequests);
    impl bcode_plugin_sdk::tui::PluginTuiSurfaceFactory for FixtureFactory {
        fn surface_kind(&self) -> &'static str {
            "test.anything"
        }
        fn open(
            &self,
            request: bcode_plugin_sdk::tui::PluginTuiSurfaceOpenRequest,
        ) -> bcode_plugin_sdk::tui::PluginTuiSurfaceFuture {
            self.0.lock().expect("requests").push(request);
            Box::pin(async {
                Ok(Box::new(FixtureSurface) as bcode_plugin_sdk::tui::BoxedPluginTuiSurface)
            })
        }
    }

    fn fixture_runtime() -> bcode_plugin::PluginRuntimeHost {
        let bundled = [bcode_plugin::StaticBundledPlugin::new(
            STARTUP_SURFACE_MANIFEST,
            bcode_plugin_sdk::static_plugin_vtable!(FixturePlugin, STARTUP_SURFACE_MANIFEST),
        )];
        let selected = bcode_plugin::filter_selected_static_plugins(
            &bundled,
            &bcode_plugin::PluginSelection::all_enabled(),
        )
        .expect("select fixture plugin");
        bcode_plugin::PluginHost::load_static_plugins(&selected)
            .expect("load fixture plugin")
            .into()
    }

    #[tokio::test]
    async fn any_declared_plugin_surface_kind_opens_at_startup() {
        let runtime = fixture_runtime();
        let opened = OpenedRequests::default();
        let registry_for = |plugin_id: &str| {
            (plugin_id == "test.startup-surface").then(|| {
                let mut registry = bcode_plugin_sdk::tui::PluginTuiRegistry::default();
                registry.register_factory(Box::new(FixtureFactory(opened.clone())));
                registry
            })
        };
        let (plugin_id, surface) = open_startup_surface(
            &runtime,
            registry_for,
            "test.anything",
            PathBuf::from("/repo"),
            BTreeMap::from([("mode".to_owned(), "full".to_owned())]),
        )
        .await
        .expect("open startup surface");
        assert_eq!(plugin_id, "test.startup-surface");
        assert_eq!(surface.id(), "fixture-surface");
        {
            let requests = opened.lock().expect("requests");
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].instance_id, "test.anything");
            assert_eq!(requests[0].repo_path, Some(PathBuf::from("/repo")));
            assert_eq!(requests[0].options, serde_json::json!({"mode": "full"}));
        }

        let missing = open_startup_surface(
            &runtime,
            registry_for,
            "test.undeclared",
            PathBuf::from("/repo"),
            BTreeMap::new(),
        )
        .await;
        assert!(matches!(
            missing,
            Err(TuiError::PluginService { ref code, .. }) if code == "unsupported_startup_surface"
        ));
        drop(runtime);
    }

    #[test]
    fn options_stay_plugin_interpreted_strings() {
        let options = open_options(BTreeMap::from([
            ("target".to_owned(), r#"{"kind":"x"}"#.to_owned()),
            ("count".to_owned(), "3".to_owned()),
        ]));
        assert_eq!(
            options,
            serde_json::json!({"target": r#"{"kind":"x"}"#, "count": "3"})
        );
    }
}
