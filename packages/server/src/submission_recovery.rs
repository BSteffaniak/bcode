//! Targeted pending-input recovery application operations.

use super::{
    Arc, ClientId, ErrorResponse, ServerState, SessionId, apply_turn_model_selection,
    capture_submission_execution_descriptor, live_submission_runtime, resolve_submission_context,
    session_error_response, session_model_selection_with_runtime_context, turn_admission_lock,
    verify_submission_model_target, wake_pending_submission,
};
use bcode_session_models::{
    SubmissionQueueBlocker, SubmissionQueueStatus, SubmissionRecord, SubmissionResumeConfirmation,
    SubmissionStatus, SubmissionTarget,
};

fn rejected(code: &str, message: &str) -> ErrorResponse {
    ErrorResponse::new(code, message)
}

fn session_failure(error: &bcode_session::SessionError) -> ErrorResponse {
    session_error_response(error)
}

fn validate_workflow_owner(record: &SubmissionRecord) -> Result<(), ErrorResponse> {
    let admission = &record.submission.admission;
    if admission.execution.correlation.is_some()
        || admission.origin.as_ref().is_some_and(|origin| {
            origin.producer == "workflow"
                || origin.producer.starts_with("workflow.")
                || origin.producer.starts_with("bcode.workflow")
        })
    {
        return Err(rejected(
            "workflow_authority_required",
            "workflow-owned input requires workflow recovery authority",
        ));
    }
    Ok(())
}

fn validate_pending(record: &SubmissionRecord, expected: u64) -> Result<(), ErrorResponse> {
    validate_workflow_owner(record)?;
    if record.receipt.accepted_event_sequence != expected {
        return Err(rejected(
            "stale_submission",
            "submission acceptance changed",
        ));
    }
    if record.submission.admission.execution.correlation.is_some()
        || record
            .submission
            .admission
            .origin
            .as_ref()
            .is_some_and(|origin| origin.producer == "bcode.workflow")
    {
        return Err(rejected(
            "workflow_authority_required",
            "workflow-owned input requires workflow recovery authority",
        ));
    }
    if record.status != SubmissionStatus::Pending {
        return Err(rejected(
            "submission_not_pending",
            "submission is no longer pending",
        ));
    }
    Ok(())
}

async fn exact_record(
    state: &ServerState,
    session_id: SessionId,
    id: bcode_session_models::SubmissionId,
) -> Result<SubmissionRecord, ErrorResponse> {
    state
        .sessions
        .submission(session_id, id)
        .await
        .map_err(|error| session_failure(&error))?
        .ok_or_else(|| rejected("submission_not_found", "submission not found"))
}

pub async fn status(
    state: &ServerState,
    session_id: SessionId,
) -> Result<SubmissionQueueStatus, ErrorResponse> {
    let mut observed = state
        .sessions
        .submission_queue_status(session_id)
        .await
        .map_err(|error| session_failure(&error))?;
    let Some(record) = observed.oldest.as_ref() else {
        return Ok(observed);
    };
    if validate_workflow_owner(record).is_err() {
        observed.blocker = Some(SubmissionQueueBlocker::WorkflowAuthorityRequired);
        return Ok(observed);
    }
    let runtime = state
        .session_runtimes
        .lock()
        .await
        .get(&session_id)
        .cloned();
    let current = if let Some(runtime) = runtime {
        runtime.current_turn.lock().await.clone()
    } else {
        None
    };
    if matches!(record.status, SubmissionStatus::Activated { .. }) {
        observed.blocker = if current
            .as_ref()
            .is_some_and(|turn| turn.turn_id == record.submission.target.turn_id().0)
        {
            None
        } else {
            Some(SubmissionQueueBlocker::InterruptedTurn)
        };
        return Ok(observed);
    }
    if matches!(record.submission.target, SubmissionTarget::Steering { .. }) {
        observed.blocker = if current
            .as_ref()
            .is_some_and(|turn| turn.turn_id == record.submission.target.turn_id().0)
        {
            None
        } else {
            Some(SubmissionQueueBlocker::SteeringTargetUnavailable)
        };
        return Ok(observed);
    }
    // Status reads use canonical bounded observations, never config files or credential materialization.
    observed.blocker = match record
        .context_observation
        .as_ref()
        .map(|observation| observation.issue)
    {
        Some(bcode_session_models::SubmissionContextIssue::Unavailable) => {
            Some(SubmissionQueueBlocker::MissingContext)
        }
        Some(bcode_session_models::SubmissionContextIssue::Incompatible) => {
            Some(SubmissionQueueBlocker::IncompatibleContext)
        }
        None if record.effective_execution_descriptor().is_none() => {
            Some(SubmissionQueueBlocker::MissingContext)
        }
        None => None,
    };
    Ok(observed)
}

