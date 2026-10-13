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
        self.require_running()
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

impl SessionApplication for SessionApplicationHandle<'_> {
    async fn create_session(
        &self,
        request: CreateSessionRequest,
    ) -> Result<SessionSummary, SessionApplicationFailure> {
        self.require_running()?;
        session_operations::create(self.state, request.name, request.working_directory)
            .await
            .map_err(|error| failure(error.code(), error.message()))
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
        let result = session_operations::submit_turn(
            self.state,
            session_id,
            self.client_id,
            text,
            admission,
            &mut ownership,
        )
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
fn session_failure(error: &bcode_session::SessionError) -> SessionApplicationFailure {
    use bcode_session::{SessionError, db::SessionDbError, persisted::PersistedSessionEventError};
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
        SessionError::NotFound(_) => (
            "session_not_found",
            "session was not found in the selected state location",
        ),
        SessionError::Db(error) => match error {
            SessionDbError::PersistedEvent(
                PersistedSessionEventError::UnsupportedSchemaVersion { actual, current },
            ) if actual < current => (
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
        },
        _ => ("session_unavailable", "session is unavailable"),
    };
    failure(code, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_models::{SessionHistoryDirection, SessionInspectionCategory};

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
