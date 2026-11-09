//! V2 transcript host wiring (TUI V2 specification sections 3.0, 7 and 12, phase 4).
//!
//! [`TranscriptEngineChoice`] is the single dispatch point between the two transcript engines
//! (EN5). It is selected once at startup from the process engine (EN1, EN3) and stored on the
//! root runtime model. With [`TranscriptEngineChoice::V1`] every seam is a no-op and the
//! existing V1 code path runs unchanged; with [`TranscriptEngineChoice::V2`] the seams route
//! transcript ingest, transcript input and transcript painting to [`TranscriptV2Host`], which
//! owns the `bcode_tui_transcript` engine plus the host state it needs (attached session,
//! outstanding history requests, last prepared frame).
//!
//! The host shell (composer, overlays, permissions, plugin surfaces, configuration) stays
//! engine-independent (EN4). In phase 4 the V1 application model still receives session
//! events, because it owns that shell state; the V2 engine keeps its own `SessionView`.
//!
//! Method ownership:
//!
//! * [`ingest`]: session events, live events, history and window results, history requests
//!   (effects) and navigation timers.
//! * [`data`]: mapping session stream updates and effect results to engine inputs (live
//!   events, submissions, timeline jumps) and live input held during window replacements.
//! * [`effects`]: executing engine history requests as bounded daemon history reads.
//! * [`render_config`]: theme, motion, Markdown, diff, structure (policy layers, plugin facts,
//!   declared targets) and accepted plugin visuals from the live application.
//! * [`focus`]: key resolution (focus entry, `transcript_focus` keys, scroll keys).
//! * [`input`]: transcript keys, wheel and mouse input and the composer/transcript focus model
//!   (spec section 6).
//! * [`paint`]: viewport geometry, frame preparation, painting, commit, the latest bar and the
//!   optional scrollbar (phase 5, X6).

// Skeleton accessors and outcomes that the follow-up phase 4 work wires into the host; the
// expectation fails once every item is used, so it is removed together with the last stub.
#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "phase 4 skeleton: focus, latest-bar and input outcomes are wired next"
    )
)]

pub mod data;
pub mod effects;
pub mod focus;
pub mod ingest;
pub mod input;
pub mod paint;
pub mod render_config;

#[cfg(test)]
pub mod test_fixtures;

#[cfg(test)]
mod scrollbar_tests;

use bcode_config::TuiEngine;
use bcode_session_models::SessionId;
use bcode_tui_transcript::engine::{HostRequest, TranscriptEngine};
use bcode_tui_transcript::focus::ChatFocus;
use bcode_tui_transcript::frame::FrameSnapshot;
use bcode_tui_transcript::latest_bar::LatestBarState;
use bcode_tui_transcript::navigation::MotionConfig;
use bcode_tui_transcript::viewport::ViewportGeometry;

/// Which engine handles transcript ingest, input and painting (EN5). Fixed for the process
/// lifetime (EN3).
#[derive(Debug, Default)]
pub enum TranscriptEngineChoice {
    /// The V1 transcript pipeline in `BmuxApp`; every seam is a no-op.
    #[default]
    V1,
    /// The V2 engine behind [`TranscriptV2Host`].
    V2(Box<TranscriptV2Host>),
}

impl TranscriptEngineChoice {
    /// Build the choice for `engine`.
    #[must_use]
    pub fn for_engine(engine: TuiEngine) -> Self {
        match engine {
            TuiEngine::V1 => Self::V1,
            TuiEngine::V2 => Self::V2(Box::default()),
        }
    }

    /// The V2 host, when V2 is selected.
    #[must_use]
    pub fn v2(&self) -> Option<&TranscriptV2Host> {
        match self {
            Self::V1 => None,
            Self::V2(host) => Some(host),
        }
    }

    /// The V2 host, mutably, when V2 is selected.
    pub fn v2_mut(&mut self) -> Option<&mut TranscriptV2Host> {
        match self {
            Self::V1 => None,
            Self::V2(host) => Some(host),
        }
    }
}

/// Host side of the V2 transcript engine: the engine façade plus the host state that the
/// engine deliberately does not own (I/O, session attachment, effect bookkeeping, frames).
#[derive(Debug)]
pub struct TranscriptV2Host {
    /// The V2 engine (window, units, layout, viewport, navigation, focus).
    engine: TranscriptEngine,
    /// Session the engine is attached to; `None` for a draft or before the first attach.
    session: Option<SessionId>,
    /// History requests issued by the engine and not yet started as host effects.
    pending_requests: Vec<HostRequest>,
    /// Engine state changed since the last frame; the transcript needs repainting.
    invalidated: bool,
    /// Last prepared frame, used for hit testing until the next frame (CK4).
    last_frame: Option<FrameSnapshot>,
    /// Data-side bookkeeping: live input held during window replacements ([`data`]).
    data: data::HostData,
    /// Render and structure inputs last applied to the engine ([`render_config`]).
    render: render_config::RenderInputs,
    /// Terminal rectangles of the last painted frame ([`paint`]).
    regions: paint::Regions,
    /// Primary-button press awaiting its release ([`input`], FO9).
    pending_click: Option<input::PendingClick>,
    /// A press on the scrollbar track is being dragged; pointer input goes to the list until
    /// the release ([`input`], VP4).
    scrollbar_drag: bool,
    /// Next latest-bar glow frame ([`paint`], C21).
    glow_frame_at: Option<std::time::Instant>,
    /// Scenario time replacing the wall clock for every host and frame clock read, so
    /// transcript scenarios run on scripted time ([`Self::set_scripted_now`]).
    #[cfg(test)]
    scripted_now: Option<std::time::Instant>,
}

