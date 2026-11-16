//! Explicit test host for the production shared-factory selection path.
use std::{collections::BTreeMap, future::Future, pin::Pin, sync::Arc};

struct Driver(tokio::runtime::Runtime);
impl bcode_plugin::SharedLifecycleDriver for Driver {
    fn runtime_handle(&self) -> tokio::runtime::Handle {
        self.0.handle().clone()
    }

    fn run(&self, work: Pin<Box<dyn Future<Output = ()> + Send + '_>>) {
        self.0.block_on(work);
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
        request: bmux_plugin_sdk::HostServiceRequest<'_>,
    ) -> bmux_plugin_sdk::Result<Vec<u8>> {
        invocation_settings(&request)
    }
}

fn invocation_settings(
    request: &bmux_plugin_sdk::HostServiceRequest<'_>,
) -> bmux_plugin_sdk::Result<Vec<u8>> {
    if request.capability == "bcode.product"
        && request.kind == bmux_plugin_sdk::ServiceKind::Query
        && request.interface == bcode_plugin_sdk::definition::PRODUCT_HOST_INTERFACE
        && request.operation == "invocation_context"
        && request.payload.is_empty()
    {
        return serde_json::to_vec(&bcode_plugin_sdk::definition::ProductInvocationSettings {
            config: bcode_plugin_sdk::PluginConfigContext::default(),
            transient_progress_limits: bcode_plugin_sdk::TransientProgressLimits::default(),
        })
        .map_err(|error| bmux_plugin_sdk::PluginError::ServiceProtocol {
            details: error.to_string(),
        });
    }
    Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
        details: "bundled test denies undeclared host service".into(),
    })
}

pub fn load_selected(
    selected: &[(
        bcode_plugin::PluginManifest,
        bcode_plugin::StaticBundledPlugin,
    )],
) -> Result<bcode_plugin::PluginHost, bcode_plugin::PluginLoadError> {
    let bundles: Vec<_> = selected
        .iter()
        .map(|(_, registration)| *registration)
        .collect();
    let runtime = bmux_plugin_runtime::PluginRuntimeScopeId::new();
    let enabled: Vec<_> = selected
        .iter()
        .map(|(manifest, _)| manifest.id.clone())
        .collect();
    let driver = Arc::new(Driver(
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("bundled test lifecycle runtime"),
    ));
    let mut configs = BTreeMap::new();
    for (manifest, _) in selected {
        let path = std::env::temp_dir()
            .join("bcode-bundled-shared-tests")
            .to_string_lossy()
            .into_owned();
        let activation = bcode_plugin::SharedActivationContext {
            runtime,
            native: bmux_plugin_sdk::NativeLifecycleContext {
                plugin_id: manifest.id.clone(),
                required_capabilities: Vec::new(),
                provided_capabilities: Vec::new(),
                services: Vec::new(),
                available_capabilities: Vec::new(),
                enabled_plugins: enabled.clone(),
                plugin_search_roots: Vec::new(),
                registered_plugins: Vec::new(),
                host: bmux_plugin_sdk::HostMetadata {
                    product_name: "bcode".into(),
                    product_version: "test".into(),
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
                plugin_settings_map: BTreeMap::new(),
                host_kernel_bridge: None,
                host_service_port: None,
                auxiliary_executables: BTreeMap::new(),
                auxiliary_executable_errors: BTreeMap::new(),
            },
            services: bmux_plugin_sdk::definition::PluginHostServices::new(|request| {
                invocation_settings(&request)
            }),
            dispatch: Arc::new(Dispatch),
            concurrency: bmux_plugin_runtime::PluginConcurrencyConfig::Concurrent,
            progress_limits: bcode_plugin_sdk::TransientProgressLimits::default(),
            driver: driver.clone(),
            registrations: Arc::new(Registrations),
        };
        configs.insert(
            manifest.id.clone(),
            bcode_plugin::ResolvedPluginConfig::default().with_shared_activation(activation),
        );
    }
    bcode_plugin::PluginHost::load_discovered_with_static_bundled_and_config(
        &bcode_plugin::PluginSelection {
            mode: bcode_plugin::PluginSelectionMode::Explicit,
            enabled: enabled.into_iter().collect(),
            disabled: std::collections::BTreeSet::default(),
        },
        Vec::new(),
        &bundles,
        configs,
    )
}
