//! Navigation controller: the single position authority (spec sections 3.5 and 4, rules
//! N1–N12, P1–P4, T1–T27).
//!
//! [`NavState`] is the only stored navigation state. Every change goes through [`reduce`]; rows
//! are derived from the keyed position and the current geometry every time they are needed
//! (P1, N3). The viewport writes the derived BMUX position before each layout and never reads
//! the list's position back (VP5).

pub mod auto;
mod facts;
mod geometry;
mod reducer;
#[cfg(test)]
mod tests;

pub use facts::{Correspondence, NoUnitFacts, RemovedUnits, UnitFacts, UnitRole};

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;
use web_time::Instant;

use bcode_session_view_models::TranscriptChanges;
use bmux_tui::style::Easing;
use bmux_tui_components::virtual_list::{
    ListGeometry, MeasuredListIndex, ResolvedPosition, SourceMap, resolve_position,
};

use crate::frame::FrameId;
use crate::layout::SourcePosition;
use crate::units::{BoundaryEdge, UnitKey};
use crate::viewport::{self, ViewportGeometry};
use crate::window::{BoundaryStatus, PageDirection, PageRequest, ResidentWindow, WindowMode};
use crate::window::{WindowRequest, WindowTarget};

/// Manual-input grace period (N5, ambiguity 3).
pub const GRACE: Duration = Duration::from_millis(450);
/// Rows moved by one Page Up/Page Down (D1, 11-P2).
pub const PAGE_STEP_ROWS: u64 = 10;

/// Offset of a reading anchor inside its unit.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AnchorOffset {
    /// Row within the unit shown on the viewport top row (clamped to the unit, P2).
    Row(u64),
    /// Mapped source position plus rows below the row where it begins (P3).
    Source {
        /// Source position inside the unit.
        position: SourcePosition,
        /// Rows below the row where `position` begins.
        row_delta: u64,
    },
}

/// Keyed reading position: a unit plus an offset inside it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReadingAnchor {
    /// Anchored unit.
    pub unit: UnitKey,
    /// Offset inside the unit.
    pub offset: AnchorOffset,
}

impl ReadingAnchor {
    /// Anchor at the first row of a unit.
    #[must_use]
    pub const fn top(unit: UnitKey) -> Self {
        Self {
            unit,
            offset: AnchorOffset::Row(0),
        }
    }

    /// Anchor at a row of a unit.
    #[must_use]
    pub const fn row(unit: UnitKey, row: u64) -> Self {
        Self {
            unit,
            offset: AnchorOffset::Row(row),
        }
    }
}

/// Bcode navigation position (P1). Mapped one way to BMUX `ListPosition` by
/// [`viewport::bmux_position`] (VP7).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ListPosition {
    /// Last content row on the viewport bottom row.
    FollowBottom,
    /// Anchor row on the viewport top row, without blank rows below content.
    Anchored {
        /// Reading anchor.
        anchor: ReadingAnchor,
    },
    /// Anchor row on the viewport top row; blank rows below content derived from geometry.
    TailSpace {
        /// Reading anchor.
        anchor: ReadingAnchor,
    },
}

/// Keyed motion target, re-resolved against current geometry at every sample (N7).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ScrollTarget {
    /// Unit top on the viewport top row.
    ItemTop(UnitKey),
    /// Unit bottom on the viewport bottom row.
    ItemBottom(UnitKey),
    /// Latest content bottom (completes as `Following`).
    LatestBottom,
    /// An anchor row on the viewport top row.
    Anchor(ReadingAnchor),
}

/// Placement of a jump target in the viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Placement {
    /// Target top on the viewport top row.
    ItemTopAtTop,
    /// Target bottom on the viewport bottom row (taller targets keep their top).
    ItemBottomAtBottom,
    /// Target centered (taller targets keep their top).
    Center,
    /// No movement when fully visible; otherwise placed from the approach edge.
    EnsureVisible,
}

/// Edge a focus traversal approaches its target from (C32).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ApproachEdge {
    /// Moving up: the target's first row goes to the viewport top.
    Top,
    /// Moving down: the target's last row goes to the viewport bottom.
    Bottom,
}

