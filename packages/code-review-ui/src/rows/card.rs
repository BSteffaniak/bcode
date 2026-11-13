//! A pull request's card: the top of its page, before its files.
//!
//! The document's [`SourceIntroItem`], ending in the same torn break as between files. See
//! `docs/review-forge-conversation.md` and `docs/review-file-separation.md`.
//!
//! * The title and author (unless a stack layer's banner already names it).
//! * The status line: the review decision and approvers, checks, open threads, *waiting on
//!   you*, and what is new, with buttons for what the provider and the viewer's permissions
//!   allow (Approve, Request changes, Comment, Open, Refresh) and a fold toggle.
//! * Open (the default): the description, each reviewer's verdict and whether it still applies,
//!   who is still asked, failing checks, and the conversation, each with *Show all*.
//! * A whole-stack source gets one status line per layer instead.
//!
//! The same status line is the review's pull request bar ([`status_row`]).

use super::file::{Density, Separation};
use super::{ActionCells, ItemAction, ItemRows};
use crate::config::conversation::OverviewMode;
use crate::conversation::{self as conv, Conversations, LayerLink, PullKey};
use crate::theme::ReviewTheme;
use bcode_code_review::conversation::Currency;
use bcode_code_review::document::SourceIntroItem;
use bcode_code_review_models::{
    CheckState, PullActivity, PullState, ReviewDecision, ReviewState, StackTimeline,
};
use bmux_tui::style::Style;
use bmux_tui::text::{Line, Span};
use bmux_tui::text_width::display_width;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

/// Conversation comments shown before *Show all*.
const RECENT_COMMENTS: usize = 3;
/// Description rows shown before *Show all*.
const DESCRIPTION_ROWS: usize = 8;
/// Rows of one comment before *Show all*.
const COMMENT_ROWS: usize = 6;
/// Indent of the card's body.
const INDENT: usize = 3;

/// A clickable part of a card or of the pull request bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardAction {
    /// Open or fold the card (on the bar: go to the card).
    Toggle,
    /// Approve the pull request.
    Approve,
    /// Request changes.
    RequestChanges,
    /// Comment in the conversation.
    Comment,
    /// Approve every fully viewed layer.
    ApproveViewed,
    /// Open the pull request's web page.
    OpenPull,
    /// Open the web page of the review at this index of the latest reviews.
    OpenReview(u16),
    /// Open the web page of the check at this index of the checks needing attention.
    OpenCheck(u16),
    /// Open the web page of the layer at this index of a whole stack's layers.
    OpenLayer(u16),
    /// Read the conversation again.
    Refresh,
    /// Show the whole description and conversation (or less again).
    ShowAll,
    /// Go to what is new.
    ShowNew,
}

/// What cards are drawn from: the conversations and how the viewer left them.
#[derive(Debug, Clone, Copy)]
pub struct CardContext<'a> {
    /// The review's conversations.
    pub conversations: &'a Conversations,
    /// Newest activity seen per pull request.
    pub seen: &'a BTreeMap<String, u64>,
    /// The stack's history, for whether reviews still apply.
    pub history: Option<&'a StackTimeline>,
    /// Sources whose cards show everything.
    pub show_all: &'a BTreeSet<String>,
    /// Layers fully viewed that the viewer may approve.
    pub viewed_layers: usize,
    /// How cards open by default.
    pub overview: OverviewMode,
    /// Each thread's viewed state, by the review's thread id.
    pub threads: &'a BTreeMap<String, super::ThreadLook>,
    /// Changes whenever anything a card shows changed.
    pub revision: u64,
}

impl CardContext<'_> {
    fn seen(&self, key: &PullKey) -> Option<u64> {
        self.seen.get(&key.id()).copied()
    }

    /// Whether the viewer may approve or request changes on `key`.
    #[must_use]
    pub fn may_judge(&self, key: &PullKey) -> bool {
        let supported = self
            .conversations
            .provider(key)
            .is_some_and(|provider| provider.capabilities.verdicts.approve);
        let allowed = self
            .conversations
            .activity(key)
            .is_none_or(|activity| activity.viewer.can_judge);
        supported && allowed
    }

    /// Whether `key`'s card is open.
    #[must_use]
    pub fn is_open(&self, key: &PullKey) -> bool {
        self.conversations
            .overview_open(key, self.overview, self.seen(key))
    }
}

