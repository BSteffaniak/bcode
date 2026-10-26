//! Behavioral coverage for queued-input application recovery.

use crate::submission_recovery::{cancel, prepare, resume, status};
use crate::{
    Arc, ClientId, ClientRuntimeContext, Duration, Path, ServerState, SessionEventKind, SessionId,
    SessionManager, SessionModelSelection, TurnExecutionOptions, live_submission_runtime,
    next_pending_submission_command, submission_execution_descriptor_with_recipe,
    verified_submission_runtime_context,
};
use bcode_session_models::{
    SubmissionQueueBlocker, SubmissionRecord, SubmissionStatus, SubmissionTarget,
};

async fn pending(
    state: &ServerState,
    session_id: SessionId,
    id: &str,
    workflow: bool,
) -> SubmissionRecord {
    let mut admission = bcode_session_models::TurnAdmissionMetadata::default();
    if workflow {
        admission.origin = Some(bcode_session_models::TurnOrigin {
            producer: "bcode.workflow".into(),
            correlation_id: None,
            display_label: None,
        });
    }
    state
        .sessions
        .accept_submission(
            session_id,
            bcode_session_models::PromptSubmission {
                id: bcode_session_models::SubmissionId(id.into()),
                client_id: ClientId::new(),
                text: format!("original {id}"),
                admission,
                requested_placement: bcode_session_models::SubmissionPlacement::FollowUp,
                target: SubmissionTarget::OwnTurn {
                    turn_id: bcode_session_models::TurnId(format!("turn-{id}")),
                },
                execution_descriptor: None,
            },
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn oldest_unresolved_acceptance_is_reported_without_skipping_or_mutation() {
    let sessions = SessionManager::default();
    let session = sessions
        .create_session(None, std::env::current_dir().unwrap())
        .await
        .unwrap();
    let state = crate::tests::test_server_state_with_fake_provider(sessions);
    let first = pending(&state, session.id, "blocked", false).await;
    let second = pending(&state, session.id, "later", false).await;
    let observed = status(&state, session.id).await.unwrap();
    assert_eq!(observed.oldest, Some(first.clone()));
    assert_eq!(
        observed.blocker,
        Some(SubmissionQueueBlocker::MissingContext)
    );
    assert_eq!(
        state
            .sessions
            .submission(session.id, first.receipt.submission_id.clone())
            .await
            .unwrap(),
        Some(first)
    );
    let preserved = state
        .sessions
        .submission(session.id, second.receipt.submission_id.clone())
        .await
        .unwrap();
    drop(state);
    assert_eq!(preserved, Some(second));
}

#[tokio::test]
async fn exact_cancel_advances_blocked_fifo_without_activating_other_input() {
    let sessions = SessionManager::default();
    let session = sessions
        .create_session(None, std::env::current_dir().unwrap())
        .await
        .unwrap();
    let state = Arc::new(crate::tests::test_server_state_with_fake_provider(sessions));
    let first = pending(&state, session.id, "cancel", false).await;
    let second = pending(&state, session.id, "preserved", false).await;
    let result = cancel(
        &state,
        session.id,
        first.receipt.submission_id.clone(),
        first.receipt.accepted_event_sequence,
        "cancel-operation".into(),
    )
    .await
    .unwrap();
    assert!(matches!(
        result.record.status,
        SubmissionStatus::Cancelled { .. }
    ));
    assert_eq!(result.record.submission, first.submission);
    let retry = cancel(
        &state,
        session.id,
        first.receipt.submission_id,
        first.receipt.accepted_event_sequence,
        "cancel-operation".into(),
    )
    .await
    .unwrap();
    assert!(retry.duplicate);
    let observed = status(&state, session.id).await.unwrap().oldest.unwrap();
    assert_eq!(observed.submission, second.submission);
    assert_eq!(observed.status, SubmissionStatus::Pending);
    state.request_shutdown();
}

#[tokio::test]
async fn workflow_blocker_prohibits_prepare_and_cancel_without_mutation() {
    let sessions = SessionManager::default();
    let session = sessions
        .create_session(None, std::env::current_dir().unwrap())
        .await
        .unwrap();
    let state = Arc::new(crate::tests::test_server_state_with_fake_provider(sessions));
    let record = pending(&state, session.id, "workflow", true).await;
    assert_eq!(
        status(&state, session.id).await.unwrap().blocker,
        Some(SubmissionQueueBlocker::WorkflowAuthorityRequired)
    );
    assert!(
        prepare(
            &state,
            session.id,
            ClientId::new(),
            record.receipt.submission_id.clone(),
            record.receipt.accepted_event_sequence
        )
        .await
        .is_err()
    );
    assert!(
        cancel(
            &state,
            session.id,
            record.receipt.submission_id.clone(),
            record.receipt.accepted_event_sequence,
            "forbidden".into()
        )
        .await
        .is_err()
    );
    let preserved = state
        .sessions
        .submission(session.id, record.receipt.submission_id.clone())
        .await
        .unwrap();
    drop(state);
    assert_eq!(preserved, Some(record));
}

#[test]
fn verified_recipe_reconstructs_original_config_not_daemon_defaults() {
    let directory = tempfile::tempdir().unwrap();
    let mut config = bcode_config::BcodeConfig::default();
    config.model.provider_plugin_id = Some("bcode.fake-provider".into());
    config.model.model_id = Some("fake-echo".into());
    let path = directory.path().join("original.toml");
    std::fs::write(
        &path,
        bcode_config::encode_effective_config(&config).unwrap(),
    )
    .unwrap();
    let launch = bcode_config::ConfigLaunchContext {
        recovery_sources: Some(bcode_config::recovery::ConfigRecoverySources {
            cwd: directory.path().to_owned(),
            exclusive_config_path: Some(path.clone()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let selection = SessionModelSelection {
        provider_plugin_id: Some("bcode.fake-provider".into()),
        model_id: Some("fake-echo".into()),
        provider_context: bcode_provider_auth::recovery::resolve(&config, None, None, None)
            .unwrap(),
        ..Default::default()
    };
    let descriptor = submission_execution_descriptor_with_recipe(
        &selection,
        &config,
        &TurnExecutionOptions::default(),
        Some(&launch),
    )
    .unwrap();
    assert_eq!(descriptor.version, 2);
    let restored = verified_submission_runtime_context(&descriptor, directory.path()).unwrap();
    assert_eq!(restored.selected_model_id.as_deref(), Some("fake-echo"));
    assert_eq!(
        bcode_config::decode_effective_config(restored.effective_config_toml.as_deref().unwrap())
            .unwrap(),
        config
    );
    std::fs::write(
        path,
        "[model]\nprovider_plugin_id='different'\nmodel_id='changed'\n",
    )
    .unwrap();
    assert!(verified_submission_runtime_context(&descriptor, directory.path()).is_err());
    let mut future = descriptor;
    future.version += 1;
    assert!(verified_submission_runtime_context(&future, directory.path()).is_err());
}

fn recovery_fixture(
    directory: &Path,
) -> (
    bcode_config::BcodeConfig,
    ClientRuntimeContext,
    bcode_session_models::SubmissionExecutionDescriptor,
) {
    let mut config = bcode_config::BcodeConfig::default();
    config.model.provider_plugin_id = Some("bcode.fake-provider".into());
    config.model.model_id = Some("fake-echo".into());
    // fake-echo has no cache protocol; this fixture exercises queue recovery.
    config.model.prompt_cache.mode = bcode_model::PromptCacheMode::Off;
    config
        .model
        .settings
        .insert("fake_turn_delay_ms".into(), "10".into());
    let path = directory.join("original.toml");
    std::fs::write(
        &path,
        bcode_config::encode_effective_config(&config).unwrap(),
    )
    .unwrap();
    let launch = bcode_config::ConfigLaunchContext {
        recovery_sources: Some(bcode_config::recovery::ConfigRecoverySources {
            cwd: directory.to_owned(),
            exclusive_config_path: Some(path),
            ..Default::default()
        }),
        ..Default::default()
    };
    let context = bcode_provider_auth::recovery::resolve(&config, None, None, None).unwrap();
    let selection = SessionModelSelection {
        provider_plugin_id: Some("bcode.fake-provider".into()),
        model_id: Some("fake-echo".into()),
        provider_context: context.clone(),
        ..Default::default()
    };
    let descriptor = submission_execution_descriptor_with_recipe(
        &selection,
        &config,
        &TurnExecutionOptions::default(),
        Some(&launch),
    )
    .unwrap();
    let runtime = ClientRuntimeContext {
        config_launch: Some(Box::new(launch)),
        working_directory: Some(directory.to_owned()),
        effective_config_toml: Some(Box::new(
            bcode_config::encode_effective_config(&config).unwrap(),
        )),
        selected_provider_plugin_id: selection.provider_plugin_id,
        selected_model_id: selection.model_id,
        provider_context: context,
        ..Default::default()
    };
    (config, runtime, descriptor)
}

async fn wait_for_terminals(
    state: &ServerState,
    session_id: SessionId,
    count: usize,
) -> Vec<bcode_session_models::SessionEvent> {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let history = state.sessions.session_history(session_id).await.unwrap();
            if history
                .iter()
                .filter(|event| matches!(event.kind, SessionEventKind::ModelTurnFinished { .. }))
                .count()
                >= count
            {
                return history;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("queued provider turns terminalize")
}

#[test]
fn persistent_restart_dispatches_two_original_recipes_once_in_fifo_order() {
    run_runtime_test(persistent_restart_dispatches_two_original_recipes_once_in_fifo_order_inner());
}

async fn restarted_fifo_fixture() -> (
    tempfile::TempDir,
    Arc<ServerState>,
    SessionId,
    SubmissionRecord,
    SubmissionRecord,
) {
    let directory = tempfile::tempdir().unwrap();
    let (_config, _runtime, mut descriptor) = recovery_fixture(directory.path());
    descriptor.reasoning = Some(bcode_session_models::TurnReasoningOptions {
        effort: Some("high".into()),
        summary: Some("auto".into()),
    });
    let root = directory.path().join("sessions");
    let original = SessionManager::persistent(&root).unwrap();
    let session = original
        .create_session(None, directory.path().to_owned())
        .await
        .unwrap();
    let initial = crate::tests::test_server_state_with_fake_provider(original);
    let mut first = pending(&initial, session.id, "one", false).await;
    // Create the second through the durable contract with its captured original descriptor.
    let second = initial
        .sessions
        .accept_submission(
            session.id,
            bcode_session_models::PromptSubmission {
                id: bcode_session_models::SubmissionId("two".into()),
                client_id: ClientId::new(),
                text: "original two".into(),
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
                requested_placement: bcode_session_models::SubmissionPlacement::FollowUp,
                target: SubmissionTarget::OwnTurn {
                    turn_id: bcode_session_models::TurnId("turn-two".into()),
                },
                execution_descriptor: Some(descriptor.clone()),
            },
        )
        .await
        .unwrap();
    initial
        .sessions
        .authorize_submission_recovery(
            session.id,
            first.receipt.submission_id.clone(),
            first.receipt.accepted_event_sequence,
            "original-context".into(),
            descriptor,
        )
        .await
        .unwrap();
    first = initial
        .sessions
        .submission(session.id, first.receipt.submission_id.clone())
        .await
        .unwrap()
        .unwrap();
    // A later explicit choice survives restart but cannot retarget accepted input.
    initial
        .sessions
        .append_model_changed(
            session.id,
            "unavailable.sticky-provider".into(),
            "sticky-model-b".into(),
            bcode_session_models::ModelSelectionSource::UserExplicit,
        )
        .await
        .unwrap();
    initial
        .sessions
        .release_idle_session_resources(session.id)
        .await
        .unwrap();
    drop(initial);
    let state = Arc::new(crate::tests::test_server_state_with_fake_provider(
        SessionManager::persistent(&root).unwrap(),
    ));
    assert_eq!(
        state
            .sessions
            .current_runtime_selection(session.id)
            .await
            .unwrap()
            .model_id
            .as_deref(),
        Some("sticky-model-b")
    );
    (directory, state, session.id, first, second)
}

async fn assert_restarted_selection(
    state: &Arc<ServerState>,
    session_id: SessionId,
    first: &SubmissionRecord,
) {
    let mut trigger = state
        .sessions
        .session_events_range(
            session_id,
            first.receipt.accepted_event_sequence,
            first.receipt.accepted_event_sequence,
            1,
        )
        .await
        .unwrap()
        .pop()
        .unwrap();
    trigger.kind = SessionEventKind::SubmissionActivated {
        submission_id: first.receipt.submission_id.clone(),
        turn_id: first.submission.target.turn_id().clone(),
        client_id: first.submission.client_id,
        text: first.submission.text.clone(),
        admission: first.submission.admission.clone(),
    };
    let original = crate::submission_bound_model_selection(state, session_id, &trigger, None)
        .await
        .unwrap();
    assert_eq!(
        original.provider_plugin_id.as_deref(),
        Some("bcode.fake-provider")
    );
    assert_eq!(original.model_id.as_deref(), Some("fake-echo"));
    assert_eq!(original.reasoning_effort.as_deref(), Some("high"));
    assert_eq!(original.reasoning_summary.as_deref(), Some("auto"));
    let verified = crate::resolve_submission_context(
        state,
        session_id,
        first.effective_execution_descriptor().unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(original.provider_context, verified.provider_context);
    let mut conflicting_submission = first.submission.clone();
    conflicting_submission.id = bcode_session_models::SubmissionId("conflicting-admission".into());
    conflicting_submission.target = SubmissionTarget::OwnTurn {
        turn_id: bcode_session_models::TurnId("conflicting-turn".into()),
    };
    conflicting_submission.admission.execution.model_id = Some("immutable-other-model".into());
    conflicting_submission.execution_descriptor = first.effective_execution_descriptor().cloned();
    let conflict_id = conflicting_submission.id.clone();
    state
        .sessions
        .accept_submission(session_id, conflicting_submission.clone())
        .await
        .unwrap();
    let mut conflicting = trigger.clone();
    conflicting.kind = SessionEventKind::SubmissionActivated {
        submission_id: conflict_id.clone(),
        turn_id: conflicting_submission.target.turn_id().clone(),
        client_id: conflicting_submission.client_id,
        text: conflicting_submission.text,
        admission: conflicting_submission.admission,
    };
    assert!(
        crate::submission_bound_model_selection(state, session_id, &conflicting, None)
            .await
            .is_err()
    );
    state
        .sessions
        .cancel_submission(session_id, conflict_id)
        .await
        .unwrap();
}

async fn persistent_restart_dispatches_two_original_recipes_once_in_fifo_order_inner() {
    let (_directory, state, session_id, first, second) = Box::pin(restarted_fifo_fixture()).await;
    assert_restarted_selection(&state, session_id, &first).await;
    let _handle = live_submission_runtime(&state, session_id).await;
    let history = wait_for_terminals(&state, session_id, 2).await;
    let activations = history
        .iter()
        .filter_map(|event| match &event.kind {
            SessionEventKind::SubmissionActivated { submission_id, .. } => {
                Some(submission_id.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        activations,
        vec![first.receipt.submission_id, second.receipt.submission_id]
    );
    assert_eq!(
        history
            .iter()
            .filter(|event| matches!(event.kind, SessionEventKind::ModelTurnStarted { .. }))
            .count(),
        2
    );
    assert!(
        history
            .iter()
            .filter_map(|event| match &event.kind {
                SessionEventKind::ModelTurnFinished { outcome, .. } => Some(outcome),
                _ => None,
            })
            .all(|outcome| outcome == &crate::ModelTurnOutcome::Completed),
        "terminal evidence: {:?}",
        history
            .iter()
            .filter(|event| matches!(event.kind, SessionEventKind::ModelTurnFinished { .. }))
            .collect::<Vec<_>>()
    );
    assert!(status(&state, session_id).await.unwrap().oldest.is_none());
    state.request_shutdown();
}

fn run_runtime_test(future: impl std::future::Future<Output = ()> + Send + 'static) {
    std::thread::Builder::new()
        .name("queued-recovery-lifecycle".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(future);
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn persistent_blocked_head_requires_exact_confirmation_and_duplicate_does_not_reactivate() {
    run_runtime_test(
        persistent_blocked_head_requires_exact_confirmation_and_duplicate_does_not_reactivate_inner(
        ),
    );
}

async fn blocked_head_fixture() -> (
    tempfile::TempDir,
    Arc<ServerState>,
    SessionId,
    ClientId,
    SubmissionRecord,
) {
    let directory = tempfile::tempdir().unwrap();
    let (_config, runtime, descriptor) = recovery_fixture(directory.path());
    let root = directory.path().join("sessions");
    let original = SessionManager::persistent(&root).unwrap();
    let session = original
        .create_session(None, directory.path().to_owned())
        .await
        .unwrap();
    let initial = crate::tests::test_server_state_with_fake_provider(original);
    let first = pending(&initial, session.id, "blocked-head", false).await;
    initial
        .sessions
        .accept_submission(
            session.id,
            bcode_session_models::PromptSubmission {
                id: bcode_session_models::SubmissionId("after-head".into()),
                client_id: ClientId::new(),
                text: "after head".into(),
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
                requested_placement: bcode_session_models::SubmissionPlacement::FollowUp,
                target: SubmissionTarget::OwnTurn {
                    turn_id: bcode_session_models::TurnId("after-head-turn".into()),
                },
                execution_descriptor: Some(descriptor),
            },
        )
        .await
        .unwrap();
    initial
        .sessions
        .release_idle_session_resources(session.id)
        .await
        .unwrap();
    drop(initial);
    let state = Arc::new(crate::tests::test_server_state_with_fake_provider(
        SessionManager::persistent(&root).unwrap(),
    ));
    let client = ClientId::new();
    state
        .set_client_runtime_context(client, Some(runtime))
        .await;
    (directory, state, session.id, client, first)
}

async fn persistent_blocked_head_requires_exact_confirmation_and_duplicate_does_not_reactivate_inner()
 {
    let (_directory, state, session_id, client, first) = Box::pin(blocked_head_fixture()).await;
    let _handle = live_submission_runtime(&state, session_id).await;
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert!(
        !state
            .sessions
            .session_history(session_id)
            .await
            .unwrap()
            .iter()
            .any(|event| matches!(event.kind, SessionEventKind::SubmissionActivated { .. }))
    );
    let confirmation = prepare(
        &state,
        session_id,
        client,
        first.receipt.submission_id.clone(),
        first.receipt.accepted_event_sequence,
    )
    .await
    .unwrap();
    let mut outdated = confirmation.clone();
    outdated.expected_accepted_sequence += 1;
    assert!(
        resume(
            &state,
            session_id,
            client,
            outdated,
            "stale-operation".into()
        )
        .await
        .is_err()
    );
    let result = resume(
        &state,
        session_id,
        client,
        confirmation.clone(),
        "resume-operation".into(),
    )
    .await
    .unwrap();
    assert!(!result.duplicate);
    let history = wait_for_terminals(&state, session_id, 2).await;
    let retry = resume(
        &state,
        session_id,
        client,
        confirmation,
        "resume-operation".into(),
    )
    .await
    .unwrap();
    assert!(retry.duplicate);
    assert_eq!(retry.record, result.record);
    let current = state.sessions.session_history(session_id).await.unwrap();
    assert_eq!(
        history
            .iter()
            .filter(|e| matches!(e.kind, SessionEventKind::SubmissionActivated { .. }))
            .count(),
        2
    );
    assert_eq!(
        current
            .iter()
            .filter(|e| matches!(e.kind, SessionEventKind::SubmissionActivated { .. }))
            .count(),
        2
    );
    state.request_shutdown();
}

async fn orphaned_permission_fixture(
    state: &ServerState,
    session_id: SessionId,
    turn_id: &str,
    directory: &Path,
) {
    let model = bcode_session_models::WorkId::new(format!("model_{turn_id}"));
    state
        .sessions
        .append_event(
            session_id,
            SessionEventKind::RuntimeWorkStarted {
                work_id: model.clone(),
                kind: bcode_session_models::RuntimeWorkKind::ModelTurn,
                label: format!("model turn {turn_id}"),
                parent_work_id: None,
                started_at_ms: Some(1),
                cancellable: true,
                tool_call_id: None,
                plugin_id: None,
                service_interface: None,
                operation: None,
            },
        )
        .await
        .unwrap();
    state
        .sessions
        .append_event(
            session_id,
            SessionEventKind::PositionedToolCallRequested {
                turn_id: turn_id.to_owned(),
                output_position: bcode_session_models::TurnOutputPosition::new(0),
                tool_call_id: "interrupted-call".into(),
                producer_plugin_id: Some("example".into()),
                tool_name: "example.write".into(),
                arguments_json: "{}".into(),
                working_directory: Some(directory.to_owned()),
            },
        )
        .await
        .unwrap();
    state
        .sessions
        .append_event(
            session_id,
            SessionEventKind::RuntimeWorkStarted {
                work_id: bcode_session_models::WorkId::new("tool_interrupted-call"),
                kind: bcode_session_models::RuntimeWorkKind::Tool,
                label: "example.write".into(),
                parent_work_id: Some(model),
                started_at_ms: Some(2),
                cancellable: true,
                tool_call_id: Some("interrupted-call".into()),
                plugin_id: None,
                service_interface: None,
                operation: None,
            },
        )
        .await
        .unwrap();
    state
        .sessions
        .append_permission_requested(
            session_id,
            SessionEventKind::PermissionRequested {
                permission_id: "orphaned-permission".into(),
                tool_call_id: "interrupted-call".into(),
                producer_plugin_id: Some("example".into()),
                tool_name: "example.write".into(),
                arguments_json: "{}".into(),
                batch: None,
                policy_source: None,
                policy_reason: None,
            },
        )
        .await
        .unwrap();
}

async fn assert_no_recovery_mutation(
    state: &Arc<ServerState>,
    session_id: SessionId,
    turn_id: &str,
) {
    let generation = state
        .sessions
        .current_session_generation(session_id)
        .await
        .unwrap();
    assert!(
        !crate::submission_recovery::end_interrupted_turn(state, session_id, turn_id)
            .await
            .unwrap()
    );
    assert_eq!(
        state
            .sessions
            .current_session_generation(session_id)
            .await
            .unwrap(),
        generation
    );
}

#[tokio::test]
async fn interrupted_turn_can_be_explicitly_ended_without_replaying_tools_or_losing_fifo() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("sessions");
    let sessions = SessionManager::persistent(root.clone()).unwrap();
    let session = sessions
        .create_session(None, directory.path().to_owned())
        .await
        .unwrap();
    let state = Arc::new(crate::tests::test_server_state_with_fake_provider(sessions));
    let first = pending(&state, session.id, "orphaned", false).await;
    state
        .sessions
        .activate_submission(session.id, first.receipt.submission_id.clone())
        .await
        .unwrap();
    let turn_id = first.submission.target.turn_id().0.clone();
    orphaned_permission_fixture(&state, session.id, &turn_id, directory.path()).await;
    let later = pending(&state, session.id, "preserved-followup", false).await;
    assert_no_recovery_mutation(&state, session.id, "wrong-turn").await;
    assert!(
        crate::session_operations::cancel_turn(
            &state,
            session.id,
            false,
            ClientId::new(),
            Some(turn_id.clone())
        )
        .await
        .unwrap()
    );
    assert_eq!(
        status(&state, session.id).await.unwrap().oldest,
        Some(later.clone())
    );
    assert!(
        state
            .sessions
            .active_tool_runs(session.id)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        state
            .sessions
            .active_runtime_work(session.id)
            .await
            .unwrap()
            .is_empty()
    );
    let history = state.sessions.session_history(session.id).await.unwrap();
    assert_eq!(
        history
            .iter()
            .filter(|event| matches!(event.kind, SessionEventKind::SubmissionActivated { .. }))
            .count(),
        1
    );
    assert_eq!(history.iter().filter(|event| matches!(&event.kind, SessionEventKind::ToolInvocationResultRecorded { record } if record.invocation_id == "interrupted-call" && record.is_error)).count(), 1);
    assert!(history.iter().any(|event| matches!(&event.kind, SessionEventKind::PermissionResolved { permission_id, approved: false } if permission_id == "orphaned-permission")));
    assert_eq!(history.iter().filter(|event| matches!(&event.kind, SessionEventKind::ModelTurnFinished { turn_id: id, outcome: bcode_session_models::ModelTurnOutcome::Cancelled, .. } if id == &turn_id)).count(), 1);
    assert_no_recovery_mutation(&state, session.id, &turn_id).await;
    state.request_shutdown();
    drop(state);
    let sessions = SessionManager::persistent(root).unwrap();
    let recovered = sessions
        .submission_queue_status(session.id)
        .await
        .unwrap()
        .oldest
        .unwrap();
    assert_eq!(recovered.receipt, later.receipt);
    assert_eq!(recovered.submission, later.submission);
    assert_eq!(recovered.status, SubmissionStatus::Pending);
}

#[tokio::test]
async fn interrupted_recovery_refuses_workflow_and_unrelated_work_without_mutation() {
    for workflow in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let sessions = SessionManager::persistent(directory.path().join("sessions")).unwrap();
        let session = sessions
            .create_session(None, directory.path().to_owned())
            .await
            .unwrap();
        let state = Arc::new(crate::tests::test_server_state_with_fake_provider(sessions));
        let first = pending(&state, session.id, "fenced", workflow).await;
        state
            .sessions
            .activate_submission(session.id, first.receipt.submission_id)
            .await
            .unwrap();
        state
            .sessions
            .append_event(
                session.id,
                SessionEventKind::RuntimeWorkStarted {
                    work_id: bcode_session_models::WorkId::new("unrelated"),
                    kind: bcode_session_models::RuntimeWorkKind::ModelTurn,
                    label: "unrelated work".into(),
                    parent_work_id: None,
                    started_at_ms: Some(1),
                    cancellable: true,
                    tool_call_id: None,
                    plugin_id: None,
                    service_interface: None,
                    operation: None,
                },
            )
            .await
            .unwrap();
        let generation = state
            .sessions
            .current_session_generation(session.id)
            .await
            .unwrap();
        let result = crate::submission_recovery::end_interrupted_turn(
            &state,
            session.id,
            &first.submission.target.turn_id().0,
        )
        .await;
        if workflow {
            assert!(!result.unwrap());
        } else {
            assert!(result.is_err());
        }
        let observed_generation = state
            .sessions
            .current_session_generation(session.id)
            .await
            .unwrap();
        drop(state);
        assert_eq!(observed_generation, generation);
    }
}

#[tokio::test]
async fn interrupted_recovery_refuses_live_execution_and_incomplete_invocation_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let sessions = SessionManager::persistent(directory.path().join("sessions")).unwrap();
    let session = sessions
        .create_session(None, directory.path().to_owned())
        .await
        .unwrap();
    let state = Arc::new(crate::tests::test_server_state_with_fake_provider(sessions));
    let first = pending(&state, session.id, "not-safe", false).await;
    state
        .sessions
        .activate_submission(session.id, first.receipt.submission_id)
        .await
        .unwrap();
    let turn_id = first.submission.target.turn_id().0.clone();
    let runtime = live_submission_runtime(&state, session.id).await;
    *runtime.phase.lock().await = crate::SessionRuntimePhase::PreparingModelRequest;
    let generation = state
        .sessions
        .current_session_generation(session.id)
        .await
        .unwrap();
    assert!(
        crate::submission_recovery::end_interrupted_turn(&state, session.id, &turn_id)
            .await
            .is_err()
    );
    assert_eq!(
        state
            .sessions
            .current_session_generation(session.id)
            .await
            .unwrap(),
        generation
    );
    *runtime.phase.lock().await = crate::SessionRuntimePhase::Idle;
    state
        .sessions
        .append_event(
            session.id,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "unattributed".into(),
                tool_name: "example.write".into(),
                arguments_json: "{}".into(),
                producer_plugin_id: None,
                working_directory: None,
            },
        )
        .await
        .unwrap();
    let generation = state
        .sessions
        .current_session_generation(session.id)
        .await
        .unwrap();
    assert!(
        crate::submission_recovery::end_interrupted_turn(&state, session.id, &turn_id)
            .await
            .is_err()
    );
    assert_eq!(
        state
            .sessions
            .current_session_generation(session.id)
            .await
            .unwrap(),
        generation
    );
    // Recovery appends cannot silently rebase across unrelated canonical changes.
    assert!(
        state
            .sessions
            .append_event_at_generation(
                session.id,
                SessionEventKind::SystemMessage {
                    text: "must not append".into()
                },
                generation - 1
            )
            .await
            .is_err()
    );
    assert_eq!(
        state
            .sessions
            .current_session_generation(session.id)
            .await
            .unwrap(),
        generation
    );
    state.request_shutdown();
}

#[tokio::test]
async fn interrupted_recovery_work_is_bounded_and_overflow_does_not_mutate() {
    let directory = tempfile::tempdir().unwrap();
    let sessions = SessionManager::persistent(directory.path().join("sessions")).unwrap();
    let session = sessions
        .create_session(None, directory.path().to_owned())
        .await
        .unwrap();
    let state = Arc::new(crate::tests::test_server_state_with_fake_provider(sessions));
    let first = pending(&state, session.id, "overflow", false).await;
    state
        .sessions
        .activate_submission(session.id, first.receipt.submission_id)
        .await
        .unwrap();
    let turn_id = first.submission.target.turn_id().0.clone();
    for index in 0..140 {
        state
            .sessions
            .append_event(
                session.id,
                SessionEventKind::PositionedToolCallRequested {
                    turn_id: turn_id.clone(),
                    output_position: bcode_session_models::TurnOutputPosition::new(index),
                    tool_call_id: format!("call-{index}"),
                    producer_plugin_id: Some("example".into()),
                    tool_name: "example.write".into(),
                    arguments_json: "{}".into(),
                    working_directory: Some(directory.path().to_owned()),
                },
            )
            .await
            .unwrap();
    }
    assert_eq!(
        state
            .sessions
            .interrupted_recovery_work(session.id)
            .await
            .unwrap()
            .tools
            .len(),
        129
    );
    let generation = state
        .sessions
        .current_session_generation(session.id)
        .await
        .unwrap();
    assert!(
        crate::submission_recovery::end_interrupted_turn(&state, session.id, &turn_id)
            .await
            .is_err()
    );
    let observed_generation = state
        .sessions
        .current_session_generation(session.id)
        .await
        .unwrap();
    drop(state);
    assert_eq!(observed_generation, generation);
}

#[tokio::test]
async fn activated_crash_is_a_blocker_not_permission_to_replay_or_run_later_input() {
    let directory = tempfile::tempdir().unwrap();
    let sessions = SessionManager::persistent(directory.path().join("sessions")).unwrap();
    let session = sessions
        .create_session(None, directory.path().to_owned())
        .await
        .unwrap();
    let state = Arc::new(crate::tests::test_server_state_with_fake_provider(sessions));
    let first = pending(&state, session.id, "activated-before-provider", false).await;
    state
        .sessions
        .activate_submission(session.id, first.receipt.submission_id.clone())
        .await
        .unwrap();
    let later = pending(&state, session.id, "must-not-pass", false).await;
    assert_eq!(
        status(&state, session.id).await.unwrap().blocker,
        Some(SubmissionQueueBlocker::InterruptedTurn)
    );
    assert!(
        next_pending_submission_command(&state, session.id, &mut None, &mut Vec::new())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        cancel(
            &state,
            session.id,
            first.receipt.submission_id.clone(),
            first.receipt.accepted_event_sequence,
            "cannot-cancel-activated".into()
        )
        .await
        .is_err()
    );
    assert_eq!(
        state
            .sessions
            .submission(session.id, later.receipt.submission_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        SubmissionStatus::Pending
    );
    assert!(
        !state
            .sessions
            .session_history(session.id)
            .await
            .unwrap()
            .iter()
            .any(|event| matches!(event.kind, SessionEventKind::ModelTurnStarted { .. }))
    );
    drop(state);
}
