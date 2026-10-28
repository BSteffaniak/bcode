//! Caller-bound implementation of the portable session application contract.

use std::{path::PathBuf, sync::Arc};

use bcode_session_models::{
    ClientId, SessionId, SessionInspectionPage, SessionInspectionQuery, SessionSummary,
    TurnAdmission, TurnAdmissionMetadata, TurnPriority, TurnRejectionReason,
    application::{
        CreateSessionRequest, SessionApplication, SessionApplicationFailure,
        SessionWorkingDirectoryChange,
    },
};

use crate::{ServerState, session_operations};

/// Session application access bound by the owning host to a verified caller.
///
/// Consumers receive this handle through the owning application, never by constructing server
/// state or supplying an arbitrary caller identity. The borrow prevents this handle from
/// retaining the canonical host beyond the callback that supplied access.
pub struct SessionApplicationHandle<'a> {
    state: &'a Arc<ServerState>,
    client_id: ClientId,
}

impl<'a> SessionApplicationHandle<'a> {
    /// The host must verify caller authority and register its runtime context before binding.
    #[must_use]
    pub const fn new(state: &'a Arc<ServerState>, client_id: ClientId) -> Self {
        Self { state, client_id }
    }

    fn require_running(&self) -> Result<(), SessionApplicationFailure> {
        if self
            .state
            .shutdown_requested
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(failure(
                "session_application_stopped",
                "session application is shutting down",
            ));
        }
        Ok(())
    }

    async fn selected_location(
        &self,
        session_id: SessionId,
    ) -> Result<(), SessionApplicationFailure> {
        self.require_running()?;
        session_operations::require_selected_location(self.state, session_id).await?;
        if self
            .state
            .active_session_namespace_mismatch(session_id, self.client_id)
            .await
            .is_some()
        {
            return Err(failure(
                "session_incompatible_active_client",
                "session is active for an incompatible client; reconnect with a matching client or wait until the session is inactive",
            ));
        }
        self.require_running()
    }

    async fn require_draft_scope(
        &self,
        scope: &bcode_session_models::ComposerDraftScope,
    ) -> Result<(), SessionApplicationFailure> {
        self.require_running()?;
        if let bcode_session_models::ComposerDraftScope::Session { session_id } = scope {
            self.selected_location(*session_id).await?;
        }
        Ok(())
    }
}

impl bcode_session_models::SessionObservationApplication for SessionApplicationHandle<'_> {
    type Observation = super::SessionObservation;

    async fn observe_session(
        &self,
        request: bcode_session_models::SessionObservationRequest,
    ) -> Result<Self::Observation, bcode_session_models::SessionObservationError> {
        self.selected_location(request.session_id)
            .await
            .map_err(|_| bcode_session_models::SessionObservationError::Unavailable)?;
        if self
            .state
            .active_session_namespace_mismatch(request.session_id, self.client_id)
            .await
            .is_some()
        {
            return Err(bcode_session_models::SessionObservationError::Unavailable);
        }
        super::session_observation::observe_session(&self.state.sessions, request).await
    }
}

/// Opaque terminal observation for compaction admitted by the owning runtime.
/// Dropping this handle does not cancel execution.
pub struct ServerSessionCompactionOperation {
    completion: tokio::sync::oneshot::Receiver<Result<String, crate::CompactionError>>,
}

impl bcode_session_models::SessionCompactionOperation for ServerSessionCompactionOperation {
    async fn complete(
        self,
    ) -> Result<bcode_session_models::SessionCompactionResult, SessionApplicationFailure> {
        session_operations::complete_compaction(self.completion)
            .await
            .map(|result| match result {
                session_operations::CompactResult::Compacted(text) => {
                    bcode_session_models::SessionCompactionResult::Compacted(text)
                }
                session_operations::CompactResult::Noop(text) => {
                    bcode_session_models::SessionCompactionResult::Noop(text)
                }
            })
            .map_err(compaction_failure)
    }
}

/// Map known host-side compaction failures without exposing provider text or captured context.
fn compaction_provider_failure(message: &str) -> SessionApplicationFailure {
    let (code, detail) = if message
        == "compaction request settlement is unresolved; boundary unchanged"
    {
        (
            "compaction_request_unsettled",
            "compaction stopped because a reserved provider request has no verified settled outcome; accepted work is retained and the boundary is unchanged; automatic replay is blocked",
        )
    } else if message == "compaction provider request allowance exhausted; boundary unchanged" {
        (
            "compaction_request_budget_exhausted",
            "compaction stopped because its durable provider-request allowance is exhausted; accepted work and spent accounting are retained and the boundary is unchanged",
        )
    } else if message == "explicit compaction restart allowance exhausted; boundary unchanged" {
        (
            "compaction_restart_allowance_exhausted",
            "compaction stopped because its retained restart deadline or request allowance was exhausted; accepted work is retained and the boundary is unchanged",
        )
    } else if message == "explicit compaction restart deadline exhausted; boundary unchanged" {
        (
            "compaction_restart_deadline_exhausted",
            "compaction publication blocked: the retained restart deadline expired; accepted work is preserved and the context boundary is unchanged. Repeating the same request cannot extend that deadline",
        )
    } else if message == "compaction corrective deadline exhausted; boundary unchanged" {
        (
            "compaction_corrective_deadline_exhausted",
            "compaction stopped: the corrective dispatch deadline expired; accepted work is preserved and the context boundary is unchanged",
        )
    } else if message == "compaction corrective allowance exhausted; boundary unchanged" {
        (
            "compaction_corrective_allowance_exhausted",
            "compaction cannot dispatch another corrective request: its allowance expired, is exhausted, or has an unsettled request; accepted work is preserved and the context boundary is unchanged",
        )
    } else if message.starts_with("compaction capacity policy rejected:")
        || message.starts_with("compaction summary policy:")
        || message.starts_with("compaction budget")
    {
        (
            "compaction_capacity",
            "compaction cannot fit the selected model capacity or required reclaim; accepted progress and boundary are unchanged",
        )
    } else if message.starts_with("compaction synthesis rejected after bounded repair;") {
        (
            "compaction_synthesis_rejected",
            "compaction synthesis exhausted its bounded repair allowance; inspect session compaction-status for progress and the recorded rejection reason",
        )
    } else if message.starts_with("compaction request/repair budget exhausted") {
        (
            "compaction_allowance_exhausted",
            "compaction attempt allowance is exhausted; retained progress requires explicit recovery rather than another identical retry",
        )
    } else if message.starts_with("compaction synthesis policy changed")
        || message.starts_with("invalid or incompatible compaction checkpoint")
        || message.starts_with("compaction consolidation outcome is unresolved")
    {
        (
            "compaction_recovery_required",
            "retained compaction state is incompatible or unresolved; preserve it and use explicit bounded recovery",
        )
    } else {
        (
            "provider_error",
            "model provider failed during context compaction",
        )
    };
    failure(code, detail)
}

fn compaction_failure(error: session_operations::CompactError) -> SessionApplicationFailure {
    use crate::CompactionError;
    match error {
        session_operations::CompactError::IncompatibleActiveNamespace(namespace) => {
            let error =
                session_operations::ReadHistoryError::IncompatibleActiveNamespace(namespace);
            failure(error.code(), error.message())
        }
        session_operations::CompactError::Server(error) => server_failure(&error),
        session_operations::CompactError::Compaction(error) => match error {
            CompactionError::Session(error) => {
                let error = crate::session_error_response(&error);
                failure(&error.code, &error.message)
            }
            CompactionError::ProviderUnavailable => {
                failure("provider_unavailable", "model provider unavailable")
            }
            CompactionError::Cancelled => failure("cancelled", "context compaction cancelled"),
            CompactionError::UncompactableCurrentTurn { .. } => failure(
                "uncompactable_current_turn",
                "current turn cannot fit the model input capacity and cannot be partially compacted",
            ),
            CompactionError::RequestStillTooLarge { .. } => failure(
                "request_still_too_large",
                "request remains too large after context compaction",
            ),
            CompactionError::RecoveryBlocked(blocker) => {
                use bcode_session_models::SessionCompactionRecoveryBlocker as Blocker;
                let (code, detail) = match blocker {
                    Blocker::UnsettledRequests => (
                        "compaction_recovery_request_unsettled",
                        "compaction recovery cannot verify a settled outcome for every reserved provider request; automatic replay is blocked; accepted work is retained and the context boundary is unchanged",
                    ),
                    Blocker::EvidenceUnavailable => (
                        "compaction_recovery_evidence_incomplete",
                        "compaction recovery evidence is missing or exceeds this bounded inspection; accepted work is retained and no provider request was dispatched by recovery",
                    ),
                    Blocker::CheckpointChanged => (
                        "compaction_recovery_checkpoint_changed",
                        "the selected compaction checkpoint changed during assessment; no recovery was published; refresh status before retrying",
                    ),
                    Blocker::AlreadySuperseded => (
                        "compaction_recovery_operation_superseded",
                        "the selected compaction operation was superseded; recovery must resolve its canonical successor before proceeding",
                    ),
                    Blocker::UnsupportedCheckpoint => (
                        "compaction_recovery_format_unsupported",
                        "the saved compaction checkpoint could not be validated for conversion; accepted work is retained and no recovery request was dispatched",
                    ),
                    Blocker::UnsupportedFailure => (
                        "compaction_recovery_failure_unproven",
                        "canonical evidence does not prove a supported settled compaction failure; automatic recovery did not dispatch provider work; accepted work is retained",
                    ),
                };
                failure(code, detail)
            }
            CompactionError::RecoveryRequired(reason) => {
                use crate::context_compaction::CompactionRecoveryReason;
                let detail = match reason {
                    CompactionRecoveryReason::InvalidCheckpoint => {
                        "retained compaction checkpoint is invalid or incompatible; preserve it and prepare explicit bounded recovery"
                    }
                    CompactionRecoveryReason::PolicyChanged => {
                        "retained compaction policy is incompatible with this operation; preserve it and prepare explicit bounded recovery"
                    }
                    CompactionRecoveryReason::ConsolidationUnresolved => {
                        "retained consolidation outcome is unresolved; preserve it and prepare explicit bounded recovery"
                    }
                };
                failure("compaction_recovery_required", detail)
            }
            CompactionError::Provider(message) => compaction_provider_failure(&message),
            CompactionError::PlanUnavailable(_) | CompactionError::InsufficientProgress { .. } => {
                failure(
                    "session_compaction_failed",
                    "compaction terminal observation was inconsistent",
                )
            }
        },
    }
}

