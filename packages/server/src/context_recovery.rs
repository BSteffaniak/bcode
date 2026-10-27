//! Explicit stale-summary reconstruction under the owning session runtime.
//!
//! Register this module in the server and call `rebuild_session_summary` only from the serialized
//! manual-compaction command path, retaining its queued-command ownership, current operation,
//! cancellation servicing and phase cleanup. This helper never admits work on its own.

use super::*;
use bcode_session::context_recovery::{SummaryRecoveryError, SummaryRecoveryReader};
use bcode_session_models::SessionSummaryRebuildRequest;

tokio::task_local! {
    static PROVIDER_REQUESTS_LEFT: std::cell::Cell<u32>;
}

/// Charge exactly once immediately before each provider dispatch, including repair and final
/// consolidation. Non-rebuild callers have no allowance in this scope and are unaffected.
/// Cancellation or an ambiguous dispatched request never refunds a charge.
pub fn reserve_provider_request() -> Result<(), CompactionError> {
    PROVIDER_REQUESTS_LEFT
        .try_with(|remaining| {
            let next = remaining.get().checked_sub(1).ok_or_else(|| {
                CompactionError::Provider(
            "summary reconstruction provider allowance exhausted; existing marker unchanged".into(),
        )
            })?;
            remaining.set(next);
            Ok(())
        })
        .unwrap_or(Ok(()))
}

fn recovery_error(_error: SummaryRecoveryError) -> CompactionError {
    // Storage diagnostics may contain private paths or payload details. Keep this public error
    // normalized; canonical readers retain their domain-specific error for internal diagnosis.
    CompactionError::Provider(
        "canonical summary reconstruction failed validation or exhausted its explicit allowance; existing marker unchanged".into(),
    )
}

/// Reconstruct and replace exactly one stale summary without advancing its covered boundary.
///
/// The caller must hold session ownership and serialize model turns, source activation and marker
/// publication for the entire operation, including the final recheck and append. Cancellation uses
/// the same current-operation state and marker-commit lock as ordinary manual compaction.
/// No historical summary seeds the first synthesis; subsequent pages carry only this operation's
/// fresh bounded synthesis. Request-only context and pending submissions never enter the input.
///
/// # Errors
/// Returns cancellation, provider failure, stale selection, unavailable canonical evidence or
/// exhausted allowance without publishing a replacement marker.
pub async fn rebuild_session_summary(
    state: &ServerState,
    request: SessionSummaryRebuildRequest,
    selection: &SessionModelSelection,
    command_context: Option<&mut RuntimeCommandContext<'_>>,
    cancel_state: &TurnCancelState,
) -> Result<String, CompactionError> {
    if !request.budget.is_valid() {
        return Err(CompactionError::Provider(
            "invalid summary reconstruction allowance".into(),
        ));
    }
    let operation = PROVIDER_REQUESTS_LEFT.scope(
        std::cell::Cell::new(request.budget.max_provider_requests),
        rebuild_inner(state, request, selection, command_context, cancel_state),
    );
    tokio::pin!(operation);
    tokio::select! {
        result = &mut operation => result,
        () = tokio::time::sleep(Duration::from_millis(u64::from(request.budget.timeout_ms))) => {
            // Do not abandon a provider invocation on timeout: cancel through its owning state
            // and let the existing engine perform bounded finalization/cleanup.
            cancel_state.close();
            // If the timeout raced a completed canonical commit, report the committed result.
            // Otherwise cancellation wins and the original marker remains authoritative.
            operation.await
        }
    }
}

fn recovery_lines(page: Vec<bcode_session_models::SessionEvent>) -> Vec<String> {
    let mut lines = Vec::new();
    for event in page {
        // Recovery must not truncate the ending of a canonical tool result. Ordinary summary
        // projections are intentionally bounded; this explicit path instead admits the full
        // already-budgeted canonical model output, or the engine rejects the complete input.
        let line = if let SessionEventKind::ToolInvocationResultRecorded { record } = &event.kind {
            Some(format!(
                "#{} tool result {} (error={}; untrusted canonical model output):\n{}",
                event.sequence, record.invocation_id, record.is_error, record.model_output
            ))
        } else {
            context_compaction::session_event_compaction_line(&event, usize::MAX)
        };
        if let Some(line) = line {
            lines.push(line);
        }
    }
    lines
}

