#![cfg(feature = "embedded-plugins")]

use bcode::{
    Agent, ArtifactCommitGuard, InvocationArtifactSink, InvocationCapabilityFuture,
    InvocationExchangeBroker, InvocationInputRouter, InvocationServiceRouter, PreparationScope,
    PreparedToolInvocation, RegisteredTool, ToolArtifactWriteRequest, ToolArtifactWriteResolution,
    ToolAuthorizationCoordinator, ToolAuthorizationDecision, ToolAuthorizationRequest, ToolCall,
    ToolDefinition, ToolExchangeRequest, ToolExchangeResolution, ToolInvocationInput,
    ToolInvocationInputResolution, ToolInvocationServiceRequest, ToolInvocationServiceResolution,
    ToolInvoker, TurnEventObservability,
};
use bcode_tool::{ToolInvocationResponse, ToolPreparationRequest, ToolPreparationResponse};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Debug, Default)]
struct Capabilities(Mutex<Vec<String>>);

impl Capabilities {
    fn record(&self, invocation_id: String) {
        self.0
            .lock()
            .expect("capability IDs lock")
            .push(invocation_id);
    }
}

impl InvocationExchangeBroker for Capabilities {
    fn request(
        &self,
        request: ToolExchangeRequest,
    ) -> InvocationCapabilityFuture<'_, ToolExchangeResolution> {
        self.record(request.invocation_id);
        Box::pin(async {
            ToolExchangeResolution::Responded {
                payload: serde_json::Value::Null,
            }
        })
    }
}

impl InvocationInputRouter for Capabilities {
    fn receive(
        &self,
        invocation_id: &str,
    ) -> InvocationCapabilityFuture<'_, ToolInvocationInputResolution> {
        let invocation_id = invocation_id.to_string();
        self.record(invocation_id.clone());
        Box::pin(async move {
            ToolInvocationInputResolution::Received {
                input: ToolInvocationInput {
                    invocation_id,
                    input_id: "input".to_string(),
                    producer_id: "test".to_string(),
                    schema: "test.input".to_string(),
                    schema_version: 1,
                    payload: serde_json::Value::Null,
                },
            }
        })
    }
}

impl InvocationServiceRouter for Capabilities {
    fn invoke(
        &self,
        request: ToolInvocationServiceRequest,
    ) -> InvocationCapabilityFuture<'_, ToolInvocationServiceResolution> {
        self.record(request.invocation_id);
        Box::pin(async {
            ToolInvocationServiceResolution::Responded {
                payload: serde_json::Value::Null,
            }
        })
    }
}

impl InvocationArtifactSink for Capabilities {
    fn write(
        &self,
        request: ToolArtifactWriteRequest,
        commit: ArtifactCommitGuard,
    ) -> InvocationCapabilityFuture<'_, ToolArtifactWriteResolution> {
        self.record(request.invocation_id);
        Box::pin(async move {
            commit
                .commit(|| ToolArtifactWriteResolution::Written {
                    artifact_id: "hello-artifact".to_string(),
                    byte_len: 5,
                    reference: serde_json::Value::Null,
                })
                .unwrap_or(ToolArtifactWriteResolution::Cancelled)
        })
    }
}

fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "hello_bridge".to_string(),
        description: "embedded bridge parity tool".to_string(),
        input_schema: serde_json::json!({"type": "object"}),
    }
}

#[tokio::test]
async fn embedded_plugin_uses_same_scope_and_capabilities_as_direct_tools() {
    let bundled = [bcode_plugin::StaticBundledPlugin::from_definition(
        include_str!("../../../examples/hello-plugin/bcode-plugin.toml"),
        bcode_hello_plugin::plugin_definition,
    )];
    let plugins = bcode_plugin::PluginRuntimeHost::from(
        bcode_plugin::PluginHost::load_discovered_with_static_bundled_and_config(
            &bcode_plugin::PluginSelection::all_enabled(),
            Vec::new(),
            &bundled,
            std::collections::BTreeMap::from([(
                "example.hello".into(),
                shared_config("example.hello"),
            )]),
        )
        .expect("hello shared definition should activate statically"),
    );
    let capabilities = Arc::new(Capabilities::default());
    let agent = Agent::builder()
        .plugin_runtime(plugins)
        .plugin_tool(definition(), "example.hello")
        .exchange_broker(capabilities.clone())
        .input_router(capabilities.clone())
        .service_router(capabilities.clone())
        .artifact_sink(capabilities.clone())
        .build();
    let call = ToolCall {
        id: "call-plugin".to_string(),
        name: "hello_bridge".to_string(),
        arguments: serde_json::Value::Null,
    };

    let output = agent
        .execute_tool_call(&call)
        .await
        .expect("embedded plugin tool should execute");

    assert_eq!(output.invocation.output, "call-plugin");
    assert_eq!(
        capabilities
            .0
            .lock()
            .expect("capability IDs lock")
            .as_slice(),
        ["call-plugin", "call-plugin", "call-plugin", "call-plugin"]
    );
}

