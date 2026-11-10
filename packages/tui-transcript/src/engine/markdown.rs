//! Engine glue for Markdown contribution focus and activation ([`crate::markdown`]).
//!
//! The focused contribution, traversal and in-transcript navigation (footnote jumps with
//! return targets, `#fragment` links) live here; every reveal is a `FocusTarget` navigation
//! intent over the target's governed rows (BMUX focus reveal, C32), so nothing here stores a
//! position. `<details>` state is a render input the host owns (L2, L3): activation returns
//! the new state for the host to apply. External destinations, session links and the Mermaid
//! source view are host concerns ([`MarkdownActivation`]).

use std::time::Instant;

use bcode_markdown_render::MarkdownContributionKind;
use bmux_tui::component::LogicalRect;

use crate::focus::{FocusStep, FocusTargetId};
use crate::frame::FrameSnapshot;
use crate::layout::producers::markdown::{CONTENT_KEY_PREFIX, MarkdownTarget};
use crate::markdown::{MarkdownActivation, MarkdownFocusState, MarkdownTargetId, MarkdownView};
use crate::navigation::{ApproachEdge, NavIntent};
use crate::units::UnitKey;

use super::{EngineUpdate, TranscriptEngine};

impl TranscriptEngine {
    /// Markdown focus state (focused contribution and footnote return targets).
    #[must_use]
    pub const fn markdown_focus_state(&self) -> &MarkdownFocusState {
        &self.markdown
    }

    /// The focused Markdown contribution, if any.
    #[must_use]
    pub const fn markdown_focus(&self) -> Option<&MarkdownTargetId> {
        self.markdown.focused()
    }