impl bcode_session_models::SessionCompactionRecoveryDiscoveryApplication
    for SessionApplicationHandle<'_>
{
    async fn prepare_session_compaction_recovery(
        &self,
        request: bcode_session_models::SessionCompactionRecoveryDiscoveryRequest,
    ) -> Result<bcode_session_models::SessionCompactionRecoveryPreparation, SessionApplicationFailure>
    {
        self.selected_location(request.session_id).await?;
        if !matches!(request.version, 1 | 2)
            || request.operation_id.is_nil()
            || !request.budget.is_valid()
        {
            return Err(failure(
                "invalid_compaction_restart",
                "invalid recovery preparation request",
            ));
        }
        if self
            .state
            .active_session_namespace_mismatch(request.session_id, self.client_id)
            .await
            .is_some()
        {
            return Err(failure(
                "incompatible_session_namespace",
                "session belongs to another active execution namespace",
            ));
        }
        let selection = super::session_model_selection_with_runtime_context(
            self.state,
            request.session_id,
            self.state.client_runtime_context(self.client_id).await,
        )
        .await;
        super::context_compaction::prepare_compaction_recovery(self.state, &selection, &request)
            .await
            .map_err(|error| {
                compaction_failure(session_operations::CompactError::Compaction(error))
            })
    }
}

impl bcode_session_models::SessionCompactionRestartApplication for SessionApplicationHandle<'_> {
    type CompactionOperation = ServerSessionCompactionOperation;

    async fn begin_session_compaction_restart(
        &self,
        request: bcode_session_models::SessionCompactionRestartRequest,
    ) -> Result<Self::CompactionOperation, SessionApplicationFailure> {
        self.selected_location(request.session_id).await?;
        request.validate().map_err(|_| {
            failure(
                "invalid_compaction_restart",
                "invalid compaction restart authorization",
            )
        })?;
        let completion =
            session_operations::restart_compaction(self.state, self.client_id, request)
                .await
                .map_err(compaction_failure)?;
        Ok(ServerSessionCompactionOperation { completion })
    }
}

