//! Narrow browser history windows. No raw event/log or caller-selected resource APIs.
use bcode_session_models::{
    ProjectionWindowAnchor, ProjectionWindowDirection, ProjectionWindowRequest,
};
use hyperchad::{renderer::Content, router::RouteRequest};
use serde::{Deserialize, Serialize};

use super::{BACKEND_TIMEOUT, BrowserApiState, ObservationGuard, error, parse_body};
use crate::HyperChadAppState;

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Direction {
    Older,
    Newer,
    Tail,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryRequest {
    session_id: bcode_session_models::SessionId,
    epoch: bcode_session_models::SessionId,
    revision: u64,
    direction: Direction,
}

pub(super) fn window_request(
    snapshot: &bcode_session_view_models::SessionViewSnapshot,
    direction: Direction,
) -> Result<ProjectionWindowRequest, &'static str> {
    let document = &snapshot.transcript;
    let (anchor, direction) = match direction {
        Direction::Older if document.has_older_history => (
            ProjectionWindowAnchor::BeforeSequence(
                document
                    .source_start_sequence
                    .ok_or("history_boundary_unavailable")?,
            ),
            ProjectionWindowDirection::Backward,
        ),
        Direction::Newer if document.has_newer_history => (
            ProjectionWindowAnchor::AfterSequence(
                document
                    .source_end_sequence
                    .ok_or("history_boundary_unavailable")?,
            ),
            ProjectionWindowDirection::Forward,
        ),
        Direction::Tail => return Ok(crate::hyperchad_projection_window_request()),
        _ => return Err("history_boundary_unavailable"),
    };
    Ok(crate::hyperchad_projection_window_request_for_anchor(
        anchor, direction,
    ))
}

impl HyperChadAppState {
    pub(super) async fn browser_window_snapshot(
        &self,
        session_id: bcode_session_models::SessionId,
        request: ProjectionWindowRequest,
    ) -> Result<bcode_session_view_models::SessionViewSnapshot, bcode_client::ClientError> {
        let mut connection = self.client.connect("bcode-browser-history").await?;
        let attached = crate::attach_hyperchad_projection_window_with_request(
            &mut connection,
            session_id,
            request,
        )
        .await?;
        crate::session_view_from_attached_history(
            &self.client,
            attached,
            &self.interaction_controllers,
            self.reasoning_presentation_policy,
        )
        .await
    }

    pub(super) async fn browser_history(&self, request: RouteRequest) -> Content {
        if !self.authorizes(&request) {
            return error("unauthorized");
        }
        let query: HistoryRequest = match parse_body(&request) {
            Ok(value) => value,
            Err(code) => return error(code),
        };
        let Ok(mut observations) = self.browser_api.try_lock() else {
            return error("browser_busy");
        };
        let guard = ObservationGuard {
            session_id: query.session_id,
            epoch: query.epoch,
            revision: query.revision,
        };
        let snapshot = match observations.guarded_snapshot(&guard) {
            Ok(value) => value,
            Err(code) => return error(code),
        };
        let window = match window_request(snapshot, query.direction) {
            Ok(value) => value,
            Err(code) => return error(code),
        };
        let Ok(Ok(mut snapshot)) = tokio::time::timeout(
            BACKEND_TIMEOUT,
            self.browser_window_snapshot(query.session_id, window.clone()),
        )
        .await
        else {
            return error("history_unavailable");
        };
        let Ok(Ok(permissions)) = tokio::time::timeout(
            BACKEND_TIMEOUT,
            self.browser_permission_observation(query.session_id, &mut snapshot),
        )
        .await
        else {
            return error("permission_state_unavailable");
        };
        let content = match observations.publish_scoped_observation(
            query.session_id,
            snapshot,
            Some(permissions),
            serde_json::from_str(&self.browser_policy).ok(),
        ) {
            Ok(content) => content,
            Err(code) => return error(code),
        };
        observations.windows.insert(query.session_id, window);
        content
    }
}

impl BrowserApiState {
    pub(super) fn guarded_snapshot(
        &self,
        guard: &ObservationGuard,
    ) -> Result<&bcode_session_view_models::SessionViewSnapshot, &'static str> {
        let (revision, snapshot) = self
            .snapshots
            .get(&guard.session_id)
            .ok_or("stale_observation")?;
        if self.epoch != guard.epoch || *revision != guard.revision {
            return Err("stale_observation");
        }
        Ok(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn observation_guard_rejects_foreign_epoch_revision_and_evicted_session() {
        let session_id = bcode_session_models::SessionId::new();
        let mut state = BrowserApiState::default();
        let revision = state
            .observe(
                session_id,
                bcode_session_view_models::SessionViewSnapshot::empty(),
            )
            .unwrap();
        let mut guard = ObservationGuard {
            session_id,
            epoch: state.epoch,
            revision,
        };
        assert!(state.guarded_snapshot(&guard).is_ok());
        guard.revision += 1;
        assert!(state.guarded_snapshot(&guard).is_err());
        guard.revision = revision;
        guard.epoch = bcode_session_models::SessionId::new();
        assert!(state.guarded_snapshot(&guard).is_err());
        guard.epoch = state.epoch;
        for _ in 0..super::super::MAX_CACHED_SESSIONS {
            state
                .observe(
                    bcode_session_models::SessionId::new(),
                    bcode_session_view_models::SessionViewSnapshot::empty(),
                )
                .unwrap();
        }
        assert!(state.guarded_snapshot(&guard).is_err());
    }

    #[test]
    fn history_uses_only_projected_boundaries_and_fixed_budgets() {
        let mut snapshot = bcode_session_view_models::SessionViewSnapshot::empty();
        assert!(window_request(&snapshot, Direction::Older).is_err());
        snapshot.transcript.has_older_history = true;
        assert!(window_request(&snapshot, Direction::Older).is_err());
        snapshot.transcript.source_start_sequence = Some(u64::MAX - 1);
        let request = window_request(&snapshot, Direction::Older).unwrap();
        assert_eq!(
            request.anchor,
            ProjectionWindowAnchor::BeforeSequence(u64::MAX - 1)
        );
        assert_eq!(
            request.limits,
            crate::hyperchad_projection_window_request().limits
        );
    }
}
