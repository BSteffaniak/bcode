//! Code forge access for bcode: GitHub pull requests.
//!
//! [`GitHub`] implements [`PullRequestSource`]. With the `http` feature it asks GitHub's
//! GraphQL API over HTTPS with a token ([`resolve_token`]: the configured variable,
//! `GITHUB_TOKEN`, `GH_TOKEN`, or `gh auth token`), and falls back to `gh api graphql`, which
//! uses `gh`'s own login and network settings; [`Transport`] pins either. Without the feature
//! (for lightweight binaries) only `gh` is used. GitHub Enterprise hosts are served when
//! configured; their API lives at `https://<host>/api/graphql`.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(feature = "http")]
use std::time::Duration;

pub use bcode_forge_models as models;
use bcode_forge_models::{
    ChecksState, ForgeError, PullRequest, PullRequestList, PullRequestSource, PullRequestState,
    RepositoryRef, ReviewDecision, Transport,
};
use serde_json::Value;
#[cfg(feature = "http")]
use serde_json::json;

/// The public GitHub host.
pub const GITHUB_HOST: &str = "github.com";
/// How long one HTTPS request may take.
#[cfg(feature = "http")]
const TIMEOUT: Duration = Duration::from_secs(20);

/// The GraphQL query for a repository's recent pull requests, with review decisions, the
/// latest commit's checks, requested reviewers, the repository's default branch, and the
/// authenticated user (to tell whether a review is requested from them).
const PULL_REQUESTS_QUERY: &str = "query($owner: String!, $name: String!) {
  viewer { login }
  repository(owner: $owner, name: $name) {
    defaultBranchRef { name }
    pullRequests(first: 100, orderBy: {field: UPDATED_AT, direction: DESC}) {
      nodes {
        number title url state isDraft
        headRefName headRefOid baseRefName baseRefOid
        headRepository { nameWithOwner }
        author { login }
        reviewRequests(first: 20) { nodes { requestedReviewer { ... on User { login } } } }
        reviewDecision updatedAt
        commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }
      }
    }
  }
}";

/// A credential and where it came from (for display; never the value).
#[derive(Clone, PartialEq, Eq)]
pub struct Token {
    value: String,
    /// Where the token came from, such as `GITHUB_TOKEN` or `gh auth token`.
    pub source: String,
}

