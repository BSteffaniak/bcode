//! Review chrome: the header bar, the footer, and toasts.
//!
//! * **Header.** Title, source pills (BMUX `badge`), `+/−` totals, viewed progress (text and a
//!   BMUX `progress_bar`), the draft count (a warning pill while the review is unsaved, since
//!   leaving would lose them), the layout mode as a pill, and a filled publish `split_button`. Every
//!   part is measured by its own component or text width, and [`plan_header`] places them; when
//!   the row is too narrow, parts are dropped in [`HEADER_ELISION_ORDER`] while the title would
//!   get fewer than its width (at most [`TITLE_KEEP`]) cells, then the title is truncated.
//!   Painting and the publish button's hit regions read the same plan.
//! * **Footer.** BMUX `key_hint_bar` with the contextual hints that fit whole (trailing hints
//!   are dropped first, never cut), and a BMUX `status_bar` message on the right that takes at
//!   most half the row.
//! * **Toasts.** Async results and errors as a BMUX `toast_stack` at the bottom right of the
//!   body; each expires after a few seconds (errors stay longer).

use super::theme::ReviewTheme;
use bmux_tui::component::{Component, Constraints, LayoutCx};
use bmux_tui::event::{Event, MouseEventKind};
use bmux_tui::geometry::{Rect, Size};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::text::{Line, Span};
use bmux_tui_components::badge::{BadgeComponent, BadgePolicy, BadgeSeverity};
use bmux_tui_components::key_hint_bar::{
    KeyHint, KeyHintBar, KeyHintBarComponent, KeyHintBarPolicy,
};
use bmux_tui_components::menu::MenuItem;
use bmux_tui_components::progress_bar::{
    ProgressBarComponent, ProgressBarPolicy, ProgressBarValue,
};
use bmux_tui_components::split_button::{SplitButton, SplitButtonState};
use bmux_tui_components::status_bar::{
    StatusBarComponent, StatusBarPolicy, StatusSegment, StatusSeverity,
};
use bmux_tui_components::toast_stack::{
    ToastItem, ToastPlacement, ToastSeverity, ToastStack, ToastStackComponent, ToastStackOutcome,
    ToastStackPolicy, ToastStackState,
};
use std::cell::Cell;
use std::time::{Duration, Instant};

/// Cells of the viewed progress bar.
pub const PROGRESS_CELLS: u16 = 10;
/// Cells between header parts.
const HEADER_GAP: u16 = 2;
/// Title cells kept before the title is truncated rather than more parts dropped.
pub const TITLE_KEEP: u16 = 40;
/// Hit id prefix of the publish button.
pub const PUBLISH_ID: &str = "review.publish";
/// Label of the publish button.
const PUBLISH_LABEL: &str = "Publish";
/// How long an informational toast stays.
const TOAST_TTL: Duration = Duration::from_secs(4);
/// How long an error toast stays.
const ERROR_TOAST_TTL: Duration = Duration::from_secs(10);
/// Toasts kept at once (older ones are dropped).
const MAX_TOASTS: usize = 3;
/// Widest toast.
const TOAST_WIDTH: u16 = 48;

/// One part of the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HeaderPart {
    /// Review title (never dropped; truncated last).
    Title,
    /// Source chip by index.
    Chip(usize),
    /// `+N` chip standing for the sources not shown.
    MoreChips,
    /// `+added −removed` over the whole review.
    Totals,
    /// `viewed x/y` text.
    Viewed,
    /// Viewed progress bar.
    Progress,
    /// Draft count.
    Drafts,
    /// Layout mode (`unified` / `split`, `nowrap`).
    Mode,
    /// Publish split button.
    Publish,
}

