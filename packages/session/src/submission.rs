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

pub fn validate_descriptor(
    descriptor: Option<&bcode_session_models::SubmissionExecutionDescriptor>,
) -> SessionDbResult<()> {
    if let Some(d) = descriptor {
        if !matches!(d.version, 1 | 2)
            || (d.version == 1 && d.recovery_recipe.is_some())
            || [&d.provider, &d.model, &d.config_identity]
                .iter()
                .any(|v| v.is_empty() || v.len() > 256)
            || [&d.model_profile, &d.auth_profile]
                .iter()
                .any(|v| v.as_ref().is_some_and(|s| s.is_empty() || s.len() > 256))
        {
            return Err(invalid());
        }
        if let Some(r) = &d.recovery_recipe {
            if r.version != 1
                || r.model_profile != d.model_profile
                || r.auth_profile != d.auth_profile
                || [&r.model_profile, &r.auth_profile, &r.auth_pool]
                    .iter()
                    .any(|v| v.as_ref().is_some_and(|v| v.is_empty() || v.len() > 256))
            {
                return Err(invalid());
            }
            r.config_source.validate().map_err(|_| invalid())?;
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

/// Look up a durable retry identity without replaying history.
pub async fn recovery_operation(
    db: &dyn Database,
    operation_id: &str,
) -> SessionDbResult<Option<(SessionEventKind, SubmissionRecord)>> {
    db.select("submission_recovery_operations")
        .columns(&["event_kind", "record"])
        .where_eq("operation_id", operation_id.to_owned())
        .execute_first(db)
        .await?
        .map(|r| {
            Ok((
                serde_json::from_str(&required_string(&r, "event_kind")?)?,
                serde_json::from_str(&required_string(&r, "record")?)?,
            ))
        })
        .transpose()
}

pub fn recovery_identity(kind: &SessionEventKind) -> Option<&str> {
    match kind {
        SessionEventKind::SubmissionRecoveryAuthorized { operation_id, .. }
        | SessionEventKind::SubmissionPendingCancelled { operation_id, .. } => Some(operation_id),
        _ => None,
    }
}

pub fn validate_recovery_owner(record: &SubmissionRecord) -> Result<(), crate::SessionError> {
    let a = &record.submission.admission;
    if a.execution.correlation.is_some()
        || a.origin.as_ref().is_some_and(|o| {
            o.producer == "workflow"
                || o.producer.starts_with("workflow.")
                || o.producer.starts_with("bcode.workflow")
        })
    {
        return Err(crate::SessionError::EventSerialization(
            "workflow authority required".into(),
        ));
    }
    Ok(())
}

fn recover(
    mut record: SubmissionRecord,
    kind: &SessionEventKind,
    sequence: u64,
) -> SessionDbResult<SubmissionRecord> {
    let (expected, descriptor) = match kind {
        SessionEventKind::SubmissionRecoveryAuthorized {
            expected_accepted_sequence,
            descriptor,
            ..
        } => (*expected_accepted_sequence, Some(descriptor)),
        SessionEventKind::SubmissionPendingCancelled {
            expected_accepted_sequence,
            ..
        } => (*expected_accepted_sequence, None),
        _ => return Err(invalid()),
    };
    let operation_id = recovery_identity(kind).ok_or_else(invalid)?;
    if record.status != SubmissionStatus::Pending
        || record.receipt.accepted_event_sequence != expected
        || operation_id.is_empty()
        || operation_id.len() > 256
    {
        return Err(invalid());
    }
    if let Some(descriptor) = descriptor {
        validate_descriptor(Some(descriptor))?;
        if descriptor.version != 2 || descriptor.recovery_recipe.is_none() {
            return Err(invalid());
        }
        // Request permission/admission remains immutable; only context selection is authorized.
        record.recovery_authorization =
            Some(bcode_session_models::SubmissionRecoveryAuthorization {
                operation_id: operation_id.to_owned(),
                event_sequence: sequence,
                descriptor: descriptor.clone(),
            });
        record.context_observation = None;
    } else {
        record.status = SubmissionStatus::Cancelled {
            event_sequence: sequence,
        };
    }
    Ok(record)
}

pub async fn project(db: &dyn Database, event: &SessionEvent) -> SessionDbResult<()> {
    if let SessionEventKind::ModelTurnFinished { turn_id, .. } = &event.kind {
        db.update("submissions")
            .value("queue_visible", 0_i64)
            .where_eq("target_turn_id", turn_id.clone())
            .where_eq("pending", 0_i64)
            .execute(db)
            .await?;
        return Ok(());
    }
    let record = match &event.kind {
        SessionEventKind::SubmissionRecoveryAuthorized { submission_id, .. }
        | SessionEventKind::SubmissionPendingCancelled { submission_id, .. } => {
            project_recovery(db, event, submission_id).await?
        }
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
                recovery_authorization: None,
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

async fn project_recovery(
    db: &dyn Database,
    event: &SessionEvent,
    submission_id: &SubmissionId,
) -> SessionDbResult<SubmissionRecord> {
    let operation_id = recovery_identity(&event.kind).ok_or_else(invalid)?;
    if recovery_operation(db, operation_id).await?.is_some() {
        return Err(invalid());
    }
    let record = recover(
        lookup(db, submission_id).await?.ok_or_else(invalid)?,
        &event.kind,
        event.sequence,
    )?;
    db.insert("submission_recovery_operations")
        .value("operation_id", operation_id.to_owned())
        .value("event_kind", serde_json::to_string(&event.kind)?)
        .value("record", serde_json::to_string(&record)?)
        .execute(db)
        .await?;
    Ok(record)
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
        .value(
            "queue_visible",
            i64::from(!matches!(record.status, SubmissionStatus::Cancelled { .. })),
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
        SessionEventKind::SubmissionRecoveryAuthorized { submission_id, .. }
        | SessionEventKind::SubmissionPendingCancelled { submission_id, .. } => recover(
            records.get(submission_id).cloned().ok_or_else(invalid)?,
            kind,
            sequence,
        ),
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
                recovery_authorization: None,
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
    /// Authorize verified context for exactly one still-pending accepted submission.
    /// # Errors
    /// Rejects stale acceptance, terminal submissions, workflow ownership, invalid context,
    /// conflicting retries, and unavailable storage ownership. Caller verifies config first.
    pub async fn authorize_submission_recovery(
        &self,
        session_id: bcode_session_models::SessionId,
        id: SubmissionId,
        expected_sequence: u64,
        operation_id: String,
        descriptor: bcode_session_models::SubmissionExecutionDescriptor,
    ) -> Result<bcode_session_models::SubmissionRecoveryResult, crate::SessionError> {
        self.commit_submission_recovery(
            session_id,
            SessionEventKind::SubmissionRecoveryAuthorized {
                submission_id: id,
                expected_accepted_sequence: expected_sequence,
                operation_id,
                descriptor,
            },
        )
        .await
    }

    /// Cancel exactly one pending submission without changing its target or conversation.
    /// # Errors
    /// Rejects stale acceptance, activated submissions, workflow ownership and conflicting retries.
    pub async fn cancel_pending_submission(
        &self,
        session_id: bcode_session_models::SessionId,
        id: SubmissionId,
        expected_sequence: u64,
        operation_id: String,
    ) -> Result<bcode_session_models::SubmissionRecoveryResult, crate::SessionError> {
        self.commit_submission_recovery(
            session_id,
            SessionEventKind::SubmissionPendingCancelled {
                submission_id: id,
                expected_accepted_sequence: expected_sequence,
                operation_id,
            },
        )
        .await
    }

    async fn commit_submission_recovery(
        &self,
        session_id: bcode_session_models::SessionId,
        kind: SessionEventKind,
    ) -> Result<bcode_session_models::SubmissionRecoveryResult, crate::SessionError> {
        self.ensure_session_loaded(session_id).await?;
        let handle = self.session_handle(session_id).await?;
        let (record, event) = handle
            .recover_submission(kind, self.next_activity_timestamp_ms())
            .await?;
        let duplicate = event.is_none();
        let summary = handle.summary().await?;
        self.release_persistent_idle_session_resources(session_id)
            .await;
        if let Some(event) = event {
            self.publish_committed_mutation(event, summary);
        }
        Ok(bcode_session_models::SubmissionRecoveryResult { record, duplicate })
    }

    /// Read the oldest actionable queue record, including ambiguous activated input.
    /// # Errors
    /// Returns projection/storage errors without replay or repair.
    pub async fn submission_queue_status(
        &self,
        session_id: bcode_session_models::SessionId,
    ) -> Result<bcode_session_models::SubmissionQueueStatus, crate::SessionError> {
        self.ensure_session_loaded(session_id).await?;
        self.session_handle(session_id)
            .await?
            .submission_queue_status()
            .await
    }

    /// Inspect an exact retry before config I/O; the eventual mutation rechecks atomically.
    /// # Errors
    /// Rejects conflicting operation identities, workflow ownership and unavailable storage.
    pub async fn check_submission_recovery_retry(
        &self,
        session_id: bcode_session_models::SessionId,
        id: SubmissionId,
        expected_sequence: u64,
        operation_id: String,
        descriptor: Option<bcode_session_models::SubmissionExecutionDescriptor>,
    ) -> Result<Option<bcode_session_models::SubmissionRecoveryResult>, crate::SessionError> {
        self.ensure_session_loaded(session_id).await?;
        let kind = if let Some(descriptor) = descriptor {
            SessionEventKind::SubmissionRecoveryAuthorized {
                submission_id: id,
                expected_accepted_sequence: expected_sequence,
                operation_id,
                descriptor,
            }
        } else {
            SessionEventKind::SubmissionPendingCancelled {
                submission_id: id,
                expected_accepted_sequence: expected_sequence,
                operation_id,
            }
        };
        self.session_handle(session_id)
            .await?
            .check_submission_recovery_retry(kind)
            .await
    }

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

    fn recovery_descriptor() -> bcode_session_models::SubmissionExecutionDescriptor {
        use bcode_session_models::{
            SubmissionConfigSource, SubmissionConfigSources, SubmissionRecoveryRecipe,
        };
        let mut d = descriptor();
        d.version = 2;
        d.recovery_recipe = Some(Box::new(SubmissionRecoveryRecipe {
            version: 1,
            model_profile: d.model_profile.clone(),
            auth_profile: d.auth_profile.clone(),
            auth_pool: Some("pool".into()),
            config_source: SubmissionConfigSource {
                schema_version: 1,
                execution_digest: "a".repeat(64),
                source_presence: vec![true],
                sources: SubmissionConfigSources {
                    cwd: "/workspace".into(),
                    discovered_paths: vec![],
                    exclusive_config_path: Some("/config.toml".into()),
                    base_config_path: None,
                    env_config_path: None,
                    cli_config_path: None,
                    active_context: None,
                    model_profile: d.model_profile.clone(),
                    inline_overrides: false,
                },
            },
        }));
        d
    }

    #[tokio::test]
    async fn recovery_is_append_only_pending_only_and_retry_safe_in_both_stores() {
        let root = tempfile::tempdir().unwrap();
        for manager in [
            crate::SessionManager::default(),
            crate::SessionManager::persistent(root.path()).unwrap(),
        ] {
            let session = manager
                .create_session(None, root.path().to_path_buf())
                .await
                .unwrap();
            let prompt = pending_recovery_prompt("recover", "recovery-turn", "never rewrite");
            let accepted = manager
                .accept_submission(session.id, prompt.clone())
                .await
                .unwrap();
            let seq = accepted.receipt.accepted_event_sequence;
            let d = recovery_descriptor();
            assert!(
                manager
                    .authorize_submission_recovery(
                        session.id,
                        prompt.id.clone(),
                        seq + 1,
                        "stale".into(),
                        d.clone()
                    )
                    .await
                    .is_err()
            );
            let first = manager
                .authorize_submission_recovery(
                    session.id,
                    prompt.id.clone(),
                    seq,
                    "resume".into(),
                    d.clone(),
                )
                .await
                .unwrap();
            assert!(!first.duplicate);
            assert_eq!(first.record.submission, prompt);
            assert_eq!(first.record.effective_execution_descriptor(), Some(&d));
            assert!(
                manager
                    .authorize_submission_recovery(
                        session.id,
                        prompt.id.clone(),
                        seq,
                        "resume".into(),
                        descriptor()
                    )
                    .await
                    .is_err()
            );
            assert!(
                manager
                    .cancel_pending_submission(session.id, prompt.id.clone(), seq, "resume".into())
                    .await
                    .is_err()
            );
            let cancelled = manager
                .cancel_pending_submission(session.id, prompt.id.clone(), seq, "cancel".into())
                .await
                .unwrap();
            assert!(!cancelled.duplicate);
            assert!(
                manager
                    .activate_submission(session.id, prompt.id.clone())
                    .await
                    .is_err()
            );
            assert_terminal_recovery_retries(
                &manager,
                session.id,
                &prompt,
                seq,
                &d,
                &first.record,
                &cancelled.record,
            )
            .await;

            if manager.store.is_some() {
                manager.release_session_ownership(session.id).await.unwrap();
                assert_reopened_recovery_retry(
                    root.path(),
                    session.id,
                    &prompt,
                    seq,
                    d,
                    &first.record,
                )
                .await;
            }
        }
    }

    fn pending_recovery_prompt(id: &str, turn: &str, text: &str) -> PromptSubmission {
        PromptSubmission {
            id: SubmissionId(id.into()),
            client_id: ClientId::new(),
            text: text.into(),
            admission: TurnAdmissionMetadata::default(),
            requested_placement: SubmissionPlacement::FollowUp,
            target: SubmissionTarget::OwnTurn {
                turn_id: TurnId(turn.into()),
            },
            execution_descriptor: None,
        }
    }

    async fn assert_reopened_recovery_retry(
        root: &std::path::Path,
        session_id: bcode_session_models::SessionId,
        prompt: &PromptSubmission,
        seq: u64,
        d: bcode_session_models::SubmissionExecutionDescriptor,
        authorized: &SubmissionRecord,
    ) {
        let reopened = crate::SessionManager::persistent(root).unwrap();
        assert_eq!(
            reopened
                .check_submission_recovery_retry(
                    session_id,
                    prompt.id.clone(),
                    seq,
                    "resume".into(),
                    Some(d)
                )
                .await
                .unwrap()
                .unwrap()
                .record,
            *authorized
        );
    }

    async fn assert_terminal_recovery_retries(
        manager: &crate::SessionManager,
        session_id: bcode_session_models::SessionId,
        prompt: &PromptSubmission,
        seq: u64,
        d: &bcode_session_models::SubmissionExecutionDescriptor,
        authorized: &SubmissionRecord,
        cancelled: &SubmissionRecord,
    ) {
        let retry = manager
            .authorize_submission_recovery(
                session_id,
                prompt.id.clone(),
                seq,
                "resume".into(),
                d.clone(),
            )
            .await
            .unwrap();
        assert!(retry.duplicate);
        assert_eq!(retry.record, *authorized);
        assert_eq!(
            manager
                .check_submission_recovery_retry(
                    session_id,
                    prompt.id.clone(),
                    seq,
                    "resume".into(),
                    Some(d.clone())
                )
                .await
                .unwrap()
                .unwrap()
                .record,
            *authorized
        );
        assert!(
            manager
                .cancel_pending_submission(
                    session_id,
                    prompt.id.clone(),
                    seq,
                    "fresh-terminal".into()
                )
                .await
                .is_err()
        );
        assert_eq!(
            manager
                .cancel_pending_submission(session_id, prompt.id.clone(), seq, "cancel".into())
                .await
                .unwrap()
                .record,
            *cancelled
        );
        assert!(
            manager
                .submission_queue_status(session_id)
                .await
                .unwrap()
                .oldest
                .is_none()
        );
        assert!(
            manager
                .model_context_events(session_id)
                .await
                .unwrap()
                .iter()
                .all(|e| !matches!(e.kind, SessionEventKind::UserMessage { .. }))
        );
    }

    #[tokio::test]
    async fn recovery_activation_cancel_race_never_resurrects_or_replays() {
        let root = tempfile::tempdir().unwrap();
        for manager in [
            crate::SessionManager::default(),
            crate::SessionManager::persistent(root.path()).unwrap(),
        ] {
            let session = manager
                .create_session(None, root.path().to_path_buf())
                .await
                .unwrap();
            let prompt = pending_recovery_prompt("race", "race-turn", "command");
            let accepted = manager
                .accept_submission(session.id, prompt.clone())
                .await
                .unwrap();
            let (activated, cancelled) = tokio::join!(
                manager.activate_submission(session.id, prompt.id.clone()),
                manager.cancel_pending_submission(
                    session.id,
                    prompt.id.clone(),
                    accepted.receipt.accepted_event_sequence,
                    "cancel-race".into()
                )
            );
            assert_ne!(activated.is_ok(), cancelled.is_ok());
            if activated.is_ok() {
                let status = manager.submission_queue_status(session.id).await.unwrap();
                assert_eq!(
                    status.blocker,
                    Some(bcode_session_models::SubmissionQueueBlocker::InterruptedTurn)
                );
                assert!(
                    manager
                        .authorize_submission_recovery(
                            session.id,
                            prompt.id,
                            accepted.receipt.accepted_event_sequence,
                            "no-replay".into(),
                            recovery_descriptor()
                        )
                        .await
                        .is_err()
                );
            }
        }
    }

    #[test]
    fn recovery_codec_rejects_future_versions_with_secret_safe_errors() {
        let d = recovery_descriptor();
        let event = SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            session_id: bcode_session_models::SessionId::new(),
            sequence: 2,
            timestamp_ms: 3,
            provenance: None,
            kind: SessionEventKind::SubmissionRecoveryAuthorized {
                submission_id: SubmissionId("codec".into()),
                expected_accepted_sequence: 1,
                operation_id: "operation".into(),
                descriptor: d,
            },
        };
        let payload = crate::persisted::encode_session_event(&event).unwrap();
        assert_eq!(
            crate::persisted::decode_session_event(&payload).unwrap(),
            event
        );
        let mut value: serde_json::Value = serde_json::from_str(&payload).unwrap();
        value["kind"]["submission_recovery_authorized"]["descriptor"]["recovery_recipe"]["config_source"]
            ["schema_version"] = serde_json::json!(999);
        value["kind"]["submission_recovery_authorized"]["descriptor"]["recovery_recipe"]["config_source"]
            ["execution_digest"] = serde_json::json!("secret-token-never-print");
        let error = crate::persisted::decode_session_event(&value.to_string())
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "invalid or unsupported submission execution descriptor"
        );
        assert!(!error.contains("secret-token"));
        value["schema_version"] = serde_json::json!(999);
        assert!(crate::persisted::decode_session_event(&value.to_string()).is_err());
    }

    #[tokio::test]
    async fn schema_51_upgrade_preserves_null_and_v1_pending_descriptors() {
        let root = tempfile::tempdir().unwrap();
        let id = bcode_session_models::SessionId::new();
        let db = crate::db::SessionDb::initialize_turso_in_root(id, root.path())
            .await
            .unwrap();
        let mut events = vec![SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            session_id: id,
            sequence: 0,
            timestamp_ms: 1,
            provenance: None,
            kind: SessionEventKind::SessionCreated {
                name: None,
                working_directory: root.path().into(),
            },
        }];
        for (index, descriptor) in [None, Some(descriptor())].into_iter().enumerate() {
            events.push(SessionEvent {
                schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                session_id: id,
                sequence: index as u64 + 1,
                timestamp_ms: index as u64 + 2,
                provenance: None,
                kind: SessionEventKind::SubmissionAccepted {
                    submission: PromptSubmission {
                        id: SubmissionId(format!("legacy-{index}")),
                        client_id: ClientId::new(),
                        text: "preserve exact input".into(),
                        admission: TurnAdmissionMetadata::default(),
                        requested_placement: SubmissionPlacement::FollowUp,
                        target: SubmissionTarget::OwnTurn {
                            turn_id: TurnId(format!("legacy-turn-{index}")),
                        },
                        execution_descriptor: descriptor,
                    },
                },
            });
        }
        for event in &events {
            db.append_event(event).await.unwrap();
        }
        let before = db.pending_submissions(None, 2).await.unwrap();
        downgrade_submission_fixture_to_schema_51(&db, &mut events).await;
        drop(db);
        let maintenance = crate::lease::acquire_session_maintenance_guard(root.path(), id).unwrap();
        let write =
            crate::lease::acquire_maintenance_session_write_lock(&maintenance, root.path(), id)
                .unwrap();
        let migrated =
            crate::db::SessionDb::migrate_turso_in_root(id, root.path(), &maintenance, &write)
                .await
                .unwrap();
        assert_eq!(migrated.pending_submissions(None, 2).await.unwrap(), before);
        assert_eq!(
            migrated.submission_queue_head().await.unwrap(),
            Some(before[0].clone())
        );
        assert!(
            migrated
                .model_context_events()
                .await
                .unwrap()
                .iter()
                .all(|e| !matches!(e.kind, SessionEventKind::UserMessage { .. }))
        );
    }

    async fn downgrade_submission_fixture_to_schema_51(
        db: &crate::db::SessionDb,
        events: &mut [SessionEvent],
    ) {
        use switchy::database::query::FilterableQuery as _;
        for event in events {
            event.schema_version = 51;
            let payload = crate::persisted::encode_session_event(event).unwrap();
            db.database()
                .update("events")
                .value("payload", payload)
                .where_eq("event_seq", i64::try_from(event.sequence).unwrap())
                .execute(db.database())
                .await
                .unwrap();
        }
        switchy::database::schema::drop_table("submission_recovery_operations")
            .execute(db.database())
            .await
            .unwrap();
        switchy::database::schema::drop_index("idx_submission_queue", "submissions")
            .execute(db.database())
            .await
            .unwrap();
        switchy::database::schema::alter_table("submissions")
            .drop_column("queue_visible".into())
            .execute(db.database())
            .await
            .unwrap();
        db.database()
            .delete("__bcode_session_migrations")
            .where_in(
                "id",
                vec![
                    "063_submission_queue_visible",
                    "064_submission_queue_index",
                    "065_submission_recovery_operations",
                ],
            )
            .execute(db.database())
            .await
            .unwrap();
        db.database()
            .update("session_storage_contract")
            .value("writer_epoch", 15_i64)
            .execute(db.database())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn recovery_projection_rebuild_and_crash_after_activation_preserve_authority() {
        use bcode_session_models::SessionId;
        let root = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let db = crate::db::SessionDb::initialize_turso_in_root(id, root.path())
            .await
            .unwrap();
        let prompt = PromptSubmission {
            id: SubmissionId("rebuild".into()),
            client_id: ClientId::new(),
            text: "exact input".into(),
            admission: TurnAdmissionMetadata::default(),
            requested_placement: SubmissionPlacement::FollowUp,
            target: SubmissionTarget::OwnTurn {
                turn_id: TurnId("interrupted".into()),
            },
            execution_descriptor: None,
        };
        let kinds = [
            SessionEventKind::SessionCreated {
                name: None,
                working_directory: root.path().into(),
            },
            SessionEventKind::SubmissionAccepted {
                submission: prompt.clone(),
            },
            SessionEventKind::SubmissionRecoveryAuthorized {
                submission_id: prompt.id.clone(),
                expected_accepted_sequence: 1,
                operation_id: "rebuild-resume".into(),
                descriptor: recovery_descriptor(),
            },
            SessionEventKind::SubmissionActivated {
                submission_id: prompt.id.clone(),
                turn_id: prompt.target.turn_id().clone(),
                text: prompt.text.clone(),
                client_id: prompt.client_id,
                admission: prompt.admission.clone(),
            },
        ];
        for (sequence, kind) in kinds.into_iter().enumerate() {
            let event = SessionEvent {
                schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                session_id: id,
                sequence: sequence as u64,
                timestamp_ms: sequence as u64 + 1,
                provenance: None,
                kind,
            };
            db.append_event(&event).await.unwrap();
        }
        let original = db
            .submission_recovery_operation("rebuild-resume")
            .await
            .unwrap();
        let activated = db.submission(&prompt.id).await.unwrap().unwrap();
        drop(db);
        let maintenance = crate::lease::acquire_session_maintenance_guard(root.path(), id).unwrap();
        let write =
            crate::lease::acquire_maintenance_session_write_lock(&maintenance, root.path(), id)
                .unwrap();
        let db = crate::db::SessionDb::open_existing_turso_in_root(id, root.path())
            .await
            .unwrap();
        assert_eq!(
            db.reindex_session_projections(&maintenance, &write)
                .await
                .unwrap(),
            4
        );
        assert_eq!(
            db.submission_recovery_operation("rebuild-resume")
                .await
                .unwrap(),
            original
        );
        assert_eq!(
            db.submission(&prompt.id).await.unwrap(),
            Some(activated.clone())
        );
        assert_eq!(db.submission_queue_head().await.unwrap(), Some(activated));
        assert!(db.pending_submissions(None, 1).await.unwrap().is_empty());
        assert_eq!(
            db.model_context_events()
                .await
                .unwrap()
                .iter()
                .filter(|e| matches!(e.kind, SessionEventKind::UserMessage { .. }))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn recovery_rejects_future_context_and_workflow_authority() {
        let manager = crate::SessionManager::default();
        let session = manager
            .create_session(None, "/workspace".into())
            .await
            .unwrap();
        let mut prompt = PromptSubmission {
            id: SubmissionId("workflow".into()),
            client_id: ClientId::new(),
            text: "protected".into(),
            admission: queued_metadata(),
            requested_placement: SubmissionPlacement::FollowUp,
            target: SubmissionTarget::OwnTurn {
                turn_id: TurnId("protected".into()),
            },
            execution_descriptor: None,
        };
        let seq = manager
            .accept_submission(session.id, prompt.clone())
            .await
            .unwrap()
            .receipt
            .accepted_event_sequence;
        assert!(
            manager
                .cancel_pending_submission(session.id, prompt.id.clone(), seq, "cancel".into())
                .await
                .is_err()
        );
        assert!(
            manager
                .authorize_submission_recovery(
                    session.id,
                    prompt.id.clone(),
                    seq,
                    "resume".into(),
                    recovery_descriptor()
                )
                .await
                .is_err()
        );
        assert!(
            manager
                .check_submission_recovery_retry(session.id, prompt.id, seq, "cancel".into(), None)
                .await
                .is_err()
        );
        prompt.id = SubmissionId("future".into());
        prompt.admission = TurnAdmissionMetadata::default();
        let mut d = recovery_descriptor();
        d.recovery_recipe
            .as_mut()
            .unwrap()
            .config_source
            .schema_version = 999;
        prompt.execution_descriptor = Some(d);
        assert!(manager.accept_submission(session.id, prompt).await.is_err());
    }

    fn descriptor() -> bcode_session_models::SubmissionExecutionDescriptor {
        bcode_session_models::SubmissionExecutionDescriptor {
            version: 1,
            provider: "provider".into(),
            model: "model".into(),
            reasoning: None,
            model_profile: Some("coding".into()),
            auth_profile: Some("work".into()),
            config_identity: "config-digest".into(),
            recovery_recipe: None,
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
        unsupported.execution_descriptor.as_mut().unwrap().version = 99;
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
