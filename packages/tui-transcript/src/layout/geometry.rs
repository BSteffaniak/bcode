//! Per-item geometry derived from one retained layout (B3, B6, B12, L6).
//!
//! Source mappings, focus targets, hit regions, selection fragments, content keys and paint
//! slots are all read from the same [`LayoutNode`] that measured the item and that paints it.
//! Nothing here re-wraps or re-measures text.

use std::ops::Range;

use bmux_tui::component::LogicalRect;

use super::SourcePosition;
use crate::units::structure::DisclosureTarget;

/// One source position starting at a row of an item (P3, C24).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceRow {
    /// Row inside the item where the position begins.
    pub row: u64,
    /// Opaque source position.
    pub position: SourcePosition,
}

/// Row-ordered source mapping of one item at one width.
///
/// Both directions are logarithmic: rows are kept in row order, and a position index (built
/// once, when the layout is measured) orders them by position, then row. Position resolution
/// runs on every prepared frame for a source-backed anchor (C24), so it never scans the item.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct SourceRows {
    rows: Vec<SourceRow>,
    /// Indices into `rows`, sorted by (position, row). Positions of one kind are contiguous
    /// because `SourcePosition` orders by kind first.
    by_position: Vec<usize>,
}

impl SourceRows {
    /// A mapping from rows (sorted by row; ties keep their order).
    #[must_use]
    pub fn new(mut rows: Vec<SourceRow>) -> Self {
        rows.sort_by_key(|row| row.row);
        let mut by_position = (0..rows.len()).collect::<Vec<_>>();
        by_position.sort_by(|&a, &b| {
            rows[a]
                .position
                .cmp(&rows[b].position)
                .then(rows[a].row.cmp(&rows[b].row))
        });
        Self { rows, by_position }
    }

    /// Whether nothing is mapped (rows fall back to item-row anchors, C24).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Mapped rows in row order.
    #[must_use]
    pub fn rows(&self) -> &[SourceRow] {
        &self.rows
    }

    /// Row where `position` begins: the row of the last mapped position of the same source at
    /// or before it ([`SourcePosition::same_source`]: the same kind, and the same identity for
    /// plugin sources). `None` when no mapped position of that source precedes it.
    #[must_use]
    pub fn row_for_source(&self, position: &SourcePosition) -> Option<u64> {
        let end = self
            .by_position
            .partition_point(|&index| &self.rows[index].position <= position);
        let mapped = &self.rows[*self.by_position[..end].last()?];
        mapped.position.same_source(position).then_some(mapped.row)
    }

    /// Source position covering `row`, with the row where it begins (at most `row`).
    #[must_use]
    pub fn source_for_row(&self, row: u64) -> Option<(SourcePosition, u64)> {
        let end = self.rows.partition_point(|mapped| mapped.row <= row);
        self.rows[..end]
            .last()
            .map(|mapped| (mapped.position.clone(), mapped.row))
    }
}

/// One focus target inside an item (FO7, B12, BMUX 12).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ItemFocusTarget {
    /// Target index within the item in layout order (`0` is the item itself).
    pub local: u32,
    /// Rows the target covers.
    pub rows: Range<u64>,
    /// Enclosing target, if nested.
    pub parent: Option<u32>,
    /// Disclosure toggled by activating the target, if it is a disclosure target.
    pub disclosure: Option<DisclosureTarget>,
}

/// What activating a hit region does.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ItemAction {
    /// Toggle a disclosure target (X1).
    ToggleDisclosure(DisclosureTarget),
    /// A producer-defined activation, namespaced by the producer (for example a link or a
    /// copy button).
    Activate(String),
}

/// One interactive region of an item, in item-local cells.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ItemHitRegion {
    /// Region in item-local logical cells.
    pub rect: LogicalRect,
    /// Activation.
    pub action: ItemAction,
}

