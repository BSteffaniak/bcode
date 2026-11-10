//! Engine glue for keyboard focus (spec section 6) and the latest-content bar (F6, C18–C21).
//!
//! Focus traversal and the latest-bar decision are pure functions in [`crate::focus`] and
//! [`crate::latest_bar`]; this module applies them to engine state at the end of every update
//! ([`TranscriptEngine::settle_interaction`]) and exposes the focus operations the host maps
//! keys to. Focus moves become `FocusTarget` navigation intents (C32); nothing here stores a
//! position.

use std::ops::Range;
use web_time::Instant;

use bmux_tui::component::LogicalRect;

use crate::focus::{FocusEdge, FocusState, FocusStep, FocusTargetId, FocusView};
use crate::frame::FrameSnapshot;
use crate::latest_bar::{LatestBarInputs, LatestBarState, evaluate};
use crate::layout::ItemLayout;
use crate::navigation::{ApproachEdge, NavInput, NavIntent, NavMode, RemovedUnits};
use crate::units::UnitChanges;
use crate::viewport::{ViewportFrame, ViewportGeometry, VisibleUnit};

use super::{EngineUpdate, ItemHit, TranscriptEngine};

impl TranscriptEngine {
    // ---- focus ----------------------------------------------------------------------------

    /// Engine-owned focus state (FO1).
    #[must_use]
    pub const fn focus_state(&self) -> &FocusState {
        &self.focus
    }

