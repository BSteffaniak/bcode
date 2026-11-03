use super::*;
use bcode_xray_models::{XrayCorrelation, XrayEvent};
use serde_json::json;

fn grant(root: &str, descendants: bool) -> ExecutionWatchGrant {
    ExecutionWatchGrant::authorized("test-client".into(), root.into(), descendants, false).unwrap()
}

fn authority(generation: u64) -> WorkflowExecutionAuthority {
    WorkflowExecutionAuthority {
        target_artifact_id: "artifact".into(),
        daemon_instance_id: "daemon".into(),
        generation,
        fencing_token: format!("private-token-{generation}"),
    }
}

fn emit(hub: &XrayHub, session: SessionId, run: &str, name: &str) {
    hub.emit(XrayEvent::new(
        "workflow",
        name,
        XrayCorrelation {
            session_id: session.to_string(),
            run_id: Some(run.to_owned()),
            ..Default::default()
        },
        json!({"diagnostic":true}),
        false,
    ));
}

fn admit(
    registry: &ExecutionWatchRegistry,
    hub: &XrayHub,
    session: SessionId,
    run: &str,
    ancestors: &[String],
) {
    registry.before_dispatch(hub, run, session, &authority(1), ancestors, true);
}

#[test]
fn future_recursive_session_is_captured_before_first_dispatch() {
    let hub = XrayHub::new(bcode_xray::XrayConfig::default());
    let registry = ExecutionWatchRegistry::default();
    let grant = grant("root", true);
    let id = registry.open(1, &grant).unwrap();
    let session = SessionId::new();
    admit(
        &registry,
        &hub,
        session,
        "grandchild",
        &["root".into(), "child".into()],
    );
    let selected = XrayCorrelation {
        session_id: session.to_string(),
        run_id: Some("grandchild".into()),
        ..Default::default()
    };
    assert!(hub.correlation_active(&selected, false));
    assert!(!hub.correlation_active(
        &XrayCorrelation {
            run_id: Some("foreign".into()),
            ..selected
        },
        false
    ));
    emit(&hub, session, "foreign", "must_not_capture");
    emit(&hub, session, "grandchild", "first_dispatch");
    let ready = registry.page(&grant, &id, 0, 8).unwrap();
    assert_eq!(ready.status, ExecutionWatchStatus::Ready);
    assert!(ready.prior_execution_gap);
    assert_eq!(ready.events.len(), 1);
    assert_eq!(ready.events[0].envelope.event.name, "ready");
    let first = registry.page(&grant, &id, ready.next_cursor, 8).unwrap();
    assert_eq!(first.events[0].envelope.event.name, "first_dispatch");
    assert_eq!(first.events[0].run_id, "grandchild");
}

#[test]
fn parent_run_access_does_not_implicitly_grant_descendants() {
    let hub = XrayHub::new(bcode_xray::XrayConfig::default());
    let registry = ExecutionWatchRegistry::default();
    let grant = grant("root", false);
    let id = registry.open(1, &grant).unwrap();
    let session = SessionId::new();
    admit(&registry, &hub, session, "child", &["root".into()]);
    assert!(!hub.capture_active(&session.to_string()));
    assert!(registry.page(&grant, &id, 0, 1).unwrap().events.is_empty());
}

#[test]
fn independently_recreated_or_foreign_grants_cannot_read_or_close() {
    let registry = ExecutionWatchRegistry::default();
    let grant = grant("root", true);
    let id = registry.open(1, &grant).unwrap();
    let foreign =
        ExecutionWatchGrant::authorized("test-client".into(), "root".into(), true, false).unwrap();
    assert!(matches!(
        registry.page(&foreign, &id, 0, 1),
        Err(ExecutionWatchError::Forbidden)
    ));
    assert_eq!(
        registry.close(&foreign, &id),
        Err(ExecutionWatchError::Forbidden)
    );
    assert_eq!(registry.close(&grant, &id), Ok(()));
}