/// Header parts in the order they are dropped when the row is too narrow (first dropped
/// first). Source chips after the first collapse into a `+N` chip before anything else goes;
/// the title is never dropped.
pub const HEADER_ELISION_ORDER: [HeaderPart; 8] = [
    HeaderPart::Publish,
    HeaderPart::Chip(1),
    HeaderPart::Drafts,
    HeaderPart::Totals,
    HeaderPart::Chip(0),
    HeaderPart::Progress,
    HeaderPart::Mode,
    HeaderPart::Viewed,
];

/// What the header shows.
#[derive(Debug, Clone)]
pub struct HeaderModel {
    /// Review title.
    pub title: String,
    /// Source labels.
    pub sources: Vec<String>,
    /// Viewed files and all files.
    pub viewed: (usize, usize),
    /// Lines added and removed across the review.
    pub totals: (u64, u64),
    /// Draft comments.
    pub drafts: usize,
    /// Whether the review is not saved yet (its drafts need the reviewer).
    pub unsaved: bool,
    /// Layout mode label.
    pub mode: String,
}

impl HeaderModel {
    fn viewed_text(&self) -> String {
        format!("viewed {}/{}", self.viewed.0, self.viewed.1)
    }

    fn drafts_text(&self) -> String {
        let text = format!(
            "{} draft{}",
            self.drafts,
            if self.drafts == 1 { "" } else { "s" }
        );
        if self.drafts_need_reviewer() {
            format!(" {text} · unsaved ")
        } else {
            text
        }
    }

    /// Drafts of an unsaved review are lost unless the reviewer saves or publishes them.
    const fn drafts_need_reviewer(&self) -> bool {
        self.unsaved && self.drafts > 0
    }

    fn mode_text(&self) -> String {
        format!(" {} ", self.mode)
    }

    fn totals_line(&self, theme: &ReviewTheme) -> Line {
        Line::from_spans(super::rows::count_spans(
            self.totals.0,
            self.totals.1,
            theme,
        ))
    }
}

/// Placed header parts, left to right, in header-row coordinates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeaderPlan {
    /// Parts and their cells.
    pub parts: Vec<(HeaderPart, Rect)>,
}

impl HeaderPlan {
    /// Area of `part`, when shown.
    #[must_use]
    pub fn area(&self, part: HeaderPart) -> Option<Rect> {
        self.parts
            .iter()
            .find(|(placed, _)| *placed == part)
            .map(|(_, area)| *area)
    }
}

/// Publish menu entry: choose a publisher.
pub const PUBLISH_MENU_CHOOSE: &str = "publish.choose";
/// Publish menu entry: import an external review.
pub const PUBLISH_MENU_IMPORT: &str = "publish.import";
/// Publish menu entry: show the changes since the last review.
pub const PUBLISH_MENU_SINCE_LAST: &str = "publish.since_last";
/// Publish menu entry: mark the review pass complete.
pub const PUBLISH_MENU_COMPLETE: &str = "publish.complete";

/// Publish menu entries (provider-neutral; publishers are chosen in the publish flow).
#[must_use]
pub fn publish_items() -> Vec<MenuItem> {
    vec![
        MenuItem::new(PUBLISH_MENU_CHOOSE, "Publish…"),
        MenuItem::new(PUBLISH_MENU_IMPORT, "Import external review…"),
        MenuItem::new(PUBLISH_MENU_SINCE_LAST, "Changes since last review"),
        MenuItem::new(PUBLISH_MENU_COMPLETE, "Mark review pass complete"),
    ]
}

fn publish_button<'a>(items: &'a [MenuItem], theme: &ReviewTheme) -> SplitButton<'a> {
    SplitButton::new(PUBLISH_LABEL, items)
        .toggle_label(theme.icons.menu)
        .styles(theme.publish)
}

/// A source pill: the label padded by a cell on each side, on the pill fill.
fn chip(label: &str) -> BadgeComponent<'_> {
    BadgeComponent::new("review.chip", label)
        .policy(BadgePolicy {
            padding: 1,
            ..BadgePolicy::bare()
        })
        .severity(BadgeSeverity::Info)
}

