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
