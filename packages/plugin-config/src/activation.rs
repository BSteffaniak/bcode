//! Concrete server selection for shared plugin lifecycle work.
use std::{future::Future, pin::Pin, sync::Arc};

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
pub struct Runtime {
    identity: bmux_plugin_runtime::PluginRuntimeScopeId,
    driver: Arc<Driver>,
}
impl Runtime {
    pub(super) fn new() -> Self {
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
    pub(super) fn activation(
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
