//! A thread card (see `docs/review-thread-cards.md`): comment threads and AI answers, drawn
//! with BMUX's row-based [`Card`](bmux_tui_components::card::Card) so they stay cached rows of
//! the review's list.
//!
//! Compact (the default): the header rides in the top border and the buttons in the bottom
//! one, so the frame costs no rows. Roomy: plain borders, the header and the buttons on rows of
//! their own. Folded: one row, the header in a rule. Under 40 columns the sides go, and the
//! ASCII icon set draws the frame in `+ - |`. The card is filled with the thread background
//! (`source.thread.background`), so it stands apart from the code around it.
//!
//! ```text
//! ╭─ ▾ 💬 ana · 3d ago · 2 comments ─ waiting on you ─────────────── ○ Viewed ─╮
//! │ ana · 3d ago                                                                │
//! │ This couples the parser to the storage layer …                              │
//! ├─────────────────────────────────────────────────────────────────────────────┤
//! │ you · 1d ago                                                        • new   │
//! ╰─ Reply · Resolve · Open ↗ ──────────────────────────────── outdated · L12 ─╯
//! ```

use super::{ActionCells, ItemAction, ItemRows, ThreadAction};
use crate::config::conversation::ThreadCardMode;
use crate::theme::ReviewTheme;
use bmux_tui::style::Style;
use bmux_tui::text::{Line, Span};
use bmux_tui_components::card::{
    Card as Frame, CardButton, CardGlyphs, CardHit, CardLayout, CardPolicy, CardSection,
    CardSegment, CardStyles,
};

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
        Frame::<ItemAction>::inner_width(self.card_width())
    }
}

impl Card<'_> {
    /// The card's rows.
    #[must_use]
    pub fn rows(self, layout: Layout, theme: &ReviewTheme) -> ItemRows {
        let ascii = theme.icons.accent_bar == "|";
        let policy = CardPolicy {
            layout: match layout.mode {
                ThreadCardMode::Compact => CardLayout::Compact,
                ThreadCardMode::Roomy => CardLayout::Roomy,
            },
            glyphs: if ascii {
                CardGlyphs::ASCII
            } else {
                CardGlyphs::ROUNDED
            },
            min_framed: MIN_FRAMED,
            separator: theme.icons.separator,
        };
        let fold = ItemAction::Thread(ThreadAction::Fold);
        let mut frame = Frame::new()
            .title(self.title.into_iter().map(CardSegment::text).collect())
            .header_action(fold)
            .sections(
                self.sections
                    .into_iter()
                    .map(|section| CardSection {
                        lines: section.lines,
                        hits: section
                            .actions
                            .into_iter()
                            .map(|action| CardHit {
                                row: action.row,
                                cells: action.cells,
                                action: action.action,
                            })
                            .collect(),
                    })
                    .collect(),
            )
            .buttons(
                self.buttons
                    .into_iter()
                    .map(|(label, action)| CardButton {
                        label: label.to_owned(),
                        action,
                    })
                    .collect(),
            )
            .footer(self.footer)
            .folded(self.folded)
            .styles(CardStyles {
                background: theme.threads.background,
                border: self.border,
                button: theme.gap_action,
                separator: theme.muted,
            })
            .policy(policy);
        if let Some(viewed) = self.viewed {
            frame = frame.right(CardSegment::action(
                vec![viewed],
                ItemAction::Thread(ThreadAction::Viewed),
            ));
        }
        let rows = frame.rows(layout.indent.min(layout.width), layout.card_width());
        ItemRows::lines(
            rows.lines,
            rows.hits
                .into_iter()
                .map(|hit| ActionCells {
                    row: hit.row,
                    cells: hit.cells,
                    action: hit.action,
                })
                .collect(),
        )
    }
}