#[test]
fn revocation_drops_capture_and_pending_events_without_execution_actions() {
    let hub = XrayHub::new(bcode_xray::XrayConfig::default());
    let registry = ExecutionWatchRegistry::default();
    let grant = grant("root", true);
    let clone = grant.clone();
    let id = registry.open(1, &grant).unwrap();
    let session = SessionId::new();
    admit(&registry, &hub, session, "root", &[]);
    emit(&hub, session, "root", "pending");
    registry.revoke(&clone).unwrap();
    assert!(!hub.capture_active(&session.to_string()));
    assert!(matches!(
        registry.page(&grant, &id, 0, 1),
        Err(ExecutionWatchError::Forbidden)
    ));
    assert_eq!(
        registry.open(1, &grant),
        Err(ExecutionWatchError::Forbidden)
    );
}

#[test]
fn overload_disables_capture_instead_of_blocking_work() {
    let hub = XrayHub::new(bcode_xray::XrayConfig::default());
    let registry = ExecutionWatchRegistry::new(ExecutionWatchPolicy {
        max_sessions_per_watch: 1,
        ..Default::default()
    })
    .unwrap();
    let grant = grant("root", true);
    let id = registry.open(1, &grant).unwrap();
    let first = SessionId::new();
    admit(&registry, &hub, first, "root", &[]);
    let second = SessionId::new();
    admit(&registry, &hub, second, "child", &["root".into()]);
    assert!(!hub.capture_active(&first.to_string()));
    assert!(!hub.capture_active(&second.to_string()));
    assert_eq!(
        registry.page(&grant, &id, 0, 1).unwrap().status,
        ExecutionWatchStatus::Overloaded
    );
}

#[test]
fn verified_transfer_ends_segment_and_never_reopens_it() {
    let hub = XrayHub::new(bcode_xray::XrayConfig::default());
    let registry = ExecutionWatchRegistry::default();
    let grant = grant("root", true);
    let id = registry.open(1, &grant).unwrap();
    let first = SessionId::new();
    admit(&registry, &hub, first, "root", &[]);
    registry.ownership_changed("root").unwrap();
    admit(&registry, &hub, SessionId::new(), "root", &[]);
    assert!(!hub.capture_active(&first.to_string()));
    assert_eq!(
        registry.page(&grant, &id, 0, 1).unwrap().status,
        ExecutionWatchStatus::OwnershipChanged
    );
}

#[test]
fn changed_fencing_generation_cannot_reuse_old_capture() {
    let hub = XrayHub::new(bcode_xray::XrayConfig::default());
    let registry = ExecutionWatchRegistry::default();
    let grant = grant("root", false);
    let id = registry.open(1, &grant).unwrap();
    let session = SessionId::new();
    admit(&registry, &hub, session, "root", &[]);
    registry.before_dispatch(&hub, "root", session, &authority(2), &[], true);
    assert!(!hub.capture_active(&session.to_string()));
    assert_eq!(
        registry.page(&grant, &id, 0, 1).unwrap().status,
        ExecutionWatchStatus::OwnershipChanged
    );
}

