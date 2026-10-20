//! Canonical composed creation receipt behavior; no independent child-run store.

#[cfg(test)]
mod tests {
    use crate::SessionManager;
    use bcode_session_models::{
        ClientId, SessionChildCreateRequest, SessionEventKind, SessionId, SessionSpawnRequest,
        TurnAdmission, TurnAdmissionMetadata, TurnOrigin,
    };

    fn request(parent_id: SessionId) -> SessionSpawnRequest {
        let child_id = SessionId::new();
        SessionSpawnRequest {
            version: 1,
            creation: SessionChildCreateRequest {
                child_id,
                parent_id,
                name: Some("spawned".into()),
                working_directory: std::env::current_dir().unwrap(),
            },
            worktree_directory: None,
            input: "initial input".into(),
            admission: TurnAdmissionMetadata {
                origin: Some(TurnOrigin {
                    producer: "test.spawn".into(),
                    correlation_id: None,
                    display_label: None,
                }),
                idempotency_key: Some(child_id.to_string()),
                ..TurnAdmissionMetadata::default()
            },
            auth_profile: None,
        }
    }

    #[tokio::test]
    async fn composed_creation_retries_preserve_envelope_and_reject_conflicting_inputs() {
        let root = tempfile::tempdir().unwrap();
        let manager = SessionManager::persistent(root.path()).unwrap();
        let parent = manager
            .create_session(None, std::env::current_dir().unwrap())
            .await
            .unwrap();
        let original = request(parent.id);
        let created = manager
            .create_spawning_session(original.clone())
            .await
            .unwrap();
        let retry = manager
            .create_spawning_session(original.clone())
            .await
            .unwrap();
        assert_eq!(created.id, retry.id);
        let history = manager.session_history(created.id).await.unwrap();
        assert!(
            matches!(&history[1].kind, SessionEventKind::ChildSessionCreated { spawning: Some(retained), .. } if retained.as_ref() == &original)
        );
        let mut changed = original.clone();
        changed.input.push_str(" changed");
        assert!(manager.create_spawning_session(changed).await.is_err());
        let mut changed = original.clone();
        changed.admission.execution.agent_profile = Some("other".into());
        assert!(manager.create_spawning_session(changed).await.is_err());
        let mut changed = original.clone();
        changed.auth_profile = Some("different".into());
        assert!(manager.create_spawning_session(changed).await.is_err());
        assert!(
            manager
                .create_child_session(original.creation.clone())
                .await
                .is_err()
        );
        assert_eq!(manager.session_history(created.id).await.unwrap(), history);
    }

    #[tokio::test]
    async fn composed_creation_before_admission_survives_restart_and_retains_exact_turn_receipt() {
        let root = tempfile::tempdir().unwrap();
        let original;
        {
            let manager = SessionManager::persistent(root.path()).unwrap();
            let parent = manager
                .create_session(None, std::env::current_dir().unwrap())
                .await
                .unwrap();
            original = request(parent.id);
            manager
                .create_spawning_session(original.clone())
                .await
                .unwrap();
        }
        let manager = SessionManager::persistent(root.path()).unwrap();
        let restored = manager
            .create_spawning_session(original.clone())
            .await
            .unwrap();
        assert_eq!(restored.id, original.creation.child_id);
        let accepted = manager
            .admit_turn(
                restored.id,
                ClientId::new(),
                original.input.clone(),
                original.admission.clone(),
            )
            .await
            .unwrap();
        let TurnAdmission::Accepted(receipt) = accepted else {
            panic!("expected accepted turn")
        };
        let retry = manager
            .admit_turn(
                restored.id,
                ClientId::new(),
                original.input.clone(),
                original.admission.clone(),
            )
            .await
            .unwrap();
        assert_eq!(retry, TurnAdmission::Existing(receipt.clone()));
        assert_eq!(
            manager
                .turn_receipt(
                    restored.id,
                    "test.spawn",
                    original.admission.idempotency_key.as_deref().unwrap()
                )
                .await
                .unwrap(),
            Some(receipt)
        );
        let mut changed = original;
        changed.admission.execution.model_id = Some("changed".into());
        assert!(manager.create_spawning_session(changed).await.is_err());
    }
}
