//! Transcript selection entry points (spec C34–C36, W7, BMUX 13).
//!
//! The frame registers selectable text from the retained layouts it paints: one selection
//! scope over the viewport, one child scope per visible unit with selectable text, and one
//! fragment per grapheme (or indivisible unit) of every visible run. Highlights come from the
//! same frame's scene in the same paint (F4). The host resolves pointer endpoints against the
//! committed scene and hands the engine logical endpoints ([`TranscriptSelectionEndpoint`]),
//! which survive scrolling, paging and reflow.

use std::cmp::Ordering;
use std::ops::Range;
use std::time::Instant;

use bmux_tui::geometry::Rect;
use bmux_tui::paint::PaintCx;
use bmux_tui::selection::{
    SelectionAutoScrollPolicy, SelectionContentId, SelectionEndpoint, SelectionFragment,
    SelectionScope, painted_text_fragments,
};
use bmux_tui::style::Style;
use bmux_tui_components::virtual_list::{ItemOrder, VirtualListAction};

use super::{EngineUpdate, TranscriptEngine};
use crate::frame::FrameSnapshot;
use crate::layout::geometry::{SelectableRunKind, SelectableText};
use crate::selection::{
    ResidentOrder, SELECTION_SCOPE, SelectionKey, TranscriptSelection, TranscriptSelectionEndpoint,
    unit_scope_id,
};
use crate::viewport::{ViewportOutput, translate_action};

/// Separator between the copied text of two parts of one unit.
const PART_SEPARATOR: &str = "\n";
/// Separator between the copied text of two units (V1 joins items the same way).
const UNIT_SEPARATOR: &str = "\n\n";

impl TranscriptEngine {
    /// The current transcript selection (a collapsed one while a press is armed).
    #[must_use]
    pub const fn selection(&self) -> Option<&TranscriptSelection> {
        self.selection.selection()
    }

    /// Whether a pointer gesture is extending the selection.
    #[must_use]
    pub const fn selection_gesture_active(&self) -> bool {
        self.selection.gesture_active()
    }

    /// Replace the selection; `gesture` marks a pointer gesture still extending it. Both defer
    /// eviction (W7). Invalidates the transcript when the painted selection changed.
    pub fn set_selection(
        &mut self,
        selection: Option<TranscriptSelection>,
        gesture: bool,
    ) -> EngineUpdate {
        EngineUpdate {
            requests: Vec::new(),
            invalidated: self.selection.set(selection, gesture),
        }
    }

    /// The selection key whose fragments `snapshot` registered under `content`, if any.
    #[must_use]
    pub fn selection_key(
        &self,
        snapshot: &FrameSnapshot,
        content: &SelectionContentId,
    ) -> Option<SelectionKey> {
        let viewport = snapshot.viewport.as_ref()?;
        viewport.units.iter().find_map(|unit| {
            let layout = self.painted_layout(snapshot, unit)?;
            (0..layout.geometry.selectable.len()).find_map(|part| {
                let key = SelectionKey::new(unit.key.clone(), u32::try_from(part).ok()?);
                (&key.content_id() == content).then_some(key)
            })
        })
    }

    /// The logical endpoint of a scene endpoint resolved against `snapshot`'s fragments.
    #[must_use]
    pub fn selection_endpoint(
        &self,
        snapshot: &FrameSnapshot,
        endpoint: &SelectionEndpoint,
    ) -> Option<TranscriptSelectionEndpoint> {
        TranscriptSelectionEndpoint::from_scene(endpoint, |content| {
            self.selection_key(snapshot, content)
        })
    }

    /// Document order of two logical endpoints; `None` when a unit is not resident.
    #[must_use]
    pub fn compare_selection_endpoints(
        &self,
        left: &TranscriptSelectionEndpoint,
        right: &TranscriptSelectionEndpoint,
    ) -> Option<Ordering> {
        let index = self.viewport.index();
        let order = ResidentOrder::new(&index);
        Some(
            order
                .compare_items(&left.key, &right.key)?
                .then(left.source_offset.cmp(&right.source_offset)),
        )
    }

