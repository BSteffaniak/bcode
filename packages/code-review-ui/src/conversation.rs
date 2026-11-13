//! Pull request conversations in the review.
//!
//! Which pull requests the review's sources are, what each one's conversation is (as last
//! read), when to read them again, and the provider calls that read and change them.
//! Provider-neutral: a provider is only an id, a label from its manifest, and the conversation
//! capabilities it declares. See `docs/review-forge-conversation.md`.
//!
//! **Freshness.** Nothing here polls. A read happens when a signal names pull requests:
//! opening, regaining focus, the viewer's own changes, a moved branch, the refresh button, or
//! the provider's watch saying they changed. Signals within the configured gap of the last read
//! are merged into one later read, so a burst of changes is one provider call.

use crate::config::conversation::{ConversationConfig, NotifyMode};
use crate::notes::{RemoteNote, RemoteThreadInfo};
use crate::services::ReviewServices;
use bcode_code_review::conversation::{self as core, LayerTarget, Placement};
use bcode_code_review::document::ReviewDocument;
use bcode_code_review_models::{
    CommentPostRequest, CommentPostedResponse, ConversationCapabilities, PullActivity, PullLocator,
    PullReview, RemoteComment, ReviewPreviewResponse, ReviewSource, ReviewSourceKind,
    ReviewSourcePullsWatchRequest, ReviewSourcePullsWatchResponse, ReviewState,
    ReviewSubmitRequest, ReviewSubmitResponse, ThreadReplyRequest, ThreadResolveRequest,
    ThreadResolveResponse,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Longest a provider's watch waits before answering (it is then asked again).
pub const WATCH_WAIT: Duration = Duration::from_secs(120);

/// One pull request of a provider's repository.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PullKey {
    /// Provider id.
    pub provider: String,
    /// Provider repository.
    pub repository: String,
    /// Number.
    pub number: u64,
}

impl PullKey {
    /// `provider/repository#number`: the key of seen markers and remote thread ids.
    #[must_use]
    pub fn id(&self) -> String {
        format!("{}/{}#{}", self.provider, self.repository, self.number)
    }

    /// The pull request as a provider request names it.
    #[must_use]
    pub fn locator(&self, repo_path: &std::path::Path) -> PullLocator {
        PullLocator {
            repo_path: repo_path.to_path_buf(),
            repository: Some(self.repository.clone()),
            number: self.number,
        }
    }
}

/// A pull request one of the review's source groups shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayerLink {
    /// Review source id of the group.
    pub source_id: String,
    /// The pull request.
    pub pull: PullKey,
    /// Head commit the review shows for it.
    pub head_oid: String,
    /// Paths the group changes.
    pub paths: BTreeSet<String>,
    /// Pull request title as the review knows it.
    pub title: String,
}

/// The pull requests of the review's sources, in document order. A whole-stack source shows
/// every layer of its stack; its threads are placed on the whole diff.
#[must_use]
pub fn links(doc: &ReviewDocument, sources: &[ReviewSource]) -> Vec<LayerLink> {
    let mut links = Vec::new();
    for group in doc.groups() {
        let Some(source) = sources
            .iter()
            .find(|source| source.id == group.source.source_id)
        else {
            continue;
        };
        let Some(head) = group.source.head.commit_oid() else {
            continue;
        };
        let paths: BTreeSet<String> = doc
            .group_files(group.index)
            .map(|file| file.path.clone())
            .collect();
        let link = |provider: &str, repository: &str, number: u64, title: String| LayerLink {
            source_id: group.source.source_id.clone(),
            pull: PullKey {
                provider: provider.to_owned(),
                repository: repository.to_owned(),
                number,
            },
            head_oid: head.to_owned(),
            paths: paths.clone(),
            title,
        };
        let title = |number: u64| {
            group
                .stack
                .and_then(|stack| {
                    stack
                        .stack
                        .layers
                        .iter()
                        .find(|layer| layer.number == number)
                        .map(|layer| layer.title.clone())
                })
                .unwrap_or_else(|| group.label.to_owned())
        };
        match &source.kind {
            ReviewSourceKind::PullRequest {
                provider,
                repository,
                number,
                ..
            }
            | ReviewSourceKind::StackLayer {
                provider,
                repository,
                number,
                ..
            } => links.push(link(provider, repository, *number, title(*number))),
            ReviewSourceKind::Stack {
                provider,
                repository,
                ..
            } => {
                for layer in group.stack.map_or(&[][..], |stack| &stack.stack.layers) {
                    links.push(link(
                        provider,
                        repository,
                        layer.number,
                        layer.title.clone(),
                    ));
                }
            }
            _ => {}
        }
    }
    links
}

