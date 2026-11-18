//! A session's runtime permission mode: clients step a session into or out of permission
//! bypass while it runs.
//!
//! * **Daemon memory only.** The mode set for a session lives in [`ServerState`] and is gone
//!   when the daemon restarts; a session with none set behaves as `Enforce`.
//! * **Admission.** A turn admitted while the session is in bypass is admitted in bypass
//!   ([`stamp`]); a turn that asks for bypass itself keeps asking.
//! * **Bypass needs both.** A turn skips discretionary permission policy only while it was
//!   admitted in bypass and the session has not stepped out since. A turn starting after the
//!   session stepped out, or resumed after a daemon restart without the session being stepped
//!   in again, starts revoked ([`starts_revoked`]). Stepping out also revokes the running turn
//!   at its next tool call, unless `permissions.bypass_step_out = "next_turn"` ([`set`]).

use crate::{ServerState, TurnCancelState};
use bcode_config::permissions::BypassStepOut;
use bcode_session_models::{
    SessionId, SessionLiveEventKind, TurnExecutionOptions, TurnPermissionMode,
};

/// Earliest execution-options schema version that may carry a permission mode other than
/// `Enforce`.
const PERMISSION_MODE_SCHEMA_VERSION: u32 = 4;

/// The mode a client set for `session_id` since the daemon started, if any.
fn set_mode(state: &ServerState, session_id: SessionId) -> Option<TurnPermissionMode> {
    state
        .session_permission_modes
        .lock()
        .ok()
        .and_then(|modes| modes.get(&session_id).copied())
}

/// The session's runtime permission mode: what turns admitted from now on run with.
pub fn current(state: &ServerState, session_id: SessionId) -> TurnPermissionMode {
    set_mode(state, session_id).unwrap_or_default()
}

/// Admit `execution` in bypass when the session is in bypass. A turn that asks for bypass
/// keeps it; options too old to carry a permission mode stay as they are.
pub fn stamp(state: &ServerState, session_id: SessionId, execution: &mut TurnExecutionOptions) {
    if execution.permission_mode == TurnPermissionMode::Enforce
        && execution.schema_version >= PERMISSION_MODE_SCHEMA_VERSION
        && set_mode(state, session_id) == Some(TurnPermissionMode::Bypass)
    {
        execution.permission_mode = TurnPermissionMode::Bypass;
    }
}

/// Whether a turn admitted in bypass that starts now must run without it: the session stepped
/// out, or the turn resumes after a daemon restart and the session was not stepped in again.
pub fn starts_revoked(state: &ServerState, session_id: SessionId, recovering: bool) -> bool {
    match set_mode(state, session_id) {
        Some(TurnPermissionMode::Bypass) => false,
        Some(TurnPermissionMode::Enforce) => true,
        None => recovering,
    }
}

/// Start a turn whose options are `execution`: when it was admitted in bypass that no longer
/// applies ([`starts_revoked`]), revoke it on `cancel_state` and record the turn as enforcing,
/// so messages sent from now on can steer it.
pub fn start_turn(
    state: &ServerState,
    session_id: SessionId,
    recovering: bool,
    cancel_state: &TurnCancelState,
    execution: &mut TurnExecutionOptions,
) {
    if execution.permission_mode == TurnPermissionMode::Bypass
        && starts_revoked(state, session_id, recovering)
    {
        cancel_state.revoke_bypass();
        execution.permission_mode = TurnPermissionMode::Enforce;
    }
}

/// Set the session's runtime permission mode, tell every client viewing it, and return it.
///
/// Stepping out revokes the running turn's bypass at once, unless the session's configuration
/// says `permissions.bypass_step_out = "next_turn"`; queued turns are revoked when they start.
pub async fn set(
    state: &ServerState,
    session_id: SessionId,
    mode: TurnPermissionMode,
) -> TurnPermissionMode {
    if let Ok(mut modes) = state.session_permission_modes.lock() {
        modes.insert(session_id, mode);
    }
    if mode == TurnPermissionMode::Enforce
        && state
            .session_config(session_id)
            .await
            .permissions
            .bypass_step_out
            == BypassStepOut::Immediate
    {
        revoke_running_turn(state, session_id).await;
    }
    tracing::info!(%session_id, ?mode, "session permission mode set");
    let _ = state
        .sessions
        .publish_live_event(
            session_id,
            SessionLiveEventKind::PermissionModeChanged { mode },
        )
        .await;
    mode
}

/// Revoke the running turn's bypass, if it runs in bypass, and record it as enforcing.
async fn revoke_running_turn(state: &ServerState, session_id: SessionId) {
    let Some(handle) = state
        .session_runtimes
        .lock()
        .await
        .get(&session_id)
        .cloned()
    else {
        return;
    };
    if let Some(turn) = handle.current_turn.lock().await.as_mut()
        && let Some(execution) = turn.execution.as_mut()
        && execution.permission_mode == TurnPermissionMode::Bypass
    {
        turn.cancel_state.revoke_bypass();
        execution.permission_mode = TurnPermissionMode::Enforce;
    }
}

/// Forget the mode set for `session_id` (the session went away).
pub fn forget(state: &ServerState, session_id: SessionId) {
    if let Ok(mut modes) = state.session_permission_modes.lock() {
        modes.remove(&session_id);
    }
}