    /// Read-only focus traversal over current units and retained layouts.
    #[must_use]
    pub const fn focus_view(&self) -> FocusView<'_> {
        FocusView::new(&self.units, &self.layouts)
    }

    /// Enter transcript focus (FO2) on the initial target (C33). Returns the update and whether
    /// focus entered; with no focus target resident the composer keeps focus.
    pub fn enter_focus(&mut self, at: Instant) -> (EngineUpdate, bool) {
        self.observe_time(at);
        if self.focus.target().is_some() {
            return (EngineUpdate::default(), false);
        }
        let position = self.presented(at);
        let frame = self
            .viewport
            .frame(&position, self.geometry.height, &self.layouts);
        let Some(target) = self.focus_view().initial(frame.as_ref()) else {
            return (EngineUpdate::default(), false);
        };
        let visible = frame
            .as_ref()
            .is_some_and(|frame| target_starts_visible(frame, &target, &self.focus_view()));
        let view = FocusView::new(&self.units, &self.layouts);
        self.focus.focus(&view, target.clone());
        let mut update = if visible {
            EngineUpdate::default()
        } else {
            let rows = self.focus_view().extent(&target);
            self.intent(
                NavIntent::FocusTarget {
                    target,
                    edge: ApproachEdge::Top,
                    rows,
                },
                at,
            )
        };
        update.invalidated = true;
        (update, true)
    }

    /// Return keyboard focus to the composer (FO3). Returns whether the transcript had focus.
    pub fn release_focus(&mut self) -> bool {
        self.focus.release()
    }

    /// Move focus one target in document order (FO6). Moving up approaches the target from the
    /// top and moving down from the bottom (C32); visible targets do not scroll (T19). At either
    /// end of the resident window focus stays (focus never forces paging, FO8).
    pub fn focus_step(&mut self, step: FocusStep, at: Instant) -> EngineUpdate {
        self.observe_time(at);
        let Some(current) = self.focus.target().cloned() else {
            return EngineUpdate::default();
        };
        let Some(target) = self.focus_view().step(&current, step) else {
            return EngineUpdate::default();
        };
        self.focus_on(target, step_edge(step), at)
    }

    /// Focus `target` (entering transcript focus), approaching it from the top (C32). Unknown
    /// targets change nothing.
    pub fn focus_target(&mut self, target: FocusTargetId, at: Instant) -> EngineUpdate {
        self.observe_time(at);
        if self.focus_view().target(&target).is_none() {
            return EngineUpdate::default();
        }
        self.focus_on(target, ApproachEdge::Top, at)
    }

    /// Focus the enclosing target of the focused one, if nested.
    pub fn focus_parent(&mut self, at: Instant) -> EngineUpdate {
        self.observe_time(at);
        let Some(parent) = self
            .focus
            .target()
            .and_then(|current| self.focus_view().parent(current))
        else {
            return EngineUpdate::default();
        };
        self.focus_on(parent, ApproachEdge::Top, at)
    }

    /// `JumpToSessionStart` or `JumpToLatest` with direct `Head`/`Tail` loads (FO4, FO5, W10).
    /// With transcript focus, focus moves to the first / last target of the loaded window once
    /// the load lands.
    pub fn jump_to_edge(&mut self, edge: FocusEdge, at: Instant) -> EngineUpdate {
        let intent = match edge {
            FocusEdge::First => NavIntent::JumpToSessionStart,
            FocusEdge::Last => NavIntent::JumpToLatest,
        };
        if self.focus.target().is_some() {
            self.focus.set_pending_edge(Some(edge));
        }
        let mut update = self.intent(intent, at);
        update.merge(self.settle_interaction(at));
        update
    }

    fn focus_on(&mut self, target: FocusTargetId, edge: ApproachEdge, at: Instant) -> EngineUpdate {
        let view = FocusView::new(&self.units, &self.layouts);
        self.focus.set_pending_edge(None);
        self.focus.focus(&view, target.clone());
        let rows = view.extent(&target);
        let mut update = self.intent(NavIntent::FocusTarget { target, edge, rows }, at);
        update.invalidated = true;
        update
    }

    // ---- latest bar -----------------------------------------------------------------------

    /// The latest-bar decision stored by the last update (F6).
    #[must_use]
    pub const fn latest_bar(&self) -> LatestBarState {
        self.latest.state()
    }

    /// Full transcript-region geometry; [`Self::geometry`] is the effective viewport (the
    /// region minus the latest-bar row while the bar is shown, F6).
    #[must_use]
    pub const fn region(&self) -> ViewportGeometry {
        self.latest.region()
    }

    /// Enable or disable the latest bar (hosts that present it enable it; disabled engines
    /// never reserve a row).
    pub fn set_latest_bar_enabled(&mut self, enabled: bool, at: Instant) -> EngineUpdate {
        self.observe_time(at);
        if self.latest.enabled() == enabled {
            return EngineUpdate::default();
        }
        self.latest.set_enabled(enabled);
        let mut update = self.apply_effective_geometry();
        update.merge(self.settle_interaction(at));
        update
    }

    /// Record hidden activity for units an update regenerated (C20): inserted or revised
    /// content units entirely below the region while navigation does not follow.
    pub(super) fn record_hidden_activity(&mut self, changes: &UnitChanges, at: Instant) {
        if !self.latest.enabled() || self.nav.mode == NavMode::Following {
            return;
        }
        let Some(last_visible) = self
            .region_frame(at)
            .and_then(|frame| frame.units.last().map(|unit| unit.index))
        else {
            return;
        };
        let below = changes
            .inserted()
            .chain(changes.revised.iter())
            .filter(|key| !key.is_boundary())
            .filter_map(|key| self.units.index_of(key))
            .any(|index| index > last_visible);
        if below {
            self.latest.record_activity(at);
        }
    }

    /// End of every update (F6, FO8): keep focus on a resident target, move focus to the
    /// requested window edge once a direct load landed, then evaluate the latest bar against
    /// the full region and, when its reservation changes, reduce one `Geometry` input with the
    /// effective viewport in the same update.
    pub(super) fn settle_interaction(&mut self, at: Instant) -> EngineUpdate {
        let mut update = EngineUpdate::default();
        update.invalidated |= self.reconcile_markdown();
        let view = FocusView::new(&self.units, &self.layouts);
        update.invalidated |= self.focus.reconcile(&view);
        if let Some(edge) = self.focus.pending_edge()
            && self.nav.pending_jump.is_none()
            && self.window.summary().in_flight.replace.is_none()
        {
            self.focus.set_pending_edge(None);
            if let Some(target) = view.edge(edge) {
                update.invalidated |= self.focus.focus(&view, target);
            }
        }
        if !self.latest.enabled() {
            return update;
        }
        self.latest
            .observe_latest_known(self.window.summary().latest_known_sequence, at);
        let frame = self.region_frame(at);
        let state = evaluate(
            &LatestBarInputs {
                units: &self.units,
                frame: frame.as_ref(),
                region_height: self.latest.region().height,
                following: self.nav.mode == NavMode::Following,
                window: self.window.summary(),
            },
            self.latest.activity(),
        );
        if state != self.latest.state() {
            update.invalidated = true;
        }
        if self.latest.store(state) {
            update.merge(self.apply_effective_geometry());
        }
        update
    }

    /// Reduce a `Geometry` input with the effective viewport (F6, VP4). A latest-bar change
    /// keeps the width, so nothing is re-measured; a scrollbar gutter change re-measures
    /// resident units at the new content width (L9).
    pub(super) fn apply_effective_geometry(&mut self) -> EngineUpdate {
        let effective = self.content_geometry();
        if effective == self.geometry {
            return EngineUpdate::default();
        }
        let width_changed = effective.width != self.geometry.width;
        self.geometry = effective;
        if width_changed {
            self.remeasure();
        }
        let effects = self.reduce_input(
            NavInput::Geometry {
                viewport: effective,
            },
            &RemovedUnits::new(),
        );
        let mut update = self.run_effects(effects);
        update.invalidated = true;
        update
    }

    /// Visible units at the presented position resolved against the full region (C19).
    fn region_frame(&self, at: Instant) -> Option<ViewportFrame> {
        let position = self.presented(at);
        self.viewport
            .frame(&position, self.latest.region().height, &self.layouts)
    }
}