/// What a provider offers, from its manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderInfo {
    /// Display name.
    pub label: String,
    /// Conversation operations.
    pub capabilities: ConversationCapabilities,
}

/// One pull request's conversation as last read.
#[derive(Debug, Clone, Default)]
pub struct PullEntry {
    /// The conversation.
    pub activity: Option<PullActivity>,
    /// When it was read (from the provider, or the cache's time).
    pub read_at: Option<std::time::SystemTime>,
    /// The last read failed.
    pub error: Option<String>,
}

/// A provider watch in progress or waiting to be asked again.
#[derive(Debug, Clone, Default)]
struct Watch {
    cursor: Option<String>,
    running: bool,
    /// Failures in a row (the next watch waits longer).
    failures: u32,
}

/// The review's pull request conversations.
#[derive(Debug, Default)]
pub struct Conversations {
    /// The review's pull requests, in document order.
    pub links: Vec<LayerLink>,
    /// Each pull request's conversation.
    pub pulls: BTreeMap<PullKey, PullEntry>,
    /// Providers of the pull requests.
    pub providers: BTreeMap<String, ProviderInfo>,
    /// Changes whenever anything shown changed.
    pub revision: u64,
    /// Pull requests a signal named, not read since.
    dirty: BTreeSet<PullKey>,
    /// A read is in flight.
    reading: bool,
    /// When the last read started.
    last_read: Option<Instant>,
    /// A delayed read is scheduled.
    due: bool,
    watches: BTreeMap<(String, String), Watch>,
    /// The terminal is not focused.
    pub unfocused: bool,
    /// The overview's open state the viewer chose, per pull request.
    pub opened: BTreeMap<PullKey, bool>,
}

/// A pull request's conversation as the cache held it, and when it was read.
pub type CachedActivity = (PullKey, PullActivity, std::time::SystemTime);

/// How stale a saved snapshot's read time may get before an unchanged snapshot is saved again
/// (to say it was read again).
pub const SNAPSHOT_REFRESH_MS: u64 = 60_000;

/// The pull request threads saved for the review tools: where, the threads, and when the app
/// read them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadsSnapshot {
    /// The review's scope.
    pub scope: crate::notes::NoteScope,
    /// The placed threads.
    pub threads: Vec<bcode_code_review_models::PullThreadRecord>,
    /// When the app read them (its oldest read of the pull requests), in milliseconds since the
    /// Unix epoch.
    pub read_at_ms: u64,
}

impl ThreadsSnapshot {
    /// Whether this snapshot says more than `kept`: other threads or scope, or a read time at
    /// least [`SNAPSHOT_REFRESH_MS`] newer.
    #[must_use]
    pub fn news_over(&self, kept: &Self) -> bool {
        self.scope != kept.scope
            || self.threads != kept.threads
            || self.read_at_ms >= kept.read_at_ms.saturating_add(SNAPSHOT_REFRESH_MS)
    }
}

/// Saves of the pull request threads, one at a time and in order.
///
/// A snapshot offered while one is being saved waits (replacing any older one waiting), and a
/// failed save leaves the last stored snapshot as the one to beat, so the next offer saves again.
#[derive(Debug, Default)]
pub struct ThreadSaves {
    /// The last snapshot the store confirmed.
    stored: Option<ThreadsSnapshot>,
    /// The snapshot being saved.
    saving: Option<ThreadsSnapshot>,
    /// The newest snapshot waiting for the save in flight.
    waiting: Option<ThreadsSnapshot>,
}

impl ThreadSaves {
    /// Offer `snapshot`; the snapshot to save now, if any.
    pub fn offer(&mut self, snapshot: ThreadsSnapshot) -> Option<ThreadsSnapshot> {
        let latest = self
            .waiting
            .as_ref()
            .or(self.saving.as_ref())
            .or(self.stored.as_ref());
        if latest.is_some_and(|latest| !snapshot.news_over(latest)) {
            return None;
        }
        if self.saving.is_some() {
            self.waiting = Some(snapshot);
            return None;
        }
        self.saving = Some(snapshot.clone());
        Some(snapshot)
    }

