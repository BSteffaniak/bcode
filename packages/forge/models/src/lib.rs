//! Code forge models: repositories on a hosting service and their pull requests.
//!
//! These types describe what a forge (such as GitHub) knows about a repository, independent of
//! how it is fetched. A [`PullRequestSource`] is the seam through which a UI asks for them; the
//! `bcode_forge` crate provides a GitHub source.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use serde::{Deserialize, Serialize};

/// A repository on a forge, as named by a git remote URL.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RepositoryRef {
    /// Host name (`github.com`).
    pub host: String,
    /// Owner or group (`octocat`).
    pub owner: String,
    /// Repository name, without `.git` (`hello-world`).
    pub name: String,
}

impl RepositoryRef {
    /// `owner/name`.
    #[must_use]
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// The repository a git remote URL points at: `https://host/owner/repo(.git)`,
    /// `ssh://user@host[:port]/owner/repo(.git)`, `git://…`, or scp-like
    /// `user@host:owner/repo(.git)`. `None` for local paths and URLs without an owner and
    /// repository.
    #[must_use]
    pub fn from_remote_url(url: &str) -> Option<Self> {
        let url = url.trim();
        let (host, path) = if let Some((scheme, rest)) = url.split_once("://") {
            if !matches!(
                scheme.to_ascii_lowercase().as_str(),
                "http" | "https" | "ssh" | "git"
            ) {
                return None;
            }
            let host_path = rest
                .split_once('@')
                .map_or(rest, |(_, host_path)| host_path);
            let (host_port, path) = host_path.split_once('/')?;
            (host_port.split(':').next()?, path)
        } else {
            // scp-like: `[user@]host:path`, where the part before `:` has no `/`.
            let (user_host, path) = url.split_once(':')?;
            if user_host.contains('/') {
                return None;
            }
            (user_host.rsplit('@').next()?, path)
        };
        let mut segments = path
            .trim_start_matches('/')
            .split('/')
            .filter(|segment| !segment.is_empty());
        let owner = segments.next()?;
        let name = segments.next()?;
        let name = name.strip_suffix(".git").unwrap_or(name);
        if host.is_empty() || owner.is_empty() || name.is_empty() {
            return None;
        }
        Some(Self {
            host: host.to_ascii_lowercase(),
            owner: owner.to_owned(),
            name: name.to_owned(),
        })
    }
}

/// Where a pull request stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PullRequestState {
    /// Open and ready for review.
    Open,
    /// Open as a draft.
    Draft,
    /// Merged.
    Merged,
    /// Closed without merging.
    Closed,
}

impl PullRequestState {
    /// Configuration and display name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Draft => "draft",
            Self::Merged => "merged",
            Self::Closed => "closed",
        }
    }

    /// Whether the pull request is still open (including drafts).
    #[must_use]
    pub const fn is_open(self) -> bool {
        matches!(self, Self::Open | Self::Draft)
    }
}

/// The review decision on a pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReviewDecision {
    /// Approved.
    Approved,
    /// Changes were requested.
    ChangesRequested,
    /// A review is required and has not been given.
    ReviewRequired,
}

impl ReviewDecision {
    /// Configuration and display name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::ChangesRequested => "changes-requested",
            Self::ReviewRequired => "review-required",
        }
    }
}

/// The combined status of a pull request's checks on its latest commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChecksState {
    /// All checks passed.
    Passing,
    /// At least one check failed.
    Failing,
    /// Checks are still running or waiting.
    Pending,
}

impl ChecksState {
    /// Configuration and display name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Passing => "passing",
            Self::Failing => "failing",
            Self::Pending => "pending",
        }
    }
}

/// A pull request (or merge request) on a forge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    /// Number within the repository.
    pub number: u64,
    /// Title.
    pub title: String,
    /// Web page.
    pub url: String,
    /// State.
    pub state: PullRequestState,
    /// Branch the changes come from.
    pub head: String,
    /// Repository the head branch is in (`owner/name`), when known; a fork's differs from
    /// the base repository.
    pub head_repository: Option<String>,
    /// Commit the head branch pointed at (when merged, the last commit merged).
    pub head_oid: String,
    /// Branch the changes go into.
    pub base: String,
    /// Review decision, when the forge reports one.
    pub review: Option<ReviewDecision>,
    /// Checks on the latest commit, when there are any.
    pub checks: Option<ChecksState>,
    /// Last update, RFC 3339.
    pub updated_at: String,
    /// Login of the author, when the forge reports one (deleted accounts have none).
    #[serde(default)]
    pub author: Option<String>,
    /// Commit the base branch pointed at, when the forge reports it.
    #[serde(default)]
    pub base_oid: Option<String>,
    /// Whether the authenticated user is one of the requested reviewers (directly, not
    /// through a team).
    #[serde(default)]
    pub review_requested_from_viewer: bool,
}

