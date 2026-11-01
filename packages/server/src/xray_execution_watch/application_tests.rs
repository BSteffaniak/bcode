use super::*;
use bcode_xray_models::execution_watch as dto;

fn request() -> dto::OpenExecutionWatchRequest {
    dto::OpenExecutionWatchRequest {
        schema_version: 1,
        scope: dto::ExecutionWatchScope {
            root_run_id: "root".into(),
            recursive: true,
            deep_capture: true,
        },
    }
}

#[test]
fn admission_reservation_covers_first_event_without_inventing_prior_gap() {
    let registry = ExecutionWatchRegistry::default();
    let hub = XrayHub::new(bcode_xray::XrayConfig::default());
    let ready = registry.reserve_admission("alice", request()).unwrap();
    assert!(!ready.prior_execution_gap);
    let session = SessionId::new();
    let authority = WorkflowExecutionAuthority {
        target_artifact_id: "artifact".into(),
        daemon_instance_id: "daemon".into(),
        generation: 1,
        fencing_token: "private".into(),
    };
    registry.before_dispatch(&hub, "root", session, &authority, &[], true);
    hub.emit(bcode_xray_models::XrayEvent::new(
        "provider",
        "first_request",
        bcode_xray_models::XrayCorrelation {
            session_id: session.to_string(),
            run_id: Some("root".into()),
            ..Default::default()
        },
        serde_json::json!({"request":"first"}),
        false,
    ));
    let first = registry
        .read_authorized(
            "alice",
            dto::ReadExecutionWatchRequest {
                schema_version: 1,
                watch_id: ready.watch_id.clone(),
                cursor: 0,
                limit: 8,
            },
        )
        .unwrap();
    assert!(!first.prior_execution_gap);
    let event = registry
        .read_authorized(
            "alice",
            dto::ReadExecutionWatchRequest {
                schema_version: 1,
                watch_id: ready.watch_id.clone(),
                cursor: first.next_cursor,
                limit: 8,
            },
        )
        .unwrap();
    assert_eq!(event.events[0].envelope.event.name, "first_request");
    let final_evidence = registry
        .finalize_authorized(
            "alice",
            dto::FinalizeExecutionWatchRequest {
                schema_version: 1,
                watch_id: ready.watch_id,
                offset: 0,
                limit: 8,
            },
        )
        .unwrap();
    assert!(!final_evidence.prior_execution_gap);
}

#[test]
fn duplicate_observed_admission_never_allocates_another_run_identity() {
    let registry = ExecutionWatchRegistry::default();
    let ready = registry.reserve_admission("alice", request()).unwrap();
    assert_eq!(ready.scope.root_run_id, "root");
    assert!(matches!(
        registry.reserve_admission("alice", request()),
        Err(dto::ExecutionWatchError::InvalidRequest)
    ));
    let page = registry
        .read_authorized(
            "alice",
            dto::ReadExecutionWatchRequest {
                schema_version: 1,
                watch_id: ready.watch_id,
                cursor: 0,
                limit: 1,
            },
        )
        .unwrap();
    assert!(!page.prior_execution_gap);
}

#[test]
fn typed_boundary_preserves_scope_and_rejects_foreign_principals() {
    let registry = ExecutionWatchRegistry::default();
    let ready = registry.open_authorized("alice", request()).unwrap();
    assert_eq!(ready.scope, request().scope);
    assert_eq!(ready.cursor, 0);
    assert!(ready.prior_execution_gap);
    let read = dto::ReadExecutionWatchRequest {
        schema_version: 1,
        watch_id: ready.watch_id.clone(),
        cursor: 0,
        limit: 1,
    };
    assert!(matches!(
        registry.read_authorized("bob", read.clone()),
        Err(dto::ExecutionWatchError::Forbidden)
    ));
    assert!(registry.read_authorized("alice", read).is_ok());
    assert!(matches!(
        registry.revoke_authorized(
            "bob",
            dto::RevokeExecutionWatchRequest {
                schema_version: 1,
                watch_id: ready.watch_id.clone(),
            }
        ),
        Err(dto::ExecutionWatchError::Forbidden)
    ));
    registry
        .revoke_authorized(
            "alice",
            dto::RevokeExecutionWatchRequest {
                schema_version: 1,
                watch_id: ready.watch_id.clone(),
            },
        )
        .unwrap();
    assert!(matches!(
        registry.read_authorized(
            "alice",
            dto::ReadExecutionWatchRequest {
                schema_version: 1,
                watch_id: ready.watch_id,
                cursor: 1,
                limit: 1,
            }
        ),
        Err(dto::ExecutionWatchError::Forbidden)
    ));
}

