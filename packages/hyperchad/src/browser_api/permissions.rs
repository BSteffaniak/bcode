//! Browser checkpoint observations retain opaque canonical CAS identities.
//! Only bounded scoped observations supply decisions; presentation fields never
//! substitute for server-issued checkpoint digests.
use bcode_session_models::{
    ConditionalPermissionDecision, MAX_SCOPED_PERMISSION_CHECKPOINTS, PermissionSummary,
    ScopedPermissionCheckpoint, ScopedPermissionLookup, ScopedPermissionQuery, SessionId,
};
use bcode_session_view_models::{PermissionView, SessionViewSnapshot};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Default, PartialEq, Eq)]
pub(super) struct PermissionObservation {
    pub checkpoints: BTreeMap<String, ScopedPermissionCheckpoint>,
    pub batches: BTreeSet<String>,
}

fn view(permission: &PermissionSummary) -> PermissionView {
    PermissionView {
        permission_id: permission.permission_id.clone(),
        session_id: Some(permission.session_id),
        tool_call_id: permission.tool_call_id.clone(),
        tool_name: permission.tool_name.clone(),
        arguments_json: permission.arguments_json.clone(),
        batch: permission.batch.as_ref().map(|batch| {
            bcode_session_view_models::PermissionBatchView {
                batch_id: batch.batch_id.clone(),
                call_index: batch.call_index,
                call_count: batch.call_count,
            }
        }),
        agent_id: permission.agent_id.clone(),
        title: Some(format!("Permission requested: {}", permission.tool_name)),
        policy_source: permission.policy_source.clone(),
        detail: permission.policy_reason.clone(),
        resolved: false,
        approved: None,
        can_remember: permission.can_remember_policy,
    }
}

impl PermissionObservation {
    pub fn decision(
        &self,
        session_id: SessionId,
        permission_id: Option<&str>,
        batch_id: Option<&str>,
        approved: bool,
        remember: bool,
    ) -> Result<ConditionalPermissionDecision, &'static str> {
        let expected = if let Some(batch_id) = batch_id {
            if remember || !self.batches.contains(batch_id) {
                return Err("stale_control");
            }
            self.checkpoints
                .values()
                .filter(|checkpoint| {
                    checkpoint
                        .permission
                        .batch
                        .as_ref()
                        .is_some_and(|batch| batch.batch_id == batch_id)
                })
                .map(|checkpoint| checkpoint.identity.clone())
                .collect::<Vec<_>>()
        } else {
            let checkpoint = self
                .checkpoints
                .get(permission_id.ok_or("invalid_control_identity")?)
                .ok_or("stale_control")?;
            if checkpoint.permission.batch.is_some()
                || (remember && !checkpoint.permission.can_remember_policy)
            {
                return Err("stale_control");
            }
            vec![checkpoint.identity.clone()]
        };
        if expected.is_empty() || expected.len() > MAX_SCOPED_PERMISSION_CHECKPOINTS {
            return Err("stale_control");
        }
        Ok(ConditionalPermissionDecision {
            session_id,
            batch_id: batch_id.map(str::to_owned),
            expected,
            approved,
            remember,
        })
    }
}

