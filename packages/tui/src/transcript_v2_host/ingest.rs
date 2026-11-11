//! Data into the V2 engine and history requests out of it (spec sections 3.0, 3.2 and 5).
//!
//! Owned by the host-effects work: live and durable session events, attach (`Window(Tail)`,
//! W10), page and window results (W3), executing [`HostRequest`]s as host effects, and the
//! engine's navigation timers. The mappings live in [`super::data`] and the history reads in
//! [`super::effects`].

use std::time::Instant;

use bcode_session_models::SessionId;
use bcode_tui_transcript::engine::{HostRequest, TranscriptEngine};
use bcode_tui_transcript::navigation::NavIntent;
use bcode_tui_transcript::viewport::ViewportGeometry;

use super::TranscriptV2Host;
use super::data::live_input;
use super::effects::HistoryLoaded;
use crate::effects::{TuiEffect, TuiEffectResult};
use crate::history_flow::SessionStreamUpdate;

/// Whether an effect result belongs to the transcript host alone and must not reach the shell's
/// effect handling: the results of transcript history requests. Every other result is
/// host-shell state.
#[must_use]
pub const fn owns_effect_result(result: &TuiEffectResult) -> bool {
    matches!(result, TuiEffectResult::TranscriptV2HistoryLoaded { .. })
}

impl TranscriptV2Host {
    /// Follow the attached session: a different session (or none) resets the engine to an
    /// empty projection of the same geometry, motion, render and structure configuration
    /// (disclosure overrides belong to the old session), and a new session attaches with the
    /// bounded tail window (W10). The same session is a no-op.
    ///
    /// The reading position of the session left is saved; attaching a session restores its
    /// saved position (T21), or shows the sequence its open asked for
    /// ([`Self::note_opening_anchor`]), with an `Around` window.
    pub fn sync_session(&mut self, session: Option<SessionId>, at: Instant) {
        if session == self.session {
            return;
        }
        if let Some(previous) = self.session {
            let position = self.reading_position();
            self.data.save_position(previous, position);
        }
        self.session = session;
        self.pending_requests.clear();
        self.last_frame = None;
        self.data.reset();
        self.render.engine_reset();
        // The new engine has no local rows; the next sync sends them again. Submissions
        // handed off belong to the session left.
        self.local_rows = None;
        self.handoffs.clear();
        let engine = &mut self.engine;
        // The new engine starts at the full region width; a gutter scrollbar narrows it again.
        let geometry = ViewportGeometry::new(engine.region().width, engine.geometry().height);
        *engine = TranscriptEngine::new(geometry, engine.motion())
            .with_render_config(engine.render_config().clone())
            .with_structure_config(engine.structure().config().clone())
            .with_scrollbar(engine.scrollbar());
        // The new engine's `SessionView` presents live text like the one it replaces (D13).
        let _empty = self.engine.set_streaming_policy(self.streaming, at);
        self.invalidated = true;
        let Some(session) = session else {
            return;
        };
        let update = self.engine.attach(at);
        self.absorb(update);
        if let Some(sequence) = self.data.take_opening_anchor(session) {
            let _ = self.jump_to_sequence(sequence, at);
        } else if let Some(position) = self.data.saved_position(session).cloned() {
            let _ = self.navigate(NavIntent::RestorePosition { position }, at);
        }
    }

    /// While a session open is in flight (`opening` is the session and the canonical sequence
    /// it asked to show, such as a search hit): that sequence is shown once the session
    /// attaches, instead of its saved position. A later open without one clears it.
    pub fn note_opening_anchor(&mut self, opening: Option<(SessionId, Option<u64>)>) {
        if let Some((session, anchor)) = opening {
            self.data
                .set_opening_anchor(anchor.map(|sequence| (session, sequence)));
        }
    }

    /// Ingest one session stream update (durable and live events, resynchronization) for the
    /// attached session. Returns whether the transcript changed.
    ///
    /// Events of other sessions, queue and submission-record updates and runtime work are
    /// host-shell state (W2) and leave the transcript unchanged.
    ///
    /// `own_submission` is the host shell's decision whether the update is the durable user
    /// message accepting one of this client's pending submissions; only those move the reader
    /// (T2, T27, C8, C10).
    pub fn ingest_stream_update(
        &mut self,
        update: &SessionStreamUpdate,
        own_submission: bool,
        at: Instant,
    ) -> bool {
        let Some(session) = self.session else {
            return false;
        };
        match update {
            SessionStreamUpdate::Event(event) => live_input(session, event)
                .is_some_and(|input| self.apply_live_input(input, own_submission, at)),
            SessionStreamUpdate::Resynchronized { session_id, .. } if *session_id == session => {
                self.resynchronize(at)
            }
            SessionStreamUpdate::Resynchronized { .. }
            | SessionStreamUpdate::Submission { .. }
            | SessionStreamUpdate::QueueStatus { .. }
            | SessionStreamUpdate::QueueUnavailable { .. }
            | SessionStreamUpdate::ResyncStarted { .. }
            | SessionStreamUpdate::ManagedOwnershipLost { .. } => false,
        }
    }

