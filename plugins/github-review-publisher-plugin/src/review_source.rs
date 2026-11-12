//! GitHub review-source provider (`bcode.review_source/v1`).
//!
//! Lists open pull requests through `bcode_forge`, detects stacks of pull requests by walking
//! their base/head branch relationships on GitHub (Graphite-style and GitHub native stacked pull
//! requests; no tool-specific metadata), and fetches a pull request's head and base commits into a
//! review-owned reference namespace (`refs/bcode-review/pull/<N>/{head,base}`) so they never
//! touch the user's branches or remote-tracking references.

use bcode_code_review_models::{
    OP_REVIEW_SOURCE_MANIFEST, OP_REVIEW_SOURCE_PULL_FETCH, OP_REVIEW_SOURCE_PULLS_LIST,
    OP_REVIEW_SOURCE_STACK_RESOLVE, PullRequestSummary, REVIEW_SOURCE_ERROR_FAILED,
    REVIEW_SOURCE_ERROR_FETCH_FAILED, REVIEW_SOURCE_ERROR_PULL_NOT_FOUND,
    REVIEW_SOURCE_ERROR_UNAUTHENTICATED, REVIEW_SOURCE_ERROR_UNSUPPORTED_REPOSITORY,
    ReviewPullFetchResult, ReviewSourceCapabilities, ReviewSourceProviderManifest,
    ReviewSourcePullFetchRequest, ReviewSourcePullsListRequest, ReviewSourcePullsListResponse,
    ReviewSourceStackResolveRequest, ReviewSourceStackResolveResponse, ReviewStack,
    ReviewStackLayer, ReviewStackSibling,
};
use bcode_forge::models::{ForgeError, PullRequest, PullRequestList, RepositoryRef};
use bcode_plugin_sdk::prelude::{ServiceRequest, ServiceResponse};
use serde::de::DeserializeOwned;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::{Command, Stdio};
use thiserror::Error;

/// Provider id stored in pull-request source kinds.
pub const PROVIDER_ID: &str = "github";
/// Reference namespace owned by code review for fetched pull requests.
pub const FETCH_REF_NAMESPACE: &str = "refs/bcode-review/pull";

/// Errors of the review-source operations; each maps to a stable service error code.
#[derive(Debug, Error)]
pub enum ReviewSourceError {
    /// The request was malformed.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// The repository is not on a GitHub host.
    #[error("{0}")]
    UnsupportedRepository(String),
    /// No GitHub credentials were found.
    #[error("{0}")]
    Unauthenticated(String),
    /// The pull request is not open in the repository.
    #[error("pull request #{0} is not open in {1}")]
    PullNotFound(u64, String),
    /// `git fetch` or reading the fetched references failed.
    #[error("fetch failed: {0}")]
    Fetch(String),
    /// GitHub could not be reached or answered with an error.
    #[error("{0}")]
    Failed(String),
}

impl ReviewSourceError {
    /// Stable service error code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidRequest(_) => "invalid_request",
            Self::UnsupportedRepository(_) => REVIEW_SOURCE_ERROR_UNSUPPORTED_REPOSITORY,
            Self::Unauthenticated(_) => REVIEW_SOURCE_ERROR_UNAUTHENTICATED,
            Self::PullNotFound(..) => REVIEW_SOURCE_ERROR_PULL_NOT_FOUND,
            Self::Fetch(_) => REVIEW_SOURCE_ERROR_FETCH_FAILED,
            Self::Failed(_) => REVIEW_SOURCE_ERROR_FAILED,
        }
    }
}

impl From<ForgeError> for ReviewSourceError {
    fn from(error: ForgeError) -> Self {
        match error {
            ForgeError::UnsupportedHost(_) => Self::UnsupportedRepository(error.to_string()),
            ForgeError::Unauthenticated(message) => Self::Unauthenticated(message),
            ForgeError::Failed(message) => Self::Failed(message),
        }
    }
}

