//! Bounded disposable observations of committed session lifecycle facts.

use bcode_session_models::{
    RuntimeWorkStatus, SessionActivityState, SessionEvent, SessionEventKind, SessionId,
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Default)]
struct ActivityEvidence {
    sequence: Option<u64>,
    complete: bool,
    permissions: BTreeSet<String>,
    exchanges: BTreeSet<String>,
    submissions: BTreeSet<String>,
    outcome: Option<RuntimeWorkStatus>,
    latest_work: Option<bcode_session_models::WorkId>,
}

impl ActivityEvidence {
    fn observe(&mut self, event: &SessionEvent) {
        if let Some(sequence) = self.sequence {
            if event.sequence <= sequence {
                return;
            }
            if event.sequence != sequence.saturating_add(1) {
                self.complete = false;
            }
        } else {
            self.complete = event.sequence == 0
                && matches!(event.kind, SessionEventKind::SessionCreated { .. });
        }
        self.sequence = Some(event.sequence);
        match &event.kind {
            SessionEventKind::PermissionRequested { permission_id, .. } => {
                self.permissions.insert(permission_id.clone());
            }
            SessionEventKind::PermissionResolved { permission_id, .. } => {
                self.permissions.remove(permission_id);
            }
            SessionEventKind::ToolExchangeRequested { request } => {
                self.exchanges.insert(request.exchange_id.clone());
            }
            SessionEventKind::ToolExchangeResolved { event } => {
                self.exchanges.remove(&event.exchange_id);
            }
            SessionEventKind::SubmissionAccepted { submission } => {
                self.submissions.insert(submission.id.0.clone());
            }
            SessionEventKind::SubmissionActivated { submission_id, .. }
            | SessionEventKind::SubmissionCancelled { submission_id } => {
                self.submissions.remove(&submission_id.0);
            }
            SessionEventKind::ModelTurnStarted { .. } => {
                self.outcome = None;
                self.latest_work = None;
            }
            SessionEventKind::RuntimeWorkStarted { work_id, .. } => {
                self.outcome = None;
                self.latest_work = Some(work_id.clone());
            }
            SessionEventKind::RuntimeWorkFinished {
                work_id, status, ..
            } if status.is_terminal()
                && self
                    .latest_work
                    .as_ref()
                    .is_none_or(|latest| latest == work_id) =>
            {
                self.outcome = Some(*status);
            }
            _ => {}
        }
        // Overflow permanently degrades this disposable observation. It must never manufacture idle.
        for ids in [
            &mut self.permissions,
            &mut self.exchanges,
            &mut self.submissions,
        ] {
            if ids.len() > 128 || ids.iter().any(|id| id.len() > 1024) {
                ids.clear();
                self.complete = false;
            }
        }
    }

    fn states(&self) -> (Vec<SessionActivityState>, bool) {
        use SessionActivityState as State;
        let mut states = Vec::new();
        if !self.permissions.is_empty() {
            states.push(State::PermissionWait);
        }
        if !self.exchanges.is_empty() {
            states.push(State::InputWait);
        }
        if !self.submissions.is_empty() {
            states.push(State::Queued);
        }
        if states.is_empty() {
            match self.outcome {
                Some(RuntimeWorkStatus::Failed | RuntimeWorkStatus::TimedOut) => {
                    states.push(State::Failed);
                }
                Some(RuntimeWorkStatus::Cancelled) => states.push(State::Interrupted),
                _ => {}
            }
        }
        (states, self.complete)
    }
}

#[derive(Debug, Default)]
pub struct ActivityObservations {
    sessions: BTreeMap<SessionId, ActivityEvidence>,
}

impl ActivityObservations {
    pub fn observe(&mut self, event: &SessionEvent) {
        if !self.sessions.contains_key(&event.session_id) && self.sessions.len() >= 128 {
            self.sessions.pop_first();
        }
        self.sessions
            .entry(event.session_id)
            .or_default()
            .observe(event);
    }