#[derive(Debug)]
struct AllowAuthorization;

impl ToolAuthorizationCoordinator for AllowAuthorization {
    fn authorize_batch<'a>(
        &'a self,
        requests: &'a [ToolAuthorizationRequest],
        _scope: &'a bcode_agent_runtime::TurnScope,
    ) -> bcode::RuntimeFuture<'a, Vec<ToolAuthorizationDecision>> {
        Box::pin(async move {
            Ok(requests
                .iter()
                .map(|_| ToolAuthorizationDecision::Allow)
                .collect())
        })
    }
}

#[cfg(unix)]
fn shell_definition() -> ToolDefinition {
    ToolDefinition {
        name: "shell.run".to_string(),
        description: "reentrant shell overlap conformance tool".to_string(),
        input_schema: serde_json::json!({"type": "object"}),
    }
}

struct LifecycleDriver(Option<tokio::runtime::Runtime>);

impl bcode_plugin::SharedLifecycleDriver for LifecycleDriver {
    fn runtime_handle(&self) -> tokio::runtime::Handle {
        self.0
            .as_ref()
            .expect("fixture runtime retained")
            .handle()
            .clone()
    }

    fn run(&self, work: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>) {
        // Keep one serving runtime through activation, invocation and cleanup.
        // Never nest an executor on the async test thread.
        std::thread::scope(|scope| {
            scope
                .spawn(move || {
                    self.0
                        .as_ref()
                        .expect("fixture runtime retained")
                        .block_on(work);
                })
                .join()
                .expect("fixture lifecycle thread");
        });
    }
}

impl Drop for LifecycleDriver {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            // Tokio runtime teardown may block; the test task is not its owner.
            std::thread::scope(|scope| {
                scope
                    .spawn(move || drop(runtime))
                    .join()
                    .expect("fixture runtime teardown");
            });
        }
    }
}

struct RegistrationOwner;

impl bcode_plugin::SharedRegistrationOwner for RegistrationOwner {
    fn revoke_and_drain(
        &self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<(), bcode_plugin::PluginLoadError>> + Send + '_,
        >,
    > {
        // These definitions publish contributions through the modeled product
        // extension; this fixture admits no background registration producers.
        Box::pin(async { Ok(()) })
    }
}

struct ActivationDispatch;

fn activation_request(
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
        details: "embedded fixture denies unbound activation host request".into(),
    })
}

impl bmux_plugin_sdk::HostServiceDispatch for ActivationDispatch {
    fn invoke(
        &self,
        _: &bmux_plugin_runtime::InvocationScope,
        request: bmux_plugin_sdk::HostServiceRequest<'_>,
    ) -> bmux_plugin_sdk::Result<Vec<u8>> {
        activation_request(&request)
    }
}