impl Token {
    /// The secret.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Token")
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

/// Find a GitHub token for `host`.
///
/// Looks at `token_env` when given, then the variables `gh` reads (`GITHUB_TOKEN` and
/// `GH_TOKEN` for github.com; `GH_ENTERPRISE_TOKEN` and `GITHUB_ENTERPRISE_TOKEN` for other
/// hosts), then runs `gh auth token --hostname <host>` in `cwd`.
///
/// # Errors
/// * [`ForgeError::Unauthenticated`] when none of these yields a token.
pub fn resolve_token(host: &str, token_env: Option<&str>, cwd: &Path) -> Result<Token, ForgeError> {
    resolve_token_with(
        host,
        token_env,
        |name| std::env::var(name).ok(),
        || gh(&["auth", "token", "--hostname", host], cwd).ok(),
    )
}

fn resolve_token_with(
    host: &str,
    token_env: Option<&str>,
    env: impl Fn(&str) -> Option<String>,
    gh_token: impl FnOnce() -> Option<String>,
) -> Result<Token, ForgeError> {
    let variables: &[&str] = if host == GITHUB_HOST {
        &["GITHUB_TOKEN", "GH_TOKEN"]
    } else {
        &["GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN"]
    };
    for name in token_env.into_iter().chain(variables.iter().copied()) {
        if let Some(value) = env(name).filter(|value| !value.trim().is_empty()) {
            return Ok(Token {
                value: value.trim().to_owned(),
                source: name.to_owned(),
            });
        }
    }
    gh_token()
        .filter(|value| !value.trim().is_empty())
        .map(|value| Token {
            value: value.trim().to_owned(),
            source: "gh auth token".into(),
        })
        .ok_or_else(|| {
            ForgeError::Unauthenticated(format!(
                "no GitHub token for {host} (set {} or run `gh auth login`)",
                variables.join(" or ")
            ))
        })
}

/// GitHub as a [`PullRequestSource`].
#[derive(Debug, Clone)]
pub struct GitHub {
    transport: Transport,
    hosts: Vec<String>,
    token_env: Option<String>,
    cwd: PathBuf,
}

impl GitHub {
    /// A source for github.com, running `gh` in `cwd`.
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            transport: Transport::Auto,
            hosts: vec![GITHUB_HOST.to_owned()],
            token_env: None,
            cwd: cwd.into(),
        }
    }

    /// Choose how GitHub is reached.
    #[must_use]
    pub const fn transport(mut self, transport: Transport) -> Self {
        self.transport = transport;
        self
    }

    /// Also serve these GitHub Enterprise hosts.
    #[must_use]
    pub fn enterprise_hosts(mut self, hosts: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.hosts.extend(
            hosts
                .into_iter()
                .map(|host| host.into().to_ascii_lowercase()),
        );
        self
    }

    /// Read the token from this variable before the standard ones.
    #[must_use]
    pub fn token_env(mut self, name: impl Into<String>) -> Self {
        self.token_env = Some(name.into());
        self
    }

    #[cfg(not(feature = "http"))]
    #[allow(clippy::unused_self)] // the same shape as the HTTPS transport
    fn http(&self, _repository: &RepositoryRef) -> Result<Value, ForgeError> {
        Err(ForgeError::Failed(
            "the HTTPS transport is not built into this binary (use `gh`)".into(),
        ))
    }

    #[cfg(feature = "http")]
    fn http(&self, repository: &RepositoryRef) -> Result<Value, ForgeError> {
        let token = resolve_token(&repository.host, self.token_env.as_deref(), &self.cwd)?;
        let endpoint = if repository.host == GITHUB_HOST {
            "https://api.github.com/graphql".to_owned()
        } else {
            format!("https://{}/api/graphql", repository.host)
        };
        let body = json!({
            "query": PULL_REQUESTS_QUERY,
            "variables": { "owner": repository.owner, "name": repository.name },
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| ForgeError::Failed(format!("could not start HTTP client: {error}")))?;
        runtime.block_on(async {
            let response = reqwest::Client::builder()
                .timeout(TIMEOUT)
                .user_agent(concat!("bcode/", env!("CARGO_PKG_VERSION")))
                .build()
                .map_err(|error| ForgeError::Failed(error.to_string()))?
                .post(&endpoint)
                .bearer_auth(token.value())
                .json(&body)
                .send()
                .await
                .map_err(|error| ForgeError::Failed(format!("{endpoint}: {error}")))?;
            let status = response.status();
            let value: Value = response
                .json()
                .await
                .map_err(|error| ForgeError::Failed(format!("{endpoint}: {error}")))?;
            match status.as_u16() {
                200 => Ok(value),
                401 => Err(ForgeError::Unauthenticated(format!(
                    "GitHub rejected the token from {}",
                    token.source
                ))),
                code => Err(ForgeError::Failed(format!(
                    "{endpoint} answered {code}: {}",
                    value["message"].as_str().unwrap_or("no details")
                ))),
            }
        })
    }

    fn cli(&self, repository: &RepositoryRef) -> Result<Value, ForgeError> {
        let output = gh(
            &[
                "api",
                "graphql",
                "--hostname",
                &repository.host,
                "-f",
                &format!("query={PULL_REQUESTS_QUERY}"),
                "-f",
                &format!("owner={}", repository.owner),
                "-f",
                &format!("name={}", repository.name),
            ],
            &self.cwd,
        )?;
        serde_json::from_str(&output)
            .map_err(|error| ForgeError::Failed(format!("unexpected gh output: {error}")))
    }
}

impl PullRequestSource for GitHub {
    fn serves(&self, repository: &RepositoryRef) -> bool {
        self.hosts.contains(&repository.host)
    }

    fn pull_requests(&self, repository: &RepositoryRef) -> Result<PullRequestList, ForgeError> {
        if !self.serves(repository) {
            return Err(ForgeError::UnsupportedHost(repository.host.clone()));
        }
        let (value, via) = match self.transport {
            Transport::Http => (self.http(repository)?, Transport::Http),
            Transport::Cli => (self.cli(repository)?, Transport::Cli),
            Transport::Auto if !cfg!(feature = "http") => (self.cli(repository)?, Transport::Cli),
            Transport::Auto => match self.http(repository) {
                Ok(value) => (value, Transport::Http),
                Err(http) => match self.cli(repository) {
                    Ok(value) => (value, Transport::Cli),
                    // Report why the preferred way failed, and that the fallback did too.
                    Err(cli) => {
                        return Err(match http {
                            ForgeError::Unauthenticated(message) => {
                                ForgeError::Unauthenticated(format!("{message}; gh: {cli}"))
                            }
                            other => ForgeError::Failed(format!("{other}; gh: {cli}")),
                        });
                    }
                },
            },
        };
        Ok(PullRequestList {
            pull_requests: parse_pull_requests(&value)?,
            via,
            default_branch: parse_default_branch(&value),
        })
    }
}