pub async fn prepare(
    state: &ServerState,
    session_id: SessionId,
    client_id: ClientId,
    id: bcode_session_models::SubmissionId,
    expected: u64,
) -> Result<SubmissionResumeConfirmation, ErrorResponse> {
    let record = exact_record(state, session_id, id.clone()).await?;
    validate_pending(&record, expected)?;
    if matches!(record.submission.target, SubmissionTarget::Steering { .. }) {
        return Err(rejected(
            "steering_target_unavailable",
            "pending steering cannot be redirected to another turn",
        ));
    }
    let runtime = state
        .client_runtime_context(client_id)
        .await
        .ok_or_else(|| {
            rejected(
                "recovery_context_unavailable",
                "client configuration provenance is unavailable",
            )
        })?;
    let config = runtime
        .effective_config_toml
        .as_deref()
        .and_then(|contents| bcode_config::decode_effective_config(contents).ok())
        .ok_or_else(|| {
            rejected(
                "recovery_context_unavailable",
                "client configuration provenance is unavailable",
            )
        })?;
    let mut selection =
        session_model_selection_with_runtime_context(state, session_id, Some(runtime.clone()))
            .await;
    apply_turn_model_selection(&mut selection, &record.submission.admission.execution);
    let descriptor = capture_submission_execution_descriptor(
        state,
        session_id,
        &selection,
        &config,
        &record.submission.admission.execution,
        runtime.config_launch.as_deref(),
    )
    .await
    .ok_or_else(|| {
        rejected(
            "recovery_context_unavailable",
            "execution configuration has no verified durable source",
        )
    })?;
    let verified = resolve_submission_context(state, session_id, &descriptor)
        .await
        .map_err(|_| {
            rejected(
                "recovery_context_unavailable",
                "confirmed execution configuration cannot be reconstructed",
            )
        })?;
    verify_submission_model_target(state, &descriptor, &verified)
        .await
        .map_err(|_| {
            rejected(
                "recovery_context_unavailable",
                "confirmed provider model is unavailable or changed",
            )
        })?;
    Ok(SubmissionResumeConfirmation {
        version: 1,
        submission_id: id,
        expected_accepted_sequence: expected,
        descriptor,
    })
}

pub async fn resume(
    state: &Arc<ServerState>,
    session_id: SessionId,
    client_id: ClientId,
    confirmation: SubmissionResumeConfirmation,
    operation_id: String,
) -> Result<bcode_session_models::SubmissionRecoveryResult, ErrorResponse> {
    Box::pin(resume_inner(
        state,
        session_id,
        client_id,
        confirmation,
        operation_id,
    ))
    .await
}

async fn resume_inner(
    state: &Arc<ServerState>,
    session_id: SessionId,
    client_id: ClientId,
    confirmation: SubmissionResumeConfirmation,
    operation_id: String,
) -> Result<bcode_session_models::SubmissionRecoveryResult, ErrorResponse> {
    if confirmation.version != 1 {
        return Err(rejected(
            "unsupported_recovery_version",
            "unsupported recovery confirmation version",
        ));
    }
    let record = exact_record(state, session_id, confirmation.submission_id.clone()).await?;
    validate_workflow_owner(&record)?;
    if let Some(result) = state
        .sessions
        .check_submission_recovery_retry(
            session_id,
            confirmation.submission_id.clone(),
            confirmation.expected_accepted_sequence,
            operation_id.clone(),
            Some(confirmation.descriptor.clone()),
        )
        .await
        .map_err(|error| session_failure(&error))?
    {
        return Ok(result);
    }
    let prepared = prepare(
        state,
        session_id,
        client_id,
        confirmation.submission_id.clone(),
        confirmation.expected_accepted_sequence,
    )
    .await?;
    if prepared != confirmation {
        return Err(rejected(
            "recovery_context_changed",
            "confirmed execution selection differs from the requesting client",
        ));
    }
    let runtime_context = resolve_submission_context(state, session_id, &confirmation.descriptor)
        .await
        .map_err(|_| {
            rejected(
                "recovery_context_changed",
                "confirmed execution configuration is unavailable or changed",
            )
        })?;
    verify_submission_model_target(state, &confirmation.descriptor, &runtime_context)
        .await
        .map_err(|_| {
            rejected(
                "recovery_context_changed",
                "confirmed provider model is unavailable or changed",
            )
        })?;
    let ownership = state
        .sessions
        .acquire_session_ownership(
            session_id,
            bcode_session::SessionOwnershipKind::QueuedCommand,
        )
        .await
        .map_err(|error| session_failure(&error))?;
    let handle = live_submission_runtime(state, session_id).await;
    let lock = turn_admission_lock(state, session_id).await;
    let guard = lock.lock().await;
    let result = state
        .sessions
        .authorize_submission_recovery(
            session_id,
            confirmation.submission_id.clone(),
            confirmation.expected_accepted_sequence,
            operation_id,
            confirmation.descriptor,
        )
        .await
        .map_err(|error| session_failure(&error))?;
    drop(guard);
    if result.record.status == SubmissionStatus::Pending {
        wake_pending_submission(
            &handle,
            confirmation.submission_id,
            Some(runtime_context),
            ownership,
        )
        .await;
    }
    Ok(result)
}