#[test]
fn typed_finalization_retains_payloads_after_event_eof_and_prevents_new_attachment() {
    let hub = XrayHub::new(bcode_xray::XrayConfig::default());
    let registry = ExecutionWatchRegistry::default();
    let ready = registry.open_authorized("alice", request()).unwrap();
    let session = SessionId::new();
    let authority = WorkflowExecutionAuthority {
        target_artifact_id: "artifact".into(),
        daemon_instance_id: "daemon".into(),
        generation: 1,
        fencing_token: "private".into(),
    };
    registry.before_dispatch(&hub, "root", session, &authority, &[], true);
    let first = registry
        .read_authorized(
            "alice",
            dto::ReadExecutionWatchRequest {
                schema_version: 1,
                watch_id: ready.watch_id.clone(),
                cursor: 0,
                limit: 8,
            },
        )
        .unwrap();
    let payload = hub
        .capture_payload(
            &bcode_xray_models::XrayCorrelation {
                session_id: session.to_string(),
                run_id: Some("root".into()),
                ..Default::default()
            },
            "application/octet-stream",
            b"retained",
        )
        .unwrap()
        .unwrap();
    let finalize = dto::FinalizeExecutionWatchRequest {
        schema_version: 1,
        watch_id: ready.watch_id.clone(),
        offset: 0,
        limit: 1,
    };
    let evidence = registry
        .finalize_authorized("alice", finalize.clone())
        .unwrap();
    assert_eq!(evidence.status, dto::ExecutionWatchStatus::Finalized);
    assert_eq!(evidence.cutoffs.len(), 1);
    let again = registry.finalize_authorized("alice", finalize).unwrap();
    assert_eq!(
        again.cutoffs[0].finalization,
        evidence.cutoffs[0].finalization
    );
    let future = SessionId::new();
    registry.before_dispatch(&hub, "child", future, &authority, &["root".into()], true);
    assert!(!hub.capture_active(&future.to_string()));
    let mut cursor = first.next_cursor;
    for _ in 0..8 {
        cursor = registry
            .read_authorized(
                "alice",
                dto::ReadExecutionWatchRequest {
                    schema_version: 1,
                    watch_id: ready.watch_id.clone(),
                    cursor,
                    limit: 8,
                },
            )
            .unwrap()
            .next_cursor;
    }
    let export = dto::ExportExecutionWatchPayloadRequest {
        schema_version: 1,
        watch_id: ready.watch_id,
        run_id: "root".into(),
        payload: bcode_xray_models::XrayReadPayloadRequest {
            schema_version: 1,
            observation_id: first.events[0].envelope.observation_id.clone(),
            session_id: session.to_string(),
            payload_id: payload.payload_id,
            offset: 0,
            length: 8,
        },
    };
    assert_eq!(
        registry
            .export_authorized(&hub, "alice", export.clone())
            .unwrap()
            .bytes,
        b"retained"
    );
    assert!(matches!(
        registry.export_authorized(&hub, "bob", export),
        Err(dto::ExecutionWatchError::Forbidden)
    ));
}

