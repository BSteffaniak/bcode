//! Engine-neutral transcript scenario driver contract.
//!
//! A driver adapts one transcript engine (V1 today, V2 later) to the semantic operations and
//! observations used by the scenario suite. Nothing here names engine internals: inputs are user
//! inputs, session events, history results and navigation intents; observations come from the
//! committed frame (what the user sees) plus a few semantic engine facts that a frame cannot
//! show (pending history requests, resident sequence range, selection text).

use std::time::Duration;

use bcode_session_models::SessionEvent;

use super::frame::ScenarioFrame;
use super::session::ScenarioSession;

/// Identity of the engine a driver adapts, used in failure messages and the manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ScenarioEngine {
    /// The current transcript engine in `bcode_tui`.
    V1,
    /// The rewritten transcript engine (`bcode_tui_transcript`).
    #[allow(dead_code)]
    V2,
}

/// Initial state for one scenario run.
#[derive(Debug, Clone)]
pub struct ScenarioSetup {
    /// Canonical session history available from the fake daemon.
    pub session: ScenarioSession,
    /// Which part of the session is resident when the scenario starts.
    pub resident: ResidentSeed,
    /// Terminal width in cells.
    pub width: u16,
    /// Terminal height in cells.
    pub height: u16,
}

/// Initial resident window of a scenario.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResidentSeed {
    /// The newest `events` canonical events, attached live (session attach).
    Latest {
        /// Number of newest canonical events made resident.
        events: usize,
    },
    /// The whole session, attached live.
    Everything,
}

/// Semantic history request an engine emitted and is waiting on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryRequest {
    /// Older page: events with `sequence < before_sequence`, newest first up to the limits.
    Older {
        /// Exclusive upper sequence bound.
        before_sequence: u64,
        /// Engine page sizing policy.
        limits: PageLimits,
    },
    /// Newer page: events with `sequence > after_sequence`, oldest first up to the limits.
    Newer {
        /// Exclusive lower sequence bound.
        after_sequence: u64,
        /// Engine page sizing policy.
        limits: PageLimits,
    },
    /// Window around one sequence (timeline jump, scrollbar fraction, engine handoff).
    Around {
        /// Target sequence, included in the window when it exists.
        sequence: u64,
        /// Maximum events at or before `sequence`.
        before_events: usize,
        /// Maximum events after `sequence`.
        after_events: usize,
    },
    /// Window at the session start.
    Head {
        /// Maximum events in the window.
        max_events: usize,
    },
    /// Window at the session tail (bounded latest attach).
    Tail {
        /// Maximum events in the window.
        max_events: usize,
    },
}

impl HistoryRequest {
    /// Return the direction class of the request for filtering.
    #[must_use]
    pub const fn kind(&self) -> HistoryRequestKind {
        match self {
            Self::Older { .. } => HistoryRequestKind::Older,
            Self::Newer { .. } => HistoryRequestKind::Newer,
            Self::Around { .. } => HistoryRequestKind::Around,
            Self::Head { .. } => HistoryRequestKind::Head,
            Self::Tail { .. } => HistoryRequestKind::Tail,
        }
    }
}

/// Direction class of a [`HistoryRequest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HistoryRequestKind {
    /// Older page.
    Older,
    /// Newer page.
    Newer,
    /// Window around a sequence.
    Around,
    /// Session-start window.
    Head,
    /// Session-tail window.
    Tail,
}

/// Page sizing an engine asks the daemon to satisfy.
///
/// The fake daemon fills a page until both targets are met or a cap is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageLimits {
    /// Minimum transcript items the page should contain.
    pub target_items: usize,
    /// Minimum estimated rows the page should contain.
    pub target_rows: usize,
    /// Maximum transcript items.
    pub max_items: usize,
    /// Maximum canonical events scanned.
    pub max_events: usize,
}

/// Engine-assigned identity of one pending history request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RequestId(pub u64);

/// One history request waiting for its result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingHistoryRequest {
    /// Identity used to complete the request.
    pub id: RequestId,
    /// What the engine asked for.
    pub request: HistoryRequest,
}

/// Result the fake daemon returns for one history request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryResponse {
    /// Canonical events in ascending sequence order.
    pub events: Vec<SessionEvent>,
    /// Whether events older than the first returned event exist.
    pub has_older: bool,
    /// Whether events newer than the last returned event exist.
    pub has_newer: bool,
}

/// Vertical wheel direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WheelDirection {
    /// Toward older content.
    Up,
    /// Toward newer content.
    Down,
}

/// Terminal cell position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellPoint {
    /// Column.
    pub x: u16,
    /// Row.
    pub y: u16,
}

