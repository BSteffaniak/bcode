//! Pull requests from the repository's forge, matched to local branches.
//!
//! The client asks a [`PullRequestSource`] (supplied by the entry point, for example
//! `bcode_forge::GitHub`) in the background: at startup, after fetch, pull and push, and on an
//! interval. Results are cached per repository, so they show immediately on the next launch.
//! A local branch matches the pull request whose head is its upstream branch (or its own name
//! without one) in the same repository, preferring an open one. A branch counts as merged by
//! its pull request only when it has nothing newer than what was merged.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bcode_forge_models::{
    ForgeError, PullRequest, PullRequestList, PullRequestSource, RepositoryRef, Transport,
};
use bcode_git::Repository;
use bcode_git::models::{Branch, Remote};
use serde::{Deserialize, Serialize};

/// What is known about the repository's pull requests, and why not more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullStatus {
    /// Turned off, or no source was supplied.
    Off,
    /// No remote names a forge repository.
    NoRemote,
    /// The remote's host is not one the source serves.
    Unsupported(String),
    /// Waiting for the first answer.
    Loading,
    /// Shown from the cache; a refresh is pending or failed.
    Cached,
    /// Up to date, fetched this way.
    Ready(Transport),
    /// The last request failed.
    Failed(String),
}

impl PullStatus {
    /// Short description for the status overview.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Off => "off".into(),
            Self::NoRemote => "no GitHub remote".into(),
            Self::Unsupported(host) => format!("{host} is not GitHub"),
            Self::Loading => "loading…".into(),
            Self::Cached => "cached".into(),
            Self::Ready(Transport::Cli) => "via gh".into(),
            Self::Ready(_) => "via the GitHub API".into(),
            Self::Failed(message) => format!("unavailable: {message}"),
        }
    }
}

/// A local branch's pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Matched {
    /// The pull request.
    pub pull: PullRequest,
    /// Whether the branch's work is merged through it.
    pub merged: bool,
}

/// Pull request state of the client.
pub struct PullRequests {
    source: Option<Arc<dyn PullRequestSource>>,
    /// Repository asked about.
    pub repository: Option<RepositoryRef>,
    /// Its pull requests, newest first.
    pub list: Vec<PullRequest>,
    /// Pull requests of local branches, by branch name.
    pub matched: BTreeMap<String, Matched>,
    /// Status.
    pub status: PullStatus,
    cache_dir: Option<PathBuf>,
    requested: Option<Instant>,
    loading: bool,
    /// Generation of the latest request.
    pub generation: u64,
    /// Generation of the latest branch matching.
    pub match_generation: u64,
}

impl std::fmt::Debug for PullRequests {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PullRequests")
            .field("repository", &self.repository)
            .field("status", &self.status)
            .field("count", &self.list.len())
            .finish_non_exhaustive()
    }
}

impl PullRequests {
    /// State asking `source` (none: turned off), caching in `cache_dir`.
    #[must_use]
    pub fn new(source: Option<Arc<dyn PullRequestSource>>, cache_dir: Option<PathBuf>) -> Self {
        Self {
            status: if source.is_some() {
                PullStatus::NoRemote
            } else {
                PullStatus::Off
            },
            source,
            repository: None,
            list: Vec::new(),
            matched: BTreeMap::new(),
            cache_dir,
            requested: None,
            loading: false,
            generation: 0,
            match_generation: 0,
        }
    }

    /// Follow the remote called `name` (else the first with a URL). Returns whether the
    /// repository changed, after loading any cached pull requests for it.
    pub fn set_remote(&mut self, remotes: &[Remote], name: &str) -> bool {
        let Some(source) = &self.source else {
            return false;
        };
        let repository = remotes
            .iter()
            .find(|remote| remote.name == name)
            .into_iter()
            .chain(remotes)
            .find_map(|remote| {
                remote
                    .fetch_url
                    .as_deref()
                    .and_then(RepositoryRef::from_remote_url)
            });
        if repository == self.repository {
            return false;
        }
        self.list.clear();
        self.matched.clear();
        self.requested = None;
        self.status = match &repository {
            None => PullStatus::NoRemote,
            Some(repository) if !source.serves(repository) => {
                PullStatus::Unsupported(repository.host.clone())
            }
            Some(repository) => match self
                .cache_file(repository)
                .and_then(|file| read_cache(&file))
            {
                Some(list) => {
                    self.list = list;
                    PullStatus::Cached
                }
                None => PullStatus::Loading,
            },
        };
        self.repository = repository;
        true
    }