fn component_width(component: &impl Component, room: u16) -> u16 {
    let node = component.layout(Constraints::loose(Size::new(room, 1)), &mut LayoutCx::new());
    u16::try_from(node.size.width).unwrap_or(u16::MAX)
}

fn text_width(line: &Line) -> u16 {
    u16::try_from(line.width()).unwrap_or(u16::MAX)
}

/// Place the header parts in a row of `width` cells.
#[must_use]
pub fn plan_header(model: &HeaderModel, width: u16, theme: &ReviewTheme) -> HeaderPlan {
    let items = publish_items();
    let more = model.sources.len().saturating_sub(1);
    let more_label = format!("+{more}");
    // Candidate parts with their widths, in display order.
    let mut left: Vec<(HeaderPart, u16)> = Vec::new();
    for (index, source) in model.sources.iter().enumerate().take(2) {
        let part = if index == 0 {
            HeaderPart::Chip(0)
        } else {
            HeaderPart::Chip(1)
        };
        if index == 1 && more > 1 {
            left.push((
                HeaderPart::MoreChips,
                component_width(&chip(&more_label), width),
            ));
        } else {
            left.push((part, component_width(&chip(source), width)));
        }
    }
    let right: Vec<(HeaderPart, u16)> = vec![
        (HeaderPart::Totals, text_width(&model.totals_line(theme))),
        (
            HeaderPart::Viewed,
            text_width(&Line::raw(model.viewed_text())),
        ),
        (HeaderPart::Progress, PROGRESS_CELLS),
        (
            HeaderPart::Drafts,
            text_width(&Line::raw(model.drafts_text())),
        ),
        (HeaderPart::Mode, text_width(&Line::raw(model.mode_text()))),
        (HeaderPart::Publish, publish_button(&items, theme).width()),
    ];
    let title = text_width(&Line::raw(format!(" {}", model.title)));
    let mut shown: Vec<HeaderPart> = left.iter().chain(&right).map(|(part, _)| *part).collect();
    let used = |shown: &[HeaderPart]| -> u16 {
        let mut total = 0_u16;
        let mut count = 0_u16;
        for (part, cells) in left.iter().chain(&right) {
            if shown.contains(part) {
                total = total.saturating_add(*cells);
                count += 1;
            }
        }
        // One gap after the title and between parts, one cell at the right edge.
        total
            .saturating_add(count.saturating_mul(HEADER_GAP))
            .saturating_add(1)
    };
    // Parts are dropped while the title would get fewer cells than its width (up to
    // `TITLE_KEEP`); only then is the title truncated.
    let title_room = title.min(TITLE_KEEP);
    for part in HEADER_ELISION_ORDER {
        if used(&shown).saturating_add(title_room) <= width {
            break;
        }
        // The `+N` chip goes with the second chip.
        shown.retain(|shown| {
            *shown != part && !(part == HeaderPart::Chip(1) && *shown == HeaderPart::MoreChips)
        });
    }
    let mut parts = Vec::new();
    let title_width = width.saturating_sub(used(&shown)).min(title);
    if title_width > 0 {
        parts.push((HeaderPart::Title, Rect::new(0, 0, title_width, 1)));
    }
    let mut x = title_width;
    for (part, cells) in &left {
        if shown.contains(part) {
            x = x.saturating_add(HEADER_GAP);
            if x.saturating_add(*cells) <= width {
                parts.push((*part, Rect::new(x, 0, *cells, 1)));
            }
            x = x.saturating_add(*cells);
        }
    }
    // Right group, aligned to the right edge.
    let right_width: u16 = right
        .iter()
        .filter(|(part, _)| shown.contains(part))
        .map(|(_, cells)| cells.saturating_add(HEADER_GAP))
        .sum();
    let mut x = width.saturating_sub(right_width.saturating_add(1)).max(x);
    for (part, cells) in &right {
        if shown.contains(part) {
            x = x.saturating_add(HEADER_GAP);
            if x.saturating_add(*cells) <= width {
                parts.push((*part, Rect::new(x, 0, *cells, 1)));
            }
            x = x.saturating_add(*cells);
        }
    }
    HeaderPlan { parts }
}

