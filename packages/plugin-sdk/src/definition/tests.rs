use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct Counter(AtomicUsize);
impl RustPlugin for Counter {}
impl ConcurrentRustPlugin for Counter {
    fn invoke_service_concurrent(&self, context: NativeServiceContext) -> ServiceResponse {
        context.events.emit(&context.request.payload);
        ServiceResponse::ok(self.0.fetch_add(1, Ordering::SeqCst).to_le_bytes().to_vec())
    }
}

fn metadata() -> PluginMetadata {
    PluginMetadata {
        plugin_id: "bcode.adapter-test".to_owned(),
        version: "1.0.0".to_owned(),
        roles: vec![ComponentRole::Backend],
        required_capabilities: Vec::new(),
        contracts: Vec::new(),
        manifest_toml: None,
    }
}
fn scope(instance: &str) -> PluginCreateContext {
    serde_json::from_value(serde_json::json!({
        "runtime_scope": "00000000-0000-0000-0000-000000000001",
        "instance": instance,
        "role": "backend"
    }))
    .unwrap()
}
fn context() -> NativeServiceContext {
    NativeServiceContext {
        diagnostics: None,
        plugin_id: "bcode.adapter-test".to_owned(),
        request: crate::ServiceRequest {
            interface_id: "bcode.adapter-counter/v1".to_owned(),
            operation: "next".to_owned(),
            payload: vec![0, 128, 255],
        },
        config: PluginConfigContext::default(),
        events: ServiceEventEmitter::default(),
        cancellation: ServiceCancellation::default(),
        bridge: crate::ServiceBridge::default(),
        transient_progress_limits: TransientProgressLimits::default(),
    }
}
fn provider(instance: &dyn PluginInstance) -> Arc<dyn BcodePluginService> {
    let mut registry = TypedServiceRegistry::default();
    instance.register_typed_services(&mut registry).unwrap();
    registry
        .get(
            &bmux_plugin_sdk::HostScope::new("bcode.product").unwrap(),
            bmux_plugin_sdk::ServiceKind::Command,
            PRODUCT_SERVICE_INTERFACE,
        )
        .unwrap()
        .provider_as_trait::<dyn BcodePluginService>()
        .unwrap()
}

#[test]
fn shared_factory_creates_independent_product_state() {
    let definition = concurrent_plugin_definition::<Counter>(metadata());
    let a = definition
        .create(scope("00000000-0000-0000-0000-000000000002"))
        .unwrap();
    let b = definition
        .create(scope("00000000-0000-0000-0000-000000000003"))
        .unwrap();
    let a = provider(a.as_ref());
    let b = provider(b.as_ref());
    assert_eq!(a.invoke(context()).payload, 0_usize.to_le_bytes());
    assert_eq!(a.invoke(context()).payload, 1_usize.to_le_bytes());
    assert_eq!(b.invoke(context()).payload, 0_usize.to_le_bytes());
}

#[test]
fn exclusive_reentry_fails_instead_of_waiting_on_parent_mutex() {
    let serial = Serial(Mutex::new(Counter::default()));
    let response = serial.with(|_| serial.invoke(context())).unwrap();
    assert_eq!(response.error.unwrap().code, "plugin_busy");
    // The rejected nested entry leaves the implementation reachable afterwards.
    assert_eq!(
        serial.invoke(context()).error.unwrap().code,
        "unsupported_service"
    );
}

#[test]
fn exclusive_independent_callers_wait_for_the_running_call() {
    let serial = Arc::new(Serial(Mutex::new(Counter::default())));
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let running = {
        let serial = Arc::clone(&serial);
        std::thread::spawn(move || {
            serial
                .with(|_| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                })
                .unwrap();
        })
    };
    entered_rx.recv().unwrap();
    let waiting = {
        let serial = Arc::clone(&serial);
        std::thread::spawn(move || serial.invoke(context()))
    };
    release_tx.send(()).unwrap();
    running.join().unwrap();
    // The queued caller reaches the implementation instead of reporting busy.
    let response = waiting.join().unwrap();
    assert_eq!(response.error.unwrap().code, "unsupported_service");
}

#[test]
fn shared_cancellation_remains_live_and_wait_observes_remote_cancel() {
    let shared = bmux_plugin_sdk::ServiceCancellation::new();
    let product = ServiceCancellation::from_shared(shared.clone());
    assert!(!product.is_cancelled());
    shared.cancel();
    assert!(product.wait_cancelled(std::time::Duration::from_secs(1)));
    let shared = bmux_plugin_sdk::ServiceCancellation::new();
    ServiceCancellation::from_shared(shared.clone()).cancel();
    assert!(shared.is_cancelled());
}

