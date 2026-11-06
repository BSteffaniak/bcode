//! TUI ownership of shallow inspector handles. The inspector domain owns host cleanup.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use bcode_client::BcodeClient;
use bcode_session_models::SessionId;
use bcode_xray_inspector::{InspectorHandle, LaunchOptions};
use tokio::sync::watch;

// Explicit presentation resource policy, not a limit on sessions or workflow lifetime.
const MAX_INSPECTORS: usize = 8;

#[derive(Default)]
struct State {
    handles: BTreeMap<SessionId, InspectorHandle>,
    // A failed/canceled stop does not prove the old observation was revoked.
    unresolved: BTreeSet<SessionId>,
}

impl State {
    fn begin(&mut self, session_id: SessionId) -> Result<Option<InspectorHandle>, &'static str> {
        if self.unresolved.contains(&session_id) {
            return Err("X-ray cleanup unconfirmed; replacement is unavailable");
        }
        if !self.handles.contains_key(&session_id)
            && self.handles.len() + self.unresolved.len() >= MAX_INSPECTORS
        {
            return Err("X-ray inspector limit reached; close this TUI to release observations");
        }
        let previous = self.handles.remove(&session_id);
        self.unresolved.insert(session_id);
        Ok(previous)
    }
}

pub struct Inspectors {
    state: Arc<Mutex<State>>,
    shutdown: watch::Sender<bool>,
}

impl Default for Inspectors {
    fn default() -> Self {
        let (shutdown, _) = watch::channel(false);
        Self {
            state: Arc::default(),
            shutdown,
        }
    }
}

impl Inspectors {
    pub fn shutdown(&self) {
        self.shutdown.send_replace(true);
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .handles
            .clear();
    }

    pub fn request(&self, session_id: SessionId) -> OpenRequest {
        OpenRequest {
            state: self.state.clone(),
            shutdown: self.shutdown.subscribe(),
            session_id,
        }
    }
}

impl Drop for Inspectors {
    fn drop(&mut self) {
        // Drop only signals domain-owned observation cleanup; never session cancellation.
        self.shutdown();
    }
}

/// Owned request passed to the background effect runner, not a running inspector.
pub struct OpenRequest {
    state: Arc<Mutex<State>>,
    shutdown: watch::Receiver<bool>,
    pub session_id: SessionId,
}

impl OpenRequest {
    pub async fn run(mut self, client: BcodeClient) -> &'static str {
        if *self.shutdown.borrow() {
            return "X-ray launch canceled";
        }
        // All opens share one queued effect key. Never hold the state lock across
        // domain work. The unresolved reservation also fences unexpected overlap.
        let outcome = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .begin(self.session_id);
        let previous = match outcome {
            Ok(previous) => previous,
            Err(message) => return message,
        };
        let launch = async {
            if let Some(previous) = previous
                && previous.stop().await.is_err()
            {
                return Err("X-ray cleanup unconfirmed; no replacement opened");
            }
            bcode_xray_inspector::start(client, self.session_id, LaunchOptions::default())
                .await
                .map_err(|_| "X-ray could not open; observation startup failed")
        };
        let result = tokio::select! {
            biased;
            _ = self.shutdown.changed() => return "X-ray launch canceled",
            result = launch => result,
        };
        match result {
            Ok(handle) => {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                // Disposal can occur while start settles. Drop the late handle instead
                // of resurrecting an observation owned by a disposed TUI.
                if *self.shutdown.borrow() {
                    drop(handle);
                    "X-ray launch canceled"
                } else {
                    state.unresolved.remove(&self.session_id);
                    state.handles.insert(self.session_id, handle);
                    "X-ray opened"
                }
            }
            // Keep the reservation: a failed start/stop may have observation effects.
            Err(message) => message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(index: u128) -> SessionId {
        format!("{index:032x}").parse().unwrap()
    }

    #[test]
    fn unsettled_launches_are_bounded_and_cannot_overlap_or_retry() {
        let mut state = State::default();
        for index in 1..=MAX_INSPECTORS {
            assert!(
                state
                    .begin(session(u128::try_from(index).unwrap()))
                    .unwrap()
                    .is_none()
            );
        }
        assert!(state.begin(session(1)).is_err());
        assert!(state.begin(session(100)).is_err());
        assert!(state.handles.is_empty());
        assert_eq!(state.unresolved.len(), MAX_INSPECTORS);
    }

    #[tokio::test]
    async fn ordinary_effect_runner_keeps_canceled_launch_secret_safe() {
        let owner = Inspectors::default();
        let target = session(1);
        let request = owner.request(target);
        drop(owner);
        let result = crate::effects::TuiEffect::OpenXray { request }
            .run_for_test(BcodeClient::default_endpoint())
            .await;
        assert!(matches!(
            result.daemon_observation(),
            crate::effects::DaemonObservation::None
        ));
        match result {
            crate::effects::TuiEffectResult::XrayOpened {
                session_id,
                message,
            } => {
                assert_eq!(session_id, target);
                assert_eq!(message, "X-ray launch canceled");
            }
            _ => panic!("wrong effect completion"),
        }
    }

    #[tokio::test]
    async fn disposed_owner_cancels_queued_launch_without_contacting_application() {
        let owner = Inspectors::default();
        let request = owner.request(session(1));
        drop(owner);
        assert_eq!(
            request.run(BcodeClient::default_endpoint()).await,
            "X-ray launch canceled"
        );
    }
}