    /// A request to make now, when one is due (or `force`d) and possible: the source,
    /// repository and generation to answer with.
    pub fn due(
        &mut self,
        refresh: Duration,
        force: bool,
    ) -> Option<(Arc<dyn PullRequestSource>, RepositoryRef, u64)> {
        let source = self.source.clone()?;
        let repository = self.repository.clone()?;
        if self.loading || !source.serves(&repository) {
            return None;
        }
        let elapsed = self.requested.is_none_or(|at| at.elapsed() >= refresh);
        if !(force || elapsed) {
            return None;
        }
        self.loading = true;
        self.requested = Some(Instant::now());
        self.generation += 1;
        Some((source, repository, self.generation))
    }

    /// Apply an answer. Returns whether pull requests changed (so branches should be matched
    /// again).
    pub fn answer(&mut self, generation: u64, result: Result<PullRequestList, ForgeError>) -> bool {
        if generation != self.generation {
            return false;
        }
        self.loading = false;
        match result {
            Ok(answer) => {
                self.status = PullStatus::Ready(answer.via);
                if let Some(file) = self
                    .repository
                    .as_ref()
                    .and_then(|repository| self.cache_file(repository))
                {
                    write_cache(&file, &answer.pull_requests);
                }
                let changed = self.list != answer.pull_requests;
                self.list = answer.pull_requests;
                changed
            }
            Err(error) => {
                self.status = PullStatus::Failed(error.to_string());
                false
            }
        }
    }

    fn cache_file(&self, repository: &RepositoryRef) -> Option<PathBuf> {
        let key = format!("{}/{}", repository.host, repository.full_name());
        Some(self.cache_dir.as_ref()?.join(format!(
            "{:016x}.json",
            crate::panels::fnv1a(key.as_bytes())
        )))
    }
}

impl crate::app::App {
    /// Ask the forge for pull requests when due (or `force`d, after fetch, pull and push).
    pub(crate) fn request_pull_requests(
        &mut self,
        force: bool,
    ) -> Option<bmux_tui_runtime::Command<crate::app::Msg>> {
        let refresh = Duration::from_secs(self.config.pull_requests.refresh_seconds.max(30));
        let (source, repository, generation) = self.pulls.due(refresh, force)?;
        self.sync_pull_status();
        self.spawn(
            Some("pull-requests"),
            move || source.pull_requests(&repository),
            move |result| crate::app::Msg::PullRequests(generation, result),
        )
    }

    /// Match local branches to the known pull requests (checking merges with git) in the
    /// background.
    pub(crate) fn match_pull_requests(
        &mut self,
    ) -> Option<bmux_tui_runtime::Command<crate::app::Msg>> {
        let repository = self.pulls.repository.clone()?;
        if self.pulls.list.is_empty() {
            return None;
        }
        self.pulls.match_generation += 1;
        let generation = self.pulls.match_generation;
        let repo = self.repo.clone();
        let branches = self.snapshot.branches.clone();
        let list = self.pulls.list.clone();
        self.spawn(
            Some("pull-request-matches"),
            move || match_branches(&repo, &repository, &branches, &list),
            move |matched| crate::app::Msg::PullRequestMatches(generation, matched),
        )
    }

    /// Apply the forge's answer.
    pub(crate) fn pull_requests_answered(
        &mut self,
        generation: u64,
        result: Result<PullRequestList, ForgeError>,
    ) -> bmux_tui_runtime::Update<crate::app::Msg> {
        let changed = self.pulls.answer(generation, result);
        self.sync_pull_status();
        let update = bmux_tui_runtime::Update::redraw();
        if changed {
            crate::app::UpdateExt::maybe(update, self.match_pull_requests())
        } else {
            update
        }
    }

    /// Apply branch matches.
    pub(crate) fn pull_requests_matched(
        &mut self,
        generation: u64,
        matched: BTreeMap<String, Matched>,
    ) -> bmux_tui_runtime::Update<crate::app::Msg> {
        if generation == self.pulls.match_generation {
            self.pulls.matched = matched;
            self.refresh_branch_colors();
        }
        bmux_tui_runtime::Update::redraw()
    }

    /// Recolor branches from the snapshot and their pull requests.
    pub(crate) fn refresh_branch_colors(&mut self) {
        self.cx.branches = self.cx.branches.with_branches(
            &self.snapshot.branches,
            &self.snapshot.merged,
            &self.pulls.matched,
        );
        self.sync_pull_status();
    }

