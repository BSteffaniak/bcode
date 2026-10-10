//! Bounded, registry-discovered settled-turn observation. Applicability belongs to plugins.
use super::{
    CancellationHandle, RuntimeCommandContext, RuntimeWorkKind, RuntimeWorkSpec, RuntimeWorkStatus,
    ServerState, WorkId, finish_registered_runtime_work, process_cancel_turn_command,
    process_steering_message_command, resolve_session_plugin_application_request,
};
use bcode_plugin::PluginInvocationScope;
use bcode_plugin_sdk::settled_turn::{INTERFACE_ID, OP_OBSERVE, ObserverResponse, SettledTurn};
use bcode_plugin_sdk::{ServiceBridgeRequest, ServiceCancellation};
use bcode_session_models::{ModelTurnOutcome, SessionEvent, SessionEventKind};
use std::time::Duration;

/// Normalize both canonical conversational admission representations without product policy.
#[must_use]
pub fn facts(
    trigger: &SessionEvent,
    outcome: ModelTurnOutcome,
    recovering: bool,
) -> Option<SettledTurn> {
    let (turn_id, admission) = match &trigger.kind {
        SessionEventKind::UserMessage { admission, .. } => {
            (super::execution_turn_id(trigger), admission)
        }
        SessionEventKind::SubmissionActivated {
            turn_id, admission, ..
        } => (turn_id.0.clone(), admission),
        _ => return None,
    };
    Some(SettledTurn {
        session_id: trigger.session_id,
        turn_id,
        trigger_sequence: trigger.sequence,
        outcome,
        priority: admission.priority,
        has_origin: admission.origin.is_some(),
        correlated: admission.execution.correlation.is_some(),
        continuity: if recovering {
            bcode_plugin_sdk::settled_turn::ExecutionContinuity::Recovery
        } else {
            bcode_plugin_sdk::settled_turn::ExecutionContinuity::Fresh
        },
        cancellation_requested: false,
    })
}

/// Dispatch only when the actual canonical settlement append succeeded.
/// Failure leaves observers uninvoked; normal cleanup is not user cancellation.
pub async fn dispatch_after_settlement(
    state: &ServerState,
    facts: SettledTurn,
    terminal_persisted: bool,
    parent: &WorkId,
    commands: &mut RuntimeCommandContext<'_>,
) {
    if terminal_persisted {
        dispatch(state, facts, parent, commands).await;
    }
}

/// Dispatch only facts whose terminal event was successfully persisted by the caller.
pub async fn dispatch(
    state: &ServerState,
    facts: SettledTurn,
    parent: &WorkId,
    commands: &mut RuntimeCommandContext<'_>,
) {
    let providers = state
        .plugins
        .registry()
        .service_registry()
        .providers_for(INTERFACE_ID)
        .cloned()
        .unwrap_or_default();
    for provider in providers {
        if !dispatch_one(state, &facts, parent, commands, &provider).await {
            break;
        }
    }
}

async fn dispatch_one(
    state: &ServerState,
    facts: &SettledTurn,
    parent: &WorkId,
    commands: &mut RuntimeCommandContext<'_>,
    provider: &str,
) -> bool {
    let cancellation = ServiceCancellation::default();
    if facts.cancellation_requested {
        cancellation.cancel();
    }
    let work_id = WorkId::new(format!("observer:{}", uuid::Uuid::new_v4()));
    let Ok(ownership) = state
        .sessions
        .acquire_session_ownership(
            facts.session_id,
            bcode_session::SessionOwnershipKind::RuntimeWork,
        )
        .await
    else {
        return true;
    };
    let mut spec = RuntimeWorkSpec::new(
        work_id.clone(),
        RuntimeWorkKind::PluginInvocation,
        "Settled turn observer".into(),
        CancellationHandle::ServiceCall(cancellation.clone()),
    )
    .with_parent_work_id(Some(parent.clone()));
    spec.ownership = Some(ownership);
    let (_, admitted) = state.runtime_work.start(facts.session_id, spec).await;
    if !admitted {
        return true;
    }
    let persisted = state
        .sessions
        .append_runtime_work_started(
            facts.session_id,
            SessionEventKind::RuntimeWorkStarted {
                work_id: work_id.clone(),
                kind: RuntimeWorkKind::PluginInvocation,
                label: "Settled turn observer".into(),
                tool_call_id: None,
                plugin_id: Some(provider.to_owned()),
                service_interface: Some(INTERFACE_ID.into()),
                operation: Some(OP_OBSERVE.into()),
                parent_work_id: Some(parent.clone()),
                started_at_ms: Some(super::current_unix_millis()),
                cancellable: true,
            },
        )
        .await;
    let Ok(event) = persisted else {
        state.runtime_work.finish(facts.session_id, &work_id).await;
        return true;
    };
    super::publish_session_event(state, &event).await;
    let operation = observe(state, provider, facts, &cancellation);
    tokio::pin!(operation);
    let result = loop {
        tokio::select! {
            biased;
            Some(command) = commands.cancel_commands.recv() => {
                cancellation.cancel();
                process_cancel_turn_command(state, facts.session_id, commands.followup_commands,
                    commands.queued_followups, command).await;
            }
            result = &mut operation => break result,
            () = tokio::time::sleep(Duration::from_millis(50)) => {
                while let Ok(command) = commands.steering_commands.try_recv() {
                    process_steering_message_command(state, facts.session_id, command).await;
                }
            }
        }
    };
    let status = if cancellation.is_cancelled() {
        RuntimeWorkStatus::Cancelled
    } else if result.is_ok() {
        RuntimeWorkStatus::Completed
    } else {
        RuntimeWorkStatus::Failed
    };
    finish_registered_runtime_work(state, facts.session_id, work_id, status, None).await;
    !cancellation.is_cancelled()
}

