//! Pull request conversations on the review screen (see `docs/review-forge-conversation.md`):
//!
//! * **Pull request cards**, in the document before each pull request's files (see
//!   [`super::super::rows::card`]): the top of the pull request's page, its description, each
//!   reviewer's verdict (and whether it still applies after later pushes), who is still asked,
//!   failing checks, and the conversation, ending in a torn break before the files. Everything
//!   in it is a button.
//! * **The pull request bar**, under the rail: the current pull request's status line, with the
//!   same buttons; clicking its status goes to the card.
//! * **Remote threads** in the document, placed on the code the review shows.
//! * **Freshness from signals**: opening, focus, the viewer's own changes, the refresh button,
//!   and provider watches. Reads are merged and spaced by the configured gap.
//! * **Writes**: replies, resolution, conversation comments, each optimistic and rolled back with
//!   the provider's typed error when it fails.

use super::super::conversation::{
    self as conv, Arrival, Change, Changed, LayerLink, Next, ProviderInfo, PullKey,
};
use super::super::loader::Message;
use super::super::rows::card::{self, CardAction, CardContext};
use super::ReviewSurface;
use crate::action::Action;
use crate::context::UiContext;
use bcode_code_review::conversation::PlacedThread;
use bcode_code_review_models::{
    PullActivity, PullState, ReviewDecision, ReviewPullRequestTarget, ReviewVerdict,
};
use bmux_tui::composition::TextBlock;
use bmux_tui::event::{Event, MouseButton, MouseEventKind};
use bmux_tui::geometry::Rect;
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::text::{Span, Text};
use bmux_tui_components::status_bar::StatusSeverity;
use bmux_tui_components::toast_stack::ToastSeverity;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::time::Duration;

/// Conversation work finished on a host task.
#[derive(Debug)]
pub enum ConversationMessage {
    /// What the cache held.
    Cached(Vec<(PullKey, PullActivity)>),
    /// A read finished.
    Read {
        /// What was read.
        keys: Vec<PullKey>,
        /// The conversations, or why not.
        result: Result<Vec<PullActivity>, String>,
    },
    /// Threads were placed on the review (for conversations at `revision`).
    Placed {
        /// Conversations revision placed.
        revision: u64,
        /// The threads.
        placed: Vec<(PullKey, PlacedThread)>,
    },
    /// A delayed read is due.
    Due,
    /// A provider watch answered.
    Watched {
        /// Provider.
        provider: String,
        /// Repository.
        repository: String,
        /// What changed, or why it could not say.
        result: Result<bcode_code_review_models::ReviewSourcePullsWatchResponse, String>,
    },
    /// A watch that failed may be asked again.
    WatchAgain,
    /// A change finished.
    Changed(Box<ChangeDone>),
}

/// A finished change.
#[derive(Debug)]
pub struct ChangeDone {
    /// Its pull request.
    pub key: PullKey,
    /// The review's thread id, for a thread change.
    pub thread: Option<String>,
    /// The change.
    pub change: Change,
    /// What came back, or the error's code and message.
    pub result: Result<Changed, (String, String)>,
}

/// The bar's painted state.
#[derive(Debug, Default)]
pub struct BarState {
    /// Where it was painted.
    pub area: Option<Rect>,
    /// Clickable cells of its row, and their actions.
    pub regions: Vec<(Range<u16>, CardAction)>,
    /// The pull request the bar showed last (marked seen when the viewer leaves it).
    pub shown: Option<PullKey>,
}

/// The review's pull request conversations, and what waits for the host.
#[derive(Debug, Default)]
pub struct ConversationUi {
    /// The conversations.
    pub data: conv::Conversations,
    /// The newest activity seen per pull request (persisted).
    pub seen: BTreeMap<String, u64>,
    /// Pull requests to read again once the host is at hand.
    pub signals: Vec<PullKey>,
    /// Changes to run once the host is at hand: pull request, the review's thread id, change.
    pub queue: Vec<(PullKey, Option<String>, Change)>,
    /// Web pages to open once the host is at hand.
    pub urls: Vec<String>,
    /// The pull request bar.
    pub bar: BarState,
    /// Sources whose cards show everything (the whole description and conversation).
    pub show_all: BTreeSet<String>,
    /// Changes whenever how cards show changed (opened, folded, shown all).
    pub view_revision: u64,
    /// Card parts clicked, to run once the host is at hand: source id and action.
    pub card_clicks: Vec<(String, CardAction)>,
    /// The pull request threads last saved for the review tools, and where.
    pub saved_threads: Option<(
        crate::notes::NoteScope,
        Vec<bcode_code_review_models::PullThreadRecord>,
    )>,
}

