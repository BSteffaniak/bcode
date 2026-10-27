//! Complete, non-dispatch request projections for compaction capacity planning.
use super::{
    CompactionError, ModelMessage, ModelRequestTargetInput, ModelTurnRequest, ServerState,
    SessionEventKind, SessionId, SessionModelSelection, StaticModelTurnContext, TurnCancelState,
    assemble_model_turn_request, context_compaction, execution_turn_id,
    prepare_static_model_turn_context_inner, resolve_model_request_target, turn_execution_options,
};
use std::cell::RefCell;

#[derive(Clone)]
struct Seed {
    trigger: bcode_session_models::SessionEvent,
    execution: bcode_session_models::TurnExecutionOptions,
    round: u32,
    static_context: StaticModelTurnContext,
    request_only: Vec<ModelMessage>,
}

tokio::task_local! {
    static SEED: RefCell<Option<Seed>>;
}

pub async fn scope<F: std::future::Future>(future: F) -> F::Output {
    if SEED.try_with(|_| ()).is_ok() {
        future.await
    } else {
        SEED.scope(RefCell::new(None), future).await
    }
}

pub fn remember(
    trigger: &bcode_session_models::SessionEvent,
    execution: &bcode_session_models::TurnExecutionOptions,
    round: u32,
    static_context: &StaticModelTurnContext,
    request_only: &[ModelMessage],
) {
    let _ = SEED.try_with(|slot| {
        let mut projected_static_context = static_context.clone();
        // Captured request-only suffix already includes recovery and retry guidance.
        projected_static_context.recovery_context = None;
        *slot.borrow_mut() = Some(Seed {
            trigger: trigger.clone(),
            execution: execution.clone(),
            round,
            static_context: projected_static_context,
            request_only: request_only.to_vec(),
        });
    });
}

