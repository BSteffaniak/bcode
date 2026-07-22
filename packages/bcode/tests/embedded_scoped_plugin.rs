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
use bcode_tool::{
    ToolInvocationResponse, ToolPolicyMetadata, ToolPreparationRequest, ToolPreparationResponse,
    ToolSideEffect, ToolUiMetadata,
};
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
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: false,
        policy: ToolPolicyMetadata::default(),
        ui: ToolUiMetadata::default(),
    }
}

#[tokio::test]
async fn embedded_plugin_uses_same_scope_and_capabilities_as_direct_tools() {
    let bundled = [bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../examples/hello-plugin/bcode-plugin.toml"),
        bcode_hello_plugin::static_plugin(),
    )];
    let selected = bcode_plugin::filter_selected_static_plugins(
        &bundled,
        &bcode_plugin::PluginSelection::all_enabled(),
    )
    .expect("hello plugin manifest should parse");
    let plugins = bcode_plugin::PluginRuntimeHost::from(
        bcode_plugin::PluginHost::load_static_plugins(&selected)
            .expect("hello plugin should load statically"),
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

fn shell_definition() -> ToolDefinition {
    ToolDefinition {
        name: "shell.run".to_string(),
        description: "reentrant shell overlap conformance tool".to_string(),
        input_schema: serde_json::json!({"type": "object"}),
        side_effect: ToolSideEffect::ExecuteProcess,
        requires_permission: false,
        policy: ToolPolicyMetadata::default(),
        ui: ToolUiMetadata::default(),
    }
}

fn static_shell_runtime() -> bcode_plugin::PluginRuntimeHost {
    let bundled = [bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/shell-plugin/bcode-plugin.toml"),
        bcode_shell_plugin::static_plugin(),
    )];
    let selected = bcode_plugin::filter_selected_static_plugins(
        &bundled,
        &bcode_plugin::PluginSelection::all_enabled(),
    )
    .expect("shell plugin manifest should parse");
    bcode_plugin::PluginRuntimeHost::from(
        bcode_plugin::PluginHost::load_static_plugins(&selected)
            .expect("shell plugin should load statically"),
    )
}

fn dynamic_shell_runtime() -> bcode_plugin::PluginRuntimeHost {
    let executable = std::env::current_exe().expect("current test executable path");
    let directory = executable.parent().expect("test executable parent");
    let target_profile = directory
        .parent()
        .expect("test executable profile directory");
    let exact_library_name = format!(
        "{}bcode_shell_plugin{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    );
    let profile_library = target_profile.join(&exact_library_name);
    let library = if profile_library.is_file() {
        profile_library
    } else {
        let prefix = format!("{}bcode_shell_plugin", std::env::consts::DLL_PREFIX);
        std::fs::read_dir(directory)
            .expect("test dependency directory should be readable")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.starts_with(&prefix) && name.ends_with(std::env::consts::DLL_SUFFIX)
                    })
            })
            .expect("shell plugin dynamic library should be built before adapter conformance")
    };
    let root =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/shell-plugin");
    let mut registered = bcode_plugin::discover_plugins_in_roots(&[root])
        .expect("shell plugin manifest should be discovered");
    let plugin = registered
        .iter_mut()
        .find(|plugin| plugin.manifest.id == "bcode.shell")
        .expect("shell plugin should be registered");
    let bcode_plugin::PluginRuntime::Native(runtime) = &mut plugin.manifest.runtime;
    runtime.library = library;
    bcode_plugin::PluginRuntimeHost::from(
        bcode_plugin::PluginHost::load_registered_plugins(std::slice::from_ref(plugin))
            .expect("shell plugin should load dynamically"),
    )
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
                side_effect: ToolSideEffect::ReadOnly,
                requires_permission: false,
                policy: ToolPolicyMetadata::default(),
                ui: ToolUiMetadata::default(),
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
                side_effect: ToolSideEffect::ReadOnly,
                requires_permission: false,
                policy: ToolPolicyMetadata::default(),
                ui: ToolUiMetadata::default(),
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
    contributions: Mutex<Vec<bcode_tool::ToolContributionEvent>>,
    lifecycle: Mutex<Vec<bcode_tool::ToolInvocationLifecycleEvent>>,
}

impl TurnEventObservability for ContributionObserver {
    fn observe(&self, event: &bcode::ScopedTurnEvent) {
        match event {
            bcode::ScopedTurnEvent::Contribution(contribution) => self
                .contributions
                .lock()
                .expect("contribution observation lock")
                .push(contribution.clone()),
            bcode::ScopedTurnEvent::InvocationLifecycle(lifecycle) => self
                .lifecycle
                .lock()
                .expect("lifecycle observation lock")
                .push(lifecycle.clone()),
            bcode::ScopedTurnEvent::Runtime(_) => {}
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn static_and_dynamic_shell_contributions_are_observable_headlessly() {
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
                arguments: serde_json::json!({"command": "printf shell-contribution"}),
            })
            .await
            .expect("shell contribution invocation");
        assert!(!output.invocation.is_error, "{}", output.invocation.output);
        let contributions = observer
            .contributions
            .lock()
            .expect("contribution observations");
        assert_eq!(contributions.len(), 2);
        let request = contributions
            .iter()
            .find(|event| event.schema == "bcode.tool.request.shell.run")
            .expect("shell request contribution");
        assert_eq!(request.invocation_id, "shell-contribution");
        assert_eq!(request.producer_id, "bcode.shell");
        assert_eq!(
            request.persistence,
            bcode_tool::ToolContributionPersistence::Durable
        );
        let contribution = contributions
            .iter()
            .find(|event| event.schema == "bcode.shell.run.summary")
            .expect("shell summary contribution");
        assert_eq!(contribution.invocation_id, "shell-contribution");
        assert_eq!(contribution.producer_id, "bcode.shell");
        assert_eq!(contribution.schema, "bcode.shell.run.summary");
        assert_eq!(
            contribution.persistence,
            bcode_tool::ToolContributionPersistence::Durable
        );
        assert!(
            contribution
                .payload
                .to_string()
                .contains("shell-contribution")
        );
        drop(contributions);
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
async fn direct_static_dynamic_and_future_remote_adapters_share_scheduler_semantics() {
    assert_direct_batch_overlaps().await;
    assert_reentrant_shell_batch_overlaps(static_shell_runtime()).await;
    assert_reentrant_shell_batch_overlaps(dynamic_shell_runtime()).await;
    assert_future_remote_batch_semantics(FutureRemoteInvoker::concurrent(), 2).await;
    assert_future_remote_batch_semantics(FutureRemoteInvoker::non_reentrant(), 1).await;
}