impl ConversationUi {
    /// Changes whenever anything a card shows changed.
    #[must_use]
    pub const fn card_revision(&self) -> u64 {
        self.data
            .revision
            .wrapping_mul(0x9e37_79b9)
            .wrapping_add(self.view_revision)
    }
}

impl ReviewSurface {
    /// Read the review's pull requests again from the document: after it opened or its sources
    /// arrived. New pull requests are shown from the cache, then read.
    pub(super) fn refresh_conversations(&mut self, host: &UiContext) {
        let config = self.services.conversation_config();
        if !config.enabled {
            return;
        }
        let Some(review) = self.review.as_ref() else {
            return;
        };
        let links = conv::links(&review.doc, &self.sources);
        let registry = self.services.providers();
        for link in &links {
            if let Some(route) = registry.source(&link.pull.provider) {
                self.talk.data.providers.insert(
                    link.pull.provider.clone(),
                    ProviderInfo {
                        label: route.manifest.label.clone(),
                        capabilities: route.manifest.conversation,
                    },
                );
            }
        }
        let known: Vec<PullKey> = self
            .talk
            .data
            .links
            .iter()
            .map(|link| link.pull.clone())
            .collect();
        let changed = links != self.talk.data.links;
        // Every pull request whose provider reports its conversation gets a card before its files.
        let reports = |provider: &str| {
            registry
                .source(provider)
                .is_some_and(|route| route.manifest.conversation.activity)
        };
        let intros: BTreeSet<String> = links
            .iter()
            .filter(|link| reports(&link.pull.provider))
            .map(|link| link.source_id.clone())
            .collect();
        if let Some(review) = self.review.as_mut()
            && review.doc.set_intros(intros)
        {
            review.note_splices();
        }
        self.talk.data.links = links;
        if !changed {
            return;
        }
        let fresh: Vec<PullKey> = self
            .talk
            .data
            .readable()
            .into_iter()
            .filter(|key| !known.contains(key))
            .collect();
        if !fresh.is_empty() {
            let (services, inbox, keys) = (self.services.clone(), self.inbox.clone(), fresh);
            host.spawn(Box::pin(async move {
                let cached = conv::cached(services, keys).await;
                inbox.push(Message::Conversation(Box::new(
                    ConversationMessage::Cached(cached),
                )));
            }));
        }
        // Heads or files changed (a fetch, a push, a restack): place again, and read every pull
        // request again, since its conversation may have moved with it.
        self.place_conversations(host);
        self.signal_all_conversations(host);
    }

    /// Note that `keys` may have changed and read them when the gap allows.
    pub(super) fn signal_conversations(&mut self, keys: Vec<PullKey>, host: &UiContext) {
        let gap = Duration::from_millis(self.services.conversation_config().signals.min_gap_ms);
        let next = self.talk.data.signal(keys, gap);
        self.run_conversation_next(next, host);
    }

    /// Every pull request of the review may have changed.
    pub(super) fn signal_all_conversations(&mut self, host: &UiContext) {
        let keys = self.talk.data.readable();
        self.signal_conversations(keys, host);
    }

    fn run_conversation_next(&self, next: Next, host: &UiContext) {
        match next {
            Next::Wait => {}
            Next::After(delay) => {
                let inbox = self.inbox.clone();
                host.spawn(Box::pin(async move {
                    tokio::time::sleep(delay).await;
                    inbox.push(Message::Conversation(Box::new(ConversationMessage::Due)));
                }));
            }
            Next::Read(keys) => {
                let (services, inbox, repo) = (
                    self.services.clone(),
                    self.inbox.clone(),
                    self.repo_path.clone(),
                );
                host.spawn(Box::pin(async move {
                    let result = conv::read(services, repo, keys.clone()).await;
                    inbox.push(Message::Conversation(Box::new(ConversationMessage::Read {
                        keys,
                        result,
                    })));
                }));
            }
        }
    }