    /// The save in flight finished (`saved` when the store kept it); the snapshot to save next,
    /// if one waits.
    pub fn finished(&mut self, saved: bool) -> Option<ThreadsSnapshot> {
        let done = self.saving.take();
        if saved {
            self.stored = done;
        }
        let next = self.waiting.take()?;
        self.saving = Some(next.clone());
        Some(next)
    }

    /// Whether a save is in flight.
    #[must_use]
    pub const fn is_saving(&self) -> bool {
        self.saving.is_some()
    }
}

/// A provider watch to ask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchStart {
    /// Provider.
    pub provider: String,
    /// Repository.
    pub repository: String,
    /// Pull requests.
    pub numbers: Vec<u64>,
    /// The previous answer's cursor.
    pub cursor: Option<String>,
    /// Failures in a row so far (the ask waits longer).
    pub failures: u32,
}

/// What to do after a signal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    /// Nothing yet.
    Wait,
    /// Read these pull requests now.
    Read(Vec<PullKey>),
    /// Ask again after this long.
    After(Duration),
}

impl Conversations {
    /// Conversations the cache held: shown for pull requests not read yet, as read when the
    /// cache read them.
    pub fn read_cached(&mut self, found: Vec<CachedActivity>) {
        let first: Vec<(PullKey, std::time::SystemTime)> = found
            .iter()
            .filter(|(key, _, _)| {
                self.pulls
                    .get(key)
                    .is_none_or(|entry| entry.activity.is_none())
            })
            .map(|(key, _, read_at)| (key.clone(), *read_at))
            .collect();
        let keys: Vec<PullKey> = found.iter().map(|(key, _, _)| key.clone()).collect();
        let activities = found.into_iter().map(|(_, activity, _)| activity).collect();
        self.read(&keys, Ok(activities), true);
        for (key, read_at) in first {
            if let Some(entry) = self.pulls.get_mut(&key) {
                entry.read_at = Some(read_at);
            }
        }
    }