/// Paint the header row at `area` from `plan`.
pub fn paint_header(
    model: &HeaderModel,
    plan: &HeaderPlan,
    theme: &ReviewTheme,
    publish: &SplitButtonState,
    area: Rect,
    cx: &mut PaintCx<'_, '_>,
) {
    cx.fill(
        LocalRect::new(area.x.into(), area.y.into(), area.width, 1),
        " ",
        theme.header_bar,
    );
    let items = publish_items();
    let more = format!("+{}", model.sources.len().saturating_sub(1));
    let viewed_style = if model.viewed.1 > 0 && model.viewed.0 == model.viewed.1 {
        theme.viewed
    } else {
        theme.muted
    };
    for (part, rect) in &plan.parts {
        let rect = Rect::new(area.x + rect.x, area.y, rect.width, 1);
        let local = LocalRect::new(rect.x.into(), rect.y.into(), rect.width, 1);
        let text = |cx: &mut PaintCx<'_, '_>, line: Line| {
            cx.write_line_with_fallback_style(
                local,
                &line.truncate(usize::from(rect.width)),
                theme.header_bar,
            );
        };
        match part {
            HeaderPart::Title => text(
                cx,
                Line::from_spans(vec![Span::styled(
                    format!(" {}", model.title),
                    theme.tiers.strong,
                )]),
            ),
            HeaderPart::Chip(index) => {
                if let Some(source) = model.sources.get(*index) {
                    cx.paint_component(&chip(source).styles(theme.badges), rect);
                }
            }
            HeaderPart::MoreChips => {
                cx.paint_component(
                    &chip(&more)
                        .severity(BadgeSeverity::Muted)
                        .styles(theme.badges),
                    rect,
                );
            }
            HeaderPart::Totals => text(cx, model.totals_line(theme)),
            HeaderPart::Viewed => text(
                cx,
                Line::from_spans(vec![Span::styled(model.viewed_text(), viewed_style)]),
            ),
            HeaderPart::Progress => {
                let (viewed, total) = model.viewed;
                cx.paint_component(
                    &ProgressBarComponent::new(
                        "review.progress",
                        ProgressBarValue::determinate(viewed as u64, total as u64),
                    )
                    .policy(ProgressBarPolicy::bare().symbols(
                        theme.icons.progress_filled,
                        theme.icons.progress_empty,
                        theme.icons.progress_filled,
                    ))
                    .styles(theme.progress),
                    rect,
                );
            }
            HeaderPart::Drafts => text(
                cx,
                Line::from_spans(vec![Span::styled(
                    model.drafts_text(),
                    if model.drafts_need_reviewer() {
                        theme.pill_warning
                    } else if model.drafts > 0 {
                        theme.badge_draft
                    } else {
                        theme.muted
                    },
                )]),
            ),
            HeaderPart::Mode => text(
                cx,
                Line::from_spans(vec![Span::styled(model.mode_text(), theme.pill)]),
            ),
            HeaderPart::Publish => {
                publish_button(&items, theme).paint(PUBLISH_ID, rect, publish, cx);
            }
        }
    }
}

/// Paint the publish menu popup (after everything else) when it is open.
pub fn paint_publish_menu(
    theme: &ReviewTheme,
    publish: &SplitButtonState,
    button: Rect,
    bounds: Rect,
    cx: &mut PaintCx<'_, '_>,
) {
    let items = publish_items();
    publish_button(&items, theme).paint_menu(PUBLISH_ID, button, bounds, publish, cx);
}