    fn sync_pull_status(&mut self) {
        self.cx.pull_status =
            (self.pulls.status != PullStatus::Off).then(|| self.pulls.status.describe());
    }

    /// Open the selected branch's pull request in the browser.
    pub(crate) fn open_pull_request(&mut self) -> bmux_tui_runtime::Update<crate::app::Msg> {
        let Some(crate::app::Row::Branch(index)) = self.selected(crate::app::View::Branches) else {
            return bmux_tui_runtime::Update::none();
        };
        let name = self.snapshot.branches[index].name.clone();
        match self.cx.branches.pull(&name).cloned() {
            Some(pull) => match self.host.open_url(&pull.url) {
                Ok(()) => self.toast(format!("Opened #{} {}", pull.number, pull.title), false),
                Err(error) => self.toast(format!("Could not open {}: {error}", pull.url), true),
            },
            None => self.toast(
                format!(
                    "No pull request for {name} ({})",
                    self.pulls.status.describe()
                ),
                false,
            ),
        }
        bmux_tui_runtime::Update::redraw()
    }
}

/// Index into [`crate::theme::GitTheme::pull_states`].
const fn state_index(state: bcode_forge_models::PullRequestState) -> usize {
    match state {
        bcode_forge_models::PullRequestState::Open => 0,
        bcode_forge_models::PullRequestState::Draft => 1,
        bcode_forge_models::PullRequestState::Merged => 2,
        bcode_forge_models::PullRequestState::Closed => 3,
    }
}

/// Compact marker after a branch name: ` #12 open ✓ ✔` (number and state, then checks and
/// review when known).
#[must_use]
pub fn marker(pull: &PullRequest, theme: &crate::theme::GitTheme) -> Vec<bmux_tui::text::Span> {
    use bcode_forge_models::{ChecksState, ReviewDecision};
    use bmux_tui::text::Span;

    let mut spans = vec![Span::styled(
        format!(" #{} {}", pull.number, pull.state.name()),
        theme.pull_states[state_index(pull.state)],
    )];
    match pull.checks {
        Some(ChecksState::Passing) => spans.push(Span::styled(" ✓", theme.success)),
        Some(ChecksState::Failing) => spans.push(Span::styled(" ✗", theme.error)),
        Some(ChecksState::Pending) => spans.push(Span::styled(" ●", theme.warning)),
        None => {}
    }
    match pull.review {
        Some(ReviewDecision::Approved) => spans.push(Span::styled(" ✔", theme.success)),
        Some(ReviewDecision::ChangesRequested) => spans.push(Span::styled(" ✎", theme.warning)),
        Some(ReviewDecision::ReviewRequired) | None => {}
    }
    spans
}

/// Full description: `#12 Add login · open · approved · checks passing`.
#[must_use]
pub fn summary(pull: &PullRequest, theme: &crate::theme::GitTheme) -> Vec<bmux_tui::text::Span> {
    use bmux_tui::text::Span;

    let mut spans = vec![
        Span::styled(
            format!("#{} ", pull.number),
            theme.pull_states[state_index(pull.state)],
        ),
        Span::styled(pull.title.clone(), theme.text),
        Span::styled(" · ", theme.muted),
        Span::styled(
            pull.state.name(),
            theme.pull_states[state_index(pull.state)],
        ),
    ];
    if let Some(review) = pull.review {
        spans.push(Span::styled(
            format!(" · {}", review.name().replace('-', " ")),
            theme.muted,
        ));
    }
    if let Some(checks) = pull.checks {
        let style = match checks {
            bcode_forge_models::ChecksState::Passing => theme.success,
            bcode_forge_models::ChecksState::Failing => theme.error,
            bcode_forge_models::ChecksState::Pending => theme.warning,
        };
        spans.push(Span::styled(format!(" · checks {}", checks.name()), style));
    }
    spans
}

/// Legend lines for the pull request markers.
#[must_use]
pub fn legend(theme: &crate::theme::GitTheme) -> Vec<bmux_tui::text::Line> {
    use bmux_tui::text::{Line, Span};

    let states = [
        bcode_forge_models::PullRequestState::Open,
        bcode_forge_models::PullRequestState::Draft,
        bcode_forge_models::PullRequestState::Merged,
        bcode_forge_models::PullRequestState::Closed,
    ];
    let mut spans = vec![Span::styled("#N ", theme.muted)];
    for state in states {
        spans.push(Span::styled(
            format!("{} ", state.name()),
            theme.pull_states[state_index(state)],
        ));
    }
    spans.push(Span::styled(" pull request", theme.muted));
    vec![
        Line::from_spans(spans),
        Line::from_spans(vec![
            Span::styled("✓", theme.success),
            Span::styled(" ✗", theme.error),
            Span::styled(" ●", theme.warning),
            Span::styled("  checks passing, failing, running   ", theme.muted),
            Span::styled("✔", theme.success),
            Span::styled(" ✎", theme.warning),
            Span::styled("  approved, changes requested", theme.muted),
        ]),
    ]
}

