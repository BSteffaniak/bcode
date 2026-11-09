//! Viewport: a controlled BMUX virtual list projected from navigation (spec section 3.4,
//! rules VP1–VP7).
//!
//! The viewport stores no navigation policy. Before every layout it writes the BMUX position
//! derived from `NavState` and never reads the list's position back (VP5). List input only
//! queues BMUX actions, which [`TranscriptViewport::drain_outputs`] translates into navigation
//! intents. The type mapping is one way, Bcode policy types to BMUX mechanics (VP7).
//!
//! Unit membership and height changes reach the list's `MeasuredListIndex` as keyed splices
//! with caller-measured exact heights ([`TranscriptViewport::apply_unit_changes`]), in the same
//! update that changed the units (W3, VP6). Rows are always derived from a keyed position and
//! the current geometry ([`TranscriptViewport::frame`], [`TranscriptViewport::edge_rows`]).

pub mod source;

use std::cell::{Ref, RefCell};

use bmux_tui_components::scrollbar_layout::ScrollbarAxisLayoutMode;
use bmux_tui_components::virtual_list::{
    self as vl, AnchorPolicy, Edge, EdgeRows, ListGeometry, MeasuredListError, MeasuredListIndex,
    MeasuredListItem, Overscroll, PageStep, PositionControl, ResolvedPosition, ScrollInputSource,
    SourceMap, VirtualItemSource, VirtualListAction, VirtualListChrome, VirtualListComponent,
    VirtualListState,
};

use crate::layout::SourcePosition;
use crate::layout::cache::ItemLayoutKey;
use crate::navigation::{
    AnchorOffset, ApproachEdge, InputSource, ListPosition, MotionConfig, NavIntent, NavView,
    PAGE_STEP_ROWS, Placement, ReadingAnchor, ScrollMotion, ScrollTarget, UnitFacts,
};
use crate::units::{UnitChanges, UnitKey, Units};
use crate::window::ResidentWindow;

/// BMUX list position over transcript units.
pub type BmuxListPosition = vl::ListPosition<UnitKey, SourcePosition>;
/// BMUX list anchor over transcript units.
pub type BmuxListAnchor = vl::ListAnchor<UnitKey, SourcePosition>;
/// BMUX scroll target over transcript units.
pub type BmuxScrollTarget = vl::ScrollTarget<UnitKey, SourcePosition>;
/// BMUX scroll motion over transcript units.
pub type BmuxScrollMotion = vl::ScrollMotion<UnitKey, SourcePosition>;
/// Controlled BMUX list state over transcript units.
pub type TranscriptListState = VirtualListState<UnitKey, SourcePosition>;

/// Tail space: at most `viewport_height - 1` blank rows below content (VP2, ambiguity 8).
pub const TAIL_OVERSCROLL: Overscroll = Overscroll::ViewportMinus(1);

/// Transcript viewport geometry in cells.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct ViewportGeometry {
    /// Content width in cells.
    pub width: u64,
    /// Transcript region height in rows (after any latest-bar reservation, F6).
    pub height: u64,
}

impl ViewportGeometry {
    /// Create a geometry.
    #[must_use]
    pub const fn new(width: u64, height: u64) -> Self {
        Self { width, height }
    }
}

/// BMUX list geometry for a viewport height with the tail-space overscroll (VP2).
#[must_use]
pub const fn list_geometry(viewport_height: u64) -> ListGeometry {
    ListGeometry::new(viewport_height).overscroll(TAIL_OVERSCROLL)
}

/// Map a Bcode anchor offset to its BMUX equivalent (VP7).
#[must_use]
pub fn bmux_offset(offset: &AnchorOffset) -> vl::AnchorOffset<SourcePosition> {
    match offset {
        AnchorOffset::Row(row) => vl::AnchorOffset::Row(*row),
        AnchorOffset::Source {
            position,
            row_delta,
        } => vl::AnchorOffset::Source {
            position: position.clone(),
            row_delta: *row_delta,
        },
    }
}