    /// Read-only Markdown traversal over current units and retained layouts.
    #[must_use]
    pub const fn markdown_view(&self) -> MarkdownView<'_> {
        MarkdownView::new(&self.units, &self.layouts)
    }

    /// The retained Markdown target of `id`, if it is resident.
    #[must_use]
    pub fn markdown_target(&self, id: &MarkdownTargetId) -> Option<&MarkdownTarget> {
        self.markdown_view().target(id)
    }

    /// The Markdown target an [`crate::layout::geometry::ItemAction::Activate`] key of `unit`
    /// names (`markdown:<contribution id>`), if it is resident.
    #[must_use]
    pub fn markdown_target_for_action(
        &self,
        unit: &UnitKey,
        key: &str,
    ) -> Option<MarkdownTargetId> {
        let contribution = key.strip_prefix(CONTENT_KEY_PREFIX)?;
        let id = MarkdownTargetId::new(unit.clone(), contribution);
        self.markdown_target(&id).is_some().then_some(id)
    }

    /// Move Markdown focus one target in document order (`tui.markdown.focusNext` /
    /// `focusPrevious`), wrapping at the ends of the resident window, and reveal it from the
    /// approach edge (C32): moving down brings its last row to the bottom, moving up its first
    /// row to the top; a visible target does not scroll (T19). Without a focused target the walk
    /// starts at the first (or last) target in view. Returns the update and whether the focus
    /// changed.
    pub fn markdown_focus_step(&mut self, step: FocusStep, at: Instant) -> (EngineUpdate, bool) {
        self.observe_time(at);
        let position = self.presented(at);
        let frame = self
            .viewport
            .frame(&position, self.geometry.height, &self.layouts);
        let view = MarkdownView::new(&self.units, &self.layouts);
        let Some(target) = view.step(self.markdown.focused(), step, frame.as_ref()) else {
            return (EngineUpdate::default(), false);
        };
        let edge = match step {
            FocusStep::Next => ApproachEdge::Bottom,
            FocusStep::Previous => ApproachEdge::Top,
        };
        self.focus_markdown_target(target, Some(edge), at)
    }

    /// Focus a resident Markdown target without moving the reader (a click on it). Returns
    /// whether the focus changed; unknown targets change nothing.
    pub fn focus_markdown(&mut self, id: &MarkdownTargetId) -> bool {
        let view = MarkdownView::new(&self.units, &self.layouts);
        view.target(id).is_some() && self.markdown.focus(&view, id.clone())
    }

    /// Activate the focused Markdown target (`tui.markdown.activate`).
    pub fn activate_focused_markdown(&mut self, at: Instant) -> (EngineUpdate, MarkdownActivation) {
        let Some(id) = self.markdown.focused().cloned() else {
            return (EngineUpdate::default(), MarkdownActivation::Unavailable);
        };
        self.activate_markdown(&id, at)
    }

    /// Activate a resident Markdown target through its typed payload.
    ///
    /// * `<details>`: returns the toggled open state for the host's render configuration.
    /// * Mermaid: returns the source for the host's source view.
    /// * Footnote reference: focuses and reveals its definition (same document) and records
    ///   the reference as the definition's return target.
    /// * Footnote definition: focuses and reveals the recorded return target, else its first
    ///   reference.
    /// * Link and GitHub issue: returns the classified destination for the host.
    pub fn activate_markdown(
        &mut self,
        id: &MarkdownTargetId,
        at: Instant,
    ) -> (EngineUpdate, MarkdownActivation) {
        self.observe_time(at);
        let Some(kind) = self.markdown_target(id).map(|target| target.kind.clone()) else {
            return (EngineUpdate::default(), MarkdownActivation::Unavailable);
        };
        match kind {
            MarkdownContributionKind::Details { default_open, .. } => {
                let open = self
                    .render
                    .markdown
                    .details_open
                    .get(&id.contribution)
                    .copied()
                    .unwrap_or(default_open);
                (
                    EngineUpdate::default(),
                    MarkdownActivation::Details {
                        key: id.contribution.clone(),
                        open: !open,
                    },
                )
            }
            MarkdownContributionKind::Mermaid { source, .. } => (
                EngineUpdate::default(),
                MarkdownActivation::Mermaid { source },
            ),
            MarkdownContributionKind::FootnoteReference { target_id, .. } => {
                let definition = MarkdownTargetId::new(id.unit.clone(), target_id);
                if definition.contribution.is_empty() || self.markdown_target(&definition).is_none()
                {
                    return (
                        EngineUpdate::default(),
                        MarkdownActivation::FootnoteDefinition { focused: false },
                    );
                }
                self.markdown.record_return(definition.clone(), id.clone());
                let (update, _) = self.focus_markdown_target(definition, None, at);
                (
                    update,
                    MarkdownActivation::FootnoteDefinition { focused: true },
                )
            }
            MarkdownContributionKind::FootnoteDefinition { reference_ids, .. } => {
                let target = self
                    .markdown
                    .take_return(id)
                    .filter(|target| self.markdown_target(target).is_some())
                    .or_else(|| {
                        reference_ids
                            .iter()
                            .map(|reference| MarkdownTargetId::new(id.unit.clone(), reference))
                            .find(|reference| self.markdown_target(reference).is_some())
                    });
                let Some(target) = target else {
                    return (
                        EngineUpdate::default(),
                        MarkdownActivation::FootnoteReference { returned: false },
                    );
                };
                let (update, _) = self.focus_markdown_target(target, None, at);
                (
                    update,
                    MarkdownActivation::FootnoteReference { returned: true },
                )
            }
            MarkdownContributionKind::Link { destination, .. }
            | MarkdownContributionKind::GitHubIssue { destination, .. } => (
                EngineUpdate::default(),
                MarkdownActivation::Link(destination),
            ),
            MarkdownContributionKind::Image { .. }
            | MarkdownContributionKind::InlineMath { .. }
            | MarkdownContributionKind::DisplayMath { .. } => {
                (EngineUpdate::default(), MarkdownActivation::Unavailable)
            }
        }
    }

    /// Navigate to the in-transcript `#fragment` anchor (`fragment`, with or without the
    /// leading `#`): in unit `from` (the activated link's) first, else the first resident unit
    /// that has it. The anchored rows are revealed from the top (C32); navigation never forces
    /// paging. Returns the update and whether the anchor was found.
    pub fn navigate_markdown_fragment(
        &mut self,
        from: Option<&UnitKey>,
        fragment: &str,
        at: Instant,
    ) -> (EngineUpdate, bool) {
        self.observe_time(at);
        let fragment = fragment.strip_prefix('#').unwrap_or(fragment);
        let Some((unit, row)) = self.markdown_view().anchor(fragment, from) else {
            return (EngineUpdate::default(), false);
        };
        // The section from its anchor, at most a viewport tall: an anchor out of view goes to
        // the viewport top, a section already fully in view does not move.
        let height = self
            .layouts
            .current(&unit)
            .map_or(row + 1, |layout| layout.height);
        let end = row
            .saturating_add(self.geometry.height.max(1))
            .min(height)
            .max(row + 1);
        let mut update = self.intent(
            NavIntent::FocusTarget {
                target: FocusTargetId::unit(unit),
                edge: ApproachEdge::Top,
                rows: Some(row..end),
            },
            at,
        );
        update.invalidated = true;
        (update, true)
    }

    /// Viewport rectangles of the focused Markdown target in a prepared frame, clipped to the
    /// viewport, from the retained layout the frame painted (FO7, B3); empty when it is not
    /// visible. The highlight is a paint-only overlay (L5).
    #[must_use]
    pub fn markdown_focus_rects(&self, snapshot: &FrameSnapshot) -> Vec<LogicalRect> {
        self.markdown
            .focused()
            .map_or_else(Vec::new, |id| self.visible_markdown_rects(snapshot, id))
    }

    /// Viewport rectangles of Markdown target `id` in a prepared frame, clipped to the
    /// viewport; empty when no part of it is visible there.
    #[must_use]
    pub fn visible_markdown_rects(
        &self,
        snapshot: &FrameSnapshot,
        id: &MarkdownTargetId,
    ) -> Vec<LogicalRect> {
        let Some(unit) = snapshot
            .viewport
            .as_ref()
            .and_then(|viewport| viewport.units.iter().find(|unit| unit.key == id.unit))
        else {
            return Vec::new();
        };
        let Some(target) = self
            .painted_layout(snapshot, unit)
            .and_then(|layout| layout.geometry.markdown.target(&id.contribution))
        else {
            return Vec::new();
        };
        let width = snapshot.geometry.width;
        let height = i128::from(snapshot.geometry.height);
        target
            .rects
            .iter()
            .filter_map(|rect| {
                let top = i128::from(unit.top) + i128::from(rect.y);
                let bottom = top + i128::from(rect.height);
                let first = u64::try_from(top.max(0)).ok()?;
                let end = u64::try_from(bottom.min(height)).ok()?;
                let x = rect.x.min(width);
                let cells = rect.width.min(width - x);
                (end > first && cells > 0).then(|| LogicalRect::new(x, first, cells, end - first))
            })
            .collect()
    }

    /// End of every update (FO8): keep Markdown focus on a resident target and drop return
    /// targets whose ends are gone. Returns whether the focus changed.
    pub(super) fn reconcile_markdown(&mut self) -> bool {
        let view = MarkdownView::new(&self.units, &self.layouts);
        self.markdown.reconcile(&view)
    }

    /// Focus `target` and reveal its rows: from `edge` for traversal, else from the edge the
    /// target lies beyond (a jump). Returns the update and whether the focus changed.
    fn focus_markdown_target(
        &mut self,
        target: MarkdownTargetId,
        edge: Option<ApproachEdge>,
        at: Instant,
    ) -> (EngineUpdate, bool) {
        let view = MarkdownView::new(&self.units, &self.layouts);
        let Some(rows) = view.target(&target).map(MarkdownTarget::rows) else {
            return (EngineUpdate::default(), false);
        };
        let edge = edge.unwrap_or_else(|| self.jump_edge(&target.unit, rows.start, at));
        let changed = self.markdown.focus(&view, target.clone());
        let mut update = self.intent(
            NavIntent::FocusTarget {
                target: FocusTargetId::unit(target.unit),
                edge,
                rows: Some(rows),
            },
            at,
        );
        update.invalidated |= changed;
        (update, changed)
    }

    /// Approach edge of a jump to item-local `row` of `unit`: from the bottom when it lies
    /// below the viewport top, else from the top.
    fn jump_edge(&self, unit: &UnitKey, row: u64, at: Instant) -> ApproachEdge {
        let position = self.presented(at);
        let below = self
            .viewport
            .frame(&position, self.geometry.height, &self.layouts)
            .and_then(|frame| {
                let first = frame.units.first()?;
                let index = self.units.index_of(unit)?;
                Some(match index.cmp(&first.index) {
                    std::cmp::Ordering::Greater => true,
                    std::cmp::Ordering::Less => false,
                    std::cmp::Ordering::Equal => {
                        first
                            .top
                            .saturating_add(i64::try_from(row).unwrap_or(i64::MAX))
                            >= 0
                    }
                })
            })
            .unwrap_or(false);
        if below {
            ApproachEdge::Bottom
        } else {
            ApproachEdge::Top
        }
    }
}
