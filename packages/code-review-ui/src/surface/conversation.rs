//! Pull request conversations on the review screen (see `docs/review-forge-conversation.md`):
//!
//! * **The pull request bar**, under the rail: the current pull request's review decision,
//!   approvals, checks, open threads, and what is new, with buttons for what the provider and the
//!   viewer's permissions allow (Approve, Request changes, Comment, Open, Refresh). One line by
//!   default (configurable); opened, it is the top of the pull request's page: the description,
//!   each reviewer's verdict (and whether it still applies after later pushes), who is still
//!   asked, failing checks, and the conversation. Everything in it is a button.
//! * **Remote threads** in the document, placed on the code the review shows.
//! * **Freshness from signals**: opening, focus, the viewer's own changes, the refresh button,
//!   and provider watches. Reads are merged and spaced by the configured gap.
//! * **Writes**: replies, resolution, conversation comments, each optimistic and rolled back with
//!   the provider's typed error when it fails.

use super::super::conversation::{
    self as conv, Arrival, Change, Changed, LayerLink, Next, ProviderInfo, PullKey,
};
use super::super::loader::Message;
use super::ReviewSurface;
use crate::action::Action;
use crate::context::UiContext;
use bcode_code_review::conversation::{Currency, PlacedThread};
use bcode_code_review_models::{
    CheckState, PullActivity, PullState, ReviewDecision, ReviewPullRequestTarget, ReviewState,
    ReviewVerdict,
};
use bmux_tui::composition::TextBlock;
use bmux_tui::event::{Event, MouseButton, MouseEventKind};
use bmux_tui::geometry::Rect;
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::style::Style;
use bmux_tui::text::{Line, Span, Text};
use bmux_tui::text_width::display_width;
use bmux_tui_components::status_bar::StatusSeverity;
use bmux_tui_components::toast_stack::ToastSeverity;
use std::collections::BTreeMap;
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

/// A clickable part of the pull request bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BarAction {
    /// Open or close the overview.
    Toggle,
    /// Approve this pull request.
    Approve,
    /// Request changes.
    RequestChanges,
    /// Comment in the conversation.
    Comment,
    /// Approve every fully viewed layer.
    ApproveViewed,
    /// Open a web page.
    Open(String),
    /// Read the conversation again.
    Refresh,
    /// Show the whole description and conversation.
    ShowAll,
    /// Go to what is new.
    ShowNew,
}

/// The bar's painted state.
#[derive(Debug, Default)]
pub struct BarState {
    /// Where it was painted.
    pub area: Option<Rect>,
    /// Clickable cells: row (within the shown rows), cells, action.
    pub regions: Vec<BarRegion>,
    /// First shown row of the open overview.
    pub scroll: u16,
    /// Rows of the open overview.
    pub rows: u16,
    /// The whole description and conversation are shown.
    pub show_all: bool,
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
}

/// One clickable part of a bar row: row, cells, action.
pub type BarRegion = (u16, Range<u16>, BarAction);

/// Rows of the bar and their clickable parts.
type BarRows = (Vec<Line>, Vec<BarRegion>);