fn shared_config(plugin_id: &str) -> bcode_plugin::ResolvedPluginConfig {
    let path = std::env::temp_dir().to_string_lossy().into_owned();
    let concurrency = match plugin_id {
        "bcode.filesystem" => bmux_plugin_runtime::PluginConcurrencyConfig::Limited {
            max: std::num::NonZeroUsize::new(4).expect("filesystem concurrency is nonzero"),
        },
        _ => bmux_plugin_runtime::PluginConcurrencyConfig::Concurrent,
    };
    bcode_plugin::ResolvedPluginConfig::default().with_shared_activation(
        bcode_plugin::SharedActivationContext {
            runtime: bmux_plugin_runtime::PluginRuntimeScopeId::new(),
            native: bmux_plugin_sdk::NativeLifecycleContext {
                plugin_id: plugin_id.into(),
                required_capabilities: Vec::new(),
                provided_capabilities: Vec::new(),
                services: Vec::new(),
                available_capabilities: Vec::new(),
                enabled_plugins: vec![plugin_id.into()],
                plugin_search_roots: Vec::new(),
                registered_plugins: Vec::new(),
                host: bmux_plugin_sdk::HostMetadata {
                    product_name: "bcode".into(),
                    product_version: "embedded-test".into(),
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
                auxiliary_executables: std::collections::BTreeMap::new(),
                auxiliary_executable_errors: std::collections::BTreeMap::new(),
            },
            services: bmux_plugin_sdk::definition::PluginHostServices::new(|request| {
                activation_request(&request)
            }),
            dispatch: Arc::new(ActivationDispatch),
            concurrency,
            progress_limits: bcode_plugin_sdk::TransientProgressLimits::default(),
            driver: Arc::new(LifecycleDriver(Some(
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("fixture serving runtime"),
            ))),
            registrations: Arc::new(RegistrationOwner),
        },
    )
}

#[cfg(unix)]
fn static_shell_runtime() -> bcode_plugin::PluginRuntimeHost {
    let bundled = [bcode_plugin::StaticBundledPlugin::from_definition(
        include_str!("../../../plugins/shell-plugin/bcode-plugin.toml"),
        bcode_shell_plugin::plugin_definition,
    )];
    bcode_plugin::PluginRuntimeHost::from(
        bcode_plugin::PluginHost::load_discovered_with_static_bundled_and_config(
            &bcode_plugin::PluginSelection::all_enabled(),
            Vec::new(),
            &bundled,
            std::collections::BTreeMap::from([(
                "bcode.shell".into(),
                shared_config("bcode.shell"),
            )]),
        )
        .expect("shell shared definition should activate statically"),
    )
}

#[cfg(unix)]
fn dynamic_shell_runtime() -> bcode_plugin::PluginRuntimeHost {
    dynamic_plugin_runtime("shell", "bcode.shell")
}

fn dynamic_plugin_runtime(domain: &str, plugin_id: &str) -> bcode_plugin::PluginRuntimeHost {
    let variable = format!("BCODE_{}_WRAPPER_ARTIFACT", domain.to_uppercase());
    let library = std::path::PathBuf::from(std::env::var_os(&variable).unwrap_or_else(|| {
        panic!("set {variable} from acceptance-artifacts.json generated by scripts/package-plugin-acceptance.py --build; implementation dylibs are not supported")
    }));
    assert!(
        library.is_absolute() && library.is_file(),
        "{variable} must select an existing absolute thin-wrapper artifact: {}",
        library.display(),
    );
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(format!("../../plugins/{domain}-plugin"));
    let registered = bcode_plugin::discover_plugins_in_roots(&[root])
        .expect("plugin manifest should be discovered");
    let plugin = registered
        .into_iter()
        .find(|plugin| plugin.manifest.id == plugin_id)
        .expect("plugin should be registered");
    // Deployment selection is separate from the exact embedded declaration.
    // Normal host admission owns activation, contributions and scheduling.
    bcode_plugin::PluginRuntimeHost::from(
        bcode_plugin::PluginHost::load_native_backend_components(vec![(
            plugin.manifest,
            library,
            shared_config(plugin_id),
        )])
        .expect("thin shared wrapper should activate through native backend admission"),
    )
}

#[tokio::test]
async fn embedded_filesystem_batch_prepares_and_commits_both_targets() {
    let directory = tempfile::tempdir().expect("workspace");
    let first = directory.path().join("first.txt");
    let second = directory.path().join("second.txt");
    std::fs::write(&first, "alpha\r\n").expect("first fixture");
    std::fs::write(&second, "beta\n").expect("second fixture");
    let agent = Agent::builder()
        .plugin_runtime(dynamic_plugin_runtime("filesystem", "bcode.filesystem"))
        .plugin_tool(
            ToolDefinition {
                name: "filesystem.multi_edit".to_owned(),
                description: "batch integration".to_owned(),
                input_schema: serde_json::json!({"type":"object"}),
            },
            "bcode.filesystem",
        )
        .authorization_coordinator(Arc::new(AllowAuthorization))
        .build();
    let output = agent
        .execute_tool_call(&ToolCall {
            id: "embedded-batch".to_owned(),
            name: "filesystem.multi_edit".to_owned(),
            arguments: serde_json::json!({"files":[
                {"path":first,"edits":[{"old_text":"alpha","new_text":"one"}]},
                {"path":second,"edits":[{"old_text":"beta","new_text":"two"}]}
            ]}),
        })
        .await
        .expect("batch invocation");
    assert!(!output.invocation.is_error, "{}", output.invocation.output);
    assert_eq!(std::fs::read(&first).unwrap(), b"one\r\n");
    assert_eq!(std::fs::read(&second).unwrap(), b"two\n");
    let outcome: serde_json::Value = serde_json::from_str(&output.invocation.output).unwrap();
    assert_eq!(outcome["files"][0]["status"], "committed");
    assert_eq!(outcome["files"][1]["status"], "committed");
    assert!(output.invocation.result.is_some());

    // Validation of a later target must precede publication of an earlier one.
    let rejected = agent
        .execute_tool_call(&ToolCall {
            id: "embedded-invalid-batch".to_owned(),
            name: "filesystem.multi_edit".to_owned(),
            arguments: serde_json::json!({"files":[
                {"path":first,"edits":[{"old_text":"one","new_text":"unexpected"}]},
                {"path":second,"edits":[{"old_text":"missing","new_text":"unexpected"}]}
            ]}),
        })
        .await
        .expect("validation failure is a tool outcome");
    assert!(rejected.invocation.is_error);
    assert_eq!(std::fs::read(&first).unwrap(), b"one\r\n");
    assert_eq!(std::fs::read(&second).unwrap(), b"two\n");
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
}

#[derive(Debug, Default)]
struct BoundedSnapshotSink(Mutex<std::collections::BTreeMap<String, Vec<u8>>>);

impl InvocationArtifactSink for BoundedSnapshotSink {
    fn write(
        &self,
        request: ToolArtifactWriteRequest,
        commit: ArtifactCommitGuard,
    ) -> InvocationCapabilityFuture<'_, ToolArtifactWriteResolution> {
        Box::pin(async move {
            if request.bytes.len() > 4096 {
                return ToolArtifactWriteResolution::TooLarge { max_bytes: 4096 };
            }
            commit
                .commit(|| {
                    let byte_len = u64::try_from(request.bytes.len()).unwrap();
                    let uri = format!("artifact://{}", request.artifact_id);
                    self.0.lock().unwrap().insert(uri.clone(), request.bytes);
                    ToolArtifactWriteResolution::Written {
                        artifact_id: request.artifact_id,
                        byte_len,
                        reference: serde_json::json!({"uri":uri}),
                    }
                })
                .unwrap_or(ToolArtifactWriteResolution::Cancelled)
        })
    }
}

