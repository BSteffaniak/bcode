//! Transport-neutral application operations for permissions and pending tool exchanges.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::Mutex;

use super::{
    ClientId, PendingPermission, PendingPermissionBatch, PendingToolExchange, ServerState,
    SessionId, SkillToolDecision, SkillToolDecisionKey, ToolExchangeResolution, TurnCancelState,
    current_time_ms, publish_session_event,
};
use bcode_session_models::PendingToolExchangeSummary;

/// Authoritative pending store with indexes updated in the same critical section.
/// No mutable map access escapes: replacement also invalidates checkpoint identity.
#[derive(Debug, Default)]
pub struct PendingPermissions {
    entries: BTreeMap<String, PendingPermission>,
    sessions: BTreeMap<SessionId, std::collections::BTreeSet<String>>,
    batches: BTreeMap<String, std::collections::BTreeSet<String>>,
    identities: BTreeMap<String, String>,
}

impl PendingPermissions {
    pub(super) fn insert(
        &mut self,
        id: String,
        value: PendingPermission,
    ) -> Option<PendingPermission> {
        use sha2::{Digest as _, Sha256};
        struct HashWriter<'a>(&'a mut Sha256);
        impl std::io::Write for HashWriter<'_> {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.update(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        // A nonce prevents ABA even if a checkpoint is restored with identical fields.
        let nonce = SessionId::new();
        let mut hash = Sha256::new();
        hash.update(nonce.to_string().as_bytes());
        // Owned structs contain no fallible custom serializer; fail closed if this changes.
        serde_json::to_writer(HashWriter(&mut hash), &value.summary)
            .expect("permission summary serialization");
        let digest = hex::encode(hash.finalize());
        let previous = self.remove(&id);
        self.sessions
            .entry(value.summary.session_id)
            .or_default()
            .insert(id.clone());
        if let Some(route) = &value.summary.interaction_route
            && route.version == 1
        {
            self.sessions
                .entry(route.destination_session_id)
                .or_default()
                .insert(id.clone());
        }
        if let Some(batch) = &value.summary.batch {
            self.batches
                .entry(batch.batch_id.clone())
                .or_default()
                .insert(id.clone());
        }
        self.identities.insert(id.clone(), digest);
        self.entries.insert(id, value);
        previous
    }

    pub(super) fn remove(&mut self, id: &str) -> Option<PendingPermission> {
        let value = self.entries.remove(id)?;
        for session in std::iter::once(value.summary.session_id).chain(
            value
                .summary
                .interaction_route
                .as_ref()
                .filter(|route| route.version == 1)
                .map(|route| route.destination_session_id),
        ) {
            if let Some(ids) = self.sessions.get_mut(&session) {
                ids.remove(id);
                if ids.is_empty() {
                    self.sessions.remove(&session);
                }
            }
        }
        if let Some(batch) = &value.summary.batch
            && let Some(ids) = self.batches.get_mut(&batch.batch_id)
        {
            ids.remove(id);
            if ids.is_empty() {
                self.batches.remove(&batch.batch_id);
            }
        }
        self.identities.remove(id);
        Some(value)
    }

    #[cfg(test)]
    pub(super) fn keys(&self) -> impl Iterator<Item = &String> {
        self.entries.keys()
    }
    pub(super) fn get(&self, id: &str) -> Option<&PendingPermission> {
        self.entries.get(id)
    }
    pub(super) fn contains_key(&self, id: &str) -> bool {
        self.entries.contains_key(id)
    }
    pub(super) fn values(&self) -> impl Iterator<Item = &PendingPermission> {
        self.entries.values()
    }
    pub(super) fn iter(&self) -> impl Iterator<Item = (&String, &PendingPermission)> {
        self.entries.iter()
    }
    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }
    #[cfg(test)]
    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }
    #[cfg(test)]
    pub(super) fn extend(&mut self, values: impl IntoIterator<Item = (String, PendingPermission)>) {
        for (id, value) in values {
            self.insert(id, value);
        }
    }
}

#[cfg(test)]
mod scoped_permission_tests {
    use super::*;
    use bcode_session_models::{
        ConditionalPermissionDecision, ConditionalPermissionResult,
        MAX_SCOPED_PERMISSION_CHECKPOINTS, PermissionBatchCorrelation, PermissionSummary,
        ScopedPermissionLookup, ScopedPermissionQuery,
    };

    fn checkpoint(
        session_id: SessionId,
        id: &str,
        index: usize,
        count: usize,
    ) -> PendingPermission {
        PendingPermission {
            summary: PermissionSummary {
                permission_id: id.into(),
                interaction_route: None,
                session_id,
                tool_call_id: format!("call-{index}"),
                tool_name: "test.tool".into(),
                arguments_json: "{}".into(),
                batch: Some(PermissionBatchCorrelation {
                    batch_id: "test-batch".into(),
                    call_index: index,
                    call_count: count,
                }),
                agent_id: "build".into(),
                policy_source: None,
                policy_reason: None,
                can_remember_policy: false,
            },
            decision: Arc::new(Mutex::new(None)),
            notify: Arc::new(tokio::sync::Notify::new()),
            skill_decision_key: None,
        }
    }

