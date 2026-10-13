//! Typed session application operations shared by Rust consumers and transport adapters.
//!
//! Implementations retain verified caller authority. These contracts do not grant authority,
//! select storage, repair history, or define a host lifecycle. Failures do not imply rollback;
//! dropping an operation future does not establish that an admitted mutation was cancelled.

use crate::{SessionId, SessionInspectionPage, SessionInspectionQuery, SessionSummary};
use serde::{Deserialize, Serialize};
use std::{future::Future, path::PathBuf};

/// Request to create a canonical session in the application's selected state location.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateSessionRequest {
    /// Optional user-facing session name.
    pub name: Option<String>,
    /// Absolute canonical session working directory.
    pub working_directory: PathBuf,
}

/// Result of changing an idle session's canonical working directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionWorkingDirectoryChange {
    /// Canonical summary after the operation.
    pub session: SessionSummary,
    /// Whether the operation appended a working-directory change event.
    pub changed: bool,
}

/// Secret-safe application failure; private storage and transport errors are not exposed.
///
/// Codes are extensible: consumers must preserve unknown codes rather than assuming success.
/// An error does not promise rollback or make automatic mutation retries safe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct SessionApplicationFailure {
    /// Stable operation failure code.
    pub code: String,
    /// Normalized public message, never a private error's display representation.
    pub message: String,
}

/// Session operations bound to a verified caller in an existing application.
///
/// The owning application supplies access; callers do not construct private server state or
/// provide a transport identity on each request. Implementations retain namespace fencing,
/// canonical event publication, bounded inspection, and derived catalog maintenance.
pub trait SessionApplication: Sync {
    /// Create and publish one canonical session.
    ///
    /// # Errors
    /// Rejects relative working directories, unavailable storage, or failed canonical creation.
    fn create_session(
        &self,
        request: CreateSessionRequest,
    ) -> impl Future<Output = Result<SessionSummary, SessionApplicationFailure>> + Send;

    /// Inspect one bounded category of canonical events without repair or full replay.
    ///
    /// # Errors
    /// Rejects incompatible active namespaces, invalid queries, or unavailable canonical history.
    fn inspect_session(
        &self,
        session_id: SessionId,
        query: SessionInspectionQuery,
    ) -> impl Future<Output = Result<SessionInspectionPage, SessionApplicationFailure>> + Send;

    /// Rename a canonical session, publishing its event and updating its derived catalog entry.
    ///
    /// # Errors
    /// Returns a normalized failure if the canonical session mutation cannot complete.
    fn rename_session(
        &self,
        session_id: SessionId,
        name: Option<String>,
    ) -> impl Future<Output = Result<SessionSummary, SessionApplicationFailure>> + Send;

    /// Change an idle session's working directory, resolving the absolute directory first.
    ///
    /// # Errors
    /// Rejects relative, unavailable, or non-directory paths, active turns, or failed mutation.
    fn change_session_working_directory(
        &self,
        session_id: SessionId,
        working_directory: PathBuf,
    ) -> impl Future<Output = Result<SessionWorkingDirectoryChange, SessionApplicationFailure>> + Send;

    /// Admit and enqueue an interactive turn through the existing canonical scheduler.
    ///
    /// Admission preserves producer/idempotency semantics and clears caller-supplied activity
    /// display metadata. Non-interactive priority returns `Rejected(ExecutionPolicy)`.
    ///
    /// # Errors
    /// Rejects incompatible namespaces, ownership or admission failures, or queue dispatch
    /// failure. A dispatch failure may follow durable admission and does not permit blind retry.
    fn submit_turn(
        &self,
        session_id: SessionId,
        text: String,
        admission: crate::TurnAdmissionMetadata,
    ) -> impl Future<Output = Result<crate::TurnAdmission, SessionApplicationFailure>> + Send;

    /// Persist an explicit model selection using the existing provider/model selection path.
    ///
    /// # Errors
    /// Rejects required-model skill conflicts or failed canonical selection mutation.
    fn set_session_model(
        &self,
        session_id: SessionId,
        provider_plugin_id: Option<String>,
        model_id: String,
    ) -> impl Future<Output = Result<(), SessionApplicationFailure>> + Send;

    /// Resolve and persist an existing agent profile selection.
    ///
    /// # Errors
    /// Rejects unknown profiles or failed canonical selection mutation.
    fn set_session_agent(
        &self,
        session_id: SessionId,
        agent_id: String,
    ) -> impl Future<Output = Result<(), SessionApplicationFailure>> + Send;

    /// Request cancellation through canonical runtime scheduling.
    ///
    /// `expected_turn_id` targets an exact active turn when supplied. The returned boolean is
    /// an acknowledgement, not a terminal outcome. Clearing queued work is explicit. Dropping
    /// observation or this future does not itself cancel execution.
    ///
    /// # Errors
    /// Returns a normalized failure if cancellation cannot be admitted or dispatched.
    fn cancel_session_turn(
        &self,
        session_id: SessionId,
        clear_queue: bool,
        expected_turn_id: Option<String>,
    ) -> impl Future<Output = Result<bool, SessionApplicationFailure>> + Send;
}
