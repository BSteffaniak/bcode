#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Shared worktree models for Bcode.

use bcode_session_models::{SessionId, SessionSummary};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Configured strategy for choosing the base ref for newly-created worktrees.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeBaseRef {
    /// Use context-sensitive defaults.
    #[default]
    Auto,
    /// Use the repository default branch when possible.
    DefaultBranch,
    /// Use the current checkout's `HEAD`.
    Head,
}

/// Git worktree summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeInfo {
    /// Filesystem path to the worktree.
    pub path: PathBuf,
    /// Whether this is the repository's main worktree.
    pub is_main: bool,
    /// Checked-out branch name, if HEAD is not detached.
    #[serde(default)]
    pub branch: Option<String>,
    /// Short current commit hash, when available.
    #[serde(default)]
    pub commit: Option<String>,
}

/// Worktree list request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeListRequest {
    /// Directory to discover the repository from. Defaults to the caller cwd.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

/// Worktree list response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeListResponse {
    /// Main repository root.
    pub repo_root: PathBuf,
    /// Current worktree path for the discovery cwd.
    pub current_worktree: PathBuf,
    /// Registered worktrees.
    pub worktrees: Vec<WorktreeInfo>,
}

/// Worktree creation request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct WorktreeCreateRequest {
    /// Human/task name used for default path and branch derivation.
    pub name: String,
    /// Directory to discover the source repository from. Defaults to caller cwd.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Explicit target path. Defaults to configured worktree root plus slug.
    #[serde(default)]
    pub path: Option<PathBuf>,
    /// Existing branch to check out.
    #[serde(default)]
    pub branch: Option<String>,
    /// New branch name to create. Defaults to configured prefix plus slug.
    #[serde(default)]
    pub new_branch: Option<String>,
    /// Explicit base ref strategy.
    #[serde(default)]
    pub base_ref: Option<WorktreeBaseRef>,
    /// Create a detached worktree.
    #[serde(default)]
    pub detach: bool,
    /// Force git worktree creation.
    #[serde(default)]
    pub force: bool,
    /// Session to move into the worktree after creation.
    #[serde(default)]
    pub attach_session_id: Option<SessionId>,
    /// Create a new Bcode session rooted at the worktree.
    #[serde(default)]
    pub new_session: bool,
    /// Disable automatic setup for this request.
    #[serde(default)]
    pub no_setup: bool,
}

/// Worktree creation response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeCreateResponse {
    /// Main repository root.
    pub repo_root: PathBuf,
    /// Created worktree path.
    pub path: PathBuf,
    /// Branch requested for creation or checkout.
    #[serde(default)]
    pub branch: Option<String>,
    /// Whether a new branch was requested.
    pub created_branch: bool,
    /// Whether setup was attempted.
    pub setup_applied: bool,
    /// Updated or created session when requested.
    #[serde(default)]
    pub session: Option<SessionSummary>,
}

/// Worktree removal request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeRemoveRequest {
    /// Directory to discover the source repository from. Defaults to caller cwd.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Worktree path to remove.
    pub path: PathBuf,
    /// Force removal.
    #[serde(default)]
    pub force: bool,
}

/// Worktree removal response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeRemoveResponse {
    /// Removed worktree path.
    pub path: PathBuf,
}
