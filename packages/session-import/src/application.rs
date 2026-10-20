//! Typed import discovery and explicit canonical import operations.

use std::{future::Future, path::PathBuf};

/// Explicit import destination context; paths are hints checked by the owning host.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ApplicationImportRequest {
    /// Exact enabled import source.
    pub source_id: String,
    /// Exact source-owned session identity.
    pub external_session_id: String,
    /// Explicit fallback workspace for imported history.
    pub working_directory: PathBuf,
}

/// Canonical import receipt with source-owned warnings, never a transient rendering state.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ApplicationImportResult {
    /// Verified canonical session summary.
    pub session: bcode_session_models::SessionSummary,
    /// Normalized source warnings.
    pub warnings: Vec<crate::ImportWarning>,
}

/// Secret-safe import failure; callers must inspect canonical state before retrying mutations.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ImportApplicationFailure {
    /// Extensible diagnostic category.
    pub code: String,
    /// Normalized public diagnostic.
    pub message: String,
}

/// Import access through the existing source routing and canonical import owners.
pub trait ImportApplication: Sync {
    /// Discover source metadata without importing history.
    fn import_sources(
        &self,
    ) -> impl Future<Output = Result<crate::ListImportSourcesResponse, ImportApplicationFailure>> + Send;
    /// Read one source-owned bounded discovery page.
    fn discover_importable_sessions(
        &self,
        request: crate::DiscoverImportableSessionsRequest,
    ) -> impl Future<
        Output = Result<crate::DiscoverImportableSessionsResponse, ImportApplicationFailure>,
    > + Send;
    /// Read source-owned import material without making it canonical implicitly.
    fn load_importable_session(
        &self,
        request: crate::LoadImportableSessionRequest,
    ) -> impl Future<Output = Result<crate::ImportableSession, ImportApplicationFailure>> + Send;
    /// Explicitly import through canonical identity, collision and ownership checks.
    fn import_session(
        &self,
        request: ApplicationImportRequest,
    ) -> impl Future<Output = Result<ApplicationImportResult, ImportApplicationFailure>> + Send;
}