impl crate::HyperChadAppState {
    pub(super) async fn browser_permission_observation(
        &self,
        session_id: SessionId,
        snapshot: &mut SessionViewSnapshot,
    ) -> Result<PermissionObservation, &'static str> {
        // One bounded session page. Never scan global pending checkpoints or
        // combine separately timed pages to invent a complete batch.
        let ScopedPermissionLookup::Ready { checkpoints, .. } = self
            .client
            .scoped_permissions(ScopedPermissionQuery {
                session_id,
                batch_id: None,
                after_permission_id: None,
                limit: MAX_SCOPED_PERMISSION_CHECKPOINTS,
            })
            .await
            .map_err(|_| "permission_state_unavailable")?
        else {
            return Err("permission_state_unavailable");
        };
        let mut observation = PermissionObservation::default();
        let mut bytes = 0usize;
        let batch_ids = checkpoints
            .iter()
            .filter_map(|checkpoint| {
                checkpoint
                    .permission
                    .batch
                    .as_ref()
                    .map(|batch| batch.batch_id.clone())
            })
            .collect::<BTreeSet<_>>();
        for checkpoint in checkpoints {
            bytes = bytes.saturating_add(
                serde_json::to_vec(&checkpoint)
                    .map_err(|_| "permission_state_unavailable")?
                    .len(),
            );
            observation
                .checkpoints
                .insert(checkpoint.identity.permission_id.clone(), checkpoint);
        }
        for batch_id in batch_ids {
            let result = self
                .client
                .scoped_permissions(ScopedPermissionQuery {
                    session_id,
                    batch_id: Some(batch_id.clone()),
                    after_permission_id: None,
                    limit: MAX_SCOPED_PERMISSION_CHECKPOINTS,
                })
                .await
                .map_err(|_| "permission_state_unavailable")?;
            if let ScopedPermissionLookup::Ready {
                checkpoints,
                next_after: None,
            } = result
            {
                let additional = checkpoints
                    .iter()
                    .filter(|checkpoint| {
                        !observation
                            .checkpoints
                            .contains_key(&checkpoint.identity.permission_id)
                    })
                    .count();
                let batch_bytes = serde_json::to_vec(&checkpoints)
                    .map_err(|_| "permission_state_unavailable")?
                    .len();
                if observation.checkpoints.len().saturating_add(additional)
                    > MAX_SCOPED_PERMISSION_CHECKPOINTS
                    || bytes.saturating_add(batch_bytes)
                        > bcode_session_models::MAX_SCOPED_PERMISSION_BYTES
                {
                    continue;
                }
                bytes = bytes.saturating_add(batch_bytes);
                for checkpoint in checkpoints {
                    observation
                        .checkpoints
                        .insert(checkpoint.identity.permission_id.clone(), checkpoint);
                }
                observation.batches.insert(batch_id);
            }
        }
        snapshot.permissions = observation
            .checkpoints
            .values()
            // No decision-bearing presentation for a partial canonical batch.
            .filter(|checkpoint| {
                checkpoint
                    .permission
                    .batch
                    .as_ref()
                    .is_none_or(|batch| observation.batches.contains(&batch.batch_id))
            })
            .map(|checkpoint| view(&checkpoint.permission))
            .collect();
        Ok(observation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkpoint(session: SessionId, id: &str) -> ScopedPermissionCheckpoint {
        ScopedPermissionCheckpoint {
            identity: bcode_session_models::PermissionCheckpointIdentity {
                permission_id: id.into(),
                checkpoint_digest: "a".repeat(64),
            },
            permission: PermissionSummary {
                permission_id: id.into(),
                interaction_route: None,
                session_id: session,
                tool_call_id: "call".into(),
                tool_name: "tool".into(),
                arguments_json: "{}".into(),
                batch: None,
                agent_id: "build".into(),
                policy_source: None,
                policy_reason: None,
                can_remember_policy: false,
            },
        }
    }

    #[test]
    fn decisions_copy_exact_checkpoint_and_reject_incomplete_or_individual_batch_authority() {
        let session = SessionId::new();
        let mut observation = PermissionObservation::default();
        let mut first = checkpoint(session, "p1");
        observation.checkpoints.insert("p1".into(), first.clone());
        let decision = observation
            .decision(session, Some("p1"), None, true, false)
            .unwrap();
        assert_eq!(decision.expected, vec![first.identity.clone()]);
        assert!(
            observation
                .decision(session, Some("p1"), None, true, true)
                .is_err()
        );
        first.permission.batch = Some(bcode_session_models::PermissionBatchCorrelation {
            batch_id: "batch".into(),
            call_index: 0,
            call_count: 2,
        });
        let mut second = checkpoint(session, "p2");
        second.permission.batch = Some(bcode_session_models::PermissionBatchCorrelation {
            batch_id: "batch".into(),
            call_index: 1,
            call_count: 2,
        });
        observation.checkpoints.insert("p1".into(), first.clone());
        observation.checkpoints.insert("p2".into(), second.clone());
        assert!(
            observation
                .decision(session, Some("p1"), None, true, false)
                .is_err()
        );
        assert!(
            observation
                .decision(session, None, Some("batch"), true, false)
                .is_err()
        );
        observation.batches.insert("batch".into());
        let decision = observation
            .decision(session, None, Some("batch"), true, false)
            .unwrap();
        assert_eq!(decision.expected, vec![first.identity, second.identity]);
        assert!(
            observation
                .decision(session, None, Some("batch"), true, true)
                .is_err()
        );
    }
}