/// One selectable fragment of an item (C34, ambiguity 10).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ItemSelectionFragment {
    /// Rows the fragment covers.
    pub rows: Range<u64>,
    /// Source position of the fragment start; `None` for rendered fragments.
    pub source: Option<SourcePosition>,
    /// Whether the fragment is explicitly declared without source mapping (diagrams, images,
    /// plugin rows) and copies its rendered text.
    pub rendered: bool,
}

/// One run of selectable cells on one item row, mapped to bytes of its [`SelectableText`]
/// (C34, BMUX 13).
///
/// Runs are read from the retained layout when the item is measured. A [`SelectableRunKind::Text`]
/// run covers cells whose rendered graphemes are exactly the painted text's `source` bytes
/// ([`SelectableText::painted`]), so BMUX's painted-cell fragment mechanics
/// (`painted_text_fragments`, which advance exactly as the buffer paints) register one selection
/// fragment per painted grapheme from it while painting; a
/// [`SelectableRunKind::Unit`] run is one indivisible visual unit (an entity, a tab, a rendered
/// transformation) registered as a single fragment.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SelectableRun {
    /// Item-local row.
    pub row: u64,
    /// First item-local column.
    pub x: u64,
    /// Width in cells.
    pub width: u64,
    /// Source bytes of the run in its text.
    pub source: Range<usize>,
    /// How the run's cells map to its source.
    pub kind: SelectableRunKind,
}

/// How a [`SelectableRun`] maps cells to source bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SelectableRunKind {
    /// Rendered graphemes equal the source bytes, one grapheme per fragment.
    Text,
    /// One indivisible visual unit covering all of `source`.
    Unit,
}

/// Source-backed selectable text of one item part: the logical source a selection copies
/// (C34) and the runs that place it on rows.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SelectableText {
    /// Logical source text a selection copies (Markdown source, plain text, tool output).
    pub text: std::sync::Arc<str>,
    /// The text as painted, when it differs from [`Self::text`] (control characters painted
    /// as spaces). It has the same byte length, so run source ranges index both; fragments are
    /// measured over it and copies read [`Self::text`] (C34, C36).
    pub display: Option<std::sync::Arc<str>>,
    /// Runs in row order.
    pub runs: Vec<SelectableRun>,
}

impl SelectableText {
    /// Selectable text whose painted text is `text` itself.
    #[must_use]
    pub const fn new(text: std::sync::Arc<str>, runs: Vec<SelectableRun>) -> Self {
        Self {
            text,
            display: None,
            runs,
        }
    }

    /// The text as painted: [`Self::display`] when present, else [`Self::text`].
    #[must_use]
    pub fn painted(&self) -> &str {
        self.display.as_deref().unwrap_or(&self.text)
    }

    /// Rows the runs cover (`0..0` without runs).
    #[must_use]
    pub fn rows(&self) -> Range<u64> {
        let start = self.runs.first().map_or(0, |run| run.row);
        let end = self.runs.last().map_or(0, |run| run.row + 1);
        start..end.max(start)
    }

    /// The runs on item-local `rows`, found by binary search over the row-ordered runs, so a
    /// frame visits only the runs it shows however tall the item is.
    #[must_use]
    pub fn runs_in_rows(&self, rows: Range<u64>) -> &[SelectableRun] {
        let start = self.runs.partition_point(|run| run.row < rows.start);
        let end = self.runs.partition_point(|run| run.row < rows.end);
        self.runs.get(start..end.max(start)).unwrap_or_default()
    }

    /// The text moved by `dx` columns and `dy` rows.
    #[must_use]
    pub fn translated(self, dx: u64, dy: u64) -> Self {
        Self {
            text: self.text,
            display: self.display,
            runs: self
                .runs
                .into_iter()
                .map(|run| SelectableRun {
                    row: run.row + dy,
                    x: run.x + dx,
                    ..run
                })
                .collect(),
        }
    }
}

/// A producer-namespaced content key covering rows (E3).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContentKeyRegion {
    /// Namespaced key; never authorizes anything.
    pub key: String,
    /// Rows the content covers.
    pub rows: Range<u64>,
}

