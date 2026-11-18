//! The panel's chips row: the context, the model, the reasoning effort, the permission
//! categories and the tools list.
//!
//! Chips are indivisible: each is placed whole, measured by its BMUX text width, left to right
//! with one cell between, on at most [`MAX_ROWS`] rows. When they do not fit, the short labels
//! (glyphs) are used, then chips are dropped from the lowest priority up; the model chip is
//! dropped last, and alone it is cut to the width. Painting and pointer regions both come from
//! the one [`ChipPlan`], so a chip's region is exactly the cells it painted.

use super::profile::Category;
use bmux_tui::component::{Component, Constraints, LayoutCx, LayoutId, LayoutNode, LogicalSize};
use bmux_tui::geometry::Rect;
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::text::Line;
use std::cell::RefCell;

/// Layout id of the chips row.
pub const CHIPS_ID: &str = "app.assistant.chips";
/// Rows the chips take at most.
pub const MAX_ROWS: u16 = 2;

/// What a chip controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChipId {
    /// What messages carry from the screen.
    Context,
    /// The conversation's model.
    Model,
    /// Its reasoning effort.
    Effort,
    /// A permission category, on or off for the conversation.
    Category(Category),
    /// The assistant's tools.
    Tools,
    /// The conversation runs in permission bypass (pressing it steps out).
    Bypass,
}

/// One chip: its full and short labels and how late it is dropped (higher is later).
#[derive(Debug, Clone)]
pub struct Chip {
    /// What it controls.
    pub id: ChipId,
    /// Label with room.
    pub full: Line,
    /// Label when room is short.
    pub short: Line,
    /// Drop priority: chips with the lowest value go first.
    pub keep: u16,
}

/// Where each chip is painted, relative to the chips row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChipPlan {
    /// Rows used.
    pub rows: u16,
    /// Each chip shown: what it controls, its cells and the line painted there.
    pub placed: Vec<(ChipId, Rect, Line)>,
}

/// Place `chips` in `width` columns on at most `max_rows` rows.
#[must_use]
pub fn plan(chips: &[Chip], width: u16, max_rows: u16) -> ChipPlan {
    if width == 0 || max_rows == 0 || chips.is_empty() {
        return ChipPlan::default();
    }
    for short in [false, true] {
        if let Some(plan) = pack(chips, short, width, max_rows) {
            return plan;
        }
    }
    let mut kept: Vec<Chip> = chips.to_vec();
    while kept.len() > 1 {
        let lowest = kept
            .iter()
            .enumerate()
            .min_by_key(|(index, chip)| (chip.keep, std::cmp::Reverse(*index)))
            .map_or(0, |(index, _)| index);
        kept.remove(lowest);
        if let Some(plan) = pack(&kept, true, width, max_rows) {
            return plan;
        }
    }
    // One chip wider than the row: cut to the row.
    kept.first().map_or_else(ChipPlan::default, |chip| {
        let line = chip.short.truncate(usize::from(width));
        let cells = u16::try_from(line.width()).unwrap_or(width).min(width);
        ChipPlan {
            rows: 1,
            placed: vec![(chip.id, Rect::new(0, 0, cells, 1), line)],
        }
    })
}

/// Pack every chip whole with its full or short label; `None` when they need more rows.
fn pack(chips: &[Chip], short: bool, width: u16, max_rows: u16) -> Option<ChipPlan> {
    let mut placed = Vec::with_capacity(chips.len());
    let (mut x, mut row) = (0u16, 0u16);
    for chip in chips {
        let line = if short { &chip.short } else { &chip.full };
        let cells = u16::try_from(line.width()).ok()?;
        if cells > width {
            return None;
        }
        if x > 0 && x.saturating_add(1).saturating_add(cells) > width {
            row += 1;
            x = 0;
        }
        if row >= max_rows {
            return None;
        }
        if x > 0 {
            x += 1;
        }
        placed.push((chip.id, Rect::new(x, row, cells, 1), line.clone()));
        x += cells;
    }
    Some(ChipPlan {
        rows: row + 1,
        placed,
    })
}

