//! Transcript host wiring (TUI V2 specification sections 3.0, 7 and 12).
//!
//! The root runtime model routes transcript ingest, transcript input and transcript painting
//! to [`TranscriptV2Host`], which owns the `bcode_tui_transcript` engine plus the host state it
//! needs (attached session, outstanding history requests, last prepared frame).
//!
//! The host shell (composer, overlays, permissions, plugin surfaces, configuration) stays
//! separate from the engine (EN4). The application model receives session events for that
//! shell state; the engine keeps its own `SessionView`.
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
//! * [`inspector`]: the structure inspector and transcript tooling (bulk disclosure, detail
//!   cycling, make-default, inline shell expansion; FO6, C39).
//! * [`interactions`]: inline interaction placement inside the transcript (C37, D10).
//! * [`latest_bar`]: the status row's transcript position label ("N rows below latest",
//!   "N rows from bottom", C2).
//! * [`local_rows`]: pending submissions and process-local notices of the host shell as
//!   engine local rows (LR1–LR7).
//! * [`markdown`]: Markdown contribution focus and activation (links, session links,
//!   footnotes, fragments, `<details>`, the Mermaid source view).
//! * [`visuals`]: Markdown images and Mermaid diagrams rendered by the host-side workers and
//!   accepted into the render configuration (phase 6, L8, D2).
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
pub mod inspector;
pub mod interactions;
pub mod latest_bar;
pub mod local_rows;
pub mod markdown;
pub mod paint;
pub mod plugin_visuals;
pub mod render_config;
pub mod visuals;

#[cfg(test)]
pub mod test_fixtures;

#[cfg(test)]
mod scrollbar_tests;

use bcode_session_models::SessionId;
use bcode_tui_transcript::engine::{HostRequest, TranscriptEngine};
use bcode_tui_transcript::focus::ChatFocus;
use bcode_tui_transcript::frame::FrameSnapshot;
use bcode_tui_transcript::latest_bar::LatestBarState;
use bcode_tui_transcript::navigation::MotionConfig;
use bcode_tui_transcript::viewport::ViewportGeometry;

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
    /// Markdown images and Mermaid diagrams: worker lanes and accepted visuals ([`visuals`]).
    visuals: visuals::MarkdownVisuals,
    /// Terminal rectangles of the last painted frame ([`paint`]).
    regions: paint::Regions,
    /// Left-button gestures: click recognition and the selection gesture ([`input`], FO9,
    /// X1, C34).
    pointer: input::PointerState,
    /// The region the frame was prepared for and not yet painted ([`paint`]).
    prepared: Option<bmux_tui::geometry::Rect>,
    /// The last presented frame: scroll damage is measured against it ([`paint`], BMUX 11).
    committed_frame: Option<FrameSnapshot>,
    /// A change since the host shell last took damage needs a full repaint rather than the
    /// transcript region alone (focus kind, inspector, notices; F5).
    full_damage: bool,
    /// Host shell activity frame last accepted as the spinner/elapsed phase ([`paint`], F7).
    activity_frame: Option<u64>,
    /// A press on the scrollbar track is being dragged; pointer input goes to the list until
    /// the release ([`input`], VP4).
    scrollbar_drag: bool,
    /// Next latest-bar glow frame ([`paint`], C21).
    glow_frame_at: Option<std::time::Instant>,
    /// Inspector panel, tooling notices and queued per-tool defaults ([`inspector`]).
    tools: inspector::HostTools,
    /// Markdown activation: source view, focus highlight, notices and shell requests
    /// ([`markdown`]).
    markdown: markdown::MarkdownHost,
    /// Focused shell screen and the plugin content position last requested
    /// ([`plugin_visuals`], E3, C39).
    content_input: plugin_visuals::ContentInput,
    /// Fingerprint of the shell state the engine's local rows were last built from; `None`
    /// until the first sync and after an engine reset ([`local_rows`]).
    local_rows: Option<u64>,
    /// Accepted submissions shown until the engine incorporates their message
    /// ([`local_rows::HandedOffSubmission`]).
    handoffs: Vec<local_rows::HandedOffSubmission>,
    /// Pending-submission rows the last local-row sync saw in the shell; kept across engine
    /// resets so a submission the shell forgets in the update that attaches the session it
    /// created is still handed off ([`local_rows`]).
    shown_submissions: Vec<bcode_tui_transcript::units::local::LocalRow>,
    /// Streaming-presentation policy applied to the engine's `SessionView`; re-applied after
    /// an engine reset ([`ingest`], D13).
    streaming: bcode_session_view_models::StreamingPresentationPolicy,
    /// Transcript position the status row last showed ([`latest_bar`], C2).
    status_painted: Option<crate::render::TranscriptStatusPosition>,
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
            visuals: visuals::MarkdownVisuals::default(),
            regions: paint::Regions::default(),
            pointer: input::PointerState::default(),
            prepared: None,
            committed_frame: None,
            full_damage: false,
            activity_frame: None,
            scrollbar_drag: false,
            glow_frame_at: None,
            tools: inspector::HostTools::default(),
            markdown: markdown::MarkdownHost::default(),
            content_input: plugin_visuals::ContentInput::default(),
            local_rows: None,
            handoffs: Vec::new(),
            shown_submissions: Vec::new(),
            streaming: bcode_session_view_models::StreamingPresentationPolicy::immediate(),
            status_painted: None,
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
        if update.invalidated {
            // The open inspector follows focus and structure in the update path (F3).
            self.refresh_inspector();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::TranscriptV2Host;

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
