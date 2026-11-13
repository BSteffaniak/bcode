//! The frame of a thread card (see `docs/review-thread-cards.md`).
//!
//! Compact (the default): the header rides in the top border and the buttons in the bottom
//! one, so the frame costs no rows. Roomy: plain borders, the header and the buttons on rows of
//! their own. Folded: one row, the header in a rule. Under 40 columns the sides go, and the
//! ASCII icon set draws the frame in `+ - |`.
//!
//! ```text
//! ╭─ ▾ 💬 ana · 3d ago · 2 comments ─ waiting on you ─────────────── ○ Viewed ─╮
//! │ ana · 3d ago                                                                │
//! │ this one is more blocking …                                                 │
//! ├─────────────────────────────────────────────────────────────────────────────┤
//! │ you · 1d ago                                                        • new   │
//! ╰─ Reply · Resolve · Open ↗ ──────────────────────────────── outdated · L12 ─╯
//! ```

use super::{ActionCells, ItemAction, ItemRows};
use crate::config::conversation::ThreadCardMode;
use crate::theme::ReviewTheme;
use bmux_tui::style::Style;
use bmux_tui::text::{Line, Span};
use bmux_tui::text_width::display_width;

/// Key prefix of AI answer cards in view marks and folds.
pub const AI_PREFIX: &str = "ai:";

/// The card key of an AI answer item (by its exchange or suggestion id).
#[must_use]
pub fn ai_key(id: &str) -> String {
    format!("{AI_PREFIX}{id}")
}

/// Least width that keeps the card's sides.
const MIN_FRAMED: usize = 40;

/// A run of the body: its lines (at most the card's inner width) and their buttons, with rows
/// from the run's first line and cells from the inner start.
#[derive(Debug, Default)]
pub struct Section {
    /// Lines.
    pub lines: Vec<Line>,
    /// Buttons.
    pub actions: Vec<ActionCells<ItemAction>>,
}

/// What a card shows.
pub struct Card<'a> {
    /// Header chips, most important first; the first is never dropped (it is truncated).
    pub title: Vec<Vec<Span>>,
    /// Right end of the header: the viewed checkbox.
    pub viewed: Option<Span>,
    /// Body runs, separated by dividers.
    pub sections: Vec<Section>,
    /// Buttons.
    pub buttons: Vec<(&'a str, ItemAction)>,
    /// Right end of the footer (the line reference, outdated).
    pub footer: Vec<Span>,
    /// Frame style (by the thread's state).
    pub border: Style,
    /// Shown folded: the header only.
    pub folded: bool,
}

/// Where and how a card is laid out.
#[derive(Debug, Clone, Copy)]
pub struct Layout {
    /// Compact or roomy.
    pub mode: ThreadCardMode,
    /// Cells before the card.
    pub indent: usize,
    /// Cells of the item.
    pub width: usize,
    /// Widest the card gets (0 for the whole width).
    pub max_width: usize,
}

/// Thread card settings (`thread_card`, `thread_width`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Params {
    /// Compact or roomy.
    pub mode: ThreadCardMode,
    /// Widest a card gets (0 for the whole width).
    pub max_width: u16,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            mode: ThreadCardMode::Compact,
            max_width: 110,
        }
    }
}

impl Params {
    /// The layout of a card `indent` cells in, in an item `width` cells wide.
    #[must_use]
    pub fn layout(self, indent: usize, width: usize) -> Layout {
        Layout {
            mode: self.mode,
            indent,
            width,
            max_width: usize::from(self.max_width),
        }
    }
}

impl Layout {
    /// The card's width.
    #[must_use]
    pub fn card_width(self) -> usize {
        let available = self.width.saturating_sub(self.indent);
        if self.max_width == 0 {
            available
        } else {
            available.min(self.max_width)
        }
    }

    /// Cells inside the card's sides and padding: what comments wrap at.
    #[must_use]
    pub fn inner(self) -> usize {
        self.card_width().saturating_sub(4).max(1)
    }
}

