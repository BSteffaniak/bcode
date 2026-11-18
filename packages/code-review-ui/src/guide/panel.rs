//! The guide panel and the context peek.
//!
//! [`PanelModel`] is what the panel says, built from the guide state independent of size:
//! the banner, the outline rows, and the current section (its header, body, citations, and
//! findings). [`GuidePanelView`] lays it out in the pane it is given:
//!
//! * Beside the document (and tall enough), the outline sits above the current section with a
//!   rule between them; the outline scrolls to keep the current section in view. The rule is
//!   the divider of a BMUX vertical panel group, so dragging it resizes the two within their
//!   limits (the outline never taller than its rows, the section at least a few rows).
//! * Under the document, only the current section shows; its header says where it is
//!   (`3.1/9`).
//! * Narrower than [`OUTLINE_ONLY_WIDTH`], only the outline shows, numbers and titles without
//!   their trailing marks.
//!
//! Every row is composed from styled spans and fitted with BMUX text primitives (trailing
//! placement, word wrapping, truncation); nothing measures text by bytes or characters. One
//! geometry ([`GuidePanelView::geometry`]) drives painting, hit regions, and clicks, so they
//! agree, and only rows that are painted register hits.
//!
//! [`PeekView`] shows cited code outside the review read-only in the document's place.

use crate::theme::ReviewTheme;
use bmux_tui::component::{
    Component, ComponentRevision, Constraints, EventCx, LayoutCx, LayoutId, LayoutNode, LogicalSize,
};
use bmux_tui::event::{Event, EventOutcome, MouseButton, MouseEventKind};
use bmux_tui::geometry::Rect;
use bmux_tui::hit::HitRegion;
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::text::{Line, Span, TextWrap, TextWrapGeometry, TrailingAlign, TrailingPlacement};
use bmux_tui::text_width::display_width;
use bmux_tui_components::panel_group::{
    PanelDividerMode, PanelGroup, PanelGroupAxis, PanelGroupConstraints, PanelGroupOutcome,
    PanelGroupPolicy, PanelGroupResizePolicy, PanelGroupState, PanelSize,
};
use std::cell::{Cell, RefCell};

/// Narrower than this, the panel shows only the outline's numbers and titles.
pub const OUTLINE_ONLY_WIDTH: u16 = 24;
/// Shortest panel that shows the outline above the current section.
const OUTLINE_AND_SECTION_HEIGHT: u16 = 8;
/// Rows the current section keeps below the outline.
const MIN_SECTION_ROWS: u16 = 3;
/// Rows the outline takes at least, when it has them.
const MIN_OUTLINE_ROWS: u16 = 3;
/// Cells of indentation per outline level, bounded to a third of the row.
const INDENT: usize = 2;
/// Prefix of the panel's hit region ids.
pub const HIT_PREFIX: &str = "review.guide";

/// What a click on the panel asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanelClick {
    /// Select the section at this index of the guide's entries.
    Section(usize),
    /// Show this citation.
    Citation(String),
    /// The previous section.
    Previous,
    /// The next section.
    Next,
}

/// One outline row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlineRow {
    /// Index of the section in the guide's entries.
    pub entry: usize,
    /// Nesting depth.
    pub depth: u16,
    /// Fold state: `None` without children, else whether folded.
    pub folded: Option<bool>,
    /// Number and title.
    pub label: Line,
    /// Marks after the title: state glyph and citation badges.
    pub trailing: Line,
    /// Whether it is the current section.
    pub current: bool,
}

/// One logical line of the current section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetailLine {
    /// Content.
    pub line: Line,
    /// Whether it wraps (prose) or keeps one row, truncated (citations, headers).
    pub wrap: bool,
    /// What clicking it does.
    pub action: Option<PanelClick>,
    /// Markdown shown in place of `line`, rendered at the panel's width.
    pub markdown: Option<MarkdownBody>,
}

/// A section body as markdown, its citation tokens written as their badges' text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownBody {
    /// Markdown source, each `[[c7]]` token replaced with its badge's text (escaped).
    pub source: String,
    /// Each badge's text and the styled spans it is shown with.
    pub badges: Vec<(String, Vec<Span>)>,
}

impl MarkdownBody {
    /// The body rendered in `width` cells, its badges styled.
    fn rows(&self, width: usize) -> Vec<Line> {
        crate::rows::markdown_all_rows(&self.source, width, false)
            .into_iter()
            .map(|line| with_badges(line, &self.badges))
            .collect()
    }
}

/// `line` with each occurrence of a badge's text shown with the badge's spans.
fn with_badges(line: Line, badges: &[(String, Vec<Span>)]) -> Line {
    if badges.is_empty() {
        return line;
    }
    let mut spans = Vec::with_capacity(line.spans.len());
    for span in line.spans {
        let mut rest = span.content.as_str();
        loop {
            let found = badges
                .iter()
                .filter_map(|(text, styled)| rest.find(text.as_str()).map(|at| (at, text, styled)))
                .min_by_key(|(at, text, _)| (*at, std::cmp::Reverse(text.len())));
            let Some((at, text, styled)) = found else {
                if !rest.is_empty() {
                    spans.push(Span::styled(rest.to_owned(), span.style));
                }
                break;
            };
            if at > 0 {
                spans.push(Span::styled(rest[..at].to_owned(), span.style));
            }
            spans.extend(styled.iter().cloned());
            rest = &rest[at + text.len()..];
        }
    }
    Line::from_spans(spans)
}