/// Route `event` to the publish button at `button`.
pub fn publish_event(
    theme: &ReviewTheme,
    publish: &mut SplitButtonState,
    button: Rect,
    bounds: Rect,
    event: &bmux_tui::event::Event,
) -> bmux_tui_components::split_button::SplitButtonOutcome {
    let items = publish_items();
    publish_button(&items, theme).handle_event(button, bounds, publish, event)
}

/// Number of leading `hints` that fit whole in `width` cells.
#[must_use]
pub fn fitting_hints(hints: &[KeyHint<'_>], width: u16, separator: &str) -> usize {
    (0..=hints.len())
        .rev()
        .find(|count| {
            let text = KeyHintBar::new(&hints[..*count])
                .policy(KeyHintBarPolicy::compact().separator(separator))
                .text();
            bmux_tui::text_width::display_width(&text) < usize::from(width)
        })
        .unwrap_or(0)
}

/// A status message with its severity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// Message text.
    pub text: String,
    /// Severity.
    pub severity: StatusSeverity,
}

/// Paint the footer row: hints that fit on the left, the status message on the right.
pub fn paint_footer(
    hints: &[(String, String)],
    status: Option<&Status>,
    theme: &ReviewTheme,
    area: Rect,
    cx: &mut PaintCx<'_, '_>,
) {
    cx.with_style(theme.footer, |cx| {
        paint_footer_row(hints, status, theme, area, cx);
    });
}

fn paint_footer_row(
    hints: &[(String, String)],
    status: Option<&Status>,
    theme: &ReviewTheme,
    area: Rect,
    cx: &mut PaintCx<'_, '_>,
) {
    let row = LocalRect::new(area.x.into(), area.y.into(), area.width, 1);
    cx.fill(row, " ", theme.footer);
    let status_width = status
        .filter(|_| area.width >= 2)
        .map_or(0, |status| paint_status(status, theme, area, cx));
    let hint_room = area.width.saturating_sub(status_width).saturating_sub(1);
    let hints: Vec<KeyHint<'_>> = hints
        .iter()
        .map(|(key, label)| KeyHint::new(key, label))
        .collect();
    let separator = theme.icons.separator;
    let count = fitting_hints(&hints, hint_room, separator);
    if count > 0 {
        let rect = Rect::new(area.x.saturating_add(1), area.y, hint_room, 1);
        cx.paint_component(
            &KeyHintBarComponent::new("review.hints", &hints[..count])
                .policy(KeyHintBarPolicy::compact().separator(separator))
                .styles(theme.hints),
            rect,
        );
    }
}

/// Paint `status` at the right of the footer row `area`; returns the cells it took. The
/// message keeps a one-cell margin and takes at most half the row (the status bar truncates it).
fn paint_status(status: &Status, theme: &ReviewTheme, area: Rect, cx: &mut PaintCx<'_, '_>) -> u16 {
    let segments = [StatusSegment::new(&status.text).severity(status.severity)];
    let bar = StatusBarComponent::new("review.status")
        .right(&segments)
        .policy(StatusBarPolicy::compact())
        .styles(theme.status);
    let width = component_width(&bar, area.width)
        .saturating_add(1)
        .min(area.width / 2);
    cx.paint_component(&bar, Rect::new(area.right() - width, area.y, width, 1));
    width
}

/// One toast.
#[derive(Debug, Clone)]
struct Toast {
    id: String,
    title: String,
    body: Option<String>,
    severity: ToastSeverity,
    /// When the toast goes away by itself; `None` for a sticky toast, which stays until
    /// dismissed.
    expires: Option<Instant>,
    /// What the toast is, for a notice whose dismissal the caller remembers.
    tag: Option<&'static str>,
}