    pub fn evidence(&self, id: SessionId) -> bcode_session_models::SessionActivityEvidence {
        let observed = self.sessions.get(&id);
        bcode_session_models::SessionActivityEvidence {
            running: 0,
            queued: observed.map_or(0, |facts| {
                u32::try_from(facts.submissions.len()).unwrap_or(u32::MAX)
            }),
            permission_waits: observed.map_or(0, |facts| {
                u32::try_from(facts.permissions.len()).unwrap_or(u32::MAX)
            }),
            input_waits: observed.map_or(0, |facts| {
                u32::try_from(facts.exchanges.len()).unwrap_or(u32::MAX)
            }),
            counts_complete: observed.is_some_and(|facts| facts.complete),
            observed_event_sequence: observed.and_then(|facts| facts.sequence),
            // Raw tool names/arguments and arbitrary labels are not normalized public operation labels.
            current_operation: None,
        }
    }

    pub fn states(&self, id: SessionId) -> (Vec<SessionActivityState>, bool) {
        self.sessions
            .get(&id)
            .map_or_else(|| (Vec::new(), false), ActivityEvidence::states)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(id: SessionId, sequence: u64, kind: SessionEventKind) -> SessionEvent {
        SessionEvent {
            session_id: id,
            sequence,
            kind,
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            timestamp_ms: 0,
            provenance: None,
        }
    }

    fn accepted() -> SessionEventKind {
        use bcode_session_models::{
            ClientId, PromptSubmission, SubmissionId, SubmissionPlacement, SubmissionTarget,
            TurnAdmissionMetadata, TurnId,
        };
        SessionEventKind::SubmissionAccepted {
            submission: PromptSubmission {
                id: SubmissionId("queued".into()),
                client_id: ClientId::new(),
                text: "input".into(),
                admission: TurnAdmissionMetadata::default(),
                requested_placement: SubmissionPlacement::FollowUp,
                target: SubmissionTarget::OwnTurn {
                    turn_id: TurnId("turn".into()),
                },
                execution_descriptor: None,
            },
        }
    }

    #[test]
    fn canonical_zero_creation_and_submission_lifecycle_has_complete_empty_evidence() {
        let id = SessionId::new();
        let mut observations = ActivityObservations::default();
        observations.observe(&event(
            id,
            0,
            SessionEventKind::SessionCreated {
                name: None,
                working_directory: std::path::PathBuf::from("/"),
            },
        ));
        assert_eq!(observations.states(id), (Vec::new(), true));
        observations.observe(&event(id, 1, accepted()));
        assert_eq!(
            observations.states(id),
            (vec![SessionActivityState::Queued], true)
        );
        observations.observe(&event(
            id,
            2,
            SessionEventKind::SubmissionActivated {
                submission_id: bcode_session_models::SubmissionId("queued".into()),
                turn_id: bcode_session_models::TurnId("turn".into()),
                client_id: bcode_session_models::ClientId::new(),
                text: "input".into(),
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
            },
        ));
        observations.observe(&event(
            id,
            3,
            SessionEventKind::RuntimeWorkFinished {
                work_id: bcode_session_models::WorkId::new("turn"),
                status: RuntimeWorkStatus::Completed,
                finished_at_ms: None,
                message: None,
            },
        ));
        // Empty complete evidence is not completion/acceptance; only an admitted idle
        // scheduler may adapt it to Idle at the application boundary.
        assert_eq!(observations.states(id), (Vec::new(), true));
    }

    #[test]
    fn permission_wait_and_normal_submission_queue_are_concurrent_facts() {
        let id = SessionId::new();
        let mut observations = ActivityObservations::default();
        observations.observe(&event(
            id,
            0,
            SessionEventKind::SessionCreated {
                name: None,
                working_directory: std::path::PathBuf::from("/"),
            },
        ));
        observations.observe(&event(id, 1, accepted()));
        observations.observe(&event(
            id,
            2,
            SessionEventKind::PermissionRequested {
                permission_id: "permission".into(),
                tool_call_id: "tool".into(),
                producer_plugin_id: None,
                tool_name: "test".into(),
                arguments_json: "{}".into(),
                batch: None,
                policy_source: None,
                policy_reason: None,
            },
        ));
        assert_eq!(
            observations.states(id),
            (
                vec![
                    SessionActivityState::PermissionWait,
                    SessionActivityState::Queued
                ],
                true
            )
        );
        observations.observe(&event(
            id,
            4,
            SessionEventKind::PermissionResolved {
                permission_id: "permission".into(),
                approved: true,
            },
        ));
        assert!(!observations.states(id).1);
    }

    #[test]
    fn stale_terminal_delivery_cannot_replace_newer_observation() {
        let id = SessionId::new();
        let mut observations = ActivityObservations::default();
        let failed = |sequence| {
            event(
                id,
                sequence,
                SessionEventKind::RuntimeWorkFinished {
                    work_id: bcode_session_models::WorkId::new("work"),
                    status: RuntimeWorkStatus::Failed,
                    finished_at_ms: None,
                    message: None,
                },
            )
        };
        observations.observe(&failed(4));
        observations.observe(&event(
            id,
            5,
            SessionEventKind::SubmissionCancelled {
                submission_id: bcode_session_models::SubmissionId("submission".into()),
            },
        ));
        observations.observe(&event(
            id,
            6,
            SessionEventKind::RuntimeWorkFinished {
                work_id: bcode_session_models::WorkId::new("new-work"),
                status: RuntimeWorkStatus::Completed,
                finished_at_ms: None,
                message: None,
            },
        ));
        observations.observe(&failed(4));
        assert_eq!(observations.states(id), (Vec::new(), false));
    }

    #[test]
    fn later_commit_from_old_work_cannot_replace_new_work_outcome() {
        let id = SessionId::new();
        let mut observations = ActivityObservations::default();
        observations.observe(&event(
            id,
            0,
            SessionEventKind::SessionCreated {
                name: None,
                working_directory: std::path::PathBuf::from("/"),
            },
        ));
        observations.observe(&event(
            id,
            1,
            SessionEventKind::RuntimeWorkStarted {
                work_id: bcode_session_models::WorkId::new("new"),
                kind: bcode_session_models::RuntimeWorkKind::ModelTurn,
                label: "new turn".into(),
                tool_call_id: None,
                plugin_id: None,
                service_interface: None,
                operation: None,
                parent_work_id: None,
                started_at_ms: None,
                cancellable: false,
            },
        ));
        observations.observe(&event(
            id,
            2,
            SessionEventKind::RuntimeWorkFinished {
                work_id: bcode_session_models::WorkId::new("old"),
                status: RuntimeWorkStatus::Failed,
                finished_at_ms: None,
                message: None,
            },
        ));
        assert_eq!(observations.states(id), (Vec::new(), true));
    }

    #[test]
    fn permission_queue_evidence_counts_are_own_and_checkpointed() {
        let id = SessionId::new();
        let mut observations = ActivityObservations::default();
        observations.observe(&event(
            id,
            0,
            SessionEventKind::SessionCreated {
                name: None,
                working_directory: std::path::PathBuf::from("/"),
            },
        ));
        observations.observe(&event(id, 1, accepted()));
        let evidence = observations.evidence(id);
        assert_eq!(evidence.queued, 1);
        assert_eq!(evidence.observed_event_sequence, Some(1));
        assert!(evidence.counts_complete);
        assert!(!observations.evidence(SessionId::new()).counts_complete);
        assert!(evidence.current_operation.is_none());
    }

    #[test]
    fn cache_eviction_and_gaps_remain_unknown() {
        let mut observations = ActivityObservations::default();
        for _ in 0..256 {
            let id = SessionId::new();
            observations.observe(&event(
                id,
                7,
                SessionEventKind::SubmissionCancelled {
                    submission_id: bcode_session_models::SubmissionId("submission".into()),
                },
            ));
            assert!(!observations.states(id).1);
        }
        assert_eq!(observations.sessions.len(), 128);
        assert!(!observations.states(SessionId::new()).1);
    }
}