    /// Apply an effect result owned by the V2 host ([`owns_effect_result`]): page and window
    /// results (W3). Results for another session or attach epoch, of another window generation
    /// or no longer in flight are dropped (generation fencing); a failed read leaves its request
    /// unanswered. Returns whether the transcript changed.
    pub fn apply_effect_result(&mut self, result: TuiEffectResult, at: Instant) -> bool {
        let TuiEffectResult::TranscriptV2HistoryLoaded { load, result } = result else {
            return false;
        };
        if Some(load.session_id) != self.session || load.epoch != self.data.epoch() {
            return false;
        }
        let loaded = match result {
            Ok(loaded) => loaded,
            Err(error) => {
                // The engine keeps the request in flight (it has no failure input): re-issue
                // it with backoff while the engine still awaits it, so one failed read never
                // leaves the window stuck or holds live input forever.
                let awaited = self.engine.is_awaiting(&load.request);
                tracing::warn!(
                    session_id = %load.session_id,
                    request = ?load.request,
                    %error,
                    retry = awaited,
                    "transcript history request failed"
                );
                if awaited {
                    let _due = self.data.schedule_retry(load.lane(), load.request, at);
                }
                return false;
            }
        };
        self.data.clear_failures(load.lane());
        let engine = &mut self.engine;
        let applied = match &loaded {
            HistoryLoaded::Page(page) => engine.apply_page(page, at),
            HistoryLoaded::Window(window) => engine.apply_window(window, at),
        };
        let mut changed = match applied {
            Ok(update) => {
                self.absorb(update);
                self.record_content_change();
                true
            }
            Err(stale) => {
                tracing::debug!(request = ?load.request, ?stale, "stale transcript history result");
                false
            }
        };
        changed |= self.release_held_inputs(at);
        changed
    }

    /// History requests issued by the engine and not yet executed.
    #[must_use]
    pub fn pending_requests(&self) -> &[HostRequest] {
        &self.pending_requests
    }

    /// Convert pending history requests into host effects to start, skipping requests the
    /// engine no longer awaits. Each effect supersedes earlier work in its lane
    /// ([`super::effects::HistoryLane`]).
    pub fn drain_effects(&mut self) -> Vec<TuiEffect> {
        self.take_history_effects()
    }

    /// Apply the host shell's streaming-presentation policy (`[tui] streaming`, the streaming
    /// configurator's override or reset) to the engine, which paces live text with it exactly
    /// as the host shell's `SessionView` does (D13). An unchanged policy is a no-op; switching to immediate
    /// presentation shows pending text in this update.
    pub fn sync_streaming_policy(
        &mut self,
        policy: bcode_session_view_models::StreamingPresentationPolicy,
        at: Instant,
    ) {
        let policy = policy.normalized();
        if policy == self.streaming {
            return;
        }
        self.streaming = policy;
        let update = self.engine.set_streaming_policy(policy, at);
        self.absorb_presented_text(update);
    }

    /// Record released or flushed paced text: a content change of the streaming units.
    fn absorb_presented_text(&mut self, update: bcode_tui_transcript::engine::EngineUpdate) {
        let changed = update.invalidated;
        self.absorb(update);
        if changed {
            self.record_content_change();
        }
    }