/// Most rows the open overview takes, as a share of the screen.
const OVERVIEW_SHARE: u16 = 2;
/// Conversation comments shown before *Show all*.
const RECENT_COMMENTS: usize = 3;
/// Description rows shown before *Show all*.
const DESCRIPTION_ROWS: usize = 6;
/// Rows of one comment before *Show all*.
const COMMENT_ROWS: usize = 6;

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
                if config.fold_resolved {
                    for note in &notes {
                        if note.info.resolved {
                            self.folds.collapsed.insert(note.thread_id.clone());
                        }
                    }
                }
                if self.notes.set_remote(notes) {
                    self.sync_notes();
                }
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

    /// Rows the bar takes in a frame of `height` rows (0 when it does not show).
    pub(super) fn bar_height(&mut self, height: u16, width: u16) -> u16 {
        let Some(link) = self.bar_link() else {
            return 0;
        };
        // Leaving a pull request marks it seen.
        if self.talk.bar.shown.as_ref() != Some(&link.pull) {
            if let Some(previous) = self.talk.bar.shown.take() {
                self.mark_conversation_seen(&previous);
            }
            self.talk.bar.shown = Some(link.pull.clone());
            self.talk.bar.scroll = 0;
            self.talk.bar.show_all = false;
        }
        if height < 12 {
            return u16::from(height >= 6);
        }
        if !self.overview_open(&link.pull) {
            return 1;
        }
        let (lines, _) = self.bar_lines(&link, width, true);
        let most = (height * OVERVIEW_SHARE / 5).max(3);
        u16::try_from(lines.len())
            .unwrap_or(u16::MAX)
            .clamp(1, most)
    }

    fn overview_open(&self, key: &PullKey) -> bool {
        let mode = self.services.conversation_config().overview;
        self.talk
            .data
            .overview_open(key, mode, self.talk.seen.get(&key.id()).copied())
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
        let open = area.height > 1;
        let (lines, regions) = self.bar_lines(&link, area.width, open);
        let rows = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        let scroll = if open {
            self.talk.bar.scroll.min(rows.saturating_sub(area.height))
        } else {
            0
        };
        self.talk.bar.scroll = scroll;
        self.talk.bar.rows = rows;
        // The summary row stays; the rest scrolls under it.
        let mut shown = vec![lines[0].clone()];
        shown.extend(
            lines
                .iter()
                .skip(1 + usize::from(scroll))
                .take(usize::from(area.height.saturating_sub(1)))
                .cloned(),
        );
        cx.paint_component(&TextBlock::new(Text::from_lines(shown)), area);
        self.talk.bar.regions = regions
            .into_iter()
            .filter_map(|(row, cells, action)| {
                let row = if row == 0 {
                    0
                } else {
                    row.checked_sub(scroll).filter(|row| *row >= 1)?
                };
                (row < area.height).then_some((row, cells, action))
            })
            .collect();
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
        let row = mouse.position.y - area.y;
        let column = mouse.position.x - area.x;
        match mouse.kind {
            MouseEventKind::ScrollDown => {
                self.talk.bar.scroll = self.talk.bar.scroll.saturating_add(2);
                Some(Action::Redraw)
            }
            MouseEventKind::ScrollUp => {
                self.talk.bar.scroll = self.talk.bar.scroll.saturating_sub(2);
                Some(Action::Redraw)
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let action = self
                    .talk
                    .bar
                    .regions
                    .iter()
                    .find(|(at, cells, _)| *at == row && cells.contains(&column))
                    .map(|(_, _, action)| action.clone());
                Some(action.map_or(Action::None, |action| {
                    self.bar_action(action, host);
                    Action::Redraw
                }))
            }
            _ => Some(Action::None),
        }
    }

    /// Run a bar action (from a click, or a key).
    pub(super) fn bar_action(&mut self, action: BarAction, host: &UiContext) {
        let Some(link) = self.bar_link() else {
            self.set_status(
                "this review has no pull request conversation",
                StatusSeverity::Info,
            );
            return;
        };
        let target = |key: &PullKey| ReviewPullRequestTarget {
            provider: key.provider.clone(),
            repository: key.repository.clone(),
            number: key.number,
        };
        match action {
            BarAction::Toggle => {
                let open = !self.overview_open(&link.pull);
                if !open {
                    self.mark_conversation_seen(&link.pull);
                }
                self.talk.data.opened.insert(link.pull.clone(), open);
                self.talk.bar.scroll = 0;
            }
            BarAction::Approve => {
                self.open_review(ReviewVerdict::Approve, vec![target(&link.pull)], host);
            }
            BarAction::RequestChanges => {
                self.open_review(
                    ReviewVerdict::RequestChanges,
                    vec![target(&link.pull)],
                    host,
                );
            }
            BarAction::ApproveViewed => {
                let targets: Vec<ReviewPullRequestTarget> =
                    self.viewed_layers().iter().map(target).collect();
                if targets.is_empty() {
                    self.set_status("no layer is fully viewed yet", StatusSeverity::Info);
                } else {
                    self.open_review(ReviewVerdict::Approve, targets, host);
                }
            }
            BarAction::Comment => self.start_conversation_comment(&link),
            BarAction::Open(url) => self.open_url(&url, host),
            BarAction::Refresh => {
                self.set_status("reading the conversation…", StatusSeverity::Info);
                self.signal_conversations(vec![link.pull], host);
            }
            BarAction::ShowAll => self.talk.bar.show_all = !self.talk.bar.show_all,
            BarAction::ShowNew => self.show_new(&link.pull),
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
    fn viewed_layers(&self) -> Vec<PullKey> {
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

    /// The bar's rows for `link` at `width`, open or not, with their clickable cells.
    fn bar_lines(&self, link: &LayerLink, width: u16, open: bool) -> BarRows {
        let mut bar = Bar::new(usize::from(width));
        let entry = self.talk.data.pulls.get(&link.pull);
        let activity = entry.and_then(|entry| entry.activity.as_ref());
        let provider = self.talk.data.provider(&link.pull);
        let capabilities = provider
            .map(|provider| provider.capabilities)
            .unwrap_or_default();
        let seen = self.talk.seen.get(&link.pull.id()).copied();
        // The summary row: what this pull request's state is, then its buttons.
        let mut buttons: Vec<(String, BarAction, Style)> = Vec::new();
        let theme = self.theme;
        if let Some(activity) = activity {
            if self.may_judge(&link.pull) && activity.state != PullState::Merged {
                buttons.push((
                    " Approve ".to_owned(),
                    BarAction::Approve,
                    theme.pill_accent,
                ));
                if capabilities.verdicts.request_changes {
                    buttons.push((
                        " Request changes ".to_owned(),
                        BarAction::RequestChanges,
                        theme.pill,
                    ));
                }
            }
            if capabilities.comment_post && activity.viewer.can_comment {
                buttons.push((" Comment ".to_owned(), BarAction::Comment, theme.pill));
            }
            if let Some(url) = &activity.url {
                buttons.push((
                    format!(" Open {} ", theme.icons.external),
                    BarAction::Open(url.clone()),
                    theme.pill,
                ));
            }
        }
        buttons.push((" ↻ ".to_owned(), BarAction::Refresh, theme.pill));
        let mut summary: Vec<(Span, Option<BarAction>)> = vec![
            (
                Span::styled(
                    format!(
                        " {} ",
                        if open {
                            theme.icons.menu
                        } else {
                            theme.icons.collapsed
                        }
                    ),
                    theme.accent,
                ),
                Some(BarAction::Toggle),
            ),
            (
                Span::styled(format!("#{} ", link.pull.number), theme.badge_open),
                Some(BarAction::Toggle),
            ),
        ];
        match (activity, entry.and_then(|entry| entry.error.as_ref())) {
            (Some(activity), _) => {
                summary.extend(summary_spans(activity, seen, &theme));
            }
            (None, Some(error)) => summary.push((
                Span::styled(
                    format!("could not read the conversation: {error}"),
                    theme.error,
                ),
                None,
            )),
            (None, None) => summary.push((
                Span::styled(
                    format!("{} reading the conversation", theme.icons.ellipsis),
                    theme.muted,
                ),
                None,
            )),
        }
        bar.row_with_buttons(summary, buttons);
        if !open {
            return bar.finish();
        }
        let Some(activity) = activity else {
            return bar.finish();
        };
        self.overview_rows(&mut bar, link, activity, seen);
        bar.finish()
    }

    /// The open overview's rows under the summary.
    fn overview_rows(
        &self,
        bar: &mut Bar,
        link: &LayerLink,
        activity: &PullActivity,
        seen: Option<u64>,
    ) {
        self.overview_head(bar, activity);
        self.overview_reviews(bar, link, activity);
        self.overview_checks(bar, activity);
        self.overview_comments(bar, activity, seen);
        self.overview_actions(bar);
    }

    /// Title, author, and description.
    fn overview_head(&self, bar: &mut Bar, activity: &PullActivity) {
        let theme = self.theme;
        let show_all = self.talk.bar.show_all;
        // Title and author.
        let mut title = vec![(
            Span::styled(format!(" {}", activity.title), theme.title),
            None,
        )];
        if let Some(author) = &activity.author {
            title.push((
                Span::styled(format!("  by {}", author.login), theme.muted),
                None,
            ));
        }
        bar.row(title);
        // Description.
        if !activity.description.trim().is_empty() {
            let rows = super::super::rows::markdown_lines(
                &activity.description,
                bar.width.saturating_sub(4),
                if show_all {
                    usize::MAX
                } else {
                    DESCRIPTION_ROWS
                },
                &theme,
            );
            let cut = !show_all && rows.len() >= DESCRIPTION_ROWS;
            bar.indented(rows, 3);
            if cut {
                bar.row(vec![(
                    Span::styled("   Show the whole description", theme.gap_action),
                    Some(BarAction::ShowAll),
                )]);
            }
        }
    }

    /// Each reviewer's latest verdict and whether it still applies, and who is still asked.
    fn overview_reviews(&self, bar: &mut Bar, link: &LayerLink, activity: &PullActivity) {
        let theme = self.theme;
        let icons = theme.icons;
        let show_all = self.talk.bar.show_all;
        // Reviews: each reviewer's latest verdict, and whether it still applies.
        let latest = activity.latest_reviews();
        if !latest.is_empty() || !activity.requested.is_empty() {
            bar.row(vec![(Span::styled(" Reviews", theme.title), None)]);
        }
        let layer = self.stack_history.as_ref().and_then(|timeline| {
            timeline
                .layers
                .iter()
                .find(|layer| layer.number == link.pull.number)
        });
        for review in latest {
            let (glyph, verb, style) = match review.state {
                ReviewState::Approved => (icons.approved, "approved", theme.badge_resolved),
                ReviewState::ChangesRequested => (icons.changes, "requested changes", theme.error),
                ReviewState::Dismissed => (icons.awaiting, "review dismissed", theme.muted),
                ReviewState::Commented | ReviewState::Pending => {
                    (icons.thread, "commented", theme.muted)
                }
            };
            let mut spans = vec![
                (Span::styled(format!("   {glyph} "), style), None),
                (Span::styled(review.author.login.clone(), theme.text), None),
                (Span::styled(format!(" {verb}"), style), None),
                (
                    Span::styled(format!(" {}", conv::ago(review.at_ms)), theme.muted),
                    None,
                ),
            ];
            if matches!(
                review.state,
                ReviewState::Approved | ReviewState::ChangesRequested
            ) && let Some(commit) = &review.commit_oid
            {
                let currency = layer.map_or_else(
                    || {
                        if *commit == activity.head_oid {
                            Currency::Current
                        } else {
                            Currency::Unknown
                        }
                    },
                    |layer| {
                        bcode_code_review::conversation::review_currency(
                            layer,
                            commit,
                            &activity.head_oid,
                        )
                    },
                );
                match currency {
                    Currency::Current => {}
                    Currency::Rebased => spans.push((
                        Span::styled("  · given before a rebase; still applies", theme.muted),
                        None,
                    )),
                    Currency::Outdated => spans.push((
                        Span::styled("  · on an older version", theme.badge_outdated),
                        None,
                    )),
                    Currency::Unknown => {
                        spans.push((Span::styled("  · on an earlier push", theme.muted), None));
                    }
                }
            }
            if let Some(url) = &review.url {
                spans.push((
                    Span::styled(format!("  {}", icons.external), theme.gap_action),
                    Some(BarAction::Open(url.clone())),
                ));
            }
            bar.row(spans);
            if !review.body.trim().is_empty() {
                let rows = super::super::rows::markdown_lines(
                    &review.body,
                    bar.width.saturating_sub(6),
                    if show_all { usize::MAX } else { COMMENT_ROWS },
                    &theme,
                );
                bar.indented(rows, 5);
            }
        }
        if !activity.requested.is_empty() {
            let names: Vec<String> = activity
                .requested
                .iter()
                .map(|participant| participant.login.clone())
                .collect();
            bar.row(vec![(
                Span::styled(
                    format!("   {} waiting for {}", icons.awaiting, names.join(", ")),
                    theme.notice,
                ),
                None,
            )]);
        }
    }

    /// Checks that need attention, each opening its page, and how many pass.
    fn overview_checks(&self, bar: &mut Bar, activity: &PullActivity) {
        let theme = self.theme;
        let icons = theme.icons;
        // Checks that need attention, each opening its page.
        let attention: Vec<_> = activity
            .checks
            .checks
            .iter()
            .filter(|check| matches!(check.state, CheckState::Failing | CheckState::Pending))
            .collect();
        if !activity.checks.checks.is_empty() {
            bar.row(vec![(Span::styled(" Checks", theme.title), None)]);
            for check in &attention {
                let (glyph, style, word) = match check.state {
                    CheckState::Failing => (icons.changes, theme.error, "failing"),
                    _ => (icons.running, theme.notice, "running"),
                };
                let mut spans = vec![
                    (Span::styled(format!("   {glyph} "), style), None),
                    (Span::styled(check.name.clone(), theme.text), None),
                    (Span::styled(format!(" {word}"), style), None),
                ];
                if let Some(url) = &check.url {
                    spans.push((
                        Span::styled(format!("  Open {}", icons.external), theme.gap_action),
                        Some(BarAction::Open(url.clone())),
                    ));
                }
                bar.row(spans);
            }
            let passing = activity.checks.count(CheckState::Passing);
            let neutral = activity.checks.count(CheckState::Neutral);
            if passing + neutral > 0 {
                bar.row(vec![(
                    Span::styled(
                        format!(
                            "   {} {passing} passing{}",
                            icons.approved,
                            if neutral > 0 {
                                format!(", {neutral} skipped")
                            } else {
                                String::new()
                            }
                        ),
                        theme.muted,
                    ),
                    None,
                )]);
            }
        }
    }

    /// The conversation's comments (the newest few, or all).
    fn overview_comments(&self, bar: &mut Bar, activity: &PullActivity, seen: Option<u64>) {
        let theme = self.theme;
        let icons = theme.icons;
        let show_all = self.talk.bar.show_all;
        // The conversation.
        if !activity.comments.is_empty() {
            bar.row(vec![(
                Span::styled(
                    format!(
                        " Conversation · {} comment{}",
                        activity.comments.len(),
                        if activity.comments.len() == 1 {
                            ""
                        } else {
                            "s"
                        }
                    ),
                    theme.title,
                ),
                None,
            )]);
            let skip = if show_all {
                0
            } else {
                activity.comments.len().saturating_sub(RECENT_COMMENTS)
            };
            if skip > 0 {
                bar.row(vec![(
                    Span::styled(format!("   Show {skip} earlier comments"), theme.gap_action),
                    Some(BarAction::ShowAll),
                )]);
            }
            for comment in activity.comments.iter().skip(skip) {
                let new = seen.is_some_and(|seen| {
                    !comment.author.is_viewer
                        && comment.updated_at_ms.or(comment.created_at_ms).unwrap_or(0) > seen
                });
                let mut byline = vec![
                    (
                        Span::styled(format!("   {}", comment.author.login), theme.text),
                        None,
                    ),
                    (
                        Span::styled(
                            format!(" {}", conv::ago(comment.created_at_ms)),
                            theme.muted,
                        ),
                        None,
                    ),
                ];
                if new {
                    byline.push((
                        Span::styled(format!(" {} new", icons.new), theme.badge_draft),
                        None,
                    ));
                }
                bar.row(byline);
                let rows = super::super::rows::markdown_lines(
                    &comment.body,
                    bar.width.saturating_sub(6),
                    if show_all { usize::MAX } else { COMMENT_ROWS },
                    &theme,
                );
                bar.indented(rows, 5);
            }
        }
    }

    /// Actions on the whole stack, and collapsing.
    fn overview_actions(&self, bar: &mut Bar) {
        let theme = self.theme;
        // Actions on the whole stack.
        let viewed = self.viewed_layers().len();
        let mut actions: Vec<(String, BarAction, Style)> = Vec::new();
        if self.talk.data.links.len() > 1 && viewed > 0 {
            actions.push((
                format!(
                    " Approve {viewed} reviewed layer{} ",
                    if viewed == 1 { "" } else { "s" }
                ),
                BarAction::ApproveViewed,
                theme.pill,
            ));
        }
        actions.push((" Collapse ".to_owned(), BarAction::Toggle, theme.pill));
        bar.row_with_buttons(Vec::new(), actions);
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

/// The summary of `activity`: state, decision, approvers, checks, open threads, what is new.
fn summary_spans(
    activity: &PullActivity,
    seen: Option<u64>,
    theme: &crate::theme::ReviewTheme,
) -> Vec<(Span, Option<BarAction>)> {
    let icons = theme.icons;
    let separator = || (Span::styled(icons.separator, theme.muted), None);
    let mut spans: Vec<(Span, Option<BarAction>)> = Vec::new();
    match activity.state {
        PullState::Draft => spans.push((Span::styled("draft ", theme.badge_outdated), None)),
        PullState::Merged => spans.push((Span::styled("merged ", theme.badge_resolved), None)),
        PullState::Closed => spans.push((Span::styled("closed ", theme.error), None)),
        PullState::Open => {}
    }
    let approvers: Vec<String> = activity
        .latest_reviews()
        .iter()
        .filter(|review| review.state == ReviewState::Approved)
        .map(|review| review.author.login.clone())
        .collect();
    let (text, style) = match activity.decision {
        Some(ReviewDecision::Approved) => {
            (format!("{} Approved", icons.approved), theme.badge_resolved)
        }
        Some(ReviewDecision::ChangesRequested) => {
            (format!("{} Changes requested", icons.changes), theme.error)
        }
        Some(ReviewDecision::ReviewRequired) => {
            (format!("{} Review required", icons.awaiting), theme.notice)
        }
        None if !approvers.is_empty() => {
            (format!("{} Approved", icons.approved), theme.badge_resolved)
        }
        None => (format!("{} No reviews yet", icons.awaiting), theme.muted),
    };
    spans.push((Span::styled(text, style), Some(BarAction::Toggle)));
    if !approvers.is_empty() {
        spans.push((
            Span::styled(format!(" by {}", approvers.join(", ")), theme.muted),
            Some(BarAction::Toggle),
        ));
    }
    let checks = &activity.checks;
    if !checks.checks.is_empty() || checks.state.is_some() {
        spans.push(separator());
        let failing = checks.count(CheckState::Failing);
        let pending = checks.count(CheckState::Pending);
        let passing = checks.count(CheckState::Passing);
        let total = checks.checks.len();
        let (text, style) = if failing > 0 {
            (format!("{} {failing} failing", icons.changes), theme.error)
        } else if pending > 0 {
            (format!("{} {pending} running", icons.running), theme.notice)
        } else if total > 0 {
            (
                format!("{} {passing}/{total} checks", icons.checks),
                theme.badge_resolved,
            )
        } else {
            match checks.state {
                Some(CheckState::Failing) => {
                    (format!("{} checks failing", icons.changes), theme.error)
                }
                Some(CheckState::Pending) => {
                    (format!("{} checks running", icons.running), theme.notice)
                }
                _ => (
                    format!("{} checks pass", icons.checks),
                    theme.badge_resolved,
                ),
            }
        };
        spans.push((Span::styled(text, style), Some(BarAction::Toggle)));
    }
    let open = activity.open_threads();
    if open > 0 {
        spans.push(separator());
        spans.push((
            Span::styled(format!("{} {open} open", icons.thread), theme.thread),
            None,
        ));
    }
    let waiting = activity
        .threads
        .iter()
        .filter(|thread| bcode_code_review::conversation::waiting_on_viewer(activity, thread))
        .count();
    if waiting > 0 {
        spans.push((Span::raw(" "), None));
        spans.push((
            Span::styled(format!(" {waiting} waiting on you "), theme.pill_warning),
            Some(BarAction::ShowNew),
        ));
    }
    if conv::is_new(activity, seen) {
        let count = new_count(activity, seen.unwrap_or(0));
        spans.push(separator());
        spans.push((
            Span::styled(format!("{} {count} new", icons.new), theme.badge_draft),
            Some(BarAction::ShowNew),
        ));
    }
    spans
}

/// How many reviews and comments from others are newer than `seen`.
fn new_count(activity: &PullActivity, seen: u64) -> usize {
    let reviews = activity
        .reviews
        .iter()
        .filter(|review| !review.author.is_viewer && review.at_ms.unwrap_or(0) > seen)
        .count();
    let comments = activity
        .threads
        .iter()
        .flat_map(|thread| &thread.comments)
        .chain(&activity.comments)
        .filter(|comment| {
            !comment.author.is_viewer
                && comment.updated_at_ms.or(comment.created_at_ms).unwrap_or(0) > seen
        })
        .count();
    reviews + comments
}

/// Rows of the bar being built, with their clickable cells.
struct Bar {
    width: usize,
    lines: Vec<Line>,
    regions: Vec<BarRegion>,
}

impl Bar {
    const fn new(width: usize) -> Self {
        Self {
            width,
            lines: Vec::new(),
            regions: Vec::new(),
        }
    }

    fn row_number(&self) -> u16 {
        u16::try_from(self.lines.len()).unwrap_or(u16::MAX)
    }

    /// A row of spans (each optionally clickable), cut at the width.
    fn row(&mut self, parts: Vec<(Span, Option<BarAction>)>) {
        let row = self.row_number();
        let mut x = 0;
        let mut spans = Vec::new();
        for (span, action) in parts {
            let cells = display_width(&span.content);
            if x >= self.width {
                break;
            }
            if let Some(action) = action {
                let end = (x + cells).min(self.width);
                self.regions.push((row, cell(x)..cell(end), action));
            }
            x += cells;
            spans.push(span);
        }
        self.lines
            .push(Line::from_spans(spans).truncate(self.width));
    }

    /// A row of `parts` with `buttons` at its right end; the parts are cut to make room, and
    /// buttons that still do not fit are dropped from the end.
    fn row_with_buttons(
        &mut self,
        parts: Vec<(Span, Option<BarAction>)>,
        buttons: Vec<(String, BarAction, Style)>,
    ) {
        let row = self.row_number();
        let mut kept = Vec::new();
        let mut room = self.width;
        for (label, action, style) in buttons {
            let cells = display_width(&label) + 1;
            // Keep at least a third of the row for the summary.
            if cells > room.saturating_sub(self.width / 3) && !parts.is_empty() {
                break;
            }
            if cells > room {
                break;
            }
            room -= cells;
            kept.push((label, action, style));
        }
        let mut x = 0;
        let mut spans = Vec::new();
        for (span, action) in parts {
            if x >= room {
                break;
            }
            let cells = display_width(&span.content).min(room - x);
            if let Some(action) = action {
                self.regions.push((row, cell(x)..cell(x + cells), action));
            }
            x += cells;
            spans.push(span);
        }
        let mut line = Line::from_spans(spans).truncate(room);
        let used = line.width();
        line.spans.push(Span::raw(" ".repeat(room - used)));
        let mut x = room;
        for (label, action, style) in kept {
            let cells = display_width(&label);
            line.spans.push(Span::raw(" "));
            x += 1;
            self.regions.push((row, cell(x)..cell(x + cells), action));
            line.spans.push(Span::styled(label, style));
            x += cells;
        }
        self.lines.push(line.truncate(self.width));
    }

    /// `rows` indented by `indent` cells.
    fn indented(&mut self, rows: Vec<Line>, indent: usize) {
        let pad = " ".repeat(indent.min(self.width));
        for row in rows {
            let mut spans = vec![Span::raw(pad.clone())];
            spans.extend(row.spans);
            self.lines
                .push(Line::from_spans(spans).truncate(self.width));
        }
    }

    fn finish(self) -> BarRows {
        (self.lines, self.regions)
    }
}

fn cell(x: usize) -> u16 {
    u16::try_from(x).unwrap_or(u16::MAX)
}
