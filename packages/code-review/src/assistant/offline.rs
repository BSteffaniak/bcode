//! Resolving a saved review's pull request and stack sources without asking their provider.
//!
//! A review tool may act on any saved review, not only the one a session is attached to. Pull
//! request and stack sources normally resolve through their review-source provider, which the
//! review app calls (network and `git fetch`). Tools resolve them from what is already known
//! instead:
//!
//! * a stack layer or whole stack: the stack the review last opened with (its
//!   [`ReviewStackSnapshot`](bcode_code_review_models::ReviewStackSnapshot)), else the stack the
//!   provider last reported (the on-disk cache, when the registry has one);
//! * a pull request: the pull request the provider last reported.
//!
//! Each pull request's head is its recorded head commit and the bottom layer's base is its
//! recorded base commit, or else the base branch as the repository has it (`origin/<branch>`,
//! then `<branch>`). The commits must be in the local repository; when they are not, the source
//! resolves to the usual "not in the local repository" diagnostic, so the tool says the review
//! must be opened in the app first.

use crate::provider::ProviderRegistry;
use bcode_code_review_models::{
    PullRequestSummary, ReviewPullFetchResult, ReviewSourceKind, ReviewSourceProviderOutcome,
    ReviewSourceProviderResolution, ReviewStack, ReviewWorkspace,
};
use std::path::Path;

/// Answers for every included pull request and stack source of `workspace` that can be resolved
/// from what is known.
#[must_use]
pub fn resolutions(
    workspace: &ReviewWorkspace,
    registry: Option<&ProviderRegistry>,
) -> Vec<ReviewSourceProviderResolution> {
    let repo = workspace.repo_root.as_path();
    workspace
        .sources
        .iter()
        .filter(|source| source.included)
        .filter_map(|source| {
            let outcome = match &source.kind {
                ReviewSourceKind::StackLayer {
                    provider,
                    repository,
                    number,
                }
                | ReviewSourceKind::Stack {
                    provider,
                    repository,
                    top: number,
                } => {
                    let stack = known_stack(workspace, registry, provider, repository, *number)?;
                    let layers = stack.layers_through(*number)?;
                    let fetched = layers
                        .iter()
                        .enumerate()
                        .map(|(index, layer)| {
                            fetch_result(repo, &layer.pull, index == 0 || layer.parent.is_none())
                        })
                        .collect::<Option<Vec<_>>>()?;
                    ReviewSourceProviderOutcome::Resolved {
                        stack: Some(stack),
                        fetched,
                    }
                }
                ReviewSourceKind::PullRequest {
                    provider,
                    repository,
                    number,
                } => {
                    let pull = registry?.known_pull(provider, repository, *number)?;
                    ReviewSourceProviderOutcome::Resolved {
                        stack: None,
                        fetched: vec![fetch_result(repo, &pull, true)?],
                    }
                }
                _ => return None,
            };
            Some(ReviewSourceProviderResolution {
                source_id: source.id.clone(),
                outcome,
            })
        })
        .collect()
}

fn known_stack(
    workspace: &ReviewWorkspace,
    registry: Option<&ProviderRegistry>,
    provider: &str,
    repository: &str,
    number: u64,
) -> Option<ReviewStack> {
    workspace
        .stack_snapshots
        .iter()
        .filter(|snapshot| snapshot.provider == provider && snapshot.repository == repository)
        .filter(|snapshot| snapshot.stack.layer(number).is_some())
        .max_by_key(|snapshot| snapshot.confirmed_at_ms)
        .map(|snapshot| snapshot.stack.clone())
        .or_else(|| registry?.known_stack(provider, repository, number))
}

/// A fetch result from a recorded pull request; the base commit is needed for the bottom layer
/// (and a lone pull request) only.
fn fetch_result(
    repo: &Path,
    pull: &PullRequestSummary,
    needs_base: bool,
) -> Option<ReviewPullFetchResult> {
    if pull.head_oid.is_empty() {
        return None;
    }
    let base_oid = match pull.base_oid.clone().filter(|oid| !oid.is_empty()) {
        Some(oid) => oid,
        None if needs_base => local_branch(repo, &pull.base_branch)?,
        None => String::new(),
    };
    Some(ReviewPullFetchResult {
        number: pull.number,
        head_ref: String::new(),
        head_oid: pull.head_oid.clone(),
        base_branch: pull.base_branch.clone(),
        base_ref: String::new(),
        base_oid,
    })
}

/// The commit `branch` names locally: its remote-tracking branch, else the branch.
fn local_branch(repo: &Path, branch: &str) -> Option<String> {
    if branch.is_empty() || branch.starts_with('-') {
        return None;
    }
    [
        format!("refs/remotes/origin/{branch}"),
        format!("refs/heads/{branch}"),
    ]
    .iter()
    .find_map(|reference| {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "--verify", "--quiet"])
            .arg(format!("{reference}^{{commit}}"))
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            .filter(|oid| !oid.is_empty())
    })
}