    /// Earliest engine timer (motion frames, grace expiry, paced live text) or history retry
    /// the host must wake for.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        [
            self.engine.next_deadline(),
            self.data.next_retry(),
            self.engine.next_streaming_deadline(self.now()),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Fire due engine timers at `now`, release paced live text due at `now` (D13) and
    /// re-issue due history retries the engine still awaits (a superseded request is dropped).
    pub fn advance(&mut self, now: Instant) {
        let update = self.engine.advance(now);
        self.absorb(update);
        let presented = self.engine.advance_streaming(now);
        self.absorb_presented_text(presented);
        for request in self.data.take_due_retries(now) {
            if self.engine.is_awaiting(&request) {
                self.pending_requests.push(request);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use bcode_client::ClientError;
    use bcode_ipc::Event as BcodeEvent;
    use bcode_session_models::{ClientId, SessionEvent, SessionEventKind, SessionId};
    use bcode_tui_transcript::engine::HostRequest;
    use bcode_tui_transcript::window::{HistoryWindowResult, WindowMode, WindowRequest};

    use super::TranscriptV2Host;
    use crate::effects::{TuiEffect, TuiEffectResult};
    use crate::history_flow::SessionStreamUpdate;
    use crate::transcript_v2_host::effects::{HistoryLoad, HistoryLoaded};

    fn event(session_id: SessionId, sequence: u64, user: bool) -> SessionEvent {
        SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind: if user {
                SessionEventKind::UserMessage {
                    client_id: ClientId::new(),
                    text: format!("question {sequence}"),
                    admission: bcode_session_models::TurnAdmissionMetadata::default(),
                }
            } else {
                SessionEventKind::AssistantMessage {
                    text: format!("answer {sequence}"),
                }
            },
        }
    }

    /// A short conversation: turns of one user message and one answer.
    fn conversation(session_id: SessionId, last: u64) -> Vec<SessionEvent> {
        (1..=last)
            .map(|sequence| event(session_id, sequence, sequence % 2 == 1))
            .collect()
    }

    fn attached(session: SessionId, at: Instant) -> (TranscriptV2Host, HistoryLoad) {
        let mut host = TranscriptV2Host::new();
        host.sync_geometry(bmux_tui::geometry::Rect::new(0, 0, 60, 20), at);
        host.sync_session(Some(session), at);
        let effects = host.drain_effects();
        let [TuiEffect::LoadTranscriptV2History { load }] = effects.as_slice() else {
            panic!("one tail request, got {} effects", effects.len());
        };
        (host, *load)
    }

    fn tail_result(load: HistoryLoad, events: Vec<SessionEvent>) -> TuiEffectResult {
        let HostRequest::Window(issued) = load.request else {
            panic!("window request");
        };
        let last = events.last().map(|event| event.sequence);
        TuiEffectResult::TranscriptV2HistoryLoaded {
            load,
            result: Ok(HistoryLoaded::Window(HistoryWindowResult {
                generation: issued.generation,
                target: issued.request,
                events,
                has_older: false,
                has_newer: false,
                first_available_sequence: Some(1),
                last_available_sequence: last,
            })),
        }
    }

    #[test]
    fn attaching_a_session_requests_the_tail_window_once() {
        let mut host = TranscriptV2Host::new();
        let at = Instant::now();
        let session = SessionId::new();
        host.sync_session(Some(session), at);
        assert_eq!(host.session(), Some(session));
        assert!(matches!(
            host.pending_requests(),
            [HostRequest::Window(issued)] if issued.request == WindowRequest::Tail
        ));
        host.sync_session(Some(session), at);
        assert_eq!(
            host.pending_requests().len(),
            1,
            "same session does not re-attach"
        );
        host.sync_session(None, at);
        assert_eq!(host.session(), None);
        assert_eq!(host.pending_requests(), []);
    }

    #[test]
    fn attach_window_result_fills_the_engine_and_is_owned_by_v2() {
        let at = Instant::now();
        let session = SessionId::new();
        let (mut host, load) = attached(session, at);
        assert_eq!(load.session_id, session);
        assert!(host.pending_requests().is_empty(), "drained into effects");
        let result = tail_result(load, conversation(session, 6));
        assert!(super::owns_effect_result(&result));
        assert!(host.apply_effect_result(result, at));
        let window = host.engine().window();
        assert_eq!(
            (window.first_sequence, window.last_sequence),
            (Some(1), Some(6))
        );
        assert_eq!(window.mode, WindowMode::Live);
        assert_eq!(
            host.engine()
                .session_view()
                .snapshot()
                .transcript
                .items
                .len(),
            6
        );
    }

    #[test]
    fn stale_foreign_and_failed_results_change_nothing() {
        let at = Instant::now();
        let session = SessionId::new();
        let (mut host, load) = attached(session, at);
        let foreign = HistoryLoad {
            session_id: SessionId::new(),
            ..load
        };
        assert!(!host.apply_effect_result(tail_result(foreign, conversation(session, 4)), at));
        let failed = TuiEffectResult::TranscriptV2HistoryLoaded {
            load,
            result: Err(ClientError::Protocol("offline".to_owned())),
        };
        assert!(!host.apply_effect_result(failed, at));
        // A newer attach supersedes the first request: its result is fenced by generation.
        host.sync_session(None, at);
        host.sync_session(Some(session), at);
        let _ = host.drain_effects();
        assert!(!host.apply_effect_result(tail_result(load, conversation(session, 4)), at));
        assert_eq!(host.engine().window().last_sequence, None);
    }

    /// A failed read is re-issued with backoff while the engine awaits it; live input held
    /// meanwhile is applied once the retried window lands.
    #[test]
    fn a_failed_history_read_is_retried_and_releases_held_input() {
        let at = Instant::now();
        let session = SessionId::new();
        let (mut host, load) = attached(session, at);
        let failed = TuiEffectResult::TranscriptV2HistoryLoaded {
            load,
            result: Err(ClientError::Protocol("offline".to_owned())),
        };
        assert!(!host.apply_effect_result(failed, at));
        let due = host.next_deadline().expect("a retry is scheduled");
        assert_eq!(due, at + crate::transcript_v2_host::data::FIRST_RETRY_DELAY);
        let held =
            SessionStreamUpdate::Event(Box::new(BcodeEvent::Session(event(session, 7, false))));
        assert!(!host.ingest_stream_update(&held, false, at), "held");
        host.advance(at);
        assert_eq!(host.drain_effects().len(), 0, "not due yet");
        host.advance(due);
        let effects = host.drain_effects();
        let [TuiEffect::LoadTranscriptV2History { load: retried }] = effects.as_slice() else {
            panic!("the request is re-issued, got {} effects", effects.len());
        };
        assert_eq!(retried.request, load.request);
        // A second failure doubles the delay.
        let failed = TuiEffectResult::TranscriptV2HistoryLoaded {
            load: *retried,
            result: Err(ClientError::Protocol("offline".to_owned())),
        };
        assert!(!host.apply_effect_result(failed, due));
        assert_eq!(
            host.next_deadline(),
            Some(due + 2 * crate::transcript_v2_host::data::FIRST_RETRY_DELAY)
        );
        assert!(host.apply_effect_result(tail_result(*retried, conversation(session, 6)), due));
        assert_eq!(
            host.engine().window().last_sequence,
            Some(7),
            "held input applied"
        );
        assert_eq!(host.next_deadline(), None, "no retry after a result");
    }

    #[test]
    fn a_retry_of_a_superseded_request_is_dropped() {
        let at = Instant::now();
        let session = SessionId::new();
        let (mut host, load) = attached(session, at);
        assert!(!host.apply_effect_result(
            TuiEffectResult::TranscriptV2HistoryLoaded {
                load,
                result: Err(ClientError::Protocol("offline".to_owned())),
            },
            at,
        ));
        // A resynchronization re-attaches with a newer generation.
        assert!(host.resynchronize(at));
        let _ = host.drain_effects();
        assert!(!host.engine().is_awaiting(&load.request), "superseded");
        host.advance(at + crate::transcript_v2_host::data::MAX_RETRY_DELAY);
        assert_eq!(
            host.drain_effects().len(),
            0,
            "the stale request is not re-issued"
        );
    }

    #[test]
    fn live_events_wait_for_the_attach_window_and_then_apply_once() {
        let at = Instant::now();
        let session = SessionId::new();
        let (mut host, load) = attached(session, at);
        // Sequence 6 is in the window being loaded, 7 arrives after the daemon read.
        for sequence in [6, 7] {
            let update = SessionStreamUpdate::Event(Box::new(BcodeEvent::Session(event(
                session, sequence, false,
            ))));
            assert!(
                !host.ingest_stream_update(&update, false, at),
                "held during the replacement"
            );
        }
        assert!(host.apply_effect_result(tail_result(load, conversation(session, 6)), at));
        let items = &host.engine().session_view().snapshot().transcript.items;
        assert_eq!(
            items.len(),
            7,
            "the held event is applied, the duplicate is not"
        );
        assert_eq!(host.engine().window().last_sequence, Some(7));
    }

    #[test]
    fn events_of_other_sessions_are_not_transcript_input() {
        let at = Instant::now();
        let session = SessionId::new();
        let (mut host, load) = attached(session, at);
        assert!(host.apply_effect_result(tail_result(load, conversation(session, 2)), at));
        let foreign = SessionStreamUpdate::Event(Box::new(BcodeEvent::Session(event(
            SessionId::new(),
            3,
            false,
        ))));
        assert!(!host.ingest_stream_update(&foreign, false, at));
        let own =
            SessionStreamUpdate::Event(Box::new(BcodeEvent::Session(event(session, 3, true))));
        assert!(host.ingest_stream_update(&own, false, at));
        assert_eq!(host.engine().window().last_sequence, Some(3));
    }

    /// A host detached from the latest content: attached, then jumped to sequence 1.
    fn detached(session: SessionId, at: Instant) -> TranscriptV2Host {
        let (mut host, load) = attached(session, at);
        let HostRequest::Window(issued) = load.request else {
            panic!("window request");
        };
        let tail = TuiEffectResult::TranscriptV2HistoryLoaded {
            load,
            result: Ok(HistoryLoaded::Window(HistoryWindowResult {
                generation: issued.generation,
                target: issued.request,
                events: conversation(session, 40).split_off(34),
                has_older: true,
                has_newer: false,
                first_available_sequence: Some(1),
                last_available_sequence: Some(40),
            })),
        };
        assert!(host.apply_effect_result(tail, at));
        assert!(host.jump_to_sequence(1, at));
        let effects = host.drain_effects();
        let [TuiEffect::LoadTranscriptV2History { load }] = effects.as_slice() else {
            panic!("one around request, got {} effects", effects.len());
        };
        let HostRequest::Window(issued) = load.request else {
            panic!("window request");
        };
        let result = TuiEffectResult::TranscriptV2HistoryLoaded {
            load: *load,
            result: Ok(HistoryLoaded::Window(HistoryWindowResult {
                generation: issued.generation,
                target: issued.request,
                events: conversation(session, 6),
                has_older: false,
                has_newer: true,
                first_available_sequence: Some(1),
                last_available_sequence: Some(40),
            })),
        };
        assert!(host.apply_effect_result(result, at));
        assert_eq!(host.engine().window().mode, WindowMode::Detached);
        host
    }

    fn requests_tail(host: &TranscriptV2Host) -> bool {
        host.pending_requests().iter().any(|request| {
            matches!(request, HostRequest::Window(issued) if issued.request == WindowRequest::Tail)
        })
    }

    /// C8/C10: only this client's accepted submission moves a detached reader (T27); a user
    /// message of another client or harness is ordinary newer content.
    #[test]
    fn only_this_clients_accepted_submission_moves_a_detached_reader() {
        let at = Instant::now();
        let session = SessionId::new();
        let user =
            SessionStreamUpdate::Event(Box::new(BcodeEvent::Session(event(session, 41, true))));

        let mut other = detached(session, at);
        let _ = other.ingest_stream_update(&user, false, at);
        assert!(
            !requests_tail(&other),
            "another client's message leaves the reader"
        );
        assert_eq!(other.engine().window().mode, WindowMode::Detached);

        let mut own = detached(session, at);
        let _ = own.ingest_stream_update(&user, true, at);
        assert!(
            requests_tail(&own),
            "T27: the own submission loads the tail"
        );
    }

    fn requested_around(host: &TranscriptV2Host) -> Option<u64> {
        host.pending_requests()
            .iter()
            .rev()
            .find_map(|request| match request {
                HostRequest::Window(issued) => match issued.request {
                    WindowRequest::Around { sequence } => Some(sequence),
                    _ => None,
                },
                HostRequest::Page(_) => None,
            })
    }

    /// T21: leaving a session while reading saves the position; attaching it again restores
    /// it with an `Around` window instead of opening at the latest content.
    #[test]
    fn reopening_a_session_restores_its_reading_position() {
        let at = Instant::now();
        let session = SessionId::new();
        let mut host = detached(session, at);
        let saved = host
            .reading_position()
            .unwrap_or_else(|| panic!("{:?}", host.engine().nav().mode));
        host.sync_session(Some(SessionId::new()), at);
        host.sync_session(Some(session), at);
        let bcode_tui_transcript::navigation::ReadingPosition::At { sequence, .. } = saved else {
            panic!("an anchored position");
        };
        assert_eq!(requested_around(&host), Some(sequence));

        // A session left while following reopens at the latest content.
        let (mut following, load) = attached(session, at);
        assert!(following.apply_effect_result(tail_result(load, conversation(session, 4)), at));
        assert_eq!(following.reading_position(), None);
        following.sync_session(None, at);
        following.sync_session(Some(session), at);
        assert_eq!(requested_around(&following), None);
    }

    /// A session opened at a search hit shows that sequence once it attaches.
    #[test]
    fn an_open_anchor_wins_over_the_saved_position() {
        let at = Instant::now();
        let session = SessionId::new();
        let mut host = detached(session, at);
        host.sync_session(None, at);
        host.note_opening_anchor(Some((session, Some(17))));
        host.sync_session(Some(session), at);
        assert_eq!(requested_around(&host), Some(17));
        // The anchor is spent: a later reopen restores the saved position again.
        host.sync_session(None, at);
        host.note_opening_anchor(Some((session, None)));
        host.sync_session(Some(session), at);
        assert_ne!(requested_around(&host), Some(17));
    }
}
