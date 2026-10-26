//! Automatic compaction safe points, bound to the original activated model turn.
//!
//! These checkpoints authorize only ownership-fenced restoration. An interrupted provider
//! operation is never inferred effect-free from a reservation or replayed as a new request.
use super::*;

#[cfg(test)]
#[path = "compaction_recovery_overflow_tests.rs"]
mod overflow_tests;
#[cfg(test)]
#[path = "compaction_recovery_tests.rs"]
mod tests;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Phase {
    Safe,
    Local { key: String, chunk: usize },
    Native,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub key: String,
    pub boundary: u64,
    pub first_kept_sequence: Option<u64>,
    pub planning_policy: context_compaction::CompactionPlanningPolicy,
    pub progress_requirement: Option<context_compaction::CompactionProgressRequirement>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverflowTrigger {
    code: String,
    message: String,
}

impl OverflowTrigger {
    #[must_use]
    pub fn capture(error: &bcode_model::ProviderError) -> Self {
        fn bounded(value: &str, limit: usize) -> String {
            value
                .chars()
                .filter(|character| !character.is_control())
                .take(limit)
                .collect()
        }
        Self {
            code: bounded(&error.code, 128),
            message: bounded(&error.message, 1024),
        }
    }

    #[must_use]
    pub fn valid(&self) -> bool {
        self.code.chars().count() <= 128
            && self.message.chars().count() <= 1024
            && !self
                .code
                .chars()
                .chain(self.message.chars())
                .any(char::is_control)
    }

    #[must_use]
    pub fn failure(&self, failure: impl std::fmt::Display) -> String {
        format!(
            "original overflow [{}]: {}; compaction recovery failed: {failure}",
            self.code, self.message
        )
    }
}

struct Binding {
    turn_id: String,
    checkpoint: provider_retry_recovery::RetryCheckpoint,
}

tokio::task_local! {
    static BINDING: Mutex<Binding>;
}

/// Protect overflow recovery after the failed provider round has already completed.
pub async fn protect_after_provider_error<F: std::future::Future>(
    state: &ServerState,
    session_id: SessionId,
    context: &ProviderErrorRetryContext<'_>,
    recovery: &ModelTurnRecoveryState,
    error: &bcode_model::ProviderError,
    operation: F,
) -> Result<F::Output, bcode_session::SessionError> {
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
    let mut rounds = ModelTurnRoundState::new(None);
    rounds.provider_round = context.provider_round.saturating_add(1);
    rounds.completed_tool_rounds = context.completed_tool_rounds;
    let mut checkpoint = provider_retry_recovery::compaction_checkpoint(
        context.trigger_event_sequence,
        (
            recovery,
            &rounds,
            context.segment_order,
            context.output_position,
        ),
    )?;
    checkpoint.overflow_trigger = Some(OverflowTrigger::capture(error));
    Box::pin(protect_checkpoint(
        state, session_id, turn_id, checkpoint, operation,
    ))
    .await
}

/// Protect one automatic compaction operation until its caller can resume ordinary dispatch.
///
/// Errors persisting or retiring the safe point must terminalize the turn before another provider
/// request. The supplied future must contain only compaction, never ordinary model/tool dispatch.
pub async fn protect<F: std::future::Future>(
    state: &ServerState,
    session_id: SessionId,
    turn_id: &str,
    activation_sequence: u64,
    counters: (&ModelTurnRecoveryState, &ModelTurnRoundState, u32, u64),
    operation: F,
) -> Result<F::Output, bcode_session::SessionError> {
    let checkpoint = provider_retry_recovery::compaction_checkpoint(activation_sequence, counters)?;
    Box::pin(protect_checkpoint(
        state, session_id, turn_id, checkpoint, operation,
    ))
    .await
}

async fn protect_checkpoint<F: std::future::Future>(
    state: &ServerState,
    session_id: SessionId,
    turn_id: &str,
    checkpoint: provider_retry_recovery::RetryCheckpoint,
    operation: F,
) -> Result<F::Output, bcode_session::SessionError> {
    provider_retry_recovery::persist(state, session_id, turn_id, Some(&checkpoint)).await?;
    let result = BINDING
        .scope(
            Mutex::new(Binding {
                turn_id: turn_id.into(),
                checkpoint,
            }),
            operation,
        )
        .await;
    provider_retry_recovery::clear(state, session_id, turn_id).await?;
    Ok(result)
}

/// Persist the phase before provider dispatch, or after validated carry has been saved.
pub async fn transition(
    state: &ServerState,
    session_id: SessionId,
    phase: Phase,
) -> Result<(), bcode_session::SessionError> {
    // Manual compaction deliberately has no automatic-turn recovery authority.
    let Ok(snapshot) = BINDING.try_with(|binding| {
        let mut binding = binding
            .try_lock()
            .expect("compaction binding has one writer");
        if !matches!(binding.checkpoint.compaction, Some(Phase::Native)) {
            binding.checkpoint.compaction = Some(phase);
        }
        (binding.turn_id.clone(), binding.checkpoint.clone())
    }) else {
        return Ok(());
    };
    provider_retry_recovery::persist(state, session_id, &snapshot.0, Some(&snapshot.1)).await
}

pub async fn record_plan(
    state: &ServerState,
    session_id: SessionId,
    plan: Plan,
) -> Result<(), bcode_session::SessionError> {
    let Ok((turn_id, checkpoint)) = BINDING.try_with(|binding| {
        let mut binding = binding
            .try_lock()
            .expect("compaction binding has one writer");
        binding.checkpoint.compaction_plan = Some(plan);
        (binding.turn_id.clone(), binding.checkpoint.clone())
    }) else {
        return Ok(());
    };
    provider_retry_recovery::persist(state, session_id, &turn_id, Some(&checkpoint)).await
}

/// Complete retained synthesis before rebuilding the ordinary generation request.
/// Called after retry recovery restores counters, and before that checkpoint is retired.
pub async fn resume(
    state: &ServerState,
    session_id: SessionId,
    turn_id: &str,
    selection: &SessionModelSelection,
    commands: &mut RuntimeCommandContext<'_>,
    cancel: &TurnCancelState,
) -> Result<(), context_compaction::CompactionError> {
    let Some(checkpoint) = provider_retry_recovery::load(state, session_id, turn_id).await? else {
        return Ok(());
    };
    let Some(plan) = checkpoint.compaction_plan.clone() else {
        return Ok(());
    };
    let trigger = checkpoint.overflow_trigger.clone();
    BINDING
        .scope(
            Mutex::new(Binding {
                turn_id: turn_id.into(),
                checkpoint,
            }),
            Box::pin(context_compaction::resume_recovery_plan(
                state, session_id, selection, &plan, commands, cancel,
            )),
        )
        .await
        .map_err(|error| match error {
            context_compaction::CompactionError::Cancelled => error,
            _ => match trigger {
                Some(trigger) => {
                    context_compaction::CompactionError::Provider(trigger.failure(&error))
                }
                None => error,
            },
        })
}

pub async fn can_resume(
    state: &ServerState,
    session_id: SessionId,
    phase: &Phase,
) -> Result<bool, bcode_session::SessionError> {
    match phase {
        Phase::Safe => Ok(true),
        Phase::Native => Ok(false),
        Phase::Local { key, chunk } => {
            context_compaction::accepted_recovery_chunk(state, session_id, key, *chunk).await
        }
    }
}