#[derive(Default)]
struct Events(Mutex<Vec<bmux_plugin_sdk::ServiceEvent>>);
impl bmux_plugin_sdk::ServiceEventSink for Events {
    fn emit_event(&self, event: bmux_plugin_sdk::ServiceEvent) -> bmux_plugin_sdk::Result<()> {
        self.0.lock().unwrap().push(event);
        Ok(())
    }
}

#[test]
fn streaming_adapter_retains_sink_and_exact_invocation_identity() {
    let sink = Arc::new(Events::default());
    let invocation = bmux_plugin_sdk::PluginInvocationId::new();
    let emitter = ServiceEventEmitter::from_shared(
        invocation.clone(),
        bmux_plugin_sdk::ServiceEventSinkHandle::from_arc(sink.clone()),
    );
    let cloned = emitter.clone();
    emitter.emit(&[0, 255]);
    drop(emitter);
    cloned.emit(&[128]);
    let events = sink.0.lock().unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].invocation_id, invocation);
    assert_eq!(events[0].payload, vec![0, 255]);
    assert_eq!(events[1].payload, vec![128]);
}

#[test]
fn portable_dispatch_without_bound_product_host_fails_closed() {
    let call = PluginCallContext {
        invocation_id: bmux_plugin_sdk::PluginInvocationId::new(),
        cancellation: bmux_plugin_sdk::ServiceCancellation::new(),
        host_services: None,
        callback_execution: None,
    };
    assert!(SharedInvocationHost.resolve(&call).is_err());
}

#[derive(Debug)]
struct RejectEvents;
impl bmux_plugin_sdk::ServiceEventSink for RejectEvents {
    fn emit_event(&self, _event: bmux_plugin_sdk::ServiceEvent) -> bmux_plugin_sdk::Result<()> {
        Err(bmux_plugin_sdk::PluginError::ServiceProtocol {
            details: "closed".to_owned(),
        })
    }
}

#[test]
fn rejected_event_does_not_advance_publisher_authority() {
    let emitter = ServiceEventEmitter::from_shared(
        bmux_plugin_sdk::PluginInvocationId::new(),
        bmux_plugin_sdk::ServiceEventSinkHandle::new(RejectEvents),
    );
    let mut progress = crate::TransientProgressPublisher::new(
        emitter.clone(),
        "call",
        "progress",
        "bcode.adapter-test",
        "test",
        1,
    );
    assert!(matches!(
        progress.upsert(&serde_json::json!({"step": 1})),
        Err(crate::TransientProgressError::EventRejected(_))
    ));
    assert_eq!(progress.sequence(), 0);
    let mut primary = crate::PrimaryPresentationPublisher::with_limits_and_cancellation(
        emitter,
        "call",
        "bcode.adapter-test",
        "test",
        1,
        bcode_tool::ToolPresentationRetention::ActiveOnly,
        TransientProgressLimits::default(),
        ServiceCancellation::default(),
    );
    assert!(matches!(
        primary.replace(&serde_json::json!({"step": 1})),
        Err(crate::PresentationUpdateError::EventRejected(_))
    ));
    assert_eq!(primary.revision(), 0);
}

#[derive(Debug, Default)]
struct HostCalls(AtomicUsize);
impl crate::ProductHostBridge for HostCalls {
    fn request(
        &self,
        _request: &crate::ServiceBridgeRequest,
    ) -> Result<crate::ServiceBridgeResponse, crate::ServiceBridgeError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(crate::ServiceBridgeResponse::Input(
            bcode_tool::ToolInvocationInputResolution::Closed,
        ))
    }
}

#[test]
fn owned_bridge_checks_cancellation_and_invokes_host_once() {
    let calls = Arc::new(HostCalls::default());
    let cancellation = ServiceCancellation::default();
    let bridge = crate::ServiceBridge::from_owned(calls.clone(), cancellation.clone());
    let request = crate::ServiceBridgeRequest::ReceiveInput {
        invocation_id: "call".to_owned(),
        timeout_ms: Some(0),
    };
    assert!(bridge.request(&request).is_ok());
    assert_eq!(calls.0.load(Ordering::SeqCst), 1);
    cancellation.cancel();
    assert_eq!(
        bridge.request(&request),
        Err(crate::ServiceBridgeError::Cancelled)
    );
    assert_eq!(calls.0.load(Ordering::SeqCst), 1);
}

