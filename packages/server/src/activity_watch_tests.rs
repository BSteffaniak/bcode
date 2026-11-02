//! Behavior of bounded non-owning hosted activity watches.
use super::*;
use bcode_session_models::{
    RuntimeWorkKind, RuntimeWorkStatus, SessionActivityQuery, SessionActivityScope,
    SessionActivityState as State, SessionActivityTarget, WorkId,
};
use std::sync::atomic::Ordering;
use tokio::time::{Duration, timeout};

async fn fixture() -> (Arc<ServerState>, SessionId, ClientId) {
    let state = Arc::new(crate::tests::test_server_state(
        bcode_session::SessionManager::default(),
    ));
    let summary = state
        .sessions
        .create_session(None, std::env::current_dir().unwrap())
        .await
        .unwrap();
    (state, summary.id, ClientId::new())
}
fn scope(id: SessionId) -> Scope {
    Scope {
        activity: vec![SessionActivityTarget {
            session_id: id,
            query: SessionActivityQuery {
                scope: SessionActivityScope::Own,
                after: None,
                limit: 1,
            },
        }],
        children: vec![],
    }
}
async fn pull(state: &ServerState, owner: ClientId, handle: Handle) -> Delivery {
    timeout(
        Duration::from_secs(2),
        state.activity_watches.pull(owner, handle),
    )
    .await
    .unwrap()
    .unwrap()
}
async fn settle(state: &ServerState, count: usize) {
    timeout(Duration::from_secs(2), async {
        while state.activity_watches.computations.load(Ordering::Relaxed) < count {
            tokio::task::yield_now().await;
        }
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
fn start_work(
    state: &ServerState,
    id: SessionId,
) -> impl std::future::Future<Output = (bool, bool)> + '_ {
    state.runtime_work.start(
        id,
        crate::runtime_work::RuntimeWorkSpec::new(
            WorkId::new("watch-work"),
            RuntimeWorkKind::Tool,
            "watch work".into(),
            crate::runtime_work::CancellationHandle::Test(Arc::new(
                std::sync::atomic::AtomicUsize::new(0),
            )),
        ),
    )
}

#[tokio::test]
async fn fifty_distinct_client_scopes_are_admitted_and_idle() {
    let (state, _, _) = fixture().await;
    let mut watches = Vec::new();
    for _ in 0..50 {
        let owner = ClientId::new();
        let handle = state
            .activity_watches
            .open(&state, owner, scope(SessionId::new()))
            .unwrap();
        watches.push((owner, handle));
    }
    for &(owner, handle) in &watches {
        assert_eq!(pull(&state, owner, handle).await.update, Update::Initial);
    }
    let initial = state.activity_watches.computations.load(Ordering::Relaxed);
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(
        state.activity_watches.computations.load(Ordering::Relaxed),
        initial
    );
    for (owner, handle) in watches {
        assert!(state.activity_watches.close(owner, handle));
    }
}

#[tokio::test]
async fn subscribe_before_initial_read_captures_racing_durable_mutation() {
    let (state, id, owner) = fixture().await;
    let handle = state
        .activity_watches
        .open(&state, owner, scope(id))
        .unwrap();
    state
        .sessions
        .append_runtime_work_finished(
            id,
            WorkId::new("failed"),
            RuntimeWorkStatus::Failed,
            None,
            None,
        )
        .await
        .unwrap();
    let mut delivery = pull(&state, owner, handle).await;
    if !delivery.snapshot.activity[0]
        .as_ref()
        .unwrap()
        .own
        .states
        .contains(&State::Failed)
    {
        delivery = pull(&state, owner, handle).await;
    }
    assert!(
        delivery.snapshot.activity[0]
            .as_ref()
            .unwrap()
            .own
            .states
            .contains(&State::Failed)
    );
    assert!(
        delivery.snapshot.activity[0]
            .as_ref()
            .unwrap()
            .own
            .states
            .contains(&State::Unknown)
    );
    assert!(state.session_runtimes.lock().await.is_empty());
}

#[tokio::test]
async fn fifty_subscribers_share_initial_projection_and_do_zero_idle_recompute() {
    let (state, id, owner) = fixture().await;
    let handles: Vec<_> = (0..50)
        .map(|_| {
            state
                .activity_watches
                .open(&state, owner, scope(id))
                .unwrap()
        })
        .collect();
    for handle in &handles {
        let initial = pull(&state, owner, *handle).await;
        assert_eq!(initial.sequence, 0);
        assert_eq!(initial.update, Update::Initial);
    }
    settle(&state, 1).await;
    let count = state.activity_watches.computations.load(Ordering::Relaxed);
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        state.activity_watches.computations.load(Ordering::Relaxed),
        count
    );
    assert_eq!(state.activity_watches.inner.lock().unwrap().groups.len(), 1);
    // A wake without semantic change (including observed_at) cannot deliver an update.
    state.activity_changes.notify();
    settle(&state, count + 1).await;
    let registry = state.activity_watches.inner.lock().unwrap();
    for entry in registry.entries.values() {
        assert_eq!(
            entry
                .group
                .receiver
                .borrow()
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap()
                .revision,
            entry.last_revision.unwrap()
        );
    }
    drop(registry);
    state.activity_watches.remove_client(owner);
    assert!(
        state
            .activity_watches
            .inner
            .lock()
            .unwrap()
            .entries
            .is_empty()
    );
}

#[tokio::test]
async fn slow_consumer_gets_explicit_reset_and_full_bounded_replacement() {
    let (state, id, owner) = fixture().await;
    let fast = state
        .activity_watches
        .open(&state, owner, scope(id))
        .unwrap();
    let slow = state
        .activity_watches
        .open(&state, owner, scope(id))
        .unwrap();
    pull(&state, owner, fast).await;
    pull(&state, owner, slow).await;
    start_work(&state, id).await;
    let running = pull(&state, owner, fast).await;
    assert!(
        running.snapshot.activity[0]
            .as_ref()
            .unwrap()
            .own
            .states
            .contains(&State::Running)
    );
    state
        .runtime_work
        .finish(id, &WorkId::new("watch-work"))
        .await;
    pull(&state, owner, fast).await;
    let reset = pull(&state, owner, slow).await;
    assert_eq!(reset.update, Update::Reset);
    assert_eq!(reset.sequence, 1);
    assert!(
        !reset.snapshot.activity[0]
            .as_ref()
            .unwrap()
            .own
            .states
            .contains(&State::Running)
    );
    assert_eq!(reset.snapshot.activity.len(), 1);
}

#[tokio::test]
async fn bounded_scope_admission_and_close_drop_disconnect_release_only_observation() {
    let (state, id, owner) = fixture().await;
    let mut invalid = scope(id);
    invalid.activity[0].query.limit = usize::MAX;
    assert!(state.activity_watches.open(&state, owner, invalid).is_err());
    let mut invalid = scope(id);
    invalid.children = vec![
        bcode_session_models::SessionChildQuery {
            parent_id: id,
            after: None,
            limit: 128
        };
        5
    ];
    assert!(state.activity_watches.open(&state, owner, invalid).is_err());
    let handle = state
        .activity_watches
        .open(&state, owner, scope(id))
        .unwrap();
    pull(&state, owner, handle).await;
    assert!(!state.activity_watches.close(ClientId::new(), handle));
    let mut waiting = Box::pin(state.activity_watches.pull(owner, handle));
    tokio::select! { biased; result = &mut waiting => panic!("idle watch returned {result:?}"), () = tokio::task::yield_now() => {} }
    drop(waiting);
    assert!(
        state
            .activity_watches
            .inner
            .lock()
            .unwrap()
            .entries
            .is_empty()
    );
    let next = state
        .activity_watches
        .open(&state, owner, scope(id))
        .unwrap();
    assert_ne!(handle.epoch, next.epoch);
    start_work(&state, id).await;
    assert!(state.activity_watches.close(owner, next));
    assert_eq!(
        state.runtime_work.activity_count_for_session(id, 1).await.0,
        1
    );
    let next = state
        .activity_watches
        .open(&state, owner, scope(id))
        .unwrap();
    state.activity_watches.remove_client(owner);
    assert!(state.activity_watches.pull(owner, next).await.is_err());
    assert_eq!(
        state.runtime_work.activity_count_for_session(id, 1).await.0,
        1
    );
}

#[tokio::test]
async fn source_lag_invalidates_even_an_equal_snapshot_without_replay() {
    let (state, id, _) = fixture().await;
    let event = state
        .sessions
        .append_runtime_work_finished(
            id,
            WorkId::new("terminal"),
            RuntimeWorkStatus::Failed,
            None,
            None,
        )
        .await
        .unwrap();
    let summary = state.sessions.session_summary_non_owning(id).await.unwrap();
    let (source, mutations) = tokio::sync::broadcast::channel(1);
    let (sender, mut receiver) = watch::channel(None);
    let worker = tokio::spawn(project(
        Arc::downgrade(&state),
        scope(id),
        sender,
        ActivitySources {
            mutations,
            changes: state.activity_changes.subscribe(),
            resident_changes: state.sessions.subscribe_activity_changes(),
            shutdown: state.subscribe_shutdown(),
        },
        Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    ));
    receiver.changed().await.unwrap();
    let first = receiver.borrow_and_update().clone().unwrap().unwrap();
    for _ in 0..8 {
        source
            .send(bcode_session::SessionMutationCommitted {
                session_id: id,
                event: event.clone(),
                summary: summary.clone(),
            })
            .unwrap();
    }
    receiver.changed().await.unwrap();
    let next = receiver.borrow_and_update().clone().unwrap().unwrap();
    assert!(next.reset > first.reset);
    assert_eq!(next.revision, first.revision + 1);
    assert!(semantic_equal(&first.snapshot, &next.snapshot));
    worker.abort();
}

#[tokio::test]
async fn scheduler_queue_phase_and_attention_contributors_are_change_driven() {
    let (state, id, owner) = fixture().await;
    let handle = state
        .activity_watches
        .open(&state, owner, scope(id))
        .unwrap();
    pull(&state, owner, handle).await;
    let queue = Arc::new(crate::activity_changes::ActivityCounter::observed(
        0,
        state.activity_changes.clone(),
    ));
    let phase = Arc::new(crate::activity_changes::ActivityMutex::observed(
        crate::SessionRuntimePhase::Idle,
        state.activity_changes.clone(),
    ));
    state.session_runtimes.lock().await.insert(
        id,
        crate::SessionRuntimeHandle {
            followup_commands: tokio::sync::mpsc::channel(1).0,
            steering_commands: tokio::sync::mpsc::channel(1).0,
            cancel_commands: tokio::sync::mpsc::channel(1).0,
            queued_followups: queue.clone(),
            queued_steering: Arc::new(crate::activity_changes::ActivityCounter::observed(
                0,
                state.activity_changes.clone(),
            )),
            phase: phase.clone(),
            current_turn: Arc::new(tokio::sync::Mutex::new(None)),
        },
    );
    pull(&state, owner, handle).await;
    queue.fetch_add(1, Ordering::AcqRel);
    assert!(
        pull(&state, owner, handle).await.snapshot.activity[0]
            .as_ref()
            .unwrap()
            .own
            .states
            .contains(&State::Queued)
    );
    *phase.lock().await = crate::SessionRuntimePhase::ProviderActive;
    assert!(
        pull(&state, owner, handle).await.snapshot.activity[0]
            .as_ref()
            .unwrap()
            .own
            .states
            .contains(&State::Running)
    );
    assert_attention_contributors_are_change_driven(state, id, owner, handle).await;
}

async fn assert_attention_contributors_are_change_driven(
    state: Arc<ServerState>,
    id: SessionId,
    owner: ClientId,
    handle: Handle,
) {
    state
        .sessions
        .append_permission_requested(
            id,
            bcode_session_models::SessionEventKind::PermissionRequested {
                permission_id: "permission".into(),
                tool_call_id: "tool".into(),
                producer_plugin_id: None,
                tool_name: "test".into(),
                arguments_json: "{}".into(),
                batch: None,
                policy_source: None,
                policy_reason: None,
            },
        )
        .await
        .unwrap();
    let attention = pull(&state, owner, handle).await;
    assert!(
        attention.snapshot.activity[0]
            .as_ref()
            .unwrap()
            .own
            .states
            .contains(&State::PermissionWait)
    );
    state
        .sessions
        .append_permission_resolved(id, "permission".into(), false)
        .await
        .unwrap();
    let resolved = pull(&state, owner, handle).await;
    assert!(
        !resolved.snapshot.activity[0]
            .as_ref()
            .unwrap()
            .own
            .states
            .contains(&State::PermissionWait)
    );
    // Input attention is independent of permission attention and may coexist with running.
    state
        .sessions
        .append_event(
            id,
            bcode_session_models::SessionEventKind::ToolExchangeRequested {
                request: bcode_session_models::ToolExchangeRequest {
                    invocation_id: "tool".into(),
                    exchange_id: "exchange".into(),
                    producer_id: "test".into(),
                    schema: "test".into(),
                    schema_version: 1,
                    payload: serde_json::json!({}),
                    response_policy: bcode_session_models::ToolExchangeResponsePolicy::Required,
                },
            },
        )
        .await
        .unwrap();
    assert!(
        pull(&state, owner, handle).await.snapshot.activity[0]
            .as_ref()
            .unwrap()
            .own
            .states
            .contains(&State::InputWait)
    );
}

#[tokio::test]
async fn loaded_child_window_tracks_relationship_and_metadata_without_recursive_expansion() {
    let root = tempfile::tempdir().unwrap();
    let sessions = bcode_session::SessionManager::persistent(root.path()).unwrap();
    let parent = sessions
        .create_session(None, root.path().to_path_buf())
        .await
        .unwrap();
    let state = Arc::new(crate::tests::test_server_state(sessions));
    let owner = ClientId::new();
    let scope = Scope {
        activity: vec![],
        children: vec![bcode_session_models::SessionChildQuery {
            parent_id: parent.id,
            after: None,
            limit: 1,
        }],
    };
    let handle = state.activity_watches.open(&state, owner, scope).unwrap();
    assert_eq!(
        pull(&state, owner, handle).await.snapshot.children[0]
            .as_ref()
            .unwrap()
            .children,
        [] as [bcode_session_models::SessionSummary; 0]
    );
    let child = Box::pin(state.sessions.create_child_session(
        bcode_session_models::SessionChildCreateRequest {
            child_id: SessionId::new(),
            parent_id: parent.id,
            name: Some("child".into()),
            working_directory: root.path().to_path_buf(),
        },
    ))
    .await
    .unwrap();
    // Publication can expose an explicit sparse/incomplete intermediate window.
    loop {
        let update = pull(&state, owner, handle).await;
        let page = update.snapshot.children[0].as_ref().unwrap();
        assert!(page.children.len() <= 1);
        if page.children.iter().any(|summary| summary.id == child.id) {
            break;
        }
    }
    state
        .sessions
        .rename_session(child.id, Some("renamed".into()))
        .await
        .unwrap();
    loop {
        let update = pull(&state, owner, handle).await;
        let page = update.snapshot.children[0].as_ref().unwrap();
        if page
            .children
            .iter()
            .any(|summary| summary.name.as_deref() == Some("renamed"))
        {
            break;
        }
    }
    assert!(state.session_runtimes.lock().await.is_empty());
    state.activity_watches.close(owner, handle);
}

#[tokio::test]
async fn unrelated_session_mutations_do_not_recompute_an_idle_scope() {
    let (state, id, owner) = fixture().await;
    let handle = state
        .activity_watches
        .open(&state, owner, scope(id))
        .unwrap();
    pull(&state, owner, handle).await;
    let count = state.activity_watches.computations.load(Ordering::Relaxed);
    let unrelated = state
        .sessions
        .create_session(None, std::env::current_dir().unwrap())
        .await
        .unwrap();
    state
        .sessions
        .append_runtime_work_finished(
            unrelated.id,
            WorkId::new("other"),
            RuntimeWorkStatus::Failed,
            None,
            None,
        )
        .await
        .unwrap();
    for _ in 0..30 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        state.activity_watches.computations.load(Ordering::Relaxed),
        count
    );
}