/// Explicitly end one orphaned interactive turn, without replaying any invocation.
/// False means this is not the exact interrupted queue head; ordinary cancellation may apply.
///
/// # Errors
/// Returns storage, ownership, concurrent-change, and incomplete-evidence errors.
pub async fn end_interrupted_turn(
    state: &Arc<ServerState>,
    session_id: SessionId,
    turn_id: &str,
) -> Result<bool, super::ServerError> {
    use bcode_session_models::SessionEventKind;

    let _ownership = state
        .sessions
        .acquire_session_ownership(
            session_id,
            bcode_session::SessionOwnershipKind::QueuedCommand,
        )
        .await?;
    let lock = turn_admission_lock(state, session_id).await;
    let admission_guard = lock.lock().await;
    let observed = status(state, session_id).await.map_err(|error| {
        super::ServerError::WorkflowApplicationOperationUnauthorized(error.message)
    })?;
    let Some(record) = observed.oldest else {
        return Ok(false);
    };
    if observed.blocker != Some(SubmissionQueueBlocker::InterruptedTurn)
        || record.submission.target.turn_id().0 != turn_id
    {
        return Ok(false);
    }
    if state
        .sessions
        .session_summary(session_id)
        .await?
        .execution
        .is_some()
        || !state
            .runtime_work
            .active_ids_for_session(session_id)
            .await
            .is_empty()
    {
        return Err(
            super::ServerError::WorkflowApplicationOperationUnauthorized(
                "interrupted turn still has execution ownership; use its authorized controls"
                    .into(),
            ),
        );
    }
    if let Some(runtime) = state
        .session_runtimes
        .lock()
        .await
        .get(&session_id)
        .cloned()
        && (runtime.current_turn.lock().await.is_some()
            || runtime.phase.lock().await.has_active_work()
            || runtime
                .queued_followups
                .load(std::sync::atomic::Ordering::Acquire)
                > 0)
    {
        return Err(
            super::ServerError::WorkflowApplicationOperationUnauthorized(
                "turn execution is live or scheduled; retry after it settles".into(),
            ),
        );
    }
    let generation = state
        .sessions
        .current_session_generation(session_id)
        .await?;
    let terminal = state
        .sessions
        .session_turn_evidence(session_id, turn_id)
        .await?;
    if terminal
        .iter()
        .any(|event| matches!(event.kind, SessionEventKind::ModelTurnFinished { .. }))
    {
        return Ok(false);
    }
    let events = interrupted_terminal_events(state, session_id, turn_id, generation).await?;
    let mut expected_generation = generation;
    for kind in events {
        let permission = match &kind {
            SessionEventKind::PermissionResolved { permission_id, .. } => {
                Some(permission_id.clone())
            }
            _ => None,
        };
        let event = state
            .sessions
            .append_event_at_generation(session_id, kind, expected_generation)
            .await?;
        expected_generation = event.sequence;
        super::publish_session_event(state, &event).await;
        if let Some(id) = permission {
            state.recovered_permissions.lock().await.remove(&id);
            state.pending_permissions.lock().await.remove(&id);
        }
    }
    drop(admission_guard);
    let _handle = live_submission_runtime(state, session_id).await;
    Ok(true)
}

const INTERRUPTED_TURN_MESSAGE: &str = "The interrupted turn was explicitly ended without replaying tools. Prior effects may have occurred; inspect them before retrying.";

fn validate_interrupted_work(
    work: &[bcode_session::db::RuntimeWorkProjection],
    turn_id: &str,
) -> Result<(), super::ServerError> {
    let mut owned = std::collections::BTreeSet::from([format!("model_{turn_id}")]);
    for _ in 0..work.len() {
        for item in work {
            if item
                .parent_work_id
                .as_ref()
                .is_some_and(|id| owned.contains(&id.to_string()))
            {
                owned.insert(item.work_id.to_string());
            }
        }
    }
    if work
        .iter()
        .any(|item| !owned.contains(&item.work_id.to_string()))
    {
        return Err(
            super::ServerError::WorkflowApplicationOperationUnauthorized(
                "unrelated runtime work must settle before interrupted recovery".into(),
            ),
        );
    }
    Ok(())
}