/// The default cache directory: `$XDG_CACHE_HOME/bcode-git/pull-requests` (or
/// `~/.cache/…`).
#[must_use]
pub fn default_cache_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    Some(base.join("bcode-git").join("pull-requests"))
}

#[derive(Serialize, Deserialize)]
struct Cache {
    version: u32,
    pull_requests: Vec<PullRequest>,
}

fn read_cache(file: &Path) -> Option<Vec<PullRequest>> {
    let cache: Cache = serde_json::from_str(&std::fs::read_to_string(file).ok()?).ok()?;
    (cache.version == 1).then_some(cache.pull_requests)
}

fn write_cache(file: &Path, pull_requests: &[PullRequest]) {
    let cache = Cache {
        version: 1,
        pull_requests: pull_requests.to_vec(),
    };
    if let (Some(dir), Ok(text)) = (file.parent(), serde_json::to_string(&cache)) {
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::write(file, text);
    }
}

/// The head branch name a local branch's pull request would have: its upstream branch, else
/// its own name.
#[must_use]
pub fn head_name(branch: &Branch) -> &str {
    branch
        .upstream
        .as_deref()
        .and_then(|upstream| upstream.split_once('/').map(|(_, name)| name))
        .unwrap_or(&branch.name)
}

/// The pull request for head branch `head` in `repository`: an open one, else the most
/// recently updated. Pull requests from forks are ignored.
#[must_use]
pub fn find<'a>(
    pulls: &'a [PullRequest],
    repository: &RepositoryRef,
    head: &str,
) -> Option<&'a PullRequest> {
    let full_name = repository.full_name();
    let mut candidates = pulls.iter().filter(|pull| {
        pull.head == head
            && pull
                .head_repository
                .as_deref()
                .is_none_or(|owner| owner.eq_ignore_ascii_case(&full_name))
    });
    let first = candidates.clone().next();
    candidates.find(|pull| pull.state.is_open()).or(first)
}