/// The current section's header.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SectionHeader {
    /// Number and title.
    pub title: Line,
    /// Where it is (`3 of 9`, or `3.1/9` under the document).
    pub position: String,
    /// The reviewed glyph.
    pub reviewed: Line,
    /// Whether a previous section exists.
    pub previous: bool,
    /// Whether a next section exists.
    pub next: bool,
}

/// What the panel says.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PanelModel {
    /// A line saying where the guide stands, when it is not simply complete.
    pub banner: Option<Line>,
    /// Fan-out progress under the banner, each row's text and right-aligned trailing text
    /// (bounded; display only).
    pub progress: Vec<(Line, Line)>,
    /// Outline rows shown (folded sections' children are left out).
    pub outline: Vec<OutlineRow>,
    /// The current section's header.
    pub header: SectionHeader,
    /// The current section's lines.
    pub detail: Vec<DetailLine>,
    /// Changes whenever the model changes.
    pub revision: u64,
}

impl PanelModel {
    /// Row of the current section in [`Self::outline`].
    #[must_use]
    pub fn current_row(&self) -> Option<usize> {
        self.outline.iter().position(|row| row.current)
    }
}

/// Where each part of the panel is, in panel rows.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Geometry {
    /// The banner's row.
    banner: Option<u16>,
    /// First row and count of the fan-out progress rows shown.
    progress: Option<(u16, u16)>,
    /// First row and height of the outline, and its first outline row shown.
    outline: Option<(u16, u16, usize)>,
    /// Whether the outline shows its trailing marks.
    trailing: bool,
    /// The rule's row.
    rule: Option<u16>,
    /// First row and height of the current section (header first).
    section: Option<(u16, u16)>,
}

/// The guide panel's content component.
pub struct GuidePanelView<'a> {
    id: LayoutId,
    model: &'a PanelModel,
    theme: &'a ReviewTheme,
    /// Whether the panel sits beside the document (the outline can show).
    beside: bool,
    /// Rows the current section is scrolled by.
    scroll: &'a Cell<u16>,
    clicks: &'a RefCell<Vec<PanelClick>>,
    /// The split between the outline and the current section.
    split: &'a RefCell<PanelGroupState>,
}

/// The split between the outline and the current section before it is dragged: two fifths for
/// the outline.
#[must_use]
pub fn split_state() -> PanelGroupState {
    PanelGroupState::new([PanelSize::Flex(2), PanelSize::Flex(3)])
}

/// The split with the outline at `share` (parts of `scale`) of the rows it shares with the
/// current section, as restored from presentation state.
#[must_use]
pub fn split_state_at(share: u16, scale: u16) -> PanelGroupState {
    let share = share.min(scale);
    PanelGroupState::new([
        PanelSize::Flex(share.max(1)),
        PanelSize::Flex((scale - share).max(1)),
    ])
}

/// The outline's share (parts of `scale`) of the rows it shares with the current section, once
/// the split has been dragged; `None` while it has its first proportions.
#[must_use]
pub fn split_share(state: &PanelGroupState, scale: u16) -> Option<u16> {
    if state.sizes() == split_state().sizes() {
        return None;
    }
    match state.sizes() {
        [PanelSize::Flex(outline), PanelSize::Flex(section)] => {
            let (outline, section) = (u32::from((*outline).max(1)), u32::from((*section).max(1)));
            let total = outline + section;
            u16::try_from((outline * u32::from(scale) + total / 2) / total).ok()
        }
        _ => None,
    }
}

/// The vertical group of the outline and the current section, its one-row divider the rule
/// between them.
const fn split_group() -> PanelGroup {
    PanelGroup::new(PanelGroupAxis::Vertical)
        .policy(PanelGroupPolicy {
            resize: PanelGroupResizePolicy::live().preserving_flex(true),
            ..PanelGroupPolicy::resize_only()
        })
        .divider_mode(PanelDividerMode::Gap)
}

impl<'a> GuidePanelView<'a> {
    /// The panel showing `model`, beside the document or under it; clicks are collected into
    /// `clicks`, and the current section scrolls by `scroll` rows.
    #[must_use]
    pub fn new(
        id: impl Into<LayoutId>,
        model: &'a PanelModel,
        theme: &'a ReviewTheme,
        beside: bool,
        scroll: &'a Cell<u16>,
        clicks: &'a RefCell<Vec<PanelClick>>,
        split: &'a RefCell<PanelGroupState>,
    ) -> Self {
        Self {
            id: id.into(),
            model,
            theme,
            beside,
            scroll,
            clicks,
            split,
        }
    }

    /// The rows below the banner the outline and the current section share, when both show,
    /// with the split's limits set for this outline.
    /// Rows of the banner and of the fan-out progress shown in `height` rows: progress rows
    /// only while [`MIN_SECTION_ROWS`] rows stay for the rest, dropped from the bottom first.
    fn top_rows(&self, height: u16) -> (u16, u16) {
        let banner = u16::from(self.model.banner.is_some() && height > 1);
        let room = height.saturating_sub(banner + MIN_SECTION_ROWS);
        let progress = u16::try_from(self.model.progress.len())
            .unwrap_or(u16::MAX)
            .min(room);
        (banner, progress)
    }

