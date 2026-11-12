//! Home's service calls. Each runs on a host-spawned task and reports one message; none blocks
//! input, and each is bounded (one listing, one repository summary, one provider listing, one
//! stack, and per-review stats of a capped number of reviews).

use super::model::{PullsAnswer, WorkspaceStats};
use crate::entry::provider_message;
use crate::services::ReviewServices;
use bcode_code_review::provider::{ProviderRoute, list_pulls, lookup_pulls};
use bcode_code_review_models::{
    ArchiveReviewWorkspaceRequest, ArchiveReviewWorkspaceResponse, ListReviewWorkspacesRequest,
    ListReviewWorkspacesResponse, ResolveReviewSourcesRequest, ResolveReviewSourcesResponse,
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
    /// The open pull requests whose head is the current branch, looked up because the recent
    /// list did not show one.
    OwnPull(Result<Vec<bcode_code_review_models::PullRequestSummary>, String>),
    /// The stack containing the current branch's pull request.
    Stack(Result<ReviewStack, String>),
    /// The current branch's stack as discovery sees it, unconfirmed.
    Provisional(Vec<bcode_code_review::discovery::ProvisionalLayer>),
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
    services: ReviewServices,
    repo: PathBuf,
) -> Result<Vec<ReviewWorkspaceListItem>, String> {
    let response: ListReviewWorkspacesResponse = services
        .call(ListReviewWorkspacesRequest {
            repo_path: repo,
            include_archived: true,
        })
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
    services: ReviewServices,
    repo: PathBuf,
) -> Result<ReviewRepoStateResponse, String> {
    services
        .call(ReviewRepoStateRequest { repo_path: repo })
        .await
        .map_err(|error| error.message)
}

/// The open pull requests from the first provider that lists them; `None` without one.
///
/// # Errors
///
/// Returns the provider's failure as a message.
pub async fn pulls(services: ReviewServices, repo: PathBuf) -> Result<Option<PullsAnswer>, String> {
    let Some(route) = services
        .providers()
        .sources()
        .iter()
        .find(|route| route.manifest.capabilities.pulls_list)
        .cloned()
    else {
        return Ok(None);
    };
    let (registry, asked) = (services.providers(), route.clone());
    let listed = services
        .blocking(move || list_pulls(&registry, &asked, &repo, None))
        .await
        .map_err(|error| provider_message(&route.manifest.label, &error))?;
    Ok(Some(PullsAnswer {
        route,
        repository: listed.repository,
        pulls: listed.pulls,
    }))
}

/// The open pull requests `query` names (a number, URL, or exact head branch) in `repository`.
///
/// # Errors
///
/// Returns the provider's failure as a message.
pub async fn lookup(
    services: ReviewServices,
    route: ProviderRoute,
    repo: PathBuf,
    repository: String,
    query: String,
) -> Result<Vec<bcode_code_review_models::PullRequestSummary>, String> {
    let (label, registry) = (route.manifest.label.clone(), services.providers());
    services
        .blocking(move || lookup_pulls(&registry, &route, &repo, Some(repository), &query))
        .await
        .map(|listed| listed.pulls)
        .map_err(|error| provider_message(&label, &error))
}

/// The stack through branch `head` as the discovery contributors see it, before any provider
/// confirms it (see [`bcode_code_review::discovery::provisional_stack`]).
pub async fn provisional(
    services: ReviewServices,
    repo: PathBuf,
    head: String,
) -> Vec<bcode_code_review::discovery::ProvisionalLayer> {
    let registry = services.providers();
    services
        .blocking(move || {
            Ok::<_, bcode_code_review::provider::ProviderCallError>(
                bcode_code_review::discovery::provisional_stack(&registry, &repo, &head),
            )
        })
        .await
        .unwrap_or_default()
}

/// The stack containing pull request `number` (whose head branch is `head_branch`, when known),
/// with discovery's hints and warnings.
///
/// # Errors
///
/// Returns the provider's failure as a message.
pub async fn stack(
    services: ReviewServices,
    route: ProviderRoute,
    repo: PathBuf,
    repository: String,
    number: u64,
    head_branch: Option<String>,
) -> Result<ReviewStack, String> {
    let label = route.manifest.label.clone();
    let registry = services.providers();
    let max_layers = services.stack_limit().resolve_limit();
    services
        .blocking(move || {
            bcode_code_review::discovery::resolve_stack_with_discovery(
                &registry,
                &route,
                &repo,
                Some(repository),
                number,
                max_layers,
                head_branch,
            )
        })
        .await
        .map(|response| response.stack)
        .map_err(|error| provider_message(&label, &error))
}

/// Stats of one review: its sources resolved without asking providers (no network), each
/// resolved source's index totals (one entry page each), and whether a commit head moved since
/// the recorded pass.
///
/// # Errors
///
/// Returns the service failure message.
pub async fn stats(
    services: ReviewServices,
    workspace: ReviewWorkspace,
) -> Result<WorkspaceStats, String> {
    let repo = workspace.repo_root.clone();
    let resolved: ResolveReviewSourcesResponse = services
        .call(ResolveReviewSourcesRequest {
            repo_path: repo.clone(),
            sources: workspace.sources.clone(),
            provider_resolutions: Vec::new(),
        })
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
        let index = totals(&services, &repo, source.clone()).await?;
        stats.files += u64::from(index.total_entries);
        stats.additions += index.total_additions;
        stats.deletions += index.total_deletions;
    }
    Ok(stats)
}

async fn totals(
    services: &ReviewServices,
    repo: &Path,
    source: bcode_code_review_models::ResolvedSource,
) -> Result<ReviewIndexResponse, String> {
    services
        .call(ReviewIndexRequest {
            repo_path: repo.to_path_buf(),
            source,
            cursor: None,
            limit: Some(1),
        })
        .await
        .map_err(|error| error.message)
}

/// Save `workspace` (rename, restore).
///
/// # Errors
///
/// Returns the service failure message.
pub async fn update(services: ReviewServices, workspace: ReviewWorkspace) -> Result<(), String> {
    let _: UpdateReviewWorkspaceResponse = services
        .call(UpdateReviewWorkspaceRequest {
            repo_path: workspace.repo_root.clone(),
            workspace,
        })
        .await
        .map_err(|error| error.message)?;
    Ok(())
}

/// Archive one review.
///
/// # Errors
///
/// Returns the service failure message, or a message when nothing was archived.
pub async fn archive(services: ReviewServices, repo: PathBuf, id: String) -> Result<(), String> {
    let response: ArchiveReviewWorkspaceResponse = services
        .call(ArchiveReviewWorkspaceRequest {
            repo_path: repo,
            workspace_id: id,
        })
        .await
        .map_err(|error| error.message)?;
    if response.archived {
        Ok(())
    } else {
        Err("the review no longer exists".to_owned())
    }
}