    #[test]
    fn remembered_policy_survives_reopen_without_crossing_workspace_or_skill_identity() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("settings.db");
        let key = SkillToolDecisionKey {
            skill_ids: std::collections::BTreeSet::from([bcode_skill_models::SkillId(
                "skill".into(),
            )]),
            tool_name: "filesystem.write".into(),
            scope: bcode_skill_models::SkillToolDecisionScope::Workspace {
                workspace_id: "workspace".into(),
            },
        };
        for decision in [SkillToolDecision::Allow, SkillToolDecision::Deny] {
            persist_skill_tool_decision(
                &bcode_settings::SettingsStore::from_settings_db_path(&path),
                key.clone(),
                decision,
            )
            .unwrap();
            let reopened = bcode_settings::SettingsStore::from_settings_db_path(&path);
            let state = reopened.skill_tool_decisions().unwrap();
            assert_eq!(state.decision_for(&key).unwrap().decision, decision);
            let mut foreign = key.clone();
            foreign.scope = bcode_skill_models::SkillToolDecisionScope::Workspace {
                workspace_id: "other".into(),
            };
            assert!(state.decision_for(&foreign).is_none());
            foreign = key.clone();
            foreign
                .skill_ids
                .insert(bcode_skill_models::SkillId("other".into()));
            assert!(state.decision_for(&foreign).is_none());
            foreign = key.clone();
            foreign.tool_name = "filesystem.read".into();
            assert!(state.decision_for(&foreign).is_none());
        }
        // A directory cannot be opened as a settings database. Failure must be observable,
        // and must not overwrite the previously committed remembered decision.
        assert!(
            persist_skill_tool_decision(
                &bcode_settings::SettingsStore::from_settings_db_path(root.path()),
                key.clone(),
                SkillToolDecision::Allow
            )
            .is_err()
        );
        assert_eq!(
            bcode_settings::SettingsStore::from_settings_db_path(path)
                .skill_tool_decisions()
                .unwrap()
                .decision_for(&key)
                .unwrap()
                .decision,
            SkillToolDecision::Deny
        );
    }

    async fn register_checkpoint(state: &ServerState, permission: &PendingPermission) {
        let summary = &permission.summary;
        register_pending_permission(
            state,
            permission,
            bcode_session_models::SessionEventKind::PermissionRequested {
                permission_id: summary.permission_id.clone(),
                tool_call_id: summary.tool_call_id.clone(),
                producer_plugin_id: Some("test".into()),
                tool_name: summary.tool_name.clone(),
                arguments_json: summary.arguments_json.clone(),
                batch: summary.batch.clone(),
                policy_source: summary.policy_source.clone(),
                policy_reason: summary.policy_reason.clone(),
            },
        )
        .await
        .expect("durable checkpoint admission");
    }

    async fn fixture() -> (
        Arc<ServerState>,
        SessionId,
        PendingPermission,
        PendingPermission,
    ) {
        let state = Arc::new(crate::tests::test_server_state(
            bcode_session::SessionManager::default(),
        ));
        let session = state
            .sessions
            .create_session(None, std::env::temp_dir())
            .await
            .unwrap();
        state.pending_permission_batches.lock().unwrap().insert(
            "test-batch".into(),
            Arc::new(PendingPermissionBatch {
                session_id: session.id,
                decision: Mutex::new(None),
                scoped_closed: std::sync::atomic::AtomicBool::new(false),
            }),
        );
        let first = checkpoint(session.id, "first", 0, 2);
        let second = checkpoint(session.id, "second", 1, 2);
        register_checkpoint(&state, &first).await;
        register_checkpoint(&state, &second).await;
        (state, session.id, first, second)
    }

    async fn observed(state: &ServerState, session_id: SessionId) -> ConditionalPermissionDecision {
        let ScopedPermissionLookup::Ready {
            checkpoints,
            next_after: None,
        } = scoped_permissions(
            state,
            ScopedPermissionQuery {
                session_id,
                batch_id: Some("test-batch".into()),
                after_permission_id: None,
                limit: MAX_SCOPED_PERMISSION_CHECKPOINTS,
            },
        )
        .await
        else {
            panic!("complete batch observation required");
        };
        ConditionalPermissionDecision {
            session_id,
            batch_id: Some("test-batch".into()),
            expected: checkpoints
                .into_iter()
                .map(|value| value.identity)
                .collect(),
            approved: true,
            remember: false,
        }
    }

    #[tokio::test]
    async fn conditional_individual_matches_native_decision_and_never_retries_a_commit() {
        for approved in [false, true] {
            let (state, session, _, _) = fixture().await;
            let mut browser = checkpoint(session, "browser", 0, 1);
            browser.summary.batch = None;
            let mut native = browser.clone();
            native.summary.permission_id = "native".into();
            native.decision = Arc::new(Mutex::new(None));
            register_checkpoint(&state, &browser).await;
            register_checkpoint(&state, &native).await;
            let ScopedPermissionLookup::Ready { checkpoints, .. } = scoped_permissions(
                &state,
                ScopedPermissionQuery {
                    session_id: session,
                    batch_id: None,
                    after_permission_id: None,
                    limit: MAX_SCOPED_PERMISSION_CHECKPOINTS,
                },
            )
            .await
            else {
                panic!("canonical checkpoint observation");
            };
            let request = ConditionalPermissionDecision {
                session_id: session,
                batch_id: None,
                expected: checkpoints
                    .into_iter()
                    .filter(|value| value.identity.permission_id == "browser")
                    .map(|value| value.identity)
                    .collect(),
                approved,
                remember: false,
            };
            assert_eq!(
                resolve_permissions_conditional(&state, request.clone()).await,
                ConditionalPermissionResult::Resolved { resolved: 1 }
            );
            assert!(resolve_permission(&state, "native", approved, false).await);
            assert_eq!(*browser.decision.lock().await, Some(approved));
            assert_eq!(*native.decision.lock().await, Some(approved));
            let committed = state.sessions.session_history(session).await.unwrap();
            for id in ["browser", "native"] {
                assert_eq!(
                    committed
                        .iter()
                        .filter(|event| matches!(
                            &event.kind,
                            bcode_session_models::SessionEventKind::PermissionResolved {
                                permission_id, approved: decision,
                            } if permission_id == id && *decision == approved
                        ))
                        .count(),
                    1
                );
            }
            assert_eq!(
                resolve_permissions_conditional(&state, request).await,
                ConditionalPermissionResult::Stale
            );
            assert_eq!(
                state.sessions.session_history(session).await.unwrap(),
                committed
            );
        }
    }

    #[tokio::test]
    async fn conditional_batch_approval_commits_before_waiter_release() {
        let (state, session, first, second) = fixture().await;
        let exact = observed(&state, session).await;
        assert_eq!(
            resolve_permissions_conditional(&state, exact).await,
            ConditionalPermissionResult::Resolved { resolved: 2 }
        );
        assert_eq!(state.pending_permissions.lock().await.len(), 0);
        assert_eq!(*first.decision.lock().await, Some(true));
        assert_eq!(*second.decision.lock().await, Some(true));
        assert_eq!(
            permission_batch_decision(&state, "test-batch").await,
            Some(true)
        );
        let history = state.sessions.session_history(session).await.unwrap();
        assert_eq!(
            history
                .iter()
                .filter(|event| matches!(
                    event.kind,
                    bcode_session_models::SessionEventKind::PermissionResolved {
                        approved: true,
                        ..
                    }
                ))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn conditional_individual_destination_and_aba_are_fenced() {
        let (state, session, _, _) = fixture().await;
        let destination = SessionId::new();
        let mut value = checkpoint(session, "individual", 0, 1);
        value.summary.batch = None;
        value.summary.interaction_route = Some(bcode_session_models::ExecutionInteractionRoute {
            version: 1,
            destination_session_id: destination,
        });
        state
            .pending_permissions
            .lock()
            .await
            .insert("individual".into(), value.clone());
        let ScopedPermissionLookup::Ready {
            checkpoints,
            next_after: None,
        } = scoped_permissions(
            &state,
            ScopedPermissionQuery {
                session_id: destination,
                batch_id: None,
                after_permission_id: None,
                limit: 1,
            },
        )
        .await
        else {
            panic!("destination indexed lookup");
        };
        assert_eq!(checkpoints.len(), 1);
        let mut exact = ConditionalPermissionDecision {
            session_id: destination,
            batch_id: None,
            expected: checkpoints
                .into_iter()
                .map(|value| value.identity)
                .collect(),
            approved: false,
            remember: false,
        };
        state.pending_permissions.lock().await.remove("individual");
        state
            .pending_permissions
            .lock()
            .await
            .insert("individual".into(), value.clone());
        assert_eq!(
            resolve_permissions_conditional(&state, exact.clone()).await,
            ConditionalPermissionResult::Stale
        );
        assert_eq!(*value.decision.lock().await, None);
        let ScopedPermissionLookup::Ready { checkpoints, .. } = scoped_permissions(
            &state,
            ScopedPermissionQuery {
                session_id: destination,
                batch_id: None,
                after_permission_id: None,
                limit: 1,
            },
        )
        .await
        else {
            panic!("fresh destination lookup");
        };
        exact.expected = checkpoints
            .into_iter()
            .map(|value| value.identity)
            .collect();
        assert_eq!(
            resolve_permissions_conditional(&state, exact).await,
            ConditionalPermissionResult::Resolved { resolved: 1 }
        );
        assert_eq!(*value.decision.lock().await, Some(false));
        let pending = state.pending_permissions.lock().await;
        assert!(!pending.sessions.contains_key(&destination));
        assert!(!pending.sessions[&session].contains("individual"));
        drop(pending);
    }

    #[tokio::test]
    async fn scoped_lookup_rejects_oversized_batch_without_partial_output() {
        let (state, session, _, _) = fixture().await;
        let mut pending = state.pending_permissions.lock().await;
        pending.remove("first");
        pending.remove("second");
        for index in 0..=MAX_SCOPED_PERMISSION_CHECKPOINTS {
            let id = format!("member-{index}");
            pending.insert(
                id.clone(),
                checkpoint(session, &id, index, MAX_SCOPED_PERMISSION_CHECKPOINTS + 1),
            );
        }
        drop(pending);
        assert_eq!(
            scoped_permissions(
                &state,
                ScopedPermissionQuery {
                    session_id: session,
                    batch_id: Some("test-batch".into()),
                    after_permission_id: None,
                    limit: MAX_SCOPED_PERMISSION_CHECKPOINTS,
                }
            )
            .await,
            ScopedPermissionLookup::LimitExceeded
        );
        assert_eq!(permission_batch_decision(&state, "test-batch").await, None);
    }

    #[tokio::test]
    async fn legacy_batch_dropped_caller_still_commits_and_releases_waiters() {
        let (state, session, first, second) = fixture().await;
        let pending = state.pending_permissions.lock().await;
        let mut operation = Box::pin(resolve_permission_batch(&state, "test-batch", true));
        tokio::select! { biased;
            result = &mut operation => panic!("passed pending fence: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
        drop(operation);
        drop(pending);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if *first.decision.lock().await == Some(true)
                    && *second.decision.lock().await == Some(true)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("legacy owned completion");
        assert_eq!(
            permission_batch_decision(&state, "test-batch").await,
            Some(true)
        );
        let history = state.sessions.session_history(session).await.unwrap();
        assert_eq!(
            history
                .iter()
                .filter(|event| matches!(
                    event.kind,
                    bcode_session_models::SessionEventKind::PermissionResolved {
                        approved: true,
                        ..
                    }
                ))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn conditional_batch_committed_membership_never_approves_late_registration() {
        let (state, session, _, _) = fixture().await;
        let exact = observed(&state, session).await;
        assert_eq!(
            resolve_permissions_conditional(&state, exact).await,
            ConditionalPermissionResult::Resolved { resolved: 2 }
        );
        let late = checkpoint(session, "late", 2, 3);
        let event = bcode_session_models::SessionEventKind::PermissionRequested {
            permission_id: "late".into(),
            tool_call_id: "late-call".into(),
            producer_plugin_id: None,
            tool_name: "test.tool".into(),
            arguments_json: "{}".into(),
            batch: late.summary.batch.clone(),
            policy_source: None,
            policy_reason: None,
        };
        assert_eq!(
            register_pending_permission(&state, &late, event).await,
            Err(false)
        );
        assert!(!state.pending_permissions.lock().await.contains_key("late"));
    }

    #[tokio::test]
    async fn conditional_batch_stale_replacement_causes_zero_mutation() {
        let (state, session, first, _) = fixture().await;
        let decision = observed(&state, session).await;
        let replacement = checkpoint(session, "second", 1, 2);
        state
            .pending_permissions
            .lock()
            .await
            .insert("second".into(), replacement.clone());
        assert_eq!(
            resolve_permissions_conditional(&state, decision).await,
            ConditionalPermissionResult::Stale
        );
        assert_eq!(state.pending_permissions.lock().await.len(), 2);
        assert_eq!(*first.decision.lock().await, None);
        assert_eq!(*replacement.decision.lock().await, None);
        assert_eq!(permission_batch_decision(&state, "test-batch").await, None);
    }

    #[tokio::test]
    async fn conditional_batch_rejects_partial_duplicate_and_changed_payload() {
        let (state, session, first, second) = fixture().await;
        let exact = observed(&state, session).await;
        let mut partial = exact.clone();
        partial.expected.pop();
        assert_eq!(
            resolve_permissions_conditional(&state, partial).await,
            ConditionalPermissionResult::Stale
        );
        let mut duplicate = exact.clone();
        duplicate.expected[1] = duplicate.expected[0].clone();
        assert_eq!(
            resolve_permissions_conditional(&state, duplicate).await,
            ConditionalPermissionResult::Invalid
        );
        let mut changed = second.clone();
        changed.summary.arguments_json = "{\"changed\":true}".into();
        state
            .pending_permissions
            .lock()
            .await
            .insert("second".into(), changed);
        assert_eq!(
            resolve_permissions_conditional(&state, exact).await,
            ConditionalPermissionResult::Stale
        );
        assert_eq!(*first.decision.lock().await, None);
        assert_eq!(*second.decision.lock().await, None);
        assert_eq!(permission_batch_decision(&state, "test-batch").await, None);
    }

    #[tokio::test]
    async fn conditional_batch_persistence_failure_preserves_pending_members_and_decisions() {
        let (state, session, first, second) = fixture().await;
        let exact = observed(&state, session).await;
        state.sessions.delete_session(session).await.unwrap();
        assert_eq!(
            resolve_permissions_conditional(&state, exact).await,
            ConditionalPermissionResult::CompletionFailed
        );
        assert_eq!(state.pending_permissions.lock().await.len(), 2);
        assert_eq!(*first.decision.lock().await, None);
        assert_eq!(*second.decision.lock().await, None);
        assert_eq!(permission_batch_decision(&state, "test-batch").await, None);
    }

    #[tokio::test]
    async fn conditional_batch_caller_cancellation_does_not_abandon_admission() {
        let (state, session, first, second) = fixture().await;
        let exact = observed(&state, session).await;
        let pending = state.pending_permissions.lock().await;
        let mut operation = Box::pin(resolve_permissions_conditional(&state, exact));
        tokio::select! { biased;
            result = &mut operation => panic!("passed membership fence: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
        drop(operation);
        drop(pending);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if *first.decision.lock().await == Some(true)
                    && *second.decision.lock().await == Some(true)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owned completion survives dropped caller");
        assert_eq!(state.pending_permissions.lock().await.len(), 0);
    }

    #[tokio::test]
    async fn conditional_batch_denial_survives_dropped_caller() {
        let (state, session, first, second) = fixture().await;
        let mut exact = observed(&state, session).await;
        exact.approved = false;
        let pending = state.pending_permissions.lock().await;
        let mut operation = Box::pin(resolve_permissions_conditional(&state, exact));
        tokio::select! { biased;
            result = &mut operation => panic!("passed membership fence: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
        drop(operation);
        drop(pending);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if *first.decision.lock().await == Some(false)
                    && *second.decision.lock().await == Some(false)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owned denial survives dropped caller");
        assert_eq!(
            permission_batch_decision(&state, "test-batch").await,
            Some(false)
        );
        let history = state.sessions.session_history(session).await.unwrap();
        assert_eq!(
            history
                .iter()
                .filter(|event| matches!(
                    event.kind,
                    bcode_session_models::SessionEventKind::PermissionResolved {
                        approved: false,
                        ..
                    }
                ))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn conditional_batch_cancel_winner_fences_all_approval() {
        let (state, session, first, second) = fixture().await;
        let exact = observed(&state, session).await;
        cancel_pending_permissions_for_session(&state, session).await;
        assert_eq!(
            resolve_permissions_conditional(&state, exact).await,
            ConditionalPermissionResult::Stale
        );
        assert_eq!(*first.decision.lock().await, Some(false));
        assert_eq!(*second.decision.lock().await, Some(false));
        assert_eq!(
            permission_batch_decision(&state, "test-batch").await,
            Some(false)
        );
        let history = state.sessions.session_history(session).await.unwrap();
        assert!(!history.iter().any(|event| matches!(
            event.kind,
            bcode_session_models::SessionEventKind::PermissionResolved { approved: true, .. }
        )));
    }

    #[tokio::test]
    async fn conditional_batch_approval_racing_session_cancel_has_one_decision() {
        let (state, session, first, second) = fixture().await;
        let exact = observed(&state, session).await;
        let (result, ()) = tokio::join!(
            resolve_permissions_conditional(&state, exact),
            cancel_pending_permissions_for_session(&state, session)
        );
        let approved = matches!(
            result,
            ConditionalPermissionResult::Resolved { resolved: 2 }
        );
        assert!(approved || result == ConditionalPermissionResult::Stale);
        assert_eq!(*first.decision.lock().await, Some(approved));
        assert_eq!(*second.decision.lock().await, Some(approved));
        let history = state.sessions.session_history(session).await.unwrap();
        let decisions = history
            .iter()
            .filter_map(|event| match event.kind {
                bcode_session_models::SessionEventKind::PermissionResolved { approved, .. } => {
                    Some(approved)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(decisions, vec![approved, approved]);
    }

    #[tokio::test]
    async fn conditional_individual_dropped_caller_still_settles_decision() {
        let (state, session, _, _) = fixture().await;
        let mut permission = checkpoint(session, "individual", 0, 1);
        permission.summary.batch = None;
        state
            .pending_permissions
            .lock()
            .await
            .insert("individual".into(), permission.clone());
        let ScopedPermissionLookup::Ready { checkpoints, .. } = scoped_permissions(
            &state,
            ScopedPermissionQuery {
                session_id: session,
                batch_id: None,
                after_permission_id: None,
                limit: MAX_SCOPED_PERMISSION_CHECKPOINTS,
            },
        )
        .await
        else {
            panic!("lookup")
        };
        let expected = checkpoints
            .into_iter()
            .filter(|value| value.identity.permission_id == "individual")
            .map(|value| value.identity)
            .collect();
        let decision = ConditionalPermissionDecision {
            session_id: session,
            batch_id: None,
            expected,
            approved: false,
            remember: false,
        };
        let pending = state.pending_permissions.lock().await;
        let mut operation = Box::pin(resolve_permissions_conditional(&state, decision));
        tokio::select! { biased;
            result = &mut operation => panic!("passed membership fence: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
        drop(operation);
        drop(pending);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while permission.decision.lock().await.is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owned individual completion");
        assert_eq!(*permission.decision.lock().await, Some(false));
        assert!(
            !state
                .pending_permissions
                .lock()
                .await
                .contains_key("individual")
        );
    }

    #[tokio::test]
    async fn conditional_batch_two_concurrent_resolvers_have_one_winner() {
        let (state, session, first, second) = fixture().await;
        let exact = observed(&state, session).await;
        let mut deny = exact.clone();
        deny.approved = false;
        let (left, right) = tokio::join!(
            resolve_permissions_conditional(&state, exact),
            resolve_permissions_conditional(&state, deny)
        );
        assert!(matches!(
            (&left, &right),
            (
                ConditionalPermissionResult::Resolved { resolved: 2 },
                ConditionalPermissionResult::Stale
            ) | (
                ConditionalPermissionResult::Stale,
                ConditionalPermissionResult::Resolved { resolved: 2 }
            )
        ));
        assert_eq!(*first.decision.lock().await, *second.decision.lock().await);
        assert!(first.decision.lock().await.is_some());
        assert!(state.pending_permissions.lock().await.is_empty());
    }

    #[tokio::test]
    async fn conditional_batch_removal_racing_final_validation_is_stale() {
        let (state, session, first, second) = fixture().await;
        let exact = observed(&state, session).await;
        // Hold authoritative pending lock while the resolver acquires the batch
        // latch. A cancellation/removal wins before final compare and claim.
        let batch = state.pending_permission_batches.lock().unwrap()["test-batch"].clone();
        let mut pending = state.pending_permissions.lock().await;
        let resolve = resolve_permissions_conditional(&state, exact);
        tokio::pin!(resolve);
        tokio::select! { biased;
            result = &mut resolve => panic!("resolver passed held pending lock: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while batch.decision.try_lock().is_ok() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "resolver did not reach membership fence"
            );
            tokio::task::yield_now().await;
        }
        pending.remove("second");
        drop(pending);
        assert_eq!(resolve.await, ConditionalPermissionResult::Stale);
        assert_eq!(*first.decision.lock().await, None);
        assert_eq!(*second.decision.lock().await, None);
        assert_eq!(permission_batch_decision(&state, "test-batch").await, None);
        assert_eq!(state.pending_permissions.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn scoped_lookup_is_paged_destination_indexed_and_never_partial_batch() {
        let (state, session, first, second) = fixture().await;
        let foreign = SessionId::new();
        let mut pending = state.pending_permissions.lock().await;
        for index in 0..1000 {
            let id = format!("foreign-{index}");
            let mut value = checkpoint(foreign, &id, 0, 1);
            value.summary.batch = None;
            pending.insert(id, value);
        }
        assert_eq!(pending.sessions[&session].len(), 2);
        drop(pending);
        let query = ScopedPermissionQuery {
            session_id: session,
            batch_id: None,
            after_permission_id: None,
            limit: 1,
        };
        let ScopedPermissionLookup::Ready {
            checkpoints,
            next_after: Some(after),
        } = scoped_permissions(&state, query.clone()).await
        else {
            panic!("bounded page");
        };
        assert_eq!(checkpoints.len(), 1);
        assert_eq!(checkpoints[0].permission.permission_id, "first");
        let ScopedPermissionLookup::Ready {
            checkpoints,
            next_after: None,
        } = scoped_permissions(
            &state,
            ScopedPermissionQuery {
                after_permission_id: Some(after),
                ..query
            },
        )
        .await
        else {
            panic!("second page");
        };
        assert_eq!(checkpoints[0].permission.permission_id, "second");
        let mut decision = observed(&state, session).await;
        decision.session_id = foreign;
        assert_eq!(
            resolve_permissions_conditional(&state, decision).await,
            ConditionalPermissionResult::Stale
        );
        assert_eq!(*first.decision.lock().await, None);
        assert_eq!(*second.decision.lock().await, None);
        state.pending_permissions.lock().await.remove("second");
        assert_eq!(
            scoped_permissions(
                &state,
                ScopedPermissionQuery {
                    session_id: session,
                    batch_id: Some("test-batch".into()),
                    after_permission_id: None,
                    limit: MAX_SCOPED_PERMISSION_CHECKPOINTS
                }
            )
            .await,
            ScopedPermissionLookup::Unavailable
        );
    }
}

/// Application-level failure while resolving a pending tool exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveToolExchangeError {
    /// The requesting client did not advertise a compatible exchange adapter.
    IncompatibleConsumer,
    /// The serialized resolution is malformed or uses an unknown variant.
    InvalidResolution,
}

impl ResolveToolExchangeError {
    /// Stable public operation error code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::IncompatibleConsumer => "incompatible_exchange_consumer",
            Self::InvalidResolution => "invalid_exchange_resolution",
        }
    }

    /// Secret-safe public operation error message.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::IncompatibleConsumer => "client did not advertise a compatible exchange adapter",
            Self::InvalidResolution => "tool exchange resolution is malformed or unsupported",
        }
    }
}

async fn append_permission_requested_event(
    state: &ServerState,
    session_id: SessionId,
    request: bcode_session_models::SessionEventKind,
) -> Option<bcode_session_models::SessionEvent> {
    match state
        .sessions
        .append_permission_requested(session_id, request)
        .await
    {
        Ok(event) => Some(event),
        Err(error) => {
            tracing::warn!("failed to append permission request: {error}");
            None
        }
    }
}

async fn append_permission_resolved_event(
    state: &ServerState,
    session_id: SessionId,
    permission_id: String,
    approved: bool,
) -> bool {
    match state
        .sessions
        .append_permission_resolved(session_id, permission_id, approved)
        .await
    {
        Ok(event) => {
            publish_session_event(state, &event).await;
            true
        }
        Err(error) => {
            tracing::warn!("failed to append permission result: {error}");
            false
        }
    }
}

impl PendingPermissionBatch {
    /// Create one unresolved batch state.
    #[must_use]
    pub const fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            decision: Mutex::const_new(None),
            scoped_closed: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

/// Return a remembered skill-tool permission decision when user state is available.
pub fn remembered_skill_tool_decision(key: &SkillToolDecisionKey) -> Option<SkillToolDecision> {
    bcode_settings::SettingsStore::default()
        .skill_tool_decisions()
        .ok()
        .and_then(|state| state.decision_for(key).map(|entry| entry.decision))
}

fn remember_skill_tool_decision(key: SkillToolDecisionKey, decision: SkillToolDecision) {
    if let Err(error) =
        persist_skill_tool_decision(&bcode_settings::SettingsStore::default(), key, decision)
    {
        tracing::warn!(%error, "permission resolved but remember-policy persistence failed");
    }
}

fn persist_skill_tool_decision(
    store: &bcode_settings::SettingsStore,
    key: SkillToolDecisionKey,
    decision: SkillToolDecision,
) -> bcode_settings::SettingsResult<()> {
    let mut state = store.skill_tool_decisions()?;
    state.upsert(bcode_skill_models::SkillToolDecisionEntry {
        key,
        decision,
        remembered_at_ms: current_time_ms(),
        reason: Some("remembered from permission dialog".to_owned()),
    });
    store.save_skill_tool_decisions(&state, current_time_ms())
}

/// Allocate the next process-local pending permission identity.
pub async fn next_permission_id(state: &ServerState) -> String {
    let mut next = state.next_permission_id.lock().await;
    let permission_id = format!("perm-{}-{}", state.daemon_status.instance_id, *next);
    *next += 1;
    permission_id
}

/// Allocate the next process-local permission batch identity.
pub async fn next_permission_batch_id(state: &ServerState) -> String {
    let mut next = state.next_permission_batch_id.lock().await;
    let batch_id = format!(
        "permission-batch-{}-{}",
        state.daemon_status.instance_id, *next
    );
    *next += 1;
    batch_id
}

/// Scoped registration for one permission batch.
pub struct PendingPermissionBatchRegistration {
    batches: Arc<StdMutex<BTreeMap<String, Arc<PendingPermissionBatch>>>>,
    batch_id: String,
}

impl PendingPermissionBatchRegistration {
    /// Allocate and register one unresolved batch until this guard is dropped.
    pub async fn allocate(state: &ServerState, session_id: SessionId) -> (String, Self) {
        let batch_id = next_permission_batch_id(state).await;
        let registration = Self::register(state, batch_id.clone(), session_id);
        (batch_id, registration)
    }

    /// Register one unresolved batch until this guard is dropped.
    #[must_use]
    pub fn register(state: &ServerState, batch_id: String, session_id: SessionId) -> Self {
        let batch = Arc::new(PendingPermissionBatch::new(session_id));
        state
            .pending_permission_batches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(batch_id.clone(), batch);
        Self {
            batches: Arc::clone(&state.pending_permission_batches),
            batch_id,
        }
    }
}

impl Drop for PendingPermissionBatchRegistration {
    fn drop(&mut self) {
        self.batches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.batch_id);
    }
}

/// Normalized permission-rule update failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionRuleError {
    /// The configuration owner rejected the category.
    UnknownCategory,
    /// The configuration owner rejected the action.
    UnknownAction,
    /// Permission state could not be read or updated.
    StateUnavailable,
}

impl PermissionRuleError {
    /// Secret-safe diagnostic suitable for application clients.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::UnknownCategory => {
                "unknown permission category; expected command, read, write, edit, or web"
            }
            Self::UnknownAction => "unknown permission action; expected allow, ask, or deny",
            Self::StateUnavailable => "permission state could not be read or updated",
        }
    }
}

impl From<bcode_config::ConfigError> for PermissionRuleError {
    fn from(error: bcode_config::ConfigError) -> Self {
        match error {
            bcode_config::ConfigError::UnknownPermissionCategory(_) => Self::UnknownCategory,
            bcode_config::ConfigError::UnknownPermissionAction(_) => Self::UnknownAction,
            bcode_config::ConfigError::Io { .. }
            | bcode_config::ConfigError::Composition { .. }
            | bcode_config::ConfigError::RemovedShorthandToolId { .. }
            | bcode_config::ConfigError::RemovedPermissionCategory { .. } => Self::StateUnavailable,
        }
    }
}

/// Persist one normalized agent permission rule through the configuration owner.
///
/// # Errors
///
/// Returns a normalized failure when the rule fields or permission state update are invalid.
pub fn add_permission_rule(
    agent_id: &str,
    category: &str,
    pattern: String,
    action: &str,
) -> Result<PathBuf, PermissionRuleError> {
    bcode_config::upsert_agent_permission_rule(agent_id, category, pattern, action)
        .map_err(PermissionRuleError::from)
}

/// Register one pending permission while preserving its optional batch-decision latch.
///
/// Returns the already-latched decision when the batch has already resolved, or denial when the
/// referenced batch is no longer active.
pub async fn register_pending_permission(
    state: &ServerState,
    pending: &PendingPermission,
    event: bcode_session_models::SessionEventKind,
) -> Result<(), bool> {
    let batch_state = pending.summary.batch.as_ref().and_then(|batch| {
        state
            .pending_permission_batches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&batch.batch_id)
            .cloned()
    });
    if pending.summary.batch.is_some() && batch_state.is_none() {
        return Err(false);
    }
    let batch_decision = if let Some(batch_state) = batch_state.as_ref() {
        let decision = batch_state.decision.lock().await;
        if let Some(decision) = *decision {
            return Err(
                if batch_state
                    .scoped_closed
                    .load(std::sync::atomic::Ordering::Acquire)
                {
                    false
                } else {
                    decision
                },
            );
        }
        Some(decision)
    } else {
        None
    };
    let mut routed = pending.clone();
    routed.summary.interaction_route = state
        .session_current_turn(pending.summary.session_id)
        .await
        .and_then(|turn| turn.interaction_route);
    // Canonical append notifies subscribers. Fence immediate list/resolve calls until
    // persistence and registration complete, without exposing an unpersisted request.
    let mut permissions = state.pending_permissions.lock().await;
    let Some(event) =
        append_permission_requested_event(state, pending.summary.session_id, event).await
    else {
        return Err(false);
    };
    permissions.insert(pending.summary.permission_id.clone(), routed.clone());
    drop(permissions);
    drop(batch_decision);
    crate::xray_tools::emit_tool(
        state,
        pending.summary.session_id,
        &pending.summary.tool_call_id,
        "permission_pending",
        serde_json::json!({
            "permission_id": pending.summary.permission_id,
        }),
    );
    publish_session_event(state, &event).await;
    notify_interaction_destination(state, &routed.summary).await;
    Ok(())
}

/// Notify clients to refresh the destination through the normal bounded snapshot boundary.
/// This is only an invalidation; canonical permission state remains in the source session.
async fn notify_interaction_destination(
    state: &ServerState,
    permission: &bcode_session_models::PermissionSummary,
) {
    notify_route_destination(state, permission.interaction_route.as_ref()).await;
}

async fn notify_route_destination(
    state: &ServerState,
    route: Option<&bcode_session_models::ExecutionInteractionRoute>,
) {
    use futures::StreamExt as _;

    let Some(route) = route else {
        return;
    };
    let _ = state
        .interaction_invalidations
        .send(route.destination_session_id);
    let sinks = state.workflow_event_sinks().await;
    futures::stream::iter(sinks)
        .for_each_concurrent(32, |sink| async move {
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                sink.send(bcode_ipc::Event::SessionViewResyncRequired {
                    session_id: route.destination_session_id,
                }),
            )
            .await;
        })
        .await;
}

/// Read one current permission-batch decision for focused verification.
#[cfg(test)]
pub async fn permission_batch_decision(state: &ServerState, batch_id: &str) -> Option<bool> {
    let batch = state
        .pending_permission_batches
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(batch_id)
        .cloned()?;
    *batch.decision.lock().await
}

/// Restore visibility of a verified canonical wait without appending another request.
/// Recovery's caller holds execution authority; this operation grants no dispatch permission.
pub async fn restore_permission(
    state: &ServerState,
    checkpoint: bcode_session::InvocationRecoveryCheckpoint,
) -> Result<(), super::ServerError> {
    let (bcode_session_models::SessionEventKind::UserMessage { admission, .. }
    | bcode_session_models::SessionEventKind::SubmissionActivated { admission, .. }) =
        &checkpoint.turn().kind
    else {
        return Ok(());
    };
    super::validate_interaction_route(state, checkpoint.session_id(), &admission.execution).await?;
    let bcode_session_models::SessionEventKind::PermissionRequested {
        permission_id,
        tool_call_id,
        tool_name,
        arguments_json,
        policy_source,
        policy_reason,
        ..
    } = &checkpoint.permission().kind
    else {
        return Ok(());
    };
    let pending = PendingPermission {
        summary: bcode_session_models::PermissionSummary {
            permission_id: permission_id.clone(),
            session_id: checkpoint.session_id(),
            tool_call_id: tool_call_id.clone(),
            tool_name: tool_name.clone(),
            arguments_json: arguments_json.clone(),
            batch: None,
            agent_id: admission
                .execution
                .agent_profile
                .clone()
                .unwrap_or_else(|| "build".into()),
            interaction_route: admission.execution.interaction_route.clone(),
            policy_source: policy_source.clone(),
            policy_reason: policy_reason.clone(),
            can_remember_policy: false,
        },
        decision: Arc::new(Mutex::new(None)),
        notify: Arc::new(tokio::sync::Notify::new()),
        skill_decision_key: None,
    };
    let mut recovered = state.recovered_permissions.lock().await;
    let mut permissions = state.pending_permissions.lock().await;
    if !permissions.contains_key(permission_id) {
        permissions.insert(permission_id.clone(), pending.clone());
        recovered.insert(permission_id.clone(), checkpoint);
    }
    drop(permissions);
    drop(recovered);
    notify_interaction_destination(state, &pending.summary).await;
    Ok(())
}

/// Return current pending permission summaries without transport framing.
pub async fn list_permissions(state: &ServerState) -> Vec<bcode_session_models::PermissionSummary> {
    let mut permissions = state
        .pending_permissions
        .lock()
        .await
        .values()
        .map(|permission| permission.summary.clone())
        .collect::<Vec<_>>();
    permissions.sort_by(|left, right| left.permission_id.cmp(&right.permission_id));
    permissions
}

/// Inspect a bounded, indexed session page or one complete batch.
/// Missing or incomplete batch membership never yields a partial decision set.
pub async fn scoped_permissions(
    state: &ServerState,
    query: bcode_session_models::ScopedPermissionQuery,
) -> bcode_session_models::ScopedPermissionLookup {
    use bcode_session_models::{
        MAX_SCOPED_PERMISSION_CHECKPOINTS as MAX, ScopedPermissionLookup as Lookup,
    };
    if query.limit == 0
        || query.limit > MAX
        || query
            .after_permission_id
            .as_deref()
            .is_some_and(|id| !valid_permission_identity(id))
        || query
            .batch_id
            .as_deref()
            .is_some_and(|id| !valid_permission_identity(id))
        || (query.batch_id.is_some() && (query.after_permission_id.is_some() || query.limit != MAX))
    {
        return Lookup::Invalid;
    }
    let batch = query.batch_id.as_ref().and_then(|id| {
        state
            .pending_permission_batches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned()
    });
    if query.batch_id.is_some() && batch.is_none() {
        return Lookup::Unavailable;
    }
    let decision = if let Some(batch) = batch.as_ref() {
        Some(batch.decision.lock().await)
    } else {
        None
    };
    if decision.as_ref().is_some_and(|decision| decision.is_some()) {
        return Lookup::Unavailable;
    }
    let pending = state.pending_permissions.lock().await;
    scoped_permissions_locked(&pending, &query, batch.as_deref())
}

fn scoped_permissions_locked(
    pending: &PendingPermissions,
    query: &bcode_session_models::ScopedPermissionQuery,
    batch: Option<&PendingPermissionBatch>,
) -> bcode_session_models::ScopedPermissionLookup {
    use bcode_session_models::{
        MAX_SCOPED_PERMISSION_CHECKPOINTS as MAX, ScopedPermissionLookup as Lookup,
    };
    let ids = query.batch_id.as_ref().map_or_else(
        || pending.sessions.get(&query.session_id),
        |id| pending.batches.get(id),
    );
    let Some(ids) = ids else {
        return if query.batch_id.is_some() {
            Lookup::Unavailable
        } else {
            Lookup::Ready {
                checkpoints: vec![],
                next_after: None,
            }
        };
    };
    if query.batch_id.is_some() && ids.len() > MAX {
        return Lookup::LimitExceeded;
    }
    if let Some(batch_id) = &query.batch_id
        && !complete_scoped_batch(
            pending,
            ids,
            query.session_id,
            batch_id,
            batch.as_ref().expect("validated batch").session_id,
        )
    {
        return Lookup::Unavailable;
    }
    let mut checkpoints = Vec::new();
    let mut bytes = 0usize;
    let lower = query
        .after_permission_id
        .as_ref()
        .map_or(std::ops::Bound::Unbounded, std::ops::Bound::Excluded);
    for id in ids
        .range::<String, _>((lower, std::ops::Bound::Unbounded))
        .take(query.limit + 1)
    {
        if checkpoints.len() == query.limit {
            return Lookup::Ready {
                next_after: checkpoints.last().map(
                    |value: &bcode_session_models::ScopedPermissionCheckpoint| {
                        value.identity.permission_id.clone()
                    },
                ),
                checkpoints,
            };
        }
        let Some(permission) = pending.entries.get(id) else {
            return Lookup::Unavailable;
        };
        if !permission.summary.is_addressed_to(query.session_id) {
            return Lookup::Unavailable;
        }
        let Some(size) = permission_summary_bytes(&permission.summary) else {
            return Lookup::LimitExceeded;
        };
        if bytes.saturating_add(size) > bcode_session_models::MAX_SCOPED_PERMISSION_BYTES {
            if query.batch_id.is_some() || checkpoints.is_empty() {
                return Lookup::LimitExceeded;
            }
            return Lookup::Ready {
                next_after: checkpoints.last().map(
                    |value: &bcode_session_models::ScopedPermissionCheckpoint| {
                        value.identity.permission_id.clone()
                    },
                ),
                checkpoints,
            };
        }
        bytes += size;
        checkpoints.push(bcode_session_models::ScopedPermissionCheckpoint {
            identity: bcode_session_models::PermissionCheckpointIdentity {
                permission_id: id.clone(),
                checkpoint_digest: pending.identities[id].clone(),
            },
            permission: permission.summary.clone(),
        });
    }
    Lookup::Ready {
        checkpoints,
        next_after: None,
    }
}

fn valid_permission_identity(id: &str) -> bool {
    !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control)
}

fn permission_summary_bytes(summary: &bcode_session_models::PermissionSummary) -> Option<usize> {
    struct Budget(usize);
    impl std::io::Write for Budget {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_add(bytes.len())
                .filter(|size| *size <= bcode_session_models::MAX_SCOPED_PERMISSION_BYTES)
                .ok_or_else(|| std::io::Error::other("permission summary budget"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut budget = Budget(0);
    serde_json::to_writer(&mut budget, summary).ok()?;
    Some(budget.0)
}

fn complete_scoped_batch(
    pending: &PendingPermissions,
    ids: &std::collections::BTreeSet<String>,
    destination: SessionId,
    batch_id: &str,
    source: SessionId,
) -> bool {
    let count = ids.len();
    if count == 0 || count > bcode_session_models::MAX_SCOPED_PERMISSION_CHECKPOINTS {
        return false;
    }
    let mut indices = std::collections::BTreeSet::new();
    ids.iter().all(|id| {
        pending.entries.get(id).is_some_and(|permission| {
            let summary = &permission.summary;
            summary.session_id == source
                && summary.is_addressed_to(destination)
                && summary.batch.as_ref().is_some_and(|batch| {
                    batch.batch_id == batch_id
                        && batch.call_count == count
                        && batch.call_index < count
                        && indices.insert(batch.call_index)
                })
        })
    })
}

/// Atomically compare the exact indexed checkpoint set and claim it under the
/// same pending-state lock used by registration/cancellation. Batch registration
/// shares the decision latch, so a successful claim cannot approve later members.
/// Stale validation returns before removing anything or setting the batch latch.
/// Normal canonical completion occurs only after this claim; failures fail closed.
pub async fn resolve_permissions_conditional(
    state: &Arc<ServerState>,
    request: bcode_session_models::ConditionalPermissionDecision,
) -> bcode_session_models::ConditionalPermissionResult {
    let state = Arc::clone(state);
    // Transport cancellation must not abandon admitted persistence or waiter release.
    tokio::spawn(async move { resolve_permissions_conditional_owned(&state, request).await })
        .await
        .unwrap_or(bcode_session_models::ConditionalPermissionResult::CompletionFailed)
}

async fn resolve_permissions_conditional_owned(
    state: &ServerState,
    request: bcode_session_models::ConditionalPermissionDecision,
) -> bcode_session_models::ConditionalPermissionResult {
    use bcode_session_models::{
        ConditionalPermissionResult as Result, MAX_SCOPED_PERMISSION_CHECKPOINTS as MAX,
    };
    if request.expected.is_empty()
        || request.expected.len() > MAX
        || (request.batch_id.is_none() && request.expected.len() != 1)
        || (request.batch_id.is_some() && request.remember)
        || request
            .batch_id
            .as_deref()
            .is_some_and(|id| !valid_permission_identity(id))
    {
        return Result::Invalid;
    }
    let expected: BTreeMap<_, _> = request
        .expected
        .iter()
        .map(|identity| (&identity.permission_id, &identity.checkpoint_digest))
        .collect();
    if expected.len() != request.expected.len()
        || expected.iter().any(|(id, digest)| {
            !valid_permission_identity(id)
                || digest.len() != 64
                || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    {
        return Result::Invalid;
    }
    let batch = request.batch_id.as_ref().and_then(|id| {
        state
            .pending_permission_batches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned()
    });
    if request.batch_id.is_some() && batch.is_none() {
        return Result::Stale;
    }
    let decision = if let Some(batch) = batch.as_ref() {
        Some(batch.decision.lock().await)
    } else {
        None
    };
    if decision.as_ref().is_some_and(|decision| decision.is_some()) {
        return Result::Stale;
    }
    let mut pending = state.pending_permissions.lock().await;
    if let Some(batch_id) = &request.batch_id {
        let Some(ids) = pending.batches.get(batch_id) else {
            return Result::Stale;
        };
        if ids.len() != expected.len()
            || !ids.iter().all(|id| expected.contains_key(id))
            || !complete_scoped_batch(
                &pending,
                ids,
                request.session_id,
                batch_id,
                batch.as_ref().expect("validated batch").session_id,
            )
        {
            return Result::Stale;
        }
    }
    for (id, digest) in &expected {
        let Some(permission) = pending.entries.get(*id) else {
            return Result::Stale;
        };
        if pending.identities.get(*id) != Some(*digest)
            || !permission.summary.is_addressed_to(request.session_id)
            || (request.batch_id.is_none() && permission.summary.batch.is_some())
        {
            return Result::Stale;
        }
        if request.remember && !permission.summary.can_remember_policy {
            return Result::Invalid;
        }
    }
    if request.batch_id.is_some() {
        let batch = batch.as_ref().expect("validated batch");
        return complete_conditional_batch(state, &request, pending, decision, batch).await;
    }
    // Individual admission is claimed under the same fence; owned completion survives
    // caller cancellation. Recovered checkpoints retain their existing recovery owner.
    let permissions = expected
        .keys()
        .map(|id| {
            pending
                .remove(id)
                .expect("validated checkpoint under exclusive lock")
        })
        .collect::<Vec<_>>();
    drop(pending);
    complete_conditional_permissions(state, &request, permissions, decision).await
}

async fn complete_conditional_batch(
    state: &ServerState,
    request: &bcode_session_models::ConditionalPermissionDecision,
    mut pending: tokio::sync::MutexGuard<'_, PendingPermissions>,
    mut decision: Option<tokio::sync::MutexGuard<'_, Option<bool>>>,
    batch: &PendingPermissionBatch,
) -> bcode_session_models::ConditionalPermissionResult {
    use bcode_session_models::ConditionalPermissionResult as Result;
    // Keep the checkpoint fence through commit so cancellation, replacement and late
    // registration cannot race the exact set validated by the admission owner.
    let permissions = request
        .expected
        .iter()
        .map(|identity| pending.entries[&identity.permission_id].clone())
        .collect::<Vec<_>>();
    let source = permissions[0].summary.session_id;
    let ids = permissions
        .iter()
        .map(|value| value.summary.permission_id.clone())
        .collect();
    let events = match state
        .sessions
        .append_permission_batch_resolved(source, ids, request.approved)
        .await
    {
        Ok(events) => events,
        Err(error) => {
            tracing::warn!("failed to commit conditional permission batch: {error}");
            return Result::CompletionFailed;
        }
    };
    for identity in &request.expected {
        pending
            .remove(&identity.permission_id)
            .expect("validated checkpoint under exclusive lock");
    }
    batch
        .scoped_closed
        .store(true, std::sync::atomic::Ordering::Release);
    if let Some(decision) = &mut decision {
        **decision = Some(request.approved);
    }
    drop(pending);
    drop(decision);
    for permission in &permissions {
        *permission.decision.lock().await = Some(request.approved);
        crate::xray_tools::emit_tool(
            state,
            permission.summary.session_id,
            &permission.summary.tool_call_id,
            "permission_resolved",
            serde_json::json!({
                "permission_id": permission.summary.permission_id,
                "approved": request.approved, "recorded": true, "batch": true,
            }),
        );
    }
    for permission in &permissions {
        permission.notify.notify_waiters();
    }
    // Plugin publication and routed invalidation must not delay committed waiters.
    for event in &events {
        publish_session_event(state, event).await;
    }
    for permission in &permissions {
        notify_interaction_destination(state, &permission.summary).await;
    }
    Result::Resolved {
        resolved: permissions.len(),
    }
}

async fn complete_conditional_permissions(
    state: &ServerState,
    request: &bcode_session_models::ConditionalPermissionDecision,
    permissions: Vec<PendingPermission>,
    decision: Option<tokio::sync::MutexGuard<'_, Option<bool>>>,
) -> bcode_session_models::ConditionalPermissionResult {
    use bcode_session_models::ConditionalPermissionResult as Result;
    debug_assert!(request.batch_id.is_none());
    drop(decision);
    let permission = permissions
        .into_iter()
        .next()
        .expect("validated individual");
    if complete_pending_permission(state, permission, request.approved, request.remember).await {
        Result::Resolved { resolved: 1 }
    } else {
        Result::CompletionFailed
    }
}

/// Resolve one pending permission without transport framing.
pub async fn resolve_permission(
    state: &ServerState,
    permission_id: &str,
    approved: bool,
    remember: bool,
) -> bool {
    let checkpoint = state
        .recovered_permissions
        .lock()
        .await
        .get(permission_id)
        .cloned();
    if let Some(checkpoint) = checkpoint {
        return Box::pin(resolve_recovered_permission(
            state,
            permission_id,
            &checkpoint,
            approved,
            remember,
        ))
        .await;
    }
    let Some(permission) = take_pending_permission_for_individual(state, permission_id).await
    else {
        return false;
    };
    complete_pending_permission(state, permission, approved, remember).await
}

async fn resolve_recovered_permission(
    state: &ServerState,
    permission_id: &str,
    checkpoint: &bcode_session::InvocationRecoveryCheckpoint,
    approved: bool,
    remember: bool,
) -> bool {
    if remember {
        return false;
    }
    let Ok(summary) = state
        .sessions
        .session_summary(checkpoint.session_id())
        .await
    else {
        return false;
    };
    // Cross-store decision commit still requires an execution-fenced continuation owner.
    // Do not turn a session association or a prior authority observation into a grant.
    if summary.execution.is_some() {
        return false;
    }
    let bcode_session_models::SessionEventKind::PermissionRequested { tool_call_id, .. } =
        &checkpoint.permission().kind
    else {
        return false;
    };
    let Ok(Some(current)) = state
        .sessions
        .invocation_recovery_checkpoint(
            checkpoint.session_id(),
            tool_call_id,
            checkpoint.request().sequence,
            1024,
        )
        .await
    else {
        return false;
    };
    if current.turn() != checkpoint.turn()
        || current.request() != checkpoint.request()
        || current.permission() != checkpoint.permission()
    {
        return false;
    }
    // This is a fresh validation of the exact original request. A commit conflict below
    // is returned to the caller, never retried against a silently advanced generation.
    let Ok(event) = state
        .sessions
        .append_recovered_permission_decision(&current, approved)
        .await
    else {
        return false;
    };
    publish_session_event(state, &event).await;
    state
        .recovered_permissions
        .lock()
        .await
        .remove(permission_id);
    let pending = state.pending_permissions.lock().await.remove(permission_id);
    if let Some(pending) = pending {
        notify_interaction_destination(state, &pending.summary).await;
    }
    true
}

pub async fn complete_pending_permission(
    state: &ServerState,
    permission: PendingPermission,
    approved: bool,
    remember: bool,
) -> bool {
    Box::pin(complete_pending_permission_inner(
        state, permission, approved, remember,
    ))
    .await
}

async fn complete_pending_permission_inner(
    state: &ServerState,
    permission: PendingPermission,
    approved: bool,
    remember: bool,
) -> bool {
    let checkpoint = state
        .recovered_permissions
        .lock()
        .await
        .get(&permission.summary.permission_id)
        .cloned();
    if let Some(checkpoint) = checkpoint {
        let resolved = Box::pin(resolve_recovered_permission(
            state,
            &permission.summary.permission_id,
            &checkpoint,
            approved,
            remember,
        ))
        .await;
        if !resolved {
            // Live cancellation callers may already have removed the summary. Keep the
            // unresolved recovered request visible when its fenced decision is refused.
            let mut pending = state.pending_permissions.lock().await;
            if !pending.contains_key(&permission.summary.permission_id) {
                pending.insert(permission.summary.permission_id.clone(), permission);
            }
        }
        return resolved;
    }
    // Never release a waiting invocation on a decision that has not been recorded.
    // A failed append fails closed; it must not become an in-memory approval.
    let recorded = append_permission_resolved_event(
        state,
        permission.summary.session_id,
        permission.summary.permission_id.clone(),
        approved,
    )
    .await;
    let approved = approved && recorded;
    if remember
        && recorded
        && let Some(key) = permission.skill_decision_key.clone()
    {
        remember_skill_tool_decision(
            key,
            if approved {
                SkillToolDecision::Allow
            } else {
                SkillToolDecision::Deny
            },
        );
    }
    *permission.decision.lock().await = Some(approved);
    crate::xray_tools::emit_tool(
        state,
        permission.summary.session_id,
        &permission.summary.tool_call_id,
        "permission_resolved",
        serde_json::json!({
            "permission_id": permission.summary.permission_id,
            "approved": approved, "recorded": recorded,
        }),
    );
    permission.notify.notify_waiters();
    notify_interaction_destination(state, &permission.summary).await;
    recorded
}

pub async fn take_pending_permission_for_individual(
    state: &ServerState,
    permission_id: &str,
) -> Option<PendingPermission> {
    let candidate = state
        .pending_permissions
        .lock()
        .await
        .get(permission_id)
        .cloned()?;
    let Some(correlation) = candidate.summary.batch.as_ref() else {
        return state.pending_permissions.lock().await.remove(permission_id);
    };
    let batch = state
        .pending_permission_batches
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&correlation.batch_id)
        .cloned();
    let Some(batch) = batch else {
        state.pending_permissions.lock().await.remove(permission_id);
        return None;
    };
    let batch_decision = batch.decision.lock().await;
    if batch_decision.is_some() {
        return None;
    }
    let permission = state.pending_permissions.lock().await.remove(permission_id);
    drop(batch_decision);
    permission
}

/// Deny one pending permission only when this caller wins removal from canonical pending state.
pub async fn cancel_pending_permission(state: &ServerState, permission_id: &str) -> bool {
    let permission = state.pending_permissions.lock().await.remove(permission_id);
    let Some(permission) = permission else {
        return false;
    };
    complete_pending_permission(state, permission, false, false).await
}

/// Deny and complete every pending permission for one cancelled session.
pub async fn cancel_pending_permissions_for_session(state: &ServerState, session_id: SessionId) {
    let batches = state
        .pending_permission_batches
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .values()
        .filter(|batch| batch.session_id == session_id)
        .cloned()
        .collect::<Vec<_>>();
    for batch in batches {
        let mut decision = batch.decision.lock().await;
        if decision.is_none() {
            *decision = Some(false);
        }
    }

    let permissions = {
        let mut pending = state.pending_permissions.lock().await;
        let permission_ids = pending
            .iter()
            .filter(|(_, permission)| permission.summary.session_id == session_id)
            .map(|(permission_id, _)| permission_id.clone())
            .collect::<Vec<_>>();
        let mut permissions = Vec::with_capacity(permission_ids.len());
        for permission_id in permission_ids {
            if let Some(permission) = pending.remove(&permission_id) {
                permissions.push(permission);
            }
        }
        permissions
    };
    for permission in permissions {
        complete_pending_permission(state, permission, false, false).await;
    }
}

/// Resolve one complete pending permission batch without transport framing.
pub async fn resolve_permission_batch(
    state: &Arc<ServerState>,
    batch_id: &str,
    approved: bool,
) -> usize {
    let state = Arc::clone(state);
    let batch_id = batch_id.to_owned();
    tokio::spawn(async move { resolve_permission_batch_owned(&state, &batch_id, approved).await })
        .await
        .unwrap_or(0)
}

async fn resolve_permission_batch_owned(
    state: &ServerState,
    batch_id: &str,
    approved: bool,
) -> usize {
    let Some(batch) = state
        .pending_permission_batches
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(batch_id)
        .cloned()
    else {
        return 0;
    };
    let mut batch_decision = batch.decision.lock().await;
    if batch_decision.is_some() {
        return 0;
    }
    let mut pending = state.pending_permissions.lock().await;
    let Some(ids) = pending.batches.get(batch_id).cloned() else {
        // Legacy callers may deny a registered batch before its first checkpoint.
        // There is no decision event to persist yet and no invocation can be approved.
        if !approved {
            *batch_decision = Some(false);
        }
        return 0;
    };
    if ids.is_empty()
        || ids.len() > bcode_session_models::MAX_SCOPED_PERMISSION_CHECKPOINTS
        || ids
            .iter()
            .any(|id| pending.entries[id].summary.session_id != batch.session_id)
    {
        return 0;
    }
    let permissions = ids
        .iter()
        .map(|id| pending.entries[id].clone())
        .collect::<Vec<_>>();
    let events = match state
        .sessions
        .append_permission_batch_resolved(batch.session_id, ids.iter().cloned().collect(), approved)
        .await
    {
        Ok(events) => events,
        Err(error) => {
            tracing::warn!("failed to commit permission batch: {error}");
            return 0;
        }
    };
    for id in &ids {
        pending.remove(id);
    }
    *batch_decision = Some(approved);
    drop(pending);
    drop(batch_decision);
    for permission in &permissions {
        *permission.decision.lock().await = Some(approved);
        crate::xray_tools::emit_tool(
            state,
            permission.summary.session_id,
            &permission.summary.tool_call_id,
            "permission_resolved",
            serde_json::json!({
                "permission_id": permission.summary.permission_id,
                "approved": approved, "recorded": true, "batch": true,
            }),
        );
    }
    for permission in &permissions {
        permission.notify.notify_waiters();
    }
    for event in &events {
        publish_session_event(state, event).await;
    }
    for permission in &permissions {
        notify_interaction_destination(state, &permission.summary).await;
    }
    permissions.len()
}

/// Validate and execute one complete server-owned tool exchange lifecycle.
///
/// Invocation identity is checked before registration. Explicit producer checkpoints
/// commit before readiness; durable waits are not terminalized for consumer absence.
/// With publication disabled, consumer admission retains the legacy behavior.
///
/// # Errors
///
/// Returns a normalized failure when the exchange identity is already pending.
pub async fn execute_tool_exchange(
    state: &Arc<ServerState>,
    session_id: SessionId,
    tool_call_id: &str,
    request: &bcode_session_models::ToolExchangeRequest,
    cancel_state: &TurnCancelState,
) -> Result<ToolExchangeResolution, String> {
    Box::pin(execute_tool_exchange_inner(
        state,
        session_id,
        tool_call_id,
        request,
        cancel_state,
        None,
    ))
    .await
}

/// Execute an exchange whose canonical request append has already committed.
/// Exact ID reads establish both source locators without a history scan.
///
/// # Errors
/// Rejects duplicate registration. Unverifiable canonical admission preserves the
/// unresolved invocation until explicit cancellation, never returning a false terminal failure.
pub async fn execute_tool_exchange_committed(
    state: &Arc<ServerState>,
    session_id: SessionId,
    tool_call_id: &str,
    request: &bcode_session_models::ToolExchangeRequest,
    cancel_state: &TurnCancelState,
    exchange_sequence: u64,
) -> Result<ToolExchangeResolution, String> {
    if !crate::durable_interaction::PUBLICATION_ENABLED {
        return Box::pin(execute_tool_exchange(
            state,
            session_id,
            tool_call_id,
            request,
            cancel_state,
        ))
        .await;
    }
    let facts = state
        .sessions
        .durable_interaction_source(session_id, tool_call_id.into(), request.exchange_id.clone())
        .await;
    if let Ok(facts) = facts
        && facts.exchange.sequence == exchange_sequence
        && request.invocation_id == tool_call_id
        && matches!(&facts.exchange.kind,
                bcode_session_models::SessionEventKind::ToolExchangeRequested { request: original }
                    if original == request)
    {
        return Box::pin(execute_tool_exchange_at(
            state,
            session_id,
            tool_call_id,
            request,
            cancel_state,
            facts.invocation.sequence,
            facts.exchange.sequence,
        ))
        .await;
    }
    tracing::warn!(%session_id, exchange_id = %request.exchange_id,
        "committed exchange source unavailable or conflicting; original invocation preserved");
    cancel_state.cancelled().await;
    Ok(ToolExchangeResolution::Cancelled)
}

/// Execute after the broker retained the exact canonical invocation/exchange sequences.
/// These locators are re-read and compared with the complete original request before
/// producer export; they are not execution authority or consumer-supplied proof.
///
/// # Errors
/// Rejects duplicate registration. Durable admission failures preserve the original
/// invocation wait until explicit cancellation rather than returning a terminal error.
pub async fn execute_tool_exchange_at(
    state: &Arc<ServerState>,
    session_id: SessionId,
    tool_call_id: &str,
    request: &bcode_session_models::ToolExchangeRequest,
    cancel_state: &TurnCancelState,
    invocation_sequence: u64,
    exchange_sequence: u64,
) -> Result<ToolExchangeResolution, String> {
    Box::pin(execute_tool_exchange_inner(
        state,
        session_id,
        tool_call_id,
        request,
        cancel_state,
        Some((invocation_sequence, exchange_sequence)),
    ))
    .await
}

async fn execute_tool_exchange_inner(
    state: &Arc<ServerState>,
    session_id: SessionId,
    tool_call_id: &str,
    request: &bcode_session_models::ToolExchangeRequest,
    cancel_state: &TurnCancelState,
    sequences: Option<(u64, u64)>,
) -> Result<ToolExchangeResolution, String> {
    if request.invocation_id != tool_call_id {
        return Ok(ToolExchangeResolution::Failed {
            code: "invocation_id_mismatch".to_owned(),
            message: "exchange does not belong to the active tool invocation".to_owned(),
        });
    }
    if !crate::durable_interaction::PUBLICATION_ENABLED
        && !has_exchange_consumer(state, request).await
    {
        return Ok(ToolExchangeResolution::NoCompatibleConsumer);
    }
    match sequences {
        Some(sequences) => {
            Box::pin(request_tool_exchange_inner(
                state,
                session_id,
                request,
                cancel_state,
                Some(sequences),
            ))
            .await
        }
        None => {
            Box::pin(request_tool_exchange(
                state,
                session_id,
                request,
                cancel_state,
            ))
            .await
        }
    }
}

/// Register, recheck consumer availability, and await one tool exchange terminal outcome.
///
/// # Errors
///
/// Returns a normalized failure when the exchange identity is already pending.
pub async fn request_tool_exchange(
    state: &Arc<ServerState>,
    session_id: SessionId,
    request: &bcode_session_models::ToolExchangeRequest,
    cancel_state: &TurnCancelState,
) -> Result<ToolExchangeResolution, String> {
    Box::pin(request_tool_exchange_inner(
        state,
        session_id,
        request,
        cancel_state,
        None,
    ))
    .await
}

async fn request_tool_exchange_inner(
    state: &Arc<ServerState>,
    session_id: SessionId,
    request: &bcode_session_models::ToolExchangeRequest,
    cancel_state: &TurnCancelState,
    sequences: Option<(u64, u64)>,
) -> Result<ToolExchangeResolution, String> {
    let mut observation = crate::xray_tools::InteractionObservation::start_tool(
        state,
        session_id,
        &request.invocation_id,
        "exchange_request",
    );
    let (resolution, notify, durable) = match Box::pin(register_pending_tool_exchange_inner(
        state, session_id, request, sequences,
    ))
    .await
    {
        Ok(registration) => registration,
        Err(ExchangeRegistrationError::Duplicate) => {
            observation.finish("rejected_duplicate");
            return Err(format!(
                "duplicate interactive tool request id: {}",
                request.exchange_id
            ));
        }
        Err(ExchangeRegistrationError::Durability(error)) => {
            // Returning an ordinary error would allow the broker to append a terminal
            // failure. Keep original canonical work alive and expose a stable diagnostic.
            tracing::warn!(%session_id, exchange_id = %request.exchange_id, error = error.message(),
                "durable exchange admission deferred; canonical invocation preserved");
            crate::xray_tools::emit_tool(
                state,
                session_id,
                &request.invocation_id,
                "exchange_admission_deferred",
                serde_json::json!({"exchange_id": request.exchange_id}),
            );
            cancel_state.cancelled().await;
            observation.finish("cancelled_after_admission_deferred");
            return Ok(ToolExchangeResolution::Cancelled);
        }
    };
    if !retain_without_consumer(Ok(durable)) && !has_exchange_consumer(state, request).await {
        observation.finish("consumer_detached");
        return Ok(abort_tool_exchange(
            state,
            &request.exchange_id,
            ToolExchangeResolution::ConsumerDetached,
        )
        .await);
    }
    let resolution = wait_for_tool_exchange_resolution(
        state,
        &request.exchange_id,
        &resolution,
        &notify,
        cancel_state,
    )
    .await;
    observation.finish(crate::xray_tools::exchange_outcome(&resolution));
    Ok(resolution)
}

/// Absence is conclusive only after a successful canonical check. Storage or
/// producer uncertainty preserves evidence; it never authorizes detach cancellation.
fn retain_without_consumer(
    proof: Result<bool, bcode_plugin_models::interaction_controller::InteractionControllerError>,
) -> bool {
    proof != Ok(false)
}

#[derive(Debug)]
enum ExchangeRegistrationError {
    Duplicate,
    Durability(bcode_plugin_models::interaction_controller::InteractionControllerError),
}

/// Return whether one connected client advertised a compatible exchange adapter.
pub async fn client_supports_exchange(
    state: &ServerState,
    client_id: ClientId,
    request: &bcode_session_models::ToolExchangeRequest,
) -> bool {
    state
        .client_runtime_contexts
        .lock()
        .await
        .get(&client_id)
        .is_some_and(|context| {
            context.interaction_adapters.iter().any(|adapter| {
                adapter.supports(&request.schema, request.schema_version)
                    && request.producer_id == adapter.producer_id
            })
        })
}

/// Return whether a registered caller explicitly supports current semantic controller contracts.
pub async fn client_supports_controller(state: &ServerState, client_id: ClientId) -> bool {
    state
        .client_runtime_contexts
        .lock()
        .await
        .get(&client_id)
        .is_some_and(|context| {
            context.interaction_controller_versions.contains(
                &bcode_plugin_models::interaction_controller::INTERACTION_CONTROLLER_VERSION,
            )
        })
}

/// Return whether any connected client advertised a compatible exchange adapter.
pub async fn has_exchange_consumer(
    state: &ServerState,
    request: &bcode_session_models::ToolExchangeRequest,
) -> bool {
    let (raw, semantic) = {
        let contexts = state.client_runtime_contexts.lock().await;
        (
            contexts.values().any(|context| {
                context.interaction_adapters.iter().any(|adapter| {
                    adapter.supports(&request.schema, request.schema_version)
                        && request.producer_id == adapter.producer_id
                })
            }),
            contexts.values().any(|context| {
                context.interaction_controller_versions.contains(
                    &bcode_plugin_models::interaction_controller::INTERACTION_CONTROLLER_VERSION,
                )
            }),
        )
    };
    if raw {
        return true;
    }
    if !semantic || !exchange_has_controller(state, request).await {
        return false;
    }
    state
        .client_runtime_contexts
        .lock()
        .await
        .values()
        .any(|context| {
            context.interaction_controller_versions.contains(
                &bcode_plugin_models::interaction_controller::INTERACTION_CONTROLLER_VERSION,
            )
        })
}

async fn exchange_has_controller(
    state: &ServerState,
    request: &bcode_session_models::ToolExchangeRequest,
) -> bool {
    bcode_plugin::interaction_controller::discover_controller(
        &state.plugins,
        &request.producer_id,
        &request.schema,
        request.schema_version,
        bcode_plugin::PluginInvocationScope::Global,
        &bcode_plugin_sdk::ServiceCancellation::default(),
    )
    .await
    .is_ok()
}

/// Register one pending exchange and return its shared resolution state.
///
/// # Errors
///
/// Returns a normalized failure when the exchange identity is already pending.
#[cfg(test)]
pub async fn register_pending_tool_exchange(
    state: &Arc<ServerState>,
    session_id: SessionId,
    request: &bcode_session_models::ToolExchangeRequest,
) -> Result<
    (
        Arc<Mutex<Option<ToolExchangeResolution>>>,
        Arc<tokio::sync::Notify>,
    ),
    String,
> {
    Box::pin(register_pending_tool_exchange_inner(
        state, session_id, request, None,
    ))
    .await
    .map(|(resolution, notify, _)| (resolution, notify))
    .map_err(|error| match error {
        ExchangeRegistrationError::Duplicate => format!(
            "duplicate interactive tool request id: {}",
            request.exchange_id
        ),
        ExchangeRegistrationError::Durability(error) => error.message().into(),
    })
}

async fn register_pending_tool_exchange_inner(
    state: &Arc<ServerState>,
    session_id: SessionId,
    request: &bcode_session_models::ToolExchangeRequest,
    sequences: Option<(u64, u64)>,
) -> Result<
    (
        Arc<Mutex<Option<ToolExchangeResolution>>>,
        Arc<tokio::sync::Notify>,
        bool,
    ),
    ExchangeRegistrationError,
> {
    Box::pin(register_exchange_after_checkpoint(
        state,
        session_id,
        request,
        crate::durable_interaction::prepare_wait_before_ready(
            state, session_id, request, sequences,
        ),
    ))
    .await
}

async fn register_exchange_after_checkpoint(
    state: &Arc<ServerState>,
    session_id: SessionId,
    request: &bcode_session_models::ToolExchangeRequest,
    checkpoint: impl std::future::Future<
        Output = Result<
            bool,
            bcode_plugin_models::interaction_controller::InteractionControllerError,
        >,
    >,
) -> Result<
    (
        Arc<Mutex<Option<ToolExchangeResolution>>>,
        Arc<tokio::sync::Notify>,
        bool,
    ),
    ExchangeRegistrationError,
> {
    if state
        .pending_tool_exchanges
        .lock()
        .await
        .contains_key(&request.exchange_id)
    {
        return Err(ExchangeRegistrationError::Duplicate);
    }
    // Pure producer Open/Export and canonical CAS finish before registry insertion,
    // notification, or returning readiness to the original tool.
    let durable = checkpoint
        .await
        .map_err(ExchangeRegistrationError::Durability)?;
    let route = state
        .session_current_turn(session_id)
        .await
        .and_then(|turn| turn.interaction_route);
    let pending = PendingToolExchange {
        summary: PendingToolExchangeSummary {
            interaction_route: route.clone(),
            session_id,
            request: request.clone(),
        },
        resolution: Arc::new(Mutex::new(None)),
        notify: Arc::new(tokio::sync::Notify::new()),
    };
    let resolution = Arc::clone(&pending.resolution);
    let notify = Arc::clone(&pending.notify);
    let mut exchanges = state.pending_tool_exchanges.lock().await;
    if exchanges.contains_key(&request.exchange_id) {
        return Err(ExchangeRegistrationError::Duplicate);
    }
    exchanges.insert(request.exchange_id.clone(), pending);
    drop(exchanges);
    crate::xray_tools::emit_tool(
        state,
        session_id,
        &request.invocation_id,
        "exchange_pending",
        serde_json::json!({
            "exchange_id": request.exchange_id, "durable": durable,
        }),
    );
    notify_route_destination(state, route.as_ref()).await;
    Ok((resolution, notify, durable))
}

/// Resolve pending exchanges that no longer have any compatible connected consumer.
pub async fn resolve_exchanges_without_consumers(state: &Arc<ServerState>) {
    resolve_exchanges_with_retention(state, |candidate| async move {
        crate::durable_interaction::retain_wait_on_detach(
            state,
            candidate.session_id,
            &candidate.request,
        )
        .await
    })
    .await;
}

async fn resolve_exchanges_with_retention<F, Fut>(state: &Arc<ServerState>, retention: F)
where
    F: Fn(PendingToolExchangeSummary) -> Fut,
    Fut: std::future::Future<
            Output = Result<
                bool,
                bcode_plugin_models::interaction_controller::InteractionControllerError,
            >,
        >,
{
    // Discovery runs outside all lifecycle/client/pending locks. Recheck current
    // connected capabilities at commit so a detached caller never confers authority.
    let candidates = state
        .pending_tool_exchanges
        .lock()
        .await
        .values()
        .map(|exchange| exchange.summary.clone())
        .collect::<Vec<_>>();
    let mut durable_retained = BTreeMap::new();
    for candidate in &candidates {
        let proof = retention(candidate.clone()).await;
        if let Err(error) = proof {
            tracing::warn!(session_id = %candidate.session_id,
                exchange_id = %candidate.request.exchange_id, error = error.message(),
                "durable detach verification deferred; canonical wait preserved");
        }
        if retain_without_consumer(proof) {
            durable_retained.insert(candidate.request.exchange_id.clone(), candidate.clone());
        }
    }
    let candidate_ids = candidates
        .iter()
        .map(|summary| summary.request.exchange_id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let mut controller_routes = std::collections::BTreeSet::new();
    let semantic = state
        .client_runtime_contexts
        .lock()
        .await
        .values()
        .any(|context| {
            context.interaction_controller_versions.contains(
                &bcode_plugin_models::interaction_controller::INTERACTION_CONTROLLER_VERSION,
            )
        });
    let mut inspected_routes = std::collections::BTreeSet::new();
    if semantic {
        for candidate in &candidates {
            let request = &candidate.request;
            let identity = (
                request.producer_id.clone(),
                request.schema.clone(),
                request.schema_version,
            );
            if inspected_routes.insert(identity.clone())
                && exchange_has_controller(state, request).await
            {
                controller_routes.insert(identity);
            }
        }
    }
    let mut controllers = state.interaction_controllers.lock().await;
    let contexts = state.client_runtime_contexts.lock().await;
    let mut pending = state.pending_tool_exchanges.lock().await;
    let entries = std::mem::take(&mut *pending);
    let (retained, detached): (BTreeMap<_, _>, BTreeMap<_, _>) =
        entries.into_iter().partition(|(_, exchange)| {
            let request = &exchange.summary.request;
            // Requests registered while discovery was in flight perform their own
            // consumer admission; this cleanup pass has no evidence about them.
            if !candidate_ids.contains(&request.exchange_id)
                || durable_retained
                    .get(&request.exchange_id)
                    .is_some_and(|candidate| candidate == &exchange.summary)
            {
                return true;
            }
            contexts.values().any(|context| {
                context.interaction_adapters.iter().any(|adapter| {
                    adapter.producer_id == request.producer_id
                        && adapter.supports(&request.schema, request.schema_version)
                }) || (context.interaction_controller_versions.contains(
                    &bcode_plugin_models::interaction_controller::INTERACTION_CONTROLLER_VERSION,
                ) && controller_routes.contains(&(
                    request.producer_id.clone(),
                    request.schema.clone(),
                    request.schema_version,
                )))
            })
        });
    *pending = retained;
    drop(contexts);
    drop(pending);
    for exchange in detached.into_values() {
        controllers.remove_exchange(&exchange.summary.request.exchange_id);
        *exchange.resolution.lock().await = Some(ToolExchangeResolution::ConsumerDetached);
        crate::xray_tools::emit_tool(
            state,
            exchange.summary.session_id,
            &exchange.summary.request.invocation_id,
            "exchange_resolved",
            serde_json::json!({
                "exchange_id": exchange.summary.request.exchange_id, "outcome": "consumer_detached",
            }),
        );
        exchange.notify.notify_waiters();
    }
    drop(controllers);
}

/// Return the current bounded pending tool-exchange summaries.
pub async fn list_pending_tool_exchanges(state: &ServerState) -> Vec<PendingToolExchangeSummary> {
    let mut exchanges = state
        .pending_tool_exchanges
        .lock()
        .await
        .values()
        .map(|request| request.summary.clone())
        .collect::<Vec<_>>();
    exchanges.sort_by(|left, right| left.request.exchange_id.cmp(&right.request.exchange_id));
    exchanges
}

/// Wait for one pending exchange resolution or abort it when the owning turn is cancelled.
pub async fn wait_for_tool_exchange_resolution(
    state: &ServerState,
    interaction_id: &str,
    resolution_slot: &Arc<Mutex<Option<ToolExchangeResolution>>>,
    notify: &Arc<tokio::sync::Notify>,
    cancel_state: &TurnCancelState,
) -> ToolExchangeResolution {
    loop {
        // Register before reading so a resolution committed between the read and the wait is
        // not lost: `notify_waiters` only wakes already-registered waiters.
        let notified = notify.notified();
        let mut notified = std::pin::pin!(notified);
        notified.as_mut().enable();
        let value = resolution_slot.lock().await.clone();
        if let Some(resolution) = value {
            return resolution;
        }
        tokio::select! {
            () = &mut notified => {}
            () = cancel_state.cancelled() => {
                return abort_tool_exchange(
                    state,
                    interaction_id,
                    ToolExchangeResolution::Cancelled,
                )
                .await;
            }
        }
    }
}

/// Cancel residual exchanges after the canonical session turn owner has settled
/// its inner execution, while its admission guard still prevents the next turn.
/// Callers must await this cleanup before publishing the turn terminal outcome.
/// This is not a concurrent session cancellation or a workflow-wide cleanup API.
/// Registry lock order matches semantic effects: controllers, then pending exchanges.
pub async fn cancel_session_tool_exchanges(state: &ServerState, session_id: SessionId) {
    let mut observation =
        crate::xray_tools::InteractionObservation::start(state, session_id, "exchange_cleanup");
    let removed = {
        let mut controllers = state.interaction_controllers.lock().await;
        let mut exchanges = state.pending_tool_exchanges.lock().await;
        let ids = exchanges
            .iter()
            .filter(|(_, pending)| pending.summary.session_id == session_id)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        let mut removed = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(pending) = exchanges.remove(&id) {
                controllers.remove_exchange(&id);
                removed.push(pending);
            }
        }
        drop(exchanges);
        drop(controllers);
        removed
    };
    let removed_count = removed.len();
    for pending in removed {
        let mut resolution = pending.resolution.lock().await;
        if resolution.is_none() {
            *resolution = Some(ToolExchangeResolution::Cancelled);
        }
        let outcome = resolution
            .as_ref()
            .map_or("unknown", crate::xray_tools::exchange_outcome);
        crate::xray_tools::emit_tool(
            state,
            pending.summary.session_id,
            &pending.summary.request.invocation_id,
            "exchange_cleanup_resolved",
            serde_json::json!({
                "exchange_id": pending.summary.request.exchange_id, "outcome": outcome,
            }),
        );
        drop(resolution);
        pending.notify.notify_waiters();
        notify_route_destination(state, pending.summary.interaction_route.as_ref()).await;
    }
    crate::xray_tools::emit(
        state,
        session_id,
        "exchanges_cleaned",
        serde_json::json!({"removed": removed_count}),
    );
    observation.finish("completed");
}

/// Remove one pending exchange because its owning workflow cancelled or lost its consumer.
pub async fn abort_tool_exchange(
    state: &ServerState,
    interaction_id: &str,
    resolution: ToolExchangeResolution,
) -> ToolExchangeResolution {
    let mut controllers = state.interaction_controllers.lock().await;
    controllers.remove_exchange(interaction_id);
    let pending = state
        .pending_tool_exchanges
        .lock()
        .await
        .remove(interaction_id);
    if let Some(pending) = pending {
        crate::xray_tools::emit_tool(
            state,
            pending.summary.session_id,
            &pending.summary.request.invocation_id,
            "exchange_aborted",
            serde_json::json!({
                "exchange_id": interaction_id, "outcome": crate::xray_tools::exchange_outcome(&resolution),
            }),
        );
        *pending.resolution.lock().await = Some(resolution.clone());
        pending.notify.notify_waiters();
        notify_route_destination(state, pending.summary.interaction_route.as_ref()).await;
    }
    drop(controllers);
    resolution
}

/// Complete a known pending exchange from an authoritative internal consumer.
pub async fn complete_pending_tool_exchange(
    state: &ServerState,
    interaction_id: &str,
    resolution: ToolExchangeResolution,
) -> bool {
    let mut controllers = state.interaction_controllers.lock().await;
    controllers.remove_exchange(interaction_id);
    let completed =
        complete_tool_exchange_under_controller_lock(state, interaction_id, resolution).await;
    drop(controllers);
    completed
}

/// Consume a canonical effect while the caller holds the controller lifecycle lock.
/// This lock order is shared by cancellation, raw resolution, and semantic input.
pub async fn complete_tool_exchange_under_controller_lock(
    state: &ServerState,
    interaction_id: &str,
    resolution: ToolExchangeResolution,
) -> bool {
    let request = state
        .pending_tool_exchanges
        .lock()
        .await
        .remove(interaction_id);
    let Some(request) = request else {
        return false;
    };
    crate::xray_tools::emit_tool(
        state,
        request.summary.session_id,
        &request.summary.request.invocation_id,
        "exchange_resolved",
        serde_json::json!({
            "exchange_id": interaction_id, "outcome": crate::xray_tools::exchange_outcome(&resolution),
        }),
    );
    *request.resolution.lock().await = Some(resolution);
    request.notify.notify_waiters();
    notify_route_destination(state, request.summary.interaction_route.as_ref()).await;
    true
}

const MAX_TOOL_EXCHANGE_RESOLUTION_BYTES: usize = 64 * 1024;

struct ResolutionSizeBudget {
    remaining: usize,
}

impl std::io::Write for ResolutionSizeBudget {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.remaining = self.remaining.checked_sub(bytes.len()).ok_or_else(|| {
            std::io::Error::other("tool exchange resolution exceeds encoded size limit")
        })?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn validate_resolution_size(value: &impl serde::Serialize) -> Result<(), ResolveToolExchangeError> {
    serde_json::to_writer(
        ResolutionSizeBudget {
            remaining: MAX_TOOL_EXCHANGE_RESOLUTION_BYTES,
        },
        value,
    )
    .map_err(|_| ResolveToolExchangeError::InvalidResolution)
}

/// Enforce canonical encoded bounds on a plugin-proposed semantic resolution.
///
/// # Errors
/// Returns `InvalidResolution` for oversized or malformed serialization.
pub fn validate_controller_resolution(
    value: &ToolExchangeResolution,
) -> Result<(), ResolveToolExchangeError> {
    validate_resolution_size(value)
}

/// Decode one serialized client tool-exchange resolution without exposing serde failures.
pub fn decode_tool_exchange_resolution(
    resolution_json: serde_json::Value,
) -> Result<ToolExchangeResolution, ResolveToolExchangeError> {
    validate_resolution_size(&resolution_json)?;
    let resolution = serde_json::from_value(resolution_json)
        .map_err(|_| ResolveToolExchangeError::InvalidResolution)?;
    validate_client_resolution(&resolution)?;
    Ok(resolution)
}

fn validate_client_resolution(
    resolution: &ToolExchangeResolution,
) -> Result<(), ResolveToolExchangeError> {
    match resolution {
        ToolExchangeResolution::Responded { .. } | ToolExchangeResolution::Cancelled => {
            validate_resolution_size(resolution)
        }
        ToolExchangeResolution::TimedOut
        | ToolExchangeResolution::NoCompatibleConsumer
        | ToolExchangeResolution::ConsumerDetached
        | ToolExchangeResolution::Failed { .. } => Err(ResolveToolExchangeError::InvalidResolution),
    }
}

/// Resolve one pending exchange through canonical server-owned interaction state.
///
/// Returns `Ok(false)` when the exchange is already terminal or otherwise no longer pending.
///
/// # Errors
///
/// Rejects host-owned outcomes, oversized encoded resolutions, and incompatible consumers.
/// Validation precedes pending-state lookup so direct and IPC callers obey the same restrictions.
pub async fn resolve_tool_exchange(
    state: &ServerState,
    client_id: ClientId,
    interaction_id: &str,
    resolution: ToolExchangeResolution,
) -> Result<bool, ResolveToolExchangeError> {
    validate_client_resolution(&resolution)?;
    let pending = state
        .pending_tool_exchanges
        .lock()
        .await
        .get(interaction_id)
        .cloned();
    let Some(pending) = pending else {
        return Ok(false);
    };
    if !client_supports_exchange(state, client_id, &pending.summary.request).await {
        return Err(ResolveToolExchangeError::IncompatibleConsumer);
    }
    Ok(complete_pending_tool_exchange(state, interaction_id, resolution).await)
}

#[cfg(test)]
mod controller_consumer_tests {
    use super::*;

    #[tokio::test]
    async fn settled_owner_cleanup_releases_abandoned_exchange_without_touching_sibling() {
        let state = Arc::new(crate::tests::test_server_state(
            bcode_session::SessionManager::default(),
        ));
        let session = SessionId::new();
        let sibling = SessionId::new();
        let request = bcode_session_models::ToolExchangeRequest {
            invocation_id: "abandoned-invocation".into(),
            exchange_id: "abandoned-exchange".into(),
            producer_id: "test".into(),
            schema: "test.request".into(),
            schema_version: 1,
            payload: serde_json::Value::Null,
            response_policy: bcode_session_models::ToolExchangeResponsePolicy::Required,
        };
        let (resolution, notify) = register_pending_tool_exchange(&state, session, &request)
            .await
            .unwrap();
        let mut other = request.clone();
        other.invocation_id = "sibling-invocation".into();
        other.exchange_id = "sibling-exchange".into();
        let (other_resolution, _) = register_pending_tool_exchange(&state, sibling, &other)
            .await
            .unwrap();
        let cancellation = TurnCancelState::default();
        {
            let waiter = wait_for_tool_exchange_resolution(
                &state,
                &request.exchange_id,
                &resolution,
                &notify,
                &cancellation,
            );
            tokio::pin!(waiter);
            tokio::select! { biased;
                outcome = &mut waiter => panic!("unexpected outcome: {outcome:?}"),
                () = tokio::task::yield_now() => {}
            }
        }
        // Canonical owner cleanup is awaited independently of the dropped inner
        // invocation future, with no spawned cleanup or sibling cancellation.
        let notified = notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        cancel_session_tool_exchanges(&state, session).await;
        tokio::time::timeout(std::time::Duration::from_secs(1), notified)
            .await
            .unwrap();
        assert_eq!(
            *resolution.lock().await,
            Some(ToolExchangeResolution::Cancelled)
        );
        assert!(other_resolution.lock().await.is_none());
        let pending = list_pending_tool_exchanges(&state).await;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].session_id, sibling);
        assert!(
            !complete_pending_tool_exchange(
                &state,
                &request.exchange_id,
                ToolExchangeResolution::Responded {
                    payload: serde_json::json!({"stale":true})
                }
            )
            .await
        );
        cancel_session_tool_exchanges(&state, session).await;
        assert!(other_resolution.lock().await.is_none());
        cancel_session_tool_exchanges(&state, sibling).await;
        assert!(list_pending_tool_exchanges(&state).await.is_empty());
        drop(state);
    }

    #[tokio::test]
    async fn committed_terminal_release_wakes_original_wait_without_reopening() {
        let state = Arc::new(crate::tests::test_server_state(
            bcode_session::SessionManager::default(),
        ));
        let request = bcode_session_models::ToolExchangeRequest {
            invocation_id: "committed-call".into(),
            exchange_id: "committed-exchange".into(),
            producer_id: "test".into(),
            schema: "test.request".into(),
            schema_version: 1,
            payload: serde_json::Value::Null,
            response_policy: bcode_session_models::ToolExchangeResponsePolicy::Required,
        };
        let (resolution, notify) =
            register_pending_tool_exchange(&state, SessionId::new(), &request)
                .await
                .unwrap();
        let notified = notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let controllers = state.interaction_controllers.lock().await;
        assert!(
            complete_tool_exchange_under_controller_lock(
                &state,
                &request.exchange_id,
                ToolExchangeResolution::Cancelled
            )
            .await
        );
        assert_eq!(
            *resolution.lock().await,
            Some(ToolExchangeResolution::Cancelled)
        );
        tokio::time::timeout(std::time::Duration::from_millis(100), notified)
            .await
            .unwrap();
        assert!(
            !complete_tool_exchange_under_controller_lock(
                &state,
                &request.exchange_id,
                ToolExchangeResolution::Responded {
                    payload: serde_json::json!({"stale": true})
                }
            )
            .await
        );
        assert_eq!(
            *resolution.lock().await,
            Some(ToolExchangeResolution::Cancelled)
        );
        assert!(state.pending_tool_exchanges.lock().await.is_empty());
        drop(controllers);
        drop(state);
    }

    #[tokio::test]
    async fn durable_registration_is_invisible_until_checkpoint_commits() {
        let state = Arc::new(crate::tests::test_server_state(
            bcode_session::SessionManager::default(),
        ));
        let session = SessionId::new();
        let request = bcode_session_models::ToolExchangeRequest {
            invocation_id: "before-ready-call".into(),
            exchange_id: "before-ready-wait".into(),
            producer_id: "test".into(),
            schema: "test.request".into(),
            schema_version: 1,
            payload: serde_json::Value::Null,
            response_policy: bcode_session_models::ToolExchangeResponsePolicy::Required,
        };
        let (commit, checkpoint) = tokio::sync::oneshot::channel();
        {
            let registration =
                register_exchange_after_checkpoint(&state, session, &request, async {
                    checkpoint.await.unwrap()
                });
            tokio::pin!(registration);
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(10), &mut registration)
                    .await
                    .is_err()
            );
            assert!(list_pending_tool_exchanges(&state).await.is_empty());
            commit.send(Ok(true)).unwrap();
            let (_, _, durable) = registration.await.unwrap();
            assert!(durable);
            assert_eq!(list_pending_tool_exchanges(&state).await.len(), 1);
        }
        drop(state);
    }

    #[tokio::test]
    async fn failed_durable_checkpoint_does_not_advertise_pending_request() {
        let state = Arc::new(crate::tests::test_server_state(
            bcode_session::SessionManager::default(),
        ));
        let request = bcode_session_models::ToolExchangeRequest {
            invocation_id: "failed-call".into(),
            exchange_id: "failed-wait".into(),
            producer_id: "test".into(),
            schema: "test.request".into(),
            schema_version: 1,
            payload: serde_json::Value::Null,
            response_policy: bcode_session_models::ToolExchangeResponsePolicy::Required,
        };
        let result = register_exchange_after_checkpoint(&state, SessionId::new(), &request, async {
            Err(bcode_plugin_models::interaction_controller::InteractionControllerError::Unavailable)
        }).await;
        assert!(matches!(
            result,
            Err(ExchangeRegistrationError::Durability(_))
        ));
        assert!(list_pending_tool_exchanges(&state).await.is_empty());
        drop(state);
    }

    #[tokio::test]
    async fn detach_preserves_durable_wait_and_legacy_wait_still_resolves() {
        let state = Arc::new(crate::tests::test_server_state(
            bcode_session::SessionManager::default(),
        ));
        let session = SessionId::new();
        let durable = bcode_session_models::ToolExchangeRequest {
            invocation_id: "durable-call".into(),
            exchange_id: "durable-wait".into(),
            producer_id: "test".into(),
            schema: "test.request".into(),
            schema_version: 1,
            payload: serde_json::Value::Null,
            response_policy: bcode_session_models::ToolExchangeResponsePolicy::Required,
        };
        let mut legacy = durable.clone();
        legacy.invocation_id = "legacy-call".into();
        legacy.exchange_id = "legacy-wait".into();
        let (durable_resolution, _) = register_pending_tool_exchange(&state, session, &durable)
            .await
            .unwrap();
        let (legacy_resolution, _) = register_pending_tool_exchange(&state, session, &legacy)
            .await
            .unwrap();
        resolve_exchanges_with_retention(&state, |candidate| async move {
            Ok(candidate.request.exchange_id == "durable-wait")
        })
        .await;
        assert!(durable_resolution.lock().await.is_none());
        assert_eq!(
            *legacy_resolution.lock().await,
            Some(ToolExchangeResolution::ConsumerDetached)
        );
        let pending = state.pending_tool_exchanges.lock().await;
        assert!(pending.contains_key(&durable.exchange_id));
        assert!(!pending.contains_key(&legacy.exchange_id));
        drop(pending);
        drop(state);
    }

    #[tokio::test]
    async fn detach_unavailable_canonical_proof_preserves_wait_without_answering() {
        let state = Arc::new(crate::tests::test_server_state(
            bcode_session::SessionManager::default(),
        ));
        let session = SessionId::new();
        let request = bcode_session_models::ToolExchangeRequest {
            invocation_id: "uncertain-call".into(),
            exchange_id: "uncertain-wait".into(),
            producer_id: "test".into(),
            schema: "test.request".into(),
            schema_version: 1,
            payload: serde_json::Value::Null,
            response_policy: bcode_session_models::ToolExchangeResponsePolicy::Required,
        };
        let (resolution, _) = register_pending_tool_exchange(&state, session, &request)
            .await
            .unwrap();
        resolve_exchanges_with_retention(&state, |_| async {
            Err(bcode_plugin_models::interaction_controller::InteractionControllerError::Unavailable)
        }).await;
        assert!(resolution.lock().await.is_none());
        assert!(
            state
                .pending_tool_exchanges
                .lock()
                .await
                .contains_key(&request.exchange_id)
        );
    }

    #[tokio::test]
    async fn disabled_publication_keeps_legacy_no_consumer_outcome() {
        let state = Arc::new(crate::tests::test_server_state(
            bcode_session::SessionManager::default(),
        ));
        let request = bcode_session_models::ToolExchangeRequest {
            invocation_id: "legacy-call".into(),
            exchange_id: "legacy-wait".into(),
            producer_id: "test".into(),
            schema: "test.request".into(),
            schema_version: 1,
            payload: serde_json::Value::Null,
            response_policy: bcode_session_models::ToolExchangeResponsePolicy::Required,
        };
        assert_eq!(
            execute_tool_exchange(
                &state,
                SessionId::new(),
                &request.invocation_id,
                &request,
                &TurnCancelState::default()
            )
            .await
            .unwrap(),
            ToolExchangeResolution::NoCompatibleConsumer
        );
        assert!(state.pending_tool_exchanges.lock().await.is_empty());
    }

    #[tokio::test]
    async fn canonical_cancel_notifies_pending_waiters_and_stale_effect_cannot_reopen() {
        let state = Arc::new(crate::tests::test_server_state(
            bcode_session::SessionManager::default(),
        ));
        let session = SessionId::new();
        let request = bcode_session_models::ToolExchangeRequest {
            invocation_id: "cancel-invocation".into(),
            exchange_id: "cancel-exchange".into(),
            producer_id: "test".into(),
            schema: "test.request".into(),
            schema_version: 1,
            payload: serde_json::Value::Null,
            response_policy: bcode_session_models::ToolExchangeResponsePolicy::Required,
        };
        let (resolution, _) = register_pending_tool_exchange(&state, session, &request)
            .await
            .unwrap();
        assert_eq!(
            abort_tool_exchange(
                &state,
                &request.exchange_id,
                ToolExchangeResolution::Cancelled
            )
            .await,
            ToolExchangeResolution::Cancelled
        );
        assert_eq!(
            *resolution.lock().await,
            Some(ToolExchangeResolution::Cancelled)
        );
        assert!(
            !complete_pending_tool_exchange(
                &state,
                &request.exchange_id,
                ToolExchangeResolution::Responded {
                    payload: serde_json::json!({"stale":true})
                }
            )
            .await
        );
        assert_eq!(
            *resolution.lock().await,
            Some(ToolExchangeResolution::Cancelled)
        );
        assert!(list_pending_tool_exchanges(&state).await.is_empty());
    }

    #[tokio::test]
    async fn semantic_capability_uses_exact_dynamic_discovery_without_granting_raw_resolution() {
        let plugin = bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/question-plugin/bcode-plugin.toml"),
            bcode_question_plugin::static_plugin(),
        );
        let state = crate::tests::test_server_state_with_plugins(
            bcode_session::SessionManager::default(),
            bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
                &bcode_plugin::PluginSelection {
                    mode: bcode_plugin::PluginSelectionMode::Explicit,
                    enabled: std::collections::BTreeSet::from(["bcode.question".to_owned()]),
                    disabled: std::collections::BTreeSet::new(),
                },
                &[plugin],
            )
            .unwrap(),
        );
        let client = ClientId::new();
        let mut context = bcode_ipc::ClientRuntimeContext::default();
        context.interaction_controller_versions.insert(2);
        state
            .client_runtime_contexts
            .lock()
            .await
            .insert(client, context.clone());
        let mut request = bcode_session_models::ToolExchangeRequest {
            invocation_id: "invocation".into(),
            exchange_id: "exchange".into(),
            producer_id: "bcode.question".into(),
            schema: "bcode.question.request".into(),
            schema_version: 1,
            payload: serde_json::json!({"questions":[{"text":"Proceed?","custom":true}]}),
            response_policy: bcode_session_models::ToolExchangeResponsePolicy::Required,
        };
        assert!(!has_exchange_consumer(&state, &request).await);
        context.interaction_controller_versions.insert(1);
        state
            .client_runtime_contexts
            .lock()
            .await
            .insert(client, context);
        assert!(has_exchange_consumer(&state, &request).await);
        assert!(client_supports_controller(&state, client).await);
        assert!(!client_supports_exchange(&state, client, &request).await);
        request.schema_version = 2;
        assert!(!has_exchange_consumer(&state, &request).await);
        request.schema_version = 1;
        request.producer_id = "spoofed".into();
        assert!(!has_exchange_consumer(&state, &request).await);
        drop(state);
    }
}

#[cfg(test)]
mod resolution_size_tests {
    use super::{
        MAX_TOOL_EXCHANGE_RESOLUTION_BYTES, ResolutionSizeBudget, validate_resolution_size,
    };
    use std::io::Write as _;

    #[test]
    fn responded_envelope_obeys_exact_encoded_limit_on_both_paths() {
        for character in ['a', '\n'] {
            let empty = super::ToolExchangeResolution::Responded {
                payload: serde_json::Value::String(String::new()),
            };
            let overhead = serde_json::to_vec(&empty).unwrap().len();
            let encoded_width = if character == '\n' { 2 } else { 1 };
            let remaining = MAX_TOOL_EXCHANGE_RESOLUTION_BYTES - overhead;
            let mut payload = character.to_string().repeat(remaining / encoded_width);
            payload.push_str(&"a".repeat(remaining % encoded_width));
            let exact = super::ToolExchangeResolution::Responded {
                payload: serde_json::Value::String(payload.clone()),
            };
            assert_eq!(
                serde_json::to_vec(&exact).unwrap().len(),
                MAX_TOOL_EXCHANGE_RESOLUTION_BYTES
            );
            assert!(super::validate_client_resolution(&exact).is_ok());
            assert_eq!(
                super::decode_tool_exchange_resolution(serde_json::to_value(&exact).unwrap())
                    .unwrap(),
                exact
            );
            payload.push('a');
            let oversized = super::ToolExchangeResolution::Responded {
                payload: serde_json::Value::String(payload),
            };
            assert_eq!(
                super::validate_client_resolution(&oversized),
                Err(super::ResolveToolExchangeError::InvalidResolution)
            );
            assert_eq!(
                super::decode_tool_exchange_resolution(serde_json::to_value(&oversized).unwrap()),
                Err(super::ResolveToolExchangeError::InvalidResolution)
            );
        }
    }

    #[test]
    fn encoded_resolution_size_accepts_exact_limit_and_rejects_next_byte() {
        let exact = "a".repeat(MAX_TOOL_EXCHANGE_RESOLUTION_BYTES - 2);
        assert!(validate_resolution_size(&exact).is_ok());
        assert!(validate_resolution_size(&(exact + "a")).is_err());
    }

    #[test]
    fn encoded_resolution_size_counts_json_escaping() {
        let escaped = "\n".repeat(MAX_TOOL_EXCHANGE_RESOLUTION_BYTES / 2);
        assert!(escaped.len() < MAX_TOOL_EXCHANGE_RESOLUTION_BYTES);
        assert!(validate_resolution_size(&escaped).is_err());
    }

    #[test]
    fn resolution_budget_rejects_overflow_without_consuming_budget() {
        let mut budget = ResolutionSizeBudget { remaining: 3 };
        budget.write_all(b"abc").expect("exact budget");
        assert_eq!(budget.remaining, 0);
        assert!(budget.write_all(b"d").is_err());
        assert_eq!(budget.remaining, 0);
    }
}
