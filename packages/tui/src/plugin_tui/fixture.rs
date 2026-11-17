//! Explicit test lifecycle authority; never used by production discovery.
use std::{future::Future, pin::Pin, sync::Arc};

struct Driver(Option<tokio::runtime::Runtime>);
impl Drop for Driver {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            std::thread::spawn(move || drop(runtime))
                .join()
                .expect("fixture runtime cleanup");
        }
    }
}
impl bcode_plugin::SharedLifecycleDriver for Driver {
    fn runtime_handle(&self) -> tokio::runtime::Handle {
        self.0.as_ref().expect("fixture runtime").handle().clone()
    }

    fn run(&self, work: Pin<Box<dyn Future<Output = ()> + Send + '_>>) {
        std::thread::scope(|scope| {
            scope
                .spawn(|| self.0.as_ref().expect("fixture runtime").block_on(work))
                .join()
                .expect("fixture lifecycle worker");
        });
    }
}
struct Registrations;
impl bcode_plugin::SharedRegistrationOwner for Registrations {
    fn revoke_and_drain(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<(), bcode_plugin::PluginLoadError>> + Send + '_>> {
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
            details: "fixture has no admitted product operation".to_owned(),
        })
    }
}

pub fn runtime(
    selection: &bcode_plugin::PluginSelection,
    plugins: &[bcode_plugin::StaticBundledPlugin],
) -> Result<bcode_plugin::PluginRuntimeHost, bcode_plugin::PluginLoadError> {
    let selected = bcode_plugin::filter_selected_static_registrations(plugins, selection)?;
    load_static(&selected).map(Into::into)
}

pub fn load_static(
    registrations: &[(
        bcode_plugin::PluginManifest,
        bcode_plugin::StaticBundledPlugin,
    )],
) -> Result<bcode_plugin::PluginHost, bcode_plugin::PluginLoadError> {
    let runtime = bmux_plugin_runtime::PluginRuntimeScopeId::new();
    let configs = registrations
        .iter()
        .map(|(manifest, _)| {
            let mut config = config(&manifest.id);
            if let Some(activation) = config.shared_activation.as_mut() {
                activation.runtime = runtime;
            }
            (manifest.id.clone(), config)
        })
        .collect();
    let plugins = registrations
        .iter()
        .map(|(_, plugin)| *plugin)
        .collect::<Vec<_>>();
    bcode_plugin::PluginHost::load_discovered_with_static_bundled_and_config(
        &bcode_plugin::PluginSelection::all_enabled(),
        Vec::new(),
        &plugins,
        configs,
    )
}

pub(super) fn config(plugin_id: &str) -> bcode_plugin::ResolvedPluginConfig {
    let path = std::env::temp_dir()
        .join("bcode-tui-selected-fixture")
        .to_string_lossy()
        .into_owned();
    bcode_plugin::ResolvedPluginConfig::default().with_shared_activation(
        bcode_plugin::SharedActivationContext {
            runtime: bmux_plugin_runtime::PluginRuntimeScopeId::new(),
            native: bmux_plugin_sdk::NativeLifecycleContext {
                plugin_id: plugin_id.to_owned(),
                required_capabilities: Vec::new(),
                provided_capabilities: Vec::new(),
                services: Vec::new(),
                available_capabilities: Vec::new(),
                enabled_plugins: vec![plugin_id.to_owned()],
                plugin_search_roots: vec![path.clone()],
                registered_plugins: Vec::new(),
                host: bmux_plugin_sdk::HostMetadata {
                    product_name: "bcode".to_owned(),
                    product_version: "fixture".to_owned(),
                    plugin_api_version: bmux_plugin_sdk::CURRENT_PLUGIN_API_VERSION,
                    plugin_abi_version: bmux_plugin_sdk::CURRENT_PLUGIN_ABI_VERSION,
                },
                connection: bmux_plugin_sdk::HostConnectionInfo {
                    config_dir: path.clone(),
                    config_dir_candidates: vec![path.clone()],
                    runtime_dir: path.clone(),
                    data_dir: path.clone(),
                    state_dir: path,
                },
                settings: None,
                plugin_settings_map: std::collections::BTreeMap::new(),
                host_kernel_bridge: None,
                host_service_port: None,
                capabilities: bmux_plugin_sdk::definition::ActivationCapabilities::default(),
                auxiliary_executables: std::collections::BTreeMap::new(),
                auxiliary_executable_errors: std::collections::BTreeMap::new(),
            },
            services: bmux_plugin_sdk::definition::PluginHostServices::new(|_| {
                Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                    details: "fixture denies unbound calls".to_owned(),
                })
            }),
            dispatch: Arc::new(Dispatch),
            concurrency: bmux_plugin_runtime::PluginConcurrencyConfig::Concurrent,
            progress_limits: bcode_plugin_sdk::TransientProgressLimits::default(),
            // The selected reactor must keep running accepted callback work
            // between lifecycle calls (`SharedLifecycleDriver::runtime_handle`),
            // as the serving runtime does; a current-thread runtime only runs
            // while a lifecycle call blocks on it.
            driver: Arc::new(Driver(Some(
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .unwrap(),
            ))),
            registrations: Arc::new(Registrations),
        },
    )
}
