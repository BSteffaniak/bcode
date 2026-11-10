//! Paced live text (spec 9.2 "Smooth streaming", D13, EN4).
//!
//! Pacing lives in the engine's own `SessionView`: accepted live text is exposed as progressive
//! visible prefixes under the renderer-neutral [`StreamingPresentationPolicy`]. The engine only
//! applies the policy the host configures, reports the next pacing deadline for the host's
//! runtime timer and incorporates released text as a content change of the streaming units,
//! like a live delta (W3 order). It reads no clock: every call carries its time.

use web_time::Instant;

use bcode_session_view_models::StreamingPresentationPolicy;

use super::{EngineUpdate, TranscriptEngine};

impl TranscriptEngine {
    /// Apply the renderer-neutral live text presentation policy (`[tui] streaming`, the
    /// streaming configurator) to the engine's `SessionView`. Switching to immediate
    /// presentation flushes pending text, which is incorporated in this update; a paced policy
    /// adopts the new rate for text already pending.
    pub fn set_streaming_policy(
        &mut self,
        policy: StreamingPresentationPolicy,
        at: Instant,
    ) -> EngineUpdate {
        self.observe_time(at);
        let _flushed = self.session.set_streaming_presentation_policy(policy);
        self.sync_presented_text(at)
    }

    /// Next time paced live text may advance, for the host's runtime timer; `None` when no
    /// accepted text waits to be shown.
    pub fn next_streaming_deadline(&self, now: Instant) -> Option<Instant> {
        self.session.next_streaming_presentation_deadline(now)
    }

    /// Release the paced live text due at `now` and incorporate it.
    pub fn advance_streaming(&mut self, now: Instant) -> EngineUpdate {
        self.observe_time(now);
        if !self.session.advance_streaming_presentation(now) {
            return EngineUpdate::default();
        }
        self.sync_presented_text(now)
    }

    /// Incorporate the item changes `SessionView` made outside a window operation (released
    /// or flushed paced text).
    fn sync_presented_text(&mut self, at: Instant) -> EngineUpdate {
        let changes = self.window.sync_document(&mut self.session);
        if changes.changes.is_empty() {
            return EngineUpdate::default();
        }
        self.content(changes, at)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;
    use web_time::Instant;

    use bcode_session_models::{
        ClientId, SessionEvent, SessionEventKind, SessionId, SessionLiveEvent,
        SessionLiveEventKind, TurnAdmissionMetadata,
    };
    use bcode_session_view_models::StreamingPresentationPolicy;
    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::Rect;
    use bmux_tui::paint::PaintCx;

    use crate::engine::{HostRequest, TranscriptEngine};
    use crate::frame::FrameClock;
    use crate::navigation::MotionConfig;
    use crate::viewport::ViewportGeometry;
    use crate::window::{HistoryWindowResult, LiveInput};

    const WIDTH: u16 = 120;
    const HEIGHT: u16 = 8;
    /// Long enough that the default 300 graphemes per second cannot show it at once.
    const REPLY: &str = "paced reply text that keeps going well past the first pacing step";

    /// An engine attached to a one-message session, with `policy` applied.
    fn attached(policy: StreamingPresentationPolicy, at: Instant) -> (TranscriptEngine, SessionId) {
        let session = SessionId::new();
        let mut engine = TranscriptEngine::with_producer(
            ViewportGeometry::new(u64::from(WIDTH), u64::from(HEIGHT)),
            MotionConfig::default(),
            Box::new(crate::layout::producers::testing::PlainTextProducer),
        );
        let update = engine.attach(at);
        let HostRequest::Window(issued) = update.requests[0] else {
            panic!("attach requests a window");
        };
        let question = SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 1,
            timestamp_ms: 1,
            session_id: session,
            provenance: None,
            kind: SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: "question".to_owned(),
                admission: TurnAdmissionMetadata::default(),
            },
        };
        let window = HistoryWindowResult {
            generation: issued.generation,
            target: issued.request,
            events: vec![question],
            has_older: false,
            has_newer: false,
            first_available_sequence: Some(1),
            last_available_sequence: Some(1),
        };
        let _ = engine.apply_window(&window, at).expect("attach window");
        let _ = engine.set_streaming_policy(policy, at);
        (engine, session)
    }

    fn delta(session: SessionId, text: &str) -> LiveInput {
        LiveInput::Stream(Box::new(SessionLiveEvent {
            session_id: session,
            kind: SessionLiveEventKind::AssistantTextDelta {
                turn_id: "turn-1".to_owned(),
                segment_id: "segment-1".to_owned(),
                segment_order: 0,
                text: text.to_owned(),
            },
        }))
    }

    /// Paint one frame and return its text.
    fn painted(engine: &mut TranscriptEngine, at: Instant) -> String {
        let clock = FrameClock { now: at };
        let snapshot = engine.prepare_frame(clock);
        let mut buffer = Buffer::empty(Rect::new(0, 0, WIDTH, HEIGHT));
        let mut frame = Frame::new(&mut buffer);
        frame.set_clock(Some(clock.presentation()));
        engine.paint_frame(&snapshot, &mut PaintCx::new(&mut frame));
        let text = (0..HEIGHT)
            .map(|row| frame.buffer().row_symbols(row).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n");
        let _ = engine.commit(snapshot.id);
        text
    }

    #[test]
    fn paced_policy_releases_live_text_on_deadlines_until_complete() {
        let start = Instant::now();
        let (mut engine, session) = attached(StreamingPresentationPolicy::default(), start);
        let update = engine.apply_live(delta(session, REPLY), start);
        assert!(update.invalidated);
        assert!(
            !painted(&mut engine, start).contains(REPLY),
            "paced text is not shown at once"
        );
        let deadline = engine
            .next_streaming_deadline(start)
            .expect("pending paced text schedules a deadline");
        assert!(deadline > start);
        let update = engine.advance_streaming(deadline + Duration::from_secs(2));
        assert!(update.invalidated, "released text is a content change");
        assert!(painted(&mut engine, deadline).contains(REPLY));
        assert_eq!(
            engine.next_streaming_deadline(deadline + Duration::from_secs(2)),
            None,
            "nothing waits once the whole text is shown"
        );
        assert!(
            !engine
                .advance_streaming(deadline + Duration::from_secs(3))
                .invalidated,
            "advancing without pending text changes nothing"
        );
    }

    #[test]
    fn immediate_policy_shows_live_text_at_once_and_schedules_nothing() {
        let start = Instant::now();
        let (mut engine, session) = attached(StreamingPresentationPolicy::immediate(), start);
        let _ = engine.apply_live(delta(session, REPLY), start);
        assert!(painted(&mut engine, start).contains(REPLY));
        assert_eq!(engine.next_streaming_deadline(start), None);
    }

    #[test]
    fn switching_to_immediate_flushes_pending_text_in_the_same_update() {
        let start = Instant::now();
        let (mut engine, session) = attached(StreamingPresentationPolicy::default(), start);
        let _ = engine.apply_live(delta(session, REPLY), start);
        assert!(engine.next_streaming_deadline(start).is_some());
        let update = engine.set_streaming_policy(StreamingPresentationPolicy::immediate(), start);
        assert!(update.invalidated, "the flushed text is incorporated");
        assert!(painted(&mut engine, start).contains(REPLY));
        assert_eq!(engine.next_streaming_deadline(start), None);
        assert!(
            !engine
                .set_streaming_policy(StreamingPresentationPolicy::immediate(), start)
                .invalidated,
            "an unchanged policy changes nothing"
        );
    }
}