#[test]
fn shared_parent_is_filtered_by_run_before_capture() {
    let hub = XrayHub::new(bcode_xray::XrayConfig::default());
    let registry = ExecutionWatchRegistry::default();
    let grant =
        ExecutionWatchGrant::authorized("test-client".into(), "root".into(), true, true).unwrap();
    let id = registry.open(1, &grant).unwrap();
    let session = SessionId::new();
    registry.before_dispatch(&hub, "root", session, &authority(1), &[], true);
    let selected = XrayCorrelation {
        session_id: session.to_string(),
        run_id: Some("root".into()),
        ..Default::default()
    };
    let foreign = XrayCorrelation {
        run_id: Some("foreign".into()),
        ..selected.clone()
    };
    assert!(hub.correlation_active(&selected, false));
    assert!(hub.correlation_active(&selected, true));
    assert!(!hub.correlation_active(&foreign, false));
    assert!(!hub.correlation_active(&foreign, true));
    assert!(
        hub.capture_payload(&foreign, "application/octet-stream", b"foreign bytes")
            .unwrap()
            .is_none()
    );
    let payload = hub
        .capture_payload(&selected, "application/octet-stream", b"selected bytes")
        .unwrap()
        .unwrap();
    emit(&hub, session, "foreign", "must_not_capture");
    emit(&hub, session, "root", "first_scoped_request");
    let ready = registry.page(&grant, &id, 0, 1).unwrap();
    assert_eq!(ready.omitted_links, 0);
    let first = registry.page(&grant, &id, ready.next_cursor, 1).unwrap();
    assert_eq!(first.events.len(), 1);
    assert_eq!(first.events[0].envelope.event.name, "first_scoped_request");
    let exported = registry
        .read_payload(
            &hub,
            &grant,
            &id,
            "root",
            &bcode_xray_models::XrayReadPayloadRequest {
                schema_version: 1,
                observation_id: first.events[0].envelope.observation_id.clone(),
                session_id: session.to_string(),
                payload_id: payload.payload_id,
                offset: 0,
                length: 14,
            },
        )
        .unwrap();
    assert_eq!(exported.bytes, b"selected bytes");
    let filtered = registry.page(&grant, &id, first.next_cursor, 1).unwrap();
    assert!(filtered.events.is_empty());
    assert_eq!(filtered.omitted_events, 0);
}

#[test]
fn contention_is_nonblocking_and_explicit() {
    let hub = XrayHub::new(bcode_xray::XrayConfig::default());
    let registry = ExecutionWatchRegistry::default();
    let grant = grant("root", true);
    let id = registry.open(1, &grant).unwrap();
    let lock = registry.watches.lock().unwrap();
    let session = SessionId::new();
    admit(&registry, &hub, session, "root", &[]);
    drop(lock);
    assert!(!hub.capture_active(&session.to_string()));
    assert_eq!(registry.page(&grant, &id, 0, 1).unwrap().contention_gaps, 1);
}

#[test]
fn paging_is_bounded_fair_and_foreign_events_are_not_exported() {
    let hub = XrayHub::new(bcode_xray::XrayConfig::default());
    let registry = ExecutionWatchRegistry::default();
    let grant = grant("root", true);
    let id = registry.open(1, &grant).unwrap();
    let first = SessionId::new();
    let second = SessionId::new();
    admit(&registry, &hub, first, "root", &[]);
    admit(&registry, &hub, second, "child", &["root".into()]);
    let page = registry.page(&grant, &id, 0, 1).unwrap();
    assert_eq!(page.events.len(), 1);
    let next = registry.page(&grant, &id, page.next_cursor, 1).unwrap();
    assert_eq!(next.events.len(), 1);
    assert_ne!(page.events[0].session_id, next.events[0].session_id);
    emit(&hub, first, "foreign-run", "must_not_export");
    let filtered = registry.page(&grant, &id, next.next_cursor, 8).unwrap();
    assert!(filtered.events.is_empty());
    assert_eq!(filtered.omitted_events, 0);
    assert!(matches!(
        registry.page(&grant, &id, 0, 1),
        Err(ExecutionWatchError::InvalidRequest)
    ));
}

#[test]
fn finalized_capture_can_drain_and_release_capacity_for_unbounded_successors() {
    let hub = XrayHub::new(bcode_xray::XrayConfig::default());
    let registry = ExecutionWatchRegistry::new(ExecutionWatchPolicy {
        max_sessions_per_watch: 1,
        ..Default::default()
    })
    .unwrap();
    let grant = grant("root", true);
    let id = registry.open(1, &grant).unwrap();
    let mut cursor = 0;
    for _ in 0..4 {
        let session = SessionId::new();
        admit(&registry, &hub, session, "child", &["root".into()]);
        registry
            .finalize_session(&grant, &id, session, "child")
            .unwrap();
        let mut finalized = false;
        for _ in 0..8 {
            let page = registry.page(&grant, &id, cursor, 8).unwrap();
            cursor = page.next_cursor;
            finalized |= page
                .events
                .iter()
                .any(|event| event.envelope.event.name == "finalized");
        }
        assert!(finalized);
        registry
            .release_session(&grant, &id, session, "child")
            .unwrap();
    }
    assert_eq!(
        registry.page(&grant, &id, cursor, 1).unwrap().status,
        ExecutionWatchStatus::Ready
    );
}