/// Origin of a relative scroll.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InputSource {
    /// Mouse wheel.
    Wheel,
    /// Keyboard line scrolling.
    Key,
    /// Selection edge autoscroll.
    SelectionAutoScroll,
    /// Programmatic movement.
    Programmatic,
}

/// Vertical direction of a movement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum VerticalDirection {
    /// Toward older content (row 0).
    Up,
    /// Toward newer content.
    Down,
}

impl VerticalDirection {
    /// The page direction that extends the window this way.
    #[must_use]
    pub const fn page_direction(self) -> PageDirection {
        match self {
            Self::Up => PageDirection::Older,
            Self::Down => PageDirection::Newer,
        }
    }
}

/// Saved per-session reading position, restored on session resurrection (T21).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ReadingPosition {
    /// Following the latest content.
    Latest,
    /// Reading at an anchor; `sequence` locates the anchor when it is not resident.
    At {
        /// Canonical sequence of the anchored item, used to load an `Around` window.
        sequence: u64,
        /// Anchor inside the loaded window.
        anchor: ReadingAnchor,
    },
}

/// Navigation intent (spec section 4.2). Every input source produces intents (N2).
#[derive(Debug, Clone, PartialEq)]
pub enum NavIntent {
    /// Explicit attach to the latest bottom.
    Follow,
    /// Relative scroll by rows (negative: toward older content).
    ScrollBy {
        /// Signed rows.
        rows: i64,
        /// Input origin.
        source: InputSource,
    },
    /// Page Up/Page Down (negative: toward older content).
    PageBy {
        /// Signed pages.
        pages: i32,
    },
    /// Automatic reveal (submission: not sticky; assistant segment: sticky).
    RevealItem {
        /// Unit to reveal.
        item: UnitKey,
        /// Whether the resulting hold survives overflow.
        sticky: bool,
    },
    /// Jump to a resident unit.
    JumpToItem {
        /// Target unit.
        item: UnitKey,
        /// Placement in the viewport.
        placement: Placement,
    },
    /// Timeline jump to a canonical sequence, resident or not.
    JumpToSequence {
        /// Target sequence.
        sequence: u64,
        /// Placement in the viewport.
        placement: Placement,
    },
    /// Scrollbar drag: maps to a sequence estimate (W12).
    JumpToFraction {
        /// Fraction of the whole session, `0.0..=1.0`.
        fraction: f64,
    },
    /// True session start, loaded directly when not resident (W10).
    JumpToSessionStart,
    /// True latest content, loaded directly when not resident (W10).
    JumpToLatest,
    /// Keep a focus target in view (C32).
    FocusTarget {
        /// Focus target.
        target: crate::focus::FocusTargetId,
        /// Traversal direction.
        edge: ApproachEdge,
        /// Unit-local rows the target governs ([`crate::focus::FocusView::extent`]); `None`
        /// keeps the whole unit in view.
        rows: Option<std::ops::Range<u64>>,
    },
    /// Stop following/holding at the current visual position.
    ReleaseHold,
    /// Restore a saved reading position.
    RestorePosition {
        /// Saved position.
        position: ReadingPosition,
    },
}

impl NavIntent {
    /// Whether this intent is manual movement (T8, T22).
    #[must_use]
    pub const fn is_manual_movement(&self) -> bool {
        matches!(self, Self::ScrollBy { .. } | Self::PageBy { .. })
    }
}

/// Navigation timers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NavTimer {
    /// Manual-input grace expired (T24).
    GraceExpired,
    /// Next motion frame (T25).
    MotionFrame,
}

/// Region a [`NavEffect::Invalidate`] requests repainting.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Region {
    /// The whole transcript region.
    Transcript,
    /// One unit.
    Unit(UnitKey),
    /// The latest-content bar.
    LatestBar,
}

