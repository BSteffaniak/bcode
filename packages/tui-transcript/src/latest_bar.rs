//! Latest-content bar (spec rules F6, C18–C20).
//!
//! Phase 2 stub. Visibility is a pure function evaluated once per frame against the full
//! transcript-region height; activity code never emits navigation (N9).

/// Latest-bar presentation state for one frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct LatestBarState {
    /// Whether the bar is shown.
    pub visible: bool,
    /// Blank tail-space rows below latest content ("N rows below latest", C2).
    pub rows_below_latest: u64,
}
