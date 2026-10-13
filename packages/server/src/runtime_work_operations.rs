//! Transport-neutral application operations for active runtime work.

use super::{ClientId, ServerState, WorkId};
use bcode_session_models::RuntimeWorkSnapshot;
use bcode_session_models::{SessionEvent, SessionId};

/// Prepared runtime-work observation, before a transport installs its forwarder.
pub struct RuntimeWorkSubscription {
    /// Initial projected durable work events.
    pub initial_events: Vec<SessionEvent>,
    /// Subscription acquired before reading the initial snapshot to avoid a delivery gap.
    pub subscription: bcode_session::SessionEventSubscription,
}

/// Subscribe before reading active work, without choosing a transport or spawning a forwarder.
///
/// # Errors
/// Returns the session error when subscription or active-work projection is unavailable.
pub async fn subscribe(
    state: &ServerState,
    session_id: SessionId,
) -> Result<RuntimeWorkSubscription, bcode_session::SessionError> {
    let subscription = state.sessions.subscribe_session_events(session_id).await?;
    // Durable projections retain historical observations, not execution ownership. A failed
    // terminal append (or an old daemon) can leave a Running row after its owner has ended.
    // Never revive that row as live work, and never repair history on this read path.
    let runtime_work = state.sessions.active_runtime_work(session_id).await?;
    let owners = state
        .runtime_work
        .active_for_session(session_id)
        .await
        .into_iter()
        .map(|owner| (owner.work_id, owner.status))
        .collect::<std::collections::BTreeMap<_, _>>();
    let initial_events = runtime_work
        .into_iter()
        .filter_map(|mut work| {
            work.status = *owners.get(&work.work_id)?;
            Some(work)
        })
        .flat_map(|work| super::runtime_work_projection_to_events(session_id, work))
        .collect();
    Ok(RuntimeWorkSubscription {
        initial_events,
        subscription,
    })
}

/// Return bounded durable runtime-work history without transport framing.
///
/// Limits are clamped to the session history read budget; zero requests one event,
/// never an unlimited persistence query.
///
/// # Errors
/// Returns the session error if bounded runtime-work history is unavailable.
pub async fn history(
    state: &ServerState,
    session_id: SessionId,
    limit: usize,
) -> Result<Vec<SessionEvent>, bcode_session::SessionError> {
    let limit = limit.clamp(1, bcode_session_models::MAX_SESSION_HISTORY_READ_EVENTS);
    let mut events = state
        .sessions
        .runtime_work_history(session_id, limit)
        .await?
        .into_iter()
        .flat_map(|work| super::runtime_work_projection_to_events(session_id, work))
        .collect::<Vec<_>>();
    // Projection rows are grouped by work, not by lifecycle event sequence.
    // Sort before trimming so overlapping work retains the latest events in this window.
    events.sort_by_key(|event| event.sequence);
    if events.len() > limit {
        events.drain(0..events.len() - limit);
    }
    Ok(events)
}

/// Return active runtime work for one session without transport framing.
pub async fn list(state: &ServerState, session_id: SessionId) -> Vec<RuntimeWorkSnapshot> {
    state.runtime_work.active_for_session(session_id).await
}

/// Return a bounded live-observation page from this daemon's runtime-work owner.
///
/// Limits are clamped to 1..=128. This does not discover foreign owners or recover
/// historical stages. Absence of a stage is unknown, not idle or completed.
/// Callers must apply the same session authorization as other runtime-work reads.
pub async fn observations(
    state: &ServerState,
    session_id: SessionId,
    after: Option<&WorkId>,
    limit: usize,
) -> bcode_session_models::RuntimeWorkObservationPage {
    let page = state
        .runtime_work
        .observation_page(session_id, after, limit)
        .await;
    bcode_session_models::RuntimeWorkObservationPage {
        entries: page
            .entries
            .into_iter()
            .map(|entry| bcode_session_models::RuntimeWorkObservationEntry {
                work_id: entry.work_id,
                observation: entry.stage.zip(entry.elapsed).map(|(stage, elapsed)| {
                    bcode_session_models::RuntimeWorkObservation {
                        stage,
                        elapsed_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
                    }
                }),
                cancellation_requested: entry.cancellation_requested,
            })
            .collect(),
        next_after: page.next_after,
    }
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