/// One interactive region of a prepared frame in viewport cells (B3, B7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleHitRegion {
    /// Region clipped to the viewport, in viewport cells.
    pub rect: LogicalRect,
    /// What the region activates.
    pub hit: ItemHit,
}

impl TranscriptEngine {
    /// The retained layout a snapshot painted for a visible unit, while it is still current:
    /// the layout key recorded at preparation, the snapshot width and height, and the unit's
    /// revision all match. Hit testing, hit regions and the focus highlight read only this
    /// layout, so they agree with the painted cells (B3, B7).
    pub(super) fn painted_layout(
        &self,
        snapshot: &FrameSnapshot,
        unit: &VisibleUnit,
    ) -> Option<&ItemLayout> {
        let width = snapshot.geometry.width;
        self.layouts.current(&unit.key).filter(|layout| {
            unit.layout.as_ref() == Some(&layout.key)
                && layout.key.width == width
                && layout.height == unit.height
                && Some(layout.key.revision) == self.units.revision(&unit.key)
        })
    }

    /// Interactive regions of a prepared frame, clipped to the viewport, in viewport cells
    /// (B3, B7). Hosts register them as hit regions; [`Self::hit_test`] resolves points against
    /// the same retained geometry.
    #[must_use]
    pub fn visible_hit_regions(&self, snapshot: &FrameSnapshot) -> Vec<VisibleHitRegion> {
        let Some(viewport) = snapshot.viewport.as_ref() else {
            return Vec::new();
        };
        let geometry = snapshot.geometry;
        let mut regions = Vec::new();
        for unit in &viewport.units {
            let Some(layout) = self.painted_layout(snapshot, unit) else {
                continue;
            };
            for region in &layout.geometry.hit_regions {
                let rows = clip_rows(unit.top, region.rect.y, region.rect.height, geometry.height);
                let x = region.rect.x.min(geometry.width);
                let width = region.rect.width.min(geometry.width - x);
                let Some(rows) = rows.filter(|rows| !rows.is_empty() && width > 0) else {
                    continue;
                };
                regions.push(VisibleHitRegion {
                    rect: LogicalRect::new(x, rows.start, width, rows.end - rows.start),
                    hit: ItemHit {
                        unit: unit.key.clone(),
                        action: region.action.clone(),
                    },
                });
            }
        }
        regions
    }

    /// Viewport rows of the focused target in a prepared frame, clipped to the viewport; `None`
    /// without transcript focus or when the target is not visible (FO7, B12).
    #[must_use]
    pub fn focus_rows(&self, snapshot: &FrameSnapshot) -> Option<Range<u64>> {
        let target = self.focus.target()?;
        let unit = snapshot
            .viewport
            .as_ref()?
            .units
            .iter()
            .find(|unit| unit.key == target.unit)?;
        let layout = self.painted_layout(snapshot, unit)?;
        let rows = &layout
            .geometry
            .focus_targets
            .iter()
            .find(|candidate| candidate.local == target.local)?
            .rows;
        clip_rows(
            unit.top,
            rows.start,
            rows.end.saturating_sub(rows.start),
            snapshot.geometry.height,
        )
        .filter(|rows| !rows.is_empty())
    }
}

/// Viewport rows `start..start + height` of a unit whose first row is at viewport row `top`,
/// clipped to `0..viewport_height`.
fn clip_rows(top: i64, start: u64, height: u64, viewport_height: u64) -> Option<Range<u64>> {
    let first = i128::from(top) + i128::from(start);
    let end = first + i128::from(height);
    let first = u64::try_from(first.max(0)).ok()?;
    let end = u64::try_from(end.max(0)).ok()?.min(viewport_height);
    Some(first.min(end)..end)
}

const fn step_edge(step: FocusStep) -> ApproachEdge {
    match step {
        FocusStep::Next => ApproachEdge::Bottom,
        FocusStep::Previous => ApproachEdge::Top,
    }
}

fn target_starts_visible(
    frame: &ViewportFrame,
    target: &FocusTargetId,
    view: &FocusView<'_>,
) -> bool {
    let height = i64::try_from(frame.resolved.viewport_height).unwrap_or(i64::MAX);
    let Some(rows) = view.rows(target) else {
        return false;
    };
    frame.units.iter().any(|unit| {
        unit.key == target.unit
            && (0..height).contains(
                &unit
                    .top
                    .saturating_add(i64::try_from(rows.start).unwrap_or(i64::MAX)),
            )
    })
}

#[cfg(test)]
mod tests;