    /// Apply finished conversation work.
    pub(super) fn apply_conversation(&mut self, message: ConversationMessage, host: &UiContext) {
        let gap = Duration::from_millis(self.services.conversation_config().signals.min_gap_ms);
        match message {
            ConversationMessage::Cached(found) => {
                let keys: Vec<PullKey> = found.iter().map(|(key, _)| key.clone()).collect();
                let activities = found.into_iter().map(|(_, activity)| activity).collect();
                self.talk.data.read(&keys, Ok(activities), true);
                self.note_baselines();
                self.place_conversations(host);
            }
            ConversationMessage::Read { keys, result } => {
                if let Err(error) = &result {
                    // Only worth saying when nothing could be shown.
                    if keys
                        .iter()
                        .all(|key| self.talk.data.activity(key).is_none())
                    {
                        self.set_status(
                            format!("Could not read the pull request conversation: {error}"),
                            StatusSeverity::Warning,
                        );
                    }
                }
                let arrivals = self.talk.data.read(&keys, result, false);
                self.notify_arrivals(&arrivals);
                self.note_baselines();
                self.place_conversations(host);
                let next = self.talk.data.next(gap);
                self.run_conversation_next(next, host);
                self.start_watches(host);
            }
            ConversationMessage::Placed { revision, placed } => {
                if revision != self.talk.data.revision {
                    return;
                }
                let config = self.services.conversation_config();
                let notes = conv::remote_notes(&placed, &self.talk.data, &self.talk.seen, &config);
                if self.notes.set_remote(notes) {
                    self.sync_notes();
                }
                self.save_pull_threads(&placed, host);
            }
            ConversationMessage::Due => {
                self.talk.data.due();
                let next = self.talk.data.next(gap);
                self.run_conversation_next(next, host);
            }
            ConversationMessage::Watched {
                provider,
                repository,
                result,
            } => {
                let changed = self.talk.data.watched(&provider, &repository, result);
                if !changed.is_empty() {
                    self.signal_conversations(changed, host);
                }
                self.start_watches(host);
            }
            ConversationMessage::WatchAgain => self.start_watches(host),
            ConversationMessage::Changed(done) => {
                let ChangeDone {
                    key,
                    thread,
                    change,
                    result,
                } = *done;
                self.apply_change(key, thread.as_deref(), &change, result, host);
            }
        }
    }

    /// Keep the placed pull request threads in the review store, where the review tools read
    /// them, when they changed since they were last kept.
    fn save_pull_threads(&mut self, placed: &[(PullKey, PlacedThread)], host: &UiContext) {
        let threads: Vec<bcode_code_review_models::PullThreadRecord> = placed
            .iter()
            .map(|(key, placed)| bcode_code_review_models::PullThreadRecord {
                id: bcode_code_review_models::pull_thread_ref(
                    &key.repository,
                    key.number,
                    &placed.thread.id,
                ),
                provider: key.provider.clone(),
                provider_label: self
                    .talk
                    .data
                    .provider(key)
                    .map_or_else(|| key.provider.clone(), |provider| provider.label.clone()),
                repository: key.repository.clone(),
                number: key.number,
                location: bcode_code_review::assistant::location_label(&placed.anchor),
                placement: placed.placement.into(),
                thread: placed.thread.clone(),
            })
            .collect();
        let scope = self.note_scope();
        let saved = (scope.clone(), threads);
        if self.talk.saved_threads.as_ref() == Some(&saved) {
            return;
        }
        let threads = saved.1.clone();
        self.talk.saved_threads = Some(saved);
        let services = self.services.clone();
        host.spawn(Box::pin(async move {
            // The tools say when they have nothing; a failed save only leaves them older.
            let _ = services
                .call(bcode_code_review_models::SaveReviewConversationRequest {
                    repo_path: scope.repo_path,
                    target: scope.target,
                    scope: scope.scope,
                    snapshot: bcode_code_review_models::ReviewConversationSnapshot {
                        threads,
                        omitted: 0,
                        read_at_ms: crate::history::now_ms(),
                    },
                })
                .await;
        }));
    }

    /// Start every provider watch the review's pull requests need and that is not running.
    fn start_watches(&mut self, host: &UiContext) {
        let config = self.services.conversation_config();
        let enabled = config.signals.provider
            && !(config.signals.pause_unfocused && self.talk.data.unfocused);
        for conv::WatchStart {
            provider,
            repository,
            numbers,
            cursor,
            failures,
        } in self.talk.data.watches_to_start(enabled)
        {
            let settings = config.signals.provider_settings(&provider);
            let (services, inbox, repo) = (
                self.services.clone(),
                self.inbox.clone(),
                self.repo_path.clone(),
            );
            host.spawn(Box::pin(async move {
                if failures > 0 {
                    tokio::time::sleep(conv::watch_backoff(failures)).await;
                }
                let result = conv::watch(
                    services,
                    repo,
                    provider.clone(),
                    repository.clone(),
                    numbers,
                    cursor,
                    settings,
                )
                .await;
                inbox.push(Message::Conversation(Box::new(
                    ConversationMessage::Watched {
                        provider,
                        repository,
                        result,
                    },
                )));
            }));
        }
    }