// The host owns the engine directly and stays `Send`, so the root runtime model that holds it
// is `Send` without thread confinement.
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<TranscriptEngine>();
    assert_send::<TranscriptV2Host>();
};

impl Default for TranscriptV2Host {
    fn default() -> Self {
        Self::new()
    }
}

impl TranscriptV2Host {
    /// Create a detached host with an empty engine. Geometry arrives with the first painted
    /// frame ([`Self::sync_geometry`]).
    #[must_use]
    pub fn new() -> Self {
        Self {
            engine: TranscriptEngine::new(ViewportGeometry::new(0, 0), MotionConfig::default()),
            session: None,
            pending_requests: Vec::new(),
            invalidated: true,
            last_frame: None,
            data: data::HostData::default(),
            render: render_config::RenderInputs::default(),
            regions: paint::Regions::default(),
            pending_click: None,
            scrollbar_drag: false,
            glow_frame_at: None,
            #[cfg(test)]
            scripted_now: None,
        }
    }

    /// The host clock: the time every transcript input, timer and frame is reduced at. This is
    /// the wall clock, except in tests that script time ([`Self::set_scripted_now`]).
    #[must_use]
    #[cfg_attr(
        not(test),
        expect(clippy::unused_self, reason = "only tests script the host clock")
    )]
    pub fn now(&self) -> std::time::Instant {
        #[cfg(test)]
        if let Some(now) = self.scripted_now {
            return now;
        }
        std::time::Instant::now()
    }

    /// Script the host clock: from now on [`Self::now`] and the frame clock of every painted
    /// frame are `now` (scenario time).
    #[cfg(test)]
    pub const fn set_scripted_now(&mut self, now: std::time::Instant) {
        self.scripted_now = Some(now);
    }

    /// The last prepared frame (hit testing, scenario observation).
    #[must_use]
    pub const fn last_frame(&self) -> Option<&FrameSnapshot> {
        self.last_frame.as_ref()
    }

    /// Item layout work since the previous call, then reset (spec 9.2, scenario observation).
    #[cfg(test)]
    pub const fn take_layout_work(&mut self) -> bcode_tui_transcript::layout::LayoutWorkCounters {
        self.engine.take_layout_work()
    }

    /// The V2 engine (read-only).
    #[must_use]
    pub const fn engine(&self) -> &TranscriptEngine {
        &self.engine
    }

    /// Session the engine is attached to.
    #[must_use]
    pub const fn session(&self) -> Option<SessionId> {
        self.session
    }

    /// Composer/transcript keyboard focus (FO1).
    #[must_use]
    pub const fn focus(&self) -> &ChatFocus {
        self.engine.focus()
    }

    /// Latest-bar decision of the last prepared frame (F6, C18–C21); hidden before the first
    /// frame.
    #[must_use]
    pub fn latest_bar(&self) -> LatestBarState {
        self.last_frame
            .as_ref()
            .map(|frame| frame.latest_bar)
            .unwrap_or_default()
    }

    /// Whether engine state changed since the last frame.
    #[must_use]
    pub const fn is_invalidated(&self) -> bool {
        self.invalidated
    }

    /// Return and clear the invalidation flag.
    pub const fn take_invalidated(&mut self) -> bool {
        std::mem::replace(&mut self.invalidated, false)
    }

    /// Record an engine update: queue its history requests and merge its invalidation.
    fn absorb(&mut self, update: bcode_tui_transcript::engine::EngineUpdate) {
        self.pending_requests.extend(update.requests);
        self.invalidated |= update.invalidated;
    }
}

#[cfg(test)]
mod tests {
    use super::{TranscriptEngineChoice, TranscriptV2Host};
    use bcode_config::TuiEngine;

    #[test]
    fn choice_follows_the_selected_engine() {
        let v1 = TranscriptEngineChoice::for_engine(TuiEngine::V1);
        assert!(v1.v2().is_none());
        let mut v2 = TranscriptEngineChoice::for_engine(TuiEngine::V2);
        assert!(v2.v2().is_some());
        assert!(v2.v2_mut().is_some());
        assert!(TranscriptEngineChoice::default().v2().is_none());
    }

    #[test]
    fn new_host_is_detached_composer_focused_and_needs_a_frame() {
        let mut host = TranscriptV2Host::new();
        assert_eq!(host.session(), None);
        assert!(host.focus().transcript_target().is_none());
        assert!(!host.latest_bar().visible);
        assert!(host.take_invalidated());
        assert!(!host.is_invalidated());
    }
}
