//! Serialized actor milestone operations.
use super::{SessionActor, SessionCommand, SessionError, SessionEventKind, SessionHandle};
use bcode_session_models::{
    SessionMilestoneEvaluationCompletion, SessionMilestoneEvaluationRequest, SessionMilestonePage,
    SessionMilestonePublicationRequest, SessionMilestoneQuery,
};

impl SessionHandle {
    /// Capture bounded request-only conversational input using the actor's retained connection.
    /// # Errors
    /// Returns invalid-budget, compatibility or storage errors.
    pub async fn milestone_context(
        &self,
        max_bytes: usize,
        max_turns: usize,
    ) -> Result<bcode_session_models::SessionMilestoneContext, SessionError> {
        self.send(|reply| SessionCommand::MilestoneContext {
            max_bytes,
            max_turns,
            reply,
        })
        .await?
    }
    /// Read exact durable evaluation status without write admission.
    /// # Errors
    /// Returns compatibility or storage errors.
    pub async fn milestone_evaluation_status(
        &self,
        evaluation_id: &str,
    ) -> Result<Option<bcode_session_models::SessionMilestoneEvaluationStatus>, SessionError> {
        self.send(|reply| SessionCommand::MilestoneEvaluationStatus {
            evaluation_id: evaluation_id.into(),
            reply,
        })
        .await?
    }
    /// Read latest durable scheduling checkpoint without write admission.
    /// # Errors
    /// Returns compatibility or storage errors.
    pub async fn milestone_evaluation_checkpoint(
        &self,
    ) -> Result<Option<bcode_session_models::SessionMilestoneEvaluationStatus>, SessionError> {
        self.send(|reply| SessionCommand::MilestoneEvaluationCheckpoint { reply })
            .await?
    }
    /// Admit a retry-safe evaluation without persisting generation input.
    /// # Errors
    /// Rejects stale generations, conflicting identities and invalid source boundaries.
    pub async fn request_milestone_evaluation(
        &self,
        request: SessionMilestoneEvaluationRequest,
    ) -> Result<bool, SessionError> {
        self.send(|reply| SessionCommand::RequestMilestoneEvaluation { request, reply })
            .await?
    }
    /// Publish an annotation and optional non-explicit display title.
    /// # Errors
    /// Rejects stale evaluations, conflicting retries and invalid generated content.
    pub async fn publish_milestone(
        &self,
        request: SessionMilestonePublicationRequest,
    ) -> Result<(), SessionError> {
        self.send(|reply| SessionCommand::PublishMilestone { request, reply })
            .await?
    }
    /// Record a successful evaluation, including no-milestone outcomes.
    /// # Errors
    /// Rejects unknown or superseded evaluations.
    pub async fn complete_milestone_evaluation(
        &self,
        completion: SessionMilestoneEvaluationCompletion,
    ) -> Result<(), SessionError> {
        self.send(|reply| SessionCommand::CompleteMilestoneEvaluation { completion, reply })
            .await?
    }
    /// List annotations using a bounded canonical publication cursor.
    /// # Errors
    /// Rejects invalid page sizes and unavailable current storage.
    pub async fn milestones(
        &self,
        query: SessionMilestoneQuery,
    ) -> Result<SessionMilestonePage, SessionError> {
        self.send(|reply| SessionCommand::Milestones { query, reply })
            .await?
    }
}