    /// When the pull requests with a conversation were read, as of the oldest of them, in
    /// milliseconds since the Unix epoch.
    #[must_use]
    pub fn read_at_ms(&self) -> Option<u64> {
        self.pulls
            .values()
            .filter(|entry| entry.activity.is_some())
            .filter_map(|entry| entry.read_at)
            .min()
            .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|since| u64::try_from(since.as_millis()).ok())
    }

    /// The pull requests whose providers report conversations.
    #[must_use]
    pub fn readable(&self) -> Vec<PullKey> {
        let mut keys: Vec<PullKey> = self
            .links
            .iter()
            .filter(|link| {
                self.providers
                    .get(&link.pull.provider)
                    .is_some_and(|provider| provider.capabilities.activity)
            })
            .map(|link| link.pull.clone())
            .collect();
        keys.sort();
        keys.dedup();
        keys
    }

    /// Note that `keys` may have changed; returns what to do (given the minimum gap between
    /// reads).
    pub fn signal(&mut self, keys: impl IntoIterator<Item = PullKey>, gap: Duration) -> Next {
        let readable: BTreeSet<PullKey> = self.readable().into_iter().collect();
        self.dirty
            .extend(keys.into_iter().filter(|key| readable.contains(key)));
        self.next(gap)
    }

    /// What to do now, given what is dirty and when the last read was.
    pub fn next(&mut self, gap: Duration) -> Next {
        if self.dirty.is_empty() || self.reading {
            return Next::Wait;
        }
        let since = self.last_read.map(|at| at.elapsed());
        match since {
            Some(since) if since < gap => {
                if self.due {
                    Next::Wait
                } else {
                    self.due = true;
                    Next::After(gap.saturating_sub(since))
                }
            }
            _ => {
                self.reading = true;
                self.due = false;
                self.last_read = Some(Instant::now());
                Next::Read(std::mem::take(&mut self.dirty).into_iter().collect())
            }
        }
    }

    /// A delayed read is due.
    pub const fn due(&mut self) {
        self.due = false;
    }

    /// A read finished with `activities` (or failed for `keys`). Returns what changed: the new
    /// reviews and comments, for notifications.
    pub fn read(
        &mut self,
        keys: &[PullKey],
        result: Result<Vec<PullActivity>, String>,
        cached: bool,
    ) -> Vec<(PullKey, Arrival)> {
        if !cached {
            self.reading = false;
        }
        let mut arrivals = Vec::new();
        match result {
            Ok(activities) => {
                for activity in activities {
                    let Some(key) = keys.iter().find(|key| key.number == activity.number) else {
                        continue;
                    };
                    let entry = self.pulls.entry(key.clone()).or_default();
                    if cached && entry.activity.is_some() {
                        continue;
                    }
                    if let Some(before) = &entry.activity {
                        arrivals.extend(
                            arrivals_between(before, &activity)
                                .into_iter()
                                .map(|arrival| (key.clone(), arrival)),
                        );
                    }
                    entry.activity = Some(activity);
                    entry.read_at = Some(std::time::SystemTime::now());
                    entry.error = None;
                }
            }
            Err(error) => {
                for key in keys {
                    self.pulls.entry(key.clone()).or_default().error = Some(error.clone());
                }
            }
        }
        self.revision += 1;
        arrivals
    }

    /// The conversation of `key`, when read.
    #[must_use]
    pub fn activity(&self, key: &PullKey) -> Option<&PullActivity> {
        self.pulls.get(key)?.activity.as_ref()
    }

    /// The pull request a source group shows (its top layer for a whole stack).
    #[must_use]
    pub fn link_of(&self, source_id: &str) -> Option<&LayerLink> {
        self.links
            .iter()
            .rev()
            .find(|link| link.source_id == source_id)
    }

    /// The provider of `key`.
    #[must_use]
    pub fn provider(&self, key: &PullKey) -> Option<&ProviderInfo> {
        self.providers.get(&key.provider)
    }

    /// Provider repositories to watch, with their pull requests: those whose provider watches
    /// and whose watch is not running.
    pub fn watches_to_start(&mut self, enabled: bool) -> Vec<WatchStart> {
        if !enabled || self.unfocused {
            return Vec::new();
        }
        let mut wanted: BTreeMap<(String, String), Vec<u64>> = BTreeMap::new();
        for key in self.readable() {
            if self
                .providers
                .get(&key.provider)
                .is_some_and(|provider| provider.capabilities.watch.is_some())
            {
                wanted
                    .entry((key.provider.clone(), key.repository.clone()))
                    .or_default()
                    .push(key.number);
            }
        }
        let mut start = Vec::new();
        for (repository, numbers) in wanted {
            let watch = self.watches.entry(repository.clone()).or_default();
            if watch.running {
                continue;
            }
            watch.running = true;
            start.push(WatchStart {
                provider: repository.0,
                repository: repository.1,
                numbers,
                cursor: watch.cursor.clone(),
                failures: watch.failures,
            });
        }
        start
    }

    /// A watch answered: the pull requests it says changed.
    pub fn watched(
        &mut self,
        provider: &str,
        repository: &str,
        result: Result<ReviewSourcePullsWatchResponse, String>,
    ) -> Vec<PullKey> {
        let watch = self
            .watches
            .entry((provider.to_owned(), repository.to_owned()))
            .or_default();
        watch.running = false;
        let Ok(answer) = result else {
            watch.failures = watch.failures.saturating_add(1);
            return Vec::new();
        };
        watch.cursor = Some(answer.cursor);
        watch.failures = 0;
        answer
            .changed
            .into_iter()
            .map(|number| PullKey {
                provider: provider.to_owned(),
                repository: repository.to_owned(),
                number,
            })
            .collect()
    }

    /// Whether the overview of `key` shows open (`seen` is the newest activity time the viewer
    /// saw on it).
    #[must_use]
    pub fn overview_open(
        &self,
        key: &PullKey,
        mode: crate::config::conversation::OverviewMode,
        seen: Option<u64>,
    ) -> bool {
        use crate::config::conversation::OverviewMode;
        if let Some(open) = self.opened.get(key) {
            return *open;
        }
        match mode {
            OverviewMode::Collapsed => false,
            OverviewMode::Expanded => true,
            OverviewMode::Auto => self
                .activity(key)
                .is_some_and(|activity| is_new(activity, seen)),
        }
    }
}

