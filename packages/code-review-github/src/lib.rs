#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! GitHub as a code review source ([`GitHubReviewSource`], a [`ReviewSourceProvider`]).
//!
//! The review links this crate and calls it in-process; the GitHub Bcode plugin serves the same
//! provider as `bcode.review_source/v1` for agents and other plugins.
//!
//! Lists open pull requests through `bcode_forge`, detects stacks of pull requests by walking
//! their base/head branch relationships on GitHub (Graphite-style and GitHub native stacked pull
//! requests; no tool-specific metadata), and fetches a pull request's head and base commits into a
//! review-owned reference namespace (`refs/bcode-review/pull/<N>/{head,base}`) so they never
//! touch the user's branches or remote-tracking references.

use bcode_code_review_models::{
    PullRequestSummary, REVIEW_SOURCE_ERROR_FAILED, REVIEW_SOURCE_ERROR_FETCH_FAILED,
    REVIEW_SOURCE_ERROR_PULL_NOT_FOUND, REVIEW_SOURCE_ERROR_UNAUTHENTICATED,
    REVIEW_SOURCE_ERROR_UNSUPPORTED_REPOSITORY, ReviewPullFetchResult, ReviewPullFetchTarget,
    ReviewSourceBatchCapabilities, ReviewSourceCapabilities, ReviewSourceProviderManifest,
    ReviewSourcePullFetchRequest, ReviewSourcePullsFetchRequest, ReviewSourcePullsFetchResponse,
    ReviewSourcePullsGetRequest, ReviewSourcePullsGetResponse, ReviewSourcePullsListRequest,
    ReviewSourcePullsListResponse, ReviewSourceStackResolveRequest,
    ReviewSourceStackResolveResponse, ReviewStack, ReviewStackLayer, ReviewStackSibling,
};
use bcode_code_review_provider::{ProviderCallError, ProviderResult, ReviewSourceProvider};
use bcode_forge::models::{
    ForgeError, PullRequest, PullRequestList, PullRequestState, RepositoryRef,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
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
    /// The caller stopped wanting the answer.
    #[error("cancelled")]
    Cancelled,
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
            Self::Cancelled => bcode_code_review_models::REVIEW_SOURCE_ERROR_CANCELLED,
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
            batched: ReviewSourceBatchCapabilities {
                pulls_fetch: true,
                pulls_get: true,
                pulls_history: true,
            },
        },
    }
}

impl From<ReviewSourceError> for ProviderCallError {
    fn from(error: ReviewSourceError) -> Self {
        Self::new(error.code(), error.to_string())
    }
}

/// GitHub as a [`ReviewSourceProvider`]: pull requests through `bcode_forge` (the HTTPS API,
/// falling back to `gh`), commits through `git fetch` into review-owned references.
#[derive(Debug, Clone, Copy, Default)]
pub struct GitHubReviewSource;

impl ReviewSourceProvider for GitHubReviewSource {
    fn manifest(&self) -> ReviewSourceProviderManifest {
        manifest()
    }

    fn list_pulls(
        &self,
        request: &ReviewSourcePullsListRequest,
    ) -> ProviderResult<ReviewSourcePullsListResponse> {
        let repository = Repository::resolve(&request.repo_path, request.repository.as_deref())?;
        let github = repository.github(&request.repo_path);
        let list = match request.query.as_deref().map(str::trim) {
            Some(query) if !query.is_empty() => lookup_pulls(&github, &repository, query)?,
            _ => github
                .open_pull_requests(&repository.0)
                .map_err(ReviewSourceError::from)?,
        };
        Ok(pulls_list_response(&repository, list))
    }

    fn get_pulls(
        &self,
        request: &ReviewSourcePullsGetRequest,
    ) -> ProviderResult<ReviewSourcePullsGetResponse> {
        let repository = Repository::resolve(&request.repo_path, request.repository.as_deref())?;
        let github = repository.github(&request.repo_path);
        // The pull requests and the branches' children are asked for at once.
        let (found, children) = std::thread::scope(|scope| {
            let children = (!request.children_of.is_empty()).then(|| {
                scope.spawn(|| github.branches_pull_requests(&repository.0, &request.children_of))
            });
            let found = github.pull_requests_by_number(&repository.0, &request.numbers);
            let children = children.map(|worker| {
                worker.join().unwrap_or_else(|_| {
                    Err(ForgeError::Failed("a GitHub request panicked".to_owned()))
                })
            });
            (found, children.transpose())
        });
        let found = found.map_err(ReviewSourceError::from)?;
        let children = children
            .map_err(ReviewSourceError::from)?
            .unwrap_or_default();
        let mut response = pulls_get_response(&repository, &request.numbers, &found);
        response.children = children
            .into_iter()
            .map(|(branch, pulls)| (branch, stack_candidates(&repository, &pulls.with_base)))
            .collect();
        Ok(response)
    }