/// Map a Bcode reading anchor to a BMUX list anchor (VP7).
#[must_use]
pub fn bmux_anchor(anchor: &ReadingAnchor) -> BmuxListAnchor {
    vl::ListAnchor::new(anchor.unit.clone(), bmux_offset(&anchor.offset))
}

/// Map a Bcode position to a BMUX position (VP7): `FollowBottom` → `FollowBottom`,
/// `Anchored` → `Anchored`, `TailSpace` → `Overscrolled`.
#[must_use]
pub fn bmux_position(position: &ListPosition) -> BmuxListPosition {
    match position {
        ListPosition::FollowBottom => vl::ListPosition::FollowBottom,
        ListPosition::Anchored { anchor } => vl::ListPosition::Anchored(bmux_anchor(anchor)),
        ListPosition::TailSpace { anchor } => vl::ListPosition::Overscrolled(bmux_anchor(anchor)),
    }
}

/// Map a Bcode scroll target to a BMUX scroll target (VP7): `ItemTop(k)` →
/// `Key(k, ItemTopAtTop)`, `ItemBottom(k)` → `Key(k, ItemBottomAtBottom)`, `LatestBottom` →
/// `Bottom`, `Anchor(a)` → `Anchor(a)`.
#[must_use]
pub fn bmux_target(target: &ScrollTarget) -> BmuxScrollTarget {
    match target {
        ScrollTarget::ItemTop(key) => {
            vl::ScrollTarget::Key(key.clone(), AnchorPolicy::ItemTopAtTop)
        }
        ScrollTarget::ItemBottom(key) => {
            vl::ScrollTarget::Key(key.clone(), AnchorPolicy::ItemBottomAtBottom)
        }
        ScrollTarget::LatestBottom => vl::ScrollTarget::Bottom,
        ScrollTarget::Anchor(anchor) => vl::ScrollTarget::Anchor(bmux_anchor(anchor)),
    }
}

/// Map a placement (and, for `EnsureVisible`, the approach edge) to a BMUX anchor policy.
#[must_use]
pub const fn bmux_policy(placement: Placement, edge: ApproachEdge) -> AnchorPolicy {
    match placement {
        Placement::ItemTopAtTop => AnchorPolicy::ItemTopAtTop,
        Placement::ItemBottomAtBottom => AnchorPolicy::ItemBottomAtBottom,
        Placement::Center => AnchorPolicy::Center,
        Placement::EnsureVisible => AnchorPolicy::EnsureVisibleFrom(match edge {
            ApproachEdge::Top => Edge::Top,
            ApproachEdge::Bottom => Edge::Bottom,
        }),
    }
}

/// Map a Bcode motion to a BMUX motion for sampling (VP7). The keyed start is an overscroll
/// position so a start inside tail space samples where it is drawn.
#[must_use]
pub fn bmux_motion(motion: &ScrollMotion) -> BmuxScrollMotion {
    vl::ScrollMotion::new(
        vl::ListPosition::Overscrolled(bmux_anchor(&motion.from)),
        bmux_target(&motion.target),
        motion.easing,
        motion.duration,
        motion.started,
    )
}

/// Output of list input after translation.
#[derive(Debug, Clone, PartialEq)]
pub enum ViewportOutput {
    /// A navigation intent for the reducer.
    Navigate(NavIntent),
    /// A click on a unit that did not handle it.
    Activated(UnitKey),
}

/// Translate one queued BMUX list action (VP5). Unknown future actions are dropped.
#[must_use]
pub fn translate_action(action: VirtualListAction<UnitKey>) -> Option<ViewportOutput> {
    let intent = match action {
        VirtualListAction::ScrollRequested { rows, source } => NavIntent::ScrollBy {
            rows,
            source: match source {
                ScrollInputSource::Wheel => InputSource::Wheel,
                ScrollInputSource::Key => InputSource::Key,
            },
        },
        VirtualListAction::PageRequested(pages) => NavIntent::PageBy { pages },
        VirtualListAction::EdgeRequested(Edge::Top) => NavIntent::JumpToSessionStart,
        VirtualListAction::EdgeRequested(Edge::Bottom) => NavIntent::JumpToLatest,
        VirtualListAction::ScrollbarDrag(fraction) => NavIntent::JumpToFraction { fraction },
        VirtualListAction::SelectionAutoScroll(rows) => NavIntent::ScrollBy {
            rows,
            source: InputSource::SelectionAutoScroll,
        },
        VirtualListAction::Activated(key) => return Some(ViewportOutput::Activated(key)),
        _ => return None,
    };
    Some(ViewportOutput::Navigate(intent))
}