impl SessionApplication for SessionApplicationHandle<'_> {
    type CompactionOperation = ServerSessionCompactionOperation;

    async fn begin_session_compaction(
        &self,
        session_id: SessionId,
    ) -> Result<Self::CompactionOperation, SessionApplicationFailure> {
        self.selected_location(session_id).await?;
        let completion = session_operations::compact(self.state, self.client_id, session_id)
            .await
            .map_err(compaction_failure)?;
        Ok(ServerSessionCompactionOperation { completion })
    }

    async fn begin_session_summary_rebuild(
        &self,
        request: bcode_session_models::SessionSummaryRebuildRequest,
    ) -> Result<Self::CompactionOperation, SessionApplicationFailure> {
        self.selected_location(request.session_id).await?;
        if !request.budget.is_valid() {
            return Err(failure(
                "invalid_summary_rebuild_budget",
                "invalid summary reconstruction allowance",
            ));
        }
        if self
            .state
            .active_session_namespace_mismatch(request.session_id, self.client_id)
            .await
            .is_some()
        {
            return Err(failure(
                "incompatible_session_namespace",
                "session belongs to another active execution namespace",
            ));
        }
        let selection = super::session_model_selection_with_runtime_context(
            self.state,
            request.session_id,
            self.state.client_runtime_context(self.client_id).await,
        )
        .await;
        let completion =
            super::enqueue_rebuild_summary_command(self.state, self.client_id, selection, request)
                .await
                .map_err(|_| {
                    failure(
                        "summary_rebuild_admission_failed",
                        "summary reconstruction could not be queued",
                    )
                })?;
        Ok(ServerSessionCompactionOperation { completion })
    }

    async fn session_catalog_snapshot(
        &self,
        working_directory: PathBuf,
    ) -> Result<bcode_session_models::SessionCatalogSnapshot, SessionApplicationFailure> {
        self.require_running()?;
        if !working_directory.is_absolute() {
            return Err(failure(
                "invalid_working_directory",
                "catalog working directory must be absolute",
            ));
        }
        let snapshot = session_operations::list(self.state, &working_directory)
            .await
            .map_err(|_| {
                failure(
                    "session_catalog_unavailable",
                    "session catalog is unavailable",
                )
            })?;
        Ok(bcode_session_models::SessionCatalogSnapshot {
            sessions: snapshot.sessions,
            catalog_status: snapshot.status,
            catalog_sources: snapshot.sources,
            catalog_revision: snapshot.revision,
        })
    }

    async fn refresh_session_catalog(
        &self,
        working_directory: PathBuf,
        sources: Option<Vec<String>>,
    ) -> Result<bcode_session_models::SessionCatalogSnapshot, SessionApplicationFailure> {
        self.require_running()?;
        if !working_directory.is_absolute() {
            return Err(failure(
                "invalid_working_directory",
                "catalog working directory must be absolute",
            ));
        }
        let snapshot =
            session_operations::refresh(self.state, &working_directory, sources.as_deref()).await;
        Ok(bcode_session_models::SessionCatalogSnapshot {
            sessions: snapshot.sessions,
            catalog_status: snapshot.status,
            catalog_sources: snapshot.sources,
            catalog_revision: snapshot.revision,
        })
    }
    async fn export_session_history(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<bcode_session_models::SessionEvent>, SessionApplicationFailure> {
        self.selected_location(session_id).await?;
        session_operations::complete_history(self.state, self.client_id, session_id)
            .await
            .map_err(history_failure)
    }

    async fn spawn_session(
        &self,
        request: bcode_session_models::SessionSpawnRequest,
    ) -> bcode_session_models::SessionSpawnResult {
        if let Err(error) = self.selected_location(request.creation.parent_id).await {
            return bcode_session_models::SessionSpawnResult {
                session: None,
                admission: None,
                failure: Some(bcode_session_models::SessionSpawnFailure {
                    stage: bcode_session_models::SessionSpawnStage::Preparation,
                    code: error.code,
                    message: error.message,
                }),
            };
        }
        Box::pin(super::session_spawning::spawn(
            self.state,
            self.client_id,
            request,
        ))
        .await
    }

    async fn reprice_session(
        &self,
        session_id: SessionId,
        range: bcode_session_models::SessionCostRange,
        catalog: bcode_model_catalog_models::CatalogDocument,
    ) -> Result<bcode_session_models::SessionRepriceReport, SessionApplicationFailure> {
        self.selected_location(session_id).await?;
        session_operations::reprice(self.state, session_id, range, catalog).await.map_err(|_| failure("session_reprice_failed", "session repricing failed: check range, catalog contract and canonical storage ownership"))
    }

    async fn session_categorization(
        &self,
        session_id: SessionId,
    ) -> Result<Option<bcode_session_models::SessionCategorization>, SessionApplicationFailure>
    {
        self.selected_location(session_id).await?;
        Ok(self
            .state
            .advisory_categorizations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&session_id)
            .cloned())
    }

    async fn usage_catalog_page(
        &self,
        after: Option<SessionId>,
    ) -> Result<Vec<SessionId>, SessionApplicationFailure> {
        self.require_running()?;
        self.state
            .sessions
            .usage_catalog_page(after)
            .await
            .map_err(|_| {
                failure(
                    "usage_catalog_unavailable",
                    "native catalog is incomplete; retry after discovery finishes",
                )
            })
    }

    async fn collect_usage(
        &self,
        session_id: SessionId,
        query: bcode_session_models::SessionUsageQuery,
    ) -> Result<bcode_session_models::SessionUsagePage, SessionApplicationFailure> {
        self.selected_location(session_id).await?;
        session_operations::collect_usage(self.state, session_id, query)
            .await
            .map_err(|message| failure("usage_unavailable", message))
    }

    async fn prepare_context_generation(
        &self,
        request: bcode_session_models::PrepareContextGeneration,
    ) -> Result<bcode_session_models::PreparedContextGeneration, SessionApplicationFailure> {
        self.selected_location(request.source_session_id).await?;
        super::context_generation::prepare(self.state, request)
            .await
            .map_err(|message| failure("generation_context_unavailable", message))
    }

    async fn session_trace_blob_range(
        &self,
        request: bcode_session_models::ReadSessionTraceBlobRequest,
    ) -> Result<bcode_session_models::SessionTraceBlobRange, SessionApplicationFailure> {
        self.selected_location(request.session_id).await?;
        super::trace_operations::read_range(self.state, request).await.map_err(|error| failure(error.code(), "session trace range unavailable: inspect the request contract and verified canonical storage"))
    }

    async fn session_projection_history_page(
        &self,
        session_id: SessionId,
        request: bcode_session_models::ProjectionWindowRequest,
    ) -> Result<bcode_session_models::ProjectionHistoryPage, SessionApplicationFailure> {
        self.selected_location(session_id).await?;
        session_operations::projection_history_page(self.state, self.client_id, session_id, request)
            .await
            .map_err(history_failure)
    }

    async fn submit_prompt(
        &self,
        session_id: SessionId,
        submission_id: bcode_session_models::SubmissionId,
        text: String,
        placement: bcode_session_models::SubmissionPlacement,
        require_active_turn: bool,
        admission: TurnAdmissionMetadata,
    ) -> Result<bcode_session_models::SubmissionReceipt, SessionApplicationFailure> {
        self.selected_location(session_id).await?;
        super::enqueue_prompt_submission(
            self.state,
            session_id,
            self.client_id,
            submission_id,
            text,
            placement,
            require_active_turn,
            admission,
        )
        .await
        .map_err(|error| match error {
            crate::ServerError::SteeringUnavailable => failure(
                "steering_unavailable",
                "compatible active turn unavailable for steering",
            ),
            error => server_failure(&error),
        })
    }

    async fn pending_submissions(
        &self,
        session_id: SessionId,
        after: Option<u64>,
        limit: u32,
    ) -> Result<bcode_session_models::PendingSubmissionsPage, SessionApplicationFailure> {
        self.selected_location(session_id).await?;
        if !(1..=128).contains(&limit) {
            return Err(failure(
                "invalid_request",
                "pending submissions limit must be between 1 and 128",
            ));
        }
        super::pending_submissions_page(self.state, session_id, after, limit)
            .await
            .map_err(|error| session_failure(&error))
    }

    async fn get_submission(
        &self,
        session_id: SessionId,
        submission_id: bcode_session_models::SubmissionId,
    ) -> Result<Option<bcode_session_models::SubmissionRecord>, SessionApplicationFailure> {
        self.selected_location(session_id).await?;
        self.state
            .sessions
            .submission(session_id, submission_id)
            .await
            .map_err(|error| session_failure(&error))
    }

    async fn generate_session_milestone(
        &self,
        session_id: SessionId,
    ) -> Result<
        bcode_session_models::application::SessionMilestoneGenerationOutcome,
        SessionApplicationFailure,
    > {
        self.selected_location(session_id).await?;
        Box::pin(super::handle_explicit_session_milestone(
            self.state, session_id,
        ))
        .await
        .map(
            |result| bcode_session_models::application::SessionMilestoneGenerationOutcome {
                success: result.success,
                text: result.text,
            },
        )
        .map_err(|_| {
            failure(
                "milestone_generation_failed",
                "milestone generation unavailable; check enabled policy and model selection",
            )
        })
    }

    async fn send_message(
        &self,
        session_id: SessionId,
        text: String,
        placement: bcode_session_models::PromptPlacement,
        execution: bcode_session_models::TurnExecutionOptions,
    ) -> Result<bcode_session_models::MessageAcceptance, SessionApplicationFailure> {
        self.selected_location(session_id).await?;
        let status = super::enqueue_user_message_command(
            self.state,
            session_id,
            self.client_id,
            self.state.client_runtime_context(self.client_id).await,
            text,
            placement,
            execution,
        )
        .await
        .map_err(|error| server_failure(&error))?;
        Ok(bcode_session_models::MessageAcceptance {
            queued: status.queued,
            queue_position: status.queue_position,
            disposition: status.disposition,
        })
    }

    async fn create_child_session(
        &self,
        request: bcode_session_models::SessionChildCreateRequest,
    ) -> Result<SessionSummary, SessionApplicationFailure> {
        self.selected_location(request.parent_id).await?;
        session_operations::create_child(self.state, request)
            .await
            .map_err(|error| failure(error.code(), error.message()))
    }

    async fn append_presentation_note(
        &self,
        request: bcode_session_models::application::SessionPresentationNoteRequest,
    ) -> Result<(), SessionApplicationFailure> {
        self.selected_location(request.session_id).await?;
        let format = match request.format {
            bcode_session_models::application::SessionPresentationNoteFormat::PlainText => {
                bcode_command::CommandTextFormat::PlainText
            }
            bcode_session_models::application::SessionPresentationNoteFormat::Markdown => {
                bcode_command::CommandTextFormat::Markdown
            }
            bcode_session_models::application::SessionPresentationNoteFormat::Json => {
                bcode_command::CommandTextFormat::Json
            }
        };
        session_operations::append_presentation_note(
            self.state,
            request.session_id,
            request.source_id,
            request.note_id,
            request.text,
            format,
        )
        .await
        .map_err(|error| match error {
            session_operations::AppendPresentationNoteError::Invalid => failure(
                "invalid_presentation_note",
                "presentation note fields are empty or exceed their bounded limits",
            ),
            session_operations::AppendPresentationNoteError::Session(error) => {
                session_failure(&error)
            }
        })
    }

    async fn compact_session(
        &self,
        session_id: SessionId,
    ) -> Result<bcode_session_models::application::SessionCompactionResult, SessionApplicationFailure>
    {
        use bcode_session_models::SessionCompactionOperation as _;
        self.begin_session_compaction(session_id)
            .await?
            .complete()
            .await
    }

    async fn query_child_sessions(
        &self,
        query: bcode_session_models::SessionChildQuery,
    ) -> Result<bcode_session_models::SessionChildPage, SessionApplicationFailure> {
        self.selected_location(query.parent_id).await?;
        self.state
            .sessions
            .query_child_sessions(query)
            .await
            .map_err(|error| session_failure(&error))
    }

    async fn session_milestones(
        &self,
        session_id: SessionId,
        query: bcode_session_models::SessionMilestoneQuery,
    ) -> Result<bcode_session_models::SessionMilestonePage, SessionApplicationFailure> {
        self.selected_location(session_id).await?;
        self.state
            .sessions
            .session_milestones(session_id, query)
            .await
            .map_err(|error| session_failure(&error))
    }

    async fn session_artifact_range(
        &self,
        session_id: SessionId,
        artifact_id: String,
        reference_key: String,
        offset: u64,
        length: u32,
    ) -> Result<bcode_session_models::SessionArtifactRange, SessionApplicationFailure> {
        self.selected_location(session_id).await?;
        super::artifact_operations::read_range(
            self.state,
            session_id,
            &artifact_id,
            &reference_key,
            offset,
            length,
        )
        .await
        .map_err(|error| failure(error.code(), &error.message()))
    }

    async fn session_working_document(
        &self,
        request: bcode_session_models::SessionWorkingDocumentRequest,
    ) -> Result<Option<bcode_session_models::SessionWorkingDocument>, SessionApplicationFailure>
    {
        self.selected_location(request.session_id).await?;
        session_operations::working_document(self.state, request).await
    }

    async fn session_admission(
        &self,
        session_id: SessionId,
        apply: bool,
    ) -> Result<bcode_session_models::StorageAdmissionReport, SessionApplicationFailure> {
        self.selected_location(session_id).await?;
        super::session_compression::admission_report(self.state, session_id, apply)
            .await
            .map_err(|message| failure("session_admission_unavailable", message))
    }

    async fn compress_session_page(
        &self,
        request: bcode_session_models::StorageCompressionRequest,
    ) -> Result<bcode_session_models::StorageCompressionResult, SessionApplicationFailure> {
        self.selected_location(request.session_id).await?;
        super::session_compression::compress_page(self.state, request)
            .await
            .map_err(|message| failure("session_compression_unavailable", message))
    }

    async fn invoke_skill(
        &self,
        session_id: SessionId,
        skill_id: bcode_skill_models::SkillId,
        arguments: String,
        display_text: String,
        execution: bcode_session_models::TurnExecutionOptions,
    ) -> Result<bcode_session_models::MessageAcceptance, SessionApplicationFailure> {
        self.selected_location(session_id).await?;
        let status = session_operations::invoke_skill(
            self.state,
            self.client_id,
            session_id,
            skill_id,
            arguments,
            display_text,
            execution,
        )
        .await
        .map_err(|error| match error {
            session_operations::InvokeSkillError::Disabled => {
                failure("skills_disabled", "skills are disabled")
            }
            session_operations::InvokeSkillError::Unknown => {
                failure("unknown_skill", "skill is unavailable")
            }
            session_operations::InvokeSkillError::Invalid(error) => session_failure(&error),
            session_operations::InvokeSkillError::Ownership(error)
            | session_operations::InvokeSkillError::Admission(error) => server_failure(&error),
        })?;
        Ok(bcode_session_models::MessageAcceptance {
            queued: status.queued,
            queue_position: status.queue_position,
            disposition: status.disposition,
        })
    }

    async fn session_catalog_page(
        &self,
        scope: bcode_session_models::SessionDiscoveryScope,
        query: bcode_session_models::SessionCatalogQuery,
        cursor: Option<bcode_session_models::SessionCatalogCursor>,
        limit: usize,
    ) -> Result<bcode_session_models::SessionCatalogPage, SessionApplicationFailure> {
        self.require_running()?;
        self.state
            .session_catalog
            .page_with_scope(self.state, scope, query, cursor, limit)
            .await
            .map_err(|_| {
                failure(
                    "session_catalog_page_rejected",
                    "session catalog query or generation is unavailable",
                )
            })
    }

    async fn session_summary(
        &self,
        session_id: SessionId,
    ) -> Result<SessionSummary, SessionApplicationFailure> {
        self.selected_location(session_id).await?;
        self.state
            .sessions
            .session_summary_non_owning(session_id)
            .await
            .map_err(|error| session_failure(&error))
    }

    async fn delete_session(
        &self,
        session_id: SessionId,
    ) -> Result<SessionSummary, SessionApplicationFailure> {
        self.selected_location(session_id).await?;
        session_operations::delete(self.state, session_id)
            .await
            .map_err(|error| match error {
                session_operations::DeleteSessionError::Session(error) => session_failure(&error),
                error @ session_operations::DeleteSessionError::Busy(_) => {
                    failure(error.code(), error.message())
                }
            })
    }

    async fn session_history_page(
        &self,
        session_id: SessionId,
        query: bcode_session_models::SessionHistoryQuery,
    ) -> Result<bcode_session_models::SessionHistoryPage, SessionApplicationFailure> {
        self.selected_location(session_id).await?;
        session_operations::history_page(self.state, self.client_id, session_id, query)
            .await
            .map_err(history_failure)
    }

    async fn session_history_around(
        &self,
        session_id: SessionId,
        query: bcode_session_models::SessionHistoryAroundQuery,
    ) -> Result<bcode_session_models::SessionHistoryWindow, SessionApplicationFailure> {
        self.selected_location(session_id).await?;
        session_operations::history_around(self.state, self.client_id, session_id, query)
            .await
            .map_err(history_failure)
    }

    async fn session_usage(
        &self,
        session_id: SessionId,
        query: bcode_session_models::SessionUsageQuery,
    ) -> Result<bcode_session_models::SessionUsagePage, SessionApplicationFailure> {
        self.selected_location(session_id).await?;
        session_operations::session_usage(self.state, session_id, query)
            .await
            .map_err(|message| failure("session_usage_unavailable", message))
    }

    async fn session_storage_usage(
        &self,
        session_id: SessionId,
        entry_budget: u32,
    ) -> Result<bcode_session_models::SessionStorageUsage, SessionApplicationFailure> {
        self.selected_location(session_id).await?;
        session_operations::storage_usage(self.state, session_id, entry_budget)
            .await
            .map_err(|message| failure("session_storage_usage_unavailable", message))
    }

    async fn composer_draft(
        &self,
        scope: bcode_session_models::ComposerDraftScope,
    ) -> Result<Option<String>, SessionApplicationFailure> {
        self.require_draft_scope(&scope).await?;
        session_operations::composer_draft(self.state, scope)
            .await
            .map_err(|error| session_failure(&error))
    }

    async fn set_composer_draft(
        &self,
        scope: bcode_session_models::ComposerDraftScope,
        text: String,
    ) -> Result<(), SessionApplicationFailure> {
        self.require_draft_scope(&scope).await?;
        session_operations::set_composer_draft(self.state, scope, text)
            .await
            .map_err(|error| session_failure(&error))
    }

    async fn set_session_reasoning(
        &self,
        session_id: SessionId,
        effort: Option<String>,
        summary: Option<String>,
    ) -> Result<(), SessionApplicationFailure> {
        self.selected_location(session_id).await?;
        session_operations::set_reasoning(self.state, session_id, effort, summary)
            .await
            .map(|_| ())
            .map_err(|error| session_failure(&error))
    }

    async fn activate_skill(
        &self,
        session_id: SessionId,
        skill_id: bcode_skill_models::SkillId,
    ) -> Result<(), SessionApplicationFailure> {
        self.selected_location(session_id).await?;
        session_operations::activate_skill(self.state, session_id, skill_id)
            .await
            .map_err(|error| match error {
                session_operations::ActivateSkillError::Session(error) => session_failure(&error),
                error => failure(error.code(), error.message()),
            })
    }

    async fn deactivate_skill(
        &self,
        session_id: SessionId,
        skill_id: bcode_skill_models::SkillId,
    ) -> Result<(), SessionApplicationFailure> {
        self.selected_location(session_id).await?;
        session_operations::deactivate_skill(self.state, session_id, skill_id)
            .await
            .map_err(|error| server_failure(&error))
    }

    async fn active_skills(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<bcode_skill_models::SkillContextResponse>, SessionApplicationFailure> {
        self.selected_location(session_id).await?;
        Ok(session_operations::active_skills(self.state, session_id).await)
    }

    async fn session_derivation_snapshot(
        &self,
        session_id: SessionId,
    ) -> Result<bcode_session_models::SessionDerivationSourceSnapshot, SessionApplicationFailure>
    {
        self.selected_location(session_id).await?;
        session_operations::derivation_snapshot(self.state, session_id)
            .await
            .map_err(|error| session_failure(&error))
    }

    async fn session_derivation_prompts(
        &self,
        session_id: SessionId,
        query: bcode_session_models::SessionDerivationPromptQuery,
    ) -> Result<bcode_session_models::SessionDerivationPromptPage, SessionApplicationFailure> {
        self.selected_location(session_id).await?;
        session_operations::derivation_prompts(self.state, session_id, query)
            .await
            .map_err(|error| session_failure(&error))
    }

    async fn derive_session(
        &self,
        request: bcode_session_models::SessionDerivationRequest,
    ) -> Result<bcode_session_models::SessionDerivationTerminalOutcome, SessionApplicationFailure>
    {
        self.selected_location(request.source.session_id).await?;
        session_operations::derive(self.state, request)
            .await
            .map_err(|error| session_failure(&error))
    }

    async fn session_derivation_status(
        &self,
        operation_id: bcode_session_models::SessionDerivationOperationId,
    ) -> Result<bcode_session_models::SessionDerivationOperationSnapshot, SessionApplicationFailure>
    {
        self.require_running()?;
        session_operations::derivation_status(self.state, operation_id)
            .await
            .map_err(|error| session_failure(&error))
    }

    async fn cancel_session_derivation(
        &self,
        operation_id: bcode_session_models::SessionDerivationOperationId,
    ) -> Result<bool, SessionApplicationFailure> {
        self.require_running()?;
        Ok(session_operations::cancel_derivation(self.state, operation_id).await)
    }

    async fn session_activity(
        &self,
        session_id: SessionId,
        query: bcode_session_models::SessionActivityQuery,
    ) -> Result<bcode_session_models::SessionActivityPage, SessionApplicationFailure> {
        self.selected_location(session_id).await?;
        session_operations::activity(self.state, session_id, query)
            .await
            .map_err(|error| session_failure(&error))
    }

    async fn create_session(
        &self,
        request: CreateSessionRequest,
    ) -> Result<SessionSummary, SessionApplicationFailure> {
        self.require_running()?;
        let session =
            session_operations::create(self.state, request.name, request.working_directory)
                .await
                .map_err(|error| failure(error.code(), error.message()))?;
        let runtime = self.state.client_runtime_context(self.client_id).await;
        self.state
            .set_session_config_from_runtime_context(session.id, runtime.as_ref())
            .await;
        crate::account_routing::initialize(self.state, session.id)
            .await
            .map_err(|message| failure("account_routing_unavailable", &message))?;
        Ok(session)
    }

    async fn inspect_compaction(
        &self,
        session_id: SessionId,
        query: bcode_session_models::SessionCompactionInspectionQuery,
    ) -> Result<bcode_session_models::SessionCompactionInspectionPage, SessionApplicationFailure>
    {
        self.selected_location(session_id).await?;
        session_operations::inspect_compaction(self.state, self.client_id, session_id, query)
            .await
            .map_err(|error| match error {
                session_operations::ReadHistoryError::Session(error) => session_failure(&error),
                error @ session_operations::ReadHistoryError::IncompatibleActiveNamespace(_) => {
                    failure(error.code(), error.message())
                }
            })
    }

    async fn inspect_session(
        &self,
        session_id: SessionId,
        query: SessionInspectionQuery,
    ) -> Result<SessionInspectionPage, SessionApplicationFailure> {
        self.selected_location(session_id).await?;
        session_operations::inspect(self.state, self.client_id, session_id, query)
            .await
            .map_err(|error| match error {
                session_operations::ReadHistoryError::Session(error) => session_failure(&error),
                error @ session_operations::ReadHistoryError::IncompatibleActiveNamespace(_) => {
                    failure(error.code(), error.message())
                }
            })
    }

    async fn rename_session(
        &self,
        session_id: SessionId,
        name: Option<String>,
    ) -> Result<SessionSummary, SessionApplicationFailure> {
        self.selected_location(session_id).await?;
        session_operations::rename(self.state, session_id, name)
            .await
            .map_err(|_| failure("session_rename_failed", "session rename failed"))
    }

    async fn change_session_working_directory(
        &self,
        session_id: SessionId,
        working_directory: PathBuf,
    ) -> Result<SessionWorkingDirectoryChange, SessionApplicationFailure> {
        self.selected_location(session_id).await?;
        let (session, changed) =
            session_operations::change_working_directory(self.state, session_id, working_directory)
                .await
                .map_err(|error| failure(error.code(), error.message()))?;
        Ok(SessionWorkingDirectoryChange { session, changed })
    }

    async fn submit_turn(
        &self,
        session_id: SessionId,
        text: String,
        admission: TurnAdmissionMetadata,
    ) -> Result<TurnAdmission, SessionApplicationFailure> {
        self.selected_location(session_id).await?;
        if self
            .state
            .active_session_namespace_mismatch(session_id, self.client_id)
            .await
            .is_some()
        {
            return Err(failure(
                "session_incompatible_active_client",
                "session is active for an incompatible client; reconnect with a matching client or wait until the session is inactive",
            ));
        }
        self.require_running()?;
        if admission.priority != TurnPriority::Interactive {
            return Ok(TurnAdmission::Rejected(
                TurnRejectionReason::ExecutionPolicy,
            ));
        }
        // The operation owns admission locking. Retain any untransferred lease until the
        // operation settles; accepted queue commands take it into the existing scheduler.
        let mut ownership = None;
        let result = Box::pin(session_operations::submit_turn(
            self.state,
            session_id,
            self.client_id,
            text,
            admission,
            &mut ownership,
        ))
        .await;
        drop(ownership);
        result.map_err(|error| match error {
            session_operations::SubmitTurnError::Admission(error)
            | session_operations::SubmitTurnError::Dispatch(crate::ServerError::Session(error)) => {
                session_failure(&error)
            }
            session_operations::SubmitTurnError::Dispatch(_) => failure(
                "session_turn_dispatch_failed",
                "turn dispatch failed; admission may already be durable; inspect the exact turn before retrying",
            ),
        })
    }

    async fn set_session_model(
        &self,
        session_id: SessionId,
        provider_plugin_id: Option<String>,
        model_id: String,
    ) -> Result<(), SessionApplicationFailure> {
        self.selected_location(session_id).await?;
        session_operations::set_model(self.state, session_id, provider_plugin_id, model_id)
            .await
            .map_err(|error| match error {
                session_operations::SetModelError::SkillRequiredModelActive(_) => failure(
                    "skill_required_model_active",
                    "an active skill declares a required model; deactivate it before changing the session model",
                ),
                session_operations::SetModelError::Session(error) => session_failure(&error),
            })
    }

    async fn set_session_agent(
        &self,
        session_id: SessionId,
        agent_id: String,
    ) -> Result<(), SessionApplicationFailure> {
        self.selected_location(session_id).await?;
        session_operations::set_agent(self.state, session_id, agent_id)
            .await
            .map_err(|error| match error {
                session_operations::SetAgentError::UnknownAgent(_) => {
                    failure("unknown_agent", "unknown prompt profile")
                }
                session_operations::SetAgentError::Session(error) => session_failure(&error),
            })
    }

    async fn cancel_session_turn(
        &self,
        session_id: SessionId,
        clear_queue: bool,
        expected_turn_id: Option<String>,
    ) -> Result<bool, SessionApplicationFailure> {
        self.selected_location(session_id).await?;
        session_operations::cancel_turn(
            self.state,
            session_id,
            clear_queue,
            self.client_id,
            expected_turn_id,
        )
        .await
        .map_err(|error| match error {
            crate::ServerError::Session(error) => session_failure(&error),
            _ => failure(
                "session_turn_cancel_failed",
                "turn cancellation request failed",
            ),
        })
    }
}

