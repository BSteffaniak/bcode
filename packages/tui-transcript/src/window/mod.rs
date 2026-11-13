//! Resident window manager.
//!
//! The window decides which contiguous range of whole turns is resident, reports page and
//! window requests as data for the host to execute, and plans eviction. It performs no I/O.
//!
//! * [`manager`]: the [`WindowManager`] (requests, incorporation, live events, eviction).
//! * [`turns`]: turn alignment of event ranges (W4).
//! * [`loader`]: the bounded history reads every host executes for [`crate::engine::HostRequest`]s.
//!
//! `TranscriptChanges` come from the `SessionView` window operations themselves; no keyed
//! document diff is computed.

pub mod loader;
#[cfg(test)]
mod loader_tests;
pub mod manager;
#[cfg(test)]
mod manager_tests;
pub mod turns;

/// Payload of [`LiveInput::Stream`], re-exported for hosts that transport live inputs.
pub use bcode_session_models::SessionLiveEvent;
pub use bmux_tui_components::resident_window::BoundaryStatus;
pub use bmux_tui_components::resident_window::EdgeRows;
pub use loader::{
    HistoryLane, HistoryLoadError, HistoryLoadRequest, HistoryLoaded, SessionEvent,
    SessionHistoryCursor, SessionHistoryDirection, SessionHistoryPage, SessionHistoryQuery,
    SessionId, load_history,
};
pub use manager::{
    EvictionContext, EvictionOutcome, LiveInput, LiveOutcome, PageIncorporated, StaleResult,
    WindowIncorporated, WindowManager, WindowWork,
};

use crate::units::BoundaryEdge;

/// Resident target size in canonical events after eviction (W7, D1).
pub const RESIDENT_TARGET_EVENTS: u64 = 512;
/// Event count above which eviction runs (W7, D1).
pub const RESIDENT_MAX_EVENTS: u64 = 1024;
/// Events one page or window request may scan (W6, W10, D1).
pub const PAGE_EVENTS: u64 = 512;
/// Hard ceiling: max plus one page (W7a).
pub const RESIDENT_CEILING_EVENTS: u64 = RESIDENT_MAX_EVENTS + PAGE_EVENTS;
/// Minimum prefetch distance in rows (W5).
pub const MIN_PREFETCH_ROWS: u64 = 48;
/// Live events buffered while a `Newer` request is in flight (W11).
pub const LIVE_BUFFER_EVENTS: usize = 512;
/// Bytes of live events buffered while a `Newer` request is in flight (W11).
pub const LIVE_BUFFER_BYTES: usize = 512 * 1024;

/// Prefetch distance for a viewport height: `max(2 × viewport_height, 48)` rows (W5).
#[must_use]
pub const fn prefetch_rows(viewport_height: u64) -> u64 {
    let doubled = viewport_height.saturating_mul(2);
    if doubled > MIN_PREFETCH_ROWS {
        doubled
    } else {
        MIN_PREFETCH_ROWS
    }
}

/// Whether the window contains the session tail (W2).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WindowMode {
    /// The window contains the session tail and live events apply.
    #[default]
    Live,
    /// The window does not contain the tail; live transcript events are not applied.
    Detached,
}

/// Direction of a page relative to the resident window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PageDirection {
    /// Older history, prepended before the window.
    Older,
    /// Newer history, appended after the window (detached windows only).
    Newer,
}

impl PageDirection {
    /// The boundary unit edge on this side of the window.
    #[must_use]
    pub const fn edge(self) -> BoundaryEdge {
        match self {
            Self::Older => BoundaryEdge::Older,
            Self::Newer => BoundaryEdge::Newer,
        }
    }
}

/// Page request executed by the host (W5, W6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PageRequest {
    /// Complete turns with sequences below `before_sequence`.
    Older {
        /// Exclusive upper sequence bound (the window's first resident sequence).
        before_sequence: u64,
    },
    /// Complete turns with sequences above `after_sequence`.
    Newer {
        /// Exclusive lower sequence bound (the window's last resident sequence).
        after_sequence: u64,
    },
}

impl PageRequest {
    /// Direction of the page.
    #[must_use]
    pub const fn direction(self) -> PageDirection {
        match self {
            Self::Older { .. } => PageDirection::Older,
            Self::Newer { .. } => PageDirection::Newer,
        }
    }
}

/// Window replacement request executed by the host (W8, W10, W11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WindowRequest {
    /// Turn-aligned window centered on a sequence (timeline, scrollbar fraction, restore).
    Around {
        /// Target sequence.
        sequence: u64,
    },
    /// Window at the session start.
    Head,
    /// Window at the session tail (also V2 session attach and return to live).
    Tail,
    /// Live-state rehydration only, without replacing the durable window (W11).
    LiveState,
}

/// Which window a [`HistoryWindowResult`] answers; identical in shape to the request.
pub type WindowTarget = WindowRequest;

