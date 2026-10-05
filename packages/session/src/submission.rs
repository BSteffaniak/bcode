//! Transactional submission projection. Canonical lifecycle events remain authoritative.

use crate::db::{SessionDbError, SessionDbResult};
use crate::db_event_store::seq_to_value;
use crate::db_row::required_string;
use bcode_session_models::{
    SessionEvent, SessionEventKind, SubmissionId, SubmissionReceipt, SubmissionRecord,
    SubmissionStatus,
};
use switchy::database::{Database, query::FilterableQuery};

pub async fn lookup(
    db: &dyn Database,
    id: &SubmissionId,
) -> SessionDbResult<Option<SubmissionRecord>> {
    db.select("submissions")
        .columns(&["record"])
        .where_eq("submission_id", id.0.clone())
        .execute_first(db)
        .await?
        .map(|row| Ok(serde_json::from_str(&required_string(&row, "record")?)?))
        .transpose()
}

fn invalid() -> SessionDbError {
    SessionDbError::InvalidData("conflicting submission lifecycle transition".into())
}

fn validate_descriptor(
    descriptor: Option<&bcode_session_models::SubmissionExecutionDescriptor>,
) -> SessionDbResult<()> {
    if let Some(d) = descriptor {
        if d.version != 1
            || [&d.provider, &d.model, &d.config_identity]
                .iter()
                .any(|v| v.is_empty() || v.len() > 256)
            || [&d.model_profile, &d.auth_profile]
                .iter()
                .any(|v| v.as_ref().is_some_and(|s| s.is_empty() || s.len() > 256))
        {
            return Err(invalid());
        }
        let mut metadata = bcode_session_models::TurnAdmissionMetadata::default();
        metadata.execution.reasoning = d.reasoning.clone().map(Box::new);
        metadata.validate().map_err(|_| invalid())?;
    }
    Ok(())
}

fn observe(
    mut record: SubmissionRecord,
    issue: bcode_session_models::SubmissionContextIssue,
    event_sequence: u64,
) -> SessionDbResult<SubmissionRecord> {
    if record.status != SubmissionStatus::Pending {
        return Err(invalid());
    }
    record.context_observation = Some(bcode_session_models::SubmissionContextObservation {
        issue,
        event_sequence,
    });
    Ok(record)
}

pub async fn project(db: &dyn Database, event: &SessionEvent) -> SessionDbResult<()> {
    let record = match &event.kind {
        SessionEventKind::SubmissionAccepted { submission } => {
            submission.admission.validate().map_err(|_| invalid())?;
            validate_descriptor(submission.execution_descriptor.as_ref())?;
            if submission.id.0.is_empty()
                || submission.id.0.len() > 256
                || submission.target.turn_id().0.is_empty()
                || matches!(
                    (&submission.requested_placement, &submission.target),
                    (
                        bcode_session_models::SubmissionPlacement::FollowUp,
                        bcode_session_models::SubmissionTarget::Steering { .. }
                    )
                )
                || lookup(db, &submission.id).await?.is_some()
            {
                return Err(invalid());
            }
            SubmissionRecord {
                receipt: SubmissionReceipt {
                    submission_id: submission.id.clone(),
                    accepted_event_sequence: event.sequence,
                },
                submission: submission.clone(),
                status: SubmissionStatus::Pending,
                context_observation: None,
            }
        }
        SessionEventKind::SubmissionActivated {
            submission_id,
            turn_id,
            client_id,
            text,
            admission,
        } => {
            let mut record = lookup(db, submission_id).await?.ok_or_else(invalid)?;
            if record.status != SubmissionStatus::Pending
                || record.submission.target.turn_id() != turn_id
                || record.submission.client_id != *client_id
                || record.submission.text != *text
                || record.submission.admission != *admission
            {
                return Err(invalid());
            }
            // A terminal turn cannot receive newly activated input, including stale steering.
            if db
                .select("turn_evidence")
                .columns(&["event_seq"])
                .where_eq("turn_id", turn_id.0.clone())
                .where_eq("evidence_kind", "terminal")
                .where_lt("event_seq", seq_to_value(event.sequence))
                .limit(1)
                .execute_first(db)
                .await?
                .is_some()
            {
                return Err(invalid());
            }
            record.status = SubmissionStatus::Activated {
                turn_id: turn_id.clone(),
                conversation_sequence: event.sequence,
            };
            record
        }
        SessionEventKind::SubmissionCancelled { submission_id } => {
            let mut record = lookup(db, submission_id).await?.ok_or_else(invalid)?;
            if record.status != SubmissionStatus::Pending {
                return Err(invalid());
            }
            record.status = SubmissionStatus::Cancelled {
                event_sequence: event.sequence,
            };
            record
        }
        SessionEventKind::SubmissionContextObserved {
            submission_id,
            issue,
        } => {
            let record = lookup(db, submission_id).await?.ok_or_else(invalid)?;
            observe(record, *issue, event.sequence)?
        }
        _ => return Ok(()),
    };
    store_record(db, &record).await
}