/// User input delivered to the engine. Several inputs may be delivered between frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScenarioInput {
    /// Key stroke by BMUX key name (for example `"pageup"`, `"ctrl+end"`, `"alt+u"`, `"k"`).
    Key(String),
    /// One mouse-wheel tick at a cell (normally inside the transcript).
    Wheel {
        /// Wheel direction.
        direction: WheelDirection,
        /// Pointer position.
        at: CellPoint,
    },
    /// Primary-button press and release without movement.
    Click {
        /// Pointer position.
        at: CellPoint,
    },
    /// Primary-button press: the start of a click or drag gesture.
    Press {
        /// Pointer position.
        at: CellPoint,
    },
    /// Pointer movement with the primary button held.
    DragTo {
        /// Pointer position.
        at: CellPoint,
    },
    /// Primary-button release: the end of a click or drag gesture.
    Release {
        /// Pointer position.
        at: CellPoint,
    },
    /// Terminal resize.
    Resize {
        /// New width in cells.
        width: u16,
        /// New height in cells.
        height: u16,
    },
}

/// Navigation intent that has no single engine-neutral key binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScenarioIntent {
    /// Timeline navigation to one canonical sequence (resident or not).
    JumpToSequence(u64),
    /// Return to the latest content (latest bar / jump-to-latest action).
    JumpToLatest,
}

/// Engine adapter used by [`super::Scenario`].
///
/// Contract:
/// * Inputs, live events, intents and history results are applied immediately and never present
///   a frame. Any number of them may be delivered between two [`Self::present`] calls.
/// * [`Self::present`] runs the engine's real frame preparation, paint and commit path and
///   returns observations taken from the committed frame.
/// * History requests are reported by [`Self::pending_requests`] until completed; the driver
///   never answers them itself.
/// * Time only moves through [`Self::advance_clock`].
pub trait TranscriptScenarioDriver: Sized {
    /// Engine this driver adapts.
    const ENGINE: ScenarioEngine;

    /// Build the engine with the setup's resident window attached and no frame presented.
    fn start(setup: &ScenarioSetup) -> Self;

    /// Rows one wheel tick scrolls under this engine's default configuration.
    fn wheel_rows(&self) -> u16;

    /// Deliver one live canonical event for the attached session.
    fn deliver_live(&mut self, event: SessionEvent);

    /// History requests emitted by the engine and not yet completed.
    fn pending_requests(&self) -> Vec<PendingHistoryRequest>;

    /// Deliver the result of one pending history request (the effect result the engine awaits).
    fn complete_request(&mut self, id: RequestId, response: HistoryResponse);

    /// Deliver one user input.
    fn input(&mut self, input: ScenarioInput);

    /// Deliver one navigation intent.
    fn navigate(&mut self, intent: ScenarioIntent);

    /// Advance the engine's presentation clock (animations, grace windows, deadlines).
    fn advance_clock(&mut self, by: Duration);

    /// Present and commit one frame, returning what the committed frame shows.
    fn present(&mut self) -> ScenarioFrame;

    /// Inclusive canonical sequence range currently resident, if any.
    fn resident_sequences(&self) -> Option<(u64, u64)>;

    /// Plain text of the current transcript selection, if any.
    fn selection_text(&self) -> Option<String>;

    /// Apply one engine-neutral configuration setting, as a configuration reload would.
    ///
    /// The default panics: a driver that cannot express a setting must say so loudly.
    fn configure(&mut self, setting: &ScenarioSetting) {
        panic!("{:?} driver cannot apply {setting:?}", Self::ENGINE);
    }

    /// Text written to the system clipboard so far, oldest first.
    ///
    /// The default panics: a driver must observe clipboard writes to run copy scenarios.
    fn clipboard_writes(&self) -> Vec<String> {
        panic!(
            "{:?} driver does not observe clipboard writes",
            Self::ENGINE
        );
    }

    /// Repaint scope of the last presented frame, when the engine reports it.
    fn last_repaint(&self) -> Option<RepaintObservation> {
        None
    }

    /// Rows the focused transcript unit-focus target occupies in the last presented frame,
    /// relative to the transcript viewport's top row (`0`). The range is half-open and may
    /// extend above (`start < 0`) or below the viewport. `None` when no target is focused.
    ///
    /// The default panics: a driver must observe focus geometry to run focus-edge scenarios.
    fn focused_extent(&self) -> Option<std::ops::Range<i64>> {
        panic!("{:?} driver does not observe focus geometry", Self::ENGINE);
    }
}

/// Engine-neutral configuration a scenario can apply between frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScenarioSetting {
    /// Replace the configured structural presentation rules with these `[[rules]]` tables
    /// (`[presentation]` section syntax).
    PresentationRules(String),
    /// Transcript selection copy policy (`[tui.selection]`).
    Selection {
        /// Copy automatically when a drag gesture is released.
        auto_copy_on_release: bool,
        /// Clear the selection after a successful copy.
        clear_after_copy: bool,
        /// Delay before the post-copy clear, in milliseconds.
        clear_delay_ms: u64,
    },
    /// `tui.reduce_motion`: make scrolls, glows and accent transitions instant.
    ReducedMotion(bool),
    /// Make subsequent clipboard writes fail (`true`) or succeed (`false`). Not a configuration
    /// key: it scripts the environment's clipboard.
    ClipboardFails(bool),
}

/// How much of the terminal the last presented frame repainted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepaintObservation {
    /// Whether the frame painted the complete surface (full damage or a full backend repaint)
    /// rather than only damaged regions.
    pub full: bool,
    /// Terminal cells changed by the presentation.
    pub changed_cells: usize,
}