    /// The terminal gained or lost focus.
    pub(super) fn conversation_focus(&mut self, focused: bool, host: &UiContext) {
        self.talk.data.unfocused = !focused;
        if focused {
            self.signal_all_conversations(host);
        }
    }

    /// Place every pull request's threads on the review again.
    fn place_conversations(&self, host: &UiContext) {
        let activities: BTreeMap<PullKey, PullActivity> = self
            .talk
            .data
            .pulls
            .iter()
            .filter_map(|(key, entry)| Some((key.clone(), entry.activity.clone()?)))
            .collect();
        if activities.is_empty() {
            return;
        }
        let revision = self.talk.data.revision;
        let (services, inbox, repo, links) = (
            self.services.clone(),
            self.inbox.clone(),
            self.repo_path.clone(),
            self.talk.data.links.clone(),
        );
        host.spawn(Box::pin(async move {
            let placed = conv::place(services, repo, links, activities).await;
            inbox.push(Message::Conversation(Box::new(
                ConversationMessage::Placed { revision, placed },
            )));
        }));
    }

    /// Pull requests seen for the first time start from what they have now (it is not news).
    fn note_baselines(&mut self) {
        let mut changed = false;
        for (key, entry) in &self.talk.data.pulls {
            let Some(activity) = &entry.activity else {
                continue;
            };
            if let std::collections::btree_map::Entry::Vacant(entry) =
                self.talk.seen.entry(key.id())
            {
                entry.insert(conv::newest_from_others(activity));
                changed = true;
            }
        }
        if changed {
            self.persist_presentation(false);
        }
    }

    /// The viewer saw `key`'s conversation as it is now.
    pub(super) fn mark_conversation_seen(&mut self, key: &PullKey) {
        let Some(activity) = self.talk.data.activity(key) else {
            return;
        };
        let newest = conv::newest_from_others(activity);
        let seen = self.talk.seen.entry(key.id()).or_insert(0);
        if *seen < newest {
            *seen = newest;
            self.talk.data.revision += 1;
            self.persist_presentation(false);
        }
    }

    fn notify_arrivals(&mut self, arrivals: &[(PullKey, Arrival)]) {
        let mode = self.services.conversation_config().notify;
        let shown: Vec<String> = arrivals
            .iter()
            .filter(|(_, arrival)| conv::notifies(mode, arrival))
            .map(|(key, arrival)| conv::arrival_text(key, arrival))
            .collect();
        if shown.is_empty() {
            return;
        }
        let title = if shown.len() == 1 {
            shown[0].clone()
        } else {
            format!("{} new on the pull requests", shown.len())
        };
        let body = (shown.len() > 1).then(|| shown.join("\n"));
        self.toasts.push(ToastSeverity::Info, title, body);
    }

    // ----------------------------------------------------------------------------------------
    // Changes
    // ----------------------------------------------------------------------------------------

    /// Queue `change` on `key` (the review's thread id for a thread change); it runs on the
    /// next poll.
    pub(super) fn queue_change(&mut self, key: PullKey, thread: Option<String>, change: Change) {
        self.talk.queue.push((key, thread, change));
    }

    /// Run queued changes and signals (they were queued where no host was at hand).
    pub(super) fn flush_conversations(&mut self, host: &UiContext) -> bool {
        let signals = std::mem::take(&mut self.talk.signals);
        let queued = std::mem::take(&mut self.talk.queue);
        let any = !signals.is_empty() || !queued.is_empty();
        if !signals.is_empty() {
            self.signal_conversations(signals, host);
        }
        for (key, thread, change) in queued {
            // Optimistic: shown at once, undone if the provider refuses.
            if let (Change::Resolve { resolved, .. }, Some(thread)) = (&change, &thread) {
                self.notes.set_remote_resolved(thread, *resolved);
                self.sync_notes();
            }
            let (services, inbox, repo) = (
                self.services.clone(),
                self.inbox.clone(),
                self.repo_path.clone(),
            );
            host.spawn(Box::pin(async move {
                let result = conv::change(services, repo, key.clone(), change.clone()).await;
                inbox.push(Message::Conversation(Box::new(
                    ConversationMessage::Changed(Box::new(ChangeDone {
                        key,
                        thread,
                        change,
                        result,
                    })),
                )));
            }));
        }
        any
    }