/// A host request stamped with the window generation it was issued for.
///
/// Results whose generation differs from the window's current generation are dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IssuedRequest<R> {
    /// Window generation at issue time.
    pub generation: u64,
    /// The request.
    pub request: R,
}

/// Per-request caps (D1, W6, W10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PageCaps {
    /// Maximum transcript items.
    pub max_items: usize,
    /// Maximum canonical events scanned.
    pub max_events_scanned: usize,
    /// Maximum payload bytes.
    pub max_bytes: usize,
}

impl PageCaps {
    /// Caps of one `Older`/`Newer` page: 16 items, 512 events, 512 KiB (W6).
    pub const PAGE: Self = Self {
        max_items: 16,
        max_events_scanned: 512,
        max_bytes: 512 * 1024,
    };
    /// Caps of one `Head`/`Tail`/`Around` window: 64 items, 512 events, 512 KiB (W10).
    pub const WINDOW: Self = Self {
        max_items: 64,
        max_events_scanned: 512,
        max_bytes: 512 * 1024,
    };
}

/// Presentation-only row estimate around the window (W12); never used for navigation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct PositionEstimate {
    /// Estimated rows of history before the window.
    pub rows_before_window: u64,
    /// Exact measured rows of the resident window.
    pub resident_rows: u64,
    /// Estimated rows of history after the window.
    pub rows_after_window: u64,
}

/// Requests currently in flight (W5: at most one per direction, one replacement at a time).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct InFlight {
    /// An `Older` page is in flight.
    pub older: bool,
    /// A `Newer` page is in flight.
    pub newer: bool,
    /// A window replacement is in flight.
    pub replace: Option<WindowRequest>,
}

/// Sides whose page demand is refused by the resident ceiling (W7a).
///
/// A blocked side needs rows (prefetch distance or pending movement at the boundary) but
/// `event_count + 512 > 1,536` and eviction could not make room (for example while eviction is
/// deferred). Its boundary unit shows `loading` until eviction frees room (W13).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct CeilingBlocked {
    /// The older side is waiting for room.
    pub older: bool,
    /// The newer side is waiting for room.
    pub newer: bool,
}

/// Result of one page request, delivered by the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryPageResult {
    /// Window generation the request was issued for.
    pub generation: u64,
    /// Page direction.
    pub direction: PageDirection,
    /// Canonical events in ascending sequence order.
    pub events: Vec<SessionEvent>,
    /// Whether more history exists beyond this page in its direction.
    pub has_more: bool,
    /// The page ends mid-turn because one turn exceeds the caps (W4, SD2 fallback).
    pub unaligned: bool,
}

/// Result of one window request, delivered by the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryWindowResult {
    /// Window generation the request was issued for.
    pub generation: u64,
    /// Which window this answers.
    pub target: WindowTarget,
    /// Canonical events in ascending sequence order.
    pub events: Vec<SessionEvent>,
    /// Whether events older than the first returned event exist.
    pub has_older: bool,
    /// Whether events newer than the last returned event exist.
    pub has_newer: bool,
    /// First canonical sequence available in the session, when known.
    pub first_available_sequence: Option<u64>,
    /// Last canonical sequence available in the session, when known.
    pub last_available_sequence: Option<u64>,
}

/// Resident window bookkeeping.
#[derive(Debug, Clone, PartialEq)]
pub struct ResidentWindow {
    /// Live or detached.
    pub mode: WindowMode,
    /// First resident canonical sequence, when any event is resident.
    pub first_sequence: Option<u64>,
    /// Last resident canonical sequence, when any event is resident.
    pub last_sequence: Option<u64>,
    /// Resident canonical events.
    pub event_count: u64,
    /// Older history exists before the window.
    pub has_older: bool,
    /// Newer history exists after the window (detached only).
    pub has_newer: bool,
    /// Requests in flight.
    pub in_flight: InFlight,
    /// Sides waiting for room under the resident ceiling (W7a).
    pub ceiling_blocked: CeilingBlocked,
    /// Generation; bumped by every window replacement. Stale results are dropped.
    pub generation: u64,
    /// First canonical sequence available in the session, when known.
    pub first_available_sequence: Option<u64>,
    /// Last canonical sequence available in the session, when known.
    pub last_available_sequence: Option<u64>,
    /// Newest sequence known to exist (detached windows track it for the latest bar, W2).
    pub latest_known_sequence: Option<u64>,
    /// Exponentially smoothed resident rows per event (W12, initial 1.0).
    pub rows_per_event_estimate: f64,
}

impl Default for ResidentWindow {
    fn default() -> Self {
        Self::new()
    }
}