/// The provider manifest.
#[must_use]
pub fn manifest() -> ReviewSourceProviderManifest {
    ReviewSourceProviderManifest {
        id: PROVIDER_ID.to_string(),
        label: "GitHub".to_string(),
        capabilities: ReviewSourceCapabilities {
            pulls_list: true,
            stack_resolve: true,
            pull_fetch: true,
        },
    }
}

/// Handle one `bcode.review_source/v1` operation.
#[must_use]
pub fn invoke(request: &ServiceRequest) -> ServiceResponse {
    match request.operation.as_str() {
        OP_REVIEW_SOURCE_MANIFEST => crate::json_response(&manifest()),
        OP_REVIEW_SOURCE_PULLS_LIST => handle(request, |req: ReviewSourcePullsListRequest| {
            let repository = Repository::resolve(&req.repo_path, req.repository.as_deref())?;
            let list = repository
                .github(&req.repo_path)
                .open_pull_requests(&repository.0)?;
            Ok(pulls_list_response(&repository, list))
        }),
        OP_REVIEW_SOURCE_STACK_RESOLVE => {
            handle(request, |req: ReviewSourceStackResolveRequest| {
                let repository = Repository::resolve(&req.repo_path, req.repository.as_deref())?;
                let stack = resolve_stack(&repository, &req.repo_path, req.number, req.max_layers)?;
                Ok(ReviewSourceStackResolveResponse {
                    repository: repository.display(),
                    stack,
                })
            })
        }
        OP_REVIEW_SOURCE_PULL_FETCH => handle(request, |req: ReviewSourcePullFetchRequest| {
            let repository = Repository::resolve(&req.repo_path, req.repository.as_deref())?;
            let base_branch = match req.base_branch {
                Some(base) => base,
                None => repository
                    .github(&req.repo_path)
                    .pull_request(&repository.0, req.number)?
                    .pull_request
                    .map(|pull| pull.base)
                    .ok_or_else(|| {
                        ReviewSourceError::PullNotFound(req.number, repository.display())
                    })?,
            };
            let remote = repository.fetch_remote(&req.repo_path);
            fetch_pull(&req.repo_path, &remote, req.number, &base_branch)
        }),
        _ => ServiceResponse::error(
            "unsupported_operation",
            "unsupported GitHub review-source operation",
        ),
    }
}