/// Toast notifications. Every toast has a close button; sticky ones stay until it is clicked or
/// [`Self::dismiss_all`] runs (the "Dismiss notifications" command).
#[derive(Debug, Default)]
pub struct Toasts {
    shown: Vec<Toast>,
    /// Tags of tagged toasts the user dismissed, not taken yet.
    dismissed: Vec<&'static str>,
    next: u64,
    state: Cell<ToastStackState>,
    /// Where the stack was last painted and the policy it was painted with, so clicks on its
    /// close buttons resolve against the same geometry.
    painted: Cell<Option<(Rect, ToastStackPolicy)>>,
}

impl Toasts {
    /// Show a toast that goes away by itself.
    pub fn push(
        &mut self,
        severity: ToastSeverity,
        title: impl Into<String>,
        body: Option<String>,
    ) {
        let ttl = if severity == ToastSeverity::Error {
            ERROR_TOAST_TTL
        } else {
            TOAST_TTL
        };
        self.insert(
            severity,
            title.into(),
            body,
            Some(Instant::now() + ttl),
            None,
        );
    }

    /// Show a toast that stays until it is dismissed.
    pub fn push_sticky(
        &mut self,
        severity: ToastSeverity,
        title: impl Into<String>,
        body: Option<String>,
    ) {
        self.insert(severity, title.into(), body, None, None);
    }

    /// Show a sticky toast tagged `tag`; when the user dismisses it, [`Self::take_dismissed`]
    /// says so.
    pub fn push_sticky_tagged(
        &mut self,
        severity: ToastSeverity,
        title: impl Into<String>,
        body: Option<String>,
        tag: &'static str,
    ) {
        self.insert(severity, title.into(), body, None, Some(tag));
    }