impl ResidentWindow {
    /// Create an empty live window with nothing resident.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            mode: WindowMode::Live,
            first_sequence: None,
            last_sequence: None,
            event_count: 0,
            has_older: false,
            has_newer: false,
            in_flight: InFlight {
                older: false,
                newer: false,
                replace: None,
            },
            ceiling_blocked: CeilingBlocked {
                older: false,
                newer: false,
            },
            generation: 0,
            first_available_sequence: None,
            last_available_sequence: None,
            latest_known_sequence: None,
            rows_per_event_estimate: 1.0,
        }
    }

    /// Display state of a boundary unit (U3, W13). A side waiting for room under the ceiling
    /// shows `loading` like a side with a page in flight (W7a).
    #[must_use]
    pub const fn boundary_status(&self, edge: BoundaryEdge) -> BoundaryStatus {
        let (in_flight, more) = match edge {
            BoundaryEdge::Older => (
                self.in_flight.older || self.ceiling_blocked.older,
                self.has_older,
            ),
            BoundaryEdge::Newer => (
                self.in_flight.newer || self.ceiling_blocked.newer,
                self.has_newer,
            ),
        };
        if in_flight || self.in_flight.replace.is_some() {
            BoundaryStatus::Loading
        } else if more {
            BoundaryStatus::MoreAvailable
        } else {
            BoundaryStatus::Exhausted
        }
    }

    /// Whether the session start is resident.
    #[must_use]
    pub const fn contains_head(&self) -> bool {
        !self.has_older
    }

    /// Whether the session tail is resident (`Live`, W2). The resident bottom of a detached
    /// window is never the session tail (W13, C31).
    #[must_use]
    pub const fn contains_tail(&self) -> bool {
        matches!(self.mode, WindowMode::Live)
    }

    /// Whether a canonical sequence lies inside the resident range.
    #[must_use]
    pub const fn contains_sequence(&self, sequence: u64) -> bool {
        match (self.first_sequence, self.last_sequence) {
            (Some(first), Some(last)) => first <= sequence && sequence <= last,
            _ => false,
        }
    }

    /// Newest canonical sequence known to exist in the session: the last available sequence,
    /// or a newer one a detached window observed live (W2).
    #[must_use]
    pub fn newest_known_sequence(&self) -> Option<u64> {
        self.last_available_sequence.max(self.latest_known_sequence)
    }

    /// Presentation-only position estimate for `resident_rows` measured rows (W12).
    ///
    /// Rows before and after the window are the non-resident events on each side times the
    /// smoothed rows-per-event average; a detached window counts the newest sequence it
    /// observed live. Never used for navigation correctness.
    #[must_use]
    pub fn position_estimate(&self, resident_rows: u64) -> PositionEstimate {
        let rate = self.rows_per_event_estimate.max(0.0);
        let estimate = |events: u64| -> u64 {
            #[allow(
                clippy::cast_precision_loss,
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss
            )]
            let rows = (events as f64 * rate).round() as u64;
            rows
        };
        let before = match (self.first_available_sequence, self.first_sequence) {
            (Some(available), Some(first)) => first.saturating_sub(available),
            _ => 0,
        };
        let after = match (self.newest_known_sequence(), self.last_sequence) {
            (Some(available), Some(last)) => available.saturating_sub(last),
            _ => 0,
        };
        PositionEstimate {
            rows_before_window: estimate(before),
            resident_rows,
            rows_after_window: estimate(after),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefetch_distance_has_floor_and_scales_with_viewport() {
        assert_eq!(prefetch_rows(0), 48);
        assert_eq!(prefetch_rows(24), 48);
        assert_eq!(prefetch_rows(40), 80);
    }

    #[test]
    fn boundary_status_reflects_availability_and_loading() {
        let mut window = ResidentWindow::new();
        assert_eq!(
            window.boundary_status(BoundaryEdge::Older),
            BoundaryStatus::Exhausted
        );
        window.has_older = true;
        assert_eq!(
            window.boundary_status(BoundaryEdge::Older),
            BoundaryStatus::MoreAvailable
        );
        window.in_flight.older = true;
        assert_eq!(
            window.boundary_status(BoundaryEdge::Older),
            BoundaryStatus::Loading
        );
    }

    #[test]
    fn detached_bottom_is_not_the_session_tail() {
        let mut window = ResidentWindow::new();
        assert!(window.contains_tail());
        window.mode = WindowMode::Detached;
        assert!(!window.contains_tail());
    }

    #[test]
    fn estimate_extrapolates_non_resident_events() {
        let window = ResidentWindow {
            first_sequence: Some(100),
            last_sequence: Some(200),
            first_available_sequence: Some(0),
            last_available_sequence: Some(300),
            rows_per_event_estimate: 2.0,
            ..ResidentWindow::new()
        };
        assert_eq!(
            window.position_estimate(50),
            PositionEstimate {
                rows_before_window: 200,
                resident_rows: 50,
                rows_after_window: 200,
            }
        );
    }
}