#[tokio::test]
async fn embedded_filesystem_retains_oversized_sources_through_artifact_sink() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("large.txt");
    let before = format!("old{}", "界".repeat(30000));
    let after = before.replacen("old", "new", 1);
    std::fs::write(&path, &before).unwrap();
    let sink = Arc::new(BoundedSnapshotSink::default());
    let agent = Agent::builder()
        .plugin_runtime(dynamic_plugin_runtime("filesystem", "bcode.filesystem"))
        .plugin_tool(
            ToolDefinition {
                name: "filesystem.multi_edit".to_owned(),
                description: "retention integration".to_owned(),
                input_schema: serde_json::json!({"type":"object"}),
            },
            "bcode.filesystem",
        )
        .authorization_coordinator(Arc::new(AllowAuthorization))
        .artifact_sink(sink.clone())
        .build();
    let output = agent.execute_tool_call(&ToolCall {
        id: "retained-batch".to_owned(),
        name: "filesystem.multi_edit".to_owned(),
        arguments: serde_json::json!({"files":[{"path":path,"edits":[{"old_text":"old","new_text":"new"}]}]}),
    }).await.unwrap();
    assert!(!output.invocation.is_error, "{}", output.invocation.output);
    assert_eq!(std::fs::read(&path).unwrap(), after.as_bytes());
    let outcome: serde_json::Value = serde_json::from_str(&output.invocation.output).unwrap();
    let stored = sink.0.lock().unwrap();
    let diff = format!(
        "--- before\n+++ after\n@@ -1,1 +1,1 @@\n-{before}\n\\ No newline at end of file\n+{after}\n\\ No newline at end of file\n"
    );
    for (side, expected) in [("old", before), ("new", after), ("diff", diff)] {
        let source = &outcome["files"][0]["change"]["retained"][side];
        assert_eq!(source["version"], 1);
        let mut reconstructed = Vec::new();
        for part in source["parts"].as_array().expect("multipart source") {
            assert_eq!(part["offset"], reconstructed.len());
            let bytes = &stored[part["reference"]["uri"].as_str().unwrap()];
            assert_eq!(part["byte_len"], bytes.len());
            reconstructed.extend_from_slice(bytes);
        }
        assert_eq!(reconstructed, expected.as_bytes());
    }
}