#[tokio::test]
async fn evidence_release_wakes_existing_watch_and_preserves_unknown() {
    let (state, id, owner) = fixture().await;
    state
        .sessions
        .append_runtime_work_finished(
            id,
            WorkId::new("failed"),
            RuntimeWorkStatus::Failed,
            None,
            None,
        )
        .await
        .unwrap();
    let handle = state
        .activity_watches
        .open(&state, owner, scope(id))
        .unwrap();
    assert!(
        pull(&state, owner, handle).await.snapshot.activity[0]
            .as_ref()
            .unwrap()
            .own
            .states
            .contains(&State::Failed)
    );
    // Evidence release must invalidate an existing view, independently of canonical mutations.
    // A synthetic cache exercises the domain hook without opening another database.
    let mut cache = crate::session_activity::ActivityObservations::default()
        .with_changes(state.activity_changes.clone());
    let event = state
        .sessions
        .append_event(
            id,
            bcode_session_models::SessionEventKind::AssistantDelta {
                text: String::new(),
            },
        )
        .await
        .unwrap();
    let mut seeded = event.clone();
    seeded.sequence = 0;
    seeded.kind = bcode_session_models::SessionEventKind::SessionCreated {
        name: None,
        working_directory: std::env::current_dir().unwrap(),
    };
    cache.observe(&seeded);
    seeded.sequence = 1;
    seeded.kind = bcode_session_models::SessionEventKind::PermissionRequested {
        permission_id: "evicted".into(),
        tool_call_id: "tool".into(),
        producer_plugin_id: None,
        tool_name: "test".into(),
        arguments_json: "{}".into(),
        batch: None,
        policy_source: None,
        policy_reason: None,
    };
    cache.observe(&seeded);
    *state.activity_observations.lock().await = cache;
    state.activity_changes.for_session(id).notify();
    pull(&state, owner, handle).await;
    state.activity_observations.lock().await.release(id);
    let degraded = pull(&state, owner, handle).await;
    assert!(
        !degraded.snapshot.activity[0]
            .as_ref()
            .unwrap()
            .own
            .evidence
            .as_ref()
            .unwrap()
            .counts_complete
    );
    assert!(
        degraded.snapshot.activity[0]
            .as_ref()
            .unwrap()
            .own
            .states
            .contains(&State::Unknown)
    );
}

