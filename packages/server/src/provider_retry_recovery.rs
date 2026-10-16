//! Bounded, canonical checkpoints for safe provider-only retry recovery.
use super::*;

const CHECKPOINT_EVENT: &str = bcode_session_models::PROVIDER_RETRY_CHECKPOINT_EVENT;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct RetryCheckpoint {
    version: u32,
    activation_sequence: u64,
    resume_at_ms: u64,
    consecutive_attempts: u64,
    attempts: BTreeMap<String, u64>,
    provider_round: u32,
    completed_tool_rounds: u32,
    segment_order: u32,
    output_position: u64,
    recovery_flags: RetryRecoveryFlags,
}

#[derive(Clone, serde::Serialize, serde::Deserialize, Default)]
struct RetryRecoveryFlags {
    context_pressure: bool,
    malformed_arguments: bool,
    max_tokens_continuations: u32,
    finalization: bool,
    corrections: u32,
    correction_error: Option<String>,
    instruction: Option<u8>,
}

async fn load(
    state: &ServerState,
    session_id: SessionId,
    turn_id: &str,
) -> Result<Option<RetryCheckpoint>, bcode_session::SessionError> {
    let events = state
        .sessions
        .session_turn_evidence(session_id, turn_id)
        .await?;
    for event in events.iter().rev() {
        if let SessionEventKind::TraceEvent { trace } = &event.kind
            && trace.turn_id.as_deref() == Some(turn_id)
            && let SessionTracePayload::ProviderEvent { event_type, detail } = &trace.payload
            && event_type == CHECKPOINT_EVENT
        {
            let Some(detail) = detail else {
                return Ok(None);
            };
            let checkpoint: RetryCheckpoint = serde_json::from_str(detail).map_err(|_| {
                bcode_session::SessionError::EventSerialization(
                    "retry recovery checkpoint is invalid".into(),
                )
            })?;
            if checkpoint.version != 1 || checkpoint.attempts.len() > 64 {
                return Err(bcode_session::SessionError::EventSerialization(
                    "retry recovery checkpoint is unsupported".into(),
                ));
            }
            return Ok(Some(checkpoint));
        }
    }
    Ok(None)
}

async fn persist(
    state: &ServerState,
    session_id: SessionId,
    turn_id: &str,
    checkpoint: Option<&RetryCheckpoint>,
) -> Result<(), bcode_session::SessionError> {
    let detail = checkpoint
        .map(serde_json::to_string)
        .transpose()
        .map_err(|error| bcode_session::SessionError::EventSerialization(error.to_string()))?;
    // This is execution evidence, not optional debug telemetry. It must survive disabled tracing.
    let event = state
        .sessions
        .append_trace_event(
            session_id,
            SessionTraceEvent {
                timestamp_ms: current_unix_millis(),
                turn_id: Some(turn_id.to_owned()),
                phase: SessionTracePhase::ModelProviderEvent,
                payload: SessionTracePayload::ProviderEvent {
                    event_type: CHECKPOINT_EVENT.into(),
                    detail,
                },
            },
        )
        .await?;
    publish_session_event(state, &event).await;
    Ok(())
}

pub async fn preserve_abandoned(
    state: &Arc<ServerState>,
    session_id: SessionId,
    work: &bcode_session::db::RuntimeWorkProjection,
) -> Result<bool, ServerError> {
    let Some(turn_id) = work.label.strip_prefix("model turn ") else {
        return Ok(false);
    };
    let Some(checkpoint) = load(state, session_id, turn_id).await? else {
        return Ok(false);
    };
    if let Some(outcome) = state
        .sessions
        .session_turn_evidence(session_id, turn_id)
        .await?
        .iter()
        .find_map(|event| match event.kind {
            SessionEventKind::ModelTurnFinished { outcome, .. } => Some(outcome),
            _ => None,
        })
    {
        // Reconcile disposable runtime work without emitting another turn terminal event.
        let status = match outcome {
            ModelTurnOutcome::Completed => RuntimeWorkStatus::Completed,
            ModelTurnOutcome::Cancelled => RuntimeWorkStatus::Cancelled,
            ModelTurnOutcome::IdleTimeout => RuntimeWorkStatus::TimedOut,
            _ => RuntimeWorkStatus::Failed,
        };
        append_runtime_work_finished_event(state, session_id, work.work_id.clone(), status, None)
            .await;
        return Ok(true);
    }
    dispatch_checkpoint(state, session_id, work, turn_id, checkpoint).await
}