    fn apply_change(
        &mut self,
        key: PullKey,
        thread: Option<&str>,
        change: &Change,
        result: Result<Changed, (String, String)>,
        host: &UiContext,
    ) {
        match result {
            Ok(changed) => {
                let text = match (change, changed) {
                    (Change::Reply { .. }, _) => "Reply posted",
                    (Change::Comment { .. }, _) => "Comment posted",
                    (Change::Resolve { .. }, Changed::Resolved(true)) => "Thread resolved",
                    (Change::Resolve { .. }, _) => "Thread reopened",
                };
                self.set_status(text, StatusSeverity::Success);
            }
            Err((code, message)) => {
                if let (Change::Resolve { resolved, .. }, Some(thread)) = (change, thread) {
                    self.notes.set_remote_resolved(thread, !resolved);
                    self.sync_notes();
                }
                let what = match change {
                    Change::Reply { .. } => "Could not reply",
                    Change::Comment { .. } => "Could not comment",
                    Change::Resolve { resolved: true, .. } => "Could not resolve the thread",
                    Change::Resolve { .. } => "Could not reopen the thread",
                };
                let body = conv::error_text(&code, &message);
                // A failed reply keeps its text: it is held for the review, to send again.
                if let (Change::Reply { body: text, .. }, Some(thread)) = (change, thread)
                    && self.reply_remote(thread, text.clone(), false).is_ok()
                {
                    self.sync_notes();
                }
                self.toasts.push(ToastSeverity::Error, what, Some(body));
            }
        }
        self.signal_conversations(vec![key], host);
    }

    /// A thread action on a pull request thread. Returns whether `thread_id` is one.
    pub(super) fn remote_thread_action(
        &mut self,
        thread_id: &str,
        action: super::super::rows::ThreadAction,
        host: Option<&UiContext>,
    ) -> bool {
        use super::super::rows::ThreadAction;
        let Some(info) = self.notes.remote(thread_id).cloned() else {
            return false;
        };
        let Some(key) = self
            .talk
            .data
            .links
            .iter()
            .find(|link| link.pull.id() == info.pull)
            .map(|link| link.pull.clone())
        else {
            return true;
        };
        match action {
            ThreadAction::Reply if info.can_reply => {
                let anchor = self
                    .notes
                    .thread(thread_id)
                    .first()
                    .map(|first| self.doc_anchor(&first.anchor));
                if let Some(anchor) = anchor {
                    self.open_composer(
                        anchor,
                        crate::composer::ComposerMode::Reply {
                            thread_id: thread_id.to_owned(),
                        },
                        "",
                    );
                    if let Some(composer) = self.composer.as_mut() {
                        composer.remote_reply = Some(self.services.conversation_config().replies);
                    }
                }
            }
            ThreadAction::Resolve if info.can_resolve => {
                self.queue_change(
                    key,
                    Some(thread_id.to_owned()),
                    Change::Resolve {
                        thread_id: info.remote_id,
                        resolved: !info.resolved,
                    },
                );
                self.resolved_by_you(thread_id, !info.resolved);
            }
            ThreadAction::Viewed => {
                self.toggle_thread_viewed();
            }
            ThreadAction::Open => match (&info.url, host) {
                (Some(url), Some(host)) => self.open_url(url, host),
                (Some(url), None) => self.talk.urls.push(url.clone()),
                (None, _) => self.set_status("this thread has no web page", StatusSeverity::Info),
            },
            ThreadAction::Ask => {
                let anchor = self
                    .notes
                    .thread(thread_id)
                    .first()
                    .map(|first| self.doc_anchor(&first.anchor));
                if let Some(anchor) = anchor {
                    self.open_composer(anchor, crate::composer::ComposerMode::Ask, "");
                    if let Some(composer) = self.composer.as_mut() {
                        composer.thread = Some(thread_id.to_owned());
                    }
                }
            }
            ThreadAction::Fold => self.fold(),
            _ => self.set_status(
                format!(
                    "this is a thread on {}: it can be replied to, resolved, or opened",
                    info.provider_label
                ),
                StatusSeverity::Info,
            ),
        }
        true
    }