    fn get_history(
        &self,
        request: &bcode_code_review_models::ReviewSourcePullsHistoryRequest,
    ) -> ProviderResult<bcode_code_review_models::ReviewSourcePullsHistoryResponse> {
        let repository = Repository::resolve(&request.repo_path, request.repository.as_deref())?;
        let github = repository.github(&request.repo_path);
        let items = github
            .pull_request_histories(&repository.0, &request.numbers)
            .map_err(ReviewSourceError::from)?;
        let histories: Vec<bcode_code_review_models::PullHistory> = items
            .into_iter()
            .map(|(number, items)| bcode_code_review_models::PullHistory {
                number,
                events: history_events(items),
            })
            .collect();
        if request.fetch {
            let remote = repository.fetch_remote(&request.repo_path);
            fetch_versions(&request.repo_path, &remote, &histories);
        }
        Ok(bcode_code_review_models::ReviewSourcePullsHistoryResponse {
            repository: repository.display(),
            histories,
        })
    }

    fn resolve_stack(
        &self,
        request: &ReviewSourceStackResolveRequest,
    ) -> ProviderResult<ReviewSourceStackResolveResponse> {
        let repository = Repository::resolve(&request.repo_path, request.repository.as_deref())?;
        let stack = resolve_stack(
            &repository,
            &request.repo_path,
            request.number,
            request.max_layers,
            &request.hints,
        )?;
        Ok(ReviewSourceStackResolveResponse {
            repository: repository.display(),
            stack,
        })
    }

    fn fetch_pull(
        &self,
        request: &ReviewSourcePullFetchRequest,
    ) -> ProviderResult<ReviewPullFetchResult> {
        let repository = Repository::resolve(&request.repo_path, request.repository.as_deref())?;
        let base_branch = match request.base_branch.clone() {
            Some(base) => base,
            None => repository
                .github(&request.repo_path)
                .pull_request(&repository.0, request.number)
                .map_err(ReviewSourceError::from)?
                .pull_request
                .map(|pull| pull.base)
                .ok_or_else(|| {
                    ReviewSourceError::PullNotFound(request.number, repository.display())
                })?,
        };
        let remote = repository.fetch_remote(&request.repo_path);
        Ok(fetch_pull(
            &request.repo_path,
            &remote,
            request.number,
            &base_branch,
        )?)
    }