impl SessionActor {
    pub(super) async fn milestone_context(
        &mut self,
        max_bytes: usize,
        max_turns: usize,
    ) -> Result<bcode_session_models::SessionMilestoneContext, SessionError> {
        let id = self.state.summary.id;
        let db = self
            .existing_session_db()
            .await?
            .ok_or(SessionError::NotFound(id))?;
        db.validate_read_readiness().await?;
        Ok(db.milestone_context(max_bytes, max_turns).await?)
    }
    pub(super) async fn milestone_evaluation_status(
        &mut self,
        evaluation_id: &str,
    ) -> Result<Option<bcode_session_models::SessionMilestoneEvaluationStatus>, SessionError> {
        let id = self.state.summary.id;
        let db = self
            .existing_session_db()
            .await?
            .ok_or(SessionError::NotFound(id))?;
        Ok(db.milestone_evaluation_status(evaluation_id).await?)
    }
    pub(super) async fn milestone_evaluation_checkpoint(
        &mut self,
    ) -> Result<Option<bcode_session_models::SessionMilestoneEvaluationStatus>, SessionError> {
        let id = self.state.summary.id;
        let db = self
            .existing_session_db()
            .await?
            .ok_or(SessionError::NotFound(id))?;
        Ok(db.milestone_evaluation_checkpoint().await?)
    }
    pub(super) async fn request_milestone_evaluation(
        &mut self,
        request: SessionMilestoneEvaluationRequest,
    ) -> Result<bool, SessionError> {
        self.ensure_ownership()?;
        let db = self.session_db_for_write().await?;
        if let Some((_, existing, _)) = db.milestone_evaluation(&request.evaluation_id).await? {
            return if existing == request {
                Ok(false)
            } else {
                Err(SessionError::EventSerialization(
                    "conflicting milestone evaluation identity".into(),
                ))
            };
        }
        self.validate_append_generation(Some(request.expected_generation))?;
        if self.state.summary.explicit_name != request.expected_explicit_name {
            return Err(SessionError::EventSerialization(
                "explicit name changed before milestone admission".into(),
            ));
        }
        self.append_event(
            SessionEventKind::MilestoneEvaluationRequested { request },
            None,
            crate::current_unix_millis(),
        )
        .await?;
        Ok(true)
    }
    pub(super) async fn publish_milestone(
        &mut self,
        request: SessionMilestonePublicationRequest,
    ) -> Result<(), SessionError> {
        self.ensure_ownership()?;
        let db = self.session_db_for_write().await?;
        let id = &request.milestone.evaluation_id;
        let (admitted, evaluation, completed) =
            db.milestone_evaluation(id).await?.ok_or_else(|| {
                SessionError::EventSerialization("unknown milestone evaluation".into())
            })?;
        if let Some(existing) = db.milestone_publication(id).await? {
            if existing != request {
                return Err(SessionError::EventSerialization(
                    "conflicting milestone publication including title".into(),
                ));
            }
        } else {
            if completed {
                return Err(SessionError::EventSerialization(
                    "evaluation already completed".into(),
                ));
            }
            self.append_event(
                SessionEventKind::MilestonePublicationRequested {
                    request: request.clone(),
                },
                None,
                crate::current_unix_millis(),
            )
            .await?;
        }
        let db = self.session_db_for_write().await?;
        if let Some(existing) = db.milestone_by_evaluation(id).await? {
            if existing != request.milestone {
                return Err(SessionError::EventSerialization(
                    "conflicting milestone publication".into(),
                ));
            }
            if completed {
                return Ok(());
            }
        } else {
            if completed {
                return Err(SessionError::EventSerialization(
                    "evaluation already completed without milestone".into(),
                ));
            }
            self.append_event(
                SessionEventKind::MilestonePublished {
                    milestone: request.milestone.clone(),
                },
                None,
                crate::current_unix_millis(),
            )
            .await?;
        }
        if let Some(title) = request.automatic_title
            && self.state.summary.explicit_name.is_none()
            && evaluation.expected_explicit_name.is_none()
            && !self
                .session_db_for_write()
                .await?
                .milestone_title_already_published(admitted)
                .await?
            && self
                .session_db_for_write()
                .await?
                .milestone_title_fence(admitted)
                .await?
        {
            self.append_event(
                SessionEventKind::AutomaticTitleUpdated {
                    evaluation_id: id.clone(),
                    title,
                },
                None,
                crate::current_unix_millis(),
            )
            .await?;
        }
        self.complete_milestone_evaluation(SessionMilestoneEvaluationCompletion {
            evaluation_id: id.clone(),
        })
        .await
    }
    pub(super) async fn complete_milestone_evaluation(
        &mut self,
        completion: SessionMilestoneEvaluationCompletion,
    ) -> Result<(), SessionError> {
        self.ensure_ownership()?;
        let db = self.session_db_for_write().await?;
        if let Some((_, _, true)) = db.milestone_evaluation(&completion.evaluation_id).await? {
            return Ok(());
        }
        self.append_event(
            SessionEventKind::MilestoneEvaluationCompleted { completion },
            None,
            crate::current_unix_millis(),
        )
        .await?;
        Ok(())
    }
    pub(super) async fn milestones(
        &mut self,
        query: SessionMilestoneQuery,
    ) -> Result<SessionMilestonePage, SessionError> {
        let session_id = self.state.summary.id;
        let db = self
            .existing_session_db()
            .await?
            .ok_or(SessionError::NotFound(session_id))?;
        Ok(db.milestones(query).await?)
    }
}