    /// The selected logical source text (C34): each selected part's source bytes, parts of one
    /// unit joined by a newline and units by a blank line, in document order. `None` without a
    /// non-empty selection.
    #[must_use]
    pub fn selected_text(&self) -> Option<String> {
        let selection = self.selection.selection()?;
        let index = self.viewport.index();
        let order = ResidentOrder::new(&index);
        let (start, end) = selection.ordered(&order)?;
        let first = index.index_of(&start.key.unit)?;
        let last = index.index_of(&end.key.unit)?;
        let mut units = Vec::new();
        for key in self.units.keys().get(first..=last)? {
            let Some(layout) = self.layouts.current(key) else {
                continue;
            };
            let parts = selected_parts(key, &layout.geometry.selectable, selection, order);
            if !parts.is_empty() {
                units.push(parts.join(PART_SEPARATOR));
            }
        }
        let text = units.join(UNIT_SEPARATOR);
        (!text.is_empty()).then_some(text)
    }

    /// Scroll for a selection edge-autoscroll request (BMUX 13): routed through the list action
    /// `SelectionAutoScroll`, which navigation reduces as a manual scroll (C14).
    pub fn selection_auto_scroll(
        &mut self,
        request: &bmux_tui::selection::SelectionAutoScrollRequest,
        at: Instant,
    ) -> EngineUpdate {
        let Some(ViewportOutput::Navigate(intent)) =
            VirtualListAction::selection_auto_scroll(request).and_then(translate_action)
        else {
            return EngineUpdate::default();
        };
        self.intent(intent, at)
    }

    /// Register the frame's selectable text and paint the selection highlight (C34, F4).
    ///
    /// `cx` is the viewport's context, as for [`Self::paint_frame`]. Scopes and fragments come
    /// from the retained layouts the snapshot painted, translated by each unit's row, so only
    /// visible, correctly positioned text is selectable (B7); fragments clipped by the viewport
    /// are dropped by BMUX. The highlight is computed from the scene just registered.
    pub fn paint_selection(
        &self,
        snapshot: &FrameSnapshot,
        cx: &mut PaintCx<'_, '_>,
        style: Style,
    ) {
        let Some(viewport) = snapshot.viewport.as_ref() else {
            return;
        };
        let cells = |value: u64| u16::try_from(value).unwrap_or(u16::MAX);
        let (width, height) = (
            cells(snapshot.geometry.width),
            cells(snapshot.geometry.height),
        );
        let area = Rect::new(0, 0, width, height);
        cx.push_selection_scope(
            SelectionScope::new(SELECTION_SCOPE, area)
                .auto_scroll(SelectionAutoScrollPolicy::enabled()),
        );
        let mut contents = std::collections::BTreeMap::new();
        for (order, unit) in viewport.units.iter().enumerate() {
            let Some(layout) = self.painted_layout(snapshot, unit) else {
                continue;
            };
            let texts = &layout.geometry.selectable;
            if texts.is_empty() {
                continue;
            }
            let order = u64::try_from(order).unwrap_or(u64::MAX) << 16;
            let visible_row = |row: u64| {
                let row = unit.top.checked_add(i64::try_from(row).ok()?)?;
                u16::try_from(row).ok().filter(|row| *row < height)
            };
            let Some(scope_area) = unit_rows(unit.top, unit.height, height)
                .map(|(top, rows)| Rect::new(0, top, width, rows))
            else {
                continue;
            };
            // Item-local rows the viewport shows: only their runs are visited (L6), found by
            // binary search, however tall the unit is.
            let first = u64::try_from(unit.top.saturating_neg()).unwrap_or(0);
            let visible = first..first.saturating_add(u64::from(scope_area.height));
            let scope = unit_scope_id(&unit.key);
            cx.push_selection_scope(
                SelectionScope::new(scope.clone(), scope_area)
                    .parent(SELECTION_SCOPE)
                    .initiation_area(initiation_area(texts, &visible, unit.top, height))
                    .order(order)
                    .auto_scroll(SelectionAutoScrollPolicy::enabled()),
            );
            for (part, text) in texts.iter().enumerate() {
                let part = u32::try_from(part).unwrap_or(u32::MAX);
                let key = SelectionKey::new(unit.key.clone(), part);
                let content = key.content_id();
                contents.insert(content.clone(), key);
                let order = order | u64::from(part);
                for run in text.runs_in_rows(visible.clone()) {
                    let Some(y) = visible_row(run.row) else {
                        continue;
                    };
                    let Some(source) = text.painted().get(run.source.clone()) else {
                        continue;
                    };
                    let rect = Rect::new(cells(run.x), y, cells(run.width), 1);
                    match run.kind {
                        SelectableRunKind::Text => {
                            // Fragments advance exactly as the buffer painted the run's cells,
                            // so hits, highlights and copied ranges agree with them (B3).
                            for fragment in painted_text_fragments(
                                scope.clone(),
                                content.clone(),
                                rect,
                                order,
                                source,
                                run.source.start,
                                0,
                            ) {
                                cx.push_selection_fragment(fragment);
                            }
                        }
                        SelectableRunKind::Unit => {
                            cx.push_selection_fragment(SelectionFragment::new(
                                scope.clone(),
                                content.clone(),
                                rect,
                                order,
                                run.source.clone(),
                            ));
                        }
                    }
                }
            }
        }
        let Some(selection) = snapshot.selection.as_ref() else {
            return;
        };
        let index = self.viewport.index();
        let order = ResidentOrder::new(&index);
        let highlights = selection.highlights(
            cx.selection(),
            |content| contents.get(content).cloned(),
            &order,
        );
        cx.paint_selection_highlights(&highlights, style);
    }
}