fn handle<Req, Resp>(
    request: &ServiceRequest,
    operation: impl FnOnce(Req) -> Result<Resp, ReviewSourceError>,
) -> ServiceResponse
where
    Req: DeserializeOwned,
    Resp: serde::Serialize,
{
    let payload = match serde_json::from_slice::<Req>(&request.payload) {
        Ok(payload) => payload,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    match operation(payload) {
        Ok(response) => crate::json_response(&response),
        Err(error) => ServiceResponse::error(error.code(), error.to_string()),
    }
}

/// A GitHub repository named by a review-source request.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Repository(RepositoryRef);

impl Repository {
    /// Parse an explicit identifier (`owner/name`, or `host/owner/name` for GitHub Enterprise),
    /// or detect the repository from the local remotes (`origin` first).
    fn resolve(repo_path: &Path, identifier: Option<&str>) -> Result<Self, ReviewSourceError> {
        if let Some(identifier) = identifier {
            return parse_repository(identifier).map(Self).ok_or_else(|| {
                ReviewSourceError::InvalidRequest(format!(
                    "repository must be owner/name or host/owner/name, not {identifier:?}"
                ))
            });
        }
        remotes(repo_path)
            .into_iter()
            .find_map(|(_, url)| RepositoryRef::from_remote_url(&url))
            .map(Self)
            .ok_or_else(|| {
                ReviewSourceError::UnsupportedRepository(
                    "no git remote names a GitHub repository; pass the repository explicitly"
                        .to_string(),
                )
            })
    }

    /// `owner/name` on github.com, `host/owner/name` elsewhere.
    fn display(&self) -> String {
        if self.0.host == bcode_forge::GITHUB_HOST {
            self.0.full_name()
        } else {
            format!("{}/{}", self.0.host, self.0.full_name())
        }
    }

    /// A forge client for this repository's host, running `gh` in `repo_path`.
    fn github(&self, repo_path: &Path) -> bcode_forge::GitHub {
        bcode_forge::GitHub::new(repo_path).enterprise_hosts([self.0.host.clone()])
    }

    /// The local remote that points at this repository (`origin` preferred), or its HTTPS URL.
    fn fetch_remote(&self, repo_path: &Path) -> String {
        remotes(repo_path)
            .into_iter()
            .find(|(_, url)| {
                RepositoryRef::from_remote_url(url).is_some_and(|remote| {
                    remote.host == self.0.host
                        && remote.owner.eq_ignore_ascii_case(&self.0.owner)
                        && remote.name.eq_ignore_ascii_case(&self.0.name)
                })
            })
            .map_or_else(
                || format!("https://{}/{}.git", self.0.host, self.0.full_name()),
                |(name, _)| name,
            )
    }
}

fn parse_repository(identifier: &str) -> Option<RepositoryRef> {
    let parts: Vec<&str> = identifier.trim().trim_matches('/').split('/').collect();
    let valid = |part: &&str| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    if !parts.iter().all(valid) {
        return None;
    }
    let (host, owner, name) = match parts.as_slice() {
        [owner, name] => (bcode_forge::GITHUB_HOST, *owner, *name),
        [host, owner, name] => (*host, *owner, *name),
        _ => return None,
    };
    Some(RepositoryRef {
        host: host.to_ascii_lowercase(),
        owner: owner.to_string(),
        name: name.strip_suffix(".git").unwrap_or(name).to_string(),
    })
}

/// Local remotes and their fetch URLs, `origin` first.
fn remotes(repo_path: &Path) -> Vec<(String, String)> {
    let Ok(names) = git(repo_path, &["remote"]) else {
        return Vec::new();
    };
    let mut remotes: Vec<(String, String)> = names
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .filter_map(|name| {
            let url = git(repo_path, &["remote", "get-url", "--", name]).ok()?;
            Some((name.to_string(), url.trim().to_string()))
        })
        .collect();
    remotes.sort_by_key(|(name, _)| name != "origin");
    remotes
}

/// Convert a forge pull request to a provider-neutral summary.
#[must_use]
pub fn summary(pull: &PullRequest) -> PullRequestSummary {
    PullRequestSummary {
        number: pull.number,
        title: pull.title.clone(),
        author: pull.author.clone(),
        base_branch: pull.base.clone(),
        head_branch: pull.head.clone(),
        head_oid: pull.head_oid.clone(),
        base_oid: pull.base_oid.clone(),
        review_requested_from_me: pull.review_requested_from_viewer,
        draft: matches!(pull.state, bcode_forge::models::PullRequestState::Draft),
        updated: Some(pull.updated_at.clone()).filter(|updated| !updated.is_empty()),
        url: Some(pull.url.clone()).filter(|url| !url.is_empty()),
    }
}

/// Open pull requests, review requested from me first, then most recently updated.
fn pulls_list_response(
    repository: &Repository,
    list: PullRequestList,
) -> ReviewSourcePullsListResponse {
    let mut pulls: Vec<PullRequestSummary> = list
        .pull_requests
        .iter()
        .filter(|pull| pull.state.is_open())
        .map(summary)
        .collect();
    pulls.sort_by(|left, right| {
        right
            .review_requested_from_me
            .cmp(&left.review_requested_from_me)
            .then_with(|| right.updated.cmp(&left.updated))
            .then_with(|| right.number.cmp(&left.number))
    });
    ReviewSourcePullsListResponse {
        repository: repository.display(),
        default_branch: list.default_branch,
        pulls,
    }
}

/// Open pull requests whose head branch lives in the repository itself. A fork's head branch
/// names a branch in another repository, so it cannot be a stack layer's base.
fn stack_candidates(repository: &Repository, pulls: &[PullRequest]) -> Vec<PullRequestSummary> {
    let full_name = repository.0.full_name();
    pulls
        .iter()
        .filter(|pull| pull.state.is_open())
        .filter(|pull| {
            pull.head_repository
                .as_deref()
                .is_none_or(|head| head.eq_ignore_ascii_case(&full_name))
        })
        .map(summary)
        .collect()
}

/// Resolve the stack containing open pull request `number` by walking GitHub branch by branch:
/// one query for the pull request, then one per layer (see [`walk_stack`]). The cost follows the
/// stack's depth, not how many pull requests the repository has.
///
/// # Errors
///
/// * [`ReviewSourceError::PullNotFound`] when `number` is not an open pull request whose head
///   branch is in the repository.
/// * The forge's errors when GitHub cannot be asked or answers with an error.
fn resolve_stack(
    repository: &Repository,
    repo_path: &Path,
    number: u64,
    max_layers: Option<usize>,
) -> Result<ReviewStack, ReviewSourceError> {
    let github = repository.github(repo_path);
    let lookup = github.pull_request(&repository.0, number)?;
    let start = stack_candidates(repository, lookup.pull_request.as_slice())
        .pop()
        .ok_or_else(|| ReviewSourceError::PullNotFound(number, repository.display()))?;
    let mut branches = ForgeBranches {
        github: &github,
        repository,
    };
    walk_stack(
        &mut branches,
        &start,
        lookup.default_branch.as_deref(),
        max_layers,
    )
}

/// Open stack candidates next to one branch.
#[derive(Debug, Clone, Default)]
pub struct BranchNeighbors {
    /// Open pull requests whose head branch is the branch: candidates for the layer below one
    /// based on it.
    pub with_head: Vec<PullRequestSummary>,
    /// Open pull requests whose base branch is the branch: the layers on top of the one whose
    /// head it is.
    pub with_base: Vec<PullRequestSummary>,
}

/// Where a stack walk learns which open pull requests come from and go into a branch.
pub trait StackBranches {
    /// Why a branch could not be looked up.
    type Error;

    /// The open stack candidates (head branch in the repository) next to `branch`.
    ///
    /// # Errors
    ///
    /// Whatever stops the lookup; the walk stops with it rather than return a partial stack.
    fn neighbors(&mut self, branch: &str) -> Result<BranchNeighbors, Self::Error>;
}

/// [`StackBranches`] answered by GitHub, one query per branch.
struct ForgeBranches<'a> {
    github: &'a bcode_forge::GitHub,
    repository: &'a Repository,
}