#[tokio::test]
async fn embedded_filesystem_denial_prevents_mutation_and_retention() {
    let directory = tempfile::tempdir().unwrap();
    let first = directory.path().join("first.txt");
    let second = directory.path().join("second.txt");
    std::fs::write(&first, "one").unwrap();
    std::fs::write(&second, "two").unwrap();
    let sink = Arc::new(BoundedSnapshotSink::default());
    let agent = Agent::builder()
        .plugin_runtime(dynamic_plugin_runtime("filesystem", "bcode.filesystem"))
        .plugin_tool(
            ToolDefinition {
                name: "filesystem.multi_edit".to_owned(),
                description: "denial integration".to_owned(),
                input_schema: serde_json::json!({"type":"object"}),
            },
            "bcode.filesystem",
        )
        .cwd(directory.path().canonicalize().unwrap())
        .agent_config(bcode::AgentConfig {
            accent: None,
            tools: Default::default(),
            permission: bcode::PermissionConfig {
                edit: std::collections::BTreeMap::from([
                    ("**/first.txt".to_owned(), bcode::Action::Allow),
                    ("**/second.txt".to_owned(), bcode::Action::Deny),
                ]),
                ..Default::default()
            },
        })
        .artifact_sink(sink.clone())
        .build();
    let output = agent
        .execute_tool_call(&ToolCall {
            id: "denied-batch".to_owned(),
            name: "filesystem.multi_edit".to_owned(),
            arguments: serde_json::json!({"files":[
                {"path":first,"edits":[{"old_text":"one","new_text":"changed"}]},
                {"path":second,"edits":[{"old_text":"two","new_text":"changed"}]}
            ]}),
        })
        .await;
    assert!(output.is_err(), "denied invocation must fail");
    assert_eq!(std::fs::read(&first).unwrap(), b"one");
    assert_eq!(std::fs::read(&second).unwrap(), b"two");
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
    assert!(sink.0.lock().unwrap().is_empty());

    // The allowed target succeeds alone, so the batch rejection above is not a
    // blanket tool denial or an unrelated preparation failure.
    let allowed = agent
        .execute_tool_call(&ToolCall {
            id: "allowed-single-target".to_owned(),
            name: "filesystem.multi_edit".to_owned(),
            arguments: serde_json::json!({"files":[
                {"path":first,"edits":[{"old_text":"one","new_text":"changed"}]}
            ]}),
        })
        .await
        .expect("first target policy allows editing");
    assert!(
        !allowed.invocation.is_error,
        "{}",
        allowed.invocation.output
    );
    assert_eq!(std::fs::read(&first).unwrap(), b"changed");
    assert_eq!(std::fs::read(&second).unwrap(), b"two");
}

#[derive(Debug)]
struct CancelAuthorization;

impl ToolAuthorizationCoordinator for CancelAuthorization {
    fn authorize_batch<'a>(
        &'a self,
        requests: &'a [ToolAuthorizationRequest],
        scope: &'a bcode_agent_runtime::TurnScope,
    ) -> bcode::RuntimeFuture<'a, Vec<ToolAuthorizationDecision>> {
        Box::pin(async move {
            assert!(scope.control().begin_cancellation());
            Ok(requests
                .iter()
                .map(|_| ToolAuthorizationDecision::Allow)
                .collect())
        })
    }
}

#[tokio::test]
async fn embedded_filesystem_cancelled_admission_prevents_mutation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("unchanged.txt");
    std::fs::write(&path, "before").unwrap();
    let sink = Arc::new(BoundedSnapshotSink::default());
    let agent = Agent::builder()
        .plugin_runtime(dynamic_plugin_runtime("filesystem", "bcode.filesystem"))
        .plugin_tool(
            ToolDefinition {
                name: "filesystem.multi_edit".to_owned(),
                description: "cancelled admission integration".to_owned(),
                input_schema: serde_json::json!({"type":"object"}),
            },
            "bcode.filesystem",
        )
        .authorization_coordinator(Arc::new(CancelAuthorization))
        .artifact_sink(sink.clone())
        .build();
    let result = agent
        .execute_tool_call(&ToolCall {
            id: "cancelled-batch".to_owned(),
            name: "filesystem.multi_edit".to_owned(),
            arguments: serde_json::json!({"files":[
                {"path":path,"edits":[{"old_text":"before","new_text":"after"}]}
            ]}),
        })
        .await;
    assert!(result.is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"before");
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    assert!(sink.0.lock().unwrap().is_empty());
}

#[derive(Default)]
struct CancelAfterPublication(Mutex<Option<Arc<bcode_agent_runtime::TurnControl>>>);

impl ToolAuthorizationCoordinator for CancelAfterPublication {
    fn authorize_batch<'a>(
        &'a self,
        requests: &'a [ToolAuthorizationRequest],
        scope: &'a bcode_agent_runtime::TurnScope,
    ) -> bcode::RuntimeFuture<'a, Vec<ToolAuthorizationDecision>> {
        Box::pin(async move {
            *self.0.lock().unwrap() = Some(scope.control());
            Ok(requests
                .iter()
                .map(|_| ToolAuthorizationDecision::Allow)
                .collect())
        })
    }
}

impl InvocationArtifactSink for CancelAfterPublication {
    fn write(
        &self,
        _request: ToolArtifactWriteRequest,
        _commit: ArtifactCommitGuard,
    ) -> InvocationCapabilityFuture<'_, ToolArtifactWriteResolution> {
        Box::pin(async move {
            // Oversized change retention occurs only after publication. Cancel here,
            // not on a timer, so the test necessarily exercises an active invocation.
            self.0
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .begin_cancellation();
            ToolArtifactWriteResolution::Cancelled
        })
    }
}