async fn store_record(db: &dyn Database, record: &SubmissionRecord) -> SessionDbResult<()> {
    db.upsert("submissions")
        .unique(&["submission_id"])
        .value("submission_id", record.receipt.submission_id.0.clone())
        .value(
            "accepted_seq",
            seq_to_value(record.receipt.accepted_event_sequence),
        )
        .value(
            "pending",
            i64::from(record.status == SubmissionStatus::Pending),
        )
        .value(
            "target_turn_id",
            record.submission.target.turn_id().0.clone(),
        )
        .value("record", serde_json::to_string(record)?)
        .execute(db)
        .await?;
    Ok(())
}

pub fn memory_transition(
    kind: &SessionEventKind,
    sequence: u64,
    records: &std::collections::BTreeMap<SubmissionId, SubmissionRecord>,
    evidence: &std::collections::BTreeMap<String, Vec<SessionEvent>>,
    running: &std::collections::BTreeSet<String>,
) -> SessionDbResult<SubmissionRecord> {
    match kind {
        SessionEventKind::SubmissionAccepted { submission } => {
            submission.admission.validate().map_err(|_| invalid())?;
            validate_descriptor(submission.execution_descriptor.as_ref())?;
            if submission.id.0.is_empty()
                || submission.id.0.len() > 256
                || submission.target.turn_id().0.is_empty()
                || matches!(
                    (&submission.requested_placement, &submission.target),
                    (
                        bcode_session_models::SubmissionPlacement::FollowUp,
                        bcode_session_models::SubmissionTarget::Steering { .. }
                    )
                )
                || records.contains_key(&submission.id)
            {
                return Err(invalid());
            }
            Ok(SubmissionRecord {
                receipt: SubmissionReceipt {
                    submission_id: submission.id.clone(),
                    accepted_event_sequence: sequence,
                },
                submission: submission.clone(),
                status: SubmissionStatus::Pending,
                context_observation: None,
            })
        }
        SessionEventKind::SubmissionActivated {
            submission_id,
            turn_id,
            text,
            client_id,
            admission,
        } => {
            let mut record = records.get(submission_id).cloned().ok_or_else(invalid)?;
            if record.status != SubmissionStatus::Pending
                || record.submission.target.turn_id() != turn_id
                || record.submission.text != *text
                || record.submission.client_id != *client_id
                || record.submission.admission != *admission
                || evidence.get(&turn_id.0).is_some_and(|events| {
                    events
                        .iter()
                        .any(|e| matches!(e.kind, SessionEventKind::ModelTurnFinished { .. }))
                })
            {
                return Err(invalid());
            }
            if matches!(
                record.submission.target,
                bcode_session_models::SubmissionTarget::Steering { .. }
            ) && !running.contains(&format!("model_{turn_id}"))
            {
                return Err(invalid());
            }
            record.status = SubmissionStatus::Activated {
                turn_id: turn_id.clone(),
                conversation_sequence: sequence,
            };
            Ok(record)
        }
        SessionEventKind::SubmissionCancelled { submission_id } => {
            let mut record = records.get(submission_id).cloned().ok_or_else(invalid)?;
            if record.status != SubmissionStatus::Pending {
                return Err(invalid());
            }
            record.status = SubmissionStatus::Cancelled {
                event_sequence: sequence,
            };
            Ok(record)
        }
        SessionEventKind::SubmissionContextObserved {
            submission_id,
            issue,
        } => observe(
            records.get(submission_id).cloned().ok_or_else(invalid)?,
            *issue,
            sequence,
        ),
        _ => Err(invalid()),
    }
}

