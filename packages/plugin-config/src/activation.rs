//! Explicit shared activation authority for every bcode plugin host.
//!
//! A host grants each selected plugin activation from one owned runtime scope.
//! The scope's lifecycle executor lives exactly as long as the loaded host
//! retains those activations, and is released when the host is dropped.
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    pin::Pin,
    sync::Arc,
};

struct Driver {
    runtime: Option<tokio::runtime::Runtime>,
}
impl bcode_plugin::SharedLifecycleDriver for Driver {
    fn runtime_handle(&self) -> tokio::runtime::Handle {
        self.runtime
            .as_ref()
            .expect("serving lifecycle runtime")
            .handle()
            .clone()
    }

    fn run(&self, work: Pin<Box<dyn Future<Output = ()> + Send + '_>>) {
        // Synchronous plugin APIs may be entered from a Tokio worker. Poll only
        // on a dedicated scoped thread, never nest block_on on that worker.
        std::thread::scope(|scope| {
            let join = scope.spawn(|| {
                self.runtime
                    .as_ref()
                    .expect("serving lifecycle runtime")
                    .block_on(work);
            });
            if let Err(panic) = join.join() {
                std::panic::resume_unwind(panic);
            }
        });
    }
}
impl Drop for Driver {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            // Tokio runtime destruction may block; it cannot run on an active
            // async worker when the last selected-plugin observer disappears.
            std::thread::scope(|scope| {
                let _ = scope.spawn(move || drop(runtime)).join();
            });
        }
    }
}
struct Registrations;
impl bcode_plugin::SharedRegistrationOwner for Registrations {
    fn revoke_and_drain(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<(), bcode_plugin::PluginLoadError>> + Send + '_>> {
        // This startup adapter grants no mutable registration service. Bcode's
        // command/auth snapshots are published transactionally by PluginHost;
        // shared runtime scopes own callbacks and producer drainage.
        Box::pin(async { Ok(()) })
    }
}
struct Dispatch;
impl bmux_plugin_sdk::HostServiceDispatch for Dispatch {
    fn invoke(
        &self,
        _: &bmux_plugin_runtime::InvocationScope,
        _: bmux_plugin_sdk::HostServiceRequest<'_>,
    ) -> bmux_plugin_sdk::Result<Vec<u8>> {
        Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
            details: "no invocation-bound product authority installed".to_string(),
        })
    }
}
struct Runtime {
    identity: bmux_plugin_runtime::PluginRuntimeScopeId,
    driver: Arc<Driver>,
}
impl Runtime {
    fn new() -> Self {
        Self {
            identity: bmux_plugin_runtime::PluginRuntimeScopeId::new(),
            driver: Arc::new(Driver {
                runtime: Some(
                    tokio::runtime::Builder::new_multi_thread()
                        .enable_all()
                        .build()
                        .expect("shared plugin lifecycle runtime initialization"),
                ),
            }),
        }
    }
    fn activation(
        &self,
        plugin_id: &str,
        settings: &serde_json::Value,
        enabled: Vec<String>,
    ) -> bcode_plugin::SharedActivationContext {
        let config_dir = bcode_config::default_config_dir();
        let state_dir = bcode_config::default_state_dir();
        let path = |value: std::path::PathBuf| value.to_string_lossy().into_owned();
        let config_dir = path(config_dir);
        let state = path(state_dir.clone());
        bcode_plugin::SharedActivationContext {
            runtime: self.identity,
            native: bmux_plugin_sdk::NativeLifecycleContext {
                plugin_id: plugin_id.to_string(),
                required_capabilities: Vec::new(),
                provided_capabilities: Vec::new(),
                services: Vec::new(),
                available_capabilities: Vec::new(),
                enabled_plugins: enabled,
                plugin_search_roots: bcode_plugin::default_plugin_roots()
                    .into_iter()
                    .map(path)
                    .collect(),
                registered_plugins: Vec::new(),
                host: bmux_plugin_sdk::HostMetadata {
                    product_name: "bcode".to_string(),
                    product_version: env!("CARGO_PKG_VERSION").to_string(),
                    plugin_api_version: bmux_plugin_sdk::CURRENT_PLUGIN_API_VERSION,
                    plugin_abi_version: bmux_plugin_sdk::CURRENT_PLUGIN_ABI_VERSION,
                },
                connection: bmux_plugin_sdk::HostConnectionInfo {
                    config_dir: config_dir.clone(),
                    config_dir_candidates: vec![config_dir],
                    runtime_dir: path(
                        state_dir
                            .join("runtime")
                            .join(bcode_ipc::daemon_namespace()),
                    ),
                    data_dir: state.clone(),
                    state_dir: state,
                },
                settings: serde_json::from_value::<toml::Value>(settings.clone()).ok(),
                plugin_settings_map: std::collections::BTreeMap::default(),
                host_kernel_bridge: None,
                host_service_port: None,
                auxiliary_executables: std::collections::BTreeMap::default(),
                auxiliary_executable_errors: std::collections::BTreeMap::default(),
            },
            services: bmux_plugin_sdk::definition::PluginHostServices::new(|_| {
                Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                    details: "activation has no invocation authority".to_string(),
                })
            }),
            dispatch: Arc::new(Dispatch),
            concurrency: bmux_plugin_runtime::PluginConcurrencyConfig::Concurrent,
            progress_limits: bcode_plugin_sdk::TransientProgressLimits::default(),
            driver: self.driver.clone(),
            registrations: Arc::new(Registrations),
        }
    }
}