    fn split_area(&self, width: u16, height: u16) -> Option<Rect> {
        let (banner, progress) = self.top_rows(height);
        let top = banner + progress;
        let rows = height.saturating_sub(top);
        let outline_rows = u16::try_from(self.model.outline.len()).unwrap_or(u16::MAX);
        let combined = width >= OUTLINE_ONLY_WIDTH
            && self.beside
            && outline_rows > 0
            && rows >= OUTLINE_AND_SECTION_HEIGHT;
        if !combined {
            return None;
        }
        self.split.borrow_mut().set_constraints([
            PanelGroupConstraints::new(MIN_OUTLINE_ROWS.min(outline_rows), Some(outline_rows)),
            PanelGroupConstraints::new(MIN_SECTION_ROWS, None),
        ]);
        Some(Rect::new(0, top, width, rows))
    }

    fn geometry(&self, width: u16, height: u16) -> Geometry {
        let (banner_rows, progress_rows) = self.top_rows(height);
        let top = banner_rows + progress_rows;
        let banner = (banner_rows == 1).then_some(0);
        let progress = (progress_rows > 0).then_some((banner_rows, progress_rows));
        let rows = height.saturating_sub(top);
        let outline_only = width < OUTLINE_ONLY_WIDTH;
        let window = |outline_height: u16| {
            let current = self.model.current_row().unwrap_or(0);
            let visible = usize::from(outline_height);
            let max_first = self.model.outline.len().saturating_sub(visible);
            current.saturating_sub(visible / 2).min(max_first)
        };
        let (outline, rule, section) = if outline_only {
            ((rows > 0).then(|| (top, rows, window(rows))), None, None)
        } else if let Some(area) = self.split_area(width, height) {
            let layout = split_group().layout(area, &self.split.borrow());
            match (layout.panels.as_slice(), layout.dividers.first()) {
                ([outline, section], Some(divider)) => (
                    (outline.height > 0)
                        .then(|| (outline.y, outline.height, window(outline.height))),
                    Some(divider.y),
                    Some((section.y, section.height)),
                ),
                _ => (None, None, Some((top, rows))),
            }
        } else {
            (None, None, Some((top, rows)))
        };
        Geometry {
            banner,
            progress,
            outline,
            trailing: !outline_only,
            rule,
            section: section.filter(|(_, rows)| *rows > 0),
        }
    }

    /// The outline row `row` laid out in `width` cells.
    fn outline_line(&self, row: &OutlineRow, width: usize, trailing: bool) -> Line {
        let theme = self.theme;
        let bar = if row.current {
            Span::styled(theme.icons.selection_bar, theme.selection_bar)
        } else {
            Span::raw(" ".repeat(display_width(theme.icons.selection_bar)))
        };
        let indent = (usize::from(row.depth) * INDENT).min(width / 3);
        let fold = match row.folded {
            Some(true) => theme.icons.collapsed,
            Some(false) => theme.icons.menu,
            None => " ",
        };
        let mut spans = vec![
            bar,
            Span::raw(" ".repeat(indent)),
            Span::styled(fold, theme.muted),
            Span::raw(" "),
        ];
        spans.extend(row.label.spans.iter().cloned());
        let content = Line::from_spans(spans);
        let line = if trailing {
            fit(&content, &row.trailing, width)
        } else {
            fit(&content, &Line::default(), width)
        };
        if row.current {
            line.patch_style(theme.title)
        } else {
            line
        }
    }

    /// The section's header row in `width` cells, and the cells of its previous and next
    /// controls when they are painted whole.
    fn header_line(
        &self,
        width: usize,
    ) -> (
        Line,
        Option<std::ops::Range<u16>>,
        Option<std::ops::Range<u16>>,
    ) {
        let theme = self.theme;
        let header = &self.model.header;
        let style = |enabled: bool| if enabled { theme.accent } else { theme.muted };
        let previous = Span::styled(theme.icons.step_back, style(header.previous));
        let position = Span::styled(format!(" {} ", header.position), theme.muted);
        let next = Span::styled(theme.icons.step_forward, style(header.next));
        let mut trailing = vec![previous.clone(), position.clone(), next.clone()];
        if header.reviewed.width() > 0 {
            trailing.push(Span::raw(" "));
            trailing.extend(header.reviewed.spans.iter().cloned());
        }
        let trailing = Line::from_spans(trailing);
        let line = fit(&header.title, &trailing, width);
        let trailing_width = trailing.width();
        // The trailing controls are kept whole (the title gives way) whenever they fit at all.
        if trailing_width > width {
            return (line, None, None);
        }
        let start = width - trailing_width;
        let at = |offset: usize, cells: usize| {
            let first = u16::try_from(start + offset).ok()?;
            let end = u16::try_from(start + offset + cells).ok()?;
            Some(first..end)
        };
        let previous_cells = display_width(&previous.content);
        let next_offset = previous_cells + display_width(&position.content);
        (
            line,
            at(0, previous_cells).filter(|_| header.previous),
            at(next_offset, display_width(&next.content)).filter(|_| header.next),
        )
    }

