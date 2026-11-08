//! Fresh Start crosses production admission, canonical delivery and static native transport.
//! Queue support must not require a target before Open has created that target.
use super::*;
use bcode_harness_models::*;
use bcode_plugin_sdk::prelude::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const MANIFEST: &str = r#"
id = "test.start-queue"
name = "Fresh Start queue fixture"
version = "0.0.1"
[[services]]
class = "service"
interface_id = "bcode.harness/v3"
name = "Native lifecycle"
[[services]]
class = "service"
interface_id = "bcode.harness.tool-queue/v1"
name = "Host queue"
[concurrency]
type = "exclusive"
[runtime]
type = "native"
abi_version = 5
library = "unused-start-queue-fixture"
"#;
static OPENS: AtomicUsize = AtomicUsize::new(0);
static SUBMITS: AtomicUsize = AtomicUsize::new(0);
static QUEUES: AtomicUsize = AtomicUsize::new(0);
static BINDS: AtomicUsize = AtomicUsize::new(0);
static FAIL_DRAIN: AtomicBool = AtomicBool::new(false);
static COMPLETIONS: AtomicUsize = AtomicUsize::new(0);
static CONFIRMED_CATALOG: AtomicBool = AtomicBool::new(false);
static FAIL_INSPECT: AtomicBool = AtomicBool::new(false);
#[derive(Default)]
struct StartQueue {
    bind_attempts: usize,
    scope: Option<ManagedToolQueueScope>,
    catalog_pending: bool,
}
fn capabilities() -> HarnessCapabilities {
    let capability = HarnessCapability {
        support: HarnessSupportLevel::Supported,
        constraints: vec![],
    };
    HarnessCapabilities {
        model_switch: bcode_harness_models::HarnessCapability::unsupported(),
        permission_mode: bcode_harness_models::HarnessCapability::unsupported(),
        resume: capability.clone(),
        steering: capability.clone(),
        approval: capability.clone(),
        questions: capability.clone(),
        assistance: capability.clone(),
        interrupt: capability.clone(),
        authoritative_settlement: capability.clone(),
        background_continuation: capability.clone(),
        authentication: capability,
    }
}
fn prepared_response(
    open: &HarnessOpenRequest,
    instance: &HarnessInstance,
) -> HarnessServiceResponseKind {
    OPENS.fetch_add(1, Ordering::SeqCst);
    HarnessServiceResponseKind::Prepared(HarnessPreparedSession {
        open_id: open.open_id.clone(),
        session_id: open.session_id.clone(),
        instance_id: instance.instance_id.clone(),
        generation: open.generation,
        native_version: "fixture".into(),
        requested_native_session_id: Some(open.session_id.clone()),
    })
}

