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
    // Only the runtime owner determines live work. Never scan all durable Running rows.
    let (owners, next_after) = state
        .runtime_work
        .active_page(
            session_id,
            None,
            super::runtime_work::MAX_RUNTIME_WORK_PAGE_ITEMS,
            super::runtime_work::MAX_RUNTIME_WORK_PAGE_BYTES,
        )
        .await
        .map_err(|_| bcode_session::SessionError::UnsupportedProjectionWindow)?;
    if next_after.is_some() {
        return Err(bcode_session::SessionError::UnsupportedProjectionWindow);
    }
    let runtime_work = if owners.is_empty() {
        Vec::new()
    } else {
        state
            .sessions
            .runtime_work_by_ids(
                session_id,
                owners.iter().map(|owner| owner.work_id.clone()).collect(),
                super::runtime_work::MAX_RUNTIME_WORK_PAGE_BYTES,
            )
            .await?
    };
    let mut initial_events = Vec::new();
    for owner in owners {
        let Some(work) = runtime_work
            .iter()
            .find(|work| work.work_id == owner.work_id)
        else {
            return Err(bcode_session::SessionError::UnsupportedProjectionWindow);
        };
        if super::runtime_work::encoded_len(
            &(
                &work.work_id,
                &work.label,
                &work.parent_work_id,
                &work.message,
            ),
            super::runtime_work::MAX_RUNTIME_WORK_PAGE_BYTES,
        )
        .is_none()
        {
            return Err(bcode_session::SessionError::UnsupportedProjectionWindow);
        }
        let mut work = work.clone();
        work.status = owner.status;
        initial_events.extend(super::runtime_work_projection_to_events(session_id, work));
        if super::runtime_work::encoded_len(
            &initial_events,
            super::runtime_work::MAX_RUNTIME_WORK_PAGE_BYTES,
        )
        .is_none()
        {
            return Err(bcode_session::SessionError::UnsupportedProjectionWindow);
        }
    }
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
/// Compatibility-only full inventory; this operation is not a bounded read.
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
        local_owner_only: Some(true),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_work::{CancellationHandle, MAX_RUNTIME_WORK_PAGE_ITEMS, RuntimeWorkSpec};
    use bcode_session_models::{RuntimeWorkKind, RuntimeWorkStatus, SessionEventKind};
    use std::sync::{Arc, atomic::AtomicUsize};

    async fn fixture() -> (tempfile::TempDir, ServerState, SessionId) {
        let root = tempfile::tempdir().unwrap();
        let sessions = bcode_session::SessionManager::persistent(root.path()).unwrap();
        let session = sessions
            .create_session(None, root.path().to_path_buf())
            .await
            .unwrap();
        (root, crate::tests::test_server_state(sessions), session.id)
    }

    fn spec(id: &str, label: String) -> RuntimeWorkSpec {
        RuntimeWorkSpec::new(
            WorkId::new(id),
            RuntimeWorkKind::Tool,
            label,
            CancellationHandle::Test(Arc::new(AtomicUsize::new(0))),
        )
    }

    #[tokio::test]
    async fn bounded_subscription_excludes_unowned_and_other_session_work_and_receives_later_events()
     {
        let (_root, state, session) = fixture().await;
        crate::register_runtime_work(&state, session, spec("stale", "stale".into())).await;
        state
            .runtime_work
            .finish(session, &WorkId::new("stale"))
            .await;
        crate::register_runtime_work(&state, session, spec("owned", "owned".into())).await;
        state
            .runtime_work
            .start(SessionId::new(), spec("other", "other".into()))
            .await;
        let mut watch = subscribe(&state, session).await.unwrap();
        assert_eq!(watch.initial_events.len(), 1);
        assert!(matches!(&watch.initial_events[0].kind,
            SessionEventKind::RuntimeWorkStarted { work_id, .. } if *work_id == WorkId::new("owned")));
        let event = state
            .sessions
            .append_event(
                session,
                SessionEventKind::RuntimeWorkFinished {
                    work_id: WorkId::new("owned"),
                    status: RuntimeWorkStatus::Completed,
                    finished_at_ms: None,
                    message: None,
                },
            )
            .await
            .unwrap();
        let received = watch.subscription.events.recv().await.unwrap();
        assert_eq!(received.sequence, event.sequence);
    }

    #[tokio::test]
    async fn bounded_subscription_rejects_incomplete_or_oversized_initial_inventory() {
        let (_root, state, session) = fixture().await;
        for index in 0..=MAX_RUNTIME_WORK_PAGE_ITEMS {
            state
                .runtime_work
                .start(session, spec(&format!("work-{index:04}"), "work".into()))
                .await;
        }
        assert!(matches!(
            subscribe(&state, session).await,
            Err(bcode_session::SessionError::UnsupportedProjectionWindow)
        ));
        let (_root, state, session) = fixture().await;
        state
            .runtime_work
            .start(
                session,
                spec(
                    "huge",
                    "x".repeat(crate::runtime_work::MAX_RUNTIME_WORK_PAGE_BYTES + 1),
                ),
            )
            .await;
        assert!(matches!(
            subscribe(&state, session).await,
            Err(bcode_session::SessionError::UnsupportedProjectionWindow)
        ));
        let (_root, state, session) = fixture().await;
        // Owner-only registration has no trustworthy canonical lifecycle; never omit it.
        state
            .runtime_work
            .start(session, spec("missing-history", "work".into()))
            .await;
        assert!(subscribe(&state, session).await.is_err());
    }
}