async fn observe(
    state: &ServerState,
    provider: &str,
    facts: &SettledTurn,
    cancellation: &ServiceCancellation,
) -> Result<(), String> {
    let response: ObserverResponse = state
        .plugins
        .invoke_service_json_scoped_cancellable(
            provider,
            INTERFACE_ID,
            OP_OBSERVE,
            facts,
            PluginInvocationScope::session(facts.session_id.to_string()),
            Duration::from_secs(5),
            cancellation,
        )
        .await
        .map_err(|_| "settled observer unavailable")?;
    if let ObserverResponse::Application { request } = response {
        if cancellation.is_cancelled() {
            return Err("settled observer cancelled".into());
        }
        // Session authority comes from this host-created dispatch, never from the response.
        let result = resolve_session_plugin_application_request(
            state,
            Some(facts.session_id),
            ServiceBridgeRequest::InvokeService(request),
            cancellation,
            true,
        )
        .await?;
        if !matches!(
            result,
            bcode_plugin_sdk::ServiceBridgeResponse::Service(
                bcode_tool::ToolInvocationServiceResolution::Responded { .. }
            )
        ) {
            return Err("settled observer application unavailable".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{INTERFACE_ID, ServiceCancellation, SettledTurn, dispatch, facts, observe};
    use bcode_session_models::{
        ClientId, ModelTurnOutcome, SessionEvent, SessionEventKind, SessionId,
        TurnAdmissionMetadata, TurnPriority,
    };
    use std::{
        collections::BTreeSet,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
        time::Duration,
    };
    const MANIFEST: &str = "id='test.observer'\nname='Observer'\nversion='0.0.1'\n[[services]]\ninterface_id='bcode.settled-turn-observer/v1'\nname='Observer'\n[runtime]\ntype='native'\nabi_version=5\nlibrary='unused.dylib'\nevent_symbol='bcode_plugin_handle_event_v1'\n";
    const FAIL_MANIFEST: &str = "id='test.a-failing'\nname='Failing observer'\nversion='0.0.1'\n[[services]]\ninterface_id='bcode.settled-turn-observer/v1'\nname='Observer'\n[runtime]\ntype='native'\nabi_version=5\nlibrary='unused.dylib'\nevent_symbol='bcode_plugin_handle_event_v1'\n";
    static OBSERVER_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    static FAILURES: AtomicUsize = AtomicUsize::new(0);
    #[derive(Default)]
    struct FailingObserver;
    impl bcode_plugin_sdk::RustPlugin for FailingObserver {
        fn invoke_service(
            &mut self,
            _: bcode_plugin_sdk::NativeServiceContext,
        ) -> bcode_plugin_sdk::ServiceResponse {
            FAILURES.fetch_add(1, Ordering::SeqCst);
            bcode_plugin_sdk::ServiceResponse::error("fixture_failure", "observer failed")
        }
    }
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    static STARTED: AtomicBool = AtomicBool::new(false);
    static CANCELLED: AtomicBool = AtomicBool::new(false);
    #[derive(Default)]
    struct FakeObserver;
    impl bcode_plugin_sdk::RustPlugin for FakeObserver {
        fn invoke_service(
            &mut self,
            context: bcode_plugin_sdk::NativeServiceContext,
        ) -> bcode_plugin_sdk::ServiceResponse {
            CALLS.fetch_add(1, Ordering::SeqCst);
            let facts: SettledTurn = context.request.payload_json().unwrap();
            assert_eq!(context.request.interface_id, INTERFACE_ID);
            if facts.turn_id == "wait" {
                STARTED.store(true, Ordering::SeqCst);
                let deadline = std::time::Instant::now() + Duration::from_secs(3);
                while !context.cancellation.is_cancelled() && std::time::Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(5));
                }
                CANCELLED.store(context.cancellation.is_cancelled(), Ordering::SeqCst);
            }
            bcode_plugin_sdk::ServiceResponse::json(
                &bcode_plugin_sdk::settled_turn::ObserverResponse::NoAction,
            )
            .unwrap()
        }
    }
    fn trigger() -> SessionEvent {
        SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            session_id: SessionId::new(),
            sequence: 2,
            timestamp_ms: 0,
            provenance: None,
            kind: SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: "private prompt".into(),
                admission: TurnAdmissionMetadata::default(),
            },
        }
    }
    #[test]
    fn normalization_preserves_outcome_and_provenance_without_content() {
        let mut trigger = trigger();
        let user = facts(&trigger, ModelTurnOutcome::Error, true).unwrap();
        assert_eq!(user.outcome, ModelTurnOutcome::Error);
        assert_eq!(
            user.continuity,
            bcode_plugin_sdk::settled_turn::ExecutionContinuity::Recovery
        );
        assert!(
            !serde_json::to_string(&user)
                .unwrap()
                .contains("private prompt")
        );
        let turn_id = bcode_session_models::TurnId("activated".into());
        trigger.kind = SessionEventKind::SubmissionActivated {
            submission_id: bcode_session_models::SubmissionId::for_queued_turn(&turn_id),
            turn_id,
            client_id: ClientId::new(),
            text: "private prompt".into(),
            admission: TurnAdmissionMetadata::default(),
        };
        let activated = facts(&trigger, ModelTurnOutcome::Completed, false).unwrap();
        assert_eq!(activated.turn_id, "activated");
        assert_eq!(activated.priority, TurnPriority::Interactive);
    }
    async fn dispatch_fixture(state: &crate::ServerState, facts: SettledTurn, persisted: bool) {
        let (_followup, mut followups) = tokio::sync::mpsc::channel(1);
        let (_steering, mut steering) = tokio::sync::mpsc::channel(1);
        let (_cancel, mut cancels) = tokio::sync::mpsc::channel(1);
        let queued = AtomicUsize::new(0);
        let current = std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let mut commands = crate::RuntimeCommandContext::new(
            &mut followups,
            &mut steering,
            &mut cancels,
            &queued,
            current,
        );
        super::dispatch_after_settlement(
            state,
            facts,
            persisted,
            &bcode_session_models::WorkId::new("parent"),
            &mut commands,
        )
        .await;
    }

    #[tokio::test]
    async fn failed_actual_terminal_persistence_gates_dispatch_and_observer_failure_is_isolated() {
        let guard = OBSERVER_TEST_LOCK.lock().await;
        let sessions = bcode_session::SessionManager::default();
        let session = sessions
            .create_session(None, std::env::temp_dir())
            .await
            .unwrap();
        let mut state = crate::tests::test_server_state(sessions);
        state.plugins = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
            &bcode_plugin::PluginSelection {
                mode: bcode_plugin::PluginSelectionMode::Explicit,
                enabled: BTreeSet::from(["test.a-failing".into(), "test.observer".into()]),
                disabled: BTreeSet::new(),
            },
            &[
                bcode_plugin::StaticBundledPlugin::new(
                    FAIL_MANIFEST,
                    bcode_plugin_sdk::static_plugin_vtable!(FailingObserver, FAIL_MANIFEST),
                ),
                bcode_plugin::StaticBundledPlugin::new(
                    MANIFEST,
                    bcode_plugin_sdk::static_plugin_vtable!(FakeObserver, MANIFEST),
                ),
            ],
        )
        .unwrap();
        let missing = trigger();
        let persisted = crate::append_model_turn_finished_event(
            &state,
            missing.session_id,
            "missing".into(),
            ModelTurnOutcome::Completed,
            None,
        )
        .await;
        assert!(
            !persisted,
            "actual canonical append must reject an unavailable session"
        );
        let failed_before = FAILURES.load(Ordering::SeqCst);
        dispatch_fixture(
            &state,
            facts(&missing, ModelTurnOutcome::Completed, false).unwrap(),
            persisted,
        )
        .await;
        assert_eq!(FAILURES.load(Ordering::SeqCst), failed_before);
        let mut existing = trigger();
        existing.session_id = session.id;
        let persisted = crate::append_model_turn_finished_event(
            &state,
            session.id,
            "completed".into(),
            ModelTurnOutcome::Completed,
            None,
        )
        .await;
        assert!(persisted);
        let mut observation = facts(&existing, ModelTurnOutcome::Completed, false).unwrap();
        observation.turn_id = "isolation".into();
        dispatch_fixture(&state, observation, persisted).await;
        assert_eq!(FAILURES.load(Ordering::SeqCst), failed_before + 1);
        let history = state
            .sessions
            .session_history_around(
                session.id,
                bcode_session_models::SessionHistoryAroundQuery {
                    sequence: 1,
                    before: 0,
                    after: 40,
                },
            )
            .await
            .unwrap();
        let statuses = history
            .events
            .iter()
            .filter_map(|event| match &event.kind {
                SessionEventKind::RuntimeWorkFinished { status, .. } => Some(*status),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(statuses.contains(&bcode_session_models::RuntimeWorkStatus::Failed));
        assert!(
            statuses.contains(&bcode_session_models::RuntimeWorkStatus::Completed),
            "later observer must run despite earlier failure"
        );
        drop(state);
        drop(guard);
    }

    async fn assert_missing_ownership_does_not_invoke(
        state: &crate::ServerState,
        facts: SettledTurn,
    ) {
        let (_followup, mut followups) = tokio::sync::mpsc::channel(1);
        let (_steering, mut steering) = tokio::sync::mpsc::channel(1);
        let (_cancel, mut cancels) = tokio::sync::mpsc::channel(1);
        let queued = std::sync::atomic::AtomicUsize::new(0);
        let current = std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let mut commands = crate::RuntimeCommandContext::new(
            &mut followups,
            &mut steering,
            &mut cancels,
            &queued,
            current,
        );
        let before = CALLS.load(Ordering::SeqCst);
        dispatch(
            state,
            facts,
            &bcode_session_models::WorkId::new("parent"),
            &mut commands,
        )
        .await;
        assert_eq!(
            CALLS.load(Ordering::SeqCst),
            before,
            "no observer may execute without canonical session ownership"
        );
    }

    #[tokio::test]
    async fn registry_observer_is_tool_free_and_receives_cancellation() {
        let guard = OBSERVER_TEST_LOCK.lock().await;
        let mut state = crate::tests::test_server_state(bcode_session::SessionManager::default());
        state.plugins = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
            &bcode_plugin::PluginSelection {
                mode: bcode_plugin::PluginSelectionMode::Explicit,
                enabled: BTreeSet::from(["test.observer".into()]),
                disabled: BTreeSet::new(),
            },
            &[bcode_plugin::StaticBundledPlugin::new(
                MANIFEST,
                bcode_plugin_sdk::static_plugin_vtable!(FakeObserver, MANIFEST),
            )],
        )
        .unwrap();
        assert!(
            state
                .plugins
                .registry()
                .service_registry()
                .providers_for(INTERFACE_ID)
                .unwrap()
                .contains("test.observer")
        );
        let mut facts = facts(&trigger(), ModelTurnOutcome::Completed, false).unwrap();
        assert_missing_ownership_does_not_invoke(&state, facts.clone()).await;
        let cancellation = ServiceCancellation::default();
        observe(&state, "test.observer", &facts, &cancellation)
            .await
            .unwrap();
        facts.turn_id = "wait".into();
        let cancel = async {
            tokio::time::timeout(Duration::from_secs(3), async {
                while !STARTED.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            cancellation.cancel();
        };
        let (result, ()) = tokio::join!(
            observe(&state, "test.observer", &facts, &cancellation),
            cancel
        );
        assert!(result.is_err());
        tokio::time::timeout(Duration::from_secs(3), async {
            while !CANCELLED.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        drop(state);
        drop(guard);
    }
    #[tokio::test]
    async fn observer_application_cannot_target_another_session_or_impersonate_command() {
        let state = crate::tests::test_server_state(bcode_session::SessionManager::default());
        let authorized = SessionId::new();
        let other = SessionId::new();
        let request = |session_id| {
            bcode_plugin_sdk::ServiceBridgeRequest::InvokeService(
                bcode_tool::ToolInvocationServiceRequest {
                    invocation_id: "observer".into(),
                    request_id: "request".into(),
                    route_id: None,
                    interface_id: bcode_plugin_sdk::milestones::MILESTONES_HOST_INTERFACE_ID.into(),
                    operation: "request".into(),
                    payload: serde_json::json!({"operation":"evaluate_settled", "session_id":session_id}),
                },
            )
        };
        let cancellation = ServiceCancellation::default();
        assert!(
            crate::resolve_session_plugin_application_request(
                &state,
                Some(authorized),
                request(other),
                &cancellation,
                true
            )
            .await
            .is_err()
        );
        assert!(
            crate::resolve_command_plugin_bridge_request(
                &state,
                Some(authorized),
                request(authorized),
                &cancellation
            )
            .await
            .is_err()
        );
        drop(state);
    }
}