#[test]
fn canonical_diagnostic_policy_never_inherits_parent_or_local_client_access() {
    use crate::workflow_operations::{
        WorkflowApplicationAuthorizationDecision, authorize_execution_observation_grants,
        authorize_local_workflow_application_operation,
    };
    use bcode_workflow::{
        WorkflowApplicationActor, WorkflowApplicationActorKind, WorkflowApplicationOperation,
        WorkflowApplicationOperationFacts, WorkflowExecutionObservationFacts,
    };
    let actor = WorkflowApplicationActor {
        kind: WorkflowApplicationActorKind::LocalClient,
        actor_id: "alice".into(),
    };
    let scope = WorkflowExecutionObservationFacts {
        root_run_id: "root".into(),
        recursive: true,
        deep_capture: true,
        state_location_id: "location".into(),
        namespace: "namespace".into(),
    };
    let mut facts = WorkflowApplicationOperationFacts {
        version: bcode_workflow::WORKFLOW_APPLICATION_OPERATION_FACTS_VERSION,
        operation: WorkflowApplicationOperation::ObserveExecution,
        execution_observation: Some(scope.clone()),
        actor: actor.clone(),
        definition_registration: None,
        workflow_id: String::new(),
        draft_id: None,
        revision: None,
        preset_id: None,
        producer: None,
        requirements: bcode_workflow::WorkflowRequirementSummary::default(),
        effects: bcode_workflow::WorkflowEffectSummary::default(),
        activates: false,
        executes: false,
    };
    assert!(facts.validate().is_ok());
    assert!(matches!(
        authorize_local_workflow_application_operation(&facts),
        WorkflowApplicationAuthorizationDecision::Deny { .. }
    ));
    let approved = [(actor, WorkflowApplicationOperation::ObserveExecution, scope)];
    assert_eq!(
        authorize_execution_observation_grants(&facts, &approved),
        WorkflowApplicationAuthorizationDecision::Allow
    );
    facts.execution_observation.as_mut().unwrap().recursive = false;
    assert!(matches!(
        authorize_execution_observation_grants(&facts, &approved),
        WorkflowApplicationAuthorizationDecision::Deny { .. }
    ));
    facts.execution_observation.as_mut().unwrap().recursive = true;
    for field in ["run", "deep", "location", "namespace", "actor", "operation"] {
        let mut foreign = facts.clone();
        let scope = foreign.execution_observation.as_mut().unwrap();
        match field {
            "run" => scope.root_run_id = "other".into(),
            "deep" => scope.deep_capture = false,
            "location" => scope.state_location_id = "other".into(),
            "namespace" => scope.namespace = "other".into(),
            "actor" => foreign.actor.actor_id = "bob".into(),
            "operation" => {
                foreign.operation = WorkflowApplicationOperation::ReadExecutionObservation;
            }
            _ => unreachable!(),
        }
        assert!(
            matches!(
                authorize_execution_observation_grants(&foreign, &approved),
                WorkflowApplicationAuthorizationDecision::Deny { .. }
            ),
            "grant must not alias {field}"
        );
    }
    facts.executes = true;
    assert!(facts.validate().is_err());
    facts.executes = false;
    facts.execution_observation = None;
    assert!(facts.validate().is_err());
}

async fn withdrawn_scope_state() -> (
    crate::ServerState,
    crate::ClientId,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    let mut state = crate::tests::test_server_state(bcode_session::SessionManager::default());
    state.daemon_status.state_location_id = Some("location".into());
    state.workflow_execution_plugin_policy = false;
    let caller = crate::ClientId::new();
    let checked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let policy_checked = std::sync::Arc::clone(&checked);
    state.workflow_application_authorization =
        crate::workflow_operations::WorkflowApplicationAuthorizationPolicy {
            evaluator: std::sync::Arc::new(move |facts| {
                assert_eq!(facts.actor.actor_id, caller.to_string());
                assert_eq!(
                    facts.operation,
                    bcode_workflow::WorkflowApplicationOperation::ObserveExecution
                );
                let scope = facts.execution_observation.as_ref().unwrap();
                assert_eq!(scope.root_run_id, "root");
                assert!(scope.recursive);
                assert!(scope.deep_capture);
                assert_eq!(scope.namespace, "namespace");
                assert_eq!(scope.state_location_id, "location");
                policy_checked.fetch_add(1, Ordering::Relaxed);
                crate::workflow_operations::WorkflowApplicationAuthorizationDecision::Deny {
                    reason: "withdrawn exact diagnostic grant".into(),
                }
            }),
        };
    state.clients.lock().await.insert(caller);
    state
        .client_session_namespaces
        .lock()
        .await
        .insert(caller, "namespace".into());
    (state, caller, checked)
}

fn ready_cursor(registry: &ExecutionWatchRegistry, caller: crate::ClientId, watch_id: &str) -> u64 {
    let ready = registry
        .read_authorized(
            &caller.to_string(),
            dto::ReadExecutionWatchRequest {
                schema_version: 1,
                watch_id: watch_id.to_owned(),
                cursor: 0,
                limit: 8,
            },
        )
        .unwrap();
    assert_eq!(ready.events.len(), 1);
    assert_eq!(ready.events[0].envelope.event.name, "ready");
    ready.next_cursor
}