async fn dispatch_checkpoint(
    state: &Arc<ServerState>,
    session_id: SessionId,
    work: &bcode_session::db::RuntimeWorkProjection,
    turn_id: &str,
    checkpoint: RetryCheckpoint,
) -> Result<bool, ServerError> {
    let activation = state
        .sessions
        .session_events_range(
            session_id,
            checkpoint.activation_sequence,
            checkpoint.activation_sequence,
            1,
        )
        .await?
        .pop();
    let Some(activation) = activation else {
        return Err(ServerError::Session(
            bcode_session::SessionError::EventSerialization(
                "retry activation is unavailable".into(),
            ),
        ));
    };
    if execution_turn_id(&activation) != turn_id {
        return Err(ServerError::Session(
            bcode_session::SessionError::EventSerialization(
                "retry activation identity mismatch".into(),
            ),
        ));
    }
    let SessionEventKind::SubmissionActivated {
        submission_id,
        client_id,
        ..
    } = &activation.kind
    else {
        // Older turns without immutable submission execution identity cannot be retargeted.
        return Ok(true);
    };
    let Some(record) = state
        .sessions
        .submission(session_id, submission_id.clone())
        .await?
    else {
        return Ok(true);
    };
    if !matches!(
        record.status,
        bcode_session_models::SubmissionStatus::Activated { .. }
    ) {
        return Ok(true);
    }
    let _workflow_authority =
        if let Some(correlation) = &record.submission.admission.execution.correlation {
            let Some(authority) =
                current_workflow_submission_authority(state, &correlation.execution_id).await?
            else {
                return Ok(true);
            };
            Some(authority)
        } else {
            None
        };
    // Reuse exact execution descriptor verification, never the replacement daemon's defaults.
    let Some(FollowupCommand::ExecuteSubmission {
        runtime_context,
        ownership,
        ..
    }) = recover_pending_submission_hint_with_authority(state, session_id, &record, true).await?
    else {
        return Ok(true);
    };
    let handle = Box::pin(session_runtime_handle(state, session_id)).await;
    if handle.phase.lock().await.has_active_work()
        || handle.queued_followups.load(Ordering::Acquire) > 0
    {
        return Ok(true);
    }
    let cancel_state = Arc::new(TurnCancelState::default());
    if work.status == RuntimeWorkStatus::Cancelling {
        cancel_state.cancel().await;
    }
    if handle
        .queued_followups
        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Ok(true);
    }
    if handle
        .followup_commands
        .try_send(FollowupCommand::ExecuteTurn {
            client_id: *client_id,
            runtime_context,
            user_event: Box::new(activation),
            queued_steering: None,
            cancel_state: Some(cancel_state),
            completion: None,
            recovering: true,
            ownership,
        })
        .is_err()
    {
        handle.queued_followups.fetch_sub(1, Ordering::AcqRel);
    }
    Ok(true)
}

pub async fn is_recovering(
    state: &ServerState,
    session_id: SessionId,
    turn_id: &str,
) -> Result<bool, bcode_session::SessionError> {
    Ok(load(state, session_id, turn_id).await?.is_some())
}