/// The source text of each part of unit `key` the selection covers, in part order.
fn selected_parts(
    key: &crate::units::UnitKey,
    texts: &[SelectableText],
    selection: &TranscriptSelection,
    order: ResidentOrder<'_>,
) -> Vec<String> {
    texts
        .iter()
        .enumerate()
        .filter_map(|(part, text)| {
            let key = SelectionKey::new(key.clone(), u32::try_from(part).ok()?);
            let range = selection.range_in(&key, text.text.len(), &order)?;
            text.text.get(range).map(str::to_owned)
        })
        .filter(|text| !text.is_empty())
        .collect()
}

/// Visible viewport rows of a unit at `top` with `height` rows: the first visible row and the
/// row count; `None` when none is visible.
fn unit_rows(top: i64, height: u64, viewport: u16) -> Option<(u16, u16)> {
    let start = top.max(0);
    let end = top
        .saturating_add(i64::try_from(height).unwrap_or(i64::MAX))
        .min(i64::from(viewport));
    (start < end).then(|| {
        (
            u16::try_from(start).unwrap_or(u16::MAX),
            u16::try_from(end - start).unwrap_or(u16::MAX),
        )
    })
}

/// Where a gesture locks to a unit: the bounding box of its selectable runs on the item-local
/// `visible` rows. Presses elsewhere (separators, margins, headers without text) start in the
/// transcript scope, which spans units (C34).
fn initiation_area(
    texts: &[SelectableText],
    visible: &Range<u64>,
    top: i64,
    viewport: u16,
) -> Rect {
    let runs = texts
        .iter()
        .flat_map(|text| text.runs_in_rows(visible.clone()));
    let (mut first, mut last, mut left, mut right) = (u64::MAX, 0, u64::MAX, 0);
    for run in runs {
        first = first.min(run.row);
        last = last.max(run.row + 1);
        left = left.min(run.x);
        right = right.max(run.x + run.width);
    }
    if first >= last || left >= right {
        return Rect::new(0, 0, 0, 0);
    }
    let Some((y, rows)) = unit_rows(
        top.saturating_add(i64::try_from(first).unwrap_or(i64::MAX)),
        last - first,
        viewport,
    ) else {
        return Rect::new(0, 0, 0, 0);
    };
    let cells = |value: u64| u16::try_from(value).unwrap_or(u16::MAX);
    Rect::new(cells(left), y, cells(right - left), rows)
}

#[cfg(test)]
mod tests;