    /// The current section's rows below its header at `width`: wrapped prose, one row per
    /// citation and header line.
    fn section_rows(&self, width: usize) -> Vec<(Line, Option<PanelClick>)> {
        let mut rows = Vec::new();
        for detail in &self.model.detail {
            if let Some(markdown) = &detail.markdown {
                if width > 0 {
                    rows.extend(
                        markdown
                            .rows(width)
                            .into_iter()
                            .map(|row| (row, detail.action.clone())),
                    );
                }
            } else if detail.wrap && detail.line.width() > width && width > 0 {
                for row in bmux_tui::text::wrap_line_bounded(
                    &detail.line,
                    TextWrapGeometry::with_continuation(width, width),
                    TextWrap::Word,
                ) {
                    rows.push((row, detail.action.clone()));
                }
            } else {
                rows.push((
                    fit(&detail.line, &Line::default(), width),
                    detail.action.clone(),
                ));
            }
        }
        rows
    }

    /// The scroll of the current section, clamped so its last row stays at the bottom.
    fn clamped_scroll(&self, rows: usize, height: u16) -> usize {
        let max = rows.saturating_sub(usize::from(height));
        usize::from(self.scroll.get()).min(max)
    }

    /// What is at panel cell `(x, y)` in a `width` x `height` panel.
    fn click_at(&self, x: u16, y: u16, width: u16, height: u16) -> Option<PanelClick> {
        let geometry = self.geometry(width, height);
        if let Some((top, rows, first)) = geometry.outline
            && (top..top + rows).contains(&y)
        {
            let row = self.model.outline.get(first + usize::from(y - top))?;
            return Some(PanelClick::Section(row.entry));
        }
        let (top, rows) = geometry.section?;
        if !(top..top + rows).contains(&y) {
            return None;
        }
        if y == top {
            let (_, previous, next) = self.header_line(usize::from(width));
            if previous.is_some_and(|cells| cells.contains(&x)) {
                return Some(PanelClick::Previous);
            }
            if next.is_some_and(|cells| cells.contains(&x)) {
                return Some(PanelClick::Next);
            }
            return None;
        }
        let body = self.section_rows(usize::from(width));
        let scroll = self.clamped_scroll(body.len(), rows.saturating_sub(1));
        body.get(scroll + usize::from(y - top - 1))
            .and_then(|(_, action)| action.clone())
    }

    fn hit(&self, cx: &mut PaintCx<'_, '_>, name: &str, rect: Rect) {
        if rect.width > 0 && rect.height > 0 {
            cx.push_hit(HitRegion::new(
                format!("{HIT_PREFIX}.{}.{name}", self.id.as_str()),
                rect,
            ));
        }
    }
}

/// `content` with `trailing` against the right edge of `width` cells (content shortened first),
/// padded to exactly `width` cells.
pub(crate) fn fit(content: &Line, trailing: &Line, width: usize) -> Line {
    let placed = content.with_trailing(
        trailing,
        width,
        TrailingPlacement::new(TrailingAlign::Right).keep_visible(true),
    );
    let placed = placed.truncate(width);
    let pad = width.saturating_sub(placed.width());
    let mut spans = placed.spans;
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    }
    Line::from_spans(spans)
}