#[tokio::test]
async fn reply_admission_stays_bounded_until_send_owners_drop() {
    let registry = ActivityWatchRegistry::default();
    let permits: Vec<_> = (0..MAX_HANDLES)
        .map(|_| registry.reserve_reply().unwrap())
        .collect();
    assert!(registry.reserve_reply().is_err());
    drop(permits);
    assert!(registry.reserve_reply().is_ok());
}

#[tokio::test]
async fn handle_capacity_releases_immediately_on_disconnect() {
    let (state, id, owner) = fixture().await;
    for _ in 0..MAX_HANDLES {
        state
            .activity_watches
            .open(&state, owner, scope(id))
            .unwrap();
    }
    assert!(
        state
            .activity_watches
            .open(&state, owner, scope(id))
            .is_err()
    );
    state.activity_watches.remove_client(owner);
    assert!(
        state
            .activity_watches
            .open(&state, owner, scope(id))
            .is_ok()
    );
}

#[tokio::test]
async fn timestamp_and_checkpoint_only_changes_are_not_semantic_updates() {
    let (state, id, _) = fixture().await;
    let first = snapshot(&state, &scope(id)).await;
    let mut changed = first.clone();
    let page = changed.activity[0].as_mut().unwrap();
    page.observed_at_ms = page.observed_at_ms.wrapping_add(1);
    page.own.evidence.as_mut().unwrap().observed_event_sequence = Some(100);
    assert!(semantic_equal(&first, &changed));
    changed.activity[0]
        .as_mut()
        .unwrap()
        .own
        .states
        .push(State::PermissionWait);
    assert!(!semantic_equal(&first, &changed));
}

