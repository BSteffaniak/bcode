//! Actor-serialized managed tool identity claims over ordinary canonical tool events.
use super::{SessionActor, SessionError, SessionEventKind};
use bcode_session_models::{
    MANAGED_TOOL_CLAIM_KEY, ManagedToolClaimEvent, ManagedToolClaimOutcome, ManagedToolClaimRecord,
    ManagedToolIdentity,
};

pub enum Action {
    Claim(ManagedToolIdentity),
    Inspect(String),
    Start {
        identity: ManagedToolIdentity,
        claim_sequence: u64,
    },
    Complete {
        identity: ManagedToolIdentity,
        claim_sequence: u64,
        result: Box<bcode_session_models::ToolInvocationResultRecord>,
    },
}
pub enum Reply {
    Claim(ManagedToolClaimOutcome),
    Record(Option<ManagedToolClaimRecord>),
    Committed,
}
fn invalid() -> SessionError {
    SessionError::EventSerialization("managed tool claim identity or owner mismatch".into())
}

impl SessionActor {
    pub(super) async fn managed_tool_action(
        &mut self,
        owner: bcode_session_models::ExternalHarnessOwner,
        action: Action,
        timestamp: u64,
    ) -> Result<Reply, SessionError> {
        self.validate_managed_tool_owner(&owner)?;
        self.ensure_session_db_for_write().await?;
        self.refresh_state_from_db_for_write().await?;
        let db = self.db.as_ref().ok_or_else(invalid)?;
        // Full durable owner comparison is performed by the existing harness projection.
        db.harness_state(&owner).await?.ok_or_else(invalid)?;
        let (id, identity) = match &action {
            Action::Claim(identity)
            | Action::Start { identity, .. }
            | Action::Complete { identity, .. } => {
                identity
                    .validate()
                    .map_err(SessionError::EventSerialization)?;
                if identity.owner != owner {
                    return Err(invalid());
                }
                (&identity.invocation_id, Some(identity))
            }
            Action::Inspect(id) => (id, None),
        };
        let retained = db.managed_tool_claim(id).await?;
        if retained.as_ref().is_some_and(|claim| {
            claim.identity.owner != owner
                || identity.is_some_and(|identity| identity != &claim.identity)
        }) {
            return Err(invalid());
        }
        match action {
            Action::Inspect(_) => Ok(Reply::Record(retained)),
            Action::Claim(identity) => {
                if let Some(claim) = retained {
                    if claim.result_sequence.is_some() {
                        let result = db.managed_tool_result(&claim).await?;
                        return Ok(Reply::Claim(ManagedToolClaimOutcome::Recorded {
                            claim,
                            result: Box::new(result),
                        }));
                    }
                    return Ok(Reply::Claim(ManagedToolClaimOutcome::Uncertain(claim)));
                }
                if db.tool_run(&identity.invocation_id).await?.is_some() {
                    return Err(invalid());
                }
                let transition = ManagedToolClaimEvent::Claimed {
                    identity: identity.clone(),
                };
                let event = self
                    .append_managed_tool_transition(transition, timestamp)
                    .await?;
                Ok(Reply::Claim(ManagedToolClaimOutcome::New(
                    ManagedToolClaimRecord {
                        identity,
                        claim_sequence: event.sequence,
                        started_sequence: None,
                        result_sequence: None,
                    },
                )))
            }
            Action::Start {
                identity,
                claim_sequence,
            } => {
                let claim = retained.ok_or_else(invalid)?;
                if claim.claim_sequence != claim_sequence
                    || claim.started_sequence.is_some()
                    || claim.result_sequence.is_some()
                {
                    return Err(invalid());
                }
                self.append_managed_tool_transition(
                    ManagedToolClaimEvent::Started {
                        identity,
                        claim_sequence,
                    },
                    timestamp,
                )
                .await?;
                Ok(Reply::Committed)
            }
            Action::Complete {
                identity,
                claim_sequence,
                result,
            } => {
                self.complete_managed_tool_claim(
                    retained.ok_or_else(invalid)?,
                    &identity,
                    claim_sequence,
                    *result,
                    timestamp,
                )
                .await
            }
        }
    }

    fn validate_managed_tool_owner(
        &mut self,
        owner: &bcode_session_models::ExternalHarnessOwner,
    ) -> Result<(), SessionError> {
        owner.validate().map_err(SessionError::EventSerialization)?;
        if owner.session_id != self.state.summary.id {
            return Err(invalid());
        }
        self.ensure_ownership()?;
        let lease = self.lease.as_ref().ok_or_else(invalid)?.owner();
        if lease.build_fingerprint.as_deref() != Some(owner.artifact_id.as_str())
            || lease.daemon_instance_id.as_deref() != Some(owner.daemon_id.as_str())
        {
            return Err(invalid());
        }
        Ok(())
    }

    async fn complete_managed_tool_claim(
        &mut self,
        claim: ManagedToolClaimRecord,
        identity: &ManagedToolIdentity,
        claim_sequence: u64,
        result: bcode_session_models::ToolInvocationResultRecord,
        timestamp: u64,
    ) -> Result<Reply, SessionError> {
        if claim.claim_sequence != claim_sequence
            || claim.started_sequence.is_none()
            || result.invocation_id != identity.invocation_id
        {
            return Err(invalid());
        }
        if claim.result_sequence.is_some() {
            let db = self.db.as_ref().ok_or_else(invalid)?;
            if db.managed_tool_result(&claim).await? != result {
                return Err(invalid());
            }
            return Ok(Reply::Committed);
        }
        self.append_tool_invocation_result(result, timestamp)
            .await?;
        Ok(Reply::Committed)
    }

    async fn append_managed_tool_transition(
        &mut self,
        transition: ManagedToolClaimEvent,
        timestamp: u64,
    ) -> Result<bcode_session_models::SessionEvent, SessionError> {
        let (id, stage) = match &transition {
            ManagedToolClaimEvent::Claimed { identity } => (
                &identity.invocation_id,
                bcode_session_models::ToolInvocationLifecycleStage::Waiting,
            ),
            ManagedToolClaimEvent::Started { identity, .. } => (
                &identity.invocation_id,
                bcode_session_models::ToolInvocationLifecycleStage::Started,
            ),
        };
        self.append_event(
            SessionEventKind::ToolInvocationLifecycle {
                event: bcode_session_models::ToolInvocationLifecycleEvent {
                    invocation_id: id.clone(),
                    sequence: 0,
                    stage,
                    message: None,
                    metadata: serde_json::json!({(MANAGED_TOOL_CLAIM_KEY):transition}),
                },
            },
            None,
            timestamp,
        )
        .await
    }
}