/// Run `gh` in `cwd`, returning its standard output.
fn gh(args: &[&str], cwd: &Path) -> Result<String, ForgeError> {
    let output = Command::new("gh")
        .args(args)
        .current_dir(cwd)
        .env("GH_PROMPT_DISABLED", "1")
        .output()
        .map_err(|error| ForgeError::Failed(format!("gh is not available: {error}")))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(ForgeError::Failed(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ))
    }
}

/// Pull requests from a GraphQL response to [`PULL_REQUESTS_QUERY`].
///
/// # Errors
/// * [`ForgeError::Failed`] when the response reports errors or lacks the repository.
pub fn parse_pull_requests(value: &Value) -> Result<Vec<PullRequest>, ForgeError> {
    if let Some(errors) = value["errors"]
        .as_array()
        .filter(|errors| !errors.is_empty())
    {
        let messages: Vec<&str> = errors
            .iter()
            .filter_map(|error| error["message"].as_str())
            .collect();
        return Err(ForgeError::Failed(format!(
            "GitHub answered: {}",
            messages.join("; ")
        )));
    }
    let Some(nodes) = value["data"]["repository"]["pullRequests"]["nodes"].as_array() else {
        return Err(ForgeError::Failed(
            "GitHub did not return the repository's pull requests".into(),
        ));
    };
    let viewer = value["data"]["viewer"]["login"].as_str();
    Ok(nodes
        .iter()
        .filter_map(|node| parse_pull_request(node, viewer))
        .collect())
}