/// Frame glyphs.
#[derive(Debug, Clone, Copy)]
struct Glyphs {
    top: [&'static str; 2],
    bottom: [&'static str; 2],
    tee: [&'static str; 2],
    ends: [&'static str; 2],
    side: &'static str,
    rule: &'static str,
}

impl Glyphs {
    const UNICODE: Self = Self {
        top: ["╭", "╮"],
        bottom: ["╰", "╯"],
        tee: ["├", "┤"],
        ends: ["╶", "╴"],
        side: "│",
        rule: "─",
    };
    const ASCII: Self = Self {
        top: ["+", "+"],
        bottom: ["+", "+"],
        tee: ["+", "+"],
        ends: ["-", "-"],
        side: "|",
        rule: "-",
    };
    /// No sides: rules only.
    const fn narrow(self) -> Self {
        Self {
            top: [self.rule, self.rule],
            bottom: [self.rule, self.rule],
            tee: [self.rule, self.rule],
            ends: [self.rule, self.rule],
            side: " ",
            rule: self.rule,
        }
    }
}

fn cells(x: usize) -> u16 {
    u16::try_from(x).unwrap_or(u16::MAX)
}

fn width_of(spans: &[Span]) -> usize {
    spans.iter().map(|span| display_width(&span.content)).sum()
}

impl Card<'_> {
    /// The card's rows.
    #[must_use]
    pub fn rows(self, layout: Layout, theme: &ReviewTheme) -> ItemRows {
        let indent = layout.indent.min(layout.width);
        let width = layout.card_width();
        let ascii = theme.icons.accent_bar == "|";
        let base = if ascii {
            Glyphs::ASCII
        } else {
            Glyphs::UNICODE
        };
        let glyphs = if width < MIN_FRAMED {
            base.narrow()
        } else {
            base
        };
        let mut out = Out {
            indent,
            width,
            border: self.border,
            glyphs,
            lines: Vec::new(),
            actions: Vec::new(),
            theme,
        };
        if self.folded {
            out.header_rule(&self.title, self.viewed, glyphs.ends);
            return out.finish();
        }
        match layout.mode {
            ThreadCardMode::Compact => {
                out.header_rule(&self.title, self.viewed, glyphs.top);
            }
            ThreadCardMode::Roomy => {
                out.plain_rule(glyphs.top);
                out.header_row(&self.title, self.viewed);
                out.plain_rule(glyphs.tee);
            }
        }
        for (index, section) in self.sections.into_iter().enumerate() {
            if index > 0 {
                out.plain_rule(glyphs.tee);
            }
            out.section(section);
        }
        match layout.mode {
            ThreadCardMode::Compact => out.footer_rule(&self.buttons, &self.footer),
            ThreadCardMode::Roomy => {
                if !self.buttons.is_empty() || !self.footer.is_empty() {
                    out.plain_rule(glyphs.tee);
                    out.button_rows(&self.buttons, &self.footer);
                }
                out.plain_rule(glyphs.bottom);
            }
        }
        out.finish()
    }
}

/// Rows being built.
struct Out<'a> {
    indent: usize,
    width: usize,
    border: Style,
    glyphs: Glyphs,
    lines: Vec<Line>,
    actions: Vec<ActionCells<ItemAction>>,
    theme: &'a ReviewTheme,
}

impl Out<'_> {
    fn finish(self) -> ItemRows {
        ItemRows::lines(self.lines, self.actions)
    }

    fn row(&self) -> u16 {
        cells(self.lines.len())
    }

    fn pad(&self) -> Span {
        Span::raw(" ".repeat(self.indent))
    }

    /// Cells inside the sides and their padding.
    const fn inner(&self) -> usize {
        self.width.saturating_sub(4)
    }

    /// `ends[0] ─── ends[1]`.
    fn plain_rule(&mut self, ends: [&'static str; 2]) {
        let line = Line::from_spans(vec![
            self.pad(),
            Span::styled(ends[0], self.border),
            Span::styled(
                self.glyphs.rule.repeat(self.width.saturating_sub(2)),
                self.border,
            ),
            Span::styled(ends[1], self.border),
        ]);
        self.lines.push(line);
    }

    /// Header chips that fit in `room` cells: the first always (truncated), then in order while
    /// they fit.
    fn fit(title: &[Vec<Span>], room: usize) -> Vec<Span> {
        let mut spans: Vec<Span> = Vec::new();
        let mut used = 0;
        for (index, chip) in title.iter().enumerate() {
            let chip_width = width_of(chip) + usize::from(index > 0);
            if index > 0 && used + chip_width > room {
                break;
            }
            if index > 0 {
                spans.push(Span::raw(" "));
            }
            spans.extend(chip.iter().cloned());
            used += chip_width;
        }
        Line::from_spans(spans).truncate(room).spans
    }

    /// The fold and viewed targets of a header row whose chevron starts at `start` and whose
    /// checkbox is at `check` (else none) — split so they do not overlap.
    fn header_actions(&mut self, start: usize, check: Option<(usize, usize)>, end: usize) {
        let row = self.row();
        let fold = |from: usize, to: usize, actions: &mut Vec<ActionCells<ItemAction>>| {
            if to > from {
                actions.push(ActionCells {
                    row,
                    cells: cells(from)..cells(to),
                    action: ItemAction::Thread(super::ThreadAction::Fold),
                });
            }
        };
        match check {
            Some((x, w)) => {
                fold(start, x, &mut self.actions);
                self.actions.push(ActionCells {
                    row,
                    cells: cells(x)..cells(x + w),
                    action: ItemAction::Thread(super::ThreadAction::Viewed),
                });
                fold(x + w, end, &mut self.actions);
            }
            None => fold(start, end, &mut self.actions),
        }
    }

    /// `ends[0]─ title ─── viewed ─ends[1]`: the compact top border, or a folded card.
    fn header_rule(&mut self, title: &[Vec<Span>], viewed: Option<Span>, ends: [&'static str; 2]) {
        let rule = self.glyphs.rule;
        let viewed_width = viewed
            .as_ref()
            .map_or(0, |span| display_width(&span.content) + 1);
        // `end─ ` title ` ─fill` [` viewed`] ` ─end`: 7 cells besides the title, fill, viewed.
        let fixed = 7 + viewed_width;
        let room = self.width.saturating_sub(fixed + 1);
        let mut spans = vec![
            self.pad(),
            Span::styled(ends[0], self.border),
            Span::styled(format!("{rule} "), self.border),
        ];
        let title = Self::fit(title, room);
        let title_width = width_of(&title);
        spans.extend(title);
        let fill = self.width.saturating_sub(fixed + title_width);
        spans.push(Span::styled(
            format!(" {}", rule.repeat(fill.max(1))),
            self.border,
        ));
        let mut check = None;
        if let Some(viewed) = viewed {
            let x = self.indent + 3 + title_width + 1 + fill.max(1) + 1;
            check = Some((x, display_width(&viewed.content)));
            spans.push(Span::raw(" "));
            spans.push(viewed);
        }
        spans.push(Span::styled(format!(" {rule}"), self.border));
        spans.push(Span::styled(ends[1], self.border));
        self.header_actions(self.indent, check, self.indent + self.width);
        self.lines
            .push(Line::from_spans(spans).truncate(self.indent + self.width));
    }

    /// `│ title ... viewed │`: the roomy header row.
    fn header_row(&mut self, title: &[Vec<Span>], viewed: Option<Span>) {
        let viewed_width = viewed
            .as_ref()
            .map_or(0, |span| display_width(&span.content) + 1);
        let room = self.inner().saturating_sub(viewed_width);
        let title = Self::fit(title, room);
        let gap = self.inner().saturating_sub(width_of(&title) + viewed_width);
        let mut content = title;
        content.push(Span::raw(" ".repeat(gap)));
        let mut check = None;
        if let Some(viewed) = viewed {
            check = Some((
                self.indent + 2 + self.inner() - display_width(&viewed.content),
                display_width(&viewed.content),
            ));
            content.push(Span::raw(" "));
            content.push(viewed);
        }
        self.header_actions(self.indent, check, self.indent + self.width);
        self.content_row(content);
    }

    /// `│ content │`, the content padded to the inner width.
    fn content_row(&mut self, content: Vec<Span>) {
        let inner = self.inner();
        let line = Line::from_spans(content).truncate(inner);
        let gap = inner.saturating_sub(line.width());
        let mut spans = vec![
            self.pad(),
            Span::styled(self.glyphs.side, self.border),
            Span::raw(" "),
        ];
        spans.extend(line.spans);
        spans.push(Span::raw(" ".repeat(gap + 1)));
        spans.push(Span::styled(self.glyphs.side, self.border));
        self.lines.push(Line::from_spans(spans));
    }

    fn section(&mut self, section: Section) {
        let first = self.row();
        let left = cells(self.indent + 2);
        for action in section.actions {
            self.actions.push(ActionCells {
                row: first.saturating_add(action.row),
                cells: action.cells.start.saturating_add(left)
                    ..action.cells.end.saturating_add(left),
                action: action.action,
            });
        }
        for line in section.lines {
            self.content_row(line.spans);
        }
    }

    /// Buttons with separators from `x`, as spans and their targets on row `row`.
    fn buttons(
        &self,
        buttons: &[(&str, ItemAction)],
        x: usize,
        row: u16,
    ) -> (Vec<Span>, Vec<ActionCells<ItemAction>>, usize) {
        let separator = self.theme.icons.separator;
        let mut spans = Vec::new();
        let mut actions = Vec::new();
        let mut at = x;
        for (index, (label, action)) in buttons.iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled(separator, self.theme.muted));
                at += display_width(separator);
            }
            let w = display_width(label);
            spans.push(Span::styled((*label).to_owned(), self.theme.gap_action));
            actions.push(ActionCells {
                row,
                cells: cells(at)..cells(at + w),
                action: *action,
            });
            at += w;
        }
        (spans, actions, at - x)
    }