impl RustPlugin for StartQueue {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        if context.request.interface_id == MANAGED_TOOL_QUEUE_INTERFACE_ID {
            QUEUES.fetch_add(1, Ordering::SeqCst);
            let request: ManagedToolQueueRequest = context.request.payload_json().unwrap();
            let response = match request {
                ManagedToolQueueRequest::Bind { scope } => {
                    BINDS.fetch_add(1, Ordering::SeqCst);
                    self.bind_attempts += 1;
                    self.scope = Some(scope);
                    self.catalog_pending = true;
                    // Simulate a bind applied natively with an unusable acknowledgement.
                    // The host must revoke even though it cannot accept this response.
                    if self.bind_attempts == 1 {
                        ManagedToolQueueResponse::Rejected
                    } else {
                        ManagedToolQueueResponse::Bound
                    }
                }
                ManagedToolQueueRequest::Revoke { scope } => {
                    if self.scope.as_ref() == Some(&scope) {
                        self.scope = None;
                    }
                    ManagedToolQueueResponse::Revoked
                }
                ManagedToolQueueRequest::Drain { scope } => {
                    if FAIL_DRAIN.swap(false, Ordering::SeqCst) {
                        ManagedToolQueueResponse::Rejected
                    } else if self.scope.as_ref() == Some(&scope) {
                        let requests = if std::mem::take(&mut self.catalog_pending)
                            || CONFIRMED_CATALOG.swap(false, Ordering::SeqCst)
                        {
                            vec![ManagedToolRequest {
                                version: MANAGED_TOOL_BRIDGE_VERSION,
                                request_id: "fixture-catalog".into(),
                                operation: ManagedToolOperation::Catalog {
                                    after: None,
                                    limit: 1,
                                },
                            }]
                        } else {
                            vec![]
                        };
                        ManagedToolQueueResponse::Requests { requests }
                    } else {
                        ManagedToolQueueResponse::Rejected
                    }
                }
                ManagedToolQueueRequest::Complete { response, .. } => {
                    assert!(
                        matches!(response.outcome, ManagedToolOutcome::Catalog { .. }),
                        "fresh admitted queue must deliver a catalog, not a rejected liveness result"
                    );
                    COMPLETIONS.fetch_add(1, Ordering::SeqCst);
                    ManagedToolQueueResponse::Completed
                }
            };
            return ServiceResponse::json(&response).unwrap();
        }
        let request: HarnessRequest = context.request.payload_json().unwrap();
        let kind = match &request.kind {
            HarnessRequestKind::Inspect => {
                if FAIL_INSPECT.swap(false, Ordering::SeqCst) {
                    return ServiceResponse::error("unavailable", "fixture inspection unavailable");
                }
                HarnessServiceResponseKind::Inspection(HarnessInspection {
                    native_version: "fixture".into(),
                    executable_identity: request.instance.executable.clone(),
                    config_namespace: request.instance.config_namespace.clone(),
                    capabilities: capabilities(),
                })
            }
            HarnessRequestKind::Open(open) => prepared_response(open, &request.instance),
            HarnessRequestKind::Submit { submission, .. } => {
                SUBMITS.fetch_add(1, Ordering::SeqCst);
                assert!(
                    self.scope.is_some(),
                    "first input requires an installed host queue"
                );
                HarnessServiceResponseKind::Receipt(HarnessReceipt {
                    submission_id: submission.submission_id.clone(),
                    attempt_id: submission.attempt_id.clone(),
                    session_id: submission.session_id.clone(),
                    generation: submission.generation,
                    state: HarnessDeliveryState::NativeAccepted,
                    native_request_id: Some("fixture-request".into()),
                })
            }
            _ => panic!("unexpected lifecycle operation"),
        };
        ServiceResponse::json(&HarnessServiceResponse {
            schema_version: HARNESS_SCHEMA_VERSION,
            request_id: request.request_id.clone(),
            operation: request.kind.operation(),
            kind,
        })
        .unwrap()
    }
}

#[tokio::test]
async fn fresh_start_delivers_open_before_any_tool_grant_and_retry_never_redelivers() {
    fresh_start_journey().await;
}

fn start_queue_state(sessions: bcode_session::SessionManager) -> ServerState {
    let mut state = crate::tests::test_server_state_with_plugins(
        sessions,
        bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
            &bcode_plugin::PluginSelection::all_enabled(),
            &[bcode_plugin::StaticBundledPlugin::new(
                MANIFEST,
                bcode_plugin_sdk::static_plugin_vtable!(StartQueue, MANIFEST),
            )],
        )
        .unwrap(),
    );
    state.daemon_status.build_fingerprint = "fresh-start".into();
    state.daemon_status.instance_id = "fresh-daemon".into();
    state.daemon_status.artifact_id = Some(bcode_ipc::ArtifactId::current());
    state
}

fn fresh_open_call(session: SessionId, directory: &std::path::Path) -> ManagedSessionCall {
    ManagedSessionCall {
        session_id: Some(session),
        parent_session_id: None,
        assistance_destination_session_id: None,
        harness: HarnessRequest {
            schema_version: HARNESS_SCHEMA_VERSION,
            request_id: "fresh-open".into(),
            instance: HarnessInstance {
                instance_id: "native".into(),
                plugin_id: "test.start-queue".into(),
                executable: "/not-executed".into(),
                config_namespace: "/fixture".into(),
                auth: HarnessAuthBinding {
                    provider_id: "native".into(),
                    profile: "native".into(),
                    credential_namespace: "/fixture".into(),
                    custody: HarnessAuthCustody::NativeManaged,
                },
            },
            kind: HarnessRequestKind::Open(HarnessOpenRequest {
                open_id: "fresh-open".into(),
                session_id: session.to_string(),
                generation: 1,
                working_directory: directory.to_str().unwrap().into(),
                resume: None,
                parent_assistance: None,
            }),
        },
    }
}