impl Component for GuidePanelView<'_> {
    fn revision(&self) -> ComponentRevision {
        ComponentRevision::new(
            self.model.revision,
            self.model
                .revision
                .wrapping_mul(31)
                .wrapping_add(u64::from(self.scroll.get()))
                .wrapping_mul(2)
                .wrapping_add(u64::from(self.beside)),
        )
    }

    fn layout(&self, constraints: Constraints, _cx: &mut LayoutCx) -> LayoutNode {
        LayoutNode::leaf(
            self.id.clone(),
            constraints.constrain(LogicalSize::new(
                constraints.max_width(),
                constraints.max_height().unwrap_or(0),
            )),
        )
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let width = u16::try_from(layout.size.width).unwrap_or(u16::MAX);
        let height = u16::try_from(layout.size.height).unwrap_or(u16::MAX);
        if width == 0 || height == 0 {
            return;
        }
        let cells = usize::from(width);
        let theme = self.theme;
        let row_rect = |y: u16| LocalRect::new(0, i64::from(y), width, 1);
        let geometry = self.geometry(width, height);
        if let (Some(y), Some(banner)) = (geometry.banner, &self.model.banner) {
            cx.write_line(row_rect(y), &fit(banner, &Line::default(), cells));
        }
        if let Some((top, rows)) = geometry.progress {
            for (offset, (line, trailing)) in self
                .model
                .progress
                .iter()
                .take(usize::from(rows))
                .enumerate()
            {
                let y = top + u16::try_from(offset).unwrap_or(u16::MAX);
                cx.write_line(row_rect(y), &fit(line, trailing, cells));
            }
        }
        if let Some((top, rows, first)) = geometry.outline {
            for (offset, row) in self
                .model
                .outline
                .iter()
                .skip(first)
                .take(usize::from(rows))
                .enumerate()
            {
                let y = top + u16::try_from(offset).unwrap_or(u16::MAX);
                cx.write_line(
                    row_rect(y),
                    &self.outline_line(row, cells, geometry.trailing),
                );
                self.hit(cx, &format!("row.{}", row.entry), Rect::new(0, y, width, 1));
            }
        }
        if let Some(y) = geometry.rule {
            let rule = Line::from_spans(vec![Span::styled(
                theme.icons.file_rule.repeat(cells),
                theme.border,
            )]);
            cx.write_line(row_rect(y), &rule.truncate(cells));
        }
        if let Some((top, rows)) = geometry.section {
            let (header, previous, next) = self.header_line(cells);
            cx.write_line(row_rect(top), &header);
            for (name, range) in [("previous", previous), ("next", next)] {
                if let Some(range) = range {
                    self.hit(
                        cx,
                        name,
                        Rect::new(range.start, top, range.end - range.start, 1),
                    );
                }
            }
            let body_rows = rows.saturating_sub(1);
            let body = self.section_rows(cells);
            let scroll = self.clamped_scroll(body.len(), body_rows);
            for (offset, (line, action)) in body
                .iter()
                .skip(scroll)
                .take(usize::from(body_rows))
                .enumerate()
            {
                let y = top + 1 + u16::try_from(offset).unwrap_or(u16::MAX);
                cx.write_line(row_rect(y), line);
                if let Some(PanelClick::Citation(id)) = action {
                    self.hit(cx, &format!("cite.{id}"), Rect::new(0, y, width, 1));
                }
            }
        }
        cx.push_damage(LocalRect::new(0, 0, width, height));
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        let Event::Mouse(mouse) = event else {
            return EventOutcome::Ignored;
        };
        let (x, y) = cx.local_point(mouse.position);
        let width = u16::try_from(layout.size.width).unwrap_or(u16::MAX);
        let height = u16::try_from(layout.size.height).unwrap_or(u16::MAX);
        // The rule between the outline and the section is the split's divider: a press on it
        // and the drag that follows (wherever the pointer goes) resize the two.
        if let Some(area) = self.split_area(width, height) {
            let local = bmux_tui::event::MouseEvent {
                position: bmux_tui::geometry::Point {
                    x: u16::try_from(x.max(0)).unwrap_or(u16::MAX),
                    y: u16::try_from(y.max(0)).unwrap_or(u16::MAX),
                },
                ..*mouse
            };
            let outcome = split_group().handle_event(
                area,
                &mut self.split.borrow_mut(),
                &Event::Mouse(local),
            );
            match outcome {
                PanelGroupOutcome::DividerDragStarted { .. }
                | PanelGroupOutcome::Resized { .. }
                | PanelGroupOutcome::DividerDragEnded { .. } => return EventOutcome::Handled,
                PanelGroupOutcome::Redraw if mouse.kind != MouseEventKind::Move => {
                    return EventOutcome::Handled;
                }
                _ => {}
            }
        }
        let (Ok(x), Ok(y)) = (u16::try_from(x), u16::try_from(y)) else {
            return EventOutcome::Ignored;
        };
        if x >= width || y >= height {
            return EventOutcome::Ignored;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(click) = self.click_at(x, y, width, height) {
                    self.clicks.borrow_mut().push(click);
                    return EventOutcome::Handled;
                }
                EventOutcome::Ignored
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let geometry = self.geometry(width, height);
                let Some((top, rows)) = geometry.section else {
                    return EventOutcome::Ignored;
                };
                if y < top {
                    return EventOutcome::Ignored;
                }
                let body = self.section_rows(usize::from(width)).len();
                let body_rows = rows.saturating_sub(1);
                let current = self.clamped_scroll(body, body_rows);
                let max = body.saturating_sub(usize::from(body_rows));
                let next = if mouse.kind == MouseEventKind::ScrollDown {
                    (current + 3).min(max)
                } else {
                    current.saturating_sub(3)
                };
                self.scroll.set(u16::try_from(next).unwrap_or(u16::MAX));
                if next == current {
                    EventOutcome::Ignored
                } else {
                    EventOutcome::Handled
                }
            }
            _ => EventOutcome::Ignored,
        }
    }
}

/// Cited code outside the review, as the context peek shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peek {
    /// The citation shown.
    pub citation: String,
    /// What the peek's title says (path, lines, and how it relates to the review).
    pub title: String,
    /// The cited lines' range.
    pub lines: (u32, u32),
    /// The file's lines once read; `Err` says why it could not be.
    pub content: Option<Result<Vec<String>, String>>,
    /// Rows scrolled by.
    pub scroll: u32,
}

impl Peek {
    /// First line to show for a peek `rows` high: the cited range a little below the top.
    #[must_use]
    pub fn first_line(&self, rows: u16) -> u32 {
        let context = u32::from(rows) / 4;
        let base = self.lines.0.saturating_sub(context).max(1);
        base.saturating_add(self.scroll)
    }
}

/// The peek's content component: line numbers and text, the cited lines tinted.
pub struct PeekView<'a> {
    id: LayoutId,
    peek: &'a Peek,
    theme: &'a ReviewTheme,
}

impl<'a> PeekView<'a> {
    /// The view of `peek`.
    #[must_use]
    pub fn new(id: impl Into<LayoutId>, peek: &'a Peek, theme: &'a ReviewTheme) -> Self {
        Self {
            id: id.into(),
            peek,
            theme,
        }
    }
}