/// Whether anything in `activity` (besides the viewer's own) is newer than `seen`.
#[must_use]
pub fn is_new(activity: &PullActivity, seen: Option<u64>) -> bool {
    let Some(seen) = seen else {
        // Never looked: what is there is the starting point, not news.
        return false;
    };
    newest_from_others(activity) > seen
}

/// The newest time someone other than the viewer did something in `activity`.
#[must_use]
pub fn newest_from_others(activity: &PullActivity) -> u64 {
    let reviews = activity
        .reviews
        .iter()
        .filter(|review| !review.author.is_viewer)
        .filter_map(|review| review.at_ms);
    let comments = activity
        .threads
        .iter()
        .flat_map(|thread| &thread.comments)
        .chain(&activity.comments)
        .filter(|comment| !comment.author.is_viewer)
        .filter_map(|comment| comment.updated_at_ms.or(comment.created_at_ms));
    reviews.chain(comments).max().unwrap_or(0)
}

/// Something that arrived between two reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arrival {
    /// A review.
    Review(PullReview),
    /// A comment; `involves_viewer` when it is in a thread the viewer took part in or on the
    /// viewer's pull request.
    Comment {
        /// The comment.
        comment: RemoteComment,
        /// In an inline thread (its id), else in the conversation.
        thread: Option<String>,
        /// It concerns the viewer.
        involves_viewer: bool,
    },
}

/// What `after` has that `before` did not (from others than the viewer).
fn arrivals_between(before: &PullActivity, after: &PullActivity) -> Vec<Arrival> {
    let known_reviews: BTreeSet<&str> = before
        .reviews
        .iter()
        .map(|review| review.id.as_str())
        .collect();
    let known_comments: BTreeSet<&str> = before
        .threads
        .iter()
        .flat_map(|thread| &thread.comments)
        .chain(&before.comments)
        .map(|comment| comment.id.as_str())
        .collect();
    let mut arrivals: Vec<Arrival> = after
        .reviews
        .iter()
        .filter(|review| {
            !review.author.is_viewer
                && review.state != ReviewState::Pending
                && !known_reviews.contains(review.id.as_str())
        })
        .cloned()
        .map(Arrival::Review)
        .collect();
    let author = after.viewer.is_author;
    let comments = &mut arrivals;
    for thread in &after.threads {
        let viewer_in = thread
            .comments
            .iter()
            .any(|comment| comment.author.is_viewer);
        for comment in &thread.comments {
            if !comment.author.is_viewer && !known_comments.contains(comment.id.as_str()) {
                comments.push(Arrival::Comment {
                    comment: comment.clone(),
                    thread: Some(thread.id.clone()),
                    involves_viewer: viewer_in || author,
                });
            }
        }
    }
    for comment in &after.comments {
        if !comment.author.is_viewer && !known_comments.contains(comment.id.as_str()) {
            comments.push(Arrival::Comment {
                comment: comment.clone(),
                thread: None,
                involves_viewer: author
                    || comment.body.contains(&format!("@{}", viewer_login(after))),
            });
        }
    }
    arrivals
}

/// The viewer's login, when the conversation shows it.
fn viewer_login(activity: &PullActivity) -> String {
    activity
        .reviews
        .iter()
        .map(|review| &review.author)
        .chain(
            activity
                .threads
                .iter()
                .flat_map(|thread| &thread.comments)
                .chain(&activity.comments)
                .map(|comment| &comment.author),
        )
        .chain(activity.author.iter())
        .find(|participant| participant.is_viewer)
        .map_or_else(
            || "\u{0}".to_owned(),
            |participant| participant.login.clone(),
        )
}

/// Whether `arrival` makes a toast under `mode`.
#[must_use]
pub const fn notifies(mode: NotifyMode, arrival: &Arrival) -> bool {
    match (mode, arrival) {
        (NotifyMode::None, _) | (NotifyMode::Reviews, Arrival::Comment { .. }) => false,
        (
            NotifyMode::All | NotifyMode::Reviews | NotifyMode::ReviewsAndReplies,
            Arrival::Review(_),
        )
        | (NotifyMode::All, Arrival::Comment { .. }) => true,
        (
            NotifyMode::ReviewsAndReplies,
            Arrival::Comment {
                involves_viewer, ..
            },
        ) => *involves_viewer,
    }
}