pub fn request_only() -> Vec<ModelMessage> {
    SEED.try_with(|slot| slot.borrow().as_ref().map(|seed| seed.request_only.clone()))
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// Build using the real assembler, but never dispatch or perform optional-context writes.
/// Manual commands prepare current static context without retaining an environment snapshot.
pub async fn project(
    state: &ServerState,
    session_id: SessionId,
    selection: &SessionModelSelection,
    history: &[bcode_session_models::SessionEvent],
    cancel: &TurnCancelState,
) -> Result<ModelTurnRequest, CompactionError> {
    if cancel.is_cancelled() {
        return Err(CompactionError::Cancelled);
    }
    let config = state.session_config(session_id).await;
    let seed = SEED.try_with(|slot| slot.borrow().clone()).ok().flatten();
    let (trigger, execution, round, static_context) = if let Some(seed) = seed {
        (
            seed.trigger,
            seed.execution,
            seed.round,
            seed.static_context,
        )
    } else {
        let trigger = history
            .iter()
            .rev()
            .find(|event| {
                matches!(
                    event.kind,
                    SessionEventKind::UserMessage { .. }
                        | SessionEventKind::SubmissionActivated { .. }
                )
            })
            .or_else(|| history.last())
            .cloned()
            .ok_or_else(|| {
                CompactionError::Provider(
                    "complete request projection requires activated history".into(),
                )
            })?;
        let execution = turn_execution_options(&trigger);
        let target = Box::pin(resolve_model_request_target(
            state,
            ModelRequestTargetInput {
                session_id: Some(session_id),
                provider_plugin_id: selection.provider_plugin_id.as_deref(),
                selected_model_id: selection.model_id.as_deref(),
                provider_context: &selection.provider_context,
                native_default: selection.native_default,
            },
        ))
        .await
        .map_err(|error| CompactionError::Provider(error.to_string()))?;
        let mut context = Box::pin(prepare_static_model_turn_context_inner(
            state,
            session_id,
            &execution_turn_id(&trigger),
            trigger.sequence,
            &execution,
            &config,
            &target,
            None,
            selection,
            false,
            None,
            cancel,
            true,
        ))
        .await?;
        context.optional_context_owner = None;
        let _ = SEED.try_with(|slot| {
            *slot.borrow_mut() = Some(Seed {
                trigger: trigger.clone(),
                execution: execution.clone(),
                round: 0,
                static_context: context.clone(),
                request_only: Vec::new(),
            });
        });
        (trigger, execution, 0, context)
    };
    let policy =
        context_compaction::automatic_compaction_policy(state, selection, &config.model.compaction)
            .await;
    let prepared = Box::pin(assemble_model_turn_request(
        state,
        session_id,
        &trigger,
        &execution,
        round,
        selection.provider_plugin_id.as_deref(),
        selection.model_id.as_deref(),
        None,
        selection,
        &policy,
        &static_context,
        &config,
        history,
        history.last().map_or(0, |event| event.sequence),
        true,
    ))
    .await?;
    Ok(prepared.request)
}

/// Preserve the exact tail and normal summary framing through the shared assembler.
pub fn replaced_history(
    history: &[bcode_session_models::SessionEvent],
    boundary: u64,
    summary: &str,
) -> Result<Vec<bcode_session_models::SessionEvent>, CompactionError> {
    let mut marker = history
        .iter()
        .find(|event| event.sequence == boundary)
        .or_else(|| history.first())
        .cloned()
        .ok_or_else(|| CompactionError::Provider("selected boundary is missing".into()))?;
    marker.sequence = boundary;
    marker.kind = SessionEventKind::ContextCompacted {
        summary: summary.to_owned(),
        compacted_through_sequence: boundary,
    };
    let mut retained = vec![marker];
    retained.extend(
        history
            .iter()
            .filter(|event| {
                if event.sequence <= boundary {
                    return false;
                }
                !matches!(event.kind,
            SessionEventKind::ContextCompacted { compacted_through_sequence, .. }
                | SessionEventKind::ProviderContextCompacted { compacted_through_sequence, .. }
                if compacted_through_sequence <= boundary)
            })
            .cloned(),
    );
    Ok(retained)
}

#[cfg(test)]
mod tests {
    use super::replaced_history;
    use crate::{SessionEventKind, SessionId, session_events_to_model_messages_with_limit};
    fn event(sequence: u64, kind: SessionEventKind) -> bcode_session_models::SessionEvent {
        bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id: SessionId::new(),
            provenance: None,
            kind,
        }
    }

    #[test]
    fn reconstruction_replaces_old_marker_and_preserves_complete_tail() {
        let history = vec![
            event(
                20,
                SessionEventKind::ContextCompacted {
                    summary: "stale".into(),
                    compacted_through_sequence: 12,
                },
            ),
            event(
                21,
                SessionEventKind::AssistantMessage {
                    text: "tail evidence".into(),
                },
            ),
        ];
        let summary = "complete carry ".repeat(700);
        let retained = replaced_history(&history, 12, &summary).unwrap();
        assert_eq!(retained.len(), 2);
        let messages = session_events_to_model_messages_with_limit(&retained, 2_000);
        let rendered = serde_json::to_string(&messages).unwrap();
        assert!(rendered.contains(&summary));
        assert!(rendered.contains("tail evidence"));
        assert!(!rendered.contains("stale"));
    }

    #[test]
    fn ordinary_projection_preserves_tail_verbatim_without_clipping_summary() {
        let history = vec![
            event(
                1,
                SessionEventKind::AssistantMessage {
                    text: "source".into(),
                },
            ),
            event(
                2,
                SessionEventKind::AssistantMessage {
                    text: "tail".into(),
                },
            ),
        ];
        let summary = "長い証拠".repeat(2_000);
        let retained = replaced_history(&history, 1, &summary).unwrap();
        assert!(
            matches!(&retained[0].kind, SessionEventKind::ContextCompacted { summary: carry, .. } if carry == &summary)
        );
        assert_eq!(
            serde_json::to_value(&retained[1]).unwrap(),
            serde_json::to_value(&history[1]).unwrap()
        );
    }
}