#[tokio::test]
async fn embedded_filesystem_active_cancellation_preserves_committed_outcome() {
    let directory = tempfile::tempdir().unwrap();
    let first = directory.path().join("first.txt");
    let second = directory.path().join("second.txt");
    let before = format!("old{}", "界".repeat(30000));
    std::fs::write(&first, &before).unwrap();
    std::fs::write(&second, "untouched").unwrap();
    let cancellation = Arc::new(CancelAfterPublication::default());
    let observer = Arc::new(ContributionObserver::default());
    let agent = Agent::builder()
        .plugin_runtime(dynamic_plugin_runtime("filesystem", "bcode.filesystem"))
        .plugin_tool(
            ToolDefinition {
                name: "filesystem.multi_edit".to_owned(),
                description: "active cancellation integration".to_owned(),
                input_schema: serde_json::json!({"type":"object"}),
            },
            "bcode.filesystem",
        )
        .authorization_coordinator(cancellation.clone())
        .artifact_sink(cancellation)
        .event_observability(observer.clone())
        .build();
    let output = agent
        .execute_tool_call(&ToolCall {
            id: "active-cancellation".to_owned(),
            name: "filesystem.multi_edit".to_owned(),
            arguments: serde_json::json!({"files":[
                {"path":first,"edits":[{"old_text":"old","new_text":"new"}]},
                {"path":second,"edits":[{"old_text":"untouched","new_text":"unexpected"}]}
            ]}),
        })
        .await;
    assert!(matches!(
        output,
        Err(bcode::BcodeError::Runtime(
            bcode_agent_runtime::RuntimeError::Cancelled
        ))
    ));
    let events = observer.lifecycle.lock().unwrap();
    let terminal = events
        .last()
        .expect("client receives cancelled terminal outcome");
    assert_eq!(
        terminal.stage,
        bcode_tool::ToolInvocationLifecycleStage::Cancelled
    );
    assert!(!events.iter().any(|event| matches!(
        event.stage,
        bcode_tool::ToolInvocationLifecycleStage::Completed
            | bcode_tool::ToolInvocationLifecycleStage::Failed
    )));
    let report = terminal
        .message
        .as_ref()
        .unwrap()
        .strip_prefix("Tool cancelled; final invocation report: ")
        .unwrap();
    let outcome: serde_json::Value = serde_json::from_str(report).unwrap_or_else(|error| {
        let prefix: Vec<u8> = report.as_bytes().iter().copied().take(512).collect();
        panic!(
            "cancelled invocation must retain the tool JSON report, not transport framing or an error: {error}; report_bytes={}, prefix={prefix:?}, prefix_text={:?}",
            report.len(),
            String::from_utf8_lossy(&prefix),
        );
    });
    assert_eq!(outcome["files"][0]["status"], "committed");
    assert_eq!(outcome["files"][1]["status"], "cancelled");
    assert_eq!(
        std::fs::read_to_string(first).unwrap(),
        before.replacen("old", "new", 1)
    );
    assert_eq!(std::fs::read_to_string(second).unwrap(), "untouched");
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
}

async fn assert_direct_batch_overlaps() {
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let handler_barrier = Arc::clone(&barrier);
    let agent = Agent::builder()
        .scoped_inline_tool(
            ToolDefinition {
                name: "direct.overlap".to_string(),
                description: "direct overlap conformance tool".to_string(),
                input_schema: serde_json::json!({"type": "object"}),
            },
            move |invocation, _scope| {
                let barrier = Arc::clone(&handler_barrier);
                async move {
                    barrier.wait().await;
                    Ok(bcode::ToolInvocationResponse {
                        output: invocation.invocation_id,
                        is_error: false,
                        content: Vec::new(),
                        full_output: None,
                        result: None,
                    })
                }
            },
        )
        .authorization_coordinator(Arc::new(AllowAuthorization))
        .build();
    let calls = (0..2)
        .map(|index| ToolCall {
            id: format!("direct-overlap-{index}"),
            name: "direct.overlap".to_string(),
            arguments: serde_json::Value::Null,
        })
        .collect::<Vec<_>>();
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        agent.execute_tool_batch(&calls),
    )
    .await
    .expect("direct same-batch calls must overlap")
    .expect("direct batch should execute");
    assert!(output.results.iter().all(Result::is_ok));
}

#[derive(Debug)]
struct FutureRemoteInvoker {
    admission: Option<Arc<tokio::sync::Semaphore>>,
    active: AtomicUsize,
    maximum: AtomicUsize,
}

impl FutureRemoteInvoker {
    fn concurrent() -> Arc<Self> {
        Arc::new(Self {
            admission: None,
            active: AtomicUsize::new(0),
            maximum: AtomicUsize::new(0),
        })
    }

    fn non_reentrant() -> Arc<Self> {
        Arc::new(Self {
            admission: Some(Arc::new(tokio::sync::Semaphore::new(1))),
            active: AtomicUsize::new(0),
            maximum: AtomicUsize::new(0),
        })
    }

    fn maximum(&self) -> usize {
        self.maximum.load(Ordering::SeqCst)
    }
}

