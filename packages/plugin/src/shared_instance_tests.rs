/// Explicit test selection for shared native fixtures; no production defaults.
pub fn fixture_config(plugin_id: &str) -> crate::ResolvedPluginConfig {
    let mut config = tests::config(bmux_plugin_runtime::PluginRuntimeScopeId::new());
    let activation = config
        .shared_activation
        .as_mut()
        .expect("fixture activation");
    activation.native.plugin_id = plugin_id.to_string();
    activation.native.enabled_plugins = vec![plugin_id.to_string()];
    config
}

#[cfg(test)]
mod tests {
    use std::{future::Future, pin::Pin, sync::Arc};

    struct Driver(tokio::runtime::Runtime);
    impl crate::SharedLifecycleDriver for Driver {
        fn runtime_handle(&self) -> tokio::runtime::Handle {
            self.0.handle().clone()
        }

        fn run(&self, work: Pin<Box<dyn Future<Output = ()> + Send + '_>>) {
            self.0.block_on(work);
        }
    }
    struct Registrations(Arc<std::sync::atomic::AtomicUsize>);
    impl crate::SharedRegistrationOwner for Registrations {
        fn revoke_and_drain(
            &self,
        ) -> Pin<Box<dyn Future<Output = Result<(), crate::PluginLoadError>> + Send + '_>> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
                details: "test host denies unbound service".to_string(),
            })
        }
    }
    pub(super) fn config(
        runtime: bmux_plugin_runtime::PluginRuntimeScopeId,
    ) -> crate::ResolvedPluginConfig {
        // Deliberate test-only selection. Production must supply its actual selected paths.
        let root = std::env::temp_dir().join("bcode-shared-activation-fixture");
        let path = root.to_string_lossy().into_owned();
        crate::ResolvedPluginConfig::default().with_shared_activation(
            crate::SharedActivationContext {
                runtime,
                native: bmux_plugin_sdk::NativeLifecycleContext {
                    plugin_id: "example.hello".to_string(),
                    required_capabilities: Vec::new(),
                    provided_capabilities: Vec::new(),
                    services: Vec::new(),
                    available_capabilities: Vec::new(),
                    enabled_plugins: vec!["example.hello".to_string()],
                    plugin_search_roots: vec![path.clone()],
                    registered_plugins: Vec::new(),
                    host: bmux_plugin_sdk::HostMetadata {
                        product_name: "bcode".to_string(),
                        product_version: "test".to_string(),
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
                    settings: Some(toml::Value::String("selected-test-setting".to_string())),
                    plugin_settings_map: std::collections::BTreeMap::default(),
                    host_kernel_bridge: None,
                    host_service_port: None,
                    auxiliary_executables: std::collections::BTreeMap::default(),
                    auxiliary_executable_errors: std::collections::BTreeMap::default(),
                },
                services: bmux_plugin_sdk::definition::PluginHostServices::new(|_| {
                    Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
                        details: "test denied".to_string(),
                    })
                }),
                dispatch: Arc::new(Dispatch),
                concurrency: bmux_plugin_runtime::PluginConcurrencyConfig::Concurrent,
                progress_limits: bcode_plugin_sdk::TransientProgressLimits::default(),
                driver: Arc::new(Driver(
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("test runtime"),
                )),
                registrations: Arc::new(Registrations(Arc::new(
                    std::sync::atomic::AtomicUsize::new(0),
                ))),
            },
        )
    }

    fn hello() -> crate::LoadedPlugin {
        let manifest = toml::from_str(include_str!(
            "../../../examples/hello-plugin/bcode-plugin.toml"
        ))
        .expect("hello declaration");
        crate::load_plugin_definition_with_config(
            manifest,
            bcode_hello_plugin::plugin_definition(),
            config(bmux_plugin_runtime::PluginRuntimeScopeId::new()),
        )
        .expect("shared instance")
    }

    #[test]
    fn selected_host_recovery_retains_sessions_after_all_author_observers_drop() {
        let plugin = hello();
        plugin.activate().unwrap();
        let host = plugin.config.shared_activation.as_ref().unwrap();
        let session = host
            .retain_selected_product_host(
                &plugin,
                bmux_plugin_sdk::definition::PluginHostServices::new(|_| panic!("sync fallback")),
                crate::ProductHostLimits::default(),
            )
            .unwrap();
        let id = session.id();
        let job = session
            .begin_job(Arc::new(()), std::num::NonZeroUsize::new(1).unwrap())
            .unwrap();
        drop(job);
        drop(session);
        assert!(host.forget_released_product_host(&plugin, id).is_err());
        let page = host.product_host_recovery_page(&plugin, None, 1).unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].id(), id);
        let recovered = &page[0];
        host.drive(recovered.join()).unwrap();
        for job in recovered.jobs().unwrap() {
            recovered.release_job(&job).unwrap();
        }
        recovered.release().unwrap();
        host.forget_released_product_host(&plugin, id).unwrap();
        assert!(
            host.product_host_recovery_page(&plugin, None, 1)
                .unwrap()
                .is_empty()
        );
        plugin.deactivate().unwrap();
    }

    #[test]
    fn retained_extension_rejects_foreign_instance_before_admitting_effects() {
        let first = hello();
        let other = hello();
        first.activate().unwrap();
        other.activate().unwrap();
        let host = first.config.shared_activation.as_ref().unwrap();
        let crate::LoadedPluginBackend::Shared(instance) = &first.backend else {
            panic!("shared fixture");
        };
        let scope = instance.activation_scope().unwrap();
        let session = host
            .retain_product_host(
                &scope,
                bmux_plugin_sdk::definition::PluginHostServices::new(|_| panic!("sync fallback")),
                Arc::new(()),
            )
            .unwrap();
        let job = session
            .begin_job(Arc::new(()), std::num::NonZeroUsize::new(1).unwrap())
            .unwrap();
        assert!(
            host.start_product_host_extension(
                &other,
                &job,
                "bcode.component-extensions/v1".to_owned(),
                1,
                Vec::new(),
            )
            .is_err()
        );
        assert_eq!(
            job.callback_outcomes().unwrap(),
            crate::ProductHostOutcomes::new()
        );
        host.drive(job.join()).unwrap();
        session.acknowledge_job(&job).unwrap();
        host.drive(session.join()).unwrap();
        session.release().unwrap();
        first.deactivate().unwrap();
        other.deactivate().unwrap();
    }

    #[test]
    fn retained_extension_negotiation_failure_is_retained_without_replay() {
        let plugin = hello();
        plugin.activate().unwrap();
        let host = plugin.config.shared_activation.as_ref().unwrap();
        let crate::LoadedPluginBackend::Shared(instance) = &plugin.backend else {
            panic!("shared fixture");
        };
        let session = host
            .retain_product_host(
                &instance.activation_scope().unwrap(),
                bmux_plugin_sdk::definition::PluginHostServices::new(|_| panic!("sync fallback")),
                Arc::new(()),
            )
            .unwrap();
        let job = session
            .begin_job(Arc::new(()), std::num::NonZeroUsize::new(1).unwrap())
            .unwrap();
        host.start_product_host_extension(
            &plugin,
            &job,
            "unknown.product.extension/v999".to_owned(),
            999,
            Vec::new(),
        )
        .unwrap();
        host.drive(job.join()).unwrap();
        let outcomes = job.callback_outcomes().unwrap();
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(&outcomes[0], Some(Err(_))));
        assert!(
            host.start_product_host_extension(
                &plugin,
                &job,
                "unknown.product.extension/v999".to_owned(),
                999,
                Vec::new(),
            )
            .is_err()
        );
        assert_eq!(job.callback_outcomes().unwrap().len(), 1);
        job.acknowledge_callback(0, job.callback_receipt(0).unwrap().unwrap())
            .unwrap();
        session.acknowledge_job(&job).unwrap();
        host.drive(session.join()).unwrap();
        session.release().unwrap();
        plugin.deactivate().unwrap();
    }

    #[test]
    fn shared_static_dispatch_has_fresh_product_state_and_stable_registration_snapshot() {
        let first = hello();
        let second = hello();
        first.activate().expect("first activation");
        second.activate().expect("second activation");
        let payload = vec![0, 255, 128];
        let response = first
            .invoke_service("example-hello/v1", "echo", payload.clone())
            .expect("typed invoke");
        assert_eq!(response.payload, payload);
        first
            .handle_event("example.event", Vec::new())
            .expect("first event");
        assert_eq!(
            first
                .invoke_service("example-hello/v1", "event-count", Vec::new())
                .expect("first state")
                .payload,
            b"1"
        );
        assert_eq!(
            second
                .invoke_service("example-hello/v1", "event-count", Vec::new())
                .expect("second state")
                .payload,
            b"0"
        );
        let mut events = Vec::new();
        first
            .invoke_service_with_events("example-hello/v1", "emit-event", Vec::new(), |event| {
                events.push(event);
            })
            .expect("live events");
        assert_eq!(events, vec![b"hello-event".to_vec()]);
        let cancellation = bcode_plugin_sdk::ServiceCancellation::default();
        cancellation.cancel();
        assert!(
            first
                .invoke_service_with_bridge(
                    "example-hello/v1",
                    "wait-cancelled",
                    Vec::new(),
                    |_| {},
                    |_, _| Err("unavailable".to_string()),
                    &cancellation
                )
                .is_err()
        );
        let mut commands = bcode_command::CommandRegistry::new();
        first
            .register_commands(&mut commands)
            .expect("snapshot registration");
        first.deactivate().expect("first cleanup");
        let response = second
            .invoke_service("example-hello/v1", "echo", vec![1])
            .expect("independent instance");
        assert_eq!(response.payload, vec![1]);
        second.deactivate().expect("second cleanup");
    }

    #[test]
    fn normal_bundled_startup_selects_shared_definition() {
        let registration = crate::StaticBundledPlugin::from_definition(
            include_str!("../../../examples/hello-plugin/bcode-plugin.toml"),
            bcode_hello_plugin::plugin_definition,
        );
        {
            let host = crate::PluginHost::load_discovered_with_static_bundled_and_config(
                &crate::PluginSelection::all_enabled(),
                Vec::new(),
                &[registration],
                std::collections::BTreeMap::from([(
                    "example.hello".to_string(),
                    config(bmux_plugin_runtime::PluginRuntimeScopeId::new()),
                )]),
            )
            .expect("production startup");
            assert!(matches!(
                &host.loaded_plugins()[0].backend,
                crate::LoadedPluginBackend::Shared(_)
            ));
            assert_eq!(
                host.invoke_service("example.hello", "example-hello/v1", "echo", vec![7])
                    .expect("typed service")
                    .payload,
                vec![7]
            );
        }
    }

    #[test]
    fn selected_extensions_bind_same_product_session_without_an_ambient_reactor() {
        use bcode_plugin_sdk::definition::host_session_models::{
            HostSessionRequest as Q, HostSessionResponse as R,
        };
        let selected = config(bmux_plugin_runtime::PluginRuntimeScopeId::new());
        let context = selected.shared_activation.clone().unwrap();
        let registration = crate::StaticBundledPlugin::from_definition(
            include_str!("../../../examples/hello-plugin/bcode-plugin.toml"),
            bcode_hello_plugin::plugin_definition,
        );
        let host = crate::PluginHost::load_discovered_with_static_bundled_and_config(
            &crate::PluginSelection::all_enabled(),
            Vec::new(),
            &[registration],
            std::collections::BTreeMap::from([("example.hello".to_string(), selected)]),
        )
        .unwrap();
        let plugin = &host.loaded_plugins()[0];
        let first = plugin
            .product_extensions()
            .unwrap()
            .product_host_session()
            .unwrap();
        let second = plugin
            .product_extensions()
            .unwrap()
            .product_host_session()
            .unwrap();
        let request = Q::BeginJob {
            request_id: uuid::Uuid::new_v4(),
            reservation: bcode_plugin_sdk::definition::host_session_models::JobReservation {
                operations: 1,
                request_bytes: 256,
                response_bytes: 256,
            },
        };
        let begun = context.drive(first.request(request.clone())).unwrap();
        assert_eq!(context.drive(second.request(request)).unwrap(), begun);
        let R::JobBegun { job } = begun else {
            panic!("job")
        };
        assert_eq!(
            context.drive(first.request(Q::JoinJob { job })).unwrap(),
            R::Joined
        );
        assert_eq!(
            context.drive(first.request(Q::ReleaseJob { job })).unwrap(),
            R::Released
        );
        assert_eq!(
            context.drive(first.request(Q::JoinSession)).unwrap(),
            R::Joined
        );
        assert_eq!(
            context.drive(first.request(Q::ReleaseSession)).unwrap(),
            R::Released
        );
        let sessions = context
            .product_host_recovery_page(plugin, None, 10)
            .unwrap();
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].is_released());
        context
            .forget_released_product_host(plugin, sessions[0].id())
            .unwrap();
    }

    #[test]
    fn definition_only_selection_never_calls_disabled_factory() {
        fn disabled_factory() -> bcode_plugin_sdk::definition::PluginDefinition {
            panic!("disabled definition must not be constructed")
        }
        let registration = crate::StaticBundledPlugin::from_definition(
            include_str!("../../../examples/hello-plugin/bcode-plugin.toml"),
            disabled_factory,
        );
        let mut selection = crate::PluginSelection::all_enabled();
        selection.disabled.insert("example.hello".to_string());
        let host = crate::PluginHost::load_discovered_with_static_bundled_and_config(
            &selection,
            Vec::new(),
            &[registration],
            std::collections::BTreeMap::new(),
        )
        .expect("disabled definition startup");
        assert!(host.loaded_plugins().is_empty());
        assert!(
            crate::filter_selected_static_registrations(&[registration], &selection)
                .expect("disabled selection")
                .is_empty()
        );
    }

    #[test]
    fn selected_definition_identity_mismatch_does_not_fall_back() {
        fn wrong_identity() -> bcode_plugin_sdk::definition::PluginDefinition {
            let mut definition = bcode_hello_plugin::plugin_definition();
            definition.metadata.plugin_id = "example.wrong".to_string();
            definition
        }
        let registration = crate::StaticBundledPlugin::from_definition(
            include_str!("../../../examples/hello-plugin/bcode-plugin.toml"),
            wrong_identity,
        );
        let result = crate::PluginHost::load_discovered_with_static_bundled_and_config(
            &crate::PluginSelection::all_enabled(),
            Vec::new(),
            &[registration],
            std::collections::BTreeMap::from([(
                "example.hello".to_string(),
                config(bmux_plugin_runtime::PluginRuntimeScopeId::new()),
            )]),
        );
        assert!(
            matches!(result, Err(crate::PluginLoadError::ManifestIdMismatch {
            file_id, library_id,
        }) if file_id == "example.hello" && library_id == "example.wrong")
        );
    }

    #[test]
    fn absent_activation_selection_fails_before_factory_creation() {
        let manifest = toml::from_str(include_str!(
            "../../../examples/hello-plugin/bcode-plugin.toml"
        ))
        .expect("manifest");
        assert!(
            crate::load_plugin_definition(manifest, bcode_hello_plugin::plugin_definition())
                .is_err()
        );
    }

    #[derive(Clone)]
    struct FailingRegistration {
        fail_typed: bool,
        deactivated: Arc<std::sync::atomic::AtomicUsize>,
        observed: Arc<std::sync::Mutex<Option<bmux_plugin_sdk::NativeLifecycleContext>>>,
    }
    impl bcode_plugin_sdk::definition::BcodePluginService for FailingRegistration {
        fn invoke(
            &self,
            _: bcode_plugin_sdk::NativeServiceContext,
        ) -> bcode_plugin_sdk::ServiceResponse {
            panic!("failed activation cannot admit product work")
        }
        fn event(
            &self,
            _: bcode_plugin_sdk::NativeEventContext,
        ) -> Result<(), bcode_plugin_sdk::PluginError> {
            Ok(())
        }
    }
    impl bmux_plugin_sdk::definition::PluginInstance for FailingRegistration {
        fn activate_scoped(
            &self,
            context: bmux_plugin_sdk::definition::PluginLifecycleContext,
            services: Option<bmux_plugin_sdk::definition::PluginHostServices>,
        ) -> bmux_plugin_sdk::definition::PluginFuture<
            '_,
            bmux_plugin_sdk::definition::PluginResult<()>,
        > {
            assert!(services.is_some());
            *self.observed.lock().expect("observation") = Some(context.native);
            Box::pin(async { Ok(()) })
        }
        fn deactivate_scoped(
            &self,
            _: bmux_plugin_sdk::definition::PluginLifecycleContext,
            services: Option<bmux_plugin_sdk::definition::PluginHostServices>,
        ) -> bmux_plugin_sdk::definition::PluginFuture<
            '_,
            bmux_plugin_sdk::definition::PluginResult<()>,
        > {
            assert!(services.is_some(), "rollback keeps active host bridge");
            self.deactivated
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
        fn register_typed_services(
            &self,
            registry: &mut bmux_plugin_sdk::TypedServiceRegistry,
        ) -> bmux_plugin_sdk::definition::PluginResult<()> {
            let provider: Arc<dyn bcode_plugin_sdk::definition::BcodePluginService> =
                Arc::new(self.clone());
            registry.insert_typed(
                bmux_plugin_sdk::HostScope::new("bcode.product").expect("scope"),
                bmux_plugin_sdk::ServiceKind::Command,
                bcode_plugin_sdk::definition::PRODUCT_SERVICE_INTERFACE,
                provider,
            );
            if self.fail_typed {
                return Err(bmux_plugin_sdk::PluginCommandError::unavailable(
                    "partial registration",
                ));
            }
            Ok(())
        }
        fn extension(
            &self,
            _: bmux_plugin_sdk::definition::PluginCallContext,
            _: String,
            _: Vec<u8>,
        ) -> bmux_plugin_sdk::definition::PluginFuture<
            '_,
            bmux_plugin_sdk::definition::PluginResult<Vec<u8>>,
        > {
            Box::pin(async { Ok(b"malformed snapshot".to_vec()) })
        }
    }

    #[test]
    fn typed_registration_and_snapshot_failures_rollback_with_selected_context() {
        for fail_typed in [true, false] {
            let deactivated = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let observed = Arc::new(std::sync::Mutex::new(None));
            let probe = FailingRegistration {
                fail_typed,
                deactivated: deactivated.clone(),
                observed: observed.clone(),
            };
            let definition = bmux_plugin_sdk::definition::PluginDefinition::new(
                bcode_hello_plugin::plugin_definition().metadata,
                move |_| {
                    Ok(Arc::new(probe.clone())
                        as Arc<dyn bmux_plugin_sdk::definition::PluginInstance>)
                },
            );
            let mut selected = config(bmux_plugin_runtime::PluginRuntimeScopeId::new());
            let revoked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            selected
                .shared_activation
                .as_mut()
                .expect("selection")
                .registrations = Arc::new(Registrations(revoked.clone()));
            let native = selected
                .shared_activation
                .as_ref()
                .expect("selection")
                .native
                .clone();
            let manifest = toml::from_str(include_str!(
                "../../../examples/hello-plugin/bcode-plugin.toml"
            ))
            .expect("manifest");
            let plugin = crate::load_plugin_definition_with_config(manifest, definition, selected)
                .expect("creation");
            assert!(plugin.activate().is_err());
            assert_eq!(
                observed.lock().expect("observation").as_ref(),
                Some(&native)
            );
            assert_eq!(revoked.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert_eq!(deactivated.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert!(
                plugin
                    .invoke_service("example-hello/v1", "echo", Vec::new())
                    .is_err()
            );
            plugin.deactivate().expect("idempotent cleanup");
            assert_eq!(deactivated.load(std::sync::atomic::Ordering::SeqCst), 1);
        }
    }

    fn context() -> bcode_plugin_sdk::NativeServiceContext {
        bcode_plugin_sdk::NativeServiceContext {
            plugin_id: "example.hello".to_string(),
            request: bcode_plugin_sdk::ServiceRequest {
                interface_id: "example-hello/v1".to_string(),
                operation: "echo".to_string(),
                payload: Vec::new(),
            },
            config: bcode_plugin_sdk::PluginConfigContext {
                config: serde_json::Value::Null,
                redacted_config: serde_json::Value::Null,
                secrets: std::collections::BTreeMap::default(),
                state_root: None,
                durable_state_root: None,
            },
            events: bcode_plugin_sdk::ServiceEventEmitter::default(),
            cancellation: bcode_plugin_sdk::ServiceCancellation::default(),
            bridge: bcode_plugin_sdk::ServiceBridge::default(),
            transient_progress_limits: bcode_plugin_sdk::TransientProgressLimits::default(),
        }
    }

    #[test]
    fn closing_signals_product_cancellation_and_waits_for_delayed_cleanup() {
        let plugin = Arc::new(hello());
        plugin.activate().expect("activation");
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (cancelled_tx, cancelled_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (closed_tx, closed_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let running = plugin.clone();
            let invocation = scope.spawn(move || {
                let crate::LoadedPluginBackend::Shared(instance) = &running.backend else {
                    panic!("shared");
                };
                instance
                    .invoke(context(), |_, context, _| {
                        started_tx.send(()).expect("started");
                        assert!(
                            context
                                .cancellation
                                .wait_cancelled(std::time::Duration::from_secs(2))
                        );
                        cancelled_tx.send(()).expect("cancelled");
                        release_rx.recv().expect("delayed cleanup release");
                    })
                    .expect("settlement");
            });
            started_rx.recv().expect("admitted");
            let closing = plugin.clone();
            let teardown = scope.spawn(move || {
                let result = closing.deactivate();
                closed_tx.send(()).expect("close observation");
                result
            });
            cancelled_rx
                .recv()
                .expect("runtime closure propagated to product token");
            assert!(
                closed_rx.try_recv().is_err(),
                "cannot destroy author while cleanup is delayed"
            );
            assert!(
                plugin.deactivate().is_err(),
                "second deactivation cannot acquire the lifecycle reservation"
            );
            release_tx.send(()).expect("settle author");
            invocation.join().expect("invocation thread");
            // A competing retirement may make teardown explicitly retryable; it
            // must never release the uncertain child or repeat author cleanup.
            let _ = teardown.join().expect("teardown thread");
        });
        plugin.deactivate().expect("settled explicit cleanup retry");
        assert!(
            plugin
                .invoke_service("example-hello/v1", "echo", Vec::new())
                .is_err()
        );
    }

    #[test]
    fn author_panic_after_child_creation_is_retained_and_child_can_be_retired() {
        let plugin = hello();
        plugin.activate().expect("activation");
        let crate::LoadedPluginBackend::Shared(instance) = &plugin.backend else {
            panic!("shared");
        };
        let result: Result<(), _> = instance.invoke(context(), |_, _, _| panic!("author panic"));
        assert!(result.is_err());
        assert_ne!(plugin.shared_failure_details(), Vec::<String>::new());
        plugin
            .deactivate()
            .expect("child retired and implementation cleanup settled");
        assert!(
            !plugin.shared_failure_details().is_empty(),
            "cleanup does not erase inspection evidence"
        );
    }

    #[test]
    fn missing_runtime_selection_cannot_be_replaced_by_a_new_plugin_runtime() {
        let manifest = toml::from_str(include_str!(
            "../../../examples/hello-plugin/bcode-plugin.toml"
        ))
        .expect("manifest");
        let selected = config(bmux_plugin_runtime::PluginRuntimeScopeId::new());
        let plugin = crate::load_plugin_definition_with_config(
            manifest,
            bcode_hello_plugin::plugin_definition(),
            selected.clone(),
        )
        .expect("selected");
        assert_eq!(
            plugin
                .config()
                .shared_activation
                .as_ref()
                .expect("retained selection")
                .runtime,
            selected
                .shared_activation
                .as_ref()
                .expect("original selection")
                .runtime
        );
        plugin.deactivate().expect("created instance retirement");
    }

    #[test]
    fn author_state_is_destroyed_inside_teardown_even_with_retained_observer() {
        let base = bcode_hello_plugin::plugin_definition();
        let weak = Arc::new(std::sync::Mutex::new(None));
        let observed = weak.clone();
        let definition = bmux_plugin_sdk::definition::PluginDefinition::new(
            base.metadata.clone(),
            move |scope| {
                let implementation = base.create(scope)?;
                *observed.lock().expect("weak observation") = Some(Arc::downgrade(&implementation));
                Ok(implementation)
            },
        );
        let manifest = toml::from_str(include_str!(
            "../../../examples/hello-plugin/bcode-plugin.toml"
        ))
        .expect("manifest");
        let plugin = crate::load_plugin_definition_with_config(
            manifest,
            definition,
            config(bmux_plugin_runtime::PluginRuntimeScopeId::new()),
        )
        .expect("create");
        plugin.activate().expect("activate");
        assert!(
            weak.lock()
                .expect("weak observation")
                .as_ref()
                .expect("implementation")
                .upgrade()
                .is_some()
        );
        plugin.deactivate().expect("teardown");
        assert!(
            weak.lock()
                .expect("weak observation")
                .as_ref()
                .expect("implementation")
                .upgrade()
                .is_none()
        );
        assert_eq!(
            plugin.manifest().id,
            "example.hello",
            "observer remains usable without author state"
        );
    }

    #[test]
    fn owning_runtime_identity_is_shared_only_when_explicitly_selected() {
        let runtime = bmux_plugin_runtime::PluginRuntimeScopeId::new();
        let first_config = config(runtime);
        let second_config = config(runtime);
        let independent_config = config(bmux_plugin_runtime::PluginRuntimeScopeId::new());
        let load = |selected| {
            let manifest = toml::from_str(include_str!(
                "../../../examples/hello-plugin/bcode-plugin.toml"
            ))
            .expect("manifest");
            crate::load_plugin_definition_with_config(
                manifest,
                bcode_hello_plugin::plugin_definition(),
                selected,
            )
            .expect("create")
        };
        let first = load(first_config);
        let second = load(second_config);
        let independent = load(independent_config);
        let identity = |plugin: &crate::LoadedPlugin| {
            plugin
                .config()
                .shared_activation
                .as_ref()
                .expect("selection")
                .runtime
        };
        assert_eq!(identity(&first), identity(&second));
        assert_ne!(identity(&first), identity(&independent));
        first.deactivate().expect("first retire");
        second.deactivate().expect("second retire");
        independent.deactivate().expect("independent retire");
    }

    #[test]
    fn explicit_frontend_deployment_has_distinct_scope_and_preserves_backend() {
        use bmux_plugin_sdk::definition::ComponentRole;
        let mut base = bcode_hello_plugin::plugin_definition();
        base.metadata.roles.push(ComponentRole::Frontend);
        let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = observed.clone();
        let definition = bmux_plugin_sdk::definition::PluginDefinition::new(
            base.metadata.clone(),
            move |scope: bmux_plugin_sdk::definition::PluginCreateContext| {
                captured
                    .lock()
                    .expect("component observations")
                    .push(scope.clone());
                base.create(scope)
            },
        );
        let runtime = bmux_plugin_runtime::PluginRuntimeScopeId::new();
        let manifest: crate::PluginManifest = toml::from_str(include_str!(
            "../../../examples/hello-plugin/bcode-plugin.toml"
        ))
        .expect("manifest");
        let backend = crate::load_plugin_component_with_config(
            manifest.clone(),
            definition.clone(),
            config(runtime),
            ComponentRole::Backend,
        )
        .expect("backend");
        let frontend = crate::load_plugin_component_with_config(
            manifest,
            definition,
            config(runtime),
            ComponentRole::Frontend,
        )
        .expect("frontend");
        backend.activate().expect("backend active");
        frontend.activate().expect("frontend active");
        assert!(
            backend.frontend_extensions().is_err(),
            "backend cannot impersonate frontend"
        );
        assert!(frontend.frontend_extensions().is_ok());
        let selected = frontend.frontend_extensions().expect("selected frontend");
        let activation = selected
            .activation_scope()
            .expect("actual activation authority");
        let resource = activation
            .register_resource(Arc::new("retained frontend state".to_string()))
            .expect("resource belongs to selected activation");
        let released = activation
            .release_resource(resource)
            .expect("selected activation release");
        assert!(released.downcast_ref::<String>().is_some());
        drop(released);
        {
            let observations = observed.lock().expect("component observations");
            assert_eq!(observations[0].role, ComponentRole::Backend);
            assert_eq!(observations[1].role, ComponentRole::Frontend);
            assert_eq!(observations[0].runtime_scope, observations[1].runtime_scope);
            assert_ne!(observations[0].instance, observations[1].instance);
        }
        frontend.deactivate().expect("frontend cleanup");
        assert_eq!(
            backend
                .invoke_service("example-hello/v1", "echo", vec![9])
                .expect("independent backend")
                .payload,
            vec![9]
        );
        backend.deactivate().expect("backend cleanup");
    }

    #[test]
    fn closed_shared_instance_rejects_product_work() {
        let plugin = hello();
        plugin.activate().expect("activation");
        plugin.deactivate().expect("cleanup");
        assert!(
            plugin
                .invoke_service("example-hello/v1", "echo", Vec::new())
                .is_err()
        );
    }
}
