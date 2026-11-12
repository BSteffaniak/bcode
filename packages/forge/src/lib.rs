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

use std::collections::BTreeMap;
#[cfg(feature = "http")]
use std::collections::btree_map::Entry;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(feature = "http")]
use std::sync::{Arc, Mutex, PoisonError};
#[cfg(feature = "http")]
use std::time::Duration;

pub use bcode_forge_models as models;
use bcode_forge_models::{
    BranchPullRequests, ChecksState, ForgeError, PullRequest, PullRequestList, PullRequestLookup,
    PullRequestSource, PullRequestState, RepositoryRef, ReviewDecision, Transport,
};
use serde_json::Value;
#[cfg(feature = "http")]
use serde_json::json;

/// The public GitHub host.
pub const GITHUB_HOST: &str = "github.com";
/// How long one HTTPS request may take.
#[cfg(feature = "http")]
const TIMEOUT: Duration = Duration::from_secs(20);

/// The fields of a pull request every query asks for: enough for lists, stacks, and
/// [`parse_pull_request`].
macro_rules! pull_request_fields {
    () => {
        "number title url state isDraft
        headRefName headRefOid baseRefName baseRefOid
        headRepository { nameWithOwner }
        author { login }
        reviewRequests(first: 20) { nodes { requestedReviewer { ... on User { login } } } }
        reviewDecision updatedAt
        commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }"
    };
}

/// The fields a stack walk needs: [`pull_request_fields`] without reviews and checks, which
/// cost a nested connection per pull request.
macro_rules! stack_pull_request_fields {
    () => {
        "number title url state isDraft
        headRefName headRefOid baseRefName baseRefOid
        headRepository { nameWithOwner }
        author { login }
        updatedAt"
    };
}

/// The GraphQL query for a repository's recent pull requests in any state, with review
/// decisions, the latest commit's checks, requested reviewers, the repository's default branch,
/// and the authenticated user (to tell whether a review is requested from them).
const PULL_REQUESTS_QUERY: &str = concat!(
    "query($owner: String!, $name: String!) {
  viewer { login }
  repository(owner: $owner, name: $name) {
    defaultBranchRef { name }
    pullRequests(first: 100, orderBy: {field: UPDATED_AT, direction: DESC}) {
      nodes { ",
    pull_request_fields!(),
    " }
    }
  }
}"
);

/// [`PULL_REQUESTS_QUERY`] for open pull requests only, so a busy repository's merged and
/// closed ones do not crowd them out.
const OPEN_PULL_REQUESTS_QUERY: &str = concat!(
    "query($owner: String!, $name: String!) {
  viewer { login }
  repository(owner: $owner, name: $name) {
    defaultBranchRef { name }
    pullRequests(first: 100, states: OPEN, orderBy: {field: UPDATED_AT, direction: DESC}) {
      nodes { ",
    pull_request_fields!(),
    " }
    }
  }
}"
);

/// The GraphQL query for one pull request by number, with the repository's default branch.
const PULL_REQUEST_QUERY: &str = concat!(
    "query($owner: String!, $name: String!, $number: Int!) {
  viewer { login }
  repository(owner: $owner, name: $name) {
    defaultBranchRef { name }
    pullRequest(number: $number) { ",
    pull_request_fields!(),
    " }
  }
}"
);

/// Branches per request of [`GitHub::branches_pull_requests`]: two connections each, so a
/// request stays well inside GitHub's node limits.
const BRANCHES_PER_QUERY: usize = 40;

/// Open pull requests per branch and direction that [`GitHub::branches_pull_requests`] reads.
const PULLS_PER_BRANCH: usize = 50;