#[tokio::test]
async fn runtime_progress_and_cancel_publish_scoped_notifications_and_finish_keeps_terminal_truth()
{
    let (state, id, owner) = fixture().await;
    let handle = state
        .activity_watches
        .open(&state, owner, scope(id))
        .unwrap();
    pull(&state, owner, handle).await;
    let mut changes = state.activity_changes.subscribe();
    start_work(&state, id).await;
    assert_eq!(changes.recv().await.unwrap(), Some(id));
    pull(&state, owner, handle).await;
    let token = state
        .runtime_work
        .observation_token(id, &WorkId::new("watch-work"))
        .await
        .unwrap();
    state
        .runtime_work
        .observe_stage(&token, crate::runtime_work::ObservedWorkStage::Dispatch)
        .await
        .unwrap();
    assert_eq!(changes.recv().await.unwrap(), Some(id));
    state
        .runtime_work
        .cancel_with_children(id, &WorkId::new("watch-work"))
        .await;
    assert_eq!(changes.recv().await.unwrap(), Some(id));
    state
        .sessions
        .append_runtime_work_finished(
            id,
            WorkId::new("watch-work"),
            RuntimeWorkStatus::Cancelled,
            None,
            None,
        )
        .await
        .unwrap();
    state
        .runtime_work
        .finish(id, &WorkId::new("watch-work"))
        .await;
    let mut final_state = pull(&state, owner, handle).await;
    if final_state.snapshot.activity[0]
        .as_ref()
        .unwrap()
        .own
        .states
        .contains(&State::Running)
    {
        final_state = pull(&state, owner, handle).await;
    }
    assert!(
        final_state.snapshot.activity[0]
            .as_ref()
            .unwrap()
            .own
            .states
            .contains(&State::Interrupted)
    );
}