/// Grant selected plugins activation authority from one new shared runtime scope.
///
/// Every selected plugin in `plugin_ids` is bound. Supplied activation contexts are
/// preserved; unselected plugins receive no configuration entry from this call.
#[must_use]
pub fn bind_shared_plugin_activations(
    selection: &bcode_plugin::PluginSelection,
    plugin_ids: impl IntoIterator<Item = String>,
    mut configs: BTreeMap<String, bcode_plugin::ResolvedPluginConfig>,
) -> BTreeMap<String, bcode_plugin::ResolvedPluginConfig> {
    let enabled = plugin_ids
        .into_iter()
        .filter(|id| selection.is_enabled(id))
        .collect::<BTreeSet<_>>();
    if enabled.is_empty() {
        return configs;
    }
    let runtime = Runtime::new();
    let enabled_list = enabled.iter().cloned().collect::<Vec<_>>();
    for id in enabled {
        let config = configs.remove(&id).unwrap_or_default();
        let config = if config.shared_activation.is_some() {
            config
        } else {
            let activation = runtime.activation(&id, &config.config, enabled_list.clone());
            config.with_shared_activation(activation)
        };
        configs.insert(id, config);
    }
    configs
}

/// Load a plugin host whose selected plugins share one owned activation scope.
///
/// Exactly the supplied discovered plugins and static registrations are loaded, each
/// selected one with activation authority from one owned shared runtime scope (see
/// [`bind_shared_plugin_activations`]). Default plugin roots are not consulted; pass
/// [`bcode_plugin::discover_plugins`] to include them.
///
/// # Errors
/// Returns manifest, selection, activation or load failures.
pub fn load_plugin_host(
    selection: &bcode_plugin::PluginSelection,
    discovered: Vec<bcode_plugin::RegisteredPlugin>,
    static_plugins: &[bcode_plugin::StaticBundledPlugin],
    configs: BTreeMap<String, bcode_plugin::ResolvedPluginConfig>,
) -> Result<bcode_plugin::PluginHost, bcode_plugin::PluginLoadError> {
    let ids = bcode_plugin::static_bundled_plugin_ids(static_plugins)?
        .into_iter()
        .chain(discovered.iter().map(|plugin| plugin.manifest.id.clone()));
    let configs = bind_shared_plugin_activations(selection, ids, configs);
    bcode_plugin::PluginHost::load_discovered_with_static_bundled_and_config(
        selection,
        discovered,
        static_plugins,
        configs,
    )
}