fn emit_scoped(hub: &XrayHub, name: &str, correlation: bcode_xray_models::XrayCorrelation) {
    hub.emit(bcode_xray_models::XrayEvent::new(
        "provider",
        name,
        correlation,
        serde_json::json!({}),
        false,
    ));
}

#[tokio::test]
async fn dispatch_reauthorization_revokes_withdrawn_scope_without_touching_other_runs() {
    let (state, caller, checked) = withdrawn_scope_state().await;
    let registry = &state.xray_execution_watches;
    let denied = registry
        .open_authorized(&caller.to_string(), request())
        .unwrap();
    let mut other = request();
    other.scope.root_run_id = "unrelated".into();
    let unrelated = registry
        .open_authorized(&caller.to_string(), other)
        .unwrap();
    let session = SessionId::new();
    let authority = WorkflowExecutionAuthority {
        target_artifact_id: "artifact".into(),
        daemon_instance_id: "daemon".into(),
        generation: 1,
        fencing_token: "private".into(),
    };
    registry.before_dispatch(&state.xray, "root", session, &authority, &[], true);
    registry.before_dispatch(&state.xray, "unrelated", session, &authority, &[], true);
    let root_correlation = bcode_xray_models::XrayCorrelation {
        session_id: session.to_string(),
        run_id: Some("root".into()),
        ..Default::default()
    };
    let unrelated_correlation = bcode_xray_models::XrayCorrelation {
        run_id: Some("unrelated".into()),
        ..root_correlation.clone()
    };
    assert!(state.xray.correlation_active(&root_correlation, true));
    assert!(state.xray.correlation_active(&unrelated_correlation, true));
    // Each page polls one envelope per subscription. Consume readiness explicitly
    // before asserting delivery of the domain events below.
    let root_cursor = ready_cursor(registry, caller, &denied.watch_id);
    let unrelated_cursor = ready_cursor(registry, caller, &unrelated.watch_id);
    emit_scoped(&state.xray, "before_withdrawal", root_correlation.clone());
    let root_page = registry
        .read_authorized(
            &caller.to_string(),
            dto::ReadExecutionWatchRequest {
                schema_version: 1,
                watch_id: denied.watch_id.clone(),
                cursor: root_cursor,
                limit: 8,
            },
        )
        .unwrap();
    assert!(root_page.events.iter().any(|event| {
        event.envelope.event.name == "before_withdrawal"
            && event.envelope.event.correlation.run_id.as_deref() == Some("root")
    }));
    // Rechecking a future descendant must release the withdrawn root capture,
    // not the independently granted run sharing the same physical session.
    assert!(
        state
            .reauthorize_execution_dispatch("child", &["root".into()])
            .await
    );
    assert_eq!(checked.load(Ordering::Relaxed), 1);
    assert!(!state.xray.correlation_active(&root_correlation, false));
    assert!(!state.xray.correlation_active(&root_correlation, true));
    assert!(state.xray.correlation_active(&unrelated_correlation, true));
    for (name, correlation) in [
        ("withdrawn_root", root_correlation),
        ("unrelated_survives", unrelated_correlation),
    ] {
        emit_scoped(&state.xray, name, correlation);
    }
    let unrelated_page = registry
        .read_authorized(
            &caller.to_string(),
            dto::ReadExecutionWatchRequest {
                schema_version: 1,
                watch_id: unrelated.watch_id.clone(),
                cursor: unrelated_cursor,
                limit: 8,
            },
        )
        .unwrap();
    assert!(
        unrelated_page
            .events
            .iter()
            .any(|event| { event.envelope.event.name == "unrelated_survives" })
    );
    assert!(unrelated_page.events.iter().all(|event| {
        event.envelope.event.correlation.run_id.as_deref() == Some("unrelated")
            && event.envelope.event.name != "withdrawn_root"
            && event.envelope.event.name != "before_withdrawal"
    }));
    assert!(matches!(
        registry.scope_for_principal(&caller.to_string(), &denied.watch_id),
        Err(dto::ExecutionWatchError::Forbidden)
    ));
    assert!(
        registry
            .scope_for_principal(&caller.to_string(), &unrelated.watch_id)
            .is_ok()
    );
}