/// One unit intersecting the viewport, derived from the keyed position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleUnit {
    /// Unit identity.
    pub key: UnitKey,
    /// Position in document order.
    pub index: usize,
    /// Viewport row of the unit's first row (negative when it starts above the viewport).
    pub top: i64,
    /// Exact height in rows.
    pub height: u64,
    /// Key of the retained layout the frame paints for this unit, recorded when the frame is
    /// prepared; hit testing against the frame uses only that layout (B3, B7).
    pub layout: Option<ItemLayoutKey>,
}

/// Rows of one frame derived from a keyed position and the current geometry (P1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewportFrame {
    /// Resolved offsets.
    pub resolved: ResolvedPosition,
    /// Units intersecting the viewport, top to bottom.
    pub units: Vec<VisibleUnit>,
}

impl ViewportFrame {
    /// Blank tail-space rows below the last content row (C2, C3).
    #[must_use]
    pub const fn blank_rows_below(&self) -> u64 {
        self.resolved.blank_rows_below()
    }

    /// Whether a unit is fully inside the viewport.
    #[must_use]
    pub fn fully_visible(&self, key: &UnitKey) -> bool {
        let height = i64::try_from(self.resolved.viewport_height).unwrap_or(i64::MAX);
        self.units.iter().any(|unit| {
            &unit.key == key
                && unit.top >= 0
                && unit
                    .top
                    .saturating_add(i64::try_from(unit.height).unwrap_or(i64::MAX))
                    <= height
        })
    }
}

/// Scrollbar presentation of the transcript list (VP4).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ScrollbarMode {
    /// No scrollbar (default, chrome `scrollbar = off`).
    #[default]
    Off,
    /// Scrollbar drawn over content while it overflows.
    Overlay,
    /// Scrollbar in a reserved one-column gutter.
    Gutter,
}

/// List chrome for the transcript: wheel input, Page Up/Down step of 10 rows (11-P2), no list
/// keyboard handling (keys reach navigation through the host keymap), optional scrollbar.
#[must_use]
pub const fn transcript_chrome(scrollbar: ScrollbarMode) -> VirtualListChrome {
    let mode = match scrollbar {
        ScrollbarMode::Off => ScrollbarAxisLayoutMode::Hidden,
        ScrollbarMode::Overlay => ScrollbarAxisLayoutMode::OverlayOnOverflow,
        ScrollbarMode::Gutter => ScrollbarAxisLayoutMode::Gutter,
    };
    VirtualListChrome::interactive()
        .scrollbar(mode)
        .keyboard(false)
        .page(PageStep::Rows(PAGE_STEP_ROWS))
}

/// Controlled BMUX list state for the transcript (VP5, VP6).
#[derive(Debug)]
pub struct TranscriptViewport {
    state: RefCell<TranscriptListState>,
}

impl Default for TranscriptViewport {
    fn default() -> Self {
        Self::new()
    }
}

impl TranscriptViewport {
    /// Create an empty controlled viewport with tail-space overscroll.
    #[must_use]
    pub fn new() -> Self {
        let mut state = TranscriptListState::with_control(0, PositionControl::Controlled);
        state.set_overscroll(TAIL_OVERSCROLL);
        Self {
            state: RefCell::new(state),
        }
    }

    /// The list state, for `VirtualListComponent`.
    #[must_use]
    pub const fn state(&self) -> &RefCell<TranscriptListState> {
        &self.state
    }

    /// Mutable access to the list state (index splices, invalidations).
    pub fn state_mut(&mut self) -> &mut TranscriptListState {
        self.state.get_mut()
    }