#[derive(Debug, Default)]
struct LargeHostReply(AtomicUsize);
impl crate::ProductHostBridge for LargeHostReply {
    fn request(
        &self,
        _request: &crate::ServiceBridgeRequest,
    ) -> Result<crate::ServiceBridgeResponse, crate::ServiceBridgeError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(crate::ServiceBridgeResponse::Input(
            bcode_tool::ToolInvocationInputResolution::Failed {
                code: "large".to_owned(),
                message: "x".repeat(crate::SERVICE_BRIDGE_MAX_RESPONSE_BYTES + 1),
            },
        ))
    }
}

#[test]
fn oversized_owned_response_does_not_reexecute_effects() {
    let calls = Arc::new(LargeHostReply::default());
    let bridge = crate::ServiceBridge::from_owned(calls.clone(), ServiceCancellation::default());
    let request = crate::ServiceBridgeRequest::ReceiveInput {
        invocation_id: "call".to_owned(),
        timeout_ms: Some(0),
    };
    assert!(matches!(
        bridge.request(&request),
        Err(crate::ServiceBridgeError::ResponseTooLarge { .. })
    ));
    assert_eq!(calls.0.load(Ordering::SeqCst), 1);
}

#[test]
#[allow(clippy::result_large_err)] // Shared SDK host callback fixes its external error type.
fn injected_shared_host_resolves_context_and_preserves_live_ports() {
    let services = bmux_plugin_sdk::definition::PluginHostServices::new(|request| {
        assert_eq!(request.interface, PRODUCT_HOST_INTERFACE);
        if request.operation == "invocation_context" {
            assert_eq!(request.payload, &[] as &[u8]);
            return Ok(serde_json::to_vec(&ProductInvocationSettings {
                config: PluginConfigContext::default(),
                transient_progress_limits: TransientProgressLimits::default(),
            })
            .unwrap());
        }
        assert_eq!(request.operation, "receive_input");
        Ok(serde_json::to_vec(&crate::ServiceBridgeResponse::Input(
            bcode_tool::ToolInvocationInputResolution::Closed,
        ))
        .unwrap())
    });
    let call = PluginCallContext {
        invocation_id: bmux_plugin_sdk::PluginInvocationId::new(),
        cancellation: bmux_plugin_sdk::ServiceCancellation::new(),
        host_services: Some(services),
        callback_execution: None,
    };
    let resolved = SharedInvocationHost.resolve(&call).unwrap();
    assert!(
        resolved
            .bridge
            .request(&crate::ServiceBridgeRequest::ReceiveInput {
                invocation_id: "caller-value-not-authority".to_owned(),
                timeout_ms: Some(0),
            })
            .is_ok()
    );
    call.cancellation.cancel();
    assert_eq!(
        resolved
            .bridge
            .request(&crate::ServiceBridgeRequest::ReceiveInput {
                invocation_id: "caller-value-not-authority".to_owned(),
                timeout_ms: Some(0),
            }),
        Err(crate::ServiceBridgeError::Cancelled)
    );
}

fn cli() -> crate::StaticCliRegistration {
    crate::StaticCliRegistration {
        command: || clap::Command::new("example"),
        requires_daemon: |_| false,
        invoke: |_| Box::pin(async { Ok(crate::StaticCliOutcome::default()) }),
    }
}

#[test]
fn actual_cli_registration_is_component_scoped() {
    let mut metadata = metadata();
    metadata.roles.push(ComponentRole::Frontend);
    let definition = concurrent_plugin_definition_with_registrations::<Counter>(
        metadata,
        ProductRegistrations::default().with_cli(cli()),
        |_| Arc::new(SharedInvocationHost),
    );
    let backend = definition
        .create(scope("00000000-0000-0000-0000-000000000002"))
        .unwrap();
    assert!(provider(backend.as_ref()).cli_registration().is_none());
    let mut frontend_scope = scope("00000000-0000-0000-0000-000000000003");
    frontend_scope.role = ComponentRole::Frontend;
    let frontend = definition.create(frontend_scope).unwrap();
    let frontend = provider(frontend.as_ref());
    assert!(frontend.cli_registration().is_some());
    assert_eq!(
        frontend.invoke(context()).error.unwrap().code,
        "wrong_component"
    );
}

