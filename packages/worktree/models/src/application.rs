//! Portable worktree operations; creation identities retain existing transient semantics.

use std::{future::Future, path::PathBuf};

/// Secret-safe failure without Git stderr or private host errors.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorktreeApplicationFailure {
    /// Stable diagnostic code.
    pub code: String,
    /// Normalized public message.
    pub message: String,
}

/// Worktree access through the existing repository and creation coordinator owners.
pub trait WorktreeApplication: Sync {
    /// List worktrees in one explicit repository context.
    fn list_worktrees(
        &self,
        cwd: PathBuf,
    ) -> impl Future<Output = Result<crate::WorktreeListResponse, WorktreeApplicationFailure>> + Send;
    /// Remove an unused worktree; sessions rooted within it prevent removal.
    fn remove_worktree(
        &self,
        cwd: PathBuf,
        path: PathBuf,
        force: bool,
    ) -> impl Future<Output = Result<crate::WorktreeRemoveResponse, WorktreeApplicationFailure>> + Send;
    /// Admit idempotent transient creation. Conflicting identity reuse is rejected.
    fn start_worktree_creation(
        &self,
        operation_id: String,
        request: crate::WorktreeCreateRequest,
    ) -> impl Future<
        Output = Result<crate::WorktreeCreateOperationStatus, WorktreeApplicationFailure>,
    > + Send;
    /// Read one retained transient creation; absence never retries creation.
    fn worktree_creation_status(
        &self,
        operation_id: String,
    ) -> impl Future<
        Output = Result<crate::WorktreeCreateOperationStatus, WorktreeApplicationFailure>,
    > + Send;
    /// Wait for a revision or terminal snapshot with a bounded timeout.
    fn wait_worktree_creation(
        &self,
        operation_id: String,
        after_revision: u64,
        timeout_ms: u64,
    ) -> impl Future<
        Output = Result<crate::WorktreeCreateOperationStatus, WorktreeApplicationFailure>,
    > + Send;
}
