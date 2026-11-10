//! Engine-neutral transcript scenario driver contract.
//!
//! A driver adapts one transcript engine (V1 today, V2 later) to the semantic operations and
//! observations used by the scenario suite. Nothing here names engine internals: inputs are user
//! inputs, session events, history results and navigation intents; observations come from the
//! committed frame (what the user sees) plus a few semantic engine facts that a frame cannot
//! show (pending history requests, resident sequence range, selection text).

use std::time::Duration;

use bcode_session_models::SessionEvent;

use super::frame::{CellRect, ScenarioFrame};
use super::session::ScenarioSession;

/// Identity of the engine a driver adapts, used in failure messages and the manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ScenarioEngine {
    /// The current transcript engine in `bcode_tui`.
    V1,
    /// The rewritten transcript engine (`bcode_tui_transcript`) in the application: the root
    /// model with the V2 host selected.
    V2,
    /// The V2 engine façade driven headlessly, without the host (engine-level checks).
    V2Headless,
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
    /// No session yet: the composer targets an unpersisted draft. The canonical session is
    /// the one the first accepted submission creates ([`SubmissionOutcome::Accepted`]).
    Draft,
}

/// How the fake daemon answers the newest submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmissionOutcome {
    /// The daemon rejects the request with the server error `code`.
    Rejected {
        /// Server error code.
        code: String,
    },
    /// The daemon accepts the message into the scenario session, creating it when the
    /// composer targets a draft: it appends the submission's acceptance and activation to the
    /// canonical session and answers with the production result shape (the submission record
    /// and the model receipt). Later events arrive live.
    Accepted,
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
    /// Live state after the resident tail (W11 rehydration): events with
    /// `sequence > after_sequence`, oldest first, without replacing the resident window.
    LiveState {
        /// Exclusive lower sequence bound (the newest resident sequence at issue), when known.
        after_sequence: Option<u64>,
        /// Maximum events read.
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
            Self::LiveState { .. } => HistoryRequestKind::LiveState,
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
    /// Live-state rehydration after the resident tail.
    LiveState,
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

    /// Item layout work performed since the previous call (updates and frames alike), then
    /// reset; `None` when the engine does not count it (spec 9.2, D4).
    fn layout_work(&mut self) -> Option<LayoutWork> {
        None
    }

    /// Resident-window operation work since the engine started (spec 9.2; phase 5: paging
    /// performs no full rebuild); `None` when the engine does not count it.
    fn window_work(&self) -> Option<WindowWork> {
        None
    }

    /// Canonical events currently resident (W7, W7a); `None` when the engine does not report it.
    fn resident_event_count(&self) -> Option<u64> {
        None
    }

    /// Damage the last presented frame was drawn with (spec section 8 item 15b), after
    /// promotion (the first frame and resets are full); `None` when the engine does not report
    /// it.
    fn last_damage(&self) -> Option<DamageObservation> {
        None
    }

    /// Accept one activity frame of the host shell, as its activity animation timer does
    /// (spinner and elapsed labels, F7, D7).
    ///
    /// The default panics: a driver must reach the host shell's activity clock to run D7.
    fn accept_activity_frame(&mut self) {
        panic!("{:?} driver cannot accept activity frames", Self::ENGINE);
    }

    /// Deliver one horizontal wheel tick at `at` (toward later columns when `right`).
    ///
    /// The default panics: a driver must deliver horizontal wheel input to run C39.
    fn horizontal_wheel(&mut self, right: bool, at: CellPoint) {
        panic!(
            "{:?} driver cannot deliver horizontal wheel input (right={right}, at={at:?})",
            Self::ENGINE
        );
    }

    /// Run one command-palette route (for example `tools.expandAll`) as the palette does.
    ///
    /// The default panics: a driver must reach the command palette's routing.
    fn run_command(&mut self, route: &str) {
        panic!(
            "{:?} driver cannot run the command route {route:?}",
            Self::ENGINE
        );
    }

    /// Open an interactive surface for `interaction_id` that prefers `rows` rows and paints
    /// `text` on each of them (permission or question controls, C37).
    ///
    /// The default panics: a driver must be able to open plugin interaction surfaces.
    fn open_interaction(&mut self, interaction_id: &str, rows: u16, text: &str) {
        panic!(
            "{:?} driver cannot open the interaction surface {interaction_id:?} ({rows} rows, {text:?})",
            Self::ENGINE
        );
    }

    /// Answer the newest message submission as the daemon would (the submit effect's result),
    /// recording an accepted submission in the canonical `session`.
    ///
    /// The default panics: a driver must reach the host shell's effect queue.
    fn resolve_submission(&mut self, outcome: &SubmissionOutcome, session: &mut ScenarioSession) {
        let _ = session;
        panic!(
            "{:?} driver cannot answer submissions ({outcome:?})",
            Self::ENGINE
        );
    }

    /// Run the newest slash command with the production executor and deliver its result. Only
    /// builtins that need no daemon complete.
    ///
    /// The default panics: a driver must reach the host shell's effect queue.
    fn complete_slash_command(&mut self) {
        panic!("{:?} driver cannot run slash commands", Self::ENGINE);
    }

    /// Rows the focused transcript unit-focus target occupies in the last presented frame,
    /// relative to the transcript viewport's top row (`0`). The range is half-open and may
    /// extend above (`start < 0`) or below the viewport. `None` when no target is focused.
    ///
    /// The default panics: a driver must observe focus geometry to run focus-edge scenarios.
    fn focused_extent(&self) -> Option<std::ops::Range<i64>> {
        panic!("{:?} driver does not observe focus geometry", Self::ENGINE);
    }

    /// Prepare and paint one frame whose presentation fails (its terminal output errors), so
    /// the frame is never committed (C27, CK2). Nothing is recorded as a scenario frame.
    ///
    /// The default panics: a driver must be able to inject a presentation failure.
    fn abandon_presentation(&mut self) {
        panic!("driver: needs presentation failure injection");
    }

    /// Load `plugins` (bundled, statically linked) into the application's plugin host, as
    /// startup does, so their visual adapters present tool results (E1).
    ///
    /// The default panics: a driver must own a plugin host to run plugin visual scenarios.
    fn load_bundled_plugins(&mut self, plugins: &[ScenarioPlugin]) {
        panic!(
            "{:?} driver cannot load bundled plugins ({plugins:?})",
            Self::ENGINE
        );
    }
}