#[tokio::test]
async fn streamed_text_and_usage_do_not_reproject_selected_or_unrelated_activity() {
    let (state, id, owner) = fixture().await;
    let other = state
        .sessions
        .create_session(None, std::env::current_dir().unwrap())
        .await
        .unwrap();
    let handle = state
        .activity_watches
        .open(&state, owner, scope(id))
        .unwrap();
    pull(&state, owner, handle).await;
    let count = state.activity_watches.computations.load(Ordering::Relaxed);
    for session in [id, other.id] {
        for _ in 0..4 {
            state
                .sessions
                .append_assistant_delta(session, "streaming".into())
                .await
                .unwrap();
            state
                .sessions
                .append_event(
                    session,
                    bcode_session_models::SessionEventKind::ModelUsage {
                        turn_id: "turn".into(),
                        usage: bcode_session_models::SessionTokenUsage::default(),
                    },
                )
                .await
                .unwrap();
        }
    }
    for _ in 0..30 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        state.activity_watches.computations.load(Ordering::Relaxed),
        count
    );
}

#[tokio::test]
async fn unrelated_runtime_register_progress_cancel_finish_do_not_reproject() {
    let (state, id, owner) = fixture().await;
    let other = state
        .sessions
        .create_session(None, std::env::current_dir().unwrap())
        .await
        .unwrap();
    let handle = state
        .activity_watches
        .open(&state, owner, scope(id))
        .unwrap();
    pull(&state, owner, handle).await;
    let count = state.activity_watches.computations.load(Ordering::Relaxed);
    start_work(&state, other.id).await;
    let token = state
        .runtime_work
        .observation_token(other.id, &WorkId::new("watch-work"))
        .await
        .unwrap();
    state
        .runtime_work
        .observe_stage(&token, crate::runtime_work::ObservedWorkStage::Dispatch)
        .await
        .unwrap();
    state
        .runtime_work
        .cancel_with_children(other.id, &WorkId::new("watch-work"))
        .await;
    state
        .runtime_work
        .finish(other.id, &WorkId::new("watch-work"))
        .await;
    for _ in 0..30 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        state.activity_watches.computations.load(Ordering::Relaxed),
        count
    );
}

#[tokio::test]
async fn mutable_bindings_read_queries_do_not_publish_activity_invalidations() {
    let changes = crate::activity_changes::ActivityChanges::default();
    let id = SessionId::new();
    let values = crate::activity_changes::ActivityMutex::observed(
        BTreeMap::from([(id, 1)]),
        changes.clone(),
    );
    let mut notifications = changes.subscribe();
    // Merely binding the guard mutably must not call DerefMut or publish a wake.
    let mut guard = values.lock_for(id).await;
    assert_eq!(guard.get(&id), Some(&1));
    assert_eq!(guard.values().copied().sum::<usize>(), 1);
    assert!(notifications.try_recv().is_err());
    guard.insert(id, 2);
    drop(guard);
    assert_eq!(notifications.try_recv().unwrap(), Some(id));
    let guard = values.lock().await;
    assert_eq!(guard.get(&id), Some(&2));
    drop(guard);
    assert!(notifications.try_recv().is_err());
}
