//! Transport-neutral application operations for active runtime work.

use super::{ClientId, ServerState, WorkId};
use bcode_ipc::RuntimeWorkSnapshot;
use bcode_session_models::{SessionEvent, SessionId};

/// Return bounded durable runtime-work history without transport framing.
pub async fn history(
    state: &ServerState,
    session_id: SessionId,
    limit: usize,
) -> Result<Vec<SessionEvent>, bcode_session::SessionError> {
    let mut events = state
        .sessions
        .runtime_work_history(session_id, limit)
        .await?
        .into_iter()
        .flat_map(|work| super::runtime_work_projection_to_events(session_id, work))
        .collect::<Vec<_>>();
    if limit > 0 && events.len() > limit {
        events.drain(0..events.len() - limit);
    }
    Ok(events)
}

/// Return active runtime work for one session without transport framing.
pub async fn list(state: &ServerState, session_id: SessionId) -> Vec<RuntimeWorkSnapshot> {
    state.runtime_work.active_for_session(session_id).await
}

/// Request cancellation of registered runtime work without transport framing.
pub async fn cancel(
    state: &ServerState,
    session_id: SessionId,
    work_id: WorkId,
    client_id: Option<ClientId>,
) -> bool {
    super::cancel_registered_runtime_work(state, session_id, work_id, client_id).await
}