/// A bundled plugin a scenario loads ([`TranscriptScenarioDriver::load_bundled_plugins`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScenarioPlugin {
    /// `bcode.filesystem`: file reads and edits.
    Filesystem,
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
    /// `[tui] transcript_scrollbar`: the optional transcript scrollbar (X6).
    TranscriptScrollbar(ScenarioScrollbar),
    /// Make subsequent clipboard writes fail (`true`) or succeed (`false`). Not a configuration
    /// key: it scripts the environment's clipboard.
    ClipboardFails(bool),
}

/// `[tui] transcript_scrollbar` modes (X6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScenarioScrollbar {
    /// No scrollbar (the default).
    Off,
    /// Drawn over the transcript's last column.
    Overlay,
    /// Drawn in a reserved column beside the transcript.
    Gutter,
}

/// Resident-window operation work an engine performed (phase 5 exit: no full rebuild on paging).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WindowWork {
    /// Whole-window replacements (attach, jumps, `Head`/`Tail`/`Around` windows).
    pub replacements: u64,
    /// Pages that could not be applied incrementally and replaced the resident window instead
    /// (SD2 fallback).
    pub fallbacks: u64,
    /// Older pages prepended incrementally.
    pub prepends: u64,
    /// Newer pages appended incrementally.
    pub appends: u64,
    /// Evictions.
    pub evictions: u64,
    /// Canonical events reduced by the session view in total.
    pub events_reduced: u64,
}

/// Item layout work an engine performed (spec 9.2 `LayoutWorkCounters`, D4).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LayoutWork {
    /// Markdown source parses.
    pub markdown_parses: u64,
    /// Syntax highlighting runs.
    pub highlights: u64,
    /// Markdown projections built.
    pub projections: u64,
    /// Unit measurements.
    pub measurements: u64,
    /// Item component builds.
    pub producer_builds: u64,
}

impl LayoutWork {
    /// Whether no work of any kind was performed.
    #[must_use]
    pub const fn is_zero(&self) -> bool {
        self.markdown_parses == 0
            && self.highlights == 0
            && self.projections == 0
            && self.measurements == 0
            && self.producer_builds == 0
    }
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

/// Damage of one presented frame (spec section 8 items 11 and 15b), in terminal cells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DamageObservation {
    /// Nothing was presented.
    None,
    /// The whole surface was repainted.
    Full,
    /// Only these regions were repainted.
    Regions(Vec<CellRect>),
    /// The content of `region` moved by `rows` (positive: toward row 0) and `regions` (which
    /// cover `region`) were repainted.
    Scroll {
        /// Rows whose content moved.
        region: CellRect,
        /// Signed rows the content moved.
        rows: i64,
        /// Every repainted region.
        regions: Vec<CellRect>,
        /// Whether the presenter moved the rows with a terminal scroll region (BMUX 11
        /// `ScrollRegionAcceleration::Auto`) instead of rewriting them.
        accelerated: bool,
    },
}

impl DamageObservation {
    /// Whether the whole surface was repainted.
    #[must_use]
    pub const fn is_full(&self) -> bool {
        matches!(self, Self::Full)
    }

    /// Every repainted region; `None` for full damage.
    #[must_use]
    pub fn regions(&self) -> Option<&[CellRect]> {
        match self {
            Self::None => Some(&[]),
            Self::Full => None,
            Self::Regions(regions) | Self::Scroll { regions, .. } => Some(regions),
        }
    }

    /// Whether no repainted region reaches outside `area`.
    #[must_use]
    pub fn within(&self, area: CellRect) -> bool {
        self.regions()
            .is_some_and(|regions| regions.iter().all(|region| area.contains_rect(*region)))
    }

    /// Whether some repainted region overlaps `area`.
    #[must_use]
    pub fn touches(&self, area: CellRect) -> bool {
        self.regions()
            .is_none_or(|regions| regions.iter().any(|region| region.overlaps(area)))
    }
}