#[test]
fn incomplete_lineage_is_explicit_loss_not_guessed_permission() {
    let hub = XrayHub::new(bcode_xray::XrayConfig::default());
    let registry = ExecutionWatchRegistry::default();
    let grant = grant("subtree", true);
    let id = registry.open(1, &grant).unwrap();
    let session = SessionId::new();
    registry.before_dispatch(
        &hub,
        "leaf",
        session,
        &authority(1),
        &["root".into()],
        false,
    );
    assert!(!hub.capture_active(&session.to_string()));
    assert_eq!(registry.page(&grant, &id, 0, 1).unwrap().omitted_links, 1);
}

#[test]
fn one_shared_session_can_capture_two_runs_without_scope_aliasing() {
    let hub = XrayHub::new(bcode_xray::XrayConfig::default());
    let registry = ExecutionWatchRegistry::default();
    let grant = grant("root", true);
    let id = registry.open(1, &grant).unwrap();
    let session = SessionId::new();
    registry.before_dispatch(&hub, "root", session, &authority(1), &[], true);
    registry.before_dispatch(
        &hub,
        "child",
        session,
        &authority(1),
        &["root".into()],
        true,
    );
    let ready = registry.page(&grant, &id, 0, 8).unwrap();
    assert_eq!(ready.events.len(), 2);
    emit(&hub, session, "root", "root_event");
    emit(&hub, session, "child", "child_event");
    let page = registry.page(&grant, &id, ready.next_cursor, 8).unwrap();
    assert_eq!(page.status, ExecutionWatchStatus::Ready);
    assert_eq!(page.events.len(), 2);
    assert!(
        page.events
            .iter()
            .any(|event| event.run_id == "root" && event.envelope.event.name == "root_event")
    );
    assert!(
        page.events
            .iter()
            .any(|event| event.run_id == "child" && event.envelope.event.name == "child_event")
    );
}

#[test]
fn page_byte_budget_reports_consumed_oversized_event_loss() {
    let hub = XrayHub::new(bcode_xray::XrayConfig::default());
    let registry = ExecutionWatchRegistry::new(ExecutionWatchPolicy {
        max_page_bytes: 1024,
        ..Default::default()
    })
    .unwrap();
    let grant = grant("root", false);
    let id = registry.open(1, &grant).unwrap();
    let session = SessionId::new();
    admit(&registry, &hub, session, "root", &[]);
    let ready = registry.page(&grant, &id, 0, 8).unwrap();
    hub.emit(XrayEvent::new(
        "workflow",
        "large",
        XrayCorrelation {
            session_id: session.to_string(),
            run_id: Some("root".into()),
            ..Default::default()
        },
        json!({"body":"x".repeat(2048)}),
        false,
    ));
    let page = registry.page(&grant, &id, ready.next_cursor, 8).unwrap();
    assert!(page.events.is_empty());
    assert_eq!(page.omitted_events, 1);
}

