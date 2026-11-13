//! Submission acceptance.
//!
//! The host reports a semantically accepted submission once its user message is durable. In a
//! live window the accepted unit is revealed at the viewport top (T2); in a detached window the
//! tail window is loaded first and the reveal applies once it is incorporated (T27). Staged
//! submissions never reach the engine (C8).

use web_time::Instant;

use crate::navigation::{NavInput, RemovedUnits};
use crate::units::UnitKey;

use super::{EngineUpdate, TranscriptEngine};

impl TranscriptEngine {
    /// Reduce a semantically accepted submission whose user message is the unit `item` (T2,
    /// T27). The host applies the user message itself ([`Self::apply_live`]) first.
    pub fn submission_accepted(&mut self, item: UnitKey, at: Instant) -> EngineUpdate {
        self.observe_time(at);
        let effects = self.reduce_input(
            NavInput::SubmissionAccepted { item, at },
            &RemovedUnits::new(),
        );
        let mut update = self.run_effects(effects);
        update.merge(self.settle(at));
        update
    }
}

#[cfg(test)]
mod tests {
    use web_time::Instant;

    use bcode_session_models::{
        ClientId, SessionEvent, SessionEventKind, SessionId, TurnAdmissionMetadata,
    };
    use bcode_session_view_models::TranscriptViewItemId;

    use crate::engine::{HostRequest, TranscriptEngine};
    use crate::navigation::{MotionConfig, NavMode};
    use crate::units::UnitKey;
    use crate::viewport::ViewportGeometry;
    use crate::window::{HistoryWindowResult, LiveInput, WindowMode, WindowRequest};

    fn event(session_id: SessionId, sequence: u64) -> SessionEvent {
        SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind: if sequence % 2 == 1 {
                SessionEventKind::UserMessage {
                    client_id: ClientId::new(),
                    text: format!("u{sequence}"),
                    admission: TurnAdmissionMetadata::default(),
                }
            } else {
                SessionEventKind::AssistantMessage {
                    text: format!("a{sequence}"),
                }
            },
        }
    }

    /// An engine showing `events` as the window `target` answers.
    fn engine_with(
        target: WindowRequest,
        events: Vec<SessionEvent>,
        has_newer: bool,
    ) -> TranscriptEngine {
        let at = Instant::now();
        let mut engine =
            TranscriptEngine::new(ViewportGeometry::new(40, 10), MotionConfig::INSTANT);
        let update = engine.attach(at);
        let generation = update.requests[0].generation();
        let generation = if target == WindowRequest::Tail {
            generation
        } else {
            engine
                .intent(
                    crate::navigation::NavIntent::JumpToSequence {
                        sequence: events[0].sequence,
                        placement: crate::navigation::Placement::ItemTopAtTop,
                    },
                    at,
                )
                .requests
                .iter()
                .find_map(|request| match request {
                    HostRequest::Window(issued) => Some(issued.generation),
                    HostRequest::Page(_) => None,
                })
                .unwrap_or(generation)
        };
        let result = HistoryWindowResult {
            generation,
            target,
            events,
            has_older: false,
            has_newer,
            first_available_sequence: Some(1),
            last_available_sequence: None,
        };
        engine.apply_window(&result, at).expect("window applies");
        engine
    }

    #[test]
    fn t2_an_accepted_submission_in_a_live_window_is_revealed() {
        let session = SessionId::new();
        let mut engine = engine_with(
            WindowRequest::Tail,
            (1..=6).map(|sequence| event(session, sequence)).collect(),
            false,
        );
        let at = Instant::now();
        let _ = engine.apply_live(LiveInput::Durable(Box::new(event(session, 7))), at);
        let item = UnitKey::Item(TranscriptViewItemId::event(7));
        let _ = engine.submission_accepted(item.clone(), at);
        assert!(
            matches!(&engine.nav().mode, NavMode::Holding { item: held, sticky: false } if *held == item),
            "{:?}",
            engine.nav().mode
        );
    }

    #[test]
    fn t27_an_accepted_submission_in_a_detached_window_loads_the_tail() {
        let session = SessionId::new();
        let mut engine = engine_with(
            WindowRequest::Around { sequence: 1 },
            (1..=6).map(|sequence| event(session, sequence)).collect(),
            true,
        );
        assert_eq!(engine.window().mode, WindowMode::Detached);
        let at = Instant::now();
        let _ = engine.apply_live(LiveInput::Durable(Box::new(event(session, 41))), at);
        let update = engine.submission_accepted(UnitKey::Item(TranscriptViewItemId::event(41)), at);
        assert!(update.requests.iter().any(|request| matches!(
            request,
            HostRequest::Window(issued) if issued.request == WindowRequest::Tail
        )));
    }
}