fn failure(code: &str, message: &str) -> SessionApplicationFailure {
    SessionApplicationFailure {
        code: code.to_owned(),
        message: message.to_owned(),
    }
}

/// Preserve existing semantic error codes while withholding private database/provider details.
impl bcode_session_models::application::RuntimeWorkApplication for SessionApplicationHandle<'_> {
    async fn runtime_work_page(
        &self,
        request: bcode_session_models::RuntimeWorkPageRequest,
    ) -> Result<bcode_session_models::RuntimeWorkPage, SessionApplicationFailure> {
        self.selected_location(request.session_id).await?;
        if request.version != bcode_session_models::RUNTIME_WORK_PAGE_VERSION {
            return Err(failure(
                "runtime_work_unsupported_version",
                "unsupported runtime work page version",
            ));
        }
        let (items, next_after) = self
            .state
            .runtime_work
            .active_page(
                request.session_id,
                request.after_work_id.as_ref(),
                request.limit,
                request.max_bytes,
            )
            .await?;
        Ok(bcode_session_models::RuntimeWorkPage {
            version: bcode_session_models::RUNTIME_WORK_PAGE_VERSION,
            session_id: request.session_id,
            items,
            complete: next_after.is_none(),
            next_after,
        })
    }

    async fn list_runtime_work(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<bcode_session_models::RuntimeWorkSnapshot>, SessionApplicationFailure> {
        self.selected_location(session_id).await?;
        Ok(super::runtime_work_operations::list(self.state, session_id).await)
    }

    async fn runtime_work_history(
        &self,
        session_id: SessionId,
        limit: usize,
    ) -> Result<Vec<bcode_session_models::SessionEvent>, SessionApplicationFailure> {
        self.selected_location(session_id).await?;
        super::runtime_work_operations::history(self.state, session_id, limit)
            .await
            .map_err(|error| session_failure(&error))
    }

    async fn cancel_runtime_work(
        &self,
        session_id: SessionId,
        work_id: bcode_session_models::WorkId,
    ) -> Result<bool, SessionApplicationFailure> {
        self.selected_location(session_id).await?;
        Ok(super::runtime_work_operations::cancel(
            self.state,
            session_id,
            work_id,
            Some(self.client_id),
        )
        .await)
    }
}