/// Validate a newly appended activation against the currently running steering target.
/// Replay validates canonical transition facts instead of today's runtime-work status.
pub async fn validate_activation(db: &dyn Database, event: &SessionEvent) -> SessionDbResult<()> {
    let SessionEventKind::SubmissionActivated {
        submission_id,
        turn_id,
        ..
    } = &event.kind
    else {
        return Ok(());
    };
    let record = lookup(db, submission_id).await?.ok_or_else(invalid)?;
    if matches!(
        record.submission.target,
        bcode_session_models::SubmissionTarget::Steering { .. }
    ) {
        let row = db
            .select("runtime_work")
            .columns(&["kind", "status"])
            .where_eq("work_id", format!("model_{turn_id}"))
            .execute_first(db)
            .await?
            .ok_or_else(invalid)?;
        if required_string(&row, "kind")? != "model_turn"
            || required_string(&row, "status")? != "running"
        {
            return Err(invalid());
        }
    }
    Ok(())
}

/// Convert activation into the established model-message contract, preserving activation order.
/// This is a derived view, never a rewrite of canonical history.
pub fn context_event(mut event: SessionEvent) -> SessionEvent {
    if let SessionEventKind::SubmissionActivated {
        client_id,
        text,
        admission,
        ..
    } = event.kind
    {
        event.kind = SessionEventKind::UserMessage {
            client_id,
            text,
            admission,
        };
    }
    event
}

impl crate::SessionManager {
    /// Admit an independent queued turn without activating its input into conversation.
    /// The acceptance sequence determines both the `TurnReceipt` and fixed own-turn target.
    /// Activate `SubmissionId::for_queued_turn(&receipt.turn_id)` before executing the turn.
    /// # Errors
    /// Returns storage/ownership errors, invalid metadata, or conflicting producer/key retries.
    pub async fn admit_queued_turn_with_events(
        &self,
        session_id: bcode_session_models::SessionId,
        client_id: bcode_session_models::ClientId,
        text: String,
        admission: bcode_session_models::TurnAdmissionMetadata,
    ) -> Result<(bcode_session_models::TurnAdmission, Vec<SessionEvent>), crate::SessionError> {
        self.admit_queued_turn_with_descriptor_and_events(
            session_id, client_id, text, admission, None,
        )
        .await
    }

    /// Admit queued input with an atomically persisted secret-free recovery descriptor.
    /// # Errors
    /// Returns storage, ownership, metadata or conflicting retry errors.
    pub async fn admit_queued_turn_with_descriptor_and_events(
        &self,
        session_id: bcode_session_models::SessionId,
        client_id: bcode_session_models::ClientId,
        text: String,
        admission: bcode_session_models::TurnAdmissionMetadata,
        descriptor: Option<bcode_session_models::SubmissionExecutionDescriptor>,
    ) -> Result<(bcode_session_models::TurnAdmission, Vec<SessionEvent>), crate::SessionError> {
        self.ensure_session_loaded(session_id).await?;
        let handle = self.session_handle(session_id).await?;
        let result = handle
            .admit_queued_turn(
                client_id,
                text,
                admission,
                descriptor,
                self.next_activity_timestamp_ms(),
            )
            .await?;
        let summary = handle.summary().await?;
        self.release_persistent_idle_session_resources(session_id)
            .await;
        for event in &result.events {
            self.publish_committed_mutation(event.clone(), summary.clone());
        }
        Ok((result.admission, result.events))
    }

