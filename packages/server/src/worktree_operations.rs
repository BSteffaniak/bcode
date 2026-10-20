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

/// Normalize domain failures without publishing Git stderr, paths, or setup output.
#[must_use]
pub fn normalize_error(
    error: &bcode_worktree::WorktreeError,
) -> bcode_worktree_models::application::WorktreeApplicationFailure {
    let (code, message) = match error {
        bcode_worktree::WorktreeError::Git(_) => {
            ("worktree_git_failed", "Git worktree operation failed")
        }
        bcode_worktree::WorktreeError::InvalidRequest(_) => {
            ("worktree_invalid_request", "worktree request is invalid")
        }
        bcode_worktree::WorktreeError::RemoveRefused(_) => {
            ("worktree_remove_refused", "worktree removal was refused")
        }
        bcode_worktree::WorktreeError::Setup(_) => {
            ("worktree_setup_failed", "worktree setup failed")
        }
        bcode_worktree::WorktreeError::Io(_) => {
            ("worktree_io_failed", "worktree filesystem operation failed")
        }
    };
    bcode_worktree_models::application::WorktreeApplicationFailure {
        code: code.into(),
        message: message.into(),
    }
}

/// Preserve the removal safety category without exposing canonical session paths.
#[must_use]
pub fn normalize_remove_error(
    error: RemoveError,
) -> bcode_worktree_models::application::WorktreeApplicationFailure {
    match error {
        RemoveError::SessionInside { .. } => bcode_worktree_models::application::WorktreeApplicationFailure {
            code: "worktree_has_session".into(),
            message: "a canonical session is rooted inside the worktree; move or delete it before removal".into(),
        },
        RemoveError::Worktree(error) => normalize_error(&error),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn normalization_does_not_expose_domain_diagnostics() {
        let cases = [
            (
                bcode_worktree::WorktreeError::InvalidRequest("secret-path".into()),
                "worktree_invalid_request",
            ),
            (
                bcode_worktree::WorktreeError::RemoveRefused("secret-path".into()),
                "worktree_remove_refused",
            ),
            (
                bcode_worktree::WorktreeError::Setup("credential-output".into()),
                "worktree_setup_failed",
            ),
            (
                bcode_worktree::WorktreeError::Io(std::io::Error::other("secret-path")),
                "worktree_io_failed",
            ),
        ];
        for (error, code) in cases {
            let public = super::normalize_error(&error);
            assert_eq!(public.code, code);
            assert!(!public.message.contains("secret-path"));
            assert!(!public.message.contains("credential-output"));
        }
    }

    #[test]
    fn rooted_session_removal_failure_is_secret_safe() {
        let public = super::normalize_remove_error(super::RemoveError::SessionInside {
            session_id: bcode_session_models::SessionId::new(),
            path: "secret-path".into(),
        });
        assert_eq!(public.code, "worktree_has_session");
        assert!(!public.message.contains("secret-path"));
    }
}
