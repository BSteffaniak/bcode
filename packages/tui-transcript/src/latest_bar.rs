//! Latest-content bar (spec rules F6, C18–C21).
//!
//! Visibility is a pure function of units, the measured index (through the viewport frame),
//! navigation state, latest activity and the full transcript-region height ([`evaluate`]). The
//! engine evaluates it at the end of every update, after the reducer ran, and when the decision
//! changes it reduces one `Geometry` input with the effective viewport (region height minus the
//! bar row, [`LatestBar::effective`]). The decision is measured against the full region, so the
//! reservation cannot cause its own visibility (C19). Frame preparation only copies the stored
//! decision. Activity code never emits navigation (N9).

use std::time::{Duration, Instant};

use crate::units::Units;
use crate::viewport::{ViewportFrame, ViewportGeometry};
use crate::window::{ResidentWindow, WindowMode};

/// Activity older than this no longer adds to the glow burst (the full-motion glow window, C21).
pub const ACTIVITY_BURST_WINDOW: Duration = Duration::from_millis(630);

/// Largest glow burst level.
pub const MAX_ACTIVITY_BURST: u8 = 8;

/// Smallest region height that shows the bar: one transcript row must remain (narrow-layout
/// degradation).
pub const MIN_REGION_HEIGHT: u64 = 2;

/// Latest-bar presentation state for one frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct LatestBarState {
    /// Whether the bar is shown.
    pub visible: bool,
    /// Blank tail-space rows below latest content ("N rows below latest", C2).
    pub rows_below_latest: u64,
    /// When hidden content last regenerated while the bar was shown (C20); drives the glow.
    pub activity_at: Option<Instant>,
    /// Glow intensity, `1..=8` while activity is recent, `0` without activity (C21).
    pub burst: u8,
}

impl LatestBarState {
    /// Rows the bar reserves at the bottom of the transcript region.
    #[must_use]
    pub const fn reserved_rows(self) -> u64 {
        if self.visible { 1 } else { 0 }
    }

    /// Age of the latest activity at `now`, when it is younger than `window`. A zero `window`
    /// (reduced motion) never glows: the bar is shown still at once (C21, C22).
    #[must_use]
    pub fn active_age(self, now: Instant, window: Duration) -> Option<Duration> {
        if !self.visible || window.is_zero() {
            return None;
        }
        let age = now.saturating_duration_since(self.activity_at?);
        (age < window).then_some(age)
    }
}

/// What [`evaluate`] reads (spec F6).
#[derive(Debug, Clone, Copy)]
pub struct LatestBarInputs<'a> {
    /// Current units in document order.
    pub units: &'a Units,
    /// Units intersecting the viewport at the presented position, resolved against the full
    /// transcript-region height (never the effective viewport, C19).
    pub frame: Option<&'a ViewportFrame>,
    /// Full transcript-region height, bar row included.
    pub region_height: u64,
    /// Navigation follows the latest bottom.
    pub following: bool,
    /// Resident window summary (a detached window has newer history beyond it, C31).
    pub window: &'a ResidentWindow,
}

/// Whether newer content than the visible region exists (C18).
///
/// It does when a distinct newer unit lies entirely below the region, or the resident window
/// is detached from newer history. Rows of the last visible unit that continue below the
/// region are not newer content.
#[must_use]
pub fn newer_content_below(inputs: &LatestBarInputs<'_>) -> bool {
    if inputs.following {
        return false;
    }
    if inputs.window.mode == WindowMode::Detached && inputs.window.has_newer {
        return true;
    }
    let Some(last) = inputs.frame.and_then(|frame| frame.units.last()) else {
        return false;
    };
    let keys = inputs.units.keys();
    keys[last.index.saturating_add(1).min(keys.len())..]
        .iter()
        .any(|key| !key.is_boundary())
}

/// The latest-bar decision for one update (F6, C18, C19): visible exactly when newer content
/// is below and the region keeps a transcript row; activity is kept only while visible.
#[must_use]
pub fn evaluate(inputs: &LatestBarInputs<'_>, activity: LatestActivity) -> LatestBarState {
    let visible = inputs.region_height >= MIN_REGION_HEIGHT && newer_content_below(inputs);
    LatestBarState {
        visible,
        rows_below_latest: inputs.frame.map_or(0, ViewportFrame::blank_rows_below),
        activity_at: activity.at.filter(|_| visible),
        burst: if visible { activity.burst } else { 0 },
    }
}

/// Hidden activity accepted so far (C20).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct LatestActivity {
    /// Time of the latest accepted activity.
    pub at: Option<Instant>,
    /// Glow burst level (`0` without activity).
    pub burst: u8,
}

impl LatestActivity {
    /// Record one regeneration of hidden content at `at`: the burst grows while activity keeps
    /// arriving within [`ACTIVITY_BURST_WINDOW`] and restarts at 1 otherwise.
    #[must_use]
    pub fn record(self, at: Instant) -> Self {
        let recent = self
            .at
            .is_some_and(|previous| at.saturating_duration_since(previous) < ACTIVITY_BURST_WINDOW);
        Self {
            at: Some(self.at.map_or(at, |previous| previous.max(at))),
            burst: if recent {
                self.burst.saturating_add(1).min(MAX_ACTIVITY_BURST)
            } else {
                1
            },
        }
    }
}