    fn fetch_pulls(
        &self,
        request: &ReviewSourcePullsFetchRequest,
    ) -> ProviderResult<ReviewSourcePullsFetchResponse> {
        let repository = Repository::resolve(&request.repo_path, request.repository.as_deref())?;
        let remote = repository.fetch_remote(&request.repo_path);
        Ok(ReviewSourcePullsFetchResponse {
            fetched: fetch_pulls(&request.repo_path, &remote, &request.pulls)?,
        })
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

    /// A forge client for this repository's host, running `gh` in `repo_path`. Clients are kept
    /// for the plugin's lifetime per repository path and host, so successive operations (a
    /// lookup, a stack resolution, a fetch) reuse one token and connection.
    fn github(&self, repo_path: &Path) -> bcode_forge::GitHub {
        static CLIENTS: std::sync::OnceLock<
            std::sync::Mutex<BTreeMap<(std::path::PathBuf, String), bcode_forge::GitHub>>,
        > = std::sync::OnceLock::new();
        let mut clients = CLIENTS
            .get_or_init(std::sync::Mutex::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clients
            .entry((repo_path.to_path_buf(), self.0.host.clone()))
            .or_insert_with(|| {
                bcode_forge::GitHub::new(repo_path).enterprise_hosts([self.0.host.clone()])
            })
            .clone()
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

/// Each repository path's remotes as [`read_remotes`] answered, with the modification time of
/// the git config they were read from.
type RemotesByRepo = BTreeMap<std::path::PathBuf, (std::time::SystemTime, Vec<(String, String)>)>;

/// Local remotes and their fetch URLs, `origin` first. Every provider call needs them and reading
/// them takes two git processes, so they are kept per repository path until its git config (where
/// remotes are stored) changes.
fn remotes(repo_path: &Path) -> Vec<(String, String)> {
    static REMOTES: std::sync::OnceLock<std::sync::Mutex<RemotesByRepo>> =
        std::sync::OnceLock::new();
    let Some(modified) = git_config_path(repo_path)
        .and_then(|config| std::fs::metadata(config).ok())
        .and_then(|metadata| metadata.modified().ok())
    else {
        return read_remotes(repo_path);
    };
    let cache = REMOTES.get_or_init(std::sync::Mutex::default);
    let known = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(repo_path)
        .filter(|(read_at, _)| *read_at == modified)
        .map(|(_, remotes)| remotes.clone());
    if let Some(known) = known {
        return known;
    }
    let read = read_remotes(repo_path);
    cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(repo_path.to_path_buf(), (modified, read.clone()));
    read
}

/// The config file of the repository at `repo_path` (its work tree root): `.git/config`, or for a
/// linked worktree, the main repository's. `None` when the layout is not one of those.
fn git_config_path(repo_path: &Path) -> Option<std::path::PathBuf> {
    let dot_git = repo_path.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git.join("config"));
    }
    let pointer = std::fs::read_to_string(&dot_git).ok()?;
    let git_dir = repo_path.join(pointer.strip_prefix("gitdir:")?.trim());
    let common = std::fs::read_to_string(git_dir.join("commondir"))
        .map_or_else(|_| git_dir.clone(), |common| git_dir.join(common.trim()));
    Some(common.join("config"))
}

/// [`remotes`], read from git.
fn read_remotes(repo_path: &Path) -> Vec<(String, String)> {
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

/// What a pull request lookup names.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PullQuery<'a> {
    /// A pull request number.
    Number(u64),
    /// An exact head branch name.
    Branch(&'a str),
}

/// Read a lookup: `123`, `#123`, a pull request URL (`…/pull/123`, optionally followed by more
/// path), or else a branch name.
fn parse_pull_query(query: &str) -> PullQuery<'_> {
    let number = query.strip_prefix('#').unwrap_or(query);
    if let Ok(number) = number.parse::<u64>() {
        return PullQuery::Number(number);
    }
    if let Some((_, rest)) = query.split_once("/pull/")
        && let Some(number) = rest
            .split(['/', '?', '#'])
            .next()
            .and_then(|number| number.parse::<u64>().ok())
    {
        return PullQuery::Number(number);
    }
    PullQuery::Branch(query)
}

/// The `review_source.pulls.get` answer for `numbers` from what GitHub `found`: open pull
/// requests (drafts included) as summaries, everything else as not open.
fn pulls_get_response(
    repository: &Repository,
    numbers: &[u64],
    found: &BTreeMap<u64, Option<PullRequest>>,
) -> ReviewSourcePullsGetResponse {
    let mut response = ReviewSourcePullsGetResponse {
        repository: repository.display(),
        pulls: Vec::new(),
        not_open: Vec::new(),
        children: BTreeMap::new(),
    };
    for number in numbers {
        match found.get(number).and_then(Option::as_ref) {
            Some(pull)
                if matches!(pull.state, PullRequestState::Open | PullRequestState::Draft) =>
            {
                response.pulls.push(summary(pull));
            }
            _ => response.not_open.push(*number),
        }
    }
    response
}

/// Milliseconds since the Unix epoch of an RFC 3339 time (as GitHub writes them).
fn millis(time: &str) -> Option<u64> {
    let parsed = chrono::DateTime::parse_from_rfc3339(time).ok()?;
    u64::try_from(parsed.timestamp_millis()).ok()
}

/// GitHub's timeline items as push events, oldest first; items without a readable time are
/// left out.
fn history_events(
    items: Vec<bcode_forge::HistoryItem>,
) -> Vec<bcode_code_review_models::PullHistoryEvent> {
    use bcode_code_review_models::PullHistoryEvent;
    let mut events: Vec<PullHistoryEvent> = items
        .into_iter()
        .filter_map(|item| match item {
            bcode_forge::HistoryItem::ForcePushed {
                at,
                actor,
                before,
                after,
            } => Some(PullHistoryEvent::ForcePushed {
                at_ms: millis(&at)?,
                actor,
                before_oid: before,
                after_oid: after,
            }),
            bcode_forge::HistoryItem::Commit { oid, at } => Some(PullHistoryEvent::CommitsPushed {
                at_ms: millis(&at)?,
                head_oid: oid,
            }),
            bcode_forge::HistoryItem::BaseChanged { at, from, to } => {
                Some(PullHistoryEvent::BaseChanged {
                    at_ms: millis(&at)?,
                    from,
                    to,
                })
            }
        })
        .collect();
    events.sort_by_key(|event| match event {
        PullHistoryEvent::ForcePushed { at_ms, .. }
        | PullHistoryEvent::CommitsPushed { at_ms, .. }
        | PullHistoryEvent::BaseChanged { at_ms, .. } => *at_ms,
    });
    events
}

/// Reference namespace old versions' commits are fetched into, so they are kept.
pub const VERSION_REF_NAMESPACE: &str = "refs/bcode-review/version";

/// Most commits fetched one by one when fetching them together fails (one of them may be gone).
const MAX_SINGLE_FETCHES: usize = 50;

/// Make the commits `histories` name local where GitHub still has them: one fetch for all of
/// those missing, then, when that fails (a commit GitHub no longer serves fails the whole fetch),
/// one fetch each. Failures leave versions unavailable; they are not errors.
fn fetch_versions(
    repo_path: &Path,
    remote: &str,
    histories: &[bcode_code_review_models::PullHistory],
) {
    use bcode_code_review_models::PullHistoryEvent;
    if validate_remote(remote).is_err() {
        return;
    }
    let named: BTreeSet<&str> = histories
        .iter()
        .flat_map(|history| &history.events)
        .flat_map(|event| match event {
            PullHistoryEvent::ForcePushed {
                before_oid,
                after_oid,
                ..
            } => vec![before_oid.as_deref(), after_oid.as_deref()],
            PullHistoryEvent::CommitsPushed { head_oid, .. } => vec![Some(head_oid.as_str())],
            PullHistoryEvent::BaseChanged { .. } => Vec::new(),
        })
        .flatten()
        .collect();
    let named: Vec<&str> = named.into_iter().collect();
    let Ok(local) = local_commits(repo_path, &named) else {
        return;
    };
    let missing: Vec<&str> = named
        .into_iter()
        .filter(|oid| oid.len() == 40 && !local.contains(*oid))
        .collect();
    if missing.is_empty() {
        return;
    }
    let refspec = |oid: &str| format!("{oid}:{VERSION_REF_NAMESPACE}/{oid}");
    let fetch = |oids: &[&str]| {
        let refspecs: Vec<String> = oids.iter().map(|oid| refspec(oid)).collect();
        let mut args = vec![
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            remote,
        ];
        args.extend(refspecs.iter().map(String::as_str));
        git(repo_path, &args).is_ok()
    };
    if !fetch(&missing) {
        for oid in missing.iter().take(MAX_SINGLE_FETCHES) {
            fetch(std::slice::from_ref(oid));
        }
    }
}

/// The open pull requests `query` names (see [`parse_pull_query`]), as a list.
fn lookup_pulls(
    github: &bcode_forge::GitHub,
    repository: &Repository,
    query: &str,
) -> Result<PullRequestList, ReviewSourceError> {
    Ok(match parse_pull_query(query) {
        PullQuery::Number(number) => {
            let lookup = github.pull_request(&repository.0, number)?;
            PullRequestList {
                pull_requests: lookup.pull_request.into_iter().collect(),
                via: lookup.via,
                default_branch: lookup.default_branch,
            }
        }
        PullQuery::Branch(branch) => PullRequestList {
            pull_requests: github
                .branch_pull_requests(&repository.0, branch)?
                .with_head,
            via: bcode_forge::models::Transport::Auto,
            default_branch: None,
        },
    })
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
    hints: &[String],
) -> Result<ReviewStack, ReviewSourceError> {
    let github = repository.github(repo_path);
    // The hinted branches are looked up while the pull request is: neither needs the other, and
    // each costs a round trip. The default branch (when known from an earlier answer) is left
    // out, as the walk never looks it up.
    let remembered_default = default_branch(repository, None);
    let prefetch: Vec<String> = hints
        .iter()
        .filter(|branch| Some(branch.as_str()) != remembered_default.as_deref())
        .take(LOOKUP_BATCH)
        .cloned()
        .collect();
    let (lookup, prefetched) = std::thread::scope(|scope| {
        let prefetching = (!prefetch.is_empty())
            .then(|| scope.spawn(|| github.branches_pull_requests(&repository.0, &prefetch)));
        let lookup = github.pull_request(&repository.0, number);
        // A failed prefetch only costs the lookups again during the walk.
        let prefetched = prefetching
            .and_then(|worker| worker.join().ok())
            .and_then(Result::ok)
            .unwrap_or_default();
        (lookup, prefetched)
    });
    let lookup = lookup?;
    let start = stack_candidates(repository, lookup.pull_request.as_slice())
        .pop()
        .ok_or_else(|| ReviewSourceError::PullNotFound(number, repository.display()))?;
    let default_branch = default_branch(repository, lookup.default_branch.as_deref());
    let default_branch = default_branch.as_deref();
    let known: BTreeMap<String, BranchNeighbors> = prefetched
        .into_iter()
        .map(|(name, pulls)| {
            (
                name,
                BranchNeighbors {
                    with_head: stack_candidates(repository, &pulls.with_head),
                    with_base: stack_candidates(repository, &pulls.with_base),
                },
            )
        })
        .collect();
    let mut seen = BTreeSet::new();
    let pending = [&start.head_branch, &start.base_branch]
        .into_iter()
        .chain(hints)
        .filter(|branch| {
            Some(branch.as_str()) != default_branch
                && !known.contains_key(branch.as_str())
                && seen.insert(*branch)
        })
        .cloned()
        .collect();
    let mut branches = ForgeBranches {
        github: &github,
        repository,
        known,
        pending,
    };
    walk_stack(&mut branches, &start, default_branch, max_layers)
}

/// `repository`'s default branch: `answered` (remembered for later) when GitHub just reported
/// it, else the one it reported before in this process.
fn default_branch(repository: &Repository, answered: Option<&str>) -> Option<String> {
    static DEFAULTS: std::sync::OnceLock<std::sync::Mutex<BTreeMap<String, String>>> =
        std::sync::OnceLock::new();
    let mut defaults = DEFAULTS
        .get_or_init(std::sync::Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let key = format!("{}/{}", repository.0.host, repository.0.full_name());
    if let Some(answered) = answered {
        defaults.insert(key, answered.to_owned());
        return Some(answered.to_owned());
    }
    defaults.get(&key).cloned()
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

/// Most branches looked up at once while walking. The forge splits them into requests of 40 that
/// run in parallel, and each request costs about as much as the next (~1 s for 40 branches), so
/// looking up every hint at once costs one request's time instead of one per 40 hints.
const LOOKUP_BATCH: usize = 200;

/// [`StackBranches`] answered by GitHub in batches. A branch the walk needs is looked up together
/// with the next hinted branches not looked up yet (in the order discovery ranked them), so a
/// well-hinted stack of `n` layers costs about `n / 40` requests, and hints the walk never needs
/// beyond the batch cost nothing.
struct ForgeBranches<'a> {
    github: &'a bcode_forge::GitHub,
    repository: &'a Repository,
    known: BTreeMap<String, BranchNeighbors>,
    /// Hinted branches not looked up yet, most relevant first.
    pending: std::collections::VecDeque<String>,
}

impl ForgeBranches<'_> {
    /// Look `branch` up with the next pending hints, and keep every answer.
    fn look_up(&mut self, branch: &str) -> Result<(), ReviewSourceError> {
        // A walk is several round trips; one the caller no longer wants stops here.
        if bcode_code_review_provider::cancelled() {
            return Err(ReviewSourceError::Cancelled);
        }
        let mut batch = vec![branch.to_owned()];
        while batch.len() < LOOKUP_BATCH {
            let Some(next) = self.pending.pop_front() else {
                break;
            };
            if next != branch && !self.known.contains_key(&next) {
                batch.push(next);
            }
        }
        for (name, pulls) in self
            .github
            .branches_pull_requests(&self.repository.0, &batch)?
        {
            self.known.insert(
                name,
                BranchNeighbors {
                    with_head: stack_candidates(self.repository, &pulls.with_head),
                    with_base: stack_candidates(self.repository, &pulls.with_base),
                },
            );
        }
        Ok(())
    }
}

impl StackBranches for ForgeBranches<'_> {
    type Error = ReviewSourceError;

    fn neighbors(&mut self, branch: &str) -> Result<BranchNeighbors, ReviewSourceError> {
        if !self.known.contains_key(branch) {
            self.look_up(branch)?;
        }
        Ok(self.known.get(branch).cloned().unwrap_or_default())
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
    validate_remote(remote)?;
    let base_source = validate_base_branch(repo_path, base_branch)?;
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
    Ok(ReviewPullFetchResult {
        number,
        head_oid: ref_commit(repo_path, &head_ref)?,
        head_ref,
        base_branch: base_branch.to_string(),
        base_oid: ref_commit(repo_path, &base_ref)?,
        base_ref,
    })
}

/// Make the head and base commits of every pull request of `targets` local under
/// `refs/bcode-review/pull/<number>/{head,base}`, returning them in `targets` order.
///
/// A commit the provider reported (`head_oid`, `base_oid`) that already exists locally is pointed
/// at directly, without the network; the rest arrive in one `git fetch` from `remote` with every
/// refspec, so a stack costs at most one fetch however many layers it has.
///
/// # Errors
///
/// * [`ReviewSourceError::InvalidRequest`] when `remote` or a base branch is not a safe name.
/// * [`ReviewSourceError::Fetch`] when `git fetch` or a reference update fails, or a reference
///   does not resolve afterwards.
pub fn fetch_pulls(
    repo_path: &Path,
    remote: &str,
    targets: &[ReviewPullFetchTarget],
) -> Result<Vec<ReviewPullFetchResult>, ReviewSourceError> {
    validate_remote(remote)?;
    let reported: Vec<&str> = targets
        .iter()
        .flat_map(|target| [target.head_oid.as_deref(), target.base_oid.as_deref()])
        .flatten()
        .collect();
    let local = local_commits(repo_path, &reported)?;
    let mut refspecs: Vec<String> = Vec::new();
    let mut updates = String::new();
    // Commits each review ref will hold, when known before fetching.
    let mut known: BTreeMap<String, String> = BTreeMap::new();
    for target in targets {
        let number = target.number;
        let head_ref = format!("{FETCH_REF_NAMESPACE}/{number}/head");
        let base_ref = format!("{FETCH_REF_NAMESPACE}/{number}/base");
        for (oid, local_ref, is_head) in [
            (target.head_oid.as_deref(), head_ref, true),
            (target.base_oid.as_deref(), base_ref, false),
        ] {
            if let Some(oid) = oid.filter(|oid| local.contains(*oid)) {
                let _ = writeln!(updates, "update {local_ref} {oid}");
                known.insert(local_ref, oid.to_owned());
            } else if is_head {
                refspecs.push(format!("+refs/pull/{number}/head:{local_ref}"));
            } else {
                // The base branch name only reaches git when it is fetched.
                let source = validate_base_branch(repo_path, &target.base_branch)?;
                refspecs.push(format!("+{source}:{local_ref}"));
            }
        }
    }
    if !refspecs.is_empty() {
        let mut args = vec![
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            // No opportunistic remote-tracking updates: only the review-owned refs change.
            "--refmap=",
            "--end-of-options",
            remote,
        ];
        args.extend(refspecs.iter().map(String::as_str));
        git(repo_path, &args).map_err(ReviewSourceError::Fetch)?;
    }
    if !updates.is_empty() {
        git_with_input(repo_path, &["update-ref", "--stdin"], &updates)
            .map_err(ReviewSourceError::Fetch)?;
    }
    let fetched_refs: Vec<String> = refspecs
        .iter()
        .filter_map(|refspec| refspec.rsplit_once(':').map(|(_, local)| local.to_owned()))
        .collect();
    for (reference, oid) in fetched_refs
        .iter()
        .zip(ref_commits(repo_path, &fetched_refs)?)
    {
        known.insert(reference.clone(), oid);
    }
    targets
        .iter()
        .map(|target| {
            let head_ref = format!("{FETCH_REF_NAMESPACE}/{}/head", target.number);
            let base_ref = format!("{FETCH_REF_NAMESPACE}/{}/base", target.number);
            let commit = |reference: &str| {
                known
                    .get(reference)
                    .cloned()
                    .ok_or_else(|| ReviewSourceError::Fetch(format!("{reference} did not resolve")))
            };
            Ok(ReviewPullFetchResult {
                number: target.number,
                head_oid: commit(&head_ref)?,
                head_ref,
                base_branch: target.base_branch.clone(),
                base_oid: commit(&base_ref)?,
                base_ref,
            })
        })
        .collect()
}

/// Which of `oids` (full commit ids) are commits in the local repository: one
/// `git cat-file --batch-check` for all of them.
fn local_commits(repo_path: &Path, oids: &[&str]) -> Result<BTreeSet<String>, ReviewSourceError> {
    let candidates: Vec<&str> = oids
        .iter()
        .copied()
        .filter(|oid| oid.len() >= 40 && oid.chars().all(|c| c.is_ascii_hexdigit()))
        .collect();
    if candidates.is_empty() {
        return Ok(BTreeSet::new());
    }
    let input = candidates.join("\n") + "\n";
    let output = git_with_input(
        repo_path,
        &["cat-file", "--batch-check=%(objectname) %(objecttype)"],
        &input,
    )
    .map_err(ReviewSourceError::Fetch)?;
    Ok(output
        .lines()
        .filter_map(|line| line.strip_suffix(" commit"))
        .map(str::to_owned)
        .collect())
}

/// The commits `references` point at, in order: one `git rev-parse` for all of them.
fn ref_commits(repo_path: &Path, references: &[String]) -> Result<Vec<String>, ReviewSourceError> {
    if references.is_empty() {
        return Ok(Vec::new());
    }
    let specs: Vec<String> = references
        .iter()
        .map(|reference| format!("{reference}^{{commit}}"))
        .collect();
    // Each revision resolves or the command fails. The references are review-owned
    // (`refs/bcode-review/…`), so none can read as an option (`rev-parse` would echo
    // `--end-of-options` back as output).
    let mut args = vec!["rev-parse"];
    args.extend(specs.iter().map(String::as_str));
    let output = git(repo_path, &args).map_err(ReviewSourceError::Fetch)?;
    let oids: Vec<String> = output.lines().map(str::to_owned).collect();
    if oids.len() != references.len() {
        return Err(ReviewSourceError::Fetch(format!(
            "{} of {} fetched references resolved",
            oids.len(),
            references.len()
        )));
    }
    Ok(oids)
}

fn validate_remote(remote: &str) -> Result<(), ReviewSourceError> {
    if remote.is_empty() || remote.starts_with('-') || remote.chars().any(char::is_control) {
        return Err(ReviewSourceError::InvalidRequest(format!(
            "invalid remote {remote:?}"
        )));
    }
    Ok(())
}

/// `refs/heads/<base_branch>`, when `base_branch` is a safe branch name.
fn validate_base_branch(repo_path: &Path, base_branch: &str) -> Result<String, ReviewSourceError> {
    let source = format!("refs/heads/{base_branch}");
    if base_branch.is_empty()
        || base_branch.starts_with('-')
        || git(repo_path, &["check-ref-format", &source]).is_err()
    {
        return Err(ReviewSourceError::InvalidRequest(format!(
            "invalid base branch {base_branch:?}"
        )));
    }
    Ok(source)
}

/// The commit `reference` points at.
fn ref_commit(repo_path: &Path, reference: &str) -> Result<String, ReviewSourceError> {
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
}

fn git_with_input(repo_path: &Path, args: &[&str], input: &str) -> Result<String, String> {
    use std::io::Write as _;
    let mut child = Command::new("git")
        .args(args)
        .current_dir(repo_path)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("git could not start: {error}"))?;
    child
        .stdin
        .take()
        .ok_or("git stdin unavailable")?
        .write_all(input.as_bytes())
        .map_err(|error| format!("git stdin: {error}"))?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("git: {error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
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