impl ToolInvoker for FutureRemoteInvoker {
    fn prepare_tool<'a>(
        &'a self,
        _tool: &'a RegisteredTool,
        _request: &'a ToolPreparationRequest,
        _scope: &'a PreparationScope,
    ) -> bcode::RuntimeFuture<'a, ToolPreparationResponse> {
        Box::pin(async { Ok(ToolPreparationResponse::default()) })
    }

    fn invoke_tool<'a>(
        &'a self,
        _tool: &'a RegisteredTool,
        invocation: &'a PreparedToolInvocation,
        _scope: &'a bcode::InvocationScope,
    ) -> bcode::RuntimeFuture<'a, ToolInvocationResponse> {
        Box::pin(async move {
            let _permit = match &self.admission {
                Some(admission) => Some(
                    admission
                        .acquire()
                        .await
                        .expect("remote admission semaphore remains open"),
                ),
                None => None,
            };
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.maximum.fetch_max(active, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(ToolInvocationResponse {
                output: invocation.invocation.invocation_id.clone(),
                is_error: false,
                content: Vec::new(),
                full_output: None,
                result: None,
            })
        })
    }
}

async fn assert_future_remote_batch_semantics(invoker: Arc<FutureRemoteInvoker>, maximum: usize) {
    let calls = [
        ToolCall {
            id: "remote-first".to_owned(),
            name: "future.remote".to_owned(),
            arguments: serde_json::Value::Null,
        },
        ToolCall {
            id: "remote-second".to_owned(),
            name: "future.remote".to_owned(),
            arguments: serde_json::Value::Null,
        },
    ];
    let agent = Agent::builder()
        .inline_tool(
            ToolDefinition {
                name: "future.remote".to_owned(),
                description: "future remote adapter conformance tool".to_owned(),
                input_schema: serde_json::json!({"type": "object"}),
            },
            |_| unreachable!("future remote invoker owns execution"),
        )
        .tool_invoker(invoker.clone())
        .authorization_coordinator(Arc::new(AllowAuthorization))
        .build();
    let output = agent
        .execute_tool_batch(&calls)
        .await
        .expect("future remote batch should execute");
    assert_eq!(invoker.maximum(), maximum);
    assert_eq!(
        output
            .results
            .into_iter()
            .map(|result| result.expect("remote result").invocation.output)
            .collect::<Vec<_>>(),
        ["remote-first", "remote-second"]
    );
}

#[cfg(unix)]
async fn assert_reentrant_shell_batch_overlaps(plugins: bcode_plugin::PluginRuntimeHost) {
    let workspace = tempfile::tempdir().expect("shell overlap workspace");
    let calls = (0..2)
        .map(|index| ToolCall {
            id: format!("shell-overlap-{index}"),
            name: "shell.run".to_string(),
            arguments: serde_json::json!({
                "command": format!(
                    "touch .overlap-{index}; while [ \"$(find . -maxdepth 1 -name '.overlap-*' | wc -l | tr -d ' ')\" -lt 2 ]; do sleep 0.02; done"
                ),
                "cwd": workspace.path(),
                "timeout_ms": 2_000
            }),
        })
        .collect::<Vec<_>>();
    let agent = Agent::builder()
        .plugin_runtime(plugins)
        .plugin_tool(shell_definition(), "bcode.shell")
        .authorization_coordinator(Arc::new(AllowAuthorization))
        .build();
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        agent.execute_tool_batch(&calls),
    )
    .await
    .expect("reentrant shell batch must overlap")
    .expect("shell batch should execute");

    assert!(
        output.results.iter().all(Result::is_ok),
        "shell batch failed: {:?}",
        output.results
    );
    assert!((0..2).all(|index| workspace.path().join(format!(".overlap-{index}")).exists()));
}

#[derive(Debug, Default)]
struct ContributionObserver {
    updates: Mutex<Vec<bcode_tool::ToolPresentationUpdate>>,
    lifecycle: Mutex<Vec<bcode_tool::ToolInvocationLifecycleEvent>>,
}