#[test]
fn attached_cli_discovery_and_classification_use_author_parser() {
    let instance = ProductInstance {
        scope: scope("00000000-0000-0000-0000-000000000009"),
        implementation: Arc::new(Concurrent(Counter::default())),
        plugin_id: "bcode.adapter-test".to_owned(),
        role: ComponentRole::Frontend,
        registrations: ProductRegistrations::default().with_cli(cli()),
        contributions: Mutex::new(None),
        host: Arc::new(SharedInvocationHost),
        extensions: std::collections::BTreeMap::new(),
        composer_registry: None,
        interaction_resources: Arc::new(resources::InteractionResources::new(
            None,
            resources::InteractionResourcePolicy::default(),
        )),
    };
    let call = || PluginCallContext {
        invocation_id: bmux_plugin_sdk::PluginInvocationId::new(),
        cancellation: bmux_plugin_sdk::ServiceCancellation::new(),
        host_services: None,
        callback_execution: None,
    };
    let mut describe = instance.extension(
        call(),
        bcode_command::cli::CLI_DESCRIBE_CONTRACT.to_owned(),
        Vec::new(),
    );
    let mut task = std::task::Context::from_waker(std::task::Waker::noop());
    let std::task::Poll::Ready(result) = describe.as_mut().poll(&mut task) else {
        panic!("inert discovery must be ready");
    };
    let descriptor: Option<bcode_command::cli::CliDescriptor> =
        serde_json::from_slice(&result.unwrap()).unwrap();
    assert_eq!(descriptor.unwrap().name, "example");
    let payload = serde_json::to_vec(&bcode_command::cli::CliArgvRequest {
        arguments: Vec::new(),
    })
    .unwrap();
    let mut classify = instance.extension(
        call(),
        bcode_command::cli::CLI_CLASSIFY_CONTRACT.to_owned(),
        payload.clone(),
    );
    let std::task::Poll::Ready(result) = classify.as_mut().poll(&mut task) else {
        panic!("classification must be ready");
    };
    let classification: bcode_command::cli::CliClassification =
        serde_json::from_slice(&result.unwrap()).unwrap();
    assert!(!classification.requires_daemon);
    // The legacy print handler must never be substituted for structured output.
    let mut invoke = instance.extension(
        call(),
        bcode_command::cli::CLI_INVOKE_CONTRACT.to_owned(),
        payload,
    );
    let std::task::Poll::Ready(result) = invoke.as_mut().poll(&mut task) else {
        panic!("missing handler must fail immediately");
    };
    assert!(result.is_err());
    let contracts = instance.registration_contracts().unwrap();
    assert!(
        contracts
            .iter()
            .any(|contract| contract.interface == bcode_command::cli::CLI_DESCRIBE_CONTRACT)
    );
    assert!(
        !contracts
            .iter()
            .any(|contract| contract.interface == bcode_command::cli::CLI_INVOKE_CONTRACT)
    );
}