impl StackBranches for ForgeBranches<'_> {
    type Error = ReviewSourceError;

    fn neighbors(&mut self, branch: &str) -> Result<BranchNeighbors, ReviewSourceError> {
        let pulls = self
            .github
            .branch_pull_requests(&self.repository.0, branch)?;
        Ok(BranchNeighbors {
            with_head: stack_candidates(self.repository, &pulls.with_head),
            with_base: stack_candidates(self.repository, &pulls.with_base),
        })
    }
}

/// Walk the linear stack containing `start`, asking `branches` about each branch once.
///
/// A stack is a chain where each pull request's base branch is the head branch of the one below
/// it, ending at `default_branch`. From `start`, the chain extends down while some pull
/// request's head branch is the current base branch, and up while exactly one pull request
/// targets the current head branch. The default branch itself is never looked up (it may have
/// thousands of pull requests), so a stack of `n` layers costs about `n` lookups.
///
/// * A missing layer (no open pull request for a base branch other than the default branch)
///   ends the chain early; the stack reports `rooted_at_default_branch: false` and a warning.
/// * Branching: when several pull requests target the same layer, the chain does not continue
///   past it; the other pull requests are reported as siblings. Below `start`, when several
///   open pull requests share a head branch, the most recently updated one is used.
/// * Cycles are guarded: the chain stops at the first repeated pull request, with a warning.
/// * With `max_layers`, chains are cut at that many layers with a warning: below `start` first,
///   then above it. Without it the whole stack is walked (cycles still end the walk).
///
/// # Errors
///
/// The first lookup error; no partial stack is returned.
pub fn walk_stack<B: StackBranches>(
    branches: &mut B,
    start: &PullRequestSummary,
    default_branch: Option<&str>,
    max_layers: Option<usize>,
) -> Result<ReviewStack, B::Error> {
    let mut walk = StackWalk {
        branches,
        neighbors: BTreeMap::new(),
        default_branch,
        max_layers,
        cut_above: None,
        visited: BTreeSet::from([start.number]),
        warnings: Vec::new(),
    };
    let mut chain = walk.below(start)?;
    chain.reverse();
    chain.push(start.clone());
    walk.extend_up(&mut chain)?;
    let siblings = walk.siblings(&chain)?;

    let trunk = chain[0].base_branch.clone();
    let parents: Vec<Option<u64>> = std::iter::once(None)
        .chain(chain.iter().map(|pull| Some(pull.number)))
        .collect();
    let layers = chain
        .into_iter()
        .zip(parents)
        .map(|(pull, parent)| ReviewStackLayer { pull, parent })
        .collect();
    Ok(ReviewStack {
        rooted_at_default_branch: default_branch == Some(trunk.as_str()),
        trunk,
        layers,
        requested: start.number,
        siblings,
        warnings: walk.warnings,
    })
}