impl Component for PeekView<'_> {
    fn revision(&self) -> ComponentRevision {
        let loaded = match &self.peek.content {
            None => 0,
            Some(Ok(lines)) => lines.len() as u64 + 1,
            Some(Err(_)) => u64::MAX,
        };
        ComponentRevision::new(loaded, u64::from(self.peek.scroll))
    }

    fn layout(&self, constraints: Constraints, _cx: &mut LayoutCx) -> LayoutNode {
        LayoutNode::leaf(
            self.id.clone(),
            constraints.constrain(LogicalSize::new(
                constraints.max_width(),
                constraints.max_height().unwrap_or(0),
            )),
        )
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let width = u16::try_from(layout.size.width).unwrap_or(u16::MAX);
        let height = u16::try_from(layout.size.height).unwrap_or(u16::MAX);
        if width == 0 || height == 0 {
            return;
        }
        let cells = usize::from(width);
        let theme = self.theme;
        let row = |y: u16| LocalRect::new(0, i64::from(y), width, 1);
        let title = Line::from_spans(vec![
            Span::styled(theme.icons.guide_marker, theme.guide.badge),
            Span::raw(" "),
            Span::styled(self.peek.title.clone(), theme.guide.context),
        ]);
        let hint = Line::from_spans(vec![Span::styled("esc back", theme.muted)]);
        cx.write_line(row(0), &fit(&title, &hint, cells));
        let lines = match &self.peek.content {
            None => {
                let loading = Line::from_spans(vec![Span::styled("reading…", theme.muted)]);
                cx.write_line(
                    row(1.min(height - 1)),
                    &fit(&loading, &Line::default(), cells),
                );
                return;
            }
            Some(Err(error)) => {
                let error = Line::from_spans(vec![Span::styled(error.clone(), theme.error)]);
                cx.write_line(
                    row(1.min(height - 1)),
                    &fit(&error, &Line::default(), cells),
                );
                return;
            }
            Some(Ok(lines)) => lines,
        };
        let body_rows = height.saturating_sub(1);
        let first = self.peek.first_line(body_rows);
        let last = u32::try_from(lines.len()).unwrap_or(u32::MAX);
        let digits = bmux_tui_components::diff_viewer::DiffRowGutter::line_number_digits(last);
        let (start, end) = self.peek.lines;
        for offset in 0..body_rows {
            let number = first.saturating_add(u32::from(offset));
            let Some(text) = usize::try_from(number - 1)
                .ok()
                .and_then(|index| lines.get(index))
            else {
                break;
            };
            let cited = (start..=end).contains(&number);
            let gutter_style = if cited {
                theme.guide.focus
            } else {
                theme.gutter
            };
            let number_text = number.to_string();
            let pad = (digits + 1).saturating_sub(display_width(&number_text));
            let line = Line::from_spans(vec![
                Span::styled(format!("{}{number_text} ", " ".repeat(pad)), gutter_style),
                Span::raw(" "),
                Span::styled(text.replace('\t', "    "), theme.text),
            ]);
            cx.write_line(row(1 + offset), &fit(&line, &Line::default(), cells));
        }
        cx.push_damage(LocalRect::new(0, 0, width, height));
    }

    fn event(&self, _event: &Event, _layout: &LayoutNode, _cx: &mut EventCx<'_>) -> EventOutcome {
        EventOutcome::Ignored
    }
}

/// Whether the model is for the panel beside the document or under it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Beside the document: the outline and the section.
    Beside,
    /// Under the document: the section alone.
    Under,
}

/// The model of `active` as the panel shows it, with `folded` sections' children left out.
#[must_use]
pub fn build_model(
    active: &super::state::ActiveGuide,
    folded: &std::collections::BTreeSet<String>,
    theme: &ReviewTheme,
    placement: Placement,
    revision: u64,
) -> PanelModel {
    use super::state::BannerKind;
    let banner = active.banner().map(|banner| {
        let (glyph, style) = match banner.kind {
            BannerKind::Busy => (theme.icons.running, theme.accent),
            BannerKind::Note => (theme.icons.guide_marker, theme.muted),
            BannerKind::Problem => (theme.icons.alert, theme.guide.problem),
        };
        Line::from_spans(vec![
            Span::styled(glyph, style),
            Span::raw(" "),
            Span::styled(banner.text, style),
        ])
    });
    let mut outline = Vec::new();
    let mut hidden_below: Option<u16> = None;
    for (index, entry) in active.entries.iter().enumerate() {
        if let Some(depth) = hidden_below {
            if entry.depth > depth {
                continue;
            }
            hidden_below = None;
        }
        let is_folded = folded.contains(&entry.id);
        if entry.has_children && is_folded {
            hidden_below = Some(entry.depth);
        }
        outline.push(OutlineRow {
            entry: index,
            depth: entry.depth,
            folded: entry.has_children.then_some(is_folded),
            label: Line::from_spans(vec![
                Span::styled(entry.number.clone(), theme.muted),
                Span::raw(" "),
                Span::styled(entry.title.clone(), theme.text),
            ]),
            trailing: outline_trailing(active, entry, theme),
            current: index == active.selected,
        });
    }
    let total = active.entries.len();
    let header = active
        .current()
        .map_or_else(SectionHeader::default, |entry| SectionHeader {
            title: Line::from_spans(vec![
                Span::styled(entry.number.clone(), theme.muted),
                Span::raw(" "),
                Span::styled(entry.title.clone(), theme.title),
            ]),
            position: match placement {
                Placement::Beside => format!("{} of {total}", active.selected + 1),
                Placement::Under => format!("{}/{total}", entry.number),
            },
            reviewed: reviewed_glyph(active.reviewed(&entry.id), theme),
            previous: active.selected > 0,
            next: active.selected + 1 < total,
        });
    PanelModel {
        banner,
        progress: Vec::new(),
        outline,
        header,
        detail: detail_lines(active, theme),
        revision,
    }
}

