//! Frame composition.
//!
//! Identifiers, the per-frame clock and the frame snapshot. The clock is the BMUX presentation
//! clock (BMUX 8): one instant sampled per presentation and handed to every layout and paint
//! context of the frame.

use web_time::Instant;

use bmux_tui::clock::PresentationClock;

/// Identity of one prepared frame; recorded on commit (CK3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FrameId(pub u64);

impl FrameId {
    /// The next frame id.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }
}

/// The single clock sample of one frame (F2).
///
/// It converts to and from the BMUX [`PresentationClock`] (BMUX 8): the host samples the
/// runtime's presentation clock once per presentation, prepares the frame with it, and the
/// engine passes the same instant to BMUX layout and paint contexts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FrameClock {
    /// Frame time.
    pub now: Instant,
}

impl FrameClock {
    /// The BMUX presentation clock carrying this frame's instant.
    #[must_use]
    pub const fn presentation(self) -> PresentationClock {
        PresentationClock::new(self.now)
    }
}

impl From<PresentationClock> for FrameClock {
    fn from(clock: PresentationClock) -> Self {
        Self { now: clock.now() }
    }
}

/// Everything one frame presents, prepared once from engine state (F1).
///
/// The snapshot carries the keyed position presented at the frame clock, the rows it resolves
/// to with the visible units and the retained layouts they paint, the latest-bar decision, the
/// scrollbar track, the focus highlight rows, the interactive regions, the selection and the
/// accepted activity phase. Preparing a snapshot reads derived caches only and never changes
/// navigation (CK1, CK2); painting reads only the snapshot and the retained layouts it names,
/// so cells, hits, focus, selection, images and damage come from the same nodes (F3, F4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameSnapshot {
    /// Frame identity, recorded on commit (CK3).
    pub id: FrameId,
    /// The frame's single clock sample (F2).
    pub clock: FrameClock,
    /// Transcript viewport geometry.
    pub geometry: crate::viewport::ViewportGeometry,
    /// Keyed position presented in this frame (a running motion is sampled at `clock`).
    pub position: crate::navigation::ListPosition,
    /// Rows and visible units derived from `position`; `None` when nothing is resident.
    pub viewport: Option<crate::viewport::ViewportFrame>,
    /// Whether navigation follows the latest bottom with no motion running.
    pub following: bool,
    /// Whether a scroll motion is running.
    pub motion_running: bool,
    /// Latest-bar decision.
    pub latest_bar: crate::latest_bar::LatestBarState,
    /// Presented scrollbar track in viewport cells, if any (VP4, X6).
    pub scrollbar_track: Option<bmux_tui::geometry::Rect>,
    /// Focused transcript target (FO7).
    pub focus: Option<crate::focus::FocusTargetId>,
    /// Viewport rows of the focus highlight, clipped to the viewport (FO7, B12).
    pub focus_rows: Option<std::ops::Range<u64>>,
    /// Interactive regions of the visible units in viewport cells (B3, B7).
    pub hit_regions: Vec<crate::engine::VisibleHitRegion>,
    /// Transcript selection painted in this frame (C34).
    pub selection: Option<crate::selection::TranscriptSelection>,
    /// Spinner and elapsed phase the frame paints (F7, D7).
    pub activity: crate::layout::ActivityPhase,
}

impl FrameSnapshot {
    /// Rows the viewport content moved since `previous` when the only change between the two
    /// frames is the scroll position (F5, BMUX 11): same geometry, latest bar, scrollbar,
    /// focus, selection and activity, and every unit visible in both frames painted from the
    /// same retained layout and shifted by the same rows. Positive rows move content toward
    /// row 0 (the reader scrolled down), as `Damage::Scroll` counts them. `None` for any other
    /// change, for no movement, and for shifts of the whole viewport or more.
    #[must_use]
    pub fn scrolled_from(&self, previous: &Self) -> Option<i64> {
        if self.geometry != previous.geometry
            || self.latest_bar != previous.latest_bar
            || self.scrollbar_track != previous.scrollbar_track
            || self.focus != previous.focus
            || self.selection != previous.selection
            || self.activity != previous.activity
        {
            return None;
        }
        let (current, earlier) = (self.viewport.as_ref()?, previous.viewport.as_ref()?);
        let mut shift = None;
        for unit in &current.units {
            let Some(old) = earlier.units.iter().find(|old| old.key == unit.key) else {
                continue;
            };
            if old.layout != unit.layout || old.height != unit.height {
                return None;
            }
            let moved = old.top.checked_sub(unit.top)?;
            match shift {
                None => shift = Some(moved),
                Some(rows) if rows == moved => {}
                Some(_) => return None,
            }
        }
        let rows = shift.filter(|rows| *rows != 0)?;
        (rows.unsigned_abs() < self.geometry.height).then_some(rows)
    }
}