/// The walk's lookups (each branch asked once) and bookkeeping.
struct StackWalk<'w, B: StackBranches> {
    branches: &'w mut B,
    neighbors: BTreeMap<String, BranchNeighbors>,
    default_branch: Option<&'w str>,
    max_layers: Option<usize>,
    /// The layer the limit left out above the chain's top: the chain's continuation, not a
    /// sibling.
    cut_above: Option<u64>,
    visited: BTreeSet<u64>,
    warnings: Vec<String>,
}

impl<B: StackBranches> StackWalk<'_, B> {
    fn neighbors(&mut self, branch: &str) -> Result<&BranchNeighbors, B::Error> {
        if !self.neighbors.contains_key(branch) {
            let found = self.branches.neighbors(branch)?;
            self.neighbors.insert(branch.to_owned(), found);
        }
        Ok(&self.neighbors[branch])
    }

    /// Open pull requests whose head branch is `pull`'s base branch.
    fn parents(&mut self, pull: &PullRequestSummary) -> Result<Vec<PullRequestSummary>, B::Error> {
        Ok(self.neighbors(&pull.base_branch)?.with_head.clone())
    }

    /// Open pull requests whose base branch is `pull`'s head branch.
    fn children(&mut self, pull: &PullRequestSummary) -> Result<Vec<PullRequestSummary>, B::Error> {
        Ok(self.neighbors(&pull.head_branch)?.with_base.clone())
    }

    /// Record a cycle when one of `candidates` is already in the chain.
    fn cycle(&mut self, candidates: &[PullRequestSummary]) -> bool {
        let Some(repeated) = candidates
            .iter()
            .find(|pull| self.visited.contains(&pull.number))
        else {
            return false;
        };
        self.warnings.push(format!(
            "pull requests form a cycle at #{}; the stack stops there",
            repeated.number
        ));
        true
    }

    /// The layers below `start`, nearest first.
    fn below(&mut self, start: &PullRequestSummary) -> Result<Vec<PullRequestSummary>, B::Error> {
        let mut below: Vec<PullRequestSummary> = Vec::new();
        let mut current = start.clone();
        while self.default_branch != Some(current.base_branch.as_str()) {
            if let Some(max) = self.max_layers.filter(|max| below.len() + 1 >= *max) {
                self.warnings.push(format!(
                    "the stack has more than {max} layers; lower layers are left out"
                ));
                break;
            }
            let candidates = self.parents(&current)?;
            if candidates.is_empty() {
                if let Some(default_branch) = self.default_branch {
                    self.warnings.push(format!(
                        "no open pull request has head branch {}; the stack is missing a layer \
                         between #{} and {default_branch}",
                        current.base_branch, current.number
                    ));
                }
                break;
            }
            if self.cycle(&candidates) {
                break;
            }
            let count = candidates.len();
            let Some(parent) = candidates.into_iter().max_by(|left, right| {
                left.updated
                    .cmp(&right.updated)
                    .then_with(|| left.number.cmp(&right.number))
            }) else {
                break;
            };
            if count > 1 {
                self.warnings.push(format!(
                    "several open pull requests have head branch {}; using #{}",
                    current.base_branch, parent.number
                ));
            }
            self.visited.insert(parent.number);
            below.push(parent.clone());
            current = parent;
        }
        Ok(below)
    }

    /// Extend `chain` upward while exactly one pull request targets its top.
    fn extend_up(&mut self, chain: &mut Vec<PullRequestSummary>) -> Result<(), B::Error> {
        while let Some(top) = chain.last() {
            let children = self.children(&top.clone())?;
            if self.cycle(&children) {
                break;
            }
            let [child] = children.as_slice() else {
                break;
            };
            if let Some(max) = self.max_layers.filter(|max| chain.len() >= *max) {
                self.warnings.push(format!(
                    "the stack has more than {max} layers; upper layers are left out"
                ));
                self.cut_above = Some(child.number);
                break;
            }
            self.visited.insert(child.number);
            chain.push(child.clone());
        }
        Ok(())
    }

    /// Open pull requests targeting a layer of `chain` that are not part of it.
    fn siblings(
        &mut self,
        chain: &[PullRequestSummary],
    ) -> Result<Vec<ReviewStackSibling>, B::Error> {
        let mut in_chain: BTreeSet<u64> = chain.iter().map(|pull| pull.number).collect();
        in_chain.extend(self.cut_above);
        let mut siblings = Vec::new();
        for layer in chain {
            siblings.extend(
                self.children(layer)?
                    .into_iter()
                    .filter(|child| !in_chain.contains(&child.number))
                    .map(|pull| ReviewStackSibling {
                        parent: layer.number,
                        pull,
                    }),
            );
        }
        Ok(siblings)
    }
}