/// The reviewed mark: the viewed pair, never the approval mark.
fn reviewed_glyph(reviewed: bool, theme: &ReviewTheme) -> Line {
    Line::from_spans(vec![if reviewed {
        Span::styled(theme.icons.viewed, theme.viewed)
    } else {
        Span::styled(theme.icons.unviewed, theme.muted)
    }])
}

/// A citation badge: `[n]` and its verification glyph.
#[must_use]
pub fn badge_spans(number: u16, glyph: super::marks::BadgeGlyph, theme: &ReviewTheme) -> Vec<Span> {
    use super::marks::BadgeGlyph;
    let (mark, number_style, mark_style) = match glyph {
        BadgeGlyph::Verified => (
            theme.icons.guide_verified,
            theme.guide.badge,
            theme.guide.badge,
        ),
        BadgeGlyph::Unverified => (
            theme.icons.guide_unverified,
            theme.guide.unverified,
            theme.guide.unverified,
        ),
        BadgeGlyph::Problem => (
            theme.icons.guide_stale,
            theme.guide.badge,
            theme.guide.problem,
        ),
    };
    vec![
        Span::styled(format!("[{number}]"), number_style),
        Span::styled(mark, mark_style),
    ]
}

/// Badges after an outline row's title: its own citations (the first few), then its state.
fn outline_trailing(
    active: &super::state::ActiveGuide,
    entry: &super::state::SectionEntry,
    theme: &ReviewTheme,
) -> Line {
    const SHOWN: usize = 3;
    let mut spans = Vec::new();
    for id in entry.citations.iter().take(SHOWN) {
        if let Some(number) = active.number(id) {
            spans.push(Span::styled(
                format!("[{number}]"),
                match active.citation_look(id) {
                    super::state::CitationLook::Verified => theme.guide.badge,
                    super::state::CitationLook::Unverified => theme.guide.unverified,
                    super::state::CitationLook::Problem => theme.guide.problem,
                },
            ));
        }
    }
    if entry.citations.len() > SHOWN {
        spans.push(Span::styled(
            format!("+{}", entry.citations.len() - SHOWN),
            theme.muted,
        ));
    }
    if !spans.is_empty() {
        spans.push(Span::raw(" "));
    }
    let state = if entry.state == bcode_code_review_models::SectionState::Planned {
        Span::styled(theme.icons.running, theme.muted)
    } else if active.stale.contains(&entry.id) {
        Span::styled(
            format!("{} stale", theme.icons.guide_stale),
            theme.guide.problem,
        )
    } else if active.reviewed(&entry.id) {
        Span::styled(theme.icons.viewed, theme.viewed)
    } else {
        Span::styled(theme.icons.unviewed, theme.muted)
    };
    spans.push(state);
    Line::from_spans(spans)
}

/// The current section's lines: its body with citation badges, its citations, and its
/// findings.
fn detail_lines(active: &super::state::ActiveGuide, theme: &ReviewTheme) -> Vec<DetailLine> {
    use bcode_code_review_models::SectionState;
    let mut lines = Vec::new();
    let Some(entry) = active.current() else {
        lines.push(DetailLine {
            line: Line::from_spans(vec![Span::styled(
                "The guide has no sections yet.",
                theme.muted,
            )]),
            wrap: true,
            action: None,
            markdown: None,
        });
        return lines;
    };
    let Some(section) = active.section(&entry.id) else {
        return lines;
    };
    let prose = |line: Line| DetailLine {
        line,
        wrap: true,
        action: None,
        markdown: None,
    };
    if section.state == SectionState::Planned {
        lines.push(prose(Line::from_spans(vec![
            Span::styled(theme.icons.running, theme.muted),
            Span::styled(" not written yet", theme.muted),
        ])));
    }
    if !section.body.trim().is_empty() {
        lines.push(DetailLine {
            markdown: Some(markdown_body(active, &section.body, theme)),
            ..prose(Line::default())
        });
    }
    let cited = &entry.citations;
    if !cited.is_empty() {
        lines.push(prose(Line::default()));
    }
    for id in cited {
        if let Some(line) = citation_line(active, id, theme) {
            lines.push(line);
        }
    }
    if !section.findings.is_empty() {
        lines.push(prose(Line::default()));
    }
    for finding in &section.findings {
        lines.push(prose(finding_line(active, finding, theme)));
    }
    if !section.checks.is_empty() {
        lines.push(prose(Line::default()));
        lines.push(prose(Line::from_spans(vec![Span::styled(
            "Checked claims",
            theme.muted,
        )])));
    }
    for check in &section.checks {
        lines.push(prose(check_line(check, theme)));
    }
    lines
}