fn history_failure(error: session_operations::ReadHistoryError) -> SessionApplicationFailure {
    match error {
        session_operations::ReadHistoryError::Session(error) => session_failure(&error),
        error @ session_operations::ReadHistoryError::IncompatibleActiveNamespace(_) => {
            failure(error.code(), error.message())
        }
    }
}

fn server_failure(error: &crate::ServerError) -> SessionApplicationFailure {
    match error {
        crate::ServerError::Session(error) => session_failure(error),
        _ => failure(
            "session_operation_failed",
            "session operation failed; inspect canonical state before retrying",
        ),
    }
}

fn session_failure(error: &bcode_session::SessionError) -> SessionApplicationFailure {
    use bcode_session::SessionError;
    let (code, message) = match error {
        SessionError::Lease(bcode_session::lease::SessionLeaseError::OwnedByOtherDaemon {
            ..
        }) => (
            "session_active_elsewhere",
            "session is owned by another daemon; inspect and release its verified owner before retrying",
        ),
        SessionError::ProjectionStale { .. } => (
            "projection_stale",
            "session projection is stale; explicit maintenance is required",
        ),
        SessionError::StorageMigrationRequired { .. } => (
            "session_migration_required",
            "session storage requires migration",
        ),
        SessionError::TurnAdmission(_) => (
            "invalid_turn_admission",
            "turn admission metadata is invalid",
        ),
        SessionError::AppendGenerationChanged { .. } => (
            "session_generation_changed",
            "canonical session generation changed before append",
        ),
        SessionError::ConnectedClients(_) => (
            "session_connected_clients",
            "session has connected clients and cannot be deleted",
        ),
        SessionError::Deleting(_) => ("session_deleting", "session deletion is in progress"),
        SessionError::InvalidDerivationRequest(_) => (
            "invalid_derivation_request",
            "session derivation request is invalid",
        ),
        SessionError::DerivationOperationConflict(_) => (
            "derivation_operation_conflict",
            "derivation identity was reused with different request facts",
        ),
        SessionError::DerivationOperationNotFound(_) => (
            "derivation_operation_not_found",
            "derivation operation was not found",
        ),
        SessionError::DerivationCancelled(_) => (
            "cancelled",
            "session derivation was cancelled before canonical publication",
        ),
        SessionError::DerivationGenerationChanged { .. }
        | SessionError::CloneGenerationChanged { .. } => (
            "derivation_generation_changed",
            "canonical derivation source generation changed",
        ),
        SessionError::DerivationPromptLimit { .. } => (
            "derivation_prompt_limit",
            "derivation prompt query exceeds its bounded limit",
        ),
        SessionError::DerivationPromptNotFound { .. } => (
            "derivation_prompt_not_found",
            "selected derivation prompt was not found",
        ),
        SessionError::DerivationRequiresPersistentStore => (
            "derivation_persistence_required",
            "session derivation requires persistent canonical storage",
        ),
        SessionError::DerivationPublicationConflict(_) => (
            "derivation_publication_conflict",
            "canonical derivation destination conflicts with the retained request",
        ),
        SessionError::DurableEventPayloadTooLarge { .. } => (
            "session_payload_too_large",
            "canonical event payload exceeds its bounded limit",
        ),
        SessionError::UnsupportedProjectionWindow => (
            "unsupported_projection_window",
            "session projection window is unsupported",
        ),
        SessionError::NotFound(_) => (
            "session_not_found",
            "session was not found in the selected state location",
        ),
        SessionError::Db(error) => database_failure_details(error),
        _ => ("session_unavailable", "session is unavailable"),
    };
    failure(code, message)
}