/// Input to [`reduce`] (spec section 4.2).
#[derive(Debug, Clone, PartialEq)]
pub enum NavInput {
    /// A navigation intent.
    Intent {
        /// The intent.
        intent: NavIntent,
        /// Input time.
        at: Instant,
    },
    /// Content changed; window, units and measured index already reflect it (N11, W3).
    Content {
        /// Item-level changes.
        changes: TranscriptChanges,
        /// Input time.
        at: Instant,
    },
    /// A page was incorporated.
    PageResult {
        /// Generation the page was requested for.
        generation: u64,
        /// Page direction.
        direction: PageDirection,
        /// Measured rows of newly inserted units.
        inserted_rows: u64,
        /// Whether that side is now exhausted.
        exhausted: bool,
    },
    /// A window replacement was incorporated.
    WindowResult {
        /// Generation the window was requested for.
        generation: u64,
        /// Which window.
        target: WindowTarget,
    },
    /// Viewport geometry changed (resize or latest-bar reservation, F6).
    Geometry {
        /// New geometry.
        viewport: ViewportGeometry,
    },
    /// A navigation timer fired.
    Timer {
        /// Timer key.
        key: NavTimer,
        /// Fire time.
        at: Instant,
    },
    /// A submission was semantically accepted (T2, T27).
    SubmissionAccepted {
        /// The accepted user-message unit.
        item: UnitKey,
        /// Input time.
        at: Instant,
    },
    /// A frame was presented (CK3).
    PresentationCommitted {
        /// Committed frame id.
        frame: FrameId,
    },
}

/// Effect returned by [`reduce`]; executed by the engine and host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavEffect {
    /// Request a page.
    Page(PageRequest),
    /// Request a window replacement.
    Window(WindowRequest),
    /// Schedule a navigation timer.
    ScheduleTimer {
        /// Timer key.
        key: NavTimer,
        /// Deadline.
        at: Instant,
    },
    /// Cancel a navigation timer.
    CancelTimer(NavTimer),
    /// Request a repaint.
    Invalidate(Region),
}

/// Navigation mode (C1).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NavMode {
    /// Following the latest bottom.
    Following,
    /// A unit's top held on the viewport top; blank rows below allowed.
    Holding {
        /// Held unit.
        item: UnitKey,
        /// Sticky holds survive overflow until the segment ends (C13).
        sticky: bool,
    },
    /// Detached manual reading.
    Reading {
        /// Reading anchor.
        anchor: ReadingAnchor,
    },
    /// Detached, inside the virtual space below content.
    TailSpace {
        /// Reading anchor.
        anchor: ReadingAnchor,
    },
}

/// Movement not yet satisfiable at a resident boundary (N8).
///
/// It is spent by the next page incorporated on its side, whatever generation that page was
/// requested in, and dropped by a window replacement, reversal, explicit jump or exhaustion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PendingMovement {
    /// Direction of the remaining movement.
    pub direction: VerticalDirection,
    /// Remaining rows.
    pub rows: u64,
}

/// Goal of a jump waiting for a window result.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum JumpGoal {
    /// Following at the latest bottom (T14).
    Latest,
    /// Session start (T16).
    SessionStart,
    /// A canonical sequence with a placement (T18).
    Sequence {
        /// Target sequence.
        sequence: u64,
        /// Placement in the viewport.
        placement: Placement,
    },
    /// A saved reading position (T21).
    Restore {
        /// Saved position.
        position: ReadingPosition,
    },
    /// Reveal an accepted submission once the tail window is incorporated (T27).
    Submission {
        /// Accepted user-message unit.
        item: UnitKey,
    },
}

/// A jump waiting for a window result (T14, T16, T18, T21, T27).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PendingJump {
    /// Window generation the request was issued for.
    pub generation: u64,
    /// Requested window.
    pub request: WindowRequest,
    /// What to do once it is incorporated.
    pub goal: JumpGoal,
}

/// A submission awaiting semantic acceptance (C8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RevealRequest {
    /// When the submission was staged.
    pub requested: Instant,
}

/// Running keyed scroll motion; sampled with BMUX mechanics (VP7, N7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrollMotion {
    /// Keyed visual start.
    pub from: ReadingAnchor,
    /// Keyed target.
    pub target: ScrollTarget,
    /// Start time.
    pub started: Instant,
    /// Duration (zero under reduced motion).
    pub duration: Duration,
    /// Easing curve.
    pub easing: Easing,
}

impl ScrollMotion {
    /// Whether the motion has completed at `now`.
    #[must_use]
    pub fn is_done(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) >= self.duration
    }
}