#[test]
fn deep_payload_ranges_are_bound_to_exact_grant_run_and_observation() {
    let hub = XrayHub::new(bcode_xray::XrayConfig::default());
    let registry = ExecutionWatchRegistry::default();
    let grant =
        ExecutionWatchGrant::authorized("test-client".into(), "root".into(), true, true).unwrap();
    let id = registry.open(1, &grant).unwrap();
    let session = SessionId::new();
    admit(&registry, &hub, session, "root", &[]);
    let ready = registry.page(&grant, &id, 0, 8).unwrap();
    let reference = hub
        .capture_payload(
            &XrayCorrelation {
                session_id: session.to_string(),
                run_id: Some("root".into()),
                ..Default::default()
            },
            "application/octet-stream",
            b"bounded bytes",
        )
        .unwrap()
        .unwrap();
    let mut request = bcode_xray_models::XrayReadPayloadRequest {
        schema_version: 1,
        observation_id: ready.events[0].envelope.observation_id.clone(),
        session_id: session.to_string(),
        payload_id: reference.payload_id,
        offset: 0,
        length: 7,
    };
    assert_eq!(
        registry
            .read_payload(&hub, &grant, &id, "root", &request)
            .unwrap()
            .bytes,
        b"bounded"
    );
    assert!(matches!(
        registry.read_payload(&hub, &grant, &id, "child", &request),
        Err(ExecutionWatchError::Forbidden)
    ));
    request.observation_id = "foreign".into();
    assert!(matches!(
        registry.read_payload(&hub, &grant, &id, "root", &request),
        Err(ExecutionWatchError::Forbidden)
    ));
    registry.revoke(&grant).unwrap();
    assert!(matches!(
        registry.read_payload(&hub, &grant, &id, "root", &request),
        Err(ExecutionWatchError::Forbidden)
    ));
}

#[tokio::test]
async fn complete_payload_read_keeps_exact_link_and_expiry_fences() {
    let hub = XrayHub::default();
    let registry = ExecutionWatchRegistry::default();
    let grant =
        ExecutionWatchGrant::authorized("test-client".into(), "root".into(), true, true).unwrap();
    let id = registry.open(1, &grant).unwrap();
    let session = SessionId::new();
    admit(&registry, &hub, session, "root", &[]);
    let ready = registry.page(&grant, &id, 0, 8).unwrap();
    let reference = hub
        .capture_payload(
            &XrayCorrelation {
                session_id: session.to_string(),
                run_id: Some("root".into()),
                ..Default::default()
            },
            "text/plain",
            b"bounded bytes",
        )
        .unwrap()
        .unwrap();
    let request = bcode_xray_models::XrayReadPayloadRequest {
        schema_version: 1,
        observation_id: ready.events[0].envelope.observation_id.clone(),
        session_id: session.to_string(),
        payload_id: reference.payload_id,
        offset: 0,
        length: 7,
    };
    let request = bcode_xray_models::execution_watch::ExportExecutionWatchPayloadRequest {
        schema_version: 1,
        watch_id: id,
        run_id: "root".into(),
        payload: request,
    };
    assert!(matches!(
        registry
            .read_payload_complete(&hub, &grant, &request, Some(std::time::Instant::now()),)
            .await,
        Err(ExecutionWatchError::Forbidden)
    ));
    let mut foreign = request.clone();
    foreign.run_id = "child".into();
    assert!(matches!(
        registry
            .read_payload_complete(&hub, &grant, &foreign, None,)
            .await,
        Err(ExecutionWatchError::Forbidden)
    ));
    let range = registry
        .read_payload_complete(&hub, &grant, &request, None)
        .await
        .unwrap();
    assert_eq!(range.bytes, b"bounded");
    registry.revoke(&grant).unwrap();
    assert!(matches!(
        registry
            .read_payload_complete(&hub, &grant, &request, None,)
            .await,
        Err(ExecutionWatchError::Forbidden)
    ));
}

#[test]
fn validates_versions_policy_and_page_bounds_before_capture() {
    let registry = ExecutionWatchRegistry::default();
    let grant = grant("root", true);
    assert_eq!(
        registry.open(2, &grant),
        Err(ExecutionWatchError::InvalidRequest)
    );
    let id = registry.open(1, &grant).unwrap();
    assert!(matches!(
        registry.page(&grant, &id, 0, 0),
        Err(ExecutionWatchError::InvalidRequest)
    ));
    assert!(
        ExecutionWatchRegistry::new(ExecutionWatchPolicy {
            concurrent_watches: 0,
            ..Default::default()
        })
        .is_err()
    );
}
