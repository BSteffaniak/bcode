//! Host boundary behavior; no providers, native programs or SQL fixtures.
mod native_observation;
mod progress;
use super::*;
use bcode_harness_models::*;
use bcode_plugin_sdk::prelude::*;

const MANIFEST: &str = r#"
id = "test.managed-tools"
name = "Managed tools fixture"
version = "0.0.1"
[[services]]
class = "service"
interface_id = "bcode.harness/v3"
name = "Managed fixture"
[concurrency]
type = "exclusive"
[runtime]
type = "native"
abi_version = 5
library = "unused-managed-tool-fixture"
"#;
#[derive(Default)]
struct NeverNative;
impl RustPlugin for NeverNative {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        let request: HarnessRequest = context.request.payload_json().unwrap();
        assert!(
            matches!(request.kind, HarnessRequestKind::Respond { .. }),
            "only explicit offline approval delivery may invoke this fixture"
        );
        ServiceResponse::json(&HarnessServiceResponse {
            schema_version: HARNESS_SCHEMA_VERSION,
            request_id: request.request_id.clone(),
            operation: HarnessOperation::Respond,
            kind: HarnessServiceResponseKind::Acknowledged {
                mutation_id: request.request_id,
            },
        })
        .unwrap()
    }
}

type Fixture = (
    tempfile::TempDir,
    Arc<ServerState>,
    ManagedCaller,
    ManagedSessionCall,
);
fn fixture() -> std::pin::Pin<Box<dyn Future<Output = Fixture> + Send>> {
    configured_fixture(false)
}
fn configured_fixture(with_open: bool) -> std::pin::Pin<Box<dyn Future<Output = Fixture> + Send>> {
    Box::pin(async move {
        let root = tempfile::tempdir().unwrap();
        let sessions = bcode_session::SessionManager::persistent_with_metrics_and_lease_owner(
            root.path(),
            bcode_metrics::MetricsRegistry::disabled(),
            bcode_session::lease::SessionLeaseOwnerContext {
                build_fingerprint: Some("managed-fixture".into()),
                daemon_instance_id: Some("managed-daemon".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let session = sessions
            .create_session(None, root.path().into())
            .await
            .unwrap();
        let durable = ExternalHarnessOwner {
            session_id: session.id,
            generation: 1,
            artifact_id: "managed-fixture".into(),
            daemon_id: "managed-daemon".into(),
        };
        let instance = HarnessInstance {
            instance_id: "fixture".into(),
            plugin_id: "test.managed-tools".into(),
            executable: "/not-run".into(),
            config_namespace: "/fixture".into(),
            auth: HarnessAuthBinding {
                provider_id: "fixture".into(),
                profile: "native".into(),
                credential_namespace: "fixture".into(),
                custody: HarnessAuthCustody::NativeManaged,
            },
        };
        let capability = HarnessCapability {
            support: HarnessSupportLevel::Supported,
            constraints: vec![],
        };
        let capabilities = HarnessCapabilities {
            resume: capability.clone(),
            steering: capability.clone(),
            approval: capability.clone(),
            questions: capability.clone(),
            assistance: capability.clone(),
            interrupt: capability.clone(),
            authoritative_settlement: capability.clone(),
            background_continuation: capability.clone(),
            authentication: capability,
        };
        if with_open {
            let opening = native_observation::opening_request(session.id, &instance, root.path());
            Box::pin(sessions.prepare_external_harness_open(
                &durable,
                &opening,
                capabilities,
                None,
            ))
            .await
            .unwrap();
        } else {
            sessions
                .configure_external_harness(&durable, instance.clone(), capabilities)
                .await
                .unwrap();
        }
        let state = fixture_server(sessions, durable);
        let client = ClientId::new();
        state
            .set_client_runtime_context(
                client,
                Some(ClientRuntimeContext {
                    working_directory: Some(root.path().into()),
                    ..Default::default()
                }),
            )
            .await;
        let call = ManagedSessionCall {
            session_id: Some(session.id),
            parent_session_id: None,
            assistance_destination_session_id: None,
            harness: HarnessRequest {
                schema_version: HARNESS_SCHEMA_VERSION,
                request_id: "host-bootstrap".into(),
                instance,
                kind: HarnessRequestKind::Inspect,
            },
        };
        (root, state, ManagedCaller::Client(client), call)
    })
}

fn fixture_server(
    sessions: bcode_session::SessionManager,
    durable: ExternalHarnessOwner,
) -> Arc<ServerState> {
    let bundle = bcode_plugin::StaticBundledPlugin::new(
        MANIFEST,
        bcode_plugin_sdk::static_plugin_vtable!(NeverNative, MANIFEST),
    );
    let mut state = Arc::new(crate::tests::test_server_state_with_plugins(
        sessions,
        bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
            &bcode_plugin::PluginSelection::all_enabled(),
            &[
                bundle,
                bcode_plugin::StaticBundledPlugin::new(
                    include_str!("../../../../../plugins/filesystem-plugin/bcode-plugin.toml"),
                    bcode_filesystem_plugin::static_plugin(),
                ),
            ],
        )
        .unwrap(),
    ));
    {
        let host = Arc::get_mut(&mut state).unwrap();
        host.daemon_status.instance_id = durable.daemon_id;
        host.daemon_status.build_fingerprint = durable.artifact_id;
        host.daemon_status.artifact_id = Some(bcode_ipc::ArtifactId::current());
    }
    crate::harness::install_canonical_backend(&state).unwrap();
    state
}

async fn bridge(
    state: Arc<ServerState>,
    caller: ManagedCaller,
    call: ManagedSessionCall,
) -> ManagedToolBridge {
    ManagedToolBridge::issue(
        state,
        caller,
        call,
        TurnExecutionOptions {
            tools: bcode_session_models::TurnToolPolicy::ReadOnly,
            ..Default::default()
        },
        Arc::new(TurnCancelState::default()),
    )
    .await
    .unwrap()
    .1
}

fn pinned(grant: &Grant) -> Arc<RetainedPreparation> {
    let call = bcode_model::ToolCall {
        id: "ordinary-call".into(),
        name: "fixture.write".into(),
        arguments: serde_json::json!({"path":"a"}),
    };
    let tool = RegisteredTool::plugin(
        bcode_tool::ToolDefinition {
            name: call.name.clone(),
            description: "fixture".into(),
            input_schema: serde_json::json!({"type":"object"}),
        },
        "test.managed-tools",
    );
    let metadata = bcode_agent_profile::ToolPolicyAuthorizationMetadata {
        requires_permission: true,
        aliases: vec![],
        compatibility_aliases: vec![],
        capabilities: vec![],
        permission_category: None,
        operation: bcode_agent_profile::ToolPolicyOperation::Mutating,
    };
    let preparation = ToolPreparationResponse {
        authorization: vec![bcode_tool::ToolAuthorizationFact {
            namespace: bcode_agent_profile::TOOL_POLICY_AUTHORIZATION_NAMESPACE.into(),
            schema_version: bcode_agent_profile::TOOL_POLICY_AUTHORIZATION_SCHEMA_VERSION,
            action: bcode_agent_profile::TOOL_POLICY_AUTHORIZATION_ACTION_INVOKE.into(),
            resource: Some(call.name.clone()),
            metadata: serde_json::to_value(metadata).unwrap(),
        }],
        descriptor: serde_json::json!({"pinned":true}),
    };
    Arc::new(RetainedPreparation {
        identity: ManagedToolIdentity {
            owner: grant.durable_owner.clone(),
            grant_id: grant.id.clone(),
            invocation_id: call.id.clone(),
            producer_plugin_id: "test.managed-tools".into(),
            tool_name: call.name.clone(),
            arguments_sha256: hash_value(&call.arguments).unwrap(),
            preparation_sha256: hash_value(&serde_json::to_value(&preparation).unwrap()).unwrap(),
            working_directory: grant.working_directory.to_str().unwrap().into(),
        },
        call,
        tool,
        preparation,
        cancellation: Arc::new(TurnCancelState::default()),
    })
}
fn request(operation: ManagedToolOperation) -> ManagedToolRequest {
    ManagedToolRequest {
        version: MANAGED_TOOL_BRIDGE_VERSION,
        request_id: "correlation".into(),
        operation,
    }
}

#[tokio::test]
async fn unregistered_client_and_invocation_parentage_cannot_issue_grants() {
    let (_root, state, _, call) = fixture().await;
    for caller in [
        ManagedCaller::Client(ClientId::new()),
        ManagedCaller::Invocation {
            session_id: call.session_id.unwrap(),
            plugin_id: call.harness.instance.plugin_id.clone(),
            invocation_id: "native-says-so".into(),
        },
    ] {
        let retry_call = ManagedSessionCall {
            session_id: call.session_id,
            parent_session_id: call.parent_session_id.clone(),
            assistance_destination_session_id: call.assistance_destination_session_id.clone(),
            harness: serde_json::from_value(serde_json::to_value(&call.harness).unwrap()).unwrap(),
        };
        assert!(
            ManagedToolBridge::issue(
                Arc::clone(&state),
                caller,
                retry_call,
                TurnExecutionOptions::default(),
                Arc::new(TurnCancelState::default())
            )
            .await
            .is_err()
        );
    }
}

#[tokio::test]
async fn ordinary_approval_denial_never_claims_or_dispatches() {
    let (_root, state, caller, call) = fixture().await;
    let bridge = bridge(Arc::clone(&state), caller, call).await;
    let retained = pinned(&bridge.grant);
    let runner = InvocationRunner {
        state: Arc::clone(&state),
        grant: Arc::clone(&bridge.grant),
    };
    assert_eq!(
        runner.invoke(Arc::clone(&retained)).await.unwrap(),
        ManagedToolOutcome::Denied
    );
    drop(runner);
    assert!(
        state
            .sessions
            .inspect_managed_tool(&bridge.grant.durable_owner, retained.call.id.clone())
            .await
            .unwrap()
            .is_none()
    );
    drop(bridge);
}

#[tokio::test]
async fn postapproval_owner_generation_recheck_prevents_claim() {
    let (_root, state, caller, call) = fixture().await;
    let mut bridge = bridge(Arc::clone(&state), caller, call).await;
    Arc::get_mut(&mut bridge.grant).unwrap().owner.generation += 1;
    let retained = pinned(&bridge.grant);
    let outcome = Mutex::new(None);
    {
        let scope = InvocationScope::new(
            TurnScope::without_events("managed", TurnGeneration::new(1)),
            retained.call.id.clone(),
        );
        assert!(
            PinnedInvoker {
                state: &state,
                grant: &bridge.grant,
                retained: &retained,
                outcome: &outcome,
            }
            .invoke_tool(
                &retained.tool,
                &PreparedToolInvocation {
                    invocation: ToolInvocationDescriptor {
                        invocation_id: retained.call.id.clone(),
                        tool_name: retained.call.name.clone(),
                        arguments: retained.call.arguments.clone()
                    },
                    preparation: retained.preparation.clone(),
                },
                &scope
            )
            .await
            .is_err()
        );
    }
    assert!(
        state
            .sessions
            .inspect_managed_tool(&bridge.grant.durable_owner, retained.call.id.clone())
            .await
            .unwrap()
            .is_none()
    );
    drop(bridge);
}

#[tokio::test]
async fn conflicting_preparation_id_is_rejected_without_repreparing() {
    let (_root, state, caller, call) = fixture().await;
    let mut bridge = bridge(state, caller, call).await;
    let retained = pinned(&bridge.grant);
    bridge
        .retained
        .insert(retained.call.id.clone(), Arc::clone(&retained));
    assert_eq!(
        bridge
            .prepare(
                retained.call.id.clone(),
                retained.call.name.clone(),
                retained.call.arguments.clone()
            )
            .await
            .unwrap(),
        prepared_outcome(&retained)
    );
    assert!(
        bridge
            .prepare(
                retained.call.id.clone(),
                retained.call.name.clone(),
                serde_json::json!({"path":"other"})
            )
            .await
            .is_err()
    );
    drop(bridge);
}

#[tokio::test]
async fn exact_replay_returns_canonical_result_and_uncertain_claim_never_dispatches() {
    let (_root, state, caller, call) = fixture().await;
    let mut bridge = bridge(Arc::clone(&state), caller, call).await;
    let retained = pinned(&bridge.grant);
    let ManagedToolClaimOutcome::New(claim) = state
        .sessions
        .claim_managed_tool(retained.identity.clone())
        .await
        .unwrap()
    else {
        panic!("new")
    };
    bridge
        .retained
        .insert(retained.call.id.clone(), Arc::clone(&retained));
    let operation = || {
        request(ManagedToolOperation::Invoke {
            invocation_id: retained.call.id.clone(),
            preparation_sha256: retained.identity.preparation_sha256.clone(),
        })
    };
    let mut altered_identity = retained.identity.clone();
    altered_identity.arguments_sha256 = "c".repeat(64);
    assert!(
        state
            .sessions
            .claim_managed_tool(altered_identity)
            .await
            .is_err()
    );
    assert_eq!(
        bridge.handle(operation()).await.unwrap(),
        ManagedToolOutcome::Uncertain
    );
    state
        .sessions
        .start_managed_tool(retained.identity.clone(), claim.claim_sequence)
        .await
        .unwrap();
    let result = bcode_session_models::ToolInvocationResultRecord {
        invocation_id: retained.call.id.clone(),
        model_output: "canonical result".into(),
        is_error: false,
        content: vec![],
        presentation: None,
        result: None,
    };
    state
        .sessions
        .complete_managed_tool(
            retained.identity.clone(),
            claim.claim_sequence,
            result.clone(),
        )
        .await
        .unwrap();
    let expected = ManagedToolOutcome::Recorded {
        result: serde_json::to_value(result).unwrap(),
    };
    retained.cancellation.close();
    assert_eq!(bridge.handle(operation()).await.unwrap(), expected);
    assert_eq!(bridge.handle(operation()).await.unwrap(), expected);
    assert_eq!(
        bridge
            .handle(request(ManagedToolOperation::Inspect {
                invocation_id: retained.call.id.clone()
            }))
            .await
            .unwrap(),
        expected
    );
    drop(bridge);
}

#[tokio::test]
async fn revoked_bootstrap_invalidates_an_already_issued_endpoint() {
    let (_root, state, caller, call) = fixture().await;
    let ManagedCaller::Client(client) = caller else {
        panic!("client fixture")
    };
    let mut bridge = bridge(Arc::clone(&state), ManagedCaller::Client(client), call).await;
    state.set_client_runtime_context(client, None).await;
    assert!(
        bridge
            .handle(request(ManagedToolOperation::Inspect {
                invocation_id: "ordinary-call".into()
            }))
            .await
            .is_err()
    );
    drop(bridge);
}

#[tokio::test]
async fn queue_service_is_not_a_public_or_invocation_routing_capability() {
    let (_root, state, _, _) = fixture().await;
    let direct = crate::plugin_operations::invoke_service(
        &state,
        "test.managed-tools",
        MANAGED_TOOL_QUEUE_INTERFACE_ID,
        "bind".into(),
        vec![],
    )
    .await;
    assert!(direct.is_err());
    let discovered = crate::plugin_operations::call_service(
        &state,
        MANAGED_TOOL_QUEUE_INTERFACE_ID,
        "bind".into(),
        vec![],
    )
    .await;
    assert!(discovered.is_err());
    assert!(crate::managed_callback_route_refusal(MANAGED_TOOL_QUEUE_INTERFACE_ID).is_some());
}

#[tokio::test]
async fn nonexistent_workflow_receipt_cannot_bootstrap_a_queue() {
    let (_root, state, _, call) = fixture().await;
    assert!(
        crate::harness_workflow::managed_tool_ceiling(
            &state,
            "unregistered-run",
            "native-supplied-dispatch",
            call.session_id.unwrap(),
            "bcode.harness",
            &call,
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn preparation_capacity_is_active_pressure_not_a_session_lifetime_limit() {
    let (_root, state, caller, call) = fixture().await;
    let mut bridge = bridge(state, caller, call).await;
    for index in 0..MAX_PREPARATIONS {
        let mut retained = pinned(&bridge.grant);
        let id = format!("cancelled-{index}");
        let preparation = Arc::get_mut(&mut retained).unwrap();
        preparation.call.id.clone_from(&id);
        preparation.identity.invocation_id.clone_from(&id);
        bridge.retained.insert(id, retained);
    }
    assert!(
        bridge.reclaim_preparations().await.is_err(),
        "live unclaimed work exerts explicit pressure"
    );
    for preparation in bridge.retained.values() {
        preparation.cancellation.close();
    }
    bridge.reclaim_preparations().await.unwrap();
    assert!(bridge.retained.is_empty());
    drop(bridge);
}

fn make_endpoint(
    mut driver: ManagedToolBridge,
) -> (
    Arc<RetainedPreparation>,
    ManagedToolEndpoint,
    tokio::task::JoinHandle<()>,
) {
    let retained = pinned(&driver.grant);
    driver
        .retained
        .insert(retained.call.id.clone(), Arc::clone(&retained));
    let (sender, receiver) = mpsc::channel(QUEUE_CAPACITY);
    driver.receiver = receiver;
    (
        retained,
        ManagedToolEndpoint { sender },
        tokio::spawn(Box::new(driver).serve()),
    )
}

#[tokio::test]
async fn issued_queue_correlates_inspect_and_cancellation_without_native_lock() {
    let (_root, state, caller, call) = fixture().await;
    let (retained, endpoint, serving) =
        make_endpoint(bridge(Arc::clone(&state), caller, call).await);
    let response = endpoint
        .request(request(ManagedToolOperation::Inspect {
            invocation_id: retained.call.id.clone(),
        }))
        .await
        .unwrap();
    assert_eq!(response.request_id, "correlation");
    assert_eq!(response.outcome, ManagedToolOutcome::Missing);
    assert_eq!(
        endpoint
            .request(request(ManagedToolOperation::Cancel {
                invocation_id: retained.call.id.clone()
            }))
            .await
            .unwrap()
            .outcome,
        ManagedToolOutcome::CancelRequested
    );
    assert!(retained.cancellation.is_cancelled());
    drop(endpoint);
    serving.await.unwrap();
}

#[tokio::test]
async fn production_file_preparation_keeps_canonical_history_responsive() {
    use tracing_subscriber::prelude::*;
    let progress = progress::Progress::new();
    // This current-thread test scopes the subscriber to its runtime thread, including
    // spawned bridge/actor tasks, without changing the process-global subscriber.
    let subscriber = tracing_subscriber::registry().with(progress.clone());
    let _diagnostics = tracing::subscriber::set_default(subscriber);
    Box::pin(async {
        let (root, state, caller, call) = configured_fixture(true).await;
        std::fs::write(root.path().join("fixture.txt"), "offline fixture").unwrap();
        tokio::time::timeout(
            Duration::from_secs(10),
            Box::pin(native_observation::observe_native_read(
                &state, &caller, &call,
            )),
        )
        .await
        .expect("canonical approval and native tool observation must complete");
        let (endpoint, driver) = ManagedToolBridge::issue(
            Arc::clone(&state),
            caller,
            call,
            TurnExecutionOptions {
                tools: bcode_session_models::TurnToolPolicy::ReadOnly,
                ..Default::default()
            },
            Arc::new(TurnCancelState::default()),
        )
        .await
        .unwrap();
        let owner = driver.grant.owner.session_id;
        let task = tokio::spawn(Box::new(driver).serve());
        let preparation = endpoint.request(ManagedToolRequest {
            version: MANAGED_TOOL_BRIDGE_VERSION,
            request_id: "mcp-prepare".into(),
            operation: ManagedToolOperation::Prepare {
                invocation_id: "native-shaped-file-read".into(),
                tool_name: "filesystem.read".into(),
                arguments: serde_json::json!({"path":"fixture.txt"}),
            },
        });
        let history = state.sessions.external_harness_history(owner, None, 16);
        let (prepared, history) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(preparation, history)
        })
        .await
        .expect("production preparation must not block canonical history");
        let ManagedToolOutcome::Prepared {
            preparation_sha256, ..
        } = prepared.unwrap().outcome
        else {
            panic!("real file preparation must succeed");
        };
        history.unwrap();
        let invoking = endpoint.request(ManagedToolRequest {
            version: MANAGED_TOOL_BRIDGE_VERSION,
            request_id: "mcp-invoke".into(),
            operation: ManagedToolOperation::Invoke {
                invocation_id: "native-shaped-file-read".into(),
                preparation_sha256,
            },
        });
        let history = state.sessions.external_harness_history(owner, None, 16);
        let probes = invocation_probes(&endpoint, &progress);
        // Bound each operation, not the cumulative latency of 16 sequential probes.
        // All branches begin together; later probes cannot extend Invoke/history deadlines.
        let (invoked, history, ()) = tokio::join!(
            progress.request("invoke", 0, invoking),
            progress.request("history", 0, history),
            progress.branch("probes", 0, probes),
        );
        progress.dump();
        assert!(matches!(
            invoked.unwrap().outcome,
            ManagedToolOutcome::Recorded { .. }
        ));
        history.unwrap();
        drop(endpoint);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
    })
    .await;
}

async fn invocation_probes(endpoint: &ManagedToolEndpoint, progress: &progress::Progress) {
    for index in 0..16 {
        let response = progress
            .request(
                "inspect",
                index,
                endpoint.request(ManagedToolRequest {
                    version: MANAGED_TOOL_BRIDGE_VERSION,
                    request_id: format!("concurrent-inspect-{index}"),
                    operation: ManagedToolOperation::Inspect {
                        invocation_id: "native-shaped-file-read".into(),
                    },
                }),
            )
            .await;
        assert!(response.is_ok(), "inspect transport must succeed");
        assert_ne!(response.unwrap().outcome, ManagedToolOutcome::Rejected);
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn independently_scheduled_invocation_releases_gate_during_inline_inspection() {
    let gate = Arc::new(Mutex::new(()));
    let invocation_gate = Arc::clone(&gate);
    let (entered, acquired) = oneshot::channel();
    let (release, released) = oneshot::channel();
    let mut pending = tokio::task::JoinSet::new();
    pending.spawn(async move {
        let guard = invocation_gate.lock().await;
        entered.send(()).unwrap();
        released.await.unwrap();
        drop(guard);
    });
    acquired.await.unwrap();
    // The driver now waits inline on the same gate, without polling join_next.
    release.send(()).unwrap();
    let guard = tokio::time::timeout(Duration::from_secs(1), gate.lock())
        .await
        .expect("invocation must progress independently of metadata");
    drop(guard);
    pending.join_next().await.unwrap().unwrap();
    assert!(pending.is_empty());
}

#[tokio::test]
async fn dropping_invocation_task_set_aborts_without_detaching_gate_holder() {
    let gate = Arc::new(Mutex::new(()));
    let invocation_gate = Arc::clone(&gate);
    let (entered, acquired) = oneshot::channel();
    let mut pending = tokio::task::JoinSet::new();
    pending.spawn(async move {
        let guard = invocation_gate.lock().await;
        entered.send(()).unwrap();
        std::future::pending::<()>().await;
        drop(guard);
    });
    acquired.await.unwrap();
    drop(pending);
    let guard = tokio::time::timeout(Duration::from_secs(1), gate.lock())
        .await
        .expect("task-set drop must not detach invocation authority");
    drop(guard);
}