    /// Write the BMUX position derived from navigation (VP5).
    pub fn apply_position(&mut self, position: &ListPosition) {
        self.state.get_mut().set_position(bmux_position(position));
    }

    /// Drain queued list actions as translated outputs (VP5).
    pub fn drain_outputs(&mut self) -> Vec<ViewportOutput> {
        self.state
            .get_mut()
            .drain_actions()
            .filter_map(translate_action)
            .collect()
    }

    /// The measured index (unit order, exact heights, offsets).
    #[must_use]
    pub fn index(&self) -> Ref<'_, MeasuredListIndex<UnitKey>> {
        Ref::map(self.state.borrow(), TranscriptListState::index)
    }

    /// Apply unit changes to the measured index incrementally (U4, L1, VP6).
    ///
    /// Splices are applied in order with exact heights from `measure` at `width`; revised
    /// units are re-measured in place. Units outside the change keep their measurements. The
    /// list position is not touched (VP5).
    ///
    /// # Errors
    ///
    /// * [`MeasuredListError::OutOfBounds`] if a splice does not match the index (the index
    ///   is then out of sync with the units; callers rebuild with [`Self::rebuild`]).
    /// * [`MeasuredListError::DuplicateKey`] if a splice inserts a key that stays resident.
    pub fn apply_unit_changes(
        &mut self,
        changes: &UnitChanges,
        units: &Units,
        width: u64,
        measure: &mut dyn FnMut(&UnitKey) -> u64,
    ) -> Result<(), MeasuredListError> {
        let state = self.state.get_mut();
        for splice in &changes.splices {
            let items = splice
                .inserted
                .iter()
                .map(|key| measured(units, key, width, measure))
                .collect::<Vec<_>>();
            let _ = state.splice_measured(splice.removed_range(), items)?;
        }
        for key in &changes.revised {
            if state.index().contains_key(key) {
                let _ = state.update_measured(measured(units, key, width, measure));
            }
        }
        Ok(())
    }

    /// Replace the whole index with every unit measured at `width` (initial sync or recovery
    /// after an inconsistent index, D3).
    ///
    /// # Errors
    ///
    /// * [`MeasuredListError::DuplicateKey`] if `units` repeats a key (never for derived
    ///   units).
    pub fn rebuild(
        &mut self,
        units: &Units,
        width: u64,
        measure: &mut dyn FnMut(&UnitKey) -> u64,
    ) -> Result<(), MeasuredListError> {
        let state = self.state.get_mut();
        let len = state.index().len();
        let items = units
            .keys()
            .iter()
            .map(|key| measured(units, key, width, measure))
            .collect::<Vec<_>>();
        state.splice_measured(0..len, items).map(|_| ())
    }

    /// Re-measure every resident unit for a new width (L9). Keyed anchors survive (P1, P3).
    ///
    /// # Errors
    ///
    /// Same as [`Self::rebuild`].
    pub fn remeasure(
        &mut self,
        units: &Units,
        width: u64,
        measure: &mut dyn FnMut(&UnitKey) -> u64,
    ) -> Result<(), MeasuredListError> {
        self.rebuild(units, width, measure)
    }

    /// Resolve a keyed position against the index (P1). `None` when its anchor unit is not
    /// resident.
    #[must_use]
    pub fn resolve(
        &self,
        position: &ListPosition,
        viewport_height: u64,
        sources: &dyn SourceMap<UnitKey, SourcePosition>,
    ) -> Option<ResolvedPosition> {
        vl::resolve_position(
            self.state.borrow().index(),
            &bmux_position(position),
            list_geometry(viewport_height),
            sources,
        )
    }

    /// Visible units and rows for a keyed position (P1, VP3).
    #[must_use]
    pub fn frame(
        &self,
        position: &ListPosition,
        viewport_height: u64,
        sources: &dyn SourceMap<UnitKey, SourcePosition>,
    ) -> Option<ViewportFrame> {
        let resolved = self.resolve(position, viewport_height, sources)?;
        let state = self.state.borrow();
        let index = state.index();
        let range = index.visible_range(resolved.top_offset, viewport_height);
        let top = i128::from(resolved.top_offset);
        let units = (range.start..range.end)
            .filter_map(|position| {
                let item = index.item(position)?;
                let offset = index.item_offset(position)?;
                Some(VisibleUnit {
                    key: item.key.clone(),
                    index: position,
                    top: i64::try_from(i128::from(offset) - top).unwrap_or(i64::MIN),
                    height: item.height,
                    layout: None,
                })
            })
            .collect();
        Some(ViewportFrame { resolved, units })
    }

    /// Exact resident rows beyond each viewport edge for prefetch (W5). Tail space counts as
    /// zero rows after. An unresolvable position reports zero rows on both sides.
    #[must_use]
    pub fn edge_rows(
        &self,
        position: &ListPosition,
        viewport_height: u64,
        sources: &dyn SourceMap<UnitKey, SourcePosition>,
    ) -> EdgeRows {
        self.resolve(position, viewport_height, sources)
            .map_or_else(EdgeRows::default, ResolvedPosition::edge_rows)
    }

    /// Run `read` with the read-only view the navigation reducer consults (spec 4.2).
    pub fn with_nav_view<R>(
        &self,
        context: NavViewContext<'_>,
        read: impl FnOnce(&NavView<'_>) -> R,
    ) -> R {
        let state = self.state.borrow();
        let view = NavView {
            index: state.index(),
            sources: context.sources,
            facts: context.facts,
            viewport: context.geometry,
            window: context.window,
            motion: context.motion,
        };
        read(&view)
    }

    /// The BMUX list component over `source` (VP3–VP6). The caller writes the position with
    /// [`Self::apply_position`] before layout.
    #[must_use]
    pub fn component<'a, Src>(
        &'a self,
        id: &'static str,
        source: &'a Src,
        sources: &'a dyn SourceMap<UnitKey, SourcePosition>,
        scrollbar: ScrollbarMode,
    ) -> VirtualListComponent<'a, UnitKey, Src, SourcePosition>
    where
        Src: VirtualItemSource<UnitKey> + ?Sized,
    {
        VirtualListComponent::new(id, source, &self.state)
            .chrome(transcript_chrome(scrollbar))
            .sources(sources)
    }
}