pub async fn restore(
    state: &ServerState,
    session_id: SessionId,
    turn_id: &str,
    (recovery, rounds, segment_order, output_position): (
        &mut ModelTurnRecoveryState,
        &mut ModelTurnRoundState,
        &mut u32,
        &mut u64,
    ),
    cancel: &TurnCancelState,
    commands: &mut RuntimeCommandContext<'_>,
) -> Result<ModelTurnRetry, bcode_session::SessionError> {
    let checkpoint = load(state, session_id, turn_id).await?.ok_or_else(|| {
        bcode_session::SessionError::EventSerialization(
            "retry recovery checkpoint unavailable; execution deferred".into(),
        )
    })?;
    recovery.retried_after_context_pressure = checkpoint.recovery_flags.context_pressure;
    recovery.retried_after_malformed_tool_arguments = checkpoint.recovery_flags.malformed_arguments;
    recovery.max_tokens_continuations = checkpoint.recovery_flags.max_tokens_continuations;
    recovery.structured_output_finalization = checkpoint.recovery_flags.finalization;
    recovery.structured_result_corrections = checkpoint.recovery_flags.corrections;
    recovery.structured_result_correction_error = checkpoint.recovery_flags.correction_error;
    recovery.retry_instruction = match checkpoint.recovery_flags.instruction {
        None => None,
        Some(0) => Some(MALFORMED_TOOL_ARGUMENTS_RETRY_INSTRUCTION),
        Some(1) => Some(MAX_TOKENS_CONTINUATION_INSTRUCTION),
        Some(2) => Some(MAX_TOKENS_INCOMPLETE_TOOL_CALL_INSTRUCTION),
        Some(_) => {
            return Err(bcode_session::SessionError::EventSerialization(
                "retry instruction is unsupported".into(),
            ));
        }
    };
    recovery.retry_attempts = checkpoint.attempts;
    recovery.consecutive_retry_attempts = checkpoint.consecutive_attempts;
    rounds.provider_round = checkpoint.provider_round;
    rounds.completed_tool_rounds = checkpoint.completed_tool_rounds;
    *segment_order = checkpoint.segment_order;
    *output_position = checkpoint.output_position;
    Ok(wait_for_provider_retry(
        state,
        session_id,
        Duration::from_millis(
            checkpoint
                .resume_at_ms
                .saturating_sub(current_unix_millis()),
        ),
        cancel,
        commands,
    )
    .await)
}

pub async fn schedule(
    state: &ServerState,
    session_id: SessionId,
    context: &ProviderErrorRetryContext<'_>,
    recovery: &ModelTurnRecoveryState,
    delay: Duration,
) -> Result<(), bcode_session::SessionError> {
    // Provider attempt IDs append the round to the application turn identity.
    let turn_id = context
        .turn_id
        .rsplit_once('-')
        .map_or(context.turn_id, |(turn, suffix)| {
            if suffix.parse::<u32>().is_ok() {
                turn
            } else {
                context.turn_id
            }
        });
    persist(
        state,
        session_id,
        turn_id,
        Some(&RetryCheckpoint {
            version: 1,
            activation_sequence: context.trigger_event_sequence,
            resume_at_ms: current_unix_millis()
                .saturating_add(u64::try_from(delay.as_millis()).unwrap_or(u64::MAX)),
            consecutive_attempts: recovery.consecutive_retry_attempts,
            attempts: recovery.retry_attempts.clone(),
            provider_round: context.provider_round.saturating_add(1),
            completed_tool_rounds: context.completed_tool_rounds,
            segment_order: context.segment_order,
            output_position: context.output_position,
            recovery_flags: RetryRecoveryFlags {
                context_pressure: recovery.retried_after_context_pressure,
                malformed_arguments: recovery.retried_after_malformed_tool_arguments,
                max_tokens_continuations: recovery.max_tokens_continuations,
                finalization: recovery.structured_output_finalization,
                corrections: recovery.structured_result_corrections,
                correction_error: recovery.structured_result_correction_error.clone(),
                instruction: match recovery.retry_instruction {
                    Some(MALFORMED_TOOL_ARGUMENTS_RETRY_INSTRUCTION) => Some(0),
                    Some(MAX_TOKENS_CONTINUATION_INSTRUCTION) => Some(1),
                    Some(MAX_TOKENS_INCOMPLETE_TOOL_CALL_INSTRUCTION) => Some(2),
                    _ => None,
                },
            },
        }),
    )
    .await
}