/// The GraphQL query for the open pull requests that come from (`h<i>`) and go into (`c<i>`)
/// each of `count` branches (`$b<i>`).
fn branches_query(count: usize) -> String {
    use std::fmt::Write as _;
    let mut query = String::from("query($owner: String!, $name: String!");
    for index in 0..count {
        let _ = write!(query, ", $b{index}: String!");
    }
    query.push_str(") {\n  repository(owner: $owner, name: $name) {\n");
    for index in 0..count {
        for (alias, filter) in [("h", "headRefName"), ("c", "baseRefName")] {
            let _ = writeln!(
                query,
                "    {alias}{index}: pullRequests(first: {PULLS_PER_BRANCH}, states: OPEN, \
                 {filter}: $b{index}, orderBy: {{field: UPDATED_AT, direction: DESC}}) {{ \
                 nodes {{ {} }} }}",
                stack_pull_request_fields!()
            );
        }
    }
    query.push_str("  }\n}");
    query
}

/// Pull requests per request of [`GitHub::pull_requests_by_number`]: one node each, so a request stays far
/// inside GitHub's node limits.
const NUMBERS_PER_QUERY: usize = 50;

/// The GraphQL query for each of `count` pull requests by number (`p<i>` for `$n<i>`).
fn numbers_query(count: usize) -> String {
    use std::fmt::Write as _;
    let mut query = String::from("query($owner: String!, $name: String!");
    for index in 0..count {
        let _ = write!(query, ", $n{index}: Int!");
    }
    query.push_str(") {\n  viewer { login }\n  repository(owner: $owner, name: $name) {\n");
    for index in 0..count {
        let _ = writeln!(
            query,
            "    p{index}: pullRequest(number: $n{index}) {{ {} }}",
            stack_pull_request_fields!()
        );
    }
    query.push_str("  }\n}");
    query
}

/// Run `request` for each chunk of `items` (`size` items each) on its own thread, answers in
/// chunk order; the first failure fails the call.
fn each_chunk<T: Sync, A: Send>(
    items: &[T],
    size: usize,
    request: impl Fn(&[T]) -> Result<A, ForgeError> + Sync,
) -> Result<Vec<A>, ForgeError> {
    let request = &request;
    std::thread::scope(|scope| {
        let workers: Vec<_> = items
            .chunks(size)
            .map(|chunk| scope.spawn(move || request(chunk)))
            .collect();
        workers
            .into_iter()
            .map(|worker| {
                worker
                    .join()
                    .unwrap_or_else(|_| Err(ForgeError::Failed("a GitHub request panicked".into())))
            })
            .collect()
    })
}

/// A GraphQL variable's value.
#[derive(Debug, Clone, Copy)]
enum Variable<'a> {
    Text(&'a str),
    Number(u64),
}

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
///
/// With the `http` feature, each host's HTTPS client and token are kept for the source's
/// lifetime (and shared by its clones), so a series of queries, such as a stack walk, reuses one
/// connection instead of opening one per query.
#[derive(Debug, Clone)]
pub struct GitHub {
    transport: Transport,
    hosts: Vec<String>,
    token_env: Option<String>,
    cwd: PathBuf,
    #[cfg(feature = "http")]
    sessions: Arc<Mutex<BTreeMap<String, Arc<HttpSession>>>>,
}

/// One host's HTTPS client, the runtime it runs on, and its token.
#[cfg(feature = "http")]
#[derive(Debug)]
struct HttpSession {
    runtime: tokio::runtime::Runtime,
    client: reqwest::Client,
    token: Token,
}