/// Inputs of [`TranscriptViewport::with_nav_view`] that do not live in the viewport.
#[derive(Clone, Copy)]
pub struct NavViewContext<'a> {
    /// Source mapping for source-backed anchors.
    pub sources: &'a dyn SourceMap<UnitKey, SourcePosition>,
    /// Unit facts (roles, sequences, removal correspondence).
    pub facts: &'a dyn UnitFacts,
    /// Transcript viewport geometry.
    pub geometry: ViewportGeometry,
    /// Resident window summary.
    pub window: &'a ResidentWindow,
    /// Motion configuration.
    pub motion: MotionConfig,
}

impl std::fmt::Debug for NavViewContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NavViewContext")
            .field("geometry", &self.geometry)
            .field("window", &self.window)
            .field("motion", &self.motion)
            .finish_non_exhaustive()
    }
}

fn measured(
    units: &Units,
    key: &UnitKey,
    width: u64,
    measure: &mut dyn FnMut(&UnitKey) -> u64,
) -> MeasuredListItem<UnitKey> {
    MeasuredListItem::new(
        key.clone(),
        units.revision(key).unwrap_or(0),
        width,
        0,
        measure(key),
    )
}

#[cfg(test)]
mod splice_tests;

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use bcode_session_view_models::TranscriptViewItemId;
    use bmux_tui::style::Easing;

    use super::*;

    fn unit(id: &str) -> UnitKey {
        UnitKey::Item(TranscriptViewItemId::new(id))
    }

    #[test]
    fn positions_map_one_to_one_onto_bmux_positions() {
        let anchor = ReadingAnchor::row(unit("a"), 3);
        assert_eq!(
            bmux_position(&ListPosition::FollowBottom),
            vl::ListPosition::FollowBottom
        );
        assert_eq!(
            bmux_position(&ListPosition::Anchored {
                anchor: anchor.clone()
            }),
            vl::ListPosition::Anchored(vl::ListAnchor::row(unit("a"), 3))
        );
        assert_eq!(
            bmux_position(&ListPosition::TailSpace { anchor }),
            vl::ListPosition::Overscrolled(vl::ListAnchor::row(unit("a"), 3))
        );
    }

    #[test]
    fn source_offsets_keep_their_position() {
        let anchor = ReadingAnchor {
            unit: unit("a"),
            offset: AnchorOffset::Source {
                position: SourcePosition::MarkdownByte(42),
                row_delta: 1,
            },
        };
        assert_eq!(
            bmux_anchor(&anchor).offset,
            vl::AnchorOffset::Source {
                position: SourcePosition::MarkdownByte(42),
                row_delta: 1
            }
        );
    }

    #[test]
    fn targets_and_placements_map_to_bmux_policies() {
        assert_eq!(
            bmux_target(&ScrollTarget::ItemTop(unit("a"))),
            vl::ScrollTarget::Key(unit("a"), AnchorPolicy::ItemTopAtTop)
        );
        assert_eq!(
            bmux_target(&ScrollTarget::ItemBottom(unit("a"))),
            vl::ScrollTarget::Key(unit("a"), AnchorPolicy::ItemBottomAtBottom)
        );
        assert_eq!(
            bmux_target(&ScrollTarget::LatestBottom),
            vl::ScrollTarget::Bottom
        );
        assert_eq!(
            bmux_policy(Placement::EnsureVisible, ApproachEdge::Bottom),
            AnchorPolicy::EnsureVisibleFrom(Edge::Bottom)
        );
        assert_eq!(
            bmux_policy(Placement::Center, ApproachEdge::Top),
            AnchorPolicy::Center
        );
    }

    #[test]
    fn list_actions_translate_into_intents() {
        assert_eq!(
            translate_action(VirtualListAction::ScrollRequested {
                rows: -3,
                source: ScrollInputSource::Wheel
            }),
            Some(ViewportOutput::Navigate(NavIntent::ScrollBy {
                rows: -3,
                source: InputSource::Wheel
            }))
        );
        assert_eq!(
            translate_action(VirtualListAction::EdgeRequested(Edge::Bottom)),
            Some(ViewportOutput::Navigate(NavIntent::JumpToLatest))
        );
        assert_eq!(
            translate_action(VirtualListAction::Activated(unit("a"))),
            Some(ViewportOutput::Activated(unit("a")))
        );
    }

    #[test]
    fn tail_space_allows_all_but_one_viewport_row() {
        let geometry = list_geometry(10);
        // 30 content rows: content top 20, overscroll top 29 (last row on the viewport top).
        assert_eq!(geometry.max_content_top(30), 20);
        assert_eq!(geometry.max_overscroll_top(30), 29);
    }

    #[test]
    fn controlled_viewport_takes_written_position_and_reports_no_actions() {
        let mut viewport = TranscriptViewport::new();
        assert!(viewport.state().borrow().is_controlled());
        let position = ListPosition::Anchored {
            anchor: ReadingAnchor::top(unit("a")),
        };
        viewport.apply_position(&position);
        assert_eq!(
            viewport.state().borrow().position(),
            bmux_position(&position)
        );
        assert_eq!(viewport.drain_outputs(), Vec::new());
    }

    #[test]
    fn motion_maps_from_anchor_to_keyed_target() {
        let started = Instant::now();
        let motion = ScrollMotion {
            from: ReadingAnchor::top(unit("a")),
            target: ScrollTarget::LatestBottom,
            started,
            duration: Duration::from_millis(100),
            easing: Easing::Linear,
        };
        let mapped = bmux_motion(&motion);
        assert_eq!(
            mapped.from,
            vl::ListPosition::Overscrolled(vl::ListAnchor::top(unit("a")))
        );
        assert_eq!(mapped.target, vl::ScrollTarget::Bottom);
        assert_eq!(mapped.deadline(), started + Duration::from_millis(100));
    }
}