#[test]
fn configured_deadline_stops_exact_capture_without_another_application_operation() {
    let registry = ExecutionWatchRegistry::default();
    let hub = XrayHub::new(bcode_xray::XrayConfig::default());
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
    let ready = registry
        .open_authorized_until("alice", request(), Some(deadline))
        .unwrap();
    let session = SessionId::new();
    let authority = WorkflowExecutionAuthority {
        target_artifact_id: "artifact".into(),
        daemon_instance_id: "daemon".into(),
        generation: 1,
        fencing_token: "private".into(),
    };
    registry.before_dispatch(&hub, "root", session, &authority, &[], true);
    let correlation = bcode_xray_models::XrayCorrelation {
        session_id: session.to_string(),
        run_id: Some("root".into()),
        ..Default::default()
    };
    assert!(hub.correlation_active(&correlation, true));
    emit_scoped(&hub, "before_deadline", correlation.clone());
    std::thread::sleep(deadline.saturating_duration_since(std::time::Instant::now()));
    assert!(!hub.correlation_active(&correlation, false));
    assert!(!hub.correlation_active(&correlation, true));
    emit_scoped(&hub, "after_deadline", correlation.clone());
    assert!(
        hub.capture_payload(&correlation, "text/plain", b"late")
            .unwrap()
            .is_none()
    );
    let mut names = Vec::new();
    let mut cursor = 0;
    for _ in 0..4 {
        let page = registry
            .read_authorized(
                "alice",
                dto::ReadExecutionWatchRequest {
                    schema_version: 1,
                    watch_id: ready.watch_id.clone(),
                    cursor,
                    limit: 8,
                },
            )
            .unwrap();
        cursor = page.next_cursor;
        names.extend(
            page.events
                .into_iter()
                .map(|event| event.envelope.event.name),
        );
    }
    assert!(names.iter().any(|name| name == "before_deadline"));
    assert!(!names.iter().any(|name| name == "after_deadline"));
    assert!(matches!(
        registry.open_authorized_until("bob", request(), Some(deadline)),
        Err(dto::ExecutionWatchError::Forbidden)
    ));
}

#[test]
fn dispatch_grants_preserve_exact_scope_and_fail_closed_on_contention() {
    let registry = ExecutionWatchRegistry::default();
    let recursive = registry.open_authorized("alice", request()).unwrap();
    let mut direct = request();
    direct.scope.recursive = false;
    direct.scope.deep_capture = false;
    registry.open_authorized("bob", direct.clone()).unwrap();
    let descendants = registry.dispatch_grants("child", &["root".into()]).unwrap();
    assert_eq!(descendants.len(), 1);
    assert_eq!(descendants[0].principal(), "alice");
    assert_eq!(descendants[0].scope(), recursive.scope);
    let roots = registry.dispatch_grants("root", &[]).unwrap();
    assert_eq!(roots.len(), 2);
    assert_eq!(
        roots
            .iter()
            .find(|grant| grant.principal() == "bob")
            .unwrap()
            .scope(),
        direct.scope
    );
    assert!(
        registry
            .dispatch_grants("unrelated", &[])
            .unwrap()
            .is_empty()
    );
    let lock = registry.watches.lock().unwrap();
    assert!(matches!(
        registry.dispatch_grants("root", &[]),
        Err(dto::ExecutionWatchError::Unavailable)
    ));
    drop(lock);
    assert_eq!(registry.contention.load(Ordering::Relaxed), 1);
}

#[test]
fn dto_unknown_fields_and_recursive_scope_are_not_guessed() {
    let unknown = serde_json::json!({"schema_version":1,"scope":{
        "root_run_id":"root", "recursive":true, "deep_capture":false,
        "parent_session_access":true
    }});
    assert!(serde_json::from_value::<dto::OpenExecutionWatchRequest>(unknown).is_err());
    let missing_recursive = serde_json::json!({"schema_version":1,"scope":{
        "root_run_id":"root", "deep_capture":true
    }});
    assert!(serde_json::from_value::<dto::OpenExecutionWatchRequest>(missing_recursive).is_err());
    let registry = ExecutionWatchRegistry::default();
    let mut future = request();
    future.schema_version = 2;
    assert!(matches!(
        registry.open_authorized("alice", future),
        Err(dto::ExecutionWatchError::InvalidRequest)
    ));
}