/// What a paint slot shows (L5: paint-only, laid out with a fixed reservation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PaintSlotKind {
    /// Activity spinner glyph; one cell.
    Spinner,
    /// Elapsed time since `started_at_ms`, right-aligned in the reserved cells.
    Elapsed {
        /// Wall-clock start in milliseconds.
        started_at_ms: u64,
    },
}

/// A reserved region painted from [`super::context::PaintInputs`] without relayout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PaintSlot {
    /// Reserved item-local cells.
    pub rect: LogicalRect,
    /// What the slot shows.
    pub kind: PaintSlotKind,
}

/// All per-item geometry from one retained layout (L6).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ItemGeometry {
    /// Source positions per row.
    pub source_map: SourceRows,
    /// Focus targets in layout order.
    pub focus_targets: Vec<ItemFocusTarget>,
    /// Interactive regions.
    pub hit_regions: Vec<ItemHitRegion>,
    /// Selectable fragments.
    pub selection_fragments: Vec<ItemSelectionFragment>,
    /// Source-backed selectable text parts in layout order; a part's index is its selection
    /// content identity within the item (C34).
    pub selectable: Vec<SelectableText>,
    /// Producer content keys.
    pub content_keys: Vec<ContentKeyRegion>,
    /// Paint-only slots.
    pub paint_slots: Vec<PaintSlot>,
    /// Reserved asynchronous visuals (Markdown images and Mermaid diagrams, L8) that host-side
    /// workers render outside drawing.
    pub visual_slots: Vec<super::producers::markdown::VisualSlot>,
    /// Actionable Markdown contributions and fragment anchors (keyboard focus and activation
    /// of links, footnotes, `<details>` and Mermaid diagrams).
    pub markdown: super::producers::markdown::MarkdownGeometry,
    /// Rows whose pointer and key input belong to their content (plugin visual rows routed to
    /// their adapter, shell screens scrolling horizontally on their own; E1, C39).
    pub content_targets: Vec<super::producers::visual::ContentTarget>,
}