    /// Read a target-indexed bounded pending page without scanning unrelated submissions.
    /// # Errors
    /// Fails closed on unavailable or stale storage.
    pub async fn pending_submissions_for_turn(
        &self,
        session_id: bcode_session_models::SessionId,
        turn_id: bcode_session_models::TurnId,
        after: Option<u64>,
        limit: usize,
    ) -> Result<Vec<SubmissionRecord>, crate::SessionError> {
        self.ensure_session_loaded(session_id).await?;
        self.session_handle(session_id)
            .await?
            .pending_submissions_for_turn(turn_id, after, limit)
            .await
    }

    /// Record a nonterminal context failure while retaining the pending scheduling obligation.
    /// # Errors
    /// Rejects observations on nonpending submissions or unavailable storage.
    pub async fn observe_submission_context(
        &self,
        session_id: bcode_session_models::SessionId,
        id: SubmissionId,
        issue: bcode_session_models::SubmissionContextIssue,
    ) -> Result<SubmissionRecord, crate::SessionError> {
        self.ensure_session_loaded(session_id).await?;
        let handle = self.session_handle(session_id).await?;
        let (record, event) = handle
            .observe_submission_context(id, issue, self.next_activity_timestamp_ms())
            .await?;
        let summary = handle.summary().await?;
        self.release_persistent_idle_session_resources(session_id)
            .await;
        if let Some(event) = event {
            self.publish_committed_mutation(event, summary);
        }
        Ok(record)
    }

    /// Exact actor-serialized context snapshot for requests and compaction.
    /// Use `through_sequence` with generation-checked append when committing compaction.
    /// # Errors
    /// Fails on missing sessions or stale projections, without repair.
    pub async fn activated_conversation_snapshot(
        &self,
        session_id: bcode_session_models::SessionId,
    ) -> Result<bcode_session_models::ActivatedConversationSnapshot, crate::SessionError> {
        self.ensure_session_loaded(session_id).await?;
        self.session_handle(session_id)
            .await?
            .activated_conversation_snapshot()
            .await
    }

