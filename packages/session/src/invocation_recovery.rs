//! Bounded canonical observations used to decide whether an invocation can be recovered.

use crate::{SessionError, SessionManager};
use bcode_session_models::{SessionEvent, SessionEventKind, SessionId};

/// Canonical authorization observation, not permission to execute or retry a tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvocationRecoveryObservation {
    /// The complete observed interval ends with an unresolved permission request.
    WaitingForPermission { permission_id: String },
    /// A decision is recorded but no invocation lifecycle is observed. This alone does not
    /// prove absence of side effects and is not a dispatch permit.
    PermissionDecided {
        permission_id: String,
        approved: bool,
    },
    /// A canonical result already exists. Recovery must not invoke the tool again.
    Terminal,
    /// The bounded interval is incomplete, inconsistent, or cannot prove an authorization wait.
    Unverifiable,
}

/// Canonical records needed to reconstruct an authorization wait.
///
/// This is an observation at `generation`, not a dispatch permit. A caller must acquire
/// current execution ownership and revalidate generation, policy, and plugin preparation
/// before registering or resolving a recovered request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvocationRecoveryCheckpoint {
    generation: u64,
    turn: SessionEvent,
    request: SessionEvent,
    permission: SessionEvent,
}

impl InvocationRecoveryCheckpoint {
    /// Canonical session that owns this checkpoint.
    #[must_use]
    pub const fn session_id(&self) -> SessionId {
        self.request.session_id
    }

    /// Canonical history generation observed during validation.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Original admitted user turn.
    #[must_use]
    pub const fn turn(&self) -> &SessionEvent {
        &self.turn
    }

    /// Original positioned tool request.
    #[must_use]
    pub const fn request(&self) -> &SessionEvent {
        &self.request
    }

    /// Original unresolved permission request.
    #[must_use]
    pub const fn permission(&self) -> &SessionEvent {
        &self.permission
    }
}

impl SessionManager {
    /// Record a recovered permission decision only if canonical history is unchanged.
    ///
    /// The caller must hold current execution authority and authorize the decision. This
    /// method grants neither authority nor dispatch permission. Conflicts must be re-read;
    /// they must not be retried with a silently updated generation.
    ///
    /// # Errors
    /// Returns ownership/storage errors or [`SessionError::AppendGenerationChanged`].
    pub async fn append_recovered_permission_decision(
        &self,
        checkpoint: &InvocationRecoveryCheckpoint,
        approved: bool,
    ) -> Result<SessionEvent, SessionError> {
        let session_id = checkpoint.session_id();
        let SessionEventKind::PermissionRequested { permission_id, .. } =
            &checkpoint.permission.kind
        else {
            unreachable!("checkpoints are constructed only from canonical permission requests");
        };
        let handle = self.session_handle(session_id).await?;
        let event = handle
            .append_event_at_generation(
                SessionEventKind::PermissionResolved {
                    permission_id: permission_id.clone(),
                    approved,
                },
                checkpoint.generation,
                self.next_activity_timestamp_ms(),
            )
            .await?;
        let summary = handle.summary().await?;
        self.release_persistent_idle_session_resources(session_id)
            .await;
        self.publish_committed_mutation(event.clone(), summary);
        Ok(event)
    }

    /// Read the canonical turn and invocation records for an unresolved authorization wait.
    ///
    /// Only positioned requests with an exact canonical user-turn identity are accepted.
    /// Unpositioned requests, changed generations, incomplete reads, and request-only context
    /// cannot establish a recoverable continuation and return `None`.
    ///
    /// # Errors
    /// Returns session ownership, storage, and bounded history-read errors.
    pub async fn invocation_recovery_checkpoint(
        &self,
        session_id: SessionId,
        invocation_id: &str,
        request_sequence: u64,
        max_events: usize,
    ) -> Result<Option<InvocationRecoveryCheckpoint>, SessionError> {
        let generation = self.current_session_generation(session_id).await?;
        if max_events == 0 || request_sequence > generation {
            return Ok(None);
        }
        let events = self
            .session_events_range(
                session_id,
                request_sequence,
                generation,
                max_events.min(1024),
            )
            .await?;
        let InvocationRecoveryObservation::WaitingForPermission { permission_id } =
            observe(&events, invocation_id, request_sequence, generation)
        else {
            return Ok(None);
        };
        let Some(request) = events.first() else {
            return Ok(None);
        };
        let SessionEventKind::PositionedToolCallRequested {
            turn_id,
            producer_plugin_id: Some(_),
            working_directory: Some(_),
            ..
        } = &request.kind
        else {
            return Ok(None);
        };
        let Some(sequence) = turn_id
            .strip_prefix(&format!("{session_id}-"))
            .and_then(|value| value.parse::<u64>().ok())
        else {
            return Ok(None);
        };
        if sequence >= request_sequence || *turn_id != format!("{session_id}-{sequence}") {
            return Ok(None);
        }
        let mut turns = self
            .session_events_range(session_id, sequence, sequence, 1)
            .await?;
        let Some(turn) = turns.pop() else {
            return Ok(None);
        };
        let SessionEventKind::UserMessage { admission, .. } = &turn.kind else {
            return Ok(None);
        };
        if admission.validate().is_err() || admission.execution.request_context_id.is_some() {
            return Ok(None);
        }
        let permission = events.iter().find(|event| matches!(&event.kind,
            SessionEventKind::PermissionRequested { permission_id: id, .. } if id == &permission_id
        )).cloned();
        if events.iter().any(|event| {
            matches!(&event.kind,
                SessionEventKind::ModelTurnFinished { turn_id: finished, .. } if finished == turn_id
            )
        }) {
            return Ok(None);
        }
        if self.current_session_generation(session_id).await? != generation {
            return Ok(None);
        }
        Ok(permission.map(|permission| InvocationRecoveryCheckpoint {
            generation,
            turn,
            request: request.clone(),
            permission,
        }))
    }