async fn interrupted_terminal_events(
    state: &ServerState,
    session_id: SessionId,
    turn_id: &str,
    generation: u64,
) -> Result<Vec<bcode_session_models::SessionEventKind>, super::ServerError> {
    use bcode_session_models::{ModelTurnOutcome, RuntimeWorkStatus, SessionEventKind};
    let bcode_session::InterruptedRecoveryWork { tools, work } =
        state.sessions.interrupted_recovery_work(session_id).await?;
    if tools.len() > 128 || work.len() > 256 {
        return Err(
            super::ServerError::WorkflowApplicationOperationUnauthorized(
                "interrupted recovery exceeds its bounded work page".into(),
            ),
        );
    }
    validate_interrupted_work(&work, turn_id)?;
    let message = INTERRUPTED_TURN_MESSAGE;
    let mut events = Vec::new();
    for tool in tools {
        let interval = state
            .sessions
            .session_events_range(session_id, tool.event_seq_start, generation, 1024)
            .await?;
        if interval
            .last()
            .is_none_or(|event| event.sequence != generation)
            || !matches!(interval.first().map(|event| &event.kind),
                Some(SessionEventKind::PositionedToolCallRequested { turn_id: requested_turn, .. }) if requested_turn == turn_id)
        {
            return Err(
                super::ServerError::WorkflowApplicationOperationUnauthorized(
                    "invocation ownership or bounded recovery evidence is unavailable".into(),
                ),
            );
        }
        for event in &interval {
            if let SessionEventKind::PermissionRequested { permission_id, tool_call_id, .. } = &event.kind
                && tool_call_id == &tool.tool_call_id
                && !interval.iter().any(|event| matches!(&event.kind,
                    SessionEventKind::PermissionResolved { permission_id: id, .. } if id == permission_id))
            {
                events.push(SessionEventKind::PermissionResolved { permission_id: permission_id.clone(), approved: false });
            }
        }
        events.push(SessionEventKind::ToolInvocationResultRecorded {
            record: bcode_session_models::ToolInvocationResultRecord {
                invocation_id: tool.tool_call_id,
                model_output: message.into(),
                is_error: true,
                content: vec![bcode_session_models::ToolResultContent::Text {
                    text: message.into(),
                }],
                presentation: None,
                result: Some(bcode_session_models::ToolInvocationResult::Text {
                    text: message.into(),
                }),
            },
        });
    }
    for item in work.into_iter().rev() {
        events.push(SessionEventKind::RuntimeWorkFinished {
            work_id: item.work_id,
            finished_at_ms: Some(super::current_time_ms()),
            status: RuntimeWorkStatus::Cancelled,
            message: Some(message.into()),
        });
    }
    // The terminal event goes last: interruption before it preserves the queue blocker.
    // A retry reobserves only unfinished work, never replays a tool or reopens a terminal turn.
    events.push(SessionEventKind::ModelTurnFinished {
        turn_id: turn_id.into(),
        outcome: ModelTurnOutcome::Cancelled,
        message: Some(message.into()),
    });
    Ok(events)
}

pub async fn cancel(
    state: &Arc<ServerState>,
    session_id: SessionId,
    id: bcode_session_models::SubmissionId,
    expected: u64,
    operation_id: String,
) -> Result<bcode_session_models::SubmissionRecoveryResult, ErrorResponse> {
    let record = exact_record(state, session_id, id.clone()).await?;
    validate_workflow_owner(&record)?;
    if let Some(result) = state
        .sessions
        .check_submission_recovery_retry(
            session_id,
            id.clone(),
            expected,
            operation_id.clone(),
            None,
        )
        .await
        .map_err(|error| session_failure(&error))?
    {
        return Ok(result);
    }
    validate_pending(&record, expected)?;
    let _ownership = state
        .sessions
        .acquire_session_ownership(
            session_id,
            bcode_session::SessionOwnershipKind::QueuedCommand,
        )
        .await
        .map_err(|error| session_failure(&error))?;
    let lock = turn_admission_lock(state, session_id).await;
    let guard = lock.lock().await;
    let result = state
        .sessions
        .cancel_pending_submission(session_id, id, expected, operation_id)
        .await
        .map_err(|error| session_failure(&error))?;
    drop(guard);
    // Cancellation only changes this input. Wake the actor so the next FIFO item can run.
    let _handle = live_submission_runtime(state, session_id).await;
    Ok(result)
}