/// Latest-bar bookkeeping owned by the engine: the full region geometry, the stored decision
/// and the accepted activity.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LatestBar {
    enabled: bool,
    region: ViewportGeometry,
    state: LatestBarState,
    activity: LatestActivity,
    /// Newest sequence known to exist when last observed (detached windows, W2).
    latest_known: Option<u64>,
}

impl LatestBar {
    /// A bar over `region`; a disabled bar never shows and never reserves a row.
    #[must_use]
    pub fn new(enabled: bool, region: ViewportGeometry) -> Self {
        Self {
            enabled,
            region,
            ..Self::default()
        }
    }

    /// Whether the bar is evaluated at all.
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    /// Enable or disable the bar. Disabling hides it and drops its activity.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.state = LatestBarState::default();
            self.activity = LatestActivity::default();
        }
    }

    /// Full transcript-region geometry, bar row included.
    #[must_use]
    pub const fn region(&self) -> ViewportGeometry {
        self.region
    }

    /// The stored decision.
    #[must_use]
    pub const fn state(&self) -> LatestBarState {
        self.state
    }

    /// Accepted activity.
    #[must_use]
    pub const fn activity(&self) -> LatestActivity {
        self.activity
    }

    /// Set the full region geometry.
    pub const fn set_region(&mut self, region: ViewportGeometry) {
        self.region = region;
    }

    /// Effective viewport: the region minus the bar row while the bar is shown (F6).
    #[must_use]
    pub const fn effective(&self) -> ViewportGeometry {
        ViewportGeometry::new(
            self.region.width,
            self.region
                .height
                .saturating_sub(self.state.reserved_rows()),
        )
    }

    /// Record hidden activity at `at` (C20). Ignored while disabled.
    pub fn record_activity(&mut self, at: Instant) {
        if self.enabled {
            self.activity = self.activity.record(at);
        }
    }

    /// Observe the newest sequence known to exist. A newer one is hidden activity (C20).
    pub fn observe_latest_known(&mut self, sequence: Option<u64>, at: Instant) {
        if sequence > self.latest_known {
            if self.latest_known.is_some() {
                self.record_activity(at);
            }
            self.latest_known = sequence;
        }
    }

    /// Store a new decision. Returns whether the reserved rows changed (the engine then
    /// reduces a `Geometry` input with [`Self::effective`]). A hidden bar drops its activity.
    pub fn store(&mut self, state: LatestBarState) -> bool {
        if !self.enabled {
            return false;
        }
        let reservation_changed = state.reserved_rows() != self.state.reserved_rows();
        if !state.visible {
            self.activity = LatestActivity::default();
        }
        self.state = state;
        reservation_changed
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{LatestActivity, LatestBar, LatestBarState, MAX_ACTIVITY_BURST};
    use crate::viewport::ViewportGeometry;

    #[test]
    fn burst_grows_with_close_activity_and_restarts_after_the_window() {
        let start = Instant::now();
        let mut activity = LatestActivity::default();
        for step in 0..12_u64 {
            activity = activity.record(start + Duration::from_millis(step * 50));
        }
        assert_eq!(activity.burst, MAX_ACTIVITY_BURST);
        let later = start + Duration::from_secs(5);
        activity = activity.record(later);
        assert_eq!(activity.burst, 1);
        assert_eq!(activity.at, Some(later));
    }

    #[test]
    fn effective_viewport_reserves_the_bar_row_only_while_visible() {
        let mut bar = LatestBar::new(true, ViewportGeometry::new(40, 10));
        assert_eq!(bar.effective().height, 10);
        assert!(bar.store(LatestBarState {
            visible: true,
            ..LatestBarState::default()
        }));
        assert_eq!(bar.effective().height, 9);
        assert!(!bar.store(LatestBarState {
            visible: true,
            rows_below_latest: 2,
            ..LatestBarState::default()
        }));
        assert!(bar.store(LatestBarState::default()));
        assert_eq!(bar.effective().height, 10);
    }

    #[test]
    fn disabled_bar_never_reserves_or_records() {
        let mut bar = LatestBar::new(false, ViewportGeometry::new(40, 10));
        bar.record_activity(Instant::now());
        assert!(!bar.store(LatestBarState {
            visible: true,
            ..LatestBarState::default()
        }));
        assert_eq!(bar.effective().height, 10);
        assert_eq!(bar.activity(), LatestActivity::default());
    }

    #[test]
    fn glow_age_needs_visibility_activity_and_motion() {
        let at = Instant::now();
        let state = LatestBarState {
            visible: true,
            rows_below_latest: 0,
            activity_at: Some(at),
            burst: 3,
        };
        let window = Duration::from_millis(420);
        assert_eq!(
            state.active_age(at + Duration::from_millis(100), window),
            Some(Duration::from_millis(100))
        );
        assert_eq!(state.active_age(at + window, window), None);
        assert_eq!(state.active_age(at, Duration::ZERO), None);
        let hidden = LatestBarState {
            visible: false,
            ..state
        };
        assert_eq!(hidden.active_age(at, window), None);
    }
}
