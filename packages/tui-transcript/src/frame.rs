//! Frame composition (spec sections 3.6 and 9, rules F1–F7).
//!
//! Identifiers, the per-frame clock and the frame snapshot. The clock is the BMUX presentation
//! clock (BMUX 8): one instant sampled per presentation and handed to every layout and paint
//! context of the frame.

use std::time::Instant;

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
/// Phase 2 minimal snapshot: the keyed position presented at the frame clock, the rows it
/// resolves to, and the visible units. Painting reads only the snapshot and the engine's
/// retained layouts (F3); preparing a snapshot never changes navigation (CK1, CK2).
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
    /// Latest-bar decision (phase 4; never visible before then).
    pub latest_bar: crate::latest_bar::LatestBarState,
}