/// How a forge is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Transport {
    /// The HTTPS API with a token, falling back to the forge's command-line client.
    #[default]
    Auto,
    /// The HTTPS API with a token only.
    Http,
    /// The forge's command-line client only (`gh` for GitHub).
    #[serde(alias = "gh")]
    Cli,
}

impl Transport {
    /// Configuration and display name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Http => "http",
            Self::Cli => "cli",
        }
    }
}

/// Pull requests of a repository, and how they were fetched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestList {
    /// Recently updated pull requests, newest first.
    pub pull_requests: Vec<PullRequest>,
    /// The transport that answered: [`Transport::Http`] or [`Transport::Cli`].
    pub via: Transport,
    /// The repository's default branch, when the forge reports it.
    #[serde(default)]
    pub default_branch: Option<String>,
}

/// Why pull requests could not be fetched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ForgeError {
    /// The repository's host is not one this source serves.
    UnsupportedHost(String),
    /// No credentials were found.
    Unauthenticated(String),
    /// The request failed.
    Failed(String),
}

impl std::fmt::Display for ForgeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedHost(host) => write!(formatter, "{host} is not a supported forge"),
            Self::Unauthenticated(message) | Self::Failed(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ForgeError {}

/// Something that can list a repository's pull requests (for example GitHub). Called off the
/// UI thread; implementations may block.
pub trait PullRequestSource: Send + Sync {
    /// Whether this source serves `repository`'s host.
    fn serves(&self, repository: &RepositoryRef) -> bool;

    /// Recently updated pull requests of `repository`.
    ///
    /// # Errors
    /// * [`ForgeError::UnsupportedHost`] when [`Self::serves`] is false.
    /// * [`ForgeError::Unauthenticated`] when no credentials are available.
    /// * [`ForgeError::Failed`] when the forge cannot be reached or answers with an error.
    fn pull_requests(&self, repository: &RepositoryRef) -> Result<PullRequestList, ForgeError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(url: &str) -> Option<(String, String, String)> {
        RepositoryRef::from_remote_url(url).map(|repo| (repo.host, repo.owner, repo.name))
    }

    #[test]
    fn remote_urls_name_their_repository() {
        let expected = Some(("github.com".into(), "octo".into(), "hello".into()));
        for url in [
            "https://github.com/octo/hello",
            "https://github.com/octo/hello.git",
            "https://user:token@GitHub.com/octo/hello.git",
            "ssh://git@github.com/octo/hello.git",
            "ssh://git@github.com:22/octo/hello",
            "git://github.com/octo/hello.git",
            "git@github.com:octo/hello.git",
            "github.com:octo/hello",
        ] {
            assert_eq!(parsed(url), expected, "{url}");
        }
        assert_eq!(
            parsed("git@ghe.example.com:team/tool.git"),
            Some(("ghe.example.com".into(), "team".into(), "tool".into()))
        );
    }

    #[test]
    fn local_and_incomplete_urls_name_nothing() {
        for url in [
            "/srv/git/repo.git",
            "../repo",
            "file:///srv/git/repo.git",
            "https://github.com/octo",
            "git@github.com:",
            "",
        ] {
            assert_eq!(parsed(url), None, "{url}");
        }
    }

    #[test]
    fn pull_requests_without_newer_fields_still_load() {
        let pull: PullRequest = serde_json::from_str(
            r#"{"number":1,"title":"t","url":"u","state":"open","head":"h",
                "head_repository":null,"head_oid":"","base":"main","review":null,
                "checks":null,"updated_at":""}"#,
        )
        .unwrap();
        assert_eq!(pull.author, None);
        assert_eq!(pull.base_oid, None);
        assert!(!pull.review_requested_from_viewer);
        let list: PullRequestList =
            serde_json::from_str(r#"{"pull_requests":[],"via":"cli"}"#).unwrap();
        assert_eq!(list.default_branch, None);
    }

    #[test]
    fn states_round_trip_through_their_names() {
        let state: PullRequestState = serde_json::from_str("\"draft\"").unwrap();
        assert_eq!(state, PullRequestState::Draft);
        assert!(state.is_open() && !PullRequestState::Merged.is_open());
        assert_eq!(
            serde_json::to_string(&ReviewDecision::ChangesRequested).unwrap(),
            "\"changes-requested\""
        );
        assert_eq!(ChecksState::Failing.name(), "failing");
        assert_eq!(Transport::default(), Transport::Auto);
    }
}
