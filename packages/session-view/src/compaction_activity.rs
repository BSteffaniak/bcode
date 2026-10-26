//! Renderer-neutral adaptation of normalized compaction diagnostics into live activity.
//!
//! This is transient progress, not a durable compaction marker or a completion claim.

use bcode_session_models::{SessionTraceEvent, SessionTracePayload, SessionTracePhase};
use bcode_session_view_models::SessionRuntimeViewState;

/// Accepted change to the current turn's transient compaction activity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactionActivityUpdate {
    /// Start or refresh compaction with bounded, public diagnostic text only.
    Active { detail: String },
    /// Compaction stopped; model request preparation may continue.
    PreparingModelRequest,
}

/// Project an already ordered trace against the authoritative runtime state.
///
/// `compaction_active` describes the consumer's current semantic activity. Diagnostics
/// cannot start activity, and terminal traces cannot change unrelated activity. Only
/// traces for the exact active, non-cancelling turn are admitted. Callers must apply
/// normal session sequence admission before this adapter; it is not a history reducer.
/// Automatic compaction producers must retain the originating turn ID across async
/// work. An unattributed (`turn_id: None`) session diagnostic cannot safely be bound
/// to the current turn: it may be late work from a cancelled or replaced turn.
/// Reasons are deliberately opaque: checkpoint acceptance, resume and repair remain
/// progress until an explicit finished/skipped phase. No summary or provider payload
/// is inspected or displayed.
// Option already carries must-use semantics; repository policy avoids duplicate attributes.
#[allow(clippy::must_use_candidate)]
pub fn compaction_activity_update(
    trace: &SessionTraceEvent,
    runtime: &SessionRuntimeViewState,
    compaction_active: bool,
) -> Option<CompactionActivityUpdate> {
    let SessionTracePayload::ContextCompaction { message, .. } = &trace.payload else {
        return None;
    };
    if runtime.cancelling
        || runtime.active_turn_id.is_none()
        || trace.turn_id.as_deref() != runtime.active_turn_id.as_deref()
    {
        return None;
    }
    match trace.phase {
        SessionTracePhase::ContextCompactionStarted => Some(CompactionActivityUpdate::Active {
            detail: diagnostic_detail(message.as_deref().unwrap_or("older context")),
        }),
        SessionTracePhase::ContextCompactionDiagnostic if compaction_active => {
            let message = message
                .as_deref()
                .filter(|message| !message.trim().is_empty())?;
            Some(CompactionActivityUpdate::Active {
                detail: diagnostic_detail(message),
            })
        }
        SessionTracePhase::ContextCompactionFinished
        | SessionTracePhase::ContextCompactionSkipped
            if compaction_active =>
        {
            Some(CompactionActivityUpdate::PreparingModelRequest)
        }
        _ => None,
    }
}