    /// `╰─ buttons ─── footer ─╯`: the compact bottom border. Buttons that do not fit go on a
    /// row of their own above it.
    fn footer_rule(&mut self, buttons: &[(&str, ItemAction)], footer: &[Span]) {
        let rule = self.glyphs.rule;
        let ends = self.glyphs.bottom;
        let x = self.indent + 3;
        let (spans, actions, used) = self.buttons(buttons, x, self.row());
        let footer_width = width_of(footer);
        // `end─ ` buttons ` ─fill` [` footer`] ` ─end`: 7 cells besides them and the fill.
        let fixed = 7;
        let fits = used + footer_width + 1 + fixed < self.width;
        let (spans, actions, used) = if fits || buttons.is_empty() {
            (spans, actions, used)
        } else {
            // On rows of their own (wrapped), then a plain bottom.
            self.wrapped_buttons(buttons);
            (Vec::new(), Vec::new(), 0)
        };
        self.actions.extend(actions);
        let mut line = vec![
            self.pad(),
            Span::styled(ends[0], self.border),
            Span::styled(format!("{rule} "), self.border),
        ];
        line.extend(spans);
        let footer: Vec<Span> = if used + footer_width + 1 + fixed < self.width {
            footer.to_vec()
        } else {
            Vec::new()
        };
        let footer_cells = if footer.is_empty() {
            0
        } else {
            width_of(&footer) + 1
        };
        let fill = self.width.saturating_sub(fixed + used + footer_cells);
        line.push(Span::styled(
            format!(" {}", rule.repeat(fill.max(1))),
            self.border,
        ));
        if !footer.is_empty() {
            line.push(Span::raw(" "));
            line.extend(footer);
        }
        line.push(Span::styled(format!(" {rule}"), self.border));
        line.push(Span::styled(ends[1], self.border));
        self.lines
            .push(Line::from_spans(line).truncate(self.indent + self.width));
    }