    /// Observe one invocation from its canonical request through a pinned session generation.
    ///
    /// Reads at most `max_events` events. Missing history, concurrent changes, and exhausted
    /// bounds produce `Unverifiable`, never an inferred permission or safe retry.
    ///
    /// # Errors
    /// Returns session ownership, storage, and bounded history-read errors.
    pub async fn invocation_recovery_observation(
        &self,
        session_id: SessionId,
        invocation_id: &str,
        request_sequence: u64,
        max_events: usize,
    ) -> Result<InvocationRecoveryObservation, SessionError> {
        let generation = self.current_session_generation(session_id).await?;
        if max_events == 0 || request_sequence > generation {
            return Ok(InvocationRecoveryObservation::Unverifiable);
        }
        let events = self
            .session_events_range(
                session_id,
                request_sequence,
                generation,
                max_events.min(1024),
            )
            .await?;
        if self.current_session_generation(session_id).await? != generation {
            return Ok(InvocationRecoveryObservation::Unverifiable);
        }
        Ok(observe(
            &events,
            invocation_id,
            request_sequence,
            generation,
        ))
    }
}

fn observe(
    events: &[SessionEvent],
    invocation: &str,
    first: u64,
    last: u64,
) -> InvocationRecoveryObservation {
    use InvocationRecoveryObservation::{Terminal, Unverifiable, WaitingForPermission};
    if events.first().is_none_or(|event| event.sequence != first)
        || events.last().is_none_or(|event| event.sequence != last)
        || events
            .windows(2)
            .any(|pair| pair[0].sequence.checked_add(1) != Some(pair[1].sequence))
    {
        return Unverifiable;
    }
    let request = match &events[0].kind {
        SessionEventKind::ToolCallRequested {
            tool_call_id,
            tool_name,
            arguments_json,
            ..
        }
        | SessionEventKind::PositionedToolCallRequested {
            tool_call_id,
            tool_name,
            arguments_json,
            ..
        } if tool_call_id == invocation => (tool_name, arguments_json),
        _ => return Unverifiable,
    };
    let mut permission = None;
    let mut resolved = None;
    for event in &events[1..] {
        match &event.kind {
            SessionEventKind::PermissionRequested {
                tool_call_id,
                permission_id,
                tool_name,
                arguments_json,
                ..
            } if tool_call_id == invocation => {
                if permission.is_some() || (tool_name, arguments_json) != request {
                    return Unverifiable;
                }
                permission = Some(permission_id.clone());
            }
            SessionEventKind::PermissionResolved {
                permission_id,
                approved,
            } if permission.as_ref() == Some(permission_id) => {
                if resolved.is_some_and(|previous| previous != *approved) {
                    return Unverifiable;
                }
                resolved = Some(*approved);
            }
            SessionEventKind::ToolInvocationResultRecorded { record }
                if record.invocation_id == invocation =>
            {
                return Terminal;
            }
            SessionEventKind::ToolInvocationLifecycle { event }
                if event.invocation_id == invocation =>
            {
                return Unverifiable;
            }
            _ => {}
        }
    }
    match (permission, resolved) {
        (Some(permission_id), None) => WaitingForPermission { permission_id },
        (Some(permission_id), Some(approved)) => InvocationRecoveryObservation::PermissionDecided {
            permission_id,
            approved,
        },
        _ => Unverifiable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn positioned_checkpoint_reopens_exact_turn_request_and_permission() {
        let root = tempfile::tempdir().unwrap();
        let manager = SessionManager::persistent(root.path()).unwrap();
        let session = manager
            .create_session(None, root.path().to_path_buf())
            .await
            .unwrap();
        let turn = manager
            .append_event(
                session.id,
                SessionEventKind::UserMessage {
                    client_id: bcode_session_models::ClientId::new(),
                    text: "implement".into(),
                    admission: bcode_session_models::TurnAdmissionMetadata::default(),
                },
            )
            .await
            .unwrap();
        let request = manager
            .append_event(
                session.id,
                SessionEventKind::PositionedToolCallRequested {
                    turn_id: format!("{}-{}", session.id, turn.sequence),
                    output_position: bcode_session_models::TurnOutputPosition::new(0),
                    tool_call_id: "call".into(),
                    producer_plugin_id: Some("example.tool".into()),
                    tool_name: "example.write".into(),
                    arguments_json: "{}".into(),
                    working_directory: Some(root.path().to_path_buf()),
                },
            )
            .await
            .unwrap();
        let permission = manager
            .append_permission_requested(
                session.id,
                SessionEventKind::PermissionRequested {
                    permission_id: "permission".into(),
                    tool_call_id: "call".into(),
                    producer_plugin_id: Some("example.tool".into()),
                    tool_name: "example.write".into(),
                    arguments_json: "{}".into(),
                    batch: None,
                    policy_source: None,
                    policy_reason: None,
                },
            )
            .await
            .unwrap();
        let checkpoint = manager
            .invocation_recovery_checkpoint(session.id, "call", request.sequence, 16)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(checkpoint.turn, turn);
        assert_eq!(checkpoint.request, request);
        assert_eq!(checkpoint.permission, permission);
        manager.release_session_ownership(session.id).await.unwrap();
        drop(manager);
        let restored = SessionManager::persistent(root.path()).unwrap();
        assert_eq!(
            restored
                .invocation_recovery_checkpoint(session.id, "call", request.sequence, 16)
                .await
                .unwrap(),
            Some(checkpoint.clone())
        );
        restored
            .append_recovered_permission_decision(&checkpoint, false)
            .await
            .unwrap();
        assert!(matches!(
            restored
                .append_recovered_permission_decision(&checkpoint, true)
                .await,
            Err(SessionError::AppendGenerationChanged { .. })
        ));
        assert_eq!(
            restored
                .current_session_generation(session.id)
                .await
                .unwrap(),
            checkpoint.generation() + 1
        );
        assert!(
            restored
                .invocation_recovery_checkpoint(session.id, "call", request.sequence, 16)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn canonical_wait_observation_survives_reopen_and_rejects_incomplete_history() {
        let root = tempfile::tempdir().unwrap();
        let manager = SessionManager::persistent(root.path()).unwrap();
        let session = manager
            .create_session(None, root.path().to_path_buf())
            .await
            .unwrap();
        let request = manager
            .append_tool_call_requested(
                session.id,
                crate::AppendToolCallRequestedInput {
                    tool_call_id: "call".into(),
                    producer_plugin_id: Some("example.tool".into()),
                    tool_name: "example.write".into(),
                    arguments_json: "{}".into(),
                    working_directory: None,
                },
            )
            .await
            .unwrap();
        manager
            .append_permission_requested(
                session.id,
                SessionEventKind::PermissionRequested {
                    permission_id: "permission".into(),
                    tool_call_id: "call".into(),
                    producer_plugin_id: Some("example.tool".into()),
                    tool_name: "example.write".into(),
                    arguments_json: "{}".into(),
                    batch: None,
                    policy_source: None,
                    policy_reason: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            manager
                .invocation_recovery_observation(session.id, "call", request.sequence, 1)
                .await
                .unwrap(),
            InvocationRecoveryObservation::Unverifiable
        );
        assert_eq!(
            manager
                .invocation_recovery_observation(session.id, "other", request.sequence, 16)
                .await
                .unwrap(),
            InvocationRecoveryObservation::Unverifiable
        );
        assert_eq!(
            manager
                .invocation_recovery_observation(session.id, "call", request.sequence, 16)
                .await
                .unwrap(),
            InvocationRecoveryObservation::WaitingForPermission {
                permission_id: "permission".into()
            }
        );
        manager.release_session_ownership(session.id).await.unwrap();
        drop(manager);
        let restored = SessionManager::persistent(root.path()).unwrap();
        assert_eq!(
            restored
                .invocation_recovery_observation(session.id, "call", request.sequence, 16)
                .await
                .unwrap(),
            InvocationRecoveryObservation::WaitingForPermission {
                permission_id: "permission".into()
            }
        );
        restored
            .append_permission_resolved(session.id, "permission".into(), true)
            .await
            .unwrap();
        assert_eq!(
            restored
                .invocation_recovery_observation(session.id, "call", request.sequence, 16)
                .await
                .unwrap(),
            InvocationRecoveryObservation::PermissionDecided {
                permission_id: "permission".into(),
                approved: true
            }
        );
        restored
            .release_session_ownership(session.id)
            .await
            .unwrap();
        drop(restored);
        let restored = SessionManager::persistent(root.path()).unwrap();
        assert_decided_and_conflicting_history(&restored, session.id, request.sequence).await;
    }

    async fn assert_decided_and_conflicting_history(
        restored: &SessionManager,
        session_id: SessionId,
        request_sequence: u64,
    ) {
        assert_eq!(
            restored
                .invocation_recovery_observation(session_id, "call", request_sequence, 16)
                .await
                .unwrap(),
            InvocationRecoveryObservation::PermissionDecided {
                permission_id: "permission".into(),
                approved: true
            }
        );
        restored
            .append_permission_resolved(session_id, "permission".into(), false)
            .await
            .unwrap();
        assert_eq!(
            restored
                .invocation_recovery_observation(session_id, "call", request_sequence, 16)
                .await
                .unwrap(),
            InvocationRecoveryObservation::Unverifiable
        );
    }
}