/// One clickable part of a built row: row, cells, action.
type Region = (u16, Range<u16>, CardAction);

/// Rows being built, with their clickable cells.
struct Rows {
    width: usize,
    lines: Vec<Line>,
    regions: Vec<Region>,
}

fn cell(x: usize) -> u16 {
    u16::try_from(x).unwrap_or(u16::MAX)
}

impl Rows {
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
    fn row(&mut self, parts: Vec<(Span, Option<CardAction>)>) {
        let row = self.row_number();
        let mut x = 0;
        let mut spans = Vec::new();
        for (span, action) in parts {
            if x >= self.width {
                break;
            }
            let cells = display_width(&span.content);
            if let Some(action) = action {
                self.regions
                    .push((row, cell(x)..cell((x + cells).min(self.width)), action));
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
        parts: Vec<(Span, Option<CardAction>)>,
        buttons: Vec<(String, CardAction, Style)>,
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

    fn actions(regions: Vec<Region>) -> Vec<ActionCells<ItemAction>> {
        regions
            .into_iter()
            .map(|(row, cells, action)| ActionCells {
                row,
                cells,
                action: ItemAction::Card(action),
            })
            .collect()
    }
}

/// The buttons of `link`'s status line.
fn buttons(
    link: &LayerLink,
    activity: Option<&PullActivity>,
    cx: &CardContext<'_>,
    theme: &ReviewTheme,
) -> Vec<(String, CardAction, Style)> {
    let mut buttons = Vec::new();
    let capabilities = cx
        .conversations
        .provider(&link.pull)
        .map(|provider| provider.capabilities)
        .unwrap_or_default();
    if let Some(activity) = activity {
        if cx.may_judge(&link.pull) && activity.state != PullState::Merged {
            buttons.push((
                " Approve ".to_owned(),
                CardAction::Approve,
                theme.pill_accent,
            ));
            if capabilities.verdicts.request_changes {
                buttons.push((
                    " Request changes ".to_owned(),
                    CardAction::RequestChanges,
                    theme.pill,
                ));
            }
        }
        if capabilities.comment_post && activity.viewer.can_comment {
            buttons.push((" Comment ".to_owned(), CardAction::Comment, theme.pill));
        }
        if activity.url.is_some() {
            buttons.push((
                format!(" Open {} ", theme.icons.external),
                CardAction::OpenPull,
                theme.pill,
            ));
        }
    }
    if capabilities.activity {
        buttons.push((" ↻ ".to_owned(), CardAction::Refresh, theme.pill));
    }
    buttons
}

/// The status of `link`: its decision, checks, threads, and what is new, or why it is unknown.
fn status_parts(
    link: &LayerLink,
    cx: &CardContext<'_>,
    theme: &ReviewTheme,
) -> Vec<(Span, Option<CardAction>)> {
    let entry = cx.conversations.pulls.get(&link.pull);
    let reports = cx
        .conversations
        .provider(&link.pull)
        .is_some_and(|provider| provider.capabilities.activity);
    match (
        entry.and_then(|entry| entry.activity.as_ref()),
        entry.and_then(|entry| entry.error.as_ref()),
    ) {
        (Some(activity), _) => summary_spans(activity, new_count(activity, &link.pull, cx), theme),
        (None, Some(error)) => vec![(
            Span::styled(
                format!("could not read the conversation: {error}"),
                theme.error,
            ),
            None,
        )],
        (None, None) if reports => vec![(
            Span::styled(
                format!("{} reading the conversation", theme.icons.ellipsis),
                theme.muted,
            ),
            None,
        )],
        (None, None) => Vec::new(),
    }
}

/// The pull request bar's one row for `link`: its number, status, and buttons. Clicking the
/// number or the status goes to its card.
#[must_use]
pub fn status_row(
    link: &LayerLink,
    cx: &CardContext<'_>,
    theme: &ReviewTheme,
    width: usize,
) -> (Line, Vec<(Range<u16>, CardAction)>) {
    let mut rows = Rows::new(width);
    let activity = cx.conversations.activity(&link.pull);
    let mut parts = vec![(
        Span::styled(format!(" #{} ", link.pull.number), theme.badge_open),
        Some(CardAction::Toggle),
    )];
    parts.extend(
        status_parts(link, cx, theme)
            .into_iter()
            .map(|(span, action)| (span, action.or(Some(CardAction::Toggle)))),
    );
    rows.row_with_buttons(parts, buttons(link, activity, cx, theme));
    let status = rows.lines.pop().unwrap_or_default();
    (
        status,
        rows.regions
            .into_iter()
            .map(|(_, cells, action)| (cells, action))
            .collect(),
    )
}

/// Rows of `item`'s card at `width`, ending in the break before the source's files.
#[must_use]
pub fn card_rows(
    item: &SourceIntroItem,
    cx: &CardContext<'_>,
    density: Density,
    separation: Separation,
    theme: &ReviewTheme,
    width: usize,
) -> ItemRows {
    let mut rows = Rows::new(width);
    let links: Vec<&LayerLink> = cx
        .conversations
        .links
        .iter()
        .filter(|link| link.source_id == item.source_id)
        .collect();
    match links.as_slice() {
        [] => {}
        [link] => single(&mut rows, link, item.under_header, cx, theme),
        layers => stack(&mut rows, layers, cx, theme),
    }
    let mut end = super::file::end_rows(
        super::file::Break { end: None },
        separation,
        density,
        theme,
        width,
    );
    let mut built = rows.lines;
    let actions = Rows::actions(rows.regions);
    built.append(&mut end.lines);
    ItemRows::lines(built, actions)
}

/// The card of one pull request.
fn single(
    rows: &mut Rows,
    link: &LayerLink,
    under_header: bool,
    cx: &CardContext<'_>,
    theme: &ReviewTheme,
) {
    let activity = cx.conversations.activity(&link.pull);
    let open = cx.is_open(&link.pull);
    // The title, unless the layer's banner above already gives it.
    if !under_header {
        let title = activity.map_or(link.title.as_str(), |activity| activity.title.as_str());
        let mut spans = vec![
            (Span::styled(theme.icons.accent_bar, theme.accent), None),
            (
                Span::styled(format!(" #{} ", link.pull.number), theme.badge_open),
                None,
            ),
            (Span::styled(title.to_owned(), theme.title), None),
        ];
        if let Some(author) = activity.and_then(|activity| activity.author.as_ref()) {
            spans.push((
                Span::styled(format!("  by {}", author.login), theme.muted),
                None,
            ));
        }
        rows.row(spans);
    }
    let mut status = vec![(
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
        Some(CardAction::Toggle),
    )];
    status.extend(status_parts(link, cx, theme));
    rows.row_with_buttons(status, buttons(link, activity, cx, theme));
    let (true, Some(activity)) = (open, activity) else {
        return;
    };
    let show_all = cx.show_all.contains(&link.source_id);
    description(rows, activity, show_all, theme);
    reviews(rows, link, activity, cx, show_all, theme);
    checks(rows, activity, theme);
    comments(rows, activity, cx.seen(&link.pull), show_all, theme);
    if cx.conversations.links.len() > 1 && cx.viewed_layers > 0 {
        let viewed = cx.viewed_layers;
        rows.row_with_buttons(
            Vec::new(),
            vec![(
                format!(
                    " Approve {viewed} reviewed layer{} ",
                    if viewed == 1 { "" } else { "s" }
                ),
                CardAction::ApproveViewed,
                theme.pill,
            )],
        );
    }
}

/// A whole stack's card: one status line per layer.
fn stack(rows: &mut Rows, layers: &[&LayerLink], cx: &CardContext<'_>, theme: &ReviewTheme) {
    rows.row(vec![(
        Span::styled(
            format!(" Pull requests in this stack · {}", layers.len()),
            theme.title,
        ),
        None,
    )]);
    for (index, link) in layers.iter().enumerate().rev() {
        let mut parts = vec![
            (
                Span::styled(format!("   #{} ", link.pull.number), theme.badge_open),
                None,
            ),
            (Span::styled(format!("{}  ", link.title), theme.text), None),
        ];
        parts.extend(status_parts(link, cx, theme));
        let open = cx
            .conversations
            .activity(&link.pull)
            .and_then(|activity| activity.url.as_ref())
            .map(|_| {
                vec![(
                    format!(" Open {} ", theme.icons.external),
                    CardAction::OpenLayer(u16::try_from(index).unwrap_or(u16::MAX)),
                    theme.pill,
                )]
            })
            .unwrap_or_default();
        rows.row_with_buttons(parts, open);
    }
}

/// The description, folded after a few rows.
fn description(rows: &mut Rows, activity: &PullActivity, show_all: bool, theme: &ReviewTheme) {
    if activity.description.trim().is_empty() {
        return;
    }
    let lines = super::markdown_lines(
        &activity.description,
        rows.width.saturating_sub(INDENT + 1),
        if show_all {
            usize::MAX
        } else {
            DESCRIPTION_ROWS
        },
        theme,
    );
    let cut = !show_all && lines.len() >= DESCRIPTION_ROWS;
    rows.indented(lines, INDENT);
    if cut {
        rows.row(vec![(
            Span::styled("   Show the whole description", theme.gap_action),
            Some(CardAction::ShowAll),
        )]);
    }
}

/// Each reviewer's latest verdict and whether it still applies, and who is still asked.
fn reviews(
    rows: &mut Rows,
    link: &LayerLink,
    activity: &PullActivity,
    cx: &CardContext<'_>,
    show_all: bool,
    theme: &ReviewTheme,
) {
    let icons = theme.icons;
    let latest = activity.latest_reviews();
    if !latest.is_empty() || !activity.requested.is_empty() {
        rows.row(vec![(Span::styled(" Reviews", theme.title), None)]);
    }
    let layer = cx.history.and_then(|timeline| {
        timeline
            .layers
            .iter()
            .find(|layer| layer.number == link.pull.number)
    });
    for (index, review) in latest.iter().enumerate() {
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
            let note = match currency {
                Currency::Current => None,
                Currency::Rebased => {
                    Some(("  · given before a rebase; still applies", theme.muted))
                }
                Currency::Outdated => Some(("  · on an older version", theme.badge_outdated)),
                Currency::Unknown => Some(("  · on an earlier push", theme.muted)),
            };
            if let Some((text, style)) = note {
                spans.push((Span::styled(text, style), None));
            }
        }
        if review.url.is_some() {
            spans.push((
                Span::styled(format!("  {}", icons.external), theme.gap_action),
                Some(CardAction::OpenReview(
                    u16::try_from(index).unwrap_or(u16::MAX),
                )),
            ));
        }
        rows.row(spans);
        if !review.body.trim().is_empty() {
            let lines = super::markdown_lines(
                &review.body,
                rows.width.saturating_sub(6),
                if show_all { usize::MAX } else { COMMENT_ROWS },
                theme,
            );
            rows.indented(lines, 5);
        }
    }
    if !activity.requested.is_empty() {
        let names: Vec<String> = activity
            .requested
            .iter()
            .map(|participant| participant.login.clone())
            .collect();
        rows.row(vec![(
            Span::styled(
                format!("   {} waiting for {}", icons.awaiting, names.join(", ")),
                theme.notice,
            ),
            None,
        )]);
    }
}

/// The checks needing attention (failing, then running), in order.
#[must_use]
pub fn attention_checks(activity: &PullActivity) -> Vec<&bcode_code_review_models::Check> {
    activity
        .checks
        .checks
        .iter()
        .filter(|check| matches!(check.state, CheckState::Failing | CheckState::Pending))
        .collect()
}

/// Checks that need attention, each opening its page, and how many pass.
fn checks(rows: &mut Rows, activity: &PullActivity, theme: &ReviewTheme) {
    let icons = theme.icons;
    if activity.checks.checks.is_empty() {
        return;
    }
    rows.row(vec![(Span::styled(" Checks", theme.title), None)]);
    for (index, check) in attention_checks(activity).into_iter().enumerate() {
        let (glyph, style, word) = match check.state {
            CheckState::Failing => (icons.changes, theme.error, "failing"),
            _ => (icons.running, theme.notice, "running"),
        };
        let mut spans = vec![
            (Span::styled(format!("   {glyph} "), style), None),
            (Span::styled(check.name.clone(), theme.text), None),
            (Span::styled(format!(" {word}"), style), None),
        ];
        if check.url.is_some() {
            spans.push((
                Span::styled(format!("  Open {}", icons.external), theme.gap_action),
                Some(CardAction::OpenCheck(
                    u16::try_from(index).unwrap_or(u16::MAX),
                )),
            ));
        }
        rows.row(spans);
    }
    let passing = activity.checks.count(CheckState::Passing);
    let neutral = activity.checks.count(CheckState::Neutral);
    if passing + neutral > 0 {
        let skipped = if neutral > 0 {
            format!(", {neutral} skipped")
        } else {
            String::new()
        };
        rows.row(vec![(
            Span::styled(
                format!("   {} {passing} passing{skipped}", icons.approved),
                theme.muted,
            ),
            None,
        )]);
    }
}

/// The conversation's comments (the newest few, or all).
fn comments(
    rows: &mut Rows,
    activity: &PullActivity,
    seen: Option<u64>,
    show_all: bool,
    theme: &ReviewTheme,
) {
    if activity.comments.is_empty() {
        return;
    }
    let count = activity.comments.len();
    rows.row(vec![(
        Span::styled(
            format!(
                " Conversation · {count} comment{}",
                if count == 1 { "" } else { "s" }
            ),
            theme.title,
        ),
        None,
    )]);
    let skip = if show_all {
        0
    } else {
        count.saturating_sub(RECENT_COMMENTS)
    };
    if skip > 0 {
        rows.row(vec![(
            Span::styled(format!("   Show {skip} earlier comments"), theme.gap_action),
            Some(CardAction::ShowAll),
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
                Span::styled(format!(" {} new", theme.icons.new), theme.badge_draft),
                None,
            ));
        }
        rows.row(byline);
        let lines = super::markdown_lines(
            &comment.body,
            rows.width.saturating_sub(6),
            if show_all { usize::MAX } else { COMMENT_ROWS },
            theme,
        );
        rows.indented(lines, 5);
    }
}

/// The summary of `activity`: state, decision, approvers, checks, open threads, what is new.
fn summary_spans(
    activity: &PullActivity,
    new: usize,
    theme: &ReviewTheme,
) -> Vec<(Span, Option<CardAction>)> {
    let icons = theme.icons;
    let separator = || (Span::styled(icons.separator, theme.muted), None);
    let mut spans: Vec<(Span, Option<CardAction>)> = Vec::new();
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
    spans.push((Span::styled(text, style), None));
    if !approvers.is_empty() {
        spans.push((
            Span::styled(format!(" by {}", approvers.join(", ")), theme.muted),
            None,
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
        spans.push((Span::styled(text, style), None));
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
            Some(CardAction::ShowNew),
        ));
    }
    if new > 0 {
        spans.push(separator());
        spans.push((
            Span::styled(format!("{} {new} new", icons.new), theme.badge_draft),
            Some(CardAction::ShowNew),
        ));
    }
    spans
}

/// What is new on the pull request `key` for the viewer:
///
/// * reviews and conversation comments from others since it was last looked at;
/// * in its threads, what view marks say: nothing in a viewed thread, the comments since in one
///   viewed before, and in one never viewed, comments from others since it was last looked at.
///
/// Nothing is new on a pull request never looked at.
fn new_count(activity: &PullActivity, key: &PullKey, cx: &CardContext<'_>) -> usize {
    let Some(seen) = cx.seen(key) else {
        return 0;
    };
    let fresh = |comment: &bcode_code_review_models::RemoteComment| {
        !comment.author.is_viewer
            && comment.updated_at_ms.or(comment.created_at_ms).unwrap_or(0) > seen
    };
    let reviews = activity
        .reviews
        .iter()
        .filter(|review| !review.author.is_viewer && review.at_ms.unwrap_or(0) > seen)
        .count();
    let conversation = activity
        .comments
        .iter()
        .filter(|comment| fresh(comment))
        .count();
    let pull = key.id();
    let threads: usize = activity
        .threads
        .iter()
        .map(|thread| {
            let id = crate::notes::remote_thread_id(&pull, &thread.id);
            match cx.threads.get(&id).map(|look| look.viewed) {
                Some(bcode_code_review::threads::ThreadViewed::Viewed) => 0,
                Some(bcode_code_review::threads::ThreadViewed::Stale { new }) => new,
                _ => thread
                    .comments
                    .iter()
                    .filter(|comment| fresh(comment))
                    .count(),
            }
        })
        .sum();
    reviews + conversation + threads
}