fn database_failure_details(
    error: &bcode_session::db::SessionDbError,
) -> (&'static str, &'static str) {
    use bcode_session::{db::SessionDbError, persisted::PersistedSessionEventError};
    match error {
        SessionDbError::PersistedEvent(PersistedSessionEventError::UnsupportedSchemaVersion {
            actual,
            current,
        }) if actual < current => (
            "session_migration_required",
            "session history requires migration",
        ),
        SessionDbError::PersistedEvent(
            PersistedSessionEventError::UnsupportedSchemaVersion { .. }
            | PersistedSessionEventError::UnsupportedEventKind { .. },
        ) => (
            "session_format_incompatible",
            "session history uses an unsupported format; upgrade Bcode",
        ),
        SessionDbError::WriterIncompatible { .. } => (
            "session_writer_incompatible",
            "session writer is incompatible",
        ),
        _ if error.is_lock_error() => (
            "session_database_locked",
            "session database is locked; inspect the verified owner before retrying",
        ),
        SessionDbError::ProjectionIncompatible { .. }
        | SessionDbError::ProjectionStale { .. }
        | SessionDbError::ModelContextProjectionVersion { .. }
        | SessionDbError::ModelContextProjectionStale { .. } => (
            "projection_stale",
            "session projection is stale or incompatible; explicit maintenance is required",
        ),
        SessionDbError::HistoryReadLimitExceeded { .. } => (
            "session_history_limit_exceeded",
            "session history read exceeds its bounded limit",
        ),
        SessionDbError::MigrationHistoryIncompatible { .. }
        | SessionDbError::InvalidCanonicalSequence { .. }
        | SessionDbError::InvalidRow { .. } => (
            "session_repair_required",
            "session canonical history requires explicit repair",
        ),
        _ if crate::database_error_requires_repair(error) => (
            "session_repair_required",
            "session database requires explicit repair",
        ),
        _ => ("session_unavailable", "session is unavailable"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_models::{SessionHistoryDirection, SessionInspectionCategory};

    #[tokio::test]
    async fn admitted_compaction_observes_terminal_result_separately() {
        use bcode_session_models::SessionCompactionOperation as _;
        let (sender, completion) = tokio::sync::oneshot::channel();
        let operation = ServerSessionCompactionOperation { completion };
        let task = tokio::spawn(operation.complete());
        tokio::task::yield_now().await;
        assert!(!task.is_finished());
        assert!(sender.send(Ok("compacted".into())).is_ok());
        assert_eq!(
            task.await
                .expect("completion task")
                .expect("terminal result"),
            bcode_session_models::SessionCompactionResult::Compacted("compacted".into())
        );
    }

    #[tokio::test]
    async fn compaction_completion_preserves_normalized_errors() {
        use bcode_session_models::SessionCompactionOperation as _;
        for (error, code, message) in [
            (
                crate::CompactionError::Cancelled,
                "cancelled",
                "context compaction cancelled",
            ),
            (
                crate::CompactionError::ProviderUnavailable,
                "provider_unavailable",
                "model provider unavailable",
            ),
        ] {
            let (sender, completion) = tokio::sync::oneshot::channel();
            assert!(sender.send(Err(error)).is_ok());
            let failure = ServerSessionCompactionOperation { completion }
                .complete()
                .await
                .expect_err("terminal failure");
            assert_eq!(failure.code, code);
            assert_eq!(failure.message, message);
        }
    }

    fn state() -> Arc<ServerState> {
        Arc::new(crate::tests::test_server_state(
            bcode_session::SessionManager::default(),
        ))
    }

    fn application(state: &Arc<ServerState>) -> SessionApplicationHandle<'_> {
        application_for(state, ClientId::new())
    }

    fn application_for(state: &Arc<ServerState>, caller: ClientId) -> SessionApplicationHandle<'_> {
        SessionApplicationHandle::new(state, caller)
    }

    #[tokio::test]
    async fn typed_lifecycle_updates_canonical_state_and_bounded_inspection() {
        // Structured canonical inspection requires a DB; in-memory actors intentionally
        // reject it rather than synthesizing a replay-based fallback.
        let root = tempfile::tempdir().unwrap();
        let state = Arc::new(crate::tests::test_server_state(
            bcode_session::SessionManager::persistent(root.path()).unwrap(),
        ));
        {
            let directory = tempfile::tempdir().unwrap();
            let summary = application(&state)
                .create_session(CreateSessionRequest {
                    name: Some("typed session".to_owned()),
                    working_directory: directory.path().to_path_buf(),
                })
                .await
                .unwrap();
            let renamed = application(&state)
                .rename_session(summary.id, Some("renamed".to_owned()))
                .await
                .unwrap();
            assert_eq!(renamed.name.as_deref(), Some("renamed"));
            let changed = application(&state)
                .change_session_working_directory(summary.id, directory.path().to_path_buf())
                .await
                .unwrap();
            let unchanged = application(&state)
                .change_session_working_directory(summary.id, directory.path().to_path_buf())
                .await
                .unwrap();
            assert!(!unchanged.changed);
            assert_eq!(
                changed.session.working_directory,
                directory.path().canonicalize().unwrap()
            );
            application(&state)
                .set_session_model(
                    summary.id,
                    Some("test.provider".to_owned()),
                    "test.model".to_owned(),
                )
                .await
                .unwrap();
            let page = application(&state)
                .inspect_session(
                    summary.id,
                    SessionInspectionQuery {
                        category: SessionInspectionCategory::SelectionChanges,
                        cursor: None,
                        limit: 1,
                        direction: SessionHistoryDirection::Backward,
                    },
                )
                .await
                .unwrap();
            assert_eq!(page.session_id, summary.id);
            assert_eq!(page.events.len(), 1);
            assert!(matches!(
                page.events[0].kind,
                bcode_session_models::SessionEventKind::ModelChanged { .. }
            ));
            assert_eq!(
                state
                    .sessions
                    .session_summary(summary.id)
                    .await
                    .unwrap()
                    .name,
                renamed.name
            );
        }
        drop(state);
    }

    #[tokio::test]
    async fn invalid_creation_is_rejected_before_mutation() {
        let state = state();
        {
            let error = application(&state)
                .create_session(CreateSessionRequest {
                    name: None,
                    working_directory: PathBuf::from("relative"),
                })
                .await
                .unwrap_err();
            assert_eq!(error.code, "session_working_directory_must_be_absolute");
            assert!(state.session_runtimes.lock().await.is_empty());
        }
        drop(state);
    }

    #[tokio::test]
    async fn noninteractive_admission_does_not_acquire_or_enqueue_work() {
        let state = state();
        {
            // A nonexistent ID demonstrates priority rejection precedes ownership acquisition.
            let admission = application(&state)
                .submit_turn(
                    SessionId::new(),
                    "ignored".to_owned(),
                    TurnAdmissionMetadata {
                        priority: TurnPriority::Background,
                        ..TurnAdmissionMetadata::default()
                    },
                )
                .await
                .unwrap();
            assert_eq!(
                admission,
                TurnAdmission::Rejected(TurnRejectionReason::ExecutionPolicy)
            );
            assert!(state.session_runtimes.lock().await.is_empty());
        }
        drop(state);
    }

    #[tokio::test]
    async fn namespace_fencing_precedes_priority_and_bounded_inspection() {
        let state = state();
        let caller = ClientId::new();
        {
            let id = SessionId::new();
            state
                .try_activate_session_namespace(id, "foreign-artifact".to_owned())
                .await
                .unwrap();
            let error = application_for(&state, caller)
                .submit_turn(
                    id,
                    "ignored".to_owned(),
                    TurnAdmissionMetadata {
                        priority: TurnPriority::Background,
                        ..TurnAdmissionMetadata::default()
                    },
                )
                .await
                .unwrap_err();
            assert_eq!(error.code, "session_incompatible_active_client");
            let error = application_for(&state, caller)
                .inspect_session(
                    id,
                    SessionInspectionQuery {
                        category: SessionInspectionCategory::SelectionChanges,
                        cursor: None,
                        limit: 1,
                        direction: SessionHistoryDirection::Backward,
                    },
                )
                .await
                .unwrap_err();
            assert_eq!(error.code, "session_incompatible_active_client");
            assert!(!error.message.contains("foreign-artifact"));
            assert!(state.session_runtimes.lock().await.is_empty());
        }
        drop(state);
    }

    #[tokio::test]
    async fn typed_bounded_history_and_artifacts_reject_oversized_requests() {
        let root = tempfile::tempdir().unwrap();
        let state = Arc::new(crate::tests::test_server_state(
            bcode_session::SessionManager::persistent(root.path()).unwrap(),
        ));
        {
            let caller = ClientId::new();
            let session = application_for(&state, caller)
                .create_session(CreateSessionRequest {
                    name: None,
                    working_directory: std::env::current_dir().unwrap(),
                })
                .await
                .unwrap();
            let page = application_for(&state, caller)
                .session_history_page(
                    session.id,
                    bcode_session_models::SessionHistoryQuery {
                        cursor: None,
                        limit: 1,
                        direction: SessionHistoryDirection::Backward,
                    },
                )
                .await
                .unwrap();
            assert!(page.events.len() <= 1);
            let error = application_for(&state, caller)
                .session_history_page(
                    session.id,
                    bcode_session_models::SessionHistoryQuery {
                        cursor: None,
                        limit: bcode_session_models::MAX_SESSION_HISTORY_READ_EVENTS + 1,
                        direction: SessionHistoryDirection::Backward,
                    },
                )
                .await
                .unwrap_err();
            assert_eq!(error.code, "session_history_limit_exceeded");
            let error = application_for(&state, caller)
                .session_artifact_range(
                    session.id,
                    "absent".to_owned(),
                    "absent".to_owned(),
                    0,
                    bcode_session_models::MAX_SESSION_ARTIFACT_RANGE_BYTES + 1,
                )
                .await
                .unwrap_err();
            assert_eq!(error.code, "invalid_artifact_range_length");
        }
        drop(state);
    }

    #[test]
    fn portable_failures_keep_conflict_cancellation_and_validation_distinct() {
        let operation_id = bcode_session_models::SessionDerivationOperationId::new();
        for (error, code) in [
            (
                bcode_session::SessionError::DerivationOperationConflict(operation_id),
                "derivation_operation_conflict",
            ),
            (
                bcode_session::SessionError::DerivationCancelled(operation_id),
                "cancelled",
            ),
            (
                bcode_session::SessionError::InvalidDerivationRequest(
                    "private diagnostic".to_owned(),
                ),
                "invalid_derivation_request",
            ),
        ] {
            let normalized = session_failure(&error);
            assert_eq!(normalized.code, code);
            assert!(!normalized.message.contains("private diagnostic"));
        }
    }

    #[tokio::test]
    async fn typed_submission_limit_rejects_before_scheduler_effects() {
        let state = state();
        {
            let caller = ClientId::new();
            for limit in [0, 129] {
                let error = application_for(&state, caller)
                    .pending_submissions(SessionId::new(), None, limit)
                    .await
                    .unwrap_err();
                assert_eq!(error.code, "invalid_request");
            }
            assert!(state.session_runtimes.lock().await.is_empty());
        }
        drop(state);
    }

    #[tokio::test]
    async fn typed_drafts_reasoning_and_skills_keep_canonical_ownership() {
        let root = tempfile::tempdir().unwrap();
        let state = Arc::new(crate::tests::test_server_state(
            bcode_session::SessionManager::persistent(root.path()).unwrap(),
        ));
        {
            let caller = ClientId::new();
            let session = application_for(&state, caller)
                .create_session(CreateSessionRequest {
                    name: None,
                    working_directory: std::env::current_dir().unwrap(),
                })
                .await
                .unwrap();
            let scope = bcode_session_models::ComposerDraftScope::Session {
                session_id: session.id,
            };
            application_for(&state, caller)
                .set_composer_draft(scope.clone(), "explicit draft".to_owned())
                .await
                .unwrap();
            assert_eq!(
                application_for(&state, caller)
                    .composer_draft(scope)
                    .await
                    .unwrap()
                    .as_deref(),
                Some("explicit draft")
            );
            application_for(&state, caller)
                .set_session_reasoning(
                    session.id,
                    Some("high".to_owned()),
                    Some("detailed".to_owned()),
                )
                .await
                .unwrap();
            let events = state.sessions.session_history(session.id).await.unwrap();
            assert!(events.iter().any(|event| matches!(
                &event.kind,
                bcode_session_models::SessionEventKind::ReasoningChanged { .. }
            )));
            let error = application_for(&state, caller)
                .activate_skill(
                    session.id,
                    bcode_skill_models::SkillId("missing".to_owned()),
                )
                .await
                .unwrap_err();
            assert_eq!(error.code, "skills_disabled");
            let active = application_for(&state, caller)
                .active_skills(session.id)
                .await
                .unwrap();
            assert!(active.is_empty());
        }
        drop(state);
    }

    #[tokio::test]
    async fn typed_runtime_and_mutations_fail_closed_for_foreign_namespace() {
        use bcode_session_models::application::RuntimeWorkApplication as _;
        let state = state();
        {
            let caller = ClientId::new();
            let id = SessionId::new();
            state
                .try_activate_session_namespace(id, "foreign-artifact".to_owned())
                .await
                .unwrap();
            for error in [
                application_for(&state, caller)
                    .delete_session(id)
                    .await
                    .unwrap_err(),
                application_for(&state, caller)
                    .set_session_reasoning(id, Some("high".to_owned()), None)
                    .await
                    .unwrap_err(),
                application_for(&state, caller)
                    .list_runtime_work(id)
                    .await
                    .unwrap_err(),
                application_for(&state, caller)
                    .runtime_work_history(id, 0)
                    .await
                    .unwrap_err(),
                application_for(&state, caller)
                    .cancel_runtime_work(id, bcode_session_models::WorkId::new("missing"))
                    .await
                    .unwrap_err(),
                application_for(&state, caller)
                    .set_composer_draft(
                        bcode_session_models::ComposerDraftScope::Session { session_id: id },
                        "ignored".to_owned(),
                    )
                    .await
                    .unwrap_err(),
            ] {
                assert_eq!(error.code, "session_incompatible_active_client");
                assert!(!error.message.contains("foreign-artifact"));
            }
            assert!(state.session_runtimes.lock().await.is_empty());
        }
        drop(state);
    }

    #[tokio::test]
    async fn typed_spawn_guard_preserves_preparation_envelope_before_effects() {
        let state = state();
        {
            let parent_id = SessionId::new();
            state
                .try_activate_session_namespace(parent_id, "foreign-artifact".to_owned())
                .await
                .unwrap();
            let child_id = SessionId::new();
            let result = application(&state)
                .spawn_session(bcode_session_models::SessionSpawnRequest {
                    version: 1,
                    creation: bcode_session_models::SessionChildCreateRequest {
                        child_id,
                        parent_id,
                        name: None,
                        working_directory: std::env::current_dir().unwrap(),
                    },
                    worktree_directory: None,
                    input: "must not execute".to_owned(),
                    admission: TurnAdmissionMetadata::default(),
                    auth_profile: None,
                })
                .await;
            assert!(result.session.is_none());
            assert!(result.admission.is_none());
            let error = result.failure.unwrap();
            assert_eq!(
                error.stage,
                bcode_session_models::SessionSpawnStage::Preparation
            );
            assert_eq!(error.code, "session_incompatible_active_client");
            assert!(matches!(
                state.sessions.session_summary_non_owning(child_id).await,
                Err(bcode_session::SessionError::NotFound(_))
            ));
            assert!(state.session_runtimes.lock().await.is_empty());
        }
        drop(state);
    }

    #[tokio::test]
    async fn missing_session_admission_preserves_not_found_failure() {
        let state = state();
        {
            let error = application(&state)
                .submit_turn(
                    SessionId::new(),
                    "hello".to_owned(),
                    TurnAdmissionMetadata::default(),
                )
                .await
                .unwrap_err();
            assert_eq!(error.code, "session_not_found");
            assert!(state.session_runtimes.lock().await.is_empty());
        }
        drop(state);
    }

    #[tokio::test]
    async fn targeted_cancellation_of_idle_session_is_not_terminal_success() {
        let state = state();
        {
            let directory = tempfile::tempdir().unwrap();
            let session = application(&state)
                .create_session(CreateSessionRequest {
                    name: None,
                    working_directory: directory.path().to_path_buf(),
                })
                .await
                .unwrap();
            assert!(
                !application(&state)
                    .cancel_session_turn(session.id, false, Some("different-turn".to_owned()))
                    .await
                    .unwrap()
            );
            assert_eq!(
                state.sessions.session_summary(session.id).await.unwrap().id,
                session.id
            );
        }
        drop(state);
    }

    #[tokio::test]
    async fn configuration_failure_does_not_leak_requested_profile() {
        let state = state();
        {
            let error = application(&state)
                .set_session_agent(SessionId::new(), "secret-profile-detail".to_owned())
                .await
                .unwrap_err();
            assert_eq!(error.code, "unknown_agent");
            assert!(!error.message.contains("secret-profile-detail"));
        }
        drop(state);
    }

    #[tokio::test]
    async fn retained_handle_rejects_every_operation_after_shutdown() {
        let state = state();
        {
            state
                .shutdown_requested
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let id = SessionId::new();
            let query = SessionInspectionQuery {
                category: SessionInspectionCategory::SelectionChanges,
                cursor: None,
                limit: 1,
                direction: SessionHistoryDirection::Backward,
            };
            let errors = [
                application(&state)
                    .create_session(CreateSessionRequest {
                        name: None,
                        working_directory: PathBuf::from("relative"),
                    })
                    .await
                    .unwrap_err(),
                application(&state)
                    .inspect_session(id, query)
                    .await
                    .unwrap_err(),
                application(&state)
                    .rename_session(id, None)
                    .await
                    .unwrap_err(),
                application(&state)
                    .change_session_working_directory(id, PathBuf::from("relative"))
                    .await
                    .unwrap_err(),
                application(&state)
                    .submit_turn(id, "ignored".to_owned(), TurnAdmissionMetadata::default())
                    .await
                    .unwrap_err(),
                application(&state)
                    .set_session_model(id, None, "ignored".to_owned())
                    .await
                    .unwrap_err(),
                application(&state)
                    .set_session_agent(id, "ignored".to_owned())
                    .await
                    .unwrap_err(),
                application(&state)
                    .cancel_session_turn(id, true, None)
                    .await
                    .unwrap_err(),
            ];
            assert!(
                errors
                    .iter()
                    .all(|error| error.code == "session_application_stopped")
            );
            assert!(state.session_runtimes.lock().await.is_empty());
        }
        drop(state);
    }

    #[tokio::test]
    async fn in_memory_inspection_preserves_database_unavailable_failure() {
        let state = state();
        {
            let directory = tempfile::tempdir().unwrap();
            let session = application(&state)
                .create_session(CreateSessionRequest {
                    name: None,
                    working_directory: directory.path().to_path_buf(),
                })
                .await
                .unwrap();
            let query = SessionInspectionQuery {
                category: SessionInspectionCategory::SelectionChanges,
                cursor: None,
                limit: 1,
                direction: SessionHistoryDirection::Backward,
            };
            assert!(matches!(
                state.sessions.session_inspection_page(session.id, query).await,
                Err(bcode_session::SessionError::DbUnavailable(id)) if id == session.id
            ));
            let error = application(&state)
                .inspect_session(session.id, query)
                .await
                .unwrap_err();
            assert_eq!(error.code, "session_unavailable");
            assert_eq!(error.message, "session is unavailable");
        }
        drop(state);
    }

    #[test]
    fn borrowed_handle_does_not_retain_a_host_reference() {
        let state = state();
        let before = Arc::strong_count(&state);
        {
            let app = application(&state);
            assert_eq!(Arc::strong_count(&state), before);
            assert!(std::ptr::eq(app.state, std::ptr::from_ref(&state)));
        }
        drop(state);
    }

    #[tokio::test]
    async fn recovery_preparation_without_checkpoint_does_not_mutate_history() {
        use bcode_session_models::SessionCompactionRecoveryDiscoveryApplication as _;
        let state = state();
        let session = state
            .sessions
            .create_session(
                Some("recovery preparation".into()),
                std::env::current_dir().unwrap(),
            )
            .await
            .unwrap();
        let before = state.sessions.session_history(session.id).await.unwrap();
        let result = SessionApplicationHandle::new(&state, ClientId::new())
            .prepare_session_compaction_recovery(
                bcode_session_models::SessionCompactionRecoveryDiscoveryRequest {
                    version: 1,
                    session_id: session.id,
                    operation_id: uuid::Uuid::new_v4(),
                    budget: bcode_session_models::SessionCompactionRestartBudget {
                        max_provider_requests: 16,
                        timeout_ms: 30_000,
                    },
                },
            )
            .await
            .unwrap();
        assert!(result.restart.is_none());
        assert!(result.blocker.is_some());
        assert_eq!(
            before,
            state.sessions.session_history(session.id).await.unwrap()
        );
    }

    #[test]
    fn recovery_blockers_reach_the_application_without_becoming_policy_errors() {
        use bcode_session_models::SessionCompactionRecoveryBlocker as Blocker;
        for (blocker, code) in [
            (
                Blocker::UnsettledRequests,
                "compaction_recovery_request_unsettled",
            ),
            (
                Blocker::EvidenceUnavailable,
                "compaction_recovery_evidence_incomplete",
            ),
            (
                Blocker::UnsupportedFailure,
                "compaction_recovery_failure_unproven",
            ),
            (
                Blocker::AlreadySuperseded,
                "compaction_recovery_operation_superseded",
            ),
        ] {
            let error = compaction_failure(session_operations::CompactError::Compaction(
                crate::CompactionError::RecoveryBlocked(blocker),
            ));
            assert_eq!(error.code, code);
            assert!(!error.message.contains("prepare explicit"));
        }
    }

    #[test]
    fn compaction_deadline_failures_survive_application_error_mapping() {
        for (message, code, expected) in [
            (
                "explicit compaction restart deadline exhausted; boundary unchanged",
                "compaction_restart_deadline_exhausted",
                "restart deadline expired",
            ),
            (
                "compaction corrective deadline exhausted; boundary unchanged",
                "compaction_corrective_deadline_exhausted",
                "corrective dispatch deadline expired",
            ),
            (
                "compaction corrective allowance exhausted; boundary unchanged",
                "compaction_corrective_allowance_exhausted",
                "unsettled request",
            ),
        ] {
            let error = compaction_failure(session_operations::CompactError::Compaction(
                crate::CompactionError::Provider(message.into()),
            ));
            assert_eq!(error.code, code);
            assert!(error.message.contains(expected));
            assert!(error.message.contains("boundary is unchanged"));
            assert!(error.message.contains("preserved"));
        }
        let secret = compaction_provider_failure(
            "explicit compaction restart deadline exhausted; boundary unchanged SECRET credential",
        );
        assert_eq!(secret.code, "provider_error");
        assert!(!secret.message.contains("SECRET"));
    }

    #[test]
    fn compaction_host_failures_are_actionable_without_leaking_provider_text() {
        for (message, code) in [
            (
                "compaction capacity policy rejected: private detail",
                "compaction_capacity",
            ),
            (
                "compaction synthesis rejected after bounded repair; private detail",
                "compaction_synthesis_rejected",
            ),
            (
                "compaction request/repair budget exhausted; private detail",
                "compaction_allowance_exhausted",
            ),
            (
                "compaction synthesis policy changed private detail",
                "compaction_recovery_required",
            ),
            ("provider credential private detail", "provider_error"),
        ] {
            let error = compaction_provider_failure(message);
            assert_eq!(error.code, code);
            assert!(!error.message.contains("private detail"));
        }
    }

    #[test]
    fn private_storage_failures_are_normalized_without_details() {
        let private = bcode_session::SessionError::MigrationBackup {
            session_id: SessionId::new(),
            reason: "secret-database-detail".to_owned(),
        };
        let error = session_failure(&private);
        drop(private);
        assert_eq!(error.code, "session_unavailable");
        assert!(!error.message.contains("secret-database-detail"));
        assert_eq!(
            session_failure(&bcode_session::SessionError::NotFound(SessionId::new())).code,
            "session_not_found"
        );
    }
}