/// The toast text of `arrival` on `key`.
#[must_use]
pub fn arrival_text(key: &PullKey, arrival: &Arrival) -> String {
    match arrival {
        Arrival::Review(review) => format!(
            "{} {} #{}",
            review.author.login,
            match review.state {
                ReviewState::Approved => "approved",
                ReviewState::ChangesRequested => "requested changes on",
                ReviewState::Dismissed => "dismissed a review on",
                ReviewState::Commented | ReviewState::Pending => "reviewed",
            },
            key.number
        ),
        Arrival::Comment {
            comment, thread, ..
        } => format!(
            "{} {} on #{}",
            comment.author.login,
            if thread.is_some() {
                "replied in a thread"
            } else {
                "commented"
            },
            key.number
        ),
    }
}

/// The remote threads of every link, placed on the review.
#[must_use]
pub fn remote_notes(
    placed: &[(PullKey, core::PlacedThread)],
    conversations: &Conversations,
    seen: &BTreeMap<String, u64>,
    config: &ConversationConfig,
) -> Vec<RemoteNote> {
    let mut notes = Vec::new();
    for (key, placed) in placed {
        if !config.outdated_on_file && placed.placement == Placement::OnFile {
            continue;
        }
        let Some(activity) = conversations.activity(key) else {
            continue;
        };
        let thread = &placed.thread;
        let pull = key.id();
        let thread_id = crate::notes::remote_thread_id(&pull, &thread.id);
        let seen_at = seen.get(&pull).copied();
        let new = seen_at.is_some_and(|seen_at| {
            thread.comments.iter().any(|comment| {
                !comment.author.is_viewer
                    && comment.updated_at_ms.or(comment.created_at_ms).unwrap_or(0) > seen_at
            })
        });
        let comments = thread
            .comments
            .iter()
            .enumerate()
            .map(|(index, comment)| {
                (
                    format!("{thread_id}\u{1}{}", comment.id),
                    comment.body.clone(),
                    comment
                        .created_at_ms
                        .unwrap_or_else(|| u64::try_from(index).unwrap_or(u64::MAX)),
                )
            })
            .collect();
        let authors = thread
            .comments
            .iter()
            .map(|comment| {
                (
                    format!("{thread_id}\u{1}{}", comment.id),
                    (comment.author.login.clone(), comment.created_at_ms),
                )
            })
            .collect();
        let mine = thread
            .comments
            .iter()
            .filter(|comment| comment.author.is_viewer)
            .map(|comment| format!("{thread_id}\u{1}{}", comment.id))
            .collect();
        let capabilities = conversations
            .provider(key)
            .map(|provider| provider.capabilities)
            .unwrap_or_default();
        notes.push(RemoteNote {
            thread_id,
            anchor: placed.anchor.clone(),
            info: RemoteThreadInfo {
                pull,
                provider_label: conversations
                    .provider(key)
                    .map_or_else(|| key.provider.clone(), |provider| provider.label.clone()),
                remote_id: thread.id.clone(),
                resolved: thread.resolved,
                outdated: thread.outdated,
                placement: placed.placement,
                can_reply: thread.can_reply && capabilities.thread_reply,
                can_resolve: thread.can_resolve && capabilities.thread_resolve,
                waiting: core::waiting_on_viewer(activity, thread),
                new,
                url: thread
                    .comments
                    .first()
                    .and_then(|comment| comment.url.clone()),
                authors,
                mine,
                repository: key.repository.clone(),
                number: key.number,
                thread: thread.clone(),
            },
            comments,
        });
    }
    notes
}

/// The pull requests' conversations the cache holds.
pub async fn cached(services: ReviewServices, keys: Vec<PullKey>) -> Vec<CachedActivity> {
    let registry = services.providers();
    services
        .blocking(move || {
            let mut found = Vec::new();
            let mut groups: BTreeMap<(String, String), Vec<u64>> = BTreeMap::new();
            for key in &keys {
                groups
                    .entry((key.provider.clone(), key.repository.clone()))
                    .or_default()
                    .push(key.number);
            }
            for ((provider, repository), numbers) in groups {
                for (number, (activity, read_at)) in
                    core::cached_activity(&registry, &provider, &repository, &numbers)
                {
                    found.push((
                        PullKey {
                            provider: provider.clone(),
                            repository: repository.clone(),
                            number,
                        },
                        activity,
                        read_at,
                    ));
                }
            }
            Ok(found)
        })
        .await
        .unwrap_or_default()
}