/// The repository's default branch from a GraphQL response to [`PULL_REQUESTS_QUERY`], when it
/// reports one (empty repositories have none).
#[must_use]
pub fn parse_default_branch(value: &Value) -> Option<String> {
    value["data"]["repository"]["defaultBranchRef"]["name"]
        .as_str()
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

fn parse_pull_request(node: &Value, viewer: Option<&str>) -> Option<PullRequest> {
    let text = |field: &str| node[field].as_str().map(str::to_owned);
    let state = match (
        node["state"].as_str()?,
        node["isDraft"].as_bool().unwrap_or(false),
    ) {
        ("MERGED", _) => PullRequestState::Merged,
        ("CLOSED", _) => PullRequestState::Closed,
        ("OPEN", true) => PullRequestState::Draft,
        ("OPEN", false) => PullRequestState::Open,
        _ => return None,
    };
    let review = match node["reviewDecision"].as_str() {
        Some("APPROVED") => Some(ReviewDecision::Approved),
        Some("CHANGES_REQUESTED") => Some(ReviewDecision::ChangesRequested),
        Some("REVIEW_REQUIRED") => Some(ReviewDecision::ReviewRequired),
        _ => None,
    };
    let checks = match node["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["state"].as_str()
    {
        Some("SUCCESS") => Some(ChecksState::Passing),
        Some("FAILURE" | "ERROR") => Some(ChecksState::Failing),
        Some("PENDING" | "EXPECTED") => Some(ChecksState::Pending),
        _ => None,
    };
    Some(PullRequest {
        number: node["number"].as_u64()?,
        title: text("title").unwrap_or_default(),
        url: text("url").unwrap_or_default(),
        state,
        head: text("headRefName")?,
        head_repository: node["headRepository"]["nameWithOwner"]
            .as_str()
            .map(str::to_owned),
        head_oid: text("headRefOid").unwrap_or_default(),
        base: text("baseRefName").unwrap_or_default(),
        review,
        checks,
        updated_at: text("updatedAt").unwrap_or_default(),
        author: node["author"]["login"].as_str().map(str::to_owned),
        base_oid: text("baseRefOid").filter(|oid| !oid.is_empty()),
        review_requested_from_viewer: viewer.is_some_and(|viewer| {
            node["reviewRequests"]["nodes"]
                .as_array()
                .is_some_and(|requests| {
                    requests.iter().any(|request| {
                        request["requestedReviewer"]["login"]
                            .as_str()
                            .is_some_and(|login| login.eq_ignore_ascii_case(viewer))
                    })
                })
        }),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn response() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/pull_requests.json")).unwrap()
    }

    #[test]
    fn responses_become_pull_requests() {
        let pulls = parse_pull_requests(&response()).unwrap();
        assert_eq!(pulls.len(), 4);
        let open = &pulls[0];
        assert_eq!(open.number, 12);
        assert_eq!(open.state, PullRequestState::Open);
        assert_eq!(open.head, "feature/login");
        assert_eq!(open.head_repository.as_deref(), Some("octo/hello"));
        assert_eq!(open.review, Some(ReviewDecision::Approved));
        assert_eq!(open.checks, Some(ChecksState::Passing));
        assert_eq!(pulls[1].state, PullRequestState::Draft);
        assert_eq!(pulls[1].checks, Some(ChecksState::Failing));
        assert_eq!(pulls[1].review, Some(ReviewDecision::ReviewRequired));
        assert_eq!(pulls[2].state, PullRequestState::Merged);
        assert_eq!(pulls[2].checks, None);
        assert_eq!(pulls[3].state, PullRequestState::Closed);
        assert_eq!(pulls[3].head_repository.as_deref(), Some("someone/hello"));
        assert_eq!(pulls[3].checks, Some(ChecksState::Pending));
        assert_eq!(open.author.as_deref(), Some("octocat"));
        assert_eq!(
            open.base_oid.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        // Requested from the viewer (case-insensitively), from someone else, or from a team.
        assert!(open.review_requested_from_viewer);
        assert!(!pulls[1].review_requested_from_viewer);
        assert!(!pulls[2].review_requested_from_viewer);
        assert_eq!(pulls[3].author, None);
        assert_eq!(pulls[3].base_oid, None);
        assert_eq!(parse_default_branch(&response()).as_deref(), Some("main"));
        assert_eq!(parse_default_branch(&json!({ "data": {} })), None);
    }

    #[test]
    fn error_responses_are_reported() {
        let value = json!({ "errors": [{ "message": "Could not resolve to a Repository" }] });
        let error = parse_pull_requests(&value).unwrap_err();
        assert!(error.to_string().contains("Could not resolve"), "{error}");
        assert!(parse_pull_requests(&json!({ "data": {} })).is_err());
    }

    #[test]
    fn tokens_come_from_the_configured_variable_then_standard_ones_then_gh() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_owned())
            }
        };
        let token = resolve_token_with(
            GITHUB_HOST,
            Some("MY_TOKEN"),
            env(&[("MY_TOKEN", "mine"), ("GITHUB_TOKEN", "standard")]),
            || panic!("gh is not asked when a variable is set"),
        )
        .unwrap();
        assert_eq!((token.value(), token.source.as_str()), ("mine", "MY_TOKEN"));
        let token =
            resolve_token_with(GITHUB_HOST, None, env(&[("GH_TOKEN", " gh ")]), || None).unwrap();
        assert_eq!((token.value(), token.source.as_str()), ("gh", "GH_TOKEN"));
        // Enterprise hosts read the enterprise variables, not github.com's.
        let token = resolve_token_with(
            "ghe.example.com",
            None,
            env(&[("GITHUB_TOKEN", "public"), ("GH_ENTERPRISE_TOKEN", "corp")]),
            || None,
        )
        .unwrap();
        assert_eq!(token.value(), "corp");
        let token = resolve_token_with(GITHUB_HOST, None, env(&[("GITHUB_TOKEN", "  ")]), || {
            Some("from-gh\n".into())
        })
        .unwrap();
        assert_eq!(
            (token.value(), token.source.as_str()),
            ("from-gh", "gh auth token")
        );
        let missing = resolve_token_with(GITHUB_HOST, None, env(&[]), || None).unwrap_err();
        assert!(matches!(missing, ForgeError::Unauthenticated(_)));
        // The secret never shows in debug output.
        assert!(!format!("{token:?}").contains("from-gh"));
    }

    #[test]
    fn only_configured_hosts_are_served() {
        let github = GitHub::new(".").enterprise_hosts(["GHE.example.com"]);
        let repo = |host: &str| RepositoryRef {
            host: host.into(),
            owner: "o".into(),
            name: "r".into(),
        };
        assert!(github.serves(&repo("github.com")));
        assert!(github.serves(&repo("ghe.example.com")));
        assert!(!github.serves(&repo("gitlab.com")));
        assert!(matches!(
            github.pull_requests(&repo("gitlab.com")),
            Err(ForgeError::UnsupportedHost(_))
        ));
    }
}