/// Fetch pull request `number`'s head and `base_branch` from `remote` into
/// `refs/bcode-review/pull/<number>/{head,base}` and return the fetched commits.
///
/// # Errors
///
/// * [`ReviewSourceError::InvalidRequest`] when `remote` or `base_branch` is not a safe name.
/// * [`ReviewSourceError::Fetch`] when `git fetch` fails or the references do not resolve.
pub fn fetch_pull(
    repo_path: &Path,
    remote: &str,
    number: u64,
    base_branch: &str,
) -> Result<ReviewPullFetchResult, ReviewSourceError> {
    if remote.is_empty() || remote.starts_with('-') || remote.chars().any(char::is_control) {
        return Err(ReviewSourceError::InvalidRequest(format!(
            "invalid remote {remote:?}"
        )));
    }
    let base_source = format!("refs/heads/{base_branch}");
    if base_branch.is_empty()
        || base_branch.starts_with('-')
        || git(repo_path, &["check-ref-format", &base_source]).is_err()
    {
        return Err(ReviewSourceError::InvalidRequest(format!(
            "invalid base branch {base_branch:?}"
        )));
    }
    let head_ref = format!("{FETCH_REF_NAMESPACE}/{number}/head");
    let base_ref = format!("{FETCH_REF_NAMESPACE}/{number}/base");
    let head_spec = format!("+refs/pull/{number}/head:{head_ref}");
    let base_spec = format!("+{base_source}:{base_ref}");
    git(
        repo_path,
        &[
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            // No opportunistic remote-tracking updates: only the review-owned refs change.
            "--refmap=",
            "--end-of-options",
            remote,
            &head_spec,
            &base_spec,
        ],
    )
    .map_err(ReviewSourceError::Fetch)?;
    let commit = |reference: &str| {
        git(
            repo_path,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{reference}^{{commit}}"),
            ],
        )
        .map(|oid| oid.trim().to_string())
        .map_err(|error| ReviewSourceError::Fetch(format!("{reference}: {error}")))
    };
    Ok(ReviewPullFetchResult {
        number,
        head_oid: commit(&head_ref)?,
        head_ref,
        base_branch: base_branch.to_string(),
        base_oid: commit(&base_ref)?,
        base_ref,
    })
}

fn git(repo_path: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("git could not start: {error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

#[cfg(test)]
mod tests;
