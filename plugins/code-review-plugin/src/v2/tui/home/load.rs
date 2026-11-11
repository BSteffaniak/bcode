//! Home's service calls. Each runs on a host-spawned task and reports one message; none blocks
//! input, and each is bounded (one listing, one repository summary, one provider listing, one
//! stack, and per-review stats of a capped number of reviews).

use super::model::{PullsAnswer, WorkspaceStats};
use crate::v2::provider::{ProviderRoute, discover_providers, list_pulls, resolve_stack};
use crate::v2::tui::backend::{BackendTransport, SharedBackend, call_json};
use crate::v2::tui::entry::provider_message;
use bcode_code_review_models::{
    ArchiveReviewWorkspaceRequest, ArchiveReviewWorkspaceResponse, ListReviewWorkspacesRequest,
    ListReviewWorkspacesResponse, OP_REVIEW_INDEX_GET, OP_REVIEW_REPO_STATE,
    OP_REVIEW_SOURCES_RESOLVE, OP_REVIEW_WORKSPACE_ARCHIVE, OP_REVIEW_WORKSPACE_LIST,
    OP_REVIEW_WORKSPACE_UPDATE, ResolveReviewSourcesRequest, ResolveReviewSourcesResponse,
    ReviewIndexRequest, ReviewIndexResponse, ReviewRepoStateRequest, ReviewRepoStateResponse,
    ReviewStack, ReviewWorkspace, ReviewWorkspaceListItem, UpdateReviewWorkspaceRequest,
    UpdateReviewWorkspaceResponse,
};
use std::path::{Path, PathBuf};

/// Results delivered to home.
#[derive(Debug)]
pub enum HomeMessage {
    /// Every review of the repository (archived included).
    Listed(Result<Vec<ReviewWorkspaceListItem>, String>),
    /// Repository state for suggestions.
    State(Result<ReviewRepoStateResponse, String>),
    /// Pull requests (`Ok(None)`: no provider lists them).
    Pulls(Result<Option<PullsAnswer>, String>),
    /// The stack containing the current branch's pull request.
    Stack(Result<ReviewStack, String>),
    /// Stats of one review.
    Stats {
        /// Workspace id.
        id: String,
        /// Stats, or the failure message.
        result: Result<WorkspaceStats, String>,
    },
    /// A rename, archive, or restore finished.
    Changed {
        /// What was done (for the toast).
        done: String,
        /// Failure message.
        result: Result<(), String>,
    },
}

/// List every review of `repo`.
///
/// # Errors
///
/// Returns the service failure message.
pub async fn list(
    backend: SharedBackend,
    repo: PathBuf,
) -> Result<Vec<ReviewWorkspaceListItem>, String> {
    let response: ListReviewWorkspacesResponse = call_json(
        backend.as_ref(),
        OP_REVIEW_WORKSPACE_LIST,
        &ListReviewWorkspacesRequest {
            repo_path: repo,
            include_archived: true,
        },
    )
    .await
    .map_err(|error| error.message)?;
    // Older services answer without items: count nothing rather than dropping reviews.
    if response.items.is_empty() && !response.workspaces.is_empty() {
        return Ok(response
            .workspaces
            .into_iter()
            .map(|workspace| ReviewWorkspaceListItem {
                workspace,
                thread_count: 0,
                draft_count: 0,
                last_publish: None,
            })
            .collect());
    }
    Ok(response.items)
}

/// Summarize the repository's state.
///
/// # Errors
///
/// Returns the service failure message.
pub async fn state(
    backend: SharedBackend,
    repo: PathBuf,
) -> Result<ReviewRepoStateResponse, String> {
    call_json(
        backend.as_ref(),
        OP_REVIEW_REPO_STATE,
        &ReviewRepoStateRequest { repo_path: repo },
    )
    .await
    .map_err(|error| error.message)
}

/// The open pull requests from the first provider that lists them; `None` without one.
///
/// # Errors
///
/// Returns the provider's failure as a message.
pub async fn pulls(backend: SharedBackend, repo: PathBuf) -> Result<Option<PullsAnswer>, String> {
    let transport = BackendTransport(backend.as_ref());
    let routes = discover_providers(&transport)
        .await
        .map_err(|error| error.message)?;
    let Some(route) = routes
        .into_iter()
        .find(|route| route.manifest.capabilities.pulls_list)
    else {
        return Ok(None);
    };
    let listed = list_pulls(&transport, &route, &repo, None)
        .await
        .map_err(|error| provider_message(&route.manifest.label, &error))?;
    Ok(Some(PullsAnswer {
        route,
        repository: listed.repository,
        pulls: listed.pulls,
    }))
}