    /// Tags of tagged toasts dismissed since the last call.
    pub fn take_dismissed(&mut self) -> Vec<&'static str> {
        std::mem::take(&mut self.dismissed)
    }

    fn insert(
        &mut self,
        severity: ToastSeverity,
        title: String,
        body: Option<String>,
        expires: Option<Instant>,
        tag: Option<&'static str>,
    ) {
        self.next += 1;
        self.shown.push(Toast {
            id: format!("toast-{}", self.next),
            title,
            body,
            severity,
            expires,
            tag,
        });
        if self.shown.len() > MAX_TOASTS {
            // Make room by dropping the oldest toast that would go away anyway; sticky toasts
            // only give way to each other.
            let oldest = self
                .shown
                .iter()
                .position(|toast| toast.expires.is_some())
                .unwrap_or(0);
            self.shown.remove(oldest);
        }
    }

    /// Drop expired toasts. Returns whether any was dropped.
    pub fn expire(&mut self, now: Instant) -> bool {
        let before = self.shown.len();
        self.shown
            .retain(|toast| toast.expires.is_none_or(|expires| expires > now));
        self.shown.len() != before
    }

    /// Drop every toast, sticky ones included. Returns whether any was shown.
    pub fn dismiss_all(&mut self) -> bool {
        let any = !self.shown.is_empty();
        self.dismissed
            .extend(self.shown.iter().filter_map(|toast| toast.tag));
        self.shown.clear();
        any
    }

    /// Handle a mouse event over the painted stack: hover and press its close buttons, and drop
    /// a toast whose close button is clicked. Returns whether the event was the stack's (it was
    /// inside the stack); events elsewhere are left to the caller.
    pub fn mouse(&mut self, event: &Event) -> bool {
        let Event::Mouse(mouse) = event else {
            return false;
        };
        let Some((area, policy)) = self.painted.get() else {
            return false;
        };
        let inside = area.contains(mouse.position);
        if !inside {
            // Leaving the stack clears its hover; a press elsewhere is not the stack's.
            if matches!(mouse.kind, MouseEventKind::Move) {
                let mut state = self.state.get();
                if state.hovered_close().is_some() {
                    state = ToastStackState::default();
                    self.state.set(state);
                }
            }
            return false;
        }
        let items = self.items();
        let stack = ToastStack::new(&items).policy(policy);
        let mut state = self.state.get();
        let closed = match stack.handle_event(area, &mut state, event) {
            ToastStackOutcome::CloseRequested { id, .. } => Some(id.to_owned()),
            ToastStackOutcome::Ignored | ToastStackOutcome::Redraw => None,
        };
        self.state.set(state);
        if let Some(id) = closed {
            self.dismissed.extend(
                self.shown
                    .iter()
                    .filter(|toast| toast.id == id)
                    .filter_map(|toast| toast.tag),
            );
            self.shown.retain(|toast| toast.id != id);
        }
        true
    }

    /// The shown toasts as stack items, newest first (the painting order).
    fn items(&self) -> Vec<ToastItem<'_>> {
        self.shown
            .iter()
            .rev()
            .map(|toast| {
                let item = ToastItem::new(&toast.id, &toast.title).severity(toast.severity);
                toast.body.as_ref().map_or(item, |body| item.body(body))
            })
            .collect()
    }

    /// Titles of the shown toasts, oldest first.
    #[must_use]
    pub fn titles(&self) -> Vec<&str> {
        self.shown
            .iter()
            .map(|toast| toast.title.as_str())
            .collect()
    }

    /// Whether no toast is shown.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.shown.is_empty()
    }

    /// Paint the toasts at the bottom right of `body`, newest at the bottom.
    pub fn paint(&self, theme: &ReviewTheme, body: Rect, cx: &mut PaintCx<'_, '_>) {
        self.painted.set(None);
        if self.shown.is_empty() || body.width < 8 || body.height < 3 {
            return;
        }
        let items = self.items();
        let width = TOAST_WIDTH.min(body.width.saturating_sub(2));
        let policy = ToastStackPolicy {
            placement: ToastPlacement::Bottom,
            width,
            close_button: true,
            mouse: true,
            ..ToastStackPolicy::cards()
        };
        let stack = ToastStackComponent::new("review.toasts", &items, &self.state)
            .policy(policy)
            .styles(theme.toasts);
        let room = Size::new(width, body.height.saturating_sub(2));
        let node = stack.layout(Constraints::loose(room), &mut LayoutCx::new());
        let size = Size::new(
            u16::try_from(node.size.width).unwrap_or(u16::MAX),
            u16::try_from(node.size.height).unwrap_or(u16::MAX),
        );
        if size.width == 0 || size.height == 0 {
            return;
        }
        let rect = Rect::new(
            body.right().saturating_sub(size.width + 1),
            body.bottom().saturating_sub(size.height + 1),
            size.width,
            size.height,
        );
        cx.paint_component(&stack, rect);
        self.painted.set(Some((rect, policy)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::IconStyle;

    #[test]
    fn sticky_toasts_stay_until_dismissed_and_outlast_passing_ones() {
        let mut toasts = Toasts::default();
        toasts.push_sticky(ToastSeverity::Warning, "sticky", None);
        toasts.push(ToastSeverity::Info, "passing", None);
        assert!(toasts.expire(Instant::now() + ERROR_TOAST_TTL + Duration::from_secs(1)));
        assert_eq!(toasts.titles(), ["sticky"]);
        // A full stack drops passing toasts before the sticky one.
        for index in 0..MAX_TOASTS {
            toasts.push(ToastSeverity::Info, format!("passing {index}"), None);
        }
        assert_eq!(toasts.titles(), ["sticky", "passing 1", "passing 2"]);
        assert!(toasts.dismiss_all());
        assert!(toasts.is_empty());
        assert!(!toasts.dismiss_all());
    }

    fn model_of(sources: usize) -> HeaderModel {
        HeaderModel {
            title: "Compare fixture/base...main".to_owned(),
            sources: (0..sources)
                .map(|index| format!("source {index}"))
                .collect(),
            viewed: (3, 12),
            totals: (120, 45),
            drafts: 2,
            unsaved: false,
            mode: "unified".to_owned(),
        }
    }

    #[test]
    fn header_parts_drop_in_elision_order_and_never_overlap() {
        let theme = ReviewTheme::derive(None, IconStyle::Unicode);
        let model = model_of(3);
        let mut dropped_before: Vec<HeaderPart> = Vec::new();
        for width in (0..=220).rev() {
            let plan = plan_header(&model, width, &theme);
            // Parts fit the row and do not overlap.
            let mut end = 0;
            for (_, area) in &plan.parts {
                assert!(area.x >= end, "{width}: {plan:?}");
                assert!(area.right() <= width, "{width}: {plan:?}");
                end = area.right();
            }
            // Whatever is gone at a width is also gone at every narrower width, and parts go
            // in the declared order.
            let dropped: Vec<HeaderPart> = HEADER_ELISION_ORDER
                .iter()
                .copied()
                .filter(|part| {
                    // The `+N` chip shows in the second chip's place.
                    plan.area(*part).is_none()
                        && !(*part == HeaderPart::Chip(1)
                            && plan.area(HeaderPart::MoreChips).is_some())
                })
                .collect();
            assert!(
                dropped_before.iter().all(|part| dropped.contains(part)),
                "{width}: {dropped:?} after {dropped_before:?}"
            );
            assert_eq!(
                dropped.as_slice(),
                &HEADER_ELISION_ORDER[..dropped.len()],
                "{width}"
            );
            // The title is truncated only after every droppable part is gone.
            let title = plan.area(HeaderPart::Title).map_or(0, |area| area.width);
            if dropped.len() < HEADER_ELISION_ORDER.len() {
                assert!(title >= TITLE_KEEP.min(28), "{width}: {plan:?}");
            }
            dropped_before = dropped;
        }
        // With more than two sources, a `+N` chip stands for the rest.
        let wide = plan_header(&model, 220, &theme);
        assert!(wide.area(HeaderPart::Chip(0)).is_some());
        assert!(wide.area(HeaderPart::MoreChips).is_some());
        assert!(wide.area(HeaderPart::Chip(1)).is_none());
        let two = plan_header(&model_of(2), 220, &theme);
        assert!(two.area(HeaderPart::Chip(1)).is_some());
    }

    #[test]
    fn unsaved_drafts_are_a_warning_pill_and_saved_drafts_are_not() {
        let theme = ReviewTheme::derive(None, IconStyle::Unicode);
        let drafts_style = |unsaved: bool| {
            let model = HeaderModel {
                unsaved,
                ..model_of(1)
            };
            let plan = plan_header(&model, 160, &theme);
            let area = plan.area(HeaderPart::Drafts).expect("drafts shown");
            let screen = crate::testing::harness::paint(160, 1, |row, cx| {
                paint_header(&model, &plan, &theme, &SplitButtonState::default(), row, cx);
            });
            let text = screen.text_in(area, 0);
            (text, screen.style_at(area.x + 1, 0))
        };
        let (text, style) = drafts_style(true);
        assert!(
            text.contains("2 drafts") && text.contains("unsaved"),
            "{text}"
        );
        assert_eq!(style, theme.pill_warning);
        let (text, style) = drafts_style(false);
        assert!(!text.contains("unsaved"), "{text}");
        assert_eq!(style.fg, theme.badge_draft.fg);
    }

    #[test]
    fn hints_fit_whole() {
        let hints = [
            KeyHint::new("n", "file"),
            KeyHint::new("}", "hunk"),
            KeyHint::new("q", "quit"),
        ];
        assert_eq!(fitting_hints(&hints, 100, " · "), 3);
        // "n file · } hunk" is 15 cells; one more cell keeps a margin.
        assert_eq!(fitting_hints(&hints, 16, " · "), 2);
        assert_eq!(fitting_hints(&hints, 15, " · "), 1);
        assert_eq!(fitting_hints(&hints, 0, " · "), 0);
    }
}