impl ItemGeometry {
    /// The geometry moved by `dx` columns and `dy` rows (a wrapper placed the content at that
    /// offset inside its own node).
    #[must_use]
    pub fn translated(self, dx: u64, dy: u64) -> Self {
        let rows = |range: Range<u64>| range.start + dy..range.end + dy;
        let rect =
            |rect: LogicalRect| LogicalRect::new(rect.x + dx, rect.y + dy, rect.width, rect.height);
        Self {
            source_map: SourceRows::new(
                self.source_map
                    .rows
                    .into_iter()
                    .map(|row| SourceRow {
                        row: row.row + dy,
                        position: row.position,
                    })
                    .collect(),
            ),
            focus_targets: self
                .focus_targets
                .into_iter()
                .map(|target| ItemFocusTarget {
                    rows: rows(target.rows),
                    ..target
                })
                .collect(),
            hit_regions: self
                .hit_regions
                .into_iter()
                .map(|region| ItemHitRegion {
                    rect: rect(region.rect),
                    action: region.action,
                })
                .collect(),
            selection_fragments: self
                .selection_fragments
                .into_iter()
                .map(|fragment| ItemSelectionFragment {
                    rows: rows(fragment.rows),
                    ..fragment
                })
                .collect(),
            selectable: self
                .selectable
                .into_iter()
                .map(|text| text.translated(dx, dy))
                .collect(),
            content_keys: self
                .content_keys
                .into_iter()
                .map(|content| ContentKeyRegion {
                    rows: rows(content.rows),
                    key: content.key,
                })
                .collect(),
            paint_slots: self
                .paint_slots
                .into_iter()
                .map(|slot| PaintSlot {
                    rect: rect(slot.rect),
                    kind: slot.kind,
                })
                .collect(),
            visual_slots: self
                .visual_slots
                .into_iter()
                .map(|slot| super::producers::markdown::VisualSlot {
                    rect: rect(slot.rect),
                    ..slot
                })
                .collect(),
            markdown: self.markdown.translated(dx, dy),
            content_targets: self
                .content_targets
                .into_iter()
                .map(|target| super::producers::visual::ContentTarget {
                    rect: rect(target.rect),
                    kind: target.kind,
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_rows_map_both_ways_and_fall_back_when_unmapped() {
        let rows = SourceRows::new(vec![
            SourceRow {
                row: 3,
                position: SourcePosition::MarkdownByte(20),
            },
            SourceRow {
                row: 0,
                position: SourcePosition::MarkdownByte(0),
            },
        ]);
        assert_eq!(
            rows.row_for_source(&SourcePosition::MarkdownByte(25)),
            Some(3)
        );
        assert_eq!(
            rows.row_for_source(&SourcePosition::MarkdownByte(5)),
            Some(0)
        );
        assert_eq!(rows.row_for_source(&SourcePosition::DiffLine(0)), None);
        assert_eq!(
            rows.source_for_row(4),
            Some((SourcePosition::MarkdownByte(20), 3))
        );
        assert_eq!(
            rows.source_for_row(2),
            Some((SourcePosition::MarkdownByte(0), 0))
        );
        assert_eq!(SourceRows::default().source_for_row(0), None);
    }

    /// Lookups agree with the definition (the row of the greatest same-kind position at or
    /// before the query, latest row on ties) across kinds, ties and repeated positions.
    #[test]
    fn row_for_source_matches_a_scan_over_mixed_kinds_and_ties() {
        let mut mapped = Vec::new();
        for row in 0..60_u64 {
            let position = match row % 4 {
                0 => SourcePosition::MarkdownByte(row / 2),
                1 => SourcePosition::ShellByte(100 - row),
                2 => SourcePosition::DiffLine(row / 8),
                _ => SourcePosition::PluginContent(format!("k{}", row % 7)),
            };
            mapped.push(SourceRow { row, position });
        }
        let rows = SourceRows::new(mapped.clone());
        let scan = |position: &SourcePosition| {
            mapped
                .iter()
                .filter(|row| {
                    std::mem::discriminant(&row.position) == std::mem::discriminant(position)
                        && &row.position <= position
                })
                .max_by(|a, b| a.position.cmp(&b.position).then(a.row.cmp(&b.row)))
                .map(|row| row.row)
        };
        let queries = (0..120_u64)
            .flat_map(|value| {
                [
                    SourcePosition::MarkdownByte(value),
                    SourcePosition::ShellByte(value),
                    SourcePosition::DiffLine(value / 4),
                    SourcePosition::PluginContent(format!("k{}", value % 9)),
                ]
            })
            .collect::<Vec<_>>();
        for query in &queries {
            assert_eq!(rows.row_for_source(query), scan(query), "{query:?}");
        }
    }

    #[test]
    fn selectable_runs_are_found_by_row_range_and_survive_translation() {
        let runs = (0..4_096_u64)
            .flat_map(|row| {
                [0, 10].map(|x| SelectableRun {
                    row,
                    x,
                    width: 5,
                    source: 0..1,
                    kind: SelectableRunKind::Unit,
                })
            })
            .collect::<Vec<_>>();
        let text = SelectableText::new(std::sync::Arc::from("x"), runs);
        assert_eq!(text.rows(), 0..4_096);
        let visible = text.runs_in_rows(2_000..2_003);
        assert_eq!(visible.len(), 6);
        assert!(visible.iter().all(|run| (2_000..2_003).contains(&run.row)));
        assert_eq!(text.runs_in_rows(5_000..5_010), &[] as &[SelectableRun]);
        assert_eq!(text.runs_in_rows(3..3), &[] as &[SelectableRun]);
        let moved = text.translated(2, 7);
        assert_eq!(moved.runs_in_rows(7..8).len(), 2);
        assert_eq!(moved.runs_in_rows(7..8)[0].x, 2);
        assert_eq!(moved.painted(), "x");
    }
}