/// Pull requests of local branches. A branch is merged through its pull request when the
/// pull request was merged and the branch is at, or behind, the commit that was merged.
#[must_use]
pub fn match_branches(
    repo: &Repository,
    repository: &RepositoryRef,
    branches: &[Branch],
    pulls: &[PullRequest],
) -> BTreeMap<String, Matched> {
    branches
        .iter()
        .filter_map(|branch| {
            let pull = find(pulls, repository, head_name(branch))?;
            let merged = pull.state == bcode_forge_models::PullRequestState::Merged
                && (branch.oid == pull.head_oid
                    || repo
                        .runner()
                        .read(["merge-base", "--is-ancestor", &branch.oid, &pull.head_oid])
                        .is_ok());
            Some((
                branch.name.clone(),
                Matched {
                    pull: pull.clone(),
                    merged,
                },
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use bcode_forge_models::PullRequestState;

    use super::*;

    fn pull(number: u64, head: &str, state: PullRequestState, from: Option<&str>) -> PullRequest {
        PullRequest {
            number,
            title: format!("#{number}"),
            url: String::new(),
            state,
            head: head.into(),
            head_repository: from.map(str::to_owned),
            head_oid: String::new(),
            base: "main".into(),
            review: None,
            checks: None,
            updated_at: String::new(),
        }
    }

    fn repository() -> RepositoryRef {
        RepositoryRef {
            host: "github.com".into(),
            owner: "octo".into(),
            name: "hello".into(),
        }
    }

    #[test]
    fn open_pull_requests_win_and_forks_are_ignored() {
        let pulls = [
            pull(9, "feature", PullRequestState::Closed, Some("octo/hello")),
            pull(8, "feature", PullRequestState::Open, Some("Octo/Hello")),
            pull(7, "fork", PullRequestState::Open, Some("someone/hello")),
            pull(6, "plain", PullRequestState::Merged, None),
        ];
        assert_eq!(
            find(&pulls, &repository(), "feature").map(|pull| pull.number),
            Some(8)
        );
        assert_eq!(find(&pulls, &repository(), "fork"), None);
        assert_eq!(
            find(&pulls, &repository(), "plain").map(|pull| pull.number),
            Some(6)
        );
        let closed = [
            pull(9, "x", PullRequestState::Closed, None),
            pull(8, "x", PullRequestState::Merged, None),
        ];
        // No open one: the most recently updated (listed first).
        assert_eq!(
            find(&closed, &repository(), "x").map(|pull| pull.number),
            Some(9)
        );
    }

    #[test]
    fn heads_are_the_upstream_branch_or_the_local_name() {
        let mut branch = Branch {
            name: "local-name".into(),
            oid: String::new(),
            is_head: false,
            upstream: Some("origin/feature/x".into()),
            ahead: 0,
            behind: 0,
            upstream_gone: false,
            commit_time: 0,
            summary: String::new(),
            worktree: None,
        };
        assert_eq!(head_name(&branch), "feature/x");
        branch.upstream = None;
        assert_eq!(head_name(&branch), "local-name");
    }

    struct Fixed(Vec<PullRequest>);

    impl PullRequestSource for Fixed {
        fn serves(&self, repository: &RepositoryRef) -> bool {
            repository.host == "github.com"
        }

        fn pull_requests(&self, _: &RepositoryRef) -> Result<PullRequestList, ForgeError> {
            Ok(PullRequestList {
                pull_requests: self.0.clone(),
                via: Transport::Http,
            })
        }
    }

    fn remote(url: &str) -> Remote {
        Remote {
            name: "origin".into(),
            fetch_url: Some(url.into()),
            push_url: None,
        }
    }

    #[test]
    fn requests_are_due_on_an_interval_and_answers_are_cached() {
        let cache = tempfile::tempdir().unwrap();
        let source: Arc<dyn PullRequestSource> =
            Arc::new(Fixed(vec![pull(1, "x", PullRequestState::Open, None)]));
        let mut pulls = PullRequests::new(Some(source.clone()), Some(cache.path().to_owned()));
        assert!(pulls.set_remote(&[remote("git@github.com:octo/hello.git")], "origin"));
        assert_eq!(pulls.status, PullStatus::Loading);
        let (source_now, repository, generation) =
            pulls.due(Duration::from_secs(60), false).unwrap();
        // One request at a time, and not again until the interval passes.
        assert!(pulls.due(Duration::from_secs(60), true).is_none());
        assert!(pulls.answer(generation, source_now.pull_requests(&repository)));
        assert_eq!(pulls.status, PullStatus::Ready(Transport::Http));
        assert!(pulls.due(Duration::from_secs(60), false).is_none());
        assert!(pulls.due(Duration::from_secs(60), true).is_some());
        // A new session shows the cached answer before asking again.
        let mut again = PullRequests::new(Some(source), Some(cache.path().to_owned()));
        again.set_remote(&[remote("https://github.com/octo/hello")], "origin");
        assert_eq!(again.status, PullStatus::Cached);
        assert_eq!(again.list.len(), 1);
    }

    #[test]
    fn other_hosts_and_missing_remotes_are_reported() {
        let source: Arc<dyn PullRequestSource> = Arc::new(Fixed(Vec::new()));
        let mut pulls = PullRequests::new(Some(source), None);
        pulls.set_remote(&[remote("git@gitlab.com:octo/hello.git")], "origin");
        assert_eq!(pulls.status, PullStatus::Unsupported("gitlab.com".into()));
        assert!(pulls.due(Duration::ZERO, true).is_none());
        pulls.set_remote(&[], "origin");
        assert_eq!(pulls.status, PullStatus::NoRemote);
        assert_eq!(PullRequests::new(None, None).status, PullStatus::Off);
    }

    #[test]
    fn stale_answers_and_failures_keep_what_is_known() {
        let source: Arc<dyn PullRequestSource> = Arc::new(Fixed(Vec::new()));
        let mut pulls = PullRequests::new(Some(source), None);
        pulls.set_remote(&[remote("git@github.com:octo/hello.git")], "origin");
        let (_, _, generation) = pulls.due(Duration::ZERO, true).unwrap();
        pulls.list = vec![pull(1, "x", PullRequestState::Open, None)];
        assert!(!pulls.answer(generation + 1, Err(ForgeError::Failed("old".into()))));
        assert!(!pulls.answer(generation, Err(ForgeError::Failed("offline".into()))));
        assert_eq!(pulls.status, PullStatus::Failed("offline".into()));
        assert_eq!(pulls.list.len(), 1);
    }
}
