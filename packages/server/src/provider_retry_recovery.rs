//! Bounded, canonical checkpoints for safe provider-only retry recovery.
use super::*;

const CHECKPOINT_EVENT: &str = bcode_session_models::PROVIDER_RETRY_CHECKPOINT_EVENT;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct RetryCheckpoint {
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<compaction_recovery::Phase>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_plan: Option<compaction_recovery::Plan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overflow_trigger: Option<compaction_recovery::OverflowTrigger>,
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

pub async fn load(
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
            if !matches!(checkpoint.version, 1 | 2)
                || (checkpoint.version == 1
                    && (checkpoint.compaction.is_some() || checkpoint.compaction_plan.is_some()))
                || (checkpoint.version == 2 && checkpoint.compaction.is_none())
                || checkpoint
                    .overflow_trigger
                    .as_ref()
                    .is_some_and(|trigger| !trigger.valid())
                || (checkpoint.version == 1 && checkpoint.overflow_trigger.is_some())
                || checkpoint.attempts.len() > 64
            {
                return Err(bcode_session::SessionError::EventSerialization(
                    "retry recovery checkpoint is unsupported".into(),
                ));
            }
            return Ok(Some(checkpoint));
        }
    }
    Ok(None)
}

pub async fn persist(
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
    // Checkpoint recovery may restore an activated submission, but ordinary queue recovery
    // must never reinterpret activation as permission to retry a turn.
    let Some(FollowupCommand::ExecuteSubmission {
        runtime_context,
        ownership,
        ..
    }) = recover_submission_execution_hint_with_authority(state, session_id, &record, true).await?
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
    if let Some(phase) = &checkpoint.compaction
        && !cancel.is_cancelled()
        && !compaction_recovery::can_resume(state, session_id, phase).await?
    {
        return Ok(ModelTurnRetry::Return(ModelTurnCompletion::with_message(
            ModelTurnOutcome::Error,
            checkpoint.overflow_trigger.as_ref().map_or_else(
                || "interrupted compaction provider outcome is unresolved; no request replayed".to_owned(),
                |trigger| trigger.failure("interrupted compaction provider outcome is unresolved; no request replayed"),
            ),
        )));
    }
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
            compaction: None,
            compaction_plan: None,
            overflow_trigger: None,
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

pub fn compaction_checkpoint(
    activation_sequence: u64,
    (recovery, rounds, segment_order, output_position): (
        &ModelTurnRecoveryState,
        &ModelTurnRoundState,
        u32,
        u64,
    ),
) -> Result<RetryCheckpoint, bcode_session::SessionError> {
    let instruction = match recovery.retry_instruction {
        None => None,
        Some(MALFORMED_TOOL_ARGUMENTS_RETRY_INSTRUCTION) => Some(0),
        Some(MAX_TOKENS_CONTINUATION_INSTRUCTION) => Some(1),
        Some(MAX_TOKENS_INCOMPLETE_TOOL_CALL_INSTRUCTION) => Some(2),
        Some(_) => {
            return Err(bcode_session::SessionError::EventSerialization(
                "compaction recovery instruction is unsupported".into(),
            ));
        }
    };
    if recovery.retry_attempts.len() > 64 {
        return Err(bcode_session::SessionError::EventSerialization(
            "compaction recovery retry accounting exceeds checkpoint bounds".into(),
        ));
    }
    Ok(RetryCheckpoint {
        version: 2,
        activation_sequence,
        resume_at_ms: 0,
        consecutive_attempts: recovery.consecutive_retry_attempts,
        attempts: recovery.retry_attempts.clone(),
        provider_round: rounds.provider_round,
        completed_tool_rounds: rounds.completed_tool_rounds,
        segment_order,
        output_position,
        recovery_flags: RetryRecoveryFlags {
            context_pressure: recovery.retried_after_context_pressure,
            malformed_arguments: recovery.retried_after_malformed_tool_arguments,
            max_tokens_continuations: recovery.max_tokens_continuations,
            finalization: recovery.structured_output_finalization,
            corrections: recovery.structured_result_corrections,
            correction_error: recovery.structured_result_correction_error.clone(),
            instruction,
        },
        compaction: Some(compaction_recovery::Phase::Safe),
        compaction_plan: None,
        overflow_trigger: None,
    })
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
            compaction: None,
            compaction_plan: None,
            overflow_trigger: None,
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
        dispatch_fixture_with_sessions(bcode_session::SessionManager::default()).await
    }

    async fn dispatch_fixture_with_sessions(
        sessions: bcode_session::SessionManager,
    ) -> (
        Arc<ServerState>,
        SessionId,
        bcode_session::db::RuntimeWorkProjection,
    ) {
        use bcode_session_models::{
            PromptSubmission, SubmissionId, SubmissionPlacement, SubmissionTarget,
            TurnAdmissionMetadata, TurnId,
        };
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

    #[tokio::test]
    async fn compaction_reservation_alone_cannot_recover_original_activation() {
        let (state, id, work) = dispatch_fixture().await;
        clear(&state, id, "retry-turn")
            .await
            .expect("remove retry checkpoint");
        state
            .sessions
            .append_compaction_checkpoint(
                id,
                "local-reservation",
                Some("reserved-without-activation".into()),
            )
            .await
            .expect("reservation");
        // This is the former restart limitation: local synthesis state alone does not
        // authorize restoring an activated submission, even when its runtime work remains.
        assert!(
            !preserve_abandoned(&state, id, &work)
                .await
                .expect("no authority")
        );
    }

    #[tokio::test]
    async fn interrupted_compaction_restores_same_activation_without_manual_cancellation() {
        let (state, id, mut work) = dispatch_fixture().await;
        work.status = RuntimeWorkStatus::Running;
        let mut checkpoint = load(&state, id, "retry-turn")
            .await
            .expect("load")
            .expect("checkpoint");
        checkpoint.version = 2;
        checkpoint.compaction = Some(compaction_recovery::Phase::Safe);
        persist(&state, id, "retry-turn", Some(&checkpoint))
            .await
            .expect("safe point");
        let (followup_commands, mut receiver) = mpsc::channel(4);
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
                .expect("restore")
        );
        let FollowupCommand::ExecuteTurn {
            user_event,
            cancel_state,
            recovering,
            ..
        } = receiver.try_recv().expect("original activation")
        else {
            panic!("execute turn")
        };
        assert!(recovering);
        assert_eq!(execution_turn_id(&user_event), "retry-turn");
        assert!(!cancel_state.expect("cancel state").is_cancelled());
        assert!(
            preserve_abandoned(&state, id, &work)
                .await
                .expect("duplicate")
        );
        assert!(receiver.try_recv().is_err());
    }

    async fn attachment_timeout_diagnostics(state: &ServerState, id: SessionId) -> String {
        let runtime = state.session_runtimes.lock().await.get(&id).cloned();
        let runtime_state = if let Some(runtime) = runtime {
            let queued = runtime.queued_followups.load(Ordering::Acquire);
            let phase = *runtime.phase.lock().await;
            let active = runtime.current_turn.lock().await.is_some();
            Some((queued, phase, active))
        } else {
            None
        };
        let queue = state.sessions.submission_queue_status(id).await.unwrap();
        let oldest = queue.oldest.as_ref().map(|record| &record.status);
        let observation = queue
            .oldest
            .as_ref()
            .and_then(|record| record.context_observation.as_ref())
            .map(|observation| observation.issue);
        let evidence = state
            .sessions
            .session_turn_evidence(id, "retry-turn")
            .await
            .unwrap();
        let kinds: Vec<_> = evidence
            .iter()
            .take(32)
            .map(|event| (event.sequence, session_event_kind_name(&event.kind)))
            .collect();
        let tail = state
            .sessions
            .session_history_page(
                id,
                bcode_session_models::SessionHistoryQuery {
                    cursor: None,
                    limit: 32,
                    direction: bcode_session_models::SessionHistoryDirection::Backward,
                },
            )
            .await
            .unwrap();
        let deferrals = tail.events.iter().filter(|event| matches!(&event.kind,
            SessionEventKind::SystemMessage { text } if text.starts_with("Retry recovery deferred:"))).count();
        format!(
            "normal reconnect did not settle: runtime={runtime_state:?}; oldest={oldest:?}; blocker={:?}; context_issue={observation:?}; recent_context_deferrals={deferrals}; turn_evidence={kinds:?}",
            queue.blocker
        )
    }

    #[tokio::test]
    async fn normal_attachment_resumes_known_safe_point_on_same_activation() {
        assert_normal_attachment_settlement(
            compaction_recovery::Phase::Safe,
            ModelTurnOutcome::Completed,
            false,
        )
        .await;
    }

    #[tokio::test]
    async fn normal_attachment_settles_interrupted_compaction_without_replacement_submission() {
        assert_normal_attachment_settlement(
            compaction_recovery::Phase::Native,
            ModelTurnOutcome::Error,
            false,
        )
        .await;
    }

    #[tokio::test]
    async fn normal_attachment_nonresident_safe_point_keeps_original_activation() {
        assert_normal_attachment_settlement(
            compaction_recovery::Phase::Safe,
            ModelTurnOutcome::Completed,
            true,
        )
        .await;
    }

    #[tokio::test]
    async fn normal_attachment_nonresident_uncertainty_settles_without_replay() {
        assert_normal_attachment_settlement(
            compaction_recovery::Phase::Native,
            ModelTurnOutcome::Error,
            true,
        )
        .await;
    }

    async fn append_abandoned_model_work(state: &ServerState, id: SessionId, work_id: WorkId) {
        state
            .sessions
            .append_event(
                id,
                SessionEventKind::RuntimeWorkStarted {
                    work_id,
                    kind: RuntimeWorkKind::ModelTurn,
                    label: "model turn retry-turn".into(),
                    tool_call_id: None,
                    plugin_id: None,
                    service_interface: None,
                    operation: None,
                    parent_work_id: None,
                    started_at_ms: Some(current_unix_millis()),
                    cancellable: true,
                },
            )
            .await
            .unwrap();
        state
            .sessions
            .append_model_turn_started(id, "retry-turn".into())
            .await
            .unwrap();
    }

    async fn reopen_attachment_fixture(
        state: Arc<ServerState>,
        id: SessionId,
        root: &std::path::Path,
    ) -> Arc<ServerState> {
        let selection = state
            .session_model_selections
            .lock()
            .await
            .get(&id)
            .cloned()
            .unwrap();
        state
            .sessions
            .release_session_ownership(id)
            .await
            .expect("verified owner release");
        drop(state);
        let sessions =
            bcode_session::SessionManager::persistent(root).expect("new daemon session manager");
        let state = Arc::new(crate::tests::test_server_state_with_fake_provider(sessions));
        // The v1 fixture's immutable descriptor requires this exact configuration, not defaults.
        state
            .session_model_selections
            .lock()
            .await
            .insert(id, selection);
        state
    }

    async fn assert_normal_attachment_settlement(
        phase: compaction_recovery::Phase,
        expected: ModelTurnOutcome,
        nonresident: bool,
    ) {
        let root = tempfile::tempdir().expect("private persistent session root");
        let sessions =
            bcode_session::SessionManager::persistent(root.path()).expect("session manager");
        let (state, id, work) = dispatch_fixture_with_sessions(sessions).await;
        let mut checkpoint = load(&state, id, "retry-turn").await.unwrap().unwrap();
        checkpoint.version = 2;
        checkpoint.recovery_flags = RetryRecoveryFlags::default();
        checkpoint.consecutive_attempts = 0;
        checkpoint.attempts.clear();
        checkpoint.provider_round = 0;
        checkpoint.completed_tool_rounds = 0;
        checkpoint.segment_order = 0;
        checkpoint.output_position = 0;
        checkpoint.compaction = Some(phase);
        persist(&state, id, "retry-turn", Some(&checkpoint))
            .await
            .unwrap();
        append_abandoned_model_work(&state, id, work.work_id).await;
        let state = if nonresident {
            reopen_attachment_fixture(state, id, root.path()).await
        } else {
            state
        };
        // Export is observation, not execution ownership or a recovery trigger.
        let history = crate::session_operations::complete_history(&state, ClientId::new(), id)
            .await
            .unwrap();
        assert!(
            !history
                .iter()
                .any(|event| matches!(event.kind, SessionEventKind::ModelTurnFinished { .. }))
        );
        // Exercise the normal attachment lifecycle, never the internal recovery helper.
        assert!(
            crate::session_operations::attach(&state, id, ClientId::new())
                .await
                .is_ok(),
            "normal session attachment failed",
        );
        let settlement = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let events = state
                    .sessions
                    .session_turn_evidence(id, "retry-turn")
                    .await
                    .unwrap();
                if let Some(outcome) = events.iter().find_map(|event| match &event.kind {
                    SessionEventKind::ModelTurnFinished { outcome, .. } => Some(*outcome),
                    _ => None,
                }) {
                    assert_eq!(outcome, expected);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            settlement.is_ok(),
            "{}",
            attachment_timeout_diagnostics(&state, id).await
        );
        let history = state.sessions.session_history(id).await.unwrap();
        assert_eq!(
            history
                .iter()
                .filter(|event| matches!(event.kind, SessionEventKind::SubmissionActivated { .. }))
                .count(),
            1
        );
        assert_eq!(
            history
                .iter()
                .filter(|event| matches!(event.kind, SessionEventKind::ModelTurnFinished { .. }))
                .count(),
            1
        );
        if expected == ModelTurnOutcome::Error {
            assert!(
                !history.iter().any(|event| matches!(
                    event.kind,
                    SessionEventKind::ModelUsage { .. }
                        | SessionEventKind::ContextCompacted { .. }
                        | SessionEventKind::ProviderContextCompacted { .. }
                )),
                "unresolved compaction must not dispatch a provider or publish a marker"
            );
        }
        state.request_shutdown();
        drop(state);
    }

    #[tokio::test(start_paused = true)]
    async fn uncertain_compaction_terminalizes_without_replaying_and_cancellation_wins() {
        for cancel_requested in [false, true] {
            let state = test_server_state(bcode_session::SessionManager::default());
            let session = state
                .sessions
                .create_session(None, std::env::temp_dir())
                .await
                .expect("session");
            let id = session.id;
            drop(session);
            let mut checkpoint = checkpoint();
            checkpoint.version = 2;
            checkpoint.compaction = Some(compaction_recovery::Phase::Native);
            persist(&state, id, "retry-turn", Some(&checkpoint))
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
            let cancel = TurnCancelState::default();
            if cancel_requested {
                cancel.cancel().await;
            }
            let ModelTurnRetry::Return(completion) = restore(
                &state,
                id,
                "retry-turn",
                (&mut recovery, &mut rounds, &mut segment, &mut position),
                &cancel,
                &mut commands,
            )
            .await
            .expect("reconcile") else {
                panic!("uncertainty cannot resume dispatch")
            };
            assert_eq!(
                completion.outcome,
                if cancel_requested {
                    ModelTurnOutcome::Cancelled
                } else {
                    ModelTurnOutcome::Error
                }
            );
            drop(state);
        }
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