/// The pull requests' conversations as their providers report them now: one call per provider
/// repository.
///
/// # Errors
/// The first provider's failure.
pub async fn read(
    services: ReviewServices,
    repo: PathBuf,
    keys: Vec<PullKey>,
) -> Result<Vec<PullActivity>, String> {
    let registry = services.providers();
    services
        .blocking(move || {
            let mut groups: BTreeMap<(String, String), Vec<u64>> = BTreeMap::new();
            for key in &keys {
                groups
                    .entry((key.provider.clone(), key.repository.clone()))
                    .or_default()
                    .push(key.number);
            }
            let mut activities = Vec::new();
            for ((provider, repository), numbers) in groups {
                activities.extend(core::fetch_activity(
                    &registry,
                    &repo,
                    &provider,
                    &repository,
                    &numbers,
                )?);
            }
            Ok(activities)
        })
        .await
        .map_err(|error| error.message)
}

/// Place every link's threads on the review.
pub async fn place(
    services: ReviewServices,
    repo: PathBuf,
    links: Vec<LayerLink>,
    activities: BTreeMap<PullKey, PullActivity>,
) -> Vec<(PullKey, core::PlacedThread)> {
    services
        .blocking(move || {
            let root = core::repository_root(&repo).unwrap_or(repo);
            let mut placed = Vec::new();
            for link in links {
                let Some(activity) = activities.get(&link.pull) else {
                    continue;
                };
                let target = LayerTarget {
                    source_id: link.source_id.clone(),
                    head_oid: link.head_oid.clone(),
                    paths: link.paths.clone(),
                };
                placed.extend(
                    core::place_threads(&root, activity, &target)
                        .into_iter()
                        .map(|thread| (link.pull.clone(), thread)),
                );
            }
            Ok(placed)
        })
        .await
        .unwrap_or_default()
}

/// What a watch asks: its provider and repository, the pull requests, where it left off, and
/// the provider's settings.
#[derive(Debug, Clone)]
pub struct WatchAsk {
    /// Provider.
    pub provider: String,
    /// Repository.
    pub repository: String,
    /// Pull requests.
    pub numbers: Vec<u64>,
    /// Where the last answer left off.
    pub cursor: Option<String>,
    /// The provider's watch settings.
    pub settings: serde_json::Value,
}

/// Ask `ask.provider`'s watch once. The watch waits up to [`WATCH_WAIT`] in a blocking task;
/// `cancel` stops it early (the review is going away), so the program is not held open by it.
///
/// # Errors
/// The provider's failure (`cancelled` once `cancel` is set).
pub async fn watch(
    services: ReviewServices,
    repo: PathBuf,
    ask: WatchAsk,
    cancel: bcode_code_review::provider::Cancel,
) -> Result<ReviewSourcePullsWatchResponse, String> {
    let registry = services.providers();
    services
        .blocking(move || {
            bcode_code_review::provider::with_cancel(&cancel, || {
                core::watch(
                    &registry,
                    &ask.provider,
                    &ReviewSourcePullsWatchRequest {
                        repo_path: repo,
                        repository: Some(ask.repository),
                        numbers: ask.numbers,
                        cursor: ask.cursor,
                        wait_ms: u64::try_from(WATCH_WAIT.as_millis()).unwrap_or(u64::MAX),
                        settings: ask.settings,
                    },
                )
            })
        })
        .await
        .map_err(|error| error.message)
}

/// A change to a conversation the viewer asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Reply to a thread now.
    Reply {
        /// The provider's thread id.
        thread_id: String,
        /// Body.
        body: String,
    },
    /// Resolve or reopen a thread.
    Resolve {
        /// The provider's thread id.
        thread_id: String,
        /// Resolve (`true`) or reopen.
        resolved: bool,
    },
    /// Post a conversation comment.
    Comment {
        /// Body.
        body: String,
    },
}

/// What a change came back with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Changed {
    /// A comment was posted.
    Posted(Box<RemoteComment>),
    /// A thread's resolution is now this.
    Resolved(bool),
}