mod manifest_serial {
    use super::*;
    #[derive(Default)]
    pub struct Author;
    impl RustPlugin for Author {
        fn component_registrations() -> ProductRegistrations {
            ProductRegistrations::default().with_cli(super::cli())
        }
    }
    crate::define_plugin!(serial Author, r#"
id = "bcode.manifest-serial"
version = "1.2.3"
[[services]]
interface_id = "bcode.example/v2"
class = "service"
"#);
}
mod manifest_concurrent {
    use super::*;
    crate::define_plugin!(concurrent Counter, r#"
id = "bcode.manifest-concurrent"
version = "2.0.0"
[[services]]
interface_id = "bcode.example/v1"
"#);
}

#[test]
fn manifest_macro_derives_metadata_and_keeps_registration_hook() {
    let definition = manifest_serial::plugin_definition();
    assert_eq!(definition.metadata.plugin_id, "bcode.manifest-serial");
    assert_eq!(definition.metadata.version, "1.2.3");
    assert!(
        definition
            .metadata
            .contracts
            .iter()
            .any(|contract| contract.interface_id == "bcode.example/v2" && contract.major == 2)
    );
    assert!(definition.metadata.roles.contains(&ComponentRole::Frontend));
    let mut frontend = scope("00000000-0000-0000-0000-000000000004");
    frontend.role = ComponentRole::Frontend;
    let instance = definition.create(frontend).unwrap();
    assert!(provider(instance.as_ref()).cli_registration().is_some());
    let concurrent = manifest_concurrent::plugin_definition();
    assert_eq!(concurrent.metadata.plugin_id, "bcode.manifest-concurrent");
    assert!(
        concurrent
            .create(scope("00000000-0000-0000-0000-000000000005"))
            .is_ok()
    );
}

#[cfg(feature = "tui")]
#[test]
fn frontend_factory_automatically_installs_scoped_portable_extension() {
    let registrations =
        ProductRegistrations::default().with_frontend(crate::tui::PluginTuiRegistry::default);
    let mut metadata = metadata();
    metadata.roles.push(ComponentRole::Frontend);
    let definition = concurrent_plugin_definition_with_registrations::<Counter>(
        metadata,
        registrations.clone(),
        |_| Arc::new(SharedInvocationHost),
    );
    let backend = scope("00000000-0000-0000-0000-000000000010");
    assert!(
        !registrations
            .instance_extensions(&backend)
            .contains_key(crate::tui::portable::TUI_FRONTEND_CONTRACT)
    );
    let backend_contracts = definition
        .create(backend.clone())
        .unwrap()
        .registration_contracts()
        .unwrap();
    assert!(!backend_contracts.iter().any(|entry| entry.interface
        == crate::tui::portable::TUI_FRONTEND_CONTRACT
        || entry.interface == bmux_tui::presentation::portable::PORTABLE_PRESENTATION_CONTRACT_ID));
    let mut frontend = backend;
    frontend.role = ComponentRole::Frontend;
    let a = registrations.instance_extensions(&frontend);
    let b = registrations.instance_extensions(&frontend);
    assert!(!Arc::ptr_eq(
        &a[crate::tui::portable::TUI_FRONTEND_CONTRACT],
        &b[crate::tui::portable::TUI_FRONTEND_CONTRACT]
    ));
    let instance = definition.create(frontend).unwrap();
    let contracts = instance.registration_contracts().unwrap();
    assert!(
        !contracts
            .iter()
            .any(|entry| entry.interface == "bcode.tui.frontend/v1")
    );
    assert_eq!(
        contracts
            .iter()
            .filter(|entry| entry.interface == crate::tui::portable::TUI_FRONTEND_CONTRACT)
            .count(),
        1
    );
    assert!(contracts.iter().any(|entry| entry.interface
        == crate::tui::portable::TUI_FRONTEND_CONTRACT
        && entry.version == crate::tui::portable::TUI_FRONTEND_VERSION));
    assert!(contracts.iter().any(|entry| entry.interface
        == bmux_tui::presentation::portable::PORTABLE_PRESENTATION_CONTRACT_ID
        && entry.version == bmux_tui::presentation::portable::PORTABLE_PRESENTATION_VERSION));
}

#[cfg(feature = "tui")]
#[test]
fn selected_descriptor_advertises_frontend_contract_from_declared_factory() {
    // Registration discovery must not depend on populating the runtime extension
    // map first. This is the descriptor queried by actual native selection.
    let instance = ProductInstance {
        scope: scope("00000000-0000-0000-0000-000000000011"),
        implementation: Arc::new(Concurrent(Counter::default())),
        plugin_id: "bcode.adapter-test".to_owned(),
        role: ComponentRole::Frontend,
        registrations: ProductRegistrations::default()
            .with_frontend(crate::tui::PluginTuiRegistry::default),
        contributions: Mutex::new(None),
        host: Arc::new(SharedInvocationHost),
        extensions: std::collections::BTreeMap::new(),
        composer_registry: None,
        interaction_resources: Arc::new(resources::InteractionResources::new(
            None,
            resources::InteractionResourcePolicy::default(),
        )),
    };
    let call = PluginCallContext {
        invocation_id: bmux_plugin_sdk::PluginInvocationId::new(),
        cancellation: bmux_plugin_sdk::ServiceCancellation::new(),
        host_services: None,
        callback_execution: None,
    };
    let mut descriptor = instance.extension(
        call,
        extensions::PRODUCT_EXTENSIONS_CONTRACT.to_owned(),
        Vec::new(),
    );
    let mut task = std::task::Context::from_waker(std::task::Waker::noop());
    let std::task::Poll::Ready(reply) = descriptor.as_mut().poll(&mut task) else {
        panic!("inert descriptor must be ready");
    };
    let reply: ProductExtensionDescriptor = serde_json::from_slice(&reply.unwrap()).unwrap();
    assert!(reply.contracts.iter().any(|entry| entry.interface
        == crate::tui::portable::TUI_FRONTEND_CONTRACT
        && entry.version == crate::tui::portable::TUI_FRONTEND_VERSION));
    assert!(reply.contracts.iter().any(|entry| entry.interface
        == bmux_tui::presentation::portable::PORTABLE_PRESENTATION_CONTRACT_ID
        && entry.version == bmux_tui::presentation::portable::PORTABLE_PRESENTATION_VERSION));
}

#[test]
fn contribution_collectors_accept_empty_snapshot() {
    let collected = Arc::new(Mutex::new(Vec::new()));
    let registrar = crate::CommandRegistrar::collect(collected.clone());
    assert!(registrar.is_available());
    // Empty collection is a real successful snapshot, not a missing callback.
    assert_eq!(encode_collected(&collected).unwrap(), Vec::<Vec<u8>>::new());
}