/// One claim check a verifier recorded: supported (the verified glyph), unsupported (the
/// problem glyph) or unclear (the dimmed unverified glyph), the claim, and the note. Checks
/// never stand in for the section's content.
fn check_line(check: &bcode_code_review_models::ClaimCheck, theme: &ReviewTheme) -> Line {
    use bcode_code_review_models::ClaimStatus;
    let (glyph, style, word) = match check.status {
        ClaimStatus::Supported => (theme.icons.guide_verified, theme.guide.badge, "supported"),
        ClaimStatus::Unsupported => (theme.icons.alert, theme.guide.problem, "unsupported"),
        ClaimStatus::Unclear => (theme.icons.guide_unverified, theme.tiers.dim, "unclear"),
    };
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(glyph, style),
        Span::raw(" "),
        Span::styled(check.claim.clone(), theme.text),
        Span::styled(format!(" · {word}"), style),
    ];
    if !check.note.trim().is_empty() {
        spans.push(Span::styled(
            format!(" · {}", check.note.trim()),
            theme.muted,
        ));
    }
    Line::from_spans(spans)
}

/// One row of the current section's citations: its badge, where it is, and how it relates to
/// the review; clicking it shows it.
fn citation_line(
    active: &super::state::ActiveGuide,
    id: &str,
    theme: &ReviewTheme,
) -> Option<DetailLine> {
    use bcode_code_review_models::{CitationRelation, Verification};
    let citation = active.citation(id)?;
    let target = active.target(id);
    let lines_shown = target
        .as_ref()
        .map_or(citation.lines, |target| target.lines);
    let number = active.number(id).unwrap_or(0);
    let mut spans = vec![Span::raw(" ")];
    spans.extend(badge_spans(number, active.citation_look(id).into(), theme));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        format!(
            "{}:{}-{}",
            citation.path, lines_shown.start, lines_shown.end
        ),
        theme.path,
    ));
    let relation = match citation.relation {
        CitationRelation::Changed => "changed",
        CitationRelation::HunkContext => "context",
        CitationRelation::Unchanged => "unchanged",
        CitationRelation::InReview => "in review",
        CitationRelation::OutsideReview => "unchanged · outside review",
    };
    spans.push(Span::raw("  "));
    spans.push(Span::styled(
        relation,
        if citation.relation == CitationRelation::Changed {
            theme.muted
        } else {
            theme.guide.context
        },
    ));
    if active
        .freshness(id)
        .is_some_and(bcode_code_review_models::Freshness::is_stale)
    {
        spans.push(Span::styled(
            format!(" {} changed since written", theme.icons.guide_stale),
            theme.guide.problem,
        ));
    } else if let Verification::Problem { message, .. } = &citation.verification {
        spans.push(Span::styled(
            format!(" {} {message}", theme.icons.guide_stale),
            theme.guide.problem,
        ));
    }
    Some(DetailLine {
        line: Line::from_spans(spans),
        wrap: false,
        action: Some(PanelClick::Citation(id.to_owned())),
        markdown: None,
    })
}

/// One finding: its kind, severity, text, and citation badges.
fn finding_line(
    active: &super::state::ActiveGuide,
    finding: &bcode_code_review_models::Finding,
    theme: &ReviewTheme,
) -> Line {
    use bcode_code_review_models::{FindingKind, ReviewThreadSeverity};
    let (glyph, label, style) = match finding.kind {
        FindingKind::Risk => (theme.icons.alert, "Risk", theme.guide.problem),
        FindingKind::Question => ("?", "Question", theme.accent),
        FindingKind::Suggestion => ("→", "Suggestion", theme.accent),
        FindingKind::Note => ("•", "Note", theme.muted),
    };
    let severity = finding.severity.map_or_else(String::new, |severity| {
        let name = match severity {
            ReviewThreadSeverity::Info => "info",
            ReviewThreadSeverity::Nit => "nit",
            ReviewThreadSeverity::Warning => "warning",
            ReviewThreadSeverity::Blocker => "blocker",
        };
        format!(" ({name})")
    });
    let mut spans = vec![
        Span::styled(format!("{glyph} {label}"), style),
        Span::styled(severity, theme.muted),
        Span::raw(": "),
        Span::styled(finding.text.clone(), theme.text),
    ];
    for id in &finding.citations {
        if let Some(number) = active.number(id) {
            spans.push(Span::raw(" "));
            spans.extend(badge_spans(number, active.citation_look(id).into(), theme));
        }
    }
    if finding.drafted_thread_id.is_some() {
        spans.push(Span::styled("  drafted → thread", theme.muted));
    }
    Line::from_spans(spans)
}

/// A section body as markdown, its `[[c7]]` tokens written as their badges' text so they are
/// measured and wrapped with the prose, and shown with the badges' styles.
fn markdown_body(
    active: &super::state::ActiveGuide,
    body: &str,
    theme: &ReviewTheme,
) -> MarkdownBody {
    let mut source = String::with_capacity(body.len());
    let mut badges: Vec<(String, Vec<Span>)> = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find("[[") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("]]") else {
            break;
        };
        source.push_str(&rest[..start]);
        let token = after[..end].trim();
        let spans = active.number(token).map_or_else(
            || vec![Span::styled(format!("[[{token}]]"), theme.muted)],
            |number| badge_spans(number, active.citation_look(token).into(), theme),
        );
        let text: String = spans.iter().map(|span| span.content.as_str()).collect();
        for character in text.chars() {
            if character.is_ascii_punctuation() {
                source.push('\\');
            }
            source.push(character);
        }
        if !badges.iter().any(|(known, _)| *known == text) {
            badges.push((text, spans));
        }
        rest = &after[end + 2..];
    }
    source.push_str(rest);
    MarkdownBody { source, badges }
}