/// [`load_plugin_host`] for a plugin runtime host with normal executors.
///
/// # Errors
/// Returns manifest, selection, activation or load failures.
pub fn load_plugin_runtime_host(
    selection: &bcode_plugin::PluginSelection,
    discovered: Vec<bcode_plugin::RegisteredPlugin>,
    static_plugins: &[bcode_plugin::StaticBundledPlugin],
    configs: BTreeMap<String, bcode_plugin::ResolvedPluginConfig>,
) -> Result<bcode_plugin::PluginRuntimeHost, bcode_plugin::PluginLoadError> {
    load_plugin_host(selection, discovered, static_plugins, configs)
        .map(|host| bcode_plugin::PluginRuntimeHost::from(host).with_selection(selection.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELLO_MANIFEST: &str = include_str!("../../../examples/hello-plugin/bcode-plugin.toml");

    fn hello() -> bcode_plugin::StaticBundledPlugin {
        bcode_plugin::StaticBundledPlugin::from_definition(
            HELLO_MANIFEST,
            bcode_hello_plugin::plugin_definition,
        )
    }

    fn echo(host: &bcode_plugin::PluginHost) -> Vec<u8> {
        let response = host
            .invoke_service(
                "example.hello",
                "example-hello/v1",
                "echo",
                b"ping".to_vec(),
            )
            .expect("activated hello serves");
        assert!(response.error.is_none());
        response.payload
    }

    /// Discover the real packaged hello backend wrapper exactly as an installed
    /// plugin: verbatim declaration plus the artifact under its declared library name.
    fn stage_hello_wrapper(root: &std::path::Path) {
        let artifact = std::path::PathBuf::from(
            std::env::var_os("BCODE_HELLO_BACKEND_WRAPPER_ARTIFACT").expect(
                "build the hello backend wrapper with package-plugin.py --component backend --feature tui; set BCODE_HELLO_BACKEND_WRAPPER_ARTIFACT to its absolute path",
            ),
        );
        assert!(artifact.is_absolute() && artifact.is_file());
        let manifest: bcode_plugin::PluginManifest = toml::from_str(HELLO_MANIFEST).unwrap();
        let bcode_plugin::PluginRuntime::Native(runtime) = &manifest.runtime;
        let directory = root.join("hello");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("bcode-plugin.toml"), HELLO_MANIFEST).unwrap();
        std::fs::copy(&artifact, directory.join(&runtime.library)).unwrap();
    }

    #[test]
    fn static_registration_activates_from_an_owned_scope_released_with_the_host() {
        let selection = bcode_plugin::PluginSelection::all_enabled();
        let configs = bind_shared_plugin_activations(
            &selection,
            ["example.hello".to_owned()],
            BTreeMap::new(),
        );
        let driver = Arc::downgrade(
            &configs["example.hello"]
                .shared_activation
                .as_ref()
                .expect("selected plugin receives activation")
                .driver,
        );
        let mut host = load_plugin_host(&selection, Vec::new(), &[hello()], configs).unwrap();
        assert_eq!(echo(&host), b"ping");
        assert!(
            driver.upgrade().is_some(),
            "the host owns its runtime scope"
        );
        host.deactivate_all().unwrap();
        drop(host);
        assert!(
            driver.upgrade().is_none(),
            "the runtime scope is released when its host shuts down"
        );
    }

    #[test]
    fn discovered_native_wrapper_receives_activation_authority() {
        let root = tempfile::tempdir().unwrap();
        stage_hello_wrapper(root.path());
        let discovered = bcode_plugin::discover_plugins_in_roots(&[root.path().into()]).unwrap();
        let mut host = load_plugin_host(
            &bcode_plugin::PluginSelection::all_enabled(),
            discovered,
            &[],
            BTreeMap::new(),
        )
        .expect("discovered native plugin loads with explicit activation");
        assert_eq!(echo(&host), b"ping");
        host.deactivate_all().unwrap();
    }

    #[test]
    fn binding_preserves_supplied_activation_and_skips_unselected_plugins() {
        let selection = bcode_plugin::PluginSelection {
            mode: bcode_plugin::PluginSelectionMode::Explicit,
            enabled: BTreeSet::from(["a".to_owned(), "b".to_owned()]),
            disabled: BTreeSet::new(),
        };
        let supplied =
            bind_shared_plugin_activations(&selection, ["a".to_owned()], BTreeMap::new());
        let supplied_runtime = supplied["a"].shared_activation.as_ref().unwrap().runtime;
        let bound = bind_shared_plugin_activations(
            &selection,
            ["a", "b", "unselected"].map(str::to_owned),
            supplied,
        );
        assert_eq!(
            bound["a"].shared_activation.as_ref().unwrap().runtime,
            supplied_runtime
        );
        let b = bound["b"].shared_activation.as_ref().unwrap();
        assert_eq!(b.native.enabled_plugins, ["a", "b"]);
        assert!(!bound.contains_key("unselected"));
    }
}