    /// Send a reply to the pull request thread `thread_id`: now, or held for the next review.
    pub(super) fn reply_remote(
        &mut self,
        thread_id: &str,
        body: String,
        now: bool,
    ) -> Result<&'static str, String> {
        let Some(info) = self.notes.remote(thread_id).cloned() else {
            return Err("the thread no longer exists".to_owned());
        };
        if now {
            let key = self
                .talk
                .data
                .links
                .iter()
                .find(|link| link.pull.id() == info.pull)
                .map(|link| link.pull.clone())
                .ok_or_else(|| "the pull request is no longer in the review".to_owned())?;
            self.queue_change(
                key,
                Some(thread_id.to_owned()),
                Change::Reply {
                    thread_id: info.remote_id,
                    body,
                },
            );
            return Ok("sending the reply…");
        }
        let mut anchor = self
            .notes
            .thread(thread_id)
            .first()
            .map(|first| first.anchor.clone())
            .ok_or_else(|| "the thread no longer exists".to_owned())?;
        anchor.reply_to = Some(crate::notes::reply_target(&info.pull, &info.remote_id));
        self.notes
            .add_draft(anchor, body, None, None, crate::history::now_ms());
        Ok("reply held for your review")
    }

    fn open_url(&mut self, url: &str, host: &UiContext) {
        if let Err(error) = host.host().open_external(url) {
            self.set_status(
                format!("Could not open {url}: {}", error.message),
                StatusSeverity::Warning,
            );
        }
    }

    // ----------------------------------------------------------------------------------------
    // The pull request bar
    // ----------------------------------------------------------------------------------------

    /// The pull request the bar shows: the cursor's layer's (the stack's top for a whole
    /// stack), else the review's first.
    pub(super) fn current_link(&self) -> Option<&LayerLink> {
        let entries = self.rail_entries();
        let source = self
            .current_layer(&entries)
            .map(|index| entries[index].source_id.clone())
            .or_else(|| {
                let review = self.review.as_ref()?;
                review
                    .doc
                    .item(review.cursor_index)?
                    .file()
                    .map(|file| file.source_id.clone())
            });
        source
            .and_then(|source| self.talk.data.link_of(&source))
            .or_else(|| self.talk.data.links.first())
    }

    /// Whether the bar shows (its provider reports conversations).
    fn bar_link(&self) -> Option<LayerLink> {
        let config = self.services.conversation_config();
        if !config.enabled {
            return None;
        }
        let link = self.current_link()?;
        self.talk
            .data
            .provider(&link.pull)
            .filter(|provider| provider.capabilities.activity)?;
        Some(link.clone())
    }

    /// Rows the bar takes in a frame of `height` rows (0 when it does not show): one, the
    /// current pull request's status line. Leaving a pull request marks it seen.
    pub(super) fn bar_height(&mut self, height: u16, _width: u16) -> u16 {
        let Some(link) = self.bar_link() else {
            return 0;
        };
        if self.talk.bar.shown.as_ref() != Some(&link.pull) {
            if let Some(previous) = self.talk.bar.shown.take() {
                self.mark_conversation_seen(&previous);
            }
            self.talk.bar.shown = Some(link.pull);
        }
        u16::from(height >= 6)
    }

    /// What pull request cards (and the bar) are drawn from.
    pub(super) fn card_context(&self, viewed_layers: usize) -> CardContext<'_> {
        CardContext {
            conversations: &self.talk.data,
            seen: &self.talk.seen,
            history: self.stack_history.as_ref(),
            show_all: &self.talk.show_all,
            viewed_layers,
            overview: self.services.conversation_config().overview,
            threads: &self.folds.threads,
            revision: self.talk.card_revision() ^ self.folds.revision.rotate_left(17),
        }
    }

    /// Paint the bar on `area`.
    pub(super) fn paint_bar(&mut self, area: Rect, cx: &mut PaintCx<'_, '_>) {
        self.talk.bar.area = None;
        self.talk.bar.regions.clear();
        let Some(link) = self.bar_link() else {
            return;
        };
        let theme = self.theme;
        cx.fill(
            LocalRect::new(area.x.into(), area.y.into(), area.width, area.height),
            " ",
            theme.header_bar,
        );
        let cards = self.card_context(self.viewed_layers().len());
        let (status, regions) = card::status_row(&link, &cards, &theme, usize::from(area.width));
        cx.paint_component(&TextBlock::new(Text::from_lines(vec![status])), area);
        self.talk.bar.regions = regions;
        self.talk.bar.area = Some(area);
    }

    /// A mouse event over the bar. `None` when it is not over the bar.
    pub(super) fn bar_mouse(&mut self, event: &Event, host: &UiContext) -> Option<Action> {
        let Event::Mouse(mouse) = event else {
            return None;
        };
        let area = self.talk.bar.area?;
        if !area.contains(mouse.position) {
            return None;
        }
        let column = mouse.position.x - area.x;
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return Some(Action::None);
        }
        let action = self
            .talk
            .bar
            .regions
            .iter()
            .find(|(cells, _)| cells.contains(&column))
            .map(|(_, action)| *action);
        Some(action.map_or(Action::None, |action| {
            self.bar_action(action, host);
            Action::Redraw
        }))
    }

    /// Run an action on the current pull request (from the bar, or a key). On the bar, the
    /// toggle goes to the pull request's card.
    pub(super) fn bar_action(&mut self, action: CardAction, host: &UiContext) {
        let Some(link) = self.bar_link() else {
            self.set_status(
                "this review has no pull request conversation",
                StatusSeverity::Info,
            );
            return;
        };
        if action == CardAction::Toggle {
            self.go_to_card(&link);
            return;
        }
        self.pull_action(&link, action, host);
    }

    /// Put the cursor on `link`'s card, opened.
    fn go_to_card(&mut self, link: &LayerLink) {
        if !self.card_open(&link.pull) {
            self.talk.data.opened.insert(link.pull.clone(), true);
            self.talk.view_revision += 1;
        }
        let key = bcode_code_review::document::ItemKey::SourceIntro {
            source_id: link.source_id.clone(),
        };
        if let Some(review) = self.review.as_mut()
            && let Some(index) = review.doc.index_of(&key)
        {
            review.set_cursor(
                index,
                bmux_tui_components::virtual_list::AnchorPolicy::ItemTopAtTop,
            );
        }
    }

    fn card_open(&self, key: &PullKey) -> bool {
        let mode = self.services.conversation_config().overview;
        self.talk
            .data
            .overview_open(key, mode, self.talk.seen.get(&key.id()).copied())
    }

    /// Open or fold the card of `source_id` (on its card, `z`).
    pub(super) fn toggle_card(&mut self, source_id: &str) {
        let Some(link) = self.talk.data.link_of(source_id).cloned() else {
            return;
        };
        let open = !self.card_open(&link.pull);
        if !open {
            self.mark_conversation_seen(&link.pull);
        }
        self.talk.data.opened.insert(link.pull, open);
        self.talk.view_revision += 1;
    }

    /// A click on a card's part: the card of `source_id`.
    pub(super) fn card_click(&mut self, source_id: &str, action: CardAction, host: &UiContext) {
        if action == CardAction::Toggle {
            self.toggle_card(source_id);
            return;
        }
        if let CardAction::OpenLayer(index) = action {
            let url = self
                .talk
                .data
                .links
                .iter()
                .filter(|link| link.source_id == source_id)
                .nth(usize::from(index))
                .and_then(|link| self.talk.data.activity(&link.pull)?.url.clone());
            if let Some(url) = url {
                self.open_url(&url, host);
            }
            return;
        }
        let Some(link) = self.talk.data.link_of(source_id).cloned() else {
            return;
        };
        self.pull_action(&link, action, host);
    }

    /// Run `action` on `link`'s pull request.
    fn pull_action(&mut self, link: &LayerLink, action: CardAction, host: &UiContext) {
        let target = |key: &PullKey| ReviewPullRequestTarget {
            provider: key.provider.clone(),
            repository: key.repository.clone(),
            number: key.number,
        };
        let activity = self.talk.data.activity(&link.pull).cloned();
        let url = match action {
            CardAction::OpenPull => activity.as_ref().and_then(|activity| activity.url.clone()),
            CardAction::OpenReview(index) => activity.as_ref().and_then(|activity| {
                activity
                    .latest_reviews()
                    .get(usize::from(index))
                    .and_then(|review| review.url.clone())
            }),
            CardAction::OpenCheck(index) => activity.as_ref().and_then(|activity| {
                card::attention_checks(activity)
                    .get(usize::from(index))
                    .and_then(|check| check.url.clone())
            }),
            _ => None,
        };
        match action {
            CardAction::Toggle => self.toggle_card(&link.source_id),
            CardAction::Approve => {
                self.open_review(ReviewVerdict::Approve, vec![target(&link.pull)], host);
            }
            CardAction::RequestChanges => {
                self.open_review(
                    ReviewVerdict::RequestChanges,
                    vec![target(&link.pull)],
                    host,
                );
            }
            CardAction::ApproveViewed => {
                let targets: Vec<ReviewPullRequestTarget> =
                    self.viewed_layers().iter().map(target).collect();
                if targets.is_empty() {
                    self.set_status("no layer is fully viewed yet", StatusSeverity::Info);
                } else {
                    self.open_review(ReviewVerdict::Approve, targets, host);
                }
            }
            CardAction::Comment => self.start_conversation_comment(link),
            CardAction::OpenPull
            | CardAction::OpenReview(_)
            | CardAction::OpenCheck(_)
            | CardAction::OpenLayer(_) => match url {
                Some(url) => self.open_url(&url, host),
                None => self.set_status("there is no web page for that", StatusSeverity::Info),
            },
            CardAction::Refresh => {
                self.set_status("reading the conversation…", StatusSeverity::Info);
                self.signal_conversations(vec![link.pull.clone()], host);
            }
            CardAction::ShowAll => {
                if !self.talk.show_all.remove(&link.source_id) {
                    self.talk.show_all.insert(link.source_id.clone());
                }
                self.talk.view_revision += 1;
            }
            CardAction::ShowNew => self.show_new(&link.pull),
        }
    }

    /// Open the current pull request's web page.
    pub(super) fn open_pull_request_page(&mut self, host: &UiContext) {
        let url = self
            .bar_link()
            .and_then(|link| self.talk.data.activity(&link.pull)?.url.clone());
        match url {
            Some(url) => self.open_url(&url, host),
            None => self.set_status("this pull request has no web page", StatusSeverity::Info),
        }
    }

    /// Open the composer for a comment in `link`'s conversation.
    fn start_conversation_comment(&mut self, link: &LayerLink) {
        self.open_composer(
            bcode_code_review::document::DocAnchor::review(),
            crate::composer::ComposerMode::Conversation {
                pull: link.pull.id(),
                number: link.pull.number,
            },
            "",
        );
    }

    /// Post the composer's conversation comment.
    pub(super) fn post_conversation_comment(
        &mut self,
        pull: &str,
        body: String,
    ) -> Result<&'static str, String> {
        let key = self
            .talk
            .data
            .links
            .iter()
            .find(|link| link.pull.id() == pull)
            .map(|link| link.pull.clone())
            .ok_or_else(|| "the pull request is no longer in the review".to_owned())?;
        self.queue_change(key, None, Change::Comment { body });
        Ok("posting the comment…")
    }

    /// Go to the newest new thread of `key`, else open its overview.
    fn show_new(&mut self, key: &PullKey) {
        let pull = key.id();
        let thread = self
            .notes
            .remote_threads()
            .iter()
            .find(|(_, info)| info.pull == pull && info.new)
            .map(|(thread_id, _)| thread_id.clone());
        if let Some(thread_id) = thread
            && let Some(review) = self.review.as_mut()
        {
            let item = bcode_code_review::document::ItemKey::Thread { thread_id };
            if let Some(index) = review.doc.index_of(&item) {
                review.set_cursor(
                    index,
                    bmux_tui_components::virtual_list::AnchorPolicy::ItemTopAtTop,
                );
                return;
            }
        }
        self.talk.data.opened.insert(key.clone(), true);
    }

    /// Layers whose files are all viewed and whose provider lets the viewer approve them.
    pub(super) fn viewed_layers(&self) -> Vec<PullKey> {
        let Some(review) = self.review.as_ref() else {
            return Vec::new();
        };
        review
            .doc
            .groups()
            .filter(|group| group.file_count > 0 && group.viewed == group.file_count)
            .filter_map(|group| self.talk.data.link_of(&group.source.source_id))
            .filter(|link| self.may_judge(&link.pull))
            .map(|link| link.pull.clone())
            .collect()
    }

    /// Whether the viewer may approve or request changes on `key`.
    fn may_judge(&self, key: &PullKey) -> bool {
        let supported = self
            .talk
            .data
            .provider(key)
            .is_some_and(|provider| provider.capabilities.verdicts.approve);
        let allowed = self
            .talk
            .data
            .activity(key)
            .is_none_or(|activity| activity.viewer.can_judge);
        supported && allowed
    }

    /// The rail's note for a layer's pull request: its decision and open threads.
    pub(super) fn conversation_tab_spans(&self, source_id: &str) -> Vec<Span> {
        let Some(link) = self.talk.data.link_of(source_id) else {
            return Vec::new();
        };
        let Some(activity) = self.talk.data.activity(&link.pull) else {
            return Vec::new();
        };
        let theme = &self.theme;
        let icons = theme.icons;
        let mut spans = Vec::new();
        match activity.decision {
            Some(ReviewDecision::Approved) => spans.push(Span::styled(
                format!(" {}", icons.approved),
                theme.badge_resolved,
            )),
            Some(ReviewDecision::ChangesRequested) => {
                spans.push(Span::styled(format!(" {}", icons.changes), theme.error));
            }
            Some(ReviewDecision::ReviewRequired) | None => {}
        }
        if activity.state == PullState::Merged {
            spans.push(Span::styled(" merged", theme.muted));
        }
        let open = activity.open_threads();
        if open > 0 {
            spans.push(Span::styled(
                format!(" {}{open}", icons.thread),
                theme.thread,
            ));
        }
        if conv::is_new(activity, self.talk.seen.get(&link.pull.id()).copied()) {
            spans.push(Span::styled(format!(" {}", icons.new), theme.badge_draft));
        }
        spans
    }
}