    /// Accept a durable prompt without admitting an independent turn or changing model context.
    /// # Errors
    /// Returns ownership/storage errors or rejects a conflicting submission identity.
    pub async fn accept_submission(
        &self,
        session_id: bcode_session_models::SessionId,
        submission: bcode_session_models::PromptSubmission,
    ) -> Result<SubmissionRecord, crate::SessionError> {
        self.ensure_session_loaded(session_id).await?;
        let handle = self.session_handle(session_id).await?;
        let (record, event) = handle
            .accept_submission(submission, self.next_activity_timestamp_ms())
            .await?;
        let summary = handle.summary().await?;
        self.release_persistent_idle_session_resources(session_id)
            .await;
        if let Some(event) = event {
            self.publish_committed_mutation(event, summary);
        }
        Ok(record)
    }
    /// Activate a submission at a runtime-selected safe boundary into its fixed target.
    /// # Errors
    /// Returns storage errors or rejects invalid lifecycle transitions.
    pub async fn activate_submission(
        &self,
        session_id: bcode_session_models::SessionId,
        id: SubmissionId,
    ) -> Result<SubmissionRecord, crate::SessionError> {
        self.ensure_session_loaded(session_id).await?;
        let handle = self.session_handle(session_id).await?;
        let (record, event) = handle
            .activate_submission(id, self.next_activity_timestamp_ms())
            .await?;
        let summary = handle.summary().await?;
        self.release_persistent_idle_session_resources(session_id)
            .await;
        if let Some(event) = event {
            self.publish_committed_mutation(event, summary);
        }
        Ok(record)
    }
    /// Cancel a pending submission without cancelling its independent target turn.
    /// # Errors
    /// Returns storage errors or rejects already activated input.
    pub async fn cancel_submission(
        &self,
        session_id: bcode_session_models::SessionId,
        id: SubmissionId,
    ) -> Result<SubmissionRecord, crate::SessionError> {
        self.ensure_session_loaded(session_id).await?;
        let handle = self.session_handle(session_id).await?;
        let (record, event) = handle
            .cancel_submission(id, self.next_activity_timestamp_ms())
            .await?;
        let summary = handle.summary().await?;
        self.release_persistent_idle_session_resources(session_id)
            .await;
        if let Some(event) = event {
            self.publish_committed_mutation(event, summary);
        }
        Ok(record)
    }
    /// Look up one durable submission independent of history paging and compaction.
    /// # Errors
    /// Returns storage errors or stale projection errors without repairing state.
    pub async fn submission(
        &self,
        session_id: bcode_session_models::SessionId,
        id: SubmissionId,
    ) -> Result<Option<SubmissionRecord>, crate::SessionError> {
        self.ensure_session_loaded(session_id).await?;
        self.session_handle(session_id).await?.submission(id).await
    }
    /// Read an acceptance-ordered pending page capped at 256 rows.
    /// # Errors
    /// Returns storage errors or stale projection errors without repairing state.
    pub async fn pending_submissions(
        &self,
        session_id: bcode_session_models::SessionId,
        after: Option<u64>,
        limit: usize,
    ) -> Result<Vec<SubmissionRecord>, crate::SessionError> {
        self.ensure_session_loaded(session_id).await?;
        self.session_handle(session_id)
            .await?
            .pending_submissions(after, limit)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_models::{
        ClientId, PromptSubmission, SubmissionPlacement, SubmissionTarget, TurnAdmissionMetadata,
        TurnId,
    };

    fn descriptor() -> bcode_session_models::SubmissionExecutionDescriptor {
        bcode_session_models::SubmissionExecutionDescriptor {
            version: 1,
            provider: "provider".into(),
            model: "model".into(),
            reasoning: None,
            model_profile: Some("coding".into()),
            auth_profile: Some("work".into()),
            config_identity: "config-digest".into(),
        }
    }

    #[tokio::test]
    async fn target_pages_and_context_observations_preserve_backlog() {
        let root = tempfile::tempdir().unwrap();
        for manager in [
            crate::SessionManager::default(),
            crate::SessionManager::persistent(root.path()).unwrap(),
        ] {
            check_target_pages(&manager, root.path()).await;
        }
    }

    async fn check_target_pages(manager: &crate::SessionManager, directory: &std::path::Path) {
        let session = manager
            .create_session(None, directory.to_path_buf())
            .await
            .unwrap();
        let target = TurnId("target".into());
        let mut accepted = Vec::new();
        for index in 0..6 {
            let submission = PromptSubmission {
                id: SubmissionId(format!("page-{index}")),
                client_id: ClientId::new(),
                text: format!("input-{index}"),
                admission: TurnAdmissionMetadata::default(),
                requested_placement: SubmissionPlacement::FollowUp,
                target: SubmissionTarget::OwnTurn {
                    turn_id: if index % 2 == 0 {
                        target.clone()
                    } else {
                        TurnId("other".into())
                    },
                },
                execution_descriptor: Some(descriptor()),
            };
            accepted.push(
                manager
                    .accept_submission(session.id, submission)
                    .await
                    .unwrap(),
            );
        }
        assert!(
            manager
                .pending_submissions_for_turn(session.id, target.clone(), None, 0)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            manager
                .pending_submissions_for_turn(session.id, target.clone(), None, 2)
                .await
                .unwrap(),
            vec![accepted[0].clone(), accepted[2].clone()]
        );
        assert_eq!(
            manager
                .pending_submissions_for_turn(
                    session.id,
                    target.clone(),
                    Some(accepted[2].receipt.accepted_event_sequence),
                    2
                )
                .await
                .unwrap(),
            vec![accepted[4].clone()]
        );
        check_observation(
            manager,
            session.id,
            target,
            accepted[0].receipt.submission_id.clone(),
        )
        .await;
    }

    async fn check_observation(
        manager: &crate::SessionManager,
        session_id: bcode_session_models::SessionId,
        target: TurnId,
        id: SubmissionId,
    ) {
        let observed = manager
            .observe_submission_context(
                session_id,
                id.clone(),
                bcode_session_models::SubmissionContextIssue::Unavailable,
            )
            .await
            .unwrap();
        assert_eq!(observed.status, SubmissionStatus::Pending);
        assert_eq!(
            observed,
            manager
                .observe_submission_context(
                    session_id,
                    id.clone(),
                    bcode_session_models::SubmissionContextIssue::Unavailable
                )
                .await
                .unwrap()
        );
        assert_eq!(
            manager
                .pending_submissions_for_turn(session_id, target, None, 1)
                .await
                .unwrap(),
            vec![observed]
        );
        assert!(
            manager
                .model_context_events(session_id)
                .await
                .unwrap()
                .is_empty()
        );
        manager
            .cancel_submission(session_id, id.clone())
            .await
            .unwrap();
        assert!(
            manager
                .observe_submission_context(
                    session_id,
                    id,
                    bcode_session_models::SubmissionContextIssue::Incompatible
                )
                .await
                .is_err()
        );
        check_descriptor_admission(manager, session_id).await;
    }

    async fn check_descriptor_admission(
        manager: &crate::SessionManager,
        session_id: bcode_session_models::SessionId,
    ) {
        let (admission, _) = manager
            .admit_queued_turn_with_descriptor_and_events(
                session_id,
                ClientId::new(),
                "resolved".into(),
                queued_metadata(),
                Some(descriptor()),
            )
            .await
            .unwrap();
        let bcode_session_models::TurnAdmission::Accepted(receipt) = admission else {
            panic!("receipt")
        };
        let stored = manager
            .submission(session_id, SubmissionId::for_queued_turn(&receipt.turn_id))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.submission.execution_descriptor, Some(descriptor()));
        let mut incompatible = descriptor();
        incompatible.auth_profile = Some("other".into());
        assert!(
            manager
                .admit_queued_turn_with_descriptor_and_events(
                    session_id,
                    ClientId::new(),
                    "resolved".into(),
                    queued_metadata(),
                    Some(incompatible)
                )
                .await
                .is_err()
        );
        let mut unsupported = stored.submission;
        unsupported.id = SubmissionId("future-descriptor".into());
        unsupported.execution_descriptor.as_mut().unwrap().version = 2;
        assert!(
            manager
                .accept_submission(session_id, unsupported)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn queued_turn_receipt_is_durable_without_context_activation() {
        let root = tempfile::tempdir().unwrap();
        for manager in [
            crate::SessionManager::default(),
            crate::SessionManager::persistent(root.path()).unwrap(),
        ] {
            check_queued_turn(&manager, root.path()).await;
        }
    }

    fn queued_metadata() -> TurnAdmissionMetadata {
        use bcode_session_models::TurnOrigin;
        TurnAdmissionMetadata {
            origin: Some(TurnOrigin {
                producer: "workflow".into(),
                correlation_id: Some("node".into()),
                display_label: None,
            }),
            idempotency_key: Some("attempt".into()),
            ..Default::default()
        }
    }

    async fn check_queued_turn(manager: &crate::SessionManager, directory: &std::path::Path) {
        use bcode_session_models::{TurnAdmission, TurnReceipt};
        let session = manager
            .create_session(None, directory.to_path_buf())
            .await
            .unwrap();
        let metadata = queued_metadata();
        let (admitted, events) = manager
            .admit_queued_turn_with_events(
                session.id,
                ClientId::new(),
                "queued".into(),
                metadata.clone(),
            )
            .await
            .unwrap();
        let TurnAdmission::Accepted(receipt) = admitted else {
            panic!("new receipt")
        };
        assert_eq!(events.len(), 1);
        assert_eq!(
            receipt,
            TurnReceipt::from_accepted_event(session.id, events[0].sequence)
        );
        let SessionEventKind::SubmissionAccepted { submission } = &events[0].kind else {
            panic!("acceptance")
        };
        assert_eq!(
            submission.target,
            SubmissionTarget::OwnTurn {
                turn_id: receipt.turn_id.clone()
            }
        );
        let id = SubmissionId::for_queued_turn(&receipt.turn_id);
        assert_eq!(submission.id, id);
        assert!(
            manager
                .model_context_events(session.id)
                .await
                .unwrap()
                .is_empty()
        );
        let (duplicate, duplicate_events) = manager
            .admit_queued_turn_with_events(
                session.id,
                ClientId::new(),
                "queued".into(),
                metadata.clone(),
            )
            .await
            .unwrap();
        assert_eq!(duplicate, TurnAdmission::Existing(receipt.clone()));
        assert!(duplicate_events.is_empty());
        assert!(
            manager
                .admit_queued_turn_with_events(
                    session.id,
                    ClientId::new(),
                    "conflict".into(),
                    metadata.clone()
                )
                .await
                .is_err()
        );
        let mut conflict = metadata.clone();
        conflict.execution.agent_profile = Some("different".into());
        assert!(
            manager
                .admit_queued_turn_with_events(
                    session.id,
                    ClientId::new(),
                    "queued".into(),
                    conflict
                )
                .await
                .is_err()
        );
        manager.activate_submission(session.id, id).await.unwrap();
        let context = manager.model_context_events(session.id).await.unwrap();
        assert!(
            matches!(&context.last().unwrap().kind, SessionEventKind::UserMessage { text, .. } if text == "queued")
        );
        assert_eq!(
            manager
                .admit_queued_turn_with_events(
                    session.id,
                    ClientId::new(),
                    "queued".into(),
                    metadata
                )
                .await
                .unwrap()
                .0,
            TurnAdmission::Existing(receipt)
        );
    }

    #[tokio::test]
    async fn steering_requires_live_target_and_rejects_terminal_target() {
        let root = tempfile::tempdir().unwrap();
        for manager in [
            crate::SessionManager::default(),
            crate::SessionManager::persistent(root.path()).unwrap(),
        ] {
            let session = manager
                .create_session(None, root.path().to_path_buf())
                .await
                .unwrap();
            let prompt = PromptSubmission {
                execution_descriptor: None,
                id: SubmissionId("steering".into()),
                client_id: ClientId::new(),
                text: "steer".into(),
                admission: TurnAdmissionMetadata::default(),
                requested_placement: SubmissionPlacement::Steer,
                target: SubmissionTarget::Steering {
                    turn_id: TurnId("active".into()),
                },
            };
            manager
                .accept_submission(session.id, prompt.clone())
                .await
                .unwrap();
            assert!(
                manager
                    .activate_submission(session.id, prompt.id.clone())
                    .await
                    .is_err()
            );
            manager
                .append_event(
                    session.id,
                    SessionEventKind::RuntimeWorkStarted {
                        work_id: bcode_session_models::WorkId::new("model_active"),
                        kind: bcode_session_models::RuntimeWorkKind::ModelTurn,
                        label: "active".into(),
                        tool_call_id: None,
                        plugin_id: None,
                        service_interface: None,
                        operation: None,
                        parent_work_id: None,
                        started_at_ms: None,
                        cancellable: true,
                    },
                )
                .await
                .unwrap();
            manager
                .activate_submission(session.id, prompt.id.clone())
                .await
                .unwrap();
            let mut second = prompt;
            second.id = SubmissionId("stale".into());
            manager
                .accept_submission(session.id, second.clone())
                .await
                .unwrap();
            manager
                .append_event(
                    session.id,
                    SessionEventKind::ModelTurnFinished {
                        turn_id: "active".into(),
                        outcome: bcode_session_models::ModelTurnOutcome::Completed,
                        message: None,
                    },
                )
                .await
                .unwrap();
            assert!(
                manager
                    .activate_submission(session.id, second.id)
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn submission_acceptance_activation_cancellation_and_compaction() {
        let root = tempfile::tempdir().unwrap();
        let manager = crate::SessionManager::persistent(root.path()).unwrap();
        check_lifecycle(manager, root.path().to_path_buf()).await;
    }

    #[tokio::test]
    async fn in_memory_submission_lifecycle() {
        check_lifecycle(
            crate::SessionManager::default(),
            std::env::current_dir().unwrap(),
        )
        .await;
    }

    async fn check_lifecycle(manager: crate::SessionManager, directory: std::path::PathBuf) {
        let session = manager.create_session(None, directory).await.unwrap();
        let prompt = PromptSubmission {
            execution_descriptor: None,
            id: SubmissionId("followup".into()),
            client_id: ClientId::new(),
            text: "later input".into(),
            admission: TurnAdmissionMetadata::default(),
            requested_placement: SubmissionPlacement::FollowUp,
            target: SubmissionTarget::OwnTurn {
                turn_id: TurnId("own-turn".into()),
            },
        };
        let accepted = manager
            .accept_submission(session.id, prompt.clone())
            .await
            .unwrap();
        assert_eq!(
            accepted,
            manager
                .accept_submission(session.id, prompt.clone())
                .await
                .unwrap()
        );
        let mut retry = prompt.clone();
        retry.client_id = ClientId::new();
        assert_eq!(
            accepted,
            manager.accept_submission(session.id, retry).await.unwrap()
        );
        assert!(
            manager
                .model_context_events(session.id)
                .await
                .unwrap()
                .iter()
                .all(|e| !matches!(e.kind, SessionEventKind::UserMessage { .. }))
        );
        assert_eq!(
            manager
                .pending_submissions(session.id, None, 1)
                .await
                .unwrap(),
            vec![accepted.clone()]
        );
        manager
            .append_event(
                session.id,
                SessionEventKind::ContextCompacted {
                    summary: "earlier output".into(),
                    compacted_through_sequence: accepted.receipt.accepted_event_sequence,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            manager
                .submission(session.id, prompt.id.clone())
                .await
                .unwrap(),
            Some(accepted)
        );
        let activated = manager
            .activate_submission(session.id, prompt.id.clone())
            .await
            .unwrap();
        let SubmissionStatus::Activated {
            conversation_sequence,
            ..
        } = activated.status
        else {
            panic!("activation")
        };
        assert!(conversation_sequence > activated.receipt.accepted_event_sequence);
        assert_eq!(
            manager
                .activate_submission(session.id, prompt.id.clone())
                .await
                .unwrap(),
            activated
        );
        let context = manager.model_context_events(session.id).await.unwrap();
        assert!(
            matches!(&context.last().unwrap().kind, SessionEventKind::UserMessage { text, .. } if text == "later input")
        );
        assert_eq!(context.last().unwrap().sequence, conversation_sequence);
        assert!(
            manager
                .cancel_submission(session.id, prompt.id.clone())
                .await
                .is_err()
        );
        check_cancelled(&manager, session.id, prompt, context).await;
    }

    async fn check_cancelled(
        manager: &crate::SessionManager,
        session_id: bcode_session_models::SessionId,
        prompt: PromptSubmission,
        context: Vec<SessionEvent>,
    ) {
        let mut cancelled = prompt;
        cancelled.id = SubmissionId("cancelled".into());
        cancelled.text = "never visible".into();
        manager
            .accept_submission(session_id, cancelled.clone())
            .await
            .unwrap();
        let result = manager
            .cancel_submission(session_id, cancelled.id.clone())
            .await
            .unwrap();
        assert_eq!(
            manager
                .cancel_submission(session_id, cancelled.id.clone())
                .await
                .unwrap(),
            result
        );
        assert!(
            manager
                .activate_submission(session_id, cancelled.id)
                .await
                .is_err()
        );
        assert!(
            manager
                .pending_submissions(session_id, None, 256)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            manager.model_context_events(session_id).await.unwrap(),
            context
        );
    }
}