/// Motion configuration consulted by the reducer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MotionConfig {
    /// Scroll motion duration; zero under reduced motion (N7, C22).
    pub duration: Duration,
    /// Easing curve.
    pub easing: Easing,
    /// Interval between motion frames.
    pub frame_interval: Duration,
}

impl MotionConfig {
    /// Instant motion (reduced motion / motion `none`).
    pub const INSTANT: Self = Self {
        duration: Duration::ZERO,
        easing: Easing::Linear,
        frame_interval: Duration::from_millis(16),
    };

    /// Scroll duration of the theme's `subtle` motion level (the default).
    pub const SUBTLE_SCROLL: Duration = Duration::from_millis(180);

    /// Scroll duration of the theme's `full` motion level.
    pub const FULL_SCROLL: Duration = Duration::from_millis(270);

    /// Motion for a theme chrome motion level (X4): `none` and `reduce_motion` are instant
    /// (C22), `subtle` and `full` animate for [`Self::SUBTLE_SCROLL`] and
    /// [`Self::FULL_SCROLL`].
    #[must_use]
    pub fn for_motion(motion: bcode_tui_theme::chrome::Motion, reduce_motion: bool) -> Self {
        use bcode_tui_theme::chrome::Motion;
        let duration = match motion {
            _ if reduce_motion => return Self::INSTANT,
            Motion::None => return Self::INSTANT,
            Motion::Subtle => Self::SUBTLE_SCROLL,
            Motion::Full => Self::FULL_SCROLL,
        };
        Self {
            duration,
            ..Self::default()
        }
    }
}

impl Default for MotionConfig {
    fn default() -> Self {
        Self {
            duration: Self::SUBTLE_SCROLL,
            easing: Easing::EaseOut,
            frame_interval: Duration::from_millis(16),
        }
    }
}

/// The single navigation authority (spec section 4.3, N1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavState {
    /// Position mode.
    pub mode: NavMode,
    /// Running motion, if any.
    pub motion: Option<ScrollMotion>,
    /// Manual-input grace deadline.
    pub grace_until: Option<Instant>,
    /// Movement waiting for a page (N8).
    pub pending_movement: Option<PendingMovement>,
    /// Jump waiting for a window result.
    pub pending_jump: Option<PendingJump>,
    /// Submissions awaiting acceptance.
    pub pending_reveals: BTreeMap<UnitKey, RevealRequest>,
    /// Units admitted for automatic navigation, by identity (N10; resident only).
    pub admitted: BTreeSet<UnitKey>,
    /// Arrived assistant segments whose admission grace deferred (T24, N5; resident only).
    pub deferred: BTreeSet<UnitKey>,
}

impl Default for NavState {
    fn default() -> Self {
        Self::new()
    }
}

impl NavState {
    /// Initial state: following, nothing pending (N12).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            mode: NavMode::Following,
            motion: None,
            grace_until: None,
            pending_movement: None,
            pending_jump: None,
            pending_reveals: BTreeMap::new(),
            admitted: BTreeSet::new(),
            deferred: BTreeSet::new(),
        }
    }

    /// Whether manual-input grace is active at `at`.
    #[must_use]
    pub fn in_grace(&self, at: Instant) -> bool {
        self.grace_until.is_some_and(|until| until > at)
    }

    /// The resting keyed position of the current mode, ignoring any running motion.
    ///
    /// `Holding` maps to a tail-space position at the held unit's top, because a hold keeps
    /// the unit's top on the viewport top and permits blank rows below content.
    #[must_use]
    pub fn resting_position(&self) -> ListPosition {
        match &self.mode {
            NavMode::Following => ListPosition::FollowBottom,
            NavMode::Holding { item, .. } => ListPosition::TailSpace {
                anchor: ReadingAnchor::top(item.clone()),
            },
            NavMode::Reading { anchor } => ListPosition::Anchored {
                anchor: anchor.clone(),
            },
            NavMode::TailSpace { anchor } => ListPosition::TailSpace {
                anchor: anchor.clone(),
            },
        }
    }
}