async fn rebuild_inner(
    state: &ServerState,
    request: SessionSummaryRebuildRequest,
    selection: &SessionModelSelection,
    mut command_context: Option<&mut RuntimeCommandContext<'_>>,
    cancel_state: &TurnCancelState,
) -> Result<String, CompactionError> {
    if cancel_state.is_cancelled() {
        return Err(CompactionError::Cancelled);
    }
    let mut reader = tokio::select! {
        () = cancel_state.cancelled() => return Err(CompactionError::Cancelled),
        result = SummaryRecoveryReader::begin(&state.sessions, request) => result.map_err(recovery_error)?,
    };
    let pin = reader.pin();
    let mut summary = None;
    let mut evidence_events = 0_usize;
    loop {
        if let Some(context) = command_context.as_deref_mut() {
            service_cancel_commands(
                state,
                request.session_id,
                context.cancel_commands,
                context.followup_commands,
                context.queued_followups,
            )
            .await;
        }
        if cancel_state.is_cancelled() {
            return Err(CompactionError::Cancelled);
        }
        let page = tokio::select! {
            () = cancel_state.cancelled() => return Err(CompactionError::Cancelled),
            result = reader.next_page(&state.sessions) => result.map_err(recovery_error)?,
        };
        let Some(page) = page else {
            break;
        };
        let lines = recovery_lines(page);
        if lines.is_empty() {
            continue;
        }
        evidence_events = evidence_events.saturating_add(lines.len());
        let transcript = context_compaction::CompactionTranscript {
            previous_summary: summary.take(),
            summary_input: lines,
            compacted_through_sequence: pin.boundary,
            event_count: evidence_events,
            estimated_reclaimable_tokens: 0,
        };
        summary = Some(
            Box::pin(context_compaction::collect_compaction_summary(
                state,
                request.session_id,
                selection,
                &transcript,
                command_context.as_deref_mut(),
                cancel_state,
            ))
            .await?,
        );
    }
    let summary = summary
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| {
            CompactionError::Provider(
                "no canonical evidence available to reconstruct summary".into(),
            )
        })?;
    context_compaction::validate_summary_publication(
        state,
        request.session_id,
        selection,
        pin.boundary,
        &summary,
        0,
        cancel_state,
    )
    .await?;
    let marker = commit_rebuilt_summary(
        state,
        request.session_id,
        &mut reader,
        summary,
        command_context,
        cancel_state,
    )
    .await?;
    Ok(format!(
        "rebuilt summary marker #{} from canonical history through #{}; replaced marker #{} without moving its boundary",
        marker.sequence, pin.boundary, pin.marker_sequence
    ))
}

