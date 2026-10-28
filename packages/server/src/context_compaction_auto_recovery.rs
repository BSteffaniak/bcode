//! Bounded replacement of a settled incompatible operation, under the current compaction owner.
use super::{
    CompactionCompletion, CompactionError, CompactionPlanningPolicy, CompactionProgressRequirement,
    CompactionRecoveryReason, RuntimeCommandContext, ServerState, SessionId, SessionModelSelection,
    TurnCancelState, checkpoint, prepare_compaction_recovery_with_policy,
    restart_session_context_with_policy,
};

#[allow(clippy::too_many_arguments)]
pub(super) async fn recover_policy_change(
    state: &ServerState,
    session_id: SessionId,
    selection: &SessionModelSelection,
    commands: Option<&mut RuntimeCommandContext<'_>>,
    cancel: &TurnCancelState,
    first_kept_sequence: Option<u64>,
    progress: Option<CompactionProgressRequirement>,
    planning: CompactionPlanningPolicy,
) -> Result<CompactionCompletion, CompactionError> {
    let unavailable = || CompactionError::RecoveryRequired(CompactionRecoveryReason::PolicyChanged);
    if cancel.is_cancelled() {
        return Err(CompactionError::Cancelled);
    }
    let preparation = prepare_compaction_recovery_with_policy(
        state,
        selection,
        &bcode_session_models::SessionCompactionRecoveryDiscoveryRequest {
            version: 1,
            session_id,
            operation_id: uuid::Uuid::new_v4(),
            budget: bcode_session_models::SessionCompactionRestartBudget {
                max_provider_requests: 64,
                timeout_ms: 300_000,
            },
        },
        planning,
    )
    .await?;
    let request = preparation.restart.ok_or_else(unavailable)?;
    let checkpoint = checkpoint::load_archived_checkpoint(state, &request).await?;
    if !checkpoint.is_original_operation() && !checkpoint.needs_upgrade() {
        return Err(unavailable());
    }
    // The existing compare-and-supersede transaction preserves original bytes and counters,
    // verifies terminal request evidence, and binds the distinct replacement allowance.
    Box::pin(restart_session_context_with_policy(
        state,
        &request,
        selection,
        commands,
        cancel,
        Some(planning),
        first_kept_sequence,
        progress,
    ))
    .await
}