/// The stack containing pull request `number`.
///
/// # Errors
///
/// Returns the provider's failure as a message.
pub async fn stack(
    backend: SharedBackend,
    route: ProviderRoute,
    repo: PathBuf,
    repository: String,
    number: u64,
) -> Result<ReviewStack, String> {
    let transport = BackendTransport(backend.as_ref());
    resolve_stack(&transport, &route, &repo, Some(repository), number)
        .await
        .map(|response| response.stack)
        .map_err(|error| provider_message(&route.manifest.label, &error))
}

/// Stats of one review: its sources resolved without asking providers (no network), each
/// resolved source's index totals (one entry page each), and whether a commit head moved since
/// the recorded pass.
///
/// # Errors
///
/// Returns the service failure message.
pub async fn stats(
    backend: SharedBackend,
    workspace: ReviewWorkspace,
) -> Result<WorkspaceStats, String> {
    let repo = workspace.repo_root.clone();
    let resolved: ResolveReviewSourcesResponse = call_json(
        backend.as_ref(),
        OP_REVIEW_SOURCES_RESOLVE,
        &ResolveReviewSourcesRequest {
            repo_path: repo.clone(),
            sources: workspace.sources.clone(),
            provider_resolutions: Vec::new(),
        },
    )
    .await
    .map_err(|error| error.message)?;
    let mut stats = WorkspaceStats {
        unresolved: resolved.diagnostics.len(),
        ..WorkspaceStats::default()
    };
    let mut seen = std::collections::BTreeSet::new();
    for source in &resolved.sources {
        // Several parts of one source record their first part's head.
        let first = seen.insert(source.source_id.clone());
        if first
            && let (Some(recorded), Some(head)) = (
                workspace
                    .reviewed_revisions
                    .get(&source.source_id)
                    .and_then(|revision| revision.reviewed_head_oid.as_deref()),
                source.head.commit_oid(),
            )
            && recorded != head
        {
            stats.new_commits = true;
        }
        let index = totals(&backend, &repo, source.clone()).await?;
        stats.files += u64::from(index.total_entries);
        stats.additions += index.total_additions;
        stats.deletions += index.total_deletions;
    }
    Ok(stats)
}

async fn totals(
    backend: &SharedBackend,
    repo: &Path,
    source: bcode_code_review_models::ResolvedSource,
) -> Result<ReviewIndexResponse, String> {
    call_json(
        backend.as_ref(),
        OP_REVIEW_INDEX_GET,
        &ReviewIndexRequest {
            repo_path: repo.to_path_buf(),
            source,
            cursor: None,
            limit: Some(1),
        },
    )
    .await
    .map_err(|error| error.message)
}

/// Save `workspace` (rename, restore).
///
/// # Errors
///
/// Returns the service failure message.
pub async fn update(backend: SharedBackend, workspace: ReviewWorkspace) -> Result<(), String> {
    let _: UpdateReviewWorkspaceResponse = call_json(
        backend.as_ref(),
        OP_REVIEW_WORKSPACE_UPDATE,
        &UpdateReviewWorkspaceRequest {
            repo_path: workspace.repo_root.clone(),
            workspace,
        },
    )
    .await
    .map_err(|error| error.message)?;
    Ok(())
}

/// Archive one review.
///
/// # Errors
///
/// Returns the service failure message, or a message when nothing was archived.
pub async fn archive(backend: SharedBackend, repo: PathBuf, id: String) -> Result<(), String> {
    let response: ArchiveReviewWorkspaceResponse = call_json(
        backend.as_ref(),
        OP_REVIEW_WORKSPACE_ARCHIVE,
        &ArchiveReviewWorkspaceRequest {
            repo_path: repo,
            workspace_id: id,
        },
    )
    .await
    .map_err(|error| error.message)?;
    if response.archived {
        Ok(())
    } else {
        Err("the review no longer exists".to_owned())
    }
}