async fn commit_rebuilt_summary(
    state: &ServerState,
    session_id: SessionId,
    reader: &mut SummaryRecoveryReader,
    summary: String,
    command_context: Option<&mut RuntimeCommandContext<'_>>,
    cancel_state: &TurnCancelState,
) -> Result<bcode_session_models::SessionEvent, CompactionError> {
    let pin = reader.pin();
    reader
        .validate_commit(&state.sessions)
        .await
        .map_err(recovery_error)?;
    if let Some(context) = command_context {
        service_cancel_commands(
            state,
            session_id,
            context.cancel_commands,
            context.followup_commands,
            context.queued_followups,
        )
        .await;
    }
    let marker_commit = cancel_state.marker_commit.lock().await;
    if cancel_state.is_cancelled() {
        return Err(CompactionError::Cancelled);
    }
    if state.sessions.current_context_epoch(session_id).await? != pin.context_epoch {
        return Err(recovery_error(SummaryRecoveryError::Conflict));
    }
    let marker = state
        .sessions
        .append_context_compacted(session_id, summary, pin.boundary)
        .await?;
    publish_session_event(state, &marker).await;
    state.invalidate_session_continuations(session_id).await;
    drop(marker_commit);
    Ok(marker)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(session_id: SessionId, marker: u64) -> SessionSummaryRebuildRequest {
        SessionSummaryRebuildRequest {
            session_id,
            expected_marker: marker,
            budget: bcode_session_models::SessionSummaryRebuildBudget::default(),
        }
    }

    #[tokio::test]
    async fn actual_request_allowance_covers_all_phases_without_upfront_overreservation() {
        assert!(reserve_provider_request().is_ok());
        PROVIDER_REQUESTS_LEFT
            .scope(std::cell::Cell::new(4), async {
                // Ordinary synthesis, bounded repair, consolidation, final consolidation.
                for expected in (0..4).rev() {
                    reserve_provider_request().unwrap();
                    assert_eq!(PROVIDER_REQUESTS_LEFT.with(std::cell::Cell::get), expected);
                }
                assert!(reserve_provider_request().is_err());
                assert_eq!(PROVIDER_REQUESTS_LEFT.with(std::cell::Cell::get), 0);
            })
            .await;
        PROVIDER_REQUESTS_LEFT
            .scope(std::cell::Cell::new(1), async {
                reserve_provider_request().unwrap();
                assert!(reserve_provider_request().is_err());
            })
            .await;
    }

    #[tokio::test]
    async fn explicit_rebuild_replaces_stale_marker_at_same_boundary() {
        let sessions = SessionManager::default();
        let session = sessions
            .create_session(None, std::env::temp_dir())
            .await
            .unwrap();
        let source = sessions
            .append_event(
                session.id,
                SessionEventKind::UserMessage {
                    client_id: ClientId::new(),
                    text: "Read only; inspect parser.".into(),
                    admission: bcode_session_models::TurnAdmissionMetadata::default(),
                },
            )
            .await
            .unwrap();
        let old = sessions
            .append_context_compacted(
                session.id,
                "STALE_UNAUTHORIZED_WRITE".into(),
                source.sequence,
            )
            .await
            .unwrap();
        let tail = sessions
            .append_event(
                session.id,
                SessionEventKind::AssistantMessage {
                    text: "uncompacted tail".into(),
                },
            )
            .await
            .unwrap();
        let state = crate::tests::test_server_state_with_fake_provider(sessions);
        let selection = SessionModelSelection {
            provider_plugin_id: Some("bcode.fake-provider".into()),
            model_id: Some("fake-model".into()),
            ..SessionModelSelection::default()
        };
        let phase = Arc::new(Mutex::new(SessionRuntimePhase::Idle));
        let current = Arc::new(Mutex::new(None));
        let (_followup_tx, mut followup_rx) = mpsc::channel(4);
        let (_steering_tx, mut steering_rx) = mpsc::channel(4);
        let (_cancel_tx, mut cancel_rx) = mpsc::channel(4);
        let queued = AtomicUsize::new(0);
        let result = process_rebuild_summary_command(
            &state,
            Arc::clone(&phase),
            &mut followup_rx,
            &mut steering_rx,
            &mut cancel_rx,
            &queued,
            Arc::clone(&current),
            ClientId::new(),
            selection,
            request(session.id, old.sequence),
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
        assert!(current.lock().await.is_none());
        assert!(matches!(*phase.lock().await, SessionRuntimePhase::Idle));
        let context = state
            .sessions
            .model_context_events(session.id)
            .await
            .unwrap();
        drop(state);
        assert!(context.iter().any(|event| event.sequence == tail.sequence));
        let fresh = context
            .iter()
            .find_map(|event| match &event.kind {
                SessionEventKind::ContextCompacted {
                    summary,
                    compacted_through_sequence,
                } => Some((summary, *compacted_through_sequence)),
                _ => None,
            })
            .unwrap();
        assert_eq!(fresh.1, source.sequence);
        assert!(!fresh.0.contains("STALE_UNAUTHORIZED_WRITE"));
    }

    #[tokio::test]
    async fn cancelled_or_stale_rebuild_publishes_no_marker() {
        let sessions = SessionManager::default();
        let session = sessions
            .create_session(None, std::env::temp_dir())
            .await
            .unwrap();
        let source = sessions
            .append_event(
                session.id,
                SessionEventKind::SystemMessage {
                    text: "canonical".into(),
                },
            )
            .await
            .unwrap();
        let old = sessions
            .append_context_compacted(session.id, "stale".into(), source.sequence)
            .await
            .unwrap();
        let state = crate::tests::test_server_state_with_fake_provider(sessions);
        let cancel = TurnCancelState::default();
        cancel.close();
        let result = Box::pin(rebuild_session_summary(
            &state,
            request(session.id, old.sequence),
            &SessionModelSelection::default(),
            None,
            &cancel,
        ))
        .await;
        assert!(matches!(result, Err(CompactionError::Cancelled)));
        assert_eq!(
            state
                .sessions
                .current_context_epoch(session.id)
                .await
                .unwrap(),
            old.sequence
        );
        let result = Box::pin(rebuild_session_summary(
            &state,
            request(session.id, source.sequence),
            &SessionModelSelection::default(),
            None,
            &TurnCancelState::default(),
        ))
        .await;
        assert!(result.is_err());
        let epoch = state
            .sessions
            .current_context_epoch(session.id)
            .await
            .unwrap();
        drop(state);
        assert_eq!(epoch, old.sequence);
    }
}