fn fresh_start_journey() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    Box::pin(async {
        let root = tempfile::tempdir().unwrap();
        let sessions = bcode_session::SessionManager::persistent_with_metrics_and_lease_owner(
            root.path(),
            bcode_metrics::MetricsRegistry::disabled(),
            bcode_session::lease::SessionLeaseOwnerContext {
                build_fingerprint: Some("fresh-start".into()),
                daemon_instance_id: Some("fresh-daemon".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let state = Arc::new(start_queue_state(sessions));
        crate::harness::install_canonical_backend(&state).unwrap();
        let client = bcode_session_models::ClientId::new();
        state
            .set_client_runtime_context(
                client,
                Some(bcode_ipc::ClientRuntimeContext {
                    working_directory: Some(root.path().into()),
                    ..Default::default()
                }),
            )
            .await;
        let session = SessionId::new();
        let call = fresh_open_call(session, root.path());
        assert!(!queue_eligible(
            &state,
            &ManagedCaller::Client(client),
            &call
        ));
        let encoded = serde_json::to_string(&call).unwrap();
        let first = crate::harness_dispatch::dispatch(
            &state,
            ManagedCaller::Client(client),
            bcode_ipc::ManagedSessionRequest {
                schema_version: bcode_ipc::harness::MANAGED_SESSION_VERSION,
                operation: bcode_ipc::harness::ManagedSessionOperation::Start,
                request_json: encoded.clone(),
            },
        )
        .await
        .unwrap();
        let response: HarnessServiceResponse = serde_json::from_str(&first.response_json).unwrap();
        assert!(matches!(
            response.kind,
            HarnessServiceResponseKind::Prepared(_)
        ));
        let snapshot = state
            .sessions
            .session_external_harness_state(session)
            .await
            .unwrap()
            .unwrap();
        assert!(
            snapshot.target.is_some(),
            "ordinary Open canonically publishes its prepared target"
        );
        let retry = crate::harness_dispatch::dispatch(
            &state,
            ManagedCaller::Client(client),
            bcode_ipc::ManagedSessionRequest {
                schema_version: bcode_ipc::harness::MANAGED_SESSION_VERSION,
                operation: bcode_ipc::harness::ManagedSessionOperation::Start,
                request_json: encoded,
            },
        )
        .await
        .unwrap();
        assert_eq!(first.response_json, retry.response_json);
        assert_eq!(OPENS.load(Ordering::SeqCst), 1);
        assert_eq!(
            QUEUES.load(Ordering::SeqCst),
            0,
            "Open/replay never issues tool authority"
        );
        let target = snapshot.target.unwrap();
        submit_journey(&state, client, session, call, target, root.path().into()).await;
        drop(state);
    })
}

fn submit_journey(
    state: &Arc<ServerState>,
    client: bcode_session_models::ClientId,
    session: SessionId,
    call: ManagedSessionCall,
    target: HarnessSessionTarget,
    working_directory: std::path::PathBuf,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
    Box::pin(async move {
        let mut submit = call;
        submit.harness.request_id = "fresh-submit".into();
        submit.harness.kind = HarnessRequestKind::Submit {
            target,
            submission: HarnessSubmission {
                submission_id: "fresh-submit".into(),
                attempt_id: "fresh-attempt".into(),
                session_id: session.to_string(),
                generation: 1,
                text: "fixture input".into(),
                placement: HarnessPlacement::NextTurn,
            },
        };
        let host = ToolQueueHost::default();
        let caller = ManagedCaller::Client(client);
        let denied = crate::harness_dispatch::dispatch(
            state,
            caller.clone(),
            bcode_ipc::ManagedSessionRequest {
                schema_version: bcode_ipc::harness::MANAGED_SESSION_VERSION,
                operation: bcode_ipc::harness::ManagedSessionOperation::Followup,
                request_json: serde_json::to_string(&submit).unwrap(),
            },
        )
        .await;
        assert!(denied.is_err());
        assert!(
            state
                .sessions
                .read_external_harness_retained_request(session, &submit.harness)
                .await
                .unwrap()
                .is_none(),
            "failed queue binding must precede canonical submission delivery"
        );
        assert!(host.live.lock().await.is_empty());
        assert_eq!(
            QUEUES.load(Ordering::SeqCst),
            2,
            "failed Bind must issue exact Revoke"
        );
        let _ = host.install(state, &caller, &submit).await.unwrap();
        let first_grant = host.live.lock().await.get(&session).unwrap().clone();
        assert!(endpoint_live(&first_grant.endpoint).await);
        tokio::time::timeout(Duration::from_secs(5), async {
            while COMPLETIONS.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("exclusive plugin lock released before catalog and completion");
        let _ = host.install(state, &caller, &submit).await.unwrap();
        assert_eq!(
            host.live.lock().await.get(&session).unwrap().scope,
            first_grant.scope,
            "unchanged authorized Submit reuses the exact grant"
        );

        // Same client/generation is not sufficient: its exact bootstrap has changed.
        state
            .set_client_runtime_context(
                client,
                Some(bcode_ipc::ClientRuntimeContext {
                    working_directory: Some(working_directory),
                    selected_model_id: Some("changed-bootstrap".into()),
                    ..Default::default()
                }),
            )
            .await;
        let _ = host.install(state, &caller, &submit).await.unwrap();
        let replacement = host.live.lock().await.get(&session).unwrap().clone();
        assert_ne!(first_grant.scope.bridge_id, replacement.scope.bridge_id);
        assert!(first_grant.cancellation.is_cancelled());
        assert!(!endpoint_live(&first_grant.endpoint).await);
        assert!(endpoint_live(&replacement.endpoint).await);
        host.revoke_exact(state, &first_grant.scope).await;
        assert!(
            endpoint_live(&replacement.endpoint).await,
            "stale failed Submit cannot revoke replacement scope"
        );
        host.revoke(state, session).await;
        assert!(replacement.cancellation.is_cancelled());
        assert!(!endpoint_live(&replacement.endpoint).await);
        assert!(host.live.lock().await.is_empty());
        failed_queue_journey(state, &host, &caller, &submit).await;
        cancelled_bind_journey(Arc::clone(state), session).await;
        production_submit_journey(
            Arc::clone(state),
            caller.clone(),
            serde_json::from_value(serde_json::to_value(&submit).unwrap()).unwrap(),
        )
        .await;
        Box::pin(confirmed_grant_journey(state, &caller, &submit)).await;
    })
}

fn confirmation_page(
    session: SessionId,
    prepared: HarnessPreparedSession,
) -> HarnessObservationPage {
    let status = HarnessExecutionStatus {
        connectivity: HarnessConnectivity::Connected,
        activity: HarnessActivity::Running,
        attention: HarnessAttention::None,
        continuation: HarnessContinuation::Available,
        completeness: HarnessCompleteness::Complete,
    };
    let binding = HarnessSessionBinding {
        schema_version: HARNESS_SCHEMA_VERSION,
        session_id: session.to_string(),
        instance_id: prepared.instance_id,
        generation: prepared.generation,
        native_version: prepared.native_version,
        native_session_id: prepared.requested_native_session_id.unwrap(),
    };
    HarnessObservationPage {
        live: Vec::new(),
        retention: None,
        hydration_requests: vec![],
        inventory_page: None,
        observations: vec![HarnessObservation {
            observation_id: "fixture-confirmation".into(),
            session_id: session.to_string(),
            generation: prepared.generation,
            native_correlations: vec![],
            completeness: HarnessCompleteness::Complete,
            kind: HarnessObservationKind::BindingConfirmed {
                open_id: prepared.open_id,
                binding,
            },
        }],
        next_after: Some("fixture-confirmation".into()),
        completeness: HarnessCompleteness::Complete,
        status,
    }
}

fn check_observation_authority(
    before: &bcode_session_models::external_harness::ExternalHarnessState,
    after: &bcode_session_models::external_harness::ExternalHarnessState,
) {
    let validate = super::super::CanonicalHarnessStore::validate_owner_observation;
    assert_ne!(before.observation_after, after.observation_after);
    validate(before, after).unwrap();
    let mut progressed = after.clone();
    progressed.observation_after = Some("later-observation-cursor".into());
    validate(after, &progressed).unwrap();
    progressed.owner.generation += 1;
    assert!(validate(after, &progressed).is_err());
    let mut replaced = after.clone();
    if let Some(HarnessSessionTarget::Confirmed(binding)) = &mut replaced.target {
        binding.native_session_id = "replacement-native-session".into();
        replaced.binding = Some(binding.clone());
    }
    assert!(validate(after, &replaced).is_err());
    let mut reconfigured = after.clone();
    reconfigured.instance.config_namespace = "/replacement/config".into();
    assert!(validate(after, &reconfigured).is_err());
}

fn confirmation_and_tool_page(
    session: SessionId,
    prepared: HarnessPreparedSession,
    generation: u64,
) -> HarnessObservationPage {
    let mut page = confirmation_page(session, prepared);
    page.observations.push(HarnessObservation {
        observation_id: "fixture-native-file-tool".into(),
        session_id: session.to_string(),
        generation,
        native_correlations: vec![],
        completeness: HarnessCompleteness::Complete,
        kind: HarnessObservationKind::Tool {
            input_json: None,
            native_tool_id: "native-file-tool".into(),
            name: "mcp__bcode-tools__filesystem_read".into(),
            status: "observed".into(),
        },
    });
    page.next_after = Some("fixture-native-file-tool".into());
    page
}

async fn confirmed_grant_journey(
    state: &Arc<ServerState>,
    caller: &ManagedCaller,
    submit: &ManagedSessionCall,
) {
    use crate::harness_dispatch::ManagedSessionAdmission;
    let copy = serde_json::from_value(serde_json::to_value(submit).unwrap()).unwrap();
    let cancellation = Arc::new(TurnCancelState::default());
    let (endpoint, driver) = ManagedToolBridge::issue(
        Arc::clone(state),
        caller.clone(),
        copy,
        admitted_ceiling(state, caller, submit).await.unwrap(),
        Arc::clone(&cancellation),
    )
    .await
    .unwrap();
    let driver = tokio::spawn(Box::new(driver).serve());
    let session = submit.session_id.unwrap();
    let selected = state
        .sessions
        .session_external_harness_state(session)
        .await
        .unwrap()
        .unwrap();
    let before_confirmation = selected.clone();
    let Some(HarnessSessionTarget::Prepared(prepared)) = selected.target else {
        panic!("fresh Open must still be prepared");
    };
    let page = confirmation_and_tool_page(session, prepared, selected.owner.generation);
    state
        .sessions
        .commit_external_harness_page(
            &selected.owner,
            selected.observation_after.as_deref(),
            &page,
        )
        .await
        .unwrap();
    let after_confirmation = state
        .sessions
        .session_external_harness_state(session)
        .await
        .unwrap()
        .unwrap();
    check_observation_authority(&before_confirmation, &after_confirmation);
    let admission = super::super::CanonicalManagedAdmission::new(
        super::super::CanonicalHarnessStore::new(state),
    );
    assert!(
        admission.existing(caller, submit).await.is_err(),
        "fresh dispatch must not adopt a historical target"
    );
    drop(admission);
    assert!(
        endpoint_live(&endpoint).await,
        "same-open confirmation must not revoke retained authority"
    );
    let response = endpoint
        .request(ManagedToolRequest {
            version: MANAGED_TOOL_BRIDGE_VERSION,
            request_id: "after-confirmation-catalog".into(),
            operation: ManagedToolOperation::Catalog {
                after: None,
                limit: 16,
            },
        })
        .await
        .unwrap();
    assert!(matches!(
        response.outcome,
        ManagedToolOutcome::Catalog { .. }
    ));
    assert_eq!(
        super::super::CanonicalHarnessStore::new(state)
            .current_owner(session)
            .await
            .unwrap()
            .generation,
        selected.owner.generation,
        "native confirmation page and catalog delivery must preserve readable ownership"
    );
    assert!(!cancellation.is_cancelled());
    let completed = COMPLETIONS.load(Ordering::SeqCst);
    CONFIRMED_CATALOG.store(true, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(5), async {
        while COMPLETIONS.load(Ordering::SeqCst) <= completed {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("production Submit pump must survive confirmation and deliver queued catalog");
    cancellation.close();
    assert!(!endpoint_live(&endpoint).await);
    driver.await.unwrap();
    state.shutdown_requested.store(true, Ordering::SeqCst);
}

async fn failed_queue_journey(
    state: &Arc<ServerState>,
    host: &ToolQueueHost,
    caller: &ManagedCaller,
    submit: &ManagedSessionCall,
) {
    let session = submit.session_id.unwrap();
    let failed_scope = host.install(state, caller, submit).await.unwrap().unwrap();
    let failed = host.live.lock().await.get(&session).unwrap().clone();
    assert_eq!(failed.scope, failed_scope);
    FAIL_DRAIN.store(true, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(5), failed.cancellation.cancelled())
        .await
        .expect("queue transport rejection must close grant admission");
    assert!(!endpoint_live(&failed.endpoint).await);
    let _ = host.install(state, caller, submit).await.unwrap();
    let last = host.live.lock().await.get(&session).unwrap().clone();
    assert_ne!(failed.scope.bridge_id, last.scope.bridge_id);
    host.shutdown().await;
    assert!(last.cancellation.is_cancelled());
    assert!(!endpoint_live(&last.endpoint).await);
    assert!(host.live.lock().await.is_empty());
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if queue_call(
                state,
                "test.start-queue",
                &ManagedToolQueueRequest::Drain {
                    scope: last.scope.clone(),
                },
            )
            .await
            .unwrap()
                == ManagedToolQueueResponse::Rejected
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("shutdown must revoke native scope before new binding");
}

fn production_submit_journey(
    state: Arc<ServerState>,
    caller: ManagedCaller,
    submit: ManagedSessionCall,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    Box::pin(async move {
        let request_json = serde_json::to_string(&submit).unwrap();
        let first = crate::harness_dispatch::dispatch(
            &state,
            caller.clone(),
            bcode_ipc::ManagedSessionRequest {
                schema_version: bcode_ipc::harness::MANAGED_SESSION_VERSION,
                operation: bcode_ipc::harness::ManagedSessionOperation::Followup,
                request_json: request_json.clone(),
            },
        )
        .await
        .unwrap();
        let mut inspection: ManagedSessionCall =
            serde_json::from_value(serde_json::to_value(&submit).unwrap()).unwrap();
        inspection.harness.request_id = "failed-concurrent-inspect".into();
        inspection.harness.kind = HarnessRequestKind::Inspect;
        FAIL_INSPECT.store(true, Ordering::SeqCst);
        let failed = crate::harness_dispatch::dispatch(
            &state,
            caller.clone(),
            bcode_ipc::ManagedSessionRequest {
                schema_version: bcode_ipc::harness::MANAGED_SESSION_VERSION,
                operation: bcode_ipc::harness::ManagedSessionOperation::Inspect,
                request_json: serde_json::to_string(&inspection).unwrap(),
            },
        )
        .await;
        assert!(failed.is_err());
        let completed = COMPLETIONS.load(Ordering::SeqCst);
        CONFIRMED_CATALOG.store(true, Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(5), async {
            while COMPLETIONS.load(Ordering::SeqCst) <= completed {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("failed Inspect must not revoke the admitted Submit queue");
        let binds = BINDS.load(Ordering::SeqCst);
        let retry = crate::harness_dispatch::dispatch(
            &state,
            caller,
            bcode_ipc::ManagedSessionRequest {
                schema_version: bcode_ipc::harness::MANAGED_SESSION_VERSION,
                operation: bcode_ipc::harness::ManagedSessionOperation::Followup,
                request_json,
            },
        )
        .await
        .unwrap();
        assert_eq!(first.response_json, retry.response_json);
        assert_eq!(SUBMITS.load(Ordering::SeqCst), 1);
        assert_eq!(
            BINDS.load(Ordering::SeqCst),
            binds,
            "retained response must not bind a queue"
        );
    })
}

fn cancelled_bind_journey(
    state: Arc<ServerState>,
    session: SessionId,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    Box::pin(async move {
        // Cancellation while an acknowledged native Bind has not been adopted must revoke
        // that exact scope. This tests cleanup without replaying any lifecycle mutation.
        let scope = ManagedToolQueueScope {
            session_id: session.to_string(),
            generation: 1,
            bridge_id: "cancelled-install".into(),
        };
        let cancellation = Arc::new(TurnCancelState::default());
        let (ready, bound) = tokio::sync::oneshot::channel();
        let binding = PendingBind {
            state: Arc::downgrade(&state),
            plugin: "test.start-queue".into(),
            scope: Some(scope.clone()),
            cancellation: Arc::clone(&cancellation),
        };
        let task_state = Arc::clone(&state);
        let task_scope = scope.clone();
        let task = tokio::spawn(async move {
            let _binding = binding;
            assert_eq!(
                queue_call(
                    &task_state,
                    "test.start-queue",
                    &ManagedToolQueueRequest::Bind { scope: task_scope }
                )
                .await
                .unwrap(),
                ManagedToolQueueResponse::Bound
            );
            let _ = ready.send(());
            std::future::pending::<()>().await;
        });
        bound.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(cancellation.is_cancelled());
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if queue_call(
                    &state,
                    "test.start-queue",
                    &ManagedToolQueueRequest::Drain {
                        scope: scope.clone(),
                    },
                )
                .await
                .unwrap()
                    == ManagedToolQueueResponse::Rejected
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cancelled install must revoke native scope");
    })
}