fn diagnostic_detail(message: &str) -> String {
    // Bound retained diagnostic work independently of terminal geometry. Layout remains
    // renderer-owned; control characters cannot inject additional presentation lines.
    message
        .chars()
        .take(512)
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime(turn: Option<&str>, cancelling: bool) -> SessionRuntimeViewState {
        SessionRuntimeViewState {
            active_turn_id: turn.map(str::to_owned),
            cancelling,
            ..SessionRuntimeViewState::default()
        }
    }

    fn trace(phase: SessionTracePhase, message: Option<&str>) -> SessionTraceEvent {
        SessionTraceEvent {
            timestamp_ms: 1,
            turn_id: Some("turn-1".to_owned()),
            phase,
            payload: SessionTracePayload::ContextCompaction {
                reason: "opaque_future_reason".to_owned(),
                projected_context_chars: 99,
                compacted: false,
                message: message.map(str::to_owned),
            },
        }
    }

    #[test]
    fn unattributed_producer_shape_requires_originating_turn_attribution() {
        for phase in [
            SessionTracePhase::ContextCompactionStarted,
            SessionTracePhase::ContextCompactionDiagnostic,
            SessionTracePhase::ContextCompactionFinished,
            SessionTracePhase::ContextCompactionSkipped,
        ] {
            // Session-scoped append_context_compaction_trace shape: all other
            // fields are usable, but it contains no evidence of turn ownership.
            let mut produced = trace(phase, Some("Accepted chunk; more context remains"));
            produced.turn_id = None;
            for runtime in [
                runtime(Some("turn-1"), false),
                runtime(Some("turn-1"), true),
                runtime(None, false),
                runtime(Some("turn-2"), false),
            ] {
                for active in [false, true] {
                    assert_eq!(
                        compaction_activity_update(&produced, &runtime, active),
                        None
                    );
                }
            }

            // Required automatic-producer contract: attach the ID captured at
            // operation entry, never whichever turn is active after an await.
            produced.turn_id = Some("turn-1".to_owned());
            let expected = match phase {
                SessionTracePhase::ContextCompactionFinished
                | SessionTracePhase::ContextCompactionSkipped => {
                    CompactionActivityUpdate::PreparingModelRequest
                }
                _ => CompactionActivityUpdate::Active {
                    detail: "Accepted chunk; more context remains".to_owned(),
                },
            };
            assert_eq!(
                compaction_activity_update(&produced, &runtime(Some("turn-1"), false), true),
                Some(expected)
            );
            assert_eq!(
                compaction_activity_update(&produced, &runtime(Some("turn-2"), false), true),
                None
            );
        }
    }

    #[test]
    fn accepted_chunks_resume_and_repairs_remain_active_until_finished() {
        let runtime = runtime(Some("turn-1"), false);
        assert_eq!(
            compaction_activity_update(
                &trace(
                    SessionTracePhase::ContextCompactionDiagnostic,
                    Some("not started")
                ),
                &runtime,
                false,
            ),
            None
        );
        assert_eq!(
            compaction_activity_update(
                &trace(SessionTracePhase::ContextCompactionStarted, None),
                &runtime,
                false,
            ),
            Some(CompactionActivityUpdate::Active {
                detail: "older context".to_owned()
            })
        );
        for message in [
            "Accepted chunk 1; more context remains",
            "Resuming checkpoint",
            "Repairing summary",
        ] {
            assert_eq!(
                compaction_activity_update(
                    &trace(
                        SessionTracePhase::ContextCompactionDiagnostic,
                        Some(message)
                    ),
                    &runtime,
                    true,
                ),
                Some(CompactionActivityUpdate::Active {
                    detail: message.to_owned()
                })
            );
        }
        for phase in [
            SessionTracePhase::ContextCompactionFinished,
            SessionTracePhase::ContextCompactionSkipped,
        ] {
            assert_eq!(
                compaction_activity_update(&trace(phase, None), &runtime, true),
                Some(CompactionActivityUpdate::PreparingModelRequest)
            );
        }
        assert_eq!(
            compaction_activity_update(
                &trace(
                    SessionTracePhase::ContextCompactionDiagnostic,
                    Some("late chunk")
                ),
                &runtime,
                false,
            ),
            None
        );
    }

    #[test]
    fn cancellation_terminal_and_restarted_turns_fence_stale_updates() {
        for runtime in [
            runtime(Some("turn-1"), true),
            runtime(None, false),
            runtime(Some("turn-2"), false),
        ] {
            for phase in [
                SessionTracePhase::ContextCompactionStarted,
                SessionTracePhase::ContextCompactionDiagnostic,
                SessionTracePhase::ContextCompactionFinished,
            ] {
                assert_eq!(
                    compaction_activity_update(&trace(phase, Some("stale")), &runtime, true),
                    None
                );
            }
        }
        let mut resumed = trace(
            SessionTracePhase::ContextCompactionStarted,
            Some("Resuming accepted chunks"),
        );
        resumed.turn_id = Some("turn-2".to_owned());
        assert!(matches!(
            compaction_activity_update(&resumed, &runtime(Some("turn-2"), false), false),
            Some(CompactionActivityUpdate::Active { .. })
        ));
    }

    #[test]
    fn progress_is_message_only_bounded_and_single_line() {
        let runtime = runtime(Some("turn-1"), false);
        let mut trace = trace(
            SessionTracePhase::ContextCompactionDiagnostic,
            Some(&format!("進捗\n{}", "🙂".repeat(600))),
        );
        if let SessionTracePayload::ContextCompaction { reason, .. } = &mut trace.payload {
            *reason = "never display this opaque metadata".repeat(1000);
        }
        let Some(CompactionActivityUpdate::Active { detail }) =
            compaction_activity_update(&trace, &runtime, true)
        else {
            panic!("active progress expected")
        };
        assert_eq!(detail.chars().count(), 512);
        assert!(detail.starts_with("進捗 🙂"));
        assert!(!detail.contains("metadata"));
        assert_eq!(
            compaction_activity_update(
                &self::trace(SessionTracePhase::ContextCompactionDiagnostic, None),
                &runtime,
                true
            ),
            None
        );
    }
}