/// The chips row as a component: laid out from the width it is given, painted from the same
/// plan, which it keeps for pointer input.
pub struct ChipRow<'a> {
    chips: Vec<Chip>,
    plan: &'a RefCell<ChipPlan>,
}

impl<'a> ChipRow<'a> {
    /// A row of `chips`, keeping its plan in `plan`.
    #[must_use]
    pub const fn new(chips: Vec<Chip>, plan: &'a RefCell<ChipPlan>) -> Self {
        Self { chips, plan }
    }
}

impl Component for ChipRow<'_> {
    fn layout(&self, constraints: Constraints, _cx: &mut LayoutCx) -> LayoutNode {
        let width = u16::try_from(constraints.max_width()).unwrap_or(u16::MAX);
        let planned = plan(&self.chips, width, MAX_ROWS);
        let rows = planned.rows;
        *self.plan.borrow_mut() = planned;
        LayoutNode::leaf(
            LayoutId::new(CHIPS_ID),
            constraints.constrain(LogicalSize::new(constraints.max_width(), u64::from(rows))),
        )
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let height = u16::try_from(layout.size.height).unwrap_or(u16::MAX);
        for (_, rect, line) in &self.plan.borrow().placed {
            if rect.y >= height {
                continue;
            }
            cx.write_line(
                LocalRect::new(i32::from(rect.x), i64::from(rect.y), rect.width, 1),
                line,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tui::text::Span;

    fn chip(id: ChipId, full: &str, short: &str, keep: u16) -> Chip {
        Chip {
            id,
            full: Line::from_spans(vec![Span::raw(full.to_owned())]),
            short: Line::from_spans(vec![Span::raw(short.to_owned())]),
            keep,
        }
    }

    fn chips() -> Vec<Chip> {
        vec![
            chip(ChipId::Context, "⌖ src/retry.rs:42", "⌖", 4),
            chip(ChipId::Model, "● 模型 🚀 ▾", "●模型", 5),
            chip(ChipId::Effort, "◆ effort high", "◆", 3),
            chip(ChipId::Category(Category::Shell), "$ shell", "$", 2),
            chip(ChipId::Tools, "⚒ tools", "⚒", 1),
        ]
    }

    fn ids(plan: &ChipPlan) -> Vec<ChipId> {
        plan.placed.iter().map(|(id, _, _)| *id).collect()
    }

    #[test]
    fn chips_wrap_shorten_then_drop_keeping_the_model_last() {
        let wide = plan(&chips(), 100, MAX_ROWS);
        assert_eq!(wide.rows, 1);
        assert_eq!(ids(&wide).len(), 5);
        let two_rows = plan(&chips(), 30, MAX_ROWS);
        assert_eq!(two_rows.rows, 2);
        assert_eq!(ids(&two_rows).len(), 5);
        let short = plan(&chips(), 8, MAX_ROWS);
        assert!(short.placed.iter().all(|(_, _, line)| line.width() <= 5));
        let tiny = plan(&chips(), 5, 1);
        assert_eq!(ids(&tiny), [ChipId::Model]);
        let cut = plan(&chips(), 3, 1);
        assert_eq!(ids(&cut), [ChipId::Model]);
        assert!(cut.placed[0].1.width <= 3);
        assert_eq!(plan(&chips(), 0, MAX_ROWS), ChipPlan::default());
    }

    #[test]
    fn every_chip_cell_is_inside_the_width_and_chips_never_overlap() {
        for width in [1u16, 3, 8, 12, 24, 40, 100] {
            let planned = plan(&chips(), width, MAX_ROWS);
            for (index, (_, rect, line)) in planned.placed.iter().enumerate() {
                assert!(rect.right() <= width, "{width}: {rect:?}");
                assert_eq!(usize::from(rect.width), line.width(), "{width}");
                for (_, other, _) in &planned.placed[index + 1..] {
                    assert!(
                        rect.y != other.y || rect.right() <= other.x,
                        "{width}: {rect:?} {other:?}"
                    );
                }
            }
        }
    }
}