    /// `│ buttons ... footer │`: the roomy button row (wrapping when needed).
    fn button_rows(&mut self, buttons: &[(&str, ItemAction)], footer: &[Span]) {
        let (spans, actions, used) = self.buttons(buttons, self.indent + 2, self.row());
        if used + width_of(footer) < self.inner() {
            self.actions.extend(actions);
            let mut content = spans;
            let gap = self.inner().saturating_sub(used + width_of(footer));
            content.push(Span::raw(" ".repeat(gap)));
            content.extend(footer.iter().cloned());
            self.content_row(content);
        } else {
            self.wrapped_buttons(buttons);
            if !footer.is_empty() {
                self.content_row(footer.to_vec());
            }
        }
    }

    /// `buttons` on content rows, wrapped to the inner width.
    fn wrapped_buttons(&mut self, buttons: &[(&str, ItemAction)]) {
        let inner = self.inner();
        let separator = display_width(self.theme.icons.separator);
        let mut row: Vec<(&str, ItemAction)> = Vec::new();
        let mut used = 0;
        for button in buttons {
            let width = display_width(button.0);
            let gap = if row.is_empty() { 0 } else { separator };
            if !row.is_empty() && used + gap + width > inner {
                let (spans, actions, _) = self.buttons(&row, self.indent + 2, self.row());
                self.actions.extend(actions);
                self.content_row(spans);
                row.clear();
                used = 0;
            }
            used += if row.is_empty() {
                width
            } else {
                separator + width
            };
            row.push(*button);
        }
        if !row.is_empty() {
            let (spans, actions, _) = self.buttons(&row, self.indent + 2, self.row());
            self.actions.extend(actions);
            self.content_row(spans);
        }
    }
}
