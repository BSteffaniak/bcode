//! Transport-neutral application operations for worktree behavior.

use super::ServerState;
use std::path::Path;

/// Return worktrees for one repository context.
pub fn list(
    cwd: &Path,
) -> Result<bcode_worktree_models::WorktreeListResponse, bcode_worktree::WorktreeError> {
    bcode_worktree::list_worktrees(cwd)
}

/// Application failure while removing one worktree.
#[derive(Debug, thiserror::Error)]
pub enum RemoveError {
    /// A canonical session remains rooted inside the target worktree.
    #[error(
        "session {session_id} is rooted inside worktree {path}; move or delete it before removal"
    )]
    SessionInside {
        session_id: bcode_session_models::SessionId,
        path: String,
    },
    /// The domain worktree operation failed.
    #[error(transparent)]
    Worktree(#[from] bcode_worktree::WorktreeError),
}

/// Remove one unused worktree through the domain owner.
pub async fn remove(
    state: &ServerState,
    cwd: &Path,
    path: &Path,
    force: bool,
) -> Result<bcode_worktree_models::WorktreeRemoveResponse, RemoveError> {
    let sessions = state.sessions.cached_sessions(cwd).await;
    if let Some(session) = sessions
        .iter()
        .find(|session| super::path_is_inside(&session.working_directory, path))
    {
        return Err(RemoveError::SessionInside {
            session_id: session.id,
            path: bcode_plugin_sdk::path::display_from_current_dir(path).to_string(),
        });
    }
    Ok(bcode_worktree::remove_worktree(cwd, path, force)?)
}