/// Make `change` on `key`'s pull request.
///
/// # Errors
/// The provider's error code and message.
pub async fn change(
    services: ReviewServices,
    repo: PathBuf,
    key: PullKey,
    change: Change,
) -> Result<Changed, (String, String)> {
    let registry = services.providers();
    services
        .blocking(move || {
            let route = registry.source(&key.provider).ok_or_else(|| {
                bcode_code_review::provider::ProviderCallError::new(
                    bcode_code_review_models::REVIEW_SOURCE_ERROR_FAILED,
                    format!("{} is not available", key.provider),
                )
            })?;
            let pull = key.locator(&repo);
            match change {
                Change::Reply { thread_id, body } => {
                    let CommentPostedResponse { comment } =
                        route.provider.reply_to_thread(&ThreadReplyRequest {
                            pull,
                            thread_id,
                            body,
                        })?;
                    Ok(Changed::Posted(Box::new(comment)))
                }
                Change::Resolve {
                    thread_id,
                    resolved,
                } => {
                    let ThreadResolveResponse { resolved } =
                        route.provider.resolve_thread(&ThreadResolveRequest {
                            pull,
                            thread_id,
                            resolved,
                        })?;
                    Ok(Changed::Resolved(resolved))
                }
                Change::Comment { body } => {
                    let CommentPostedResponse { comment } = route
                        .provider
                        .post_comment(&CommentPostRequest { pull, body })?;
                    Ok(Changed::Posted(Box::new(comment)))
                }
            }
        })
        .await
        .map_err(|error| (error.code, error.message))
}

/// Preview a review of `provider`'s pull request.
///
/// # Errors
/// The provider's failure.
pub async fn preview_review(
    services: ReviewServices,
    provider: String,
    request: ReviewSubmitRequest,
) -> Result<ReviewPreviewResponse, String> {
    let registry = services.providers();
    services
        .blocking(move || {
            let route = registry.source(&provider).ok_or_else(|| {
                bcode_code_review::provider::ProviderCallError::new(
                    bcode_code_review_models::REVIEW_SOURCE_ERROR_FAILED,
                    format!("{provider} is not available"),
                )
            })?;
            route.provider.preview_review(&request)
        })
        .await
        .map_err(|error| error.message)
}

/// Submit a review of `provider`'s pull request.
///
/// # Errors
/// The provider's failure.
pub async fn submit_review(
    services: ReviewServices,
    provider: String,
    request: ReviewSubmitRequest,
) -> Result<ReviewSubmitResponse, String> {
    let registry = services.providers();
    services
        .blocking(move || {
            let route = registry.source(&provider).ok_or_else(|| {
                bcode_code_review::provider::ProviderCallError::new(
                    bcode_code_review_models::REVIEW_SOURCE_ERROR_FAILED,
                    format!("{provider} is not available"),
                )
            })?;
            route.provider.submit_review(&request)
        })
        .await
        .map_err(|error| error.message)
}

/// What an error code means for the viewer.
#[must_use]
pub fn error_text(code: &str, message: &str) -> String {
    use bcode_code_review_models::{
        REVIEW_SOURCE_ERROR_NOT_PERMITTED, REVIEW_SOURCE_ERROR_STALE,
        REVIEW_SOURCE_ERROR_UNAUTHENTICATED, REVIEW_SOURCE_ERROR_UNAVAILABLE,
    };
    match code {
        REVIEW_SOURCE_ERROR_NOT_PERMITTED => format!("Not permitted: {message}"),
        REVIEW_SOURCE_ERROR_STALE => {
            format!("The pull request changed since it was read; refresh and try again ({message})")
        }
        REVIEW_SOURCE_ERROR_UNAVAILABLE => format!("The provider is unavailable: {message}"),
        REVIEW_SOURCE_ERROR_UNAUTHENTICATED => format!("Not signed in: {message}"),
        _ => message.to_owned(),
    }
}

/// How long a watch that failed `failures` times in a row waits before it is asked again.
#[must_use]
pub fn watch_backoff(failures: u32) -> Duration {
    Duration::from_secs(15u64.saturating_mul(1 << failures.min(6)))
}

/// `3m ago`-style age of `at_ms`.
#[must_use]
pub fn ago(at_ms: Option<u64>) -> String {
    at_ms.map_or_else(String::new, |at| {
        crate::history::ago(at, crate::history::now_ms())
    })
}

#[cfg(test)]
mod tests;