pub async fn clear(
    state: &ServerState,
    session_id: SessionId,
    turn_id: &str,
) -> Result<(), bcode_session::SessionError> {
    persist(state, session_id, turn_id, None).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::test_server_state;

    fn checkpoint() -> RetryCheckpoint {
        RetryCheckpoint {
            version: 1,
            activation_sequence: 0,
            resume_at_ms: 0,
            consecutive_attempts: 12,
            attempts: BTreeMap::from([("builtin.transient".into(), 12)]),
            provider_round: 14,
            completed_tool_rounds: 3,
            segment_order: 7,
            output_position: 19,
            recovery_flags: RetryRecoveryFlags {
                finalization: true,
                corrections: 2,
                instruction: Some(2),
                ..Default::default()
            },
        }
    }

    async fn dispatch_fixture() -> (
        Arc<ServerState>,
        SessionId,
        bcode_session::db::RuntimeWorkProjection,
    ) {
        use bcode_session_models::{
            PromptSubmission, SubmissionId, SubmissionPlacement, SubmissionTarget,
            TurnAdmissionMetadata, TurnId,
        };
        let sessions = bcode_session::SessionManager::default();
        let session = sessions
            .create_session(None, std::env::temp_dir())
            .await
            .expect("session");
        let id = session.id;
        drop(session);
        let state = Arc::new(crate::tests::test_server_state_with_fake_provider(sessions));
        state.session_model_selections.lock().await.insert(
            id,
            SessionModelSelection {
                provider_plugin_id: Some("bcode.fake-provider".into()),
                model_id: Some("fake-echo".into()),
                ..Default::default()
            },
        );
        let execution = TurnExecutionOptions::default();
        let mut selection = session_model_selection_with_runtime_context(&state, id, None).await;
        apply_turn_model_selection(&mut selection, &execution);
        let descriptor =
            submission_execution_descriptor(&selection, &state.startup_config, &execution)
                .expect("descriptor");
        let submission_id = SubmissionId("retry-submission".into());
        state
            .sessions
            .accept_submission(
                id,
                PromptSubmission {
                    id: submission_id.clone(),
                    client_id: ClientId::new(),
                    text: "continue safely".into(),
                    admission: TurnAdmissionMetadata {
                        execution,
                        ..Default::default()
                    },
                    requested_placement: SubmissionPlacement::FollowUp,
                    target: SubmissionTarget::OwnTurn {
                        turn_id: TurnId("retry-turn".into()),
                    },
                    execution_descriptor: Some(descriptor),
                },
            )
            .await
            .expect("accept");
        let record = state
            .sessions
            .activate_submission(id, submission_id)
            .await
            .expect("activate");
        let bcode_session_models::SubmissionStatus::Activated {
            conversation_sequence,
            ..
        } = record.status
        else {
            panic!("activated");
        };
        let mut saved = checkpoint();
        saved.activation_sequence = conversation_sequence;
        persist(&state, id, "retry-turn", Some(&saved))
            .await
            .expect("checkpoint");
        let work = bcode_session::db::RuntimeWorkProjection {
            work_id: WorkId::new("model_retry-turn"),
            event_seq_start: 0,
            event_seq_end: None,
            kind: RuntimeWorkKind::ModelTurn,
            label: "model turn retry-turn".into(),
            status: RuntimeWorkStatus::Cancelling,
            parent_work_id: None,
            started_at_ms: None,
            finished_at_ms: None,
            message: None,
            cancellable: true,
        };
        (state, id, work)
    }

    #[tokio::test]
    async fn provider_retry_dispatch_is_context_checked_idempotent_and_terminal_stable() {
        let (state, id, work) = dispatch_fixture().await;
        let (followup_commands, mut receiver) = mpsc::channel(2);
        let (steering_commands, _steering_receiver) = mpsc::channel(1);
        let (cancel_commands, _cancel_receiver) = mpsc::channel(1);
        let queued = Arc::new(AtomicUsize::new(0));
        state.session_runtimes.lock().await.insert(
            id,
            SessionRuntimeHandle {
                followup_commands,
                steering_commands,
                cancel_commands,
                queued_followups: Arc::clone(&queued),
                queued_steering: Arc::new(AtomicUsize::new(0)),
                phase: Arc::new(Mutex::new(SessionRuntimePhase::Idle)),
                current_turn: Arc::new(Mutex::new(None)),
            },
        );
        assert!(
            preserve_abandoned(&state, id, &work)
                .await
                .expect("dispatch")
        );
        assert!(
            preserve_abandoned(&state, id, &work)
                .await
                .expect("duplicate")
        );
        assert_eq!(queued.load(Ordering::Acquire), 1);
        let command = receiver.try_recv().expect("one recovery");
        let FollowupCommand::ExecuteTurn {
            recovering,
            user_event,
            cancel_state,
            runtime_context,
            ..
        } = &command
        else {
            panic!("recovery command");
        };
        assert!(*recovering);
        assert_eq!(execution_turn_id(user_event), "retry-turn");
        assert!(cancel_state.as_ref().expect("cancel state").is_cancelled());
        assert_eq!(
            runtime_context
                .as_ref()
                .expect("context")
                .selected_model_id
                .as_deref(),
            Some("fake-echo")
        );
        assert!(receiver.try_recv().is_err());
        drop(command);
        queued.store(0, Ordering::Release);
        state
            .session_model_selections
            .lock()
            .await
            .get_mut(&id)
            .expect("selection")
            .model_id = Some("different-model".into());
        assert!(
            preserve_abandoned(&state, id, &work)
                .await
                .expect("defer incompatible context")
        );
        assert!(receiver.try_recv().is_err());
        state
            .sessions
            .append_event(
                id,
                SessionEventKind::ModelTurnFinished {
                    turn_id: "retry-turn".into(),
                    outcome: ModelTurnOutcome::Cancelled,
                    message: None,
                },
            )
            .await
            .expect("terminal");
        assert!(
            preserve_abandoned(&state, id, &work)
                .await
                .expect("preserve terminal")
        );
        assert!(receiver.try_recv().is_err());
        drop(state);
    }

    #[tokio::test]
    async fn provider_retry_checkpoint_survives_restart_and_unrelated_history() {
        let root = tempfile::tempdir().expect("root");
        let sessions = bcode_session::SessionManager::persistent(root.path()).expect("sessions");
        let session = sessions
            .create_session(None, std::env::temp_dir())
            .await
            .expect("session");
        let id = session.id;
        drop(session);
        let state = test_server_state(sessions);
        persist(&state, id, "retry-turn", Some(&checkpoint()))
            .await
            .expect("checkpoint");
        for index in 0..140 {
            state
                .sessions
                .append_event(
                    id,
                    SessionEventKind::SystemMessage {
                        text: format!("unrelated {index}"),
                    },
                )
                .await
                .expect("unrelated event");
        }
        assert_eq!(
            load(&state, id, "retry-turn")
                .await
                .expect("load")
                .expect("checkpoint")
                .consecutive_attempts,
            12
        );
        state
            .sessions
            .release_session_ownership(id)
            .await
            .expect("release");
        drop(state);
        let state = test_server_state(
            bcode_session::SessionManager::persistent(root.path()).expect("reopen"),
        );
        let restored = load(&state, id, "retry-turn")
            .await
            .expect("load")
            .expect("checkpoint");
        assert_eq!(restored.provider_round, 14);
        assert_eq!(restored.completed_tool_rounds, 3);
        assert_eq!(restored.segment_order, 7);
        assert_eq!(restored.output_position, 19);
        clear(&state, id, "retry-turn").await.expect("retire");
        assert!(
            load(&state, id, "retry-turn")
                .await
                .expect("load retired")
                .is_none()
        );
        drop(state);
    }

    #[tokio::test(start_paused = true)]
    async fn provider_retry_restore_retains_backoff_and_source_positions() {
        let state = test_server_state(bcode_session::SessionManager::default());
        let session = state
            .sessions
            .create_session(None, std::env::temp_dir())
            .await
            .expect("session");
        let id = session.id;
        drop(session);
        persist(&state, id, "retry-turn", Some(&checkpoint()))
            .await
            .expect("checkpoint");
        let mut recovery = ModelTurnRecoveryState::default();
        let mut rounds = ModelTurnRoundState::new(Some(5));
        let mut segment = 0;
        let mut position = 0;
        let (_followup_tx, mut followup_rx) = mpsc::channel(1);
        let (_steering_tx, mut steering_rx) = mpsc::channel(1);
        let (_cancel_tx, mut cancel_rx) = mpsc::channel(1);
        let queued = AtomicUsize::new(0);
        let mut commands = RuntimeCommandContext::new(
            &mut followup_rx,
            &mut steering_rx,
            &mut cancel_rx,
            &queued,
            Arc::new(Mutex::new(None)),
        );
        let result = restore(
            &state,
            id,
            "retry-turn",
            (&mut recovery, &mut rounds, &mut segment, &mut position),
            &TurnCancelState::default(),
            &mut commands,
        )
        .await
        .expect("restore");
        assert!(matches!(result, ModelTurnRetry::Continue));
        assert_eq!(recovery.consecutive_retry_attempts, 12);
        assert_eq!(recovery.retry_attempts["builtin.transient"], 12);
        assert_eq!(rounds.provider_round, 14);
        assert_eq!(rounds.completed_tool_rounds, 3);
        assert_eq!((segment, position), (7, 19));
        assert!(recovery.structured_output_finalization);
        assert_eq!(recovery.structured_result_corrections, 2);
        assert_eq!(
            recovery.retry_instruction,
            Some(MAX_TOKENS_INCOMPLETE_TOOL_CALL_INSTRUCTION)
        );
        assert!(rounds.begin_tool_round().is_ok());
        drop(state);
    }
}