/// Read-only view of layout and window state the reducer consults (spec section 4.2).
///
/// Unit order, heights and offsets come from the measured index, which every resident unit has
/// an exact height in before the update that made it resident returns (L9, W3).
#[derive(Clone, Copy)]
pub struct NavView<'a> {
    /// Measured index of resident units in document order.
    pub index: &'a MeasuredListIndex<UnitKey>,
    /// Source mapping for source-backed anchors.
    pub sources: &'a dyn SourceMap<UnitKey, SourcePosition>,
    /// Unit roles, sequence lookup and removal correspondence (P4, N9).
    pub facts: &'a dyn UnitFacts,
    /// Transcript viewport geometry.
    pub viewport: ViewportGeometry,
    /// Resident window state.
    pub window: &'a ResidentWindow,
    /// Motion configuration.
    pub motion: MotionConfig,
}

impl NavView<'_> {
    /// BMUX list geometry with the tail-space overscroll policy (VP2).
    #[must_use]
    pub const fn geometry(&self) -> ListGeometry {
        viewport::list_geometry(self.viewport.height)
    }

    /// Unit keys in document order.
    pub fn unit_keys(&self) -> impl ExactSizeIterator<Item = &UnitKey> + '_ {
        self.index.keys()
    }

    /// Exact height of a resident unit.
    #[must_use]
    pub fn unit_height(&self, key: &UnitKey) -> Option<u64> {
        self.index.item_by_key(key).map(|item| item.height)
    }

    /// Collection row where a resident unit starts.
    #[must_use]
    pub fn unit_offset(&self, key: &UnitKey) -> Option<u64> {
        self.index.key_offset(key)
    }

    /// Total measured rows of resident units.
    #[must_use]
    pub fn total_height(&self) -> u64 {
        self.index.total_height()
    }

    /// Resolve a keyed position against the current geometry; `None` when its anchor unit is
    /// not resident.
    #[must_use]
    pub fn resolve(&self, position: &ListPosition) -> Option<ResolvedPosition> {
        resolve_position(
            self.index,
            &viewport::bmux_position(position),
            self.geometry(),
            self.sources,
        )
    }

    /// Window mode.
    #[must_use]
    pub const fn window_mode(&self) -> WindowMode {
        self.window.mode
    }

    /// Boundary unit state on one side of the window.
    #[must_use]
    pub const fn boundary_status(&self, edge: BoundaryEdge) -> BoundaryStatus {
        self.window.boundary_status(edge)
    }
}

impl std::fmt::Debug for NavView<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NavView")
            .field("units", &self.index.len())
            .field("viewport", &self.viewport)
            .field("window", &self.window)
            .field("motion", &self.motion)
            .finish_non_exhaustive()
    }
}

/// Reduce one navigation input (spec section 4.4).
///
/// This is the only function that changes [`NavState`] (N2). The view must already reflect
/// every content change the input reports (N11): window, units and measured index are updated
/// before `Content`, `PageResult` and `WindowResult` inputs are reduced, so correspondence is
/// resolved by key in the same update and manual input that follows is computed against the
/// current geometry.
///
/// Time enters only through the inputs (`at` fields and timers); the reducer never reads a
/// clock. Grace and motion frames are timers the reducer schedules through [`NavEffect`]s.
#[allow(
    clippy::needless_pass_by_value,
    reason = "spec section 4.2 fixes the signature: an input is handed over to the reducer"
)]
pub fn reduce(state: &mut NavState, input: NavInput, view: &NavView<'_>) -> Vec<NavEffect> {
    reducer::reduce(state, &input, view)
}

/// The keyed position to present at `now`: a running motion sampled with BMUX mechanics
/// (VP7, N7), otherwise the resting position.
///
/// Pure: preparing or abandoning a frame never changes navigation state (CK1, CK2). A
/// completed motion, or any motion under reduced motion (C22), presents the resting position
/// until `Timer(MotionFrame)` clears it.
#[must_use]
pub fn presented_position(state: &NavState, view: &NavView<'_>, now: Instant) -> ListPosition {
    if let Some(motion) = &state.motion
        && !motion.is_done(now)
        && !view.motion.duration.is_zero()
        && let Some(position) = viewport::bmux_motion(motion).position_at(
            now,
            view.index,
            view.geometry(),
            view.sources,
        )
    {
        return view.position_from_bmux(position);
    }
    state.resting_position()
}