impl TurnEventObservability for ContributionObserver {
    fn observe(&self, event: &bcode::ScopedTurnEvent) {
        match event {
            bcode::ScopedTurnEvent::PresentationUpdate(update) => self
                .updates
                .lock()
                .expect("presentation observation lock")
                .push(update.clone()),
            bcode::ScopedTurnEvent::InvocationLifecycle(lifecycle) => self
                .lifecycle
                .lock()
                .expect("lifecycle observation lock")
                .push(lifecycle.clone()),
            bcode::ScopedTurnEvent::Runtime(_) | bcode::ScopedTurnEvent::Contribution(_) => {}
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn static_and_dynamic_shell_contributions_are_observable_headlessly() {
    let workspace = tempfile::tempdir().expect("shell workspace");
    for plugins in [static_shell_runtime(), dynamic_shell_runtime()] {
        let observer = Arc::new(ContributionObserver::default());
        let agent = Agent::builder()
            .plugin_runtime(plugins)
            .plugin_tool(shell_definition(), "bcode.shell")
            .authorization_coordinator(Arc::new(AllowAuthorization))
            .event_observability(observer.clone())
            .build();
        let output = agent
            .execute_tool_call(&ToolCall {
                id: "shell-contribution".to_owned(),
                name: "shell.run".to_owned(),
                arguments: serde_json::json!({"command": "printf shell-contribution", "cwd": workspace.path()}),
            })
            .await
            .expect("shell contribution invocation");
        assert!(!output.invocation.is_error, "{}", output.invocation.output);
        let updates = observer.updates.lock().expect("presentation observations");
        let request = updates.first().expect("shell request presentation");
        assert_eq!(request.schema, "bcode.tool.request.shell.run");
        assert_eq!(request.revision, 1);
        // This SDK path supplies workspace context but no artifact root, so shell
        // publishes its request and then replaces the same primary with producer-owned
        // semantic output snapshots, never recording-artifact revisions.
        let semantic = &updates[1..];
        assert!(
            !semantic.is_empty(),
            "shell should publish semantic output after its request: {updates:?}"
        );
        assert!(
            semantic
                .iter()
                .all(|update| update.schema == "bcode.shell.output" && update.artifact.is_none())
        );
        assert!(
            updates
                .windows(2)
                .all(|pair| pair[0].revision < pair[1].revision)
        );
        let latest = semantic.last().expect("latest semantic output");
        assert_eq!(latest.payload["command"], "printf shell-contribution");
        assert!(
            latest.payload["output"]
                .as_str()
                .is_some_and(|output| output.contains("shell-contribution")),
            "latest semantic output should carry command output: {}",
            latest.payload
        );
        assert!(updates.iter().all(|update| {
            update.invocation_id == "shell-contribution"
                && update.producer_id == "bcode.shell"
                && update.generation == 0
                && update.identity == bcode_tool::ToolPresentationIdentity::Primary
                && update.retention == bcode_tool::ToolPresentationRetention::RetainLatest
        }));
        assert!(request.artifact.is_none());
        assert!(request.payload.to_string().contains("shell-contribution"));
        drop(updates);
        let lifecycle = observer.lifecycle.lock().expect("lifecycle observations");
        assert!(lifecycle.iter().any(|event| {
            event.invocation_id == "shell-contribution"
                && event.stage == bcode_tool::ToolInvocationLifecycleStage::Progress
                && event
                    .message
                    .as_deref()
                    .is_some_and(|message| message.contains("starting command"))
        }));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn explicit_artifact_root_enables_shell_recording_updates() {
    for plugins in [static_shell_runtime(), dynamic_shell_runtime()] {
        let artifacts = tempfile::tempdir().expect("artifact root");
        let root = artifacts
            .path()
            .canonicalize()
            .expect("absolute artifact root");
        let observer = Arc::new(ContributionObserver::default());
        let sdk = bcode::Bcode::builder()
            .plugin_runtime(plugins)
            .tool_artifact_root(&root)
            .build();
        let agent = sdk
            .agent_from_context(bcode::SessionId::new(), root.clone())
            .plugin_tool(shell_definition(), "bcode.shell")
            .authorization_coordinator(Arc::new(AllowAuthorization))
            .event_observability(observer.clone())
            .build();
        let output = agent
            .execute_tool_call(&ToolCall {
                id: "recording-context".to_owned(),
                name: "shell.run".to_owned(),
                arguments: serde_json::json!({"command": "printf recording-context"}),
            })
            .await
            .expect("shell invocation");
        assert!(!output.invocation.is_error, "{}", output.invocation.output);
        let updates = observer.updates.lock().expect("updates");
        assert!(updates.len() >= 2);
        assert!(
            updates
                .windows(2)
                .all(|pair| pair[0].revision < pair[1].revision)
        );
        let artifact = updates
            .last()
            .and_then(|update| update.artifact.as_ref())
            .expect("final recording artifact");
        assert!(artifact.finalized);
        assert!(artifact.committed_bytes > 0);
    }
}

#[tokio::test]
async fn direct_and_future_remote_adapters_share_scheduler_semantics() {
    assert_direct_batch_overlaps().await;
    assert_future_remote_batch_semantics(FutureRemoteInvoker::concurrent(), 2).await;
    assert_future_remote_batch_semantics(FutureRemoteInvoker::non_reentrant(), 1).await;
}

/// The overlap probe is a POSIX shell script.
#[cfg(unix)]
#[tokio::test]
async fn static_and_dynamic_shell_adapters_share_scheduler_semantics() {
    assert_reentrant_shell_batch_overlaps(static_shell_runtime()).await;
    assert_reentrant_shell_batch_overlaps(dynamic_shell_runtime()).await;
}