#[cfg(feature = "http")]
impl HttpSession {
    fn new(host: &str, token_env: Option<&str>, cwd: &Path) -> Result<Self, ForgeError> {
        let token = resolve_token(host, token_env, cwd)?;
        // Requests from several threads share the session; a multi-threaded runtime drives their
        // connections while each caller blocks on its own request.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|error| ForgeError::Failed(format!("could not start HTTP client: {error}")))?;
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent(concat!("bcode/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| ForgeError::Failed(error.to_string()))?;
        Ok(Self {
            runtime,
            client,
            token,
        })
    }

    fn post(&self, endpoint: &str, body: &Value) -> Result<Value, ForgeError> {
        self.runtime.block_on(async {
            let response = self
                .client
                .post(endpoint)
                .bearer_auth(self.token.value())
                .json(body)
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
                    self.token.source
                ))),
                code => Err(ForgeError::Failed(format!(
                    "{endpoint} answered {code}: {}",
                    value["message"].as_str().unwrap_or("no details")
                ))),
            }
        })
    }
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
            #[cfg(feature = "http")]
            sessions: Arc::default(),
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

    /// The 100 most recently updated open pull requests of `repository`. Unlike
    /// [`PullRequestSource::pull_requests`], merged and closed pull requests do not take places
    /// in the list.
    ///
    /// # Errors
    /// * [`ForgeError::UnsupportedHost`] when [`PullRequestSource::serves`] is false.
    /// * [`ForgeError::Unauthenticated`] when no credentials are available.
    /// * [`ForgeError::Failed`] when GitHub cannot be reached or answers with an error.
    pub fn open_pull_requests(
        &self,
        repository: &RepositoryRef,
    ) -> Result<PullRequestList, ForgeError> {
        let (value, via) = self.query(repository, OPEN_PULL_REQUESTS_QUERY, &[])?;
        Ok(PullRequestList {
            pull_requests: parse_pull_requests(&value)?,
            via,
            default_branch: parse_default_branch(&value),
        })
    }

    /// Pull request `number` of `repository` in any state, and the default branch.
    ///
    /// # Errors
    /// * [`ForgeError::UnsupportedHost`] when [`PullRequestSource::serves`] is false.
    /// * [`ForgeError::Unauthenticated`] when no credentials are available.
    /// * [`ForgeError::Failed`] when GitHub cannot be reached or answers with an error other than
    ///   not finding the pull request.
    pub fn pull_request(
        &self,
        repository: &RepositoryRef,
        number: u64,
    ) -> Result<PullRequestLookup, ForgeError> {
        let (value, via) = self.query(
            repository,
            PULL_REQUEST_QUERY,
            &[("number", Variable::Number(number))],
        )?;
        Ok(PullRequestLookup {
            pull_request: parse_single_pull_request(&value)?,
            via,
            default_branch: parse_default_branch(&value),
        })
    }

    /// The open pull requests of `repository` whose head or base branch is `branch` (up to 50
    /// of each). Pull requests from forks are included; their head branch names a branch of the
    /// fork.
    ///
    /// # Errors
    /// * [`ForgeError::UnsupportedHost`] when [`PullRequestSource::serves`] is false.
    /// * [`ForgeError::Unauthenticated`] when no credentials are available.
    /// * [`ForgeError::Failed`] when GitHub cannot be reached or answers with an error.
    pub fn branch_pull_requests(
        &self,
        repository: &RepositoryRef,
        branch: &str,
    ) -> Result<BranchPullRequests, ForgeError> {
        let mut answers = self.branches_pull_requests(repository, &[branch.to_owned()])?;
        Ok(answers.remove(branch).unwrap_or_default())
    }

    /// [`Self::branch_pull_requests`] for many branches: 40 branches per GraphQL request, so a
    /// stack of `n` layers is looked up in `n / 40` requests rather than `n`.
    ///
    /// # Errors
    /// As [`Self::branch_pull_requests`]; the first failing request fails the call.
    pub fn branches_pull_requests(
        &self,
        repository: &RepositoryRef,
        branches: &[String],
    ) -> Result<BTreeMap<String, BranchPullRequests>, ForgeError> {
        let chunks = each_chunk(branches, BRANCHES_PER_QUERY, |chunk| {
            let names: Vec<String> = (0..chunk.len()).map(|index| format!("b{index}")).collect();
            let variables: Vec<(&str, Variable<'_>)> = names
                .iter()
                .zip(chunk)
                .map(|(name, branch)| (name.as_str(), Variable::Text(branch)))
                .collect();
            let (value, _) = self.query(repository, &branches_query(chunk.len()), &variables)?;
            chunk
                .iter()
                .enumerate()
                .map(|(index, branch)| {
                    Ok((branch.clone(), parse_branch_pull_requests(&value, index)?))
                })
                .collect::<Result<Vec<_>, ForgeError>>()
        })?;
        Ok(chunks.into_iter().flatten().collect())
    }

    /// Pull requests of `repository` by number, in any state: 50 per GraphQL request, the
    /// requests in parallel. A number GitHub does not know maps to `None`.
    ///
    /// # Errors
    /// As [`Self::branch_pull_requests`]; the first failing request fails the call.
    pub fn pull_requests_by_number(
        &self,
        repository: &RepositoryRef,
        numbers: &[u64],
    ) -> Result<BTreeMap<u64, Option<PullRequest>>, ForgeError> {
        let chunks = each_chunk(numbers, NUMBERS_PER_QUERY, |chunk| {
            let names: Vec<String> = (0..chunk.len()).map(|index| format!("n{index}")).collect();
            let variables: Vec<(&str, Variable<'_>)> = names
                .iter()
                .zip(chunk)
                .map(|(name, number)| (name.as_str(), Variable::Number(*number)))
                .collect();
            let (value, _) = self.query(repository, &numbers_query(chunk.len()), &variables)?;
            let pulls = parse_numbered_pull_requests(&value, chunk.len())?;
            Ok(chunk.iter().copied().zip(pulls).collect::<Vec<_>>())
        })?;
        Ok(chunks.into_iter().flatten().collect())
    }

    /// Run `query` against `repository` (its `owner` and `name` are passed as variables) with the
    /// configured transport, returning the response and the transport that answered.
    fn query(
        &self,
        repository: &RepositoryRef,
        query: &str,
        variables: &[(&str, Variable<'_>)],
    ) -> Result<(Value, Transport), ForgeError> {
        if !self.serves(repository) {
            return Err(ForgeError::UnsupportedHost(repository.host.clone()));
        }
        let mut all = vec![
            ("owner", Variable::Text(&repository.owner)),
            ("name", Variable::Text(&repository.name)),
        ];
        all.extend_from_slice(variables);
        match self.transport {
            Transport::Http => Ok((self.http(repository, query, &all)?, Transport::Http)),
            Transport::Cli => Ok((self.cli(repository, query, &all)?, Transport::Cli)),
            Transport::Auto if !cfg!(feature = "http") => {
                Ok((self.cli(repository, query, &all)?, Transport::Cli))
            }
            Transport::Auto => match self.http(repository, query, &all) {
                Ok(value) => Ok((value, Transport::Http)),
                Err(http) => match self.cli(repository, query, &all) {
                    Ok(value) => Ok((value, Transport::Cli)),
                    // Report why the preferred way failed, and that the fallback did too.
                    Err(cli) => Err(match http {
                        ForgeError::Unauthenticated(message) => {
                            ForgeError::Unauthenticated(format!("{message}; gh: {cli}"))
                        }
                        other => ForgeError::Failed(format!("{other}; gh: {cli}")),
                    }),
                },
            },
        }
    }

    #[cfg(not(feature = "http"))]
    #[allow(clippy::unused_self)] // the same shape as the HTTPS transport
    fn http(
        &self,
        _repository: &RepositoryRef,
        _query: &str,
        _variables: &[(&str, Variable<'_>)],
    ) -> Result<Value, ForgeError> {
        Err(ForgeError::Failed(
            "the HTTPS transport is not built into this binary (use `gh`)".into(),
        ))
    }

    #[cfg(feature = "http")]
    fn http(
        &self,
        repository: &RepositoryRef,
        query: &str,
        variables: &[(&str, Variable<'_>)],
    ) -> Result<Value, ForgeError> {
        let endpoint = if repository.host == GITHUB_HOST {
            "https://api.github.com/graphql".to_owned()
        } else {
            format!("https://{}/api/graphql", repository.host)
        };
        let variables: serde_json::Map<String, Value> = variables
            .iter()
            .map(|(name, value)| {
                let value = match value {
                    Variable::Text(text) => json!(text),
                    Variable::Number(number) => json!(number),
                };
                ((*name).to_owned(), value)
            })
            .collect();
        let body = json!({ "query": query, "variables": variables });
        // The lock covers finding the session only; requests run concurrently on it.
        let session = {
            let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
            match sessions.entry(repository.host.clone()) {
                Entry::Occupied(entry) => Arc::clone(entry.get()),
                Entry::Vacant(entry) => Arc::clone(entry.insert(Arc::new(HttpSession::new(
                    &repository.host,
                    self.token_env.as_deref(),
                    &self.cwd,
                )?))),
            }
        };
        let result = session.post(&endpoint, &body);
        if matches!(result, Err(ForgeError::Unauthenticated(_))) {
            // A rejected token is resolved again next time (it may have been refreshed), unless
            // another request already replaced the session.
            let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
            if sessions
                .get(&repository.host)
                .is_some_and(|current| Arc::ptr_eq(current, &session))
            {
                sessions.remove(&repository.host);
            }
        }
        result
    }

    fn cli(
        &self,
        repository: &RepositoryRef,
        query: &str,
        variables: &[(&str, Variable<'_>)],
    ) -> Result<Value, ForgeError> {
        let mut args = vec![
            "api".to_owned(),
            "graphql".to_owned(),
            "--hostname".to_owned(),
            repository.host.clone(),
            "-f".to_owned(),
            format!("query={query}"),
        ];
        for (name, value) in variables {
            // `-f` passes text verbatim (a branch named `123` stays a string); `-F` converts
            // numbers.
            match value {
                Variable::Text(text) => args.extend(["-f".to_owned(), format!("{name}={text}")]),
                Variable::Number(number) => {
                    args.extend(["-F".to_owned(), format!("{name}={number}")]);
                }
            }
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = run_gh(&args, &self.cwd)?;
        // `gh` exits with an error when GitHub reports one, but still prints the response, whose
        // errors the parsers interpret (a missing pull request is not a failure).
        match serde_json::from_str::<Value>(&output.stdout) {
            Ok(value) if value.get("data").is_some_and(|data| !data.is_null()) => Ok(value),
            Ok(_) | Err(_) if !output.success => Err(ForgeError::Failed(output.stderr)),
            Ok(value) => Ok(value),
            Err(error) => Err(ForgeError::Failed(format!("unexpected gh output: {error}"))),
        }
    }
}

impl PullRequestSource for GitHub {
    fn serves(&self, repository: &RepositoryRef) -> bool {
        self.hosts.contains(&repository.host)
    }

    fn pull_requests(&self, repository: &RepositoryRef) -> Result<PullRequestList, ForgeError> {
        let (value, via) = self.query(repository, PULL_REQUESTS_QUERY, &[])?;
        Ok(PullRequestList {
            pull_requests: parse_pull_requests(&value)?,
            via,
            default_branch: parse_default_branch(&value),
        })
    }
}

/// What `gh` printed and whether it succeeded.
struct GhOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

/// Run `gh` in `cwd`.
fn run_gh(args: &[&str], cwd: &Path) -> Result<GhOutput, ForgeError> {
    let output = Command::new("gh")
        .args(args)
        .current_dir(cwd)
        .env("GH_PROMPT_DISABLED", "1")
        .output()
        .map_err(|error| ForgeError::Failed(format!("gh is not available: {error}")))?;
    Ok(GhOutput {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    })
}

/// Run `gh` in `cwd`, returning its standard output.
fn gh(args: &[&str], cwd: &Path) -> Result<String, ForgeError> {
    let output = run_gh(args, cwd)?;
    if output.success {
        Ok(output.stdout)
    } else {
        Err(ForgeError::Failed(output.stderr))
    }
}

/// The messages of a GraphQL response's errors, when it reports any.
fn response_errors(value: &Value) -> Option<Vec<&Value>> {
    value["errors"]
        .as_array()
        .filter(|errors| !errors.is_empty())
        .map(|errors| errors.iter().collect())
}

fn answered_with(errors: &[&Value]) -> ForgeError {
    let messages: Vec<&str> = errors
        .iter()
        .filter_map(|error| error["message"].as_str())
        .collect();
    ForgeError::Failed(format!("GitHub answered: {}", messages.join("; ")))
}

/// The pull requests of the connection `field` of a GraphQL response's repository.
fn connection_pull_requests(value: &Value, field: &str) -> Result<Vec<PullRequest>, ForgeError> {
    if let Some(errors) = response_errors(value) {
        return Err(answered_with(&errors));
    }
    let Some(nodes) = value["data"]["repository"][field]["nodes"].as_array() else {
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

/// Pull requests from a GraphQL response to [`PULL_REQUESTS_QUERY`] (or its open-only form).
///
/// # Errors
/// * [`ForgeError::Failed`] when the response reports errors or lacks the repository.
pub fn parse_pull_requests(value: &Value) -> Result<Vec<PullRequest>, ForgeError> {
    connection_pull_requests(value, "pullRequests")
}

/// The pull request of a response to [`PULL_REQUEST_QUERY`]; `None` when GitHub reports that the
/// repository has no pull request with that number.
///
/// # Errors
/// * [`ForgeError::Failed`] when the response reports other errors or lacks the repository.
pub fn parse_single_pull_request(value: &Value) -> Result<Option<PullRequest>, ForgeError> {
    if let Some(errors) = response_errors(value) {
        let not_found = |error: &&Value| {
            error["type"].as_str() == Some("NOT_FOUND")
                && error["path"]
                    .as_array()
                    .is_some_and(|path| path.last().and_then(Value::as_str) == Some("pullRequest"))
        };
        if !errors.iter().all(not_found) {
            return Err(answered_with(&errors));
        }
    }
    let repository = &value["data"]["repository"];
    if !repository.is_object() {
        return Err(ForgeError::Failed(
            "GitHub did not return the repository".into(),
        ));
    }
    let node = &repository["pullRequest"];
    if node.is_null() {
        return Ok(None);
    }
    let viewer = value["data"]["viewer"]["login"].as_str();
    parse_pull_request(node, viewer)
        .map(Some)
        .ok_or_else(|| ForgeError::Failed("GitHub returned an unreadable pull request".into()))
}

/// The open pull requests from and into the branch at `index` of a response to a
/// [`branches_query`].
///
/// # Errors
/// * [`ForgeError::Failed`] when the response reports errors or lacks the repository.
pub fn parse_branch_pull_requests(
    value: &Value,
    index: usize,
) -> Result<BranchPullRequests, ForgeError> {
    Ok(BranchPullRequests {
        with_head: connection_pull_requests(value, &format!("h{index}"))?,
        with_base: connection_pull_requests(value, &format!("c{index}"))?,
    })
}

/// The pull requests (`p0` to `p<count - 1>`) of a response to [`numbers_query`]; `None` for a
/// number GitHub reports it does not know.
///
/// # Errors
/// * [`ForgeError::Failed`] when the response reports other errors, lacks the repository, or
///   holds an unreadable pull request.
pub fn parse_numbered_pull_requests(
    value: &Value,
    count: usize,
) -> Result<Vec<Option<PullRequest>>, ForgeError> {
    if let Some(errors) = response_errors(value) {
        let not_found = |error: &&Value| {
            error["type"].as_str() == Some("NOT_FOUND")
                && error["path"].as_array().is_some_and(|path| {
                    path.last()
                        .and_then(Value::as_str)
                        .is_some_and(|alias| alias.starts_with('p'))
                })
        };
        if !errors.iter().all(not_found) {
            return Err(answered_with(&errors));
        }
    }
    let repository = &value["data"]["repository"];
    if !repository.is_object() {
        return Err(ForgeError::Failed(
            "GitHub did not return the repository".into(),
        ));
    }
    let viewer = value["data"]["viewer"]["login"].as_str();
    (0..count)
        .map(|index| {
            let node = &repository[format!("p{index}")];
            if node.is_null() {
                return Ok(None);
            }
            parse_pull_request(node, viewer).map(Some).ok_or_else(|| {
                ForgeError::Failed("GitHub returned an unreadable pull request".into())
            })
        })
        .collect()
}

/// The repository's default branch from a GraphQL response to [`PULL_REQUESTS_QUERY`] or
/// [`PULL_REQUEST_QUERY`], when it reports one (empty repositories have none).
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
    fn batched_branch_answers_are_read_per_branch() {
        let query = branches_query(2);
        for needle in [
            "$b0: String!",
            "$b1: String!",
            "h0: pullRequests",
            "c1: pullRequests",
        ] {
            assert!(query.contains(needle), "{needle}: {query}");
        }
        let node = |number: u64, base: &str, head: &str| json!({ "number": number, "state": "OPEN", "headRefName": head, "baseRefName": base });
        let value = json!({ "data": { "repository": {
            "h0": { "nodes": [node(1, "main", "a")] },
            "c0": { "nodes": [node(2, "a", "b")] },
            "h1": { "nodes": [] },
            "c1": { "nodes": [node(3, "b", "c"), node(4, "b", "d")] },
        } } });
        let first = parse_branch_pull_requests(&value, 0).unwrap();
        assert_eq!(first.with_head[0].number, 1);
        assert_eq!(first.with_base[0].number, 2);
        let second = parse_branch_pull_requests(&value, 1).unwrap();
        assert_eq!(second.with_head, Vec::new());
        assert_eq!(second.with_base.len(), 2);
        assert!(parse_branch_pull_requests(&value, 2).is_err());
    }

    #[test]
    fn pull_requests_by_number_are_read_per_alias_with_unknown_numbers_absent() {
        let query = numbers_query(2);
        for needle in [
            "$n0: Int!",
            "$n1: Int!",
            "p0: pullRequest(number: $n0)",
            "viewer",
        ] {
            assert!(query.contains(needle), "{needle}: {query}");
        }
        let value = json!({
            "data": { "viewer": { "login": "me" }, "repository": {
                "p0": { "number": 7, "state": "MERGED", "headRefName": "a", "baseRefName": "main" },
                "p1": null,
            } },
            "errors": [{ "type": "NOT_FOUND", "path": ["repository", "p1"], "message": "gone" }],
        });
        let pulls = parse_numbered_pull_requests(&value, 2).unwrap();
        assert_eq!(
            pulls[0].as_ref().map(|pull| (pull.number, pull.state)),
            Some((7, PullRequestState::Merged))
        );
        assert_eq!(pulls[1], None);
        // Any other error fails the batch.
        let failed = json!({ "data": { "repository": {} }, "errors": [{ "type": "RATE_LIMITED", "message": "slow down" }] });
        assert!(parse_numbered_pull_requests(&failed, 1).is_err());
    }

    #[test]
    fn chunks_are_requested_together_and_answered_in_order() {
        let started = std::sync::Barrier::new(3);
        let answers = each_chunk(&[1, 2, 3, 4, 5], 2, |chunk| {
            // Every chunk is in flight before any answers: three at once.
            started.wait();
            Ok(chunk.iter().sum::<i32>())
        })
        .unwrap();
        assert_eq!(answers, [3, 7, 5]);
        let failed = each_chunk(&[1, 2], 1, |chunk| {
            if chunk[0] == 2 {
                Err(ForgeError::Failed("no".into()))
            } else {
                Ok(())
            }
        });
        assert!(failed.is_err());
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
