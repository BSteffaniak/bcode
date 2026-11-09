//! Session data into the V2 engine (spec sections 3.2, 4.4 and 5; W2, W3, W10, W11, T2, T27).
//!
//! Maps host inputs to engine inputs:
//!
//! * Canonical session events and live-only events of the attached session become
//!   [`LiveInput`]s. The engine applies them to a `Live` window and records newer activity for
//!   a `Detached` one (W2).
//! * A durable user message that accepts one of this client's pending submissions is a
//!   semantically accepted submission: after it is applied, the engine reduces
//!   `SubmissionAccepted` for its unit (T2; T27 loads the tail window first when the window is
//!   detached). User messages of other clients and harnesses are ordinary live content (C8,
//!   C10), and staged submissions never reach the engine.
//! * Timeline jumps become `JumpToSequence` intents (`Around` windows); session start and
//!   latest (Home/End, `ctrl+home`/`ctrl+end`, FO4/FO5) are input intents. The window
//!   requests they issue are executed by the host ([`super::effects`]).
//!
//! While a `Head`, `Tail` or `Around` replacement is in flight, live inputs are held by the
//! host and applied after the replacement is incorporated, so a live event that arrives
//! between the daemon read and the result is never lost or applied to the window being
//! replaced. Durable events the replacement already contains are dropped by the engine (SD1).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use bcode_ipc::Event as BcodeEvent;
use bcode_session_models::{SessionEventKind, SessionId};
use bcode_session_view_models::{TranscriptViewItemId, TranscriptViewItemKind};
use bcode_tui_transcript::engine::{HostRequest, TranscriptEngine};
use bcode_tui_transcript::navigation::{
    NavIntent, NavMode, Placement, ReadingAnchor, ReadingPosition,
};
use bcode_tui_transcript::units::{BoundaryEdge, UnitKey};
use bcode_tui_transcript::window::{LIVE_BUFFER_EVENTS, LiveInput, WindowMode};

use super::TranscriptV2Host;
use super::effects::{HistoryLane, HistoryLoad};
use crate::effects::TuiEffect;

/// A live input held while a window replacement is in flight.
#[derive(Debug)]
struct HeldInput {
    input: LiveInput,
    /// Decided on arrival: whether the input accepts this client's own submission. The host
    /// shell forgets the pending submission once it is accepted, so the decision cannot be
    /// made again when the held input is applied.
    own_submission: bool,
}

/// Data-side host bookkeeping.
#[derive(Debug, Default)]
pub struct HostData {
    /// Live inputs that arrived while a window replacement was in flight, in arrival order.
    held: Vec<HeldInput>,
    /// More than [`LIVE_BUFFER_EVENTS`] inputs arrived while held; the tail is re-attached
    /// once the replacement is incorporated.
    held_overflowed: bool,
    /// Bumped by every content change the host applied (plugin visual reconciliation).
    content_generation: u64,
    /// Bumped by every engine reset; history results of an earlier epoch are dropped.
    epoch: u64,
    /// Failed history requests the engine still awaits, re-issued when due (at most one per
    /// lane, so bounded).
    retries: BTreeMap<HistoryLane, HistoryRetry>,
    /// Consecutive failures per lane, for the retry backoff; cleared by a result.
    failures: BTreeMap<HistoryLane, u32>,
    /// Reading positions of sessions left while reading (T21), kept across engine resets and
    /// bounded by [`MAX_SAVED_POSITIONS`].
    saved_positions: BTreeMap<SessionId, ReadingPosition>,
    /// Canonical sequence an in-flight session open asked to show (a search hit), applied when
    /// that session attaches.
    opening_anchor: Option<(SessionId, u64)>,
}

/// Sessions whose reading position is remembered.
pub const MAX_SAVED_POSITIONS: usize = 64;

/// A failed history request waiting to be re-issued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HistoryRetry {
    request: HostRequest,
    due: Instant,
}

/// Delay before the first re-issue of a failed history request; doubled for every further
/// consecutive failure in the lane up to [`MAX_RETRY_DELAY`].
pub const FIRST_RETRY_DELAY: Duration = Duration::from_millis(250);
/// Longest delay between re-issues of a failing history request.
pub const MAX_RETRY_DELAY: Duration = Duration::from_secs(8);

impl HostData {
    /// Forget held inputs and start a new attach epoch (engine reset on a session change).
    pub fn reset(&mut self) {
        self.held.clear();
        self.held_overflowed = false;
        self.retries.clear();
        self.failures.clear();
        self.content_generation = self.content_generation.wrapping_add(1);
        self.epoch = self.epoch.wrapping_add(1);
    }

    /// Attach epoch: bumped by every engine reset ([`Self::reset`]).
    #[must_use]
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Generation of applied content; changes whenever resident items may have changed.
    #[must_use]
    pub const fn content_generation(&self) -> u64 {
        self.content_generation
    }

    const fn content_changed(&mut self) {
        self.content_generation = self.content_generation.wrapping_add(1);
    }

    /// Remember `position` for `session`, or forget it when the reader follows the latest
    /// content. The oldest-keyed entry goes first when the bound is reached.
    pub fn save_position(&mut self, session: SessionId, position: Option<ReadingPosition>) {
        match position {
            Some(position) => {
                if !self.saved_positions.contains_key(&session)
                    && self.saved_positions.len() >= MAX_SAVED_POSITIONS
                {
                    let _evicted = self.saved_positions.pop_first();
                }
                self.saved_positions.insert(session, position);
            }
            None => {
                self.saved_positions.remove(&session);
            }
        }
    }

    /// The reading position remembered for `session`.
    #[must_use]
    pub fn saved_position(&self, session: SessionId) -> Option<&ReadingPosition> {
        self.saved_positions.get(&session)
    }

    /// Record the sequence an in-flight open of `session` asked to show.
    pub const fn set_opening_anchor(&mut self, anchor: Option<(SessionId, u64)>) {
        self.opening_anchor = anchor;
    }

    /// Take the opening anchor if it belongs to `session`.
    pub fn take_opening_anchor(&mut self, session: SessionId) -> Option<u64> {
        match self.opening_anchor {
            Some((anchored, sequence)) if anchored == session => {
                self.opening_anchor = None;
                Some(sequence)
            }
            _ => None,
        }
    }

    /// Schedule `request`, which failed at `at`, for a re-issue after the lane's backoff
    /// delay. Returns when it is due.
    pub fn schedule_retry(
        &mut self,
        lane: HistoryLane,
        request: HostRequest,
        at: Instant,
    ) -> Instant {
        let failures = self.failures.entry(lane).or_default();
        let delay = FIRST_RETRY_DELAY
            .saturating_mul(1_u32 << (*failures).min(5))
            .min(MAX_RETRY_DELAY);
        *failures = failures.saturating_add(1);
        let due = at + delay;
        self.retries.insert(lane, HistoryRetry { request, due });
        due
    }

    /// A result arrived in `lane`: its failure streak ends.
    pub fn clear_failures(&mut self, lane: HistoryLane) {
        self.failures.remove(&lane);
        self.retries.remove(&lane);
    }

    /// Earliest due retry.
    #[must_use]
    pub fn next_retry(&self) -> Option<Instant> {
        self.retries.values().map(|retry| retry.due).min()
    }

    /// Remove and return the retries due at `now`.
    pub fn take_due_retries(&mut self, now: Instant) -> Vec<HostRequest> {
        let (due, waiting): (BTreeMap<_, _>, BTreeMap<_, _>) = std::mem::take(&mut self.retries)
            .into_iter()
            .partition(|(_, retry)| retry.due <= now);
        self.retries = waiting;
        due.into_values().map(|retry| retry.request).collect()
    }
}

/// The live input of a daemon event for `session`, if it is transcript input for it.
///
/// Canonical session events are durable inputs and live-only events stream inputs. Runtime
/// work, workflow and catalog notifications are host-shell state, not transcript input (W2).
#[must_use]
pub fn live_input(session: SessionId, event: &BcodeEvent) -> Option<LiveInput> {
    match event {
        BcodeEvent::Session(event) if event.session_id == session => {
            Some(LiveInput::Durable(Box::new(event.clone())))
        }
        BcodeEvent::SessionLive(event) if event.session_id == session => {
            Some(LiveInput::Stream(Box::new(event.clone())))
        }
        BcodeEvent::Session(_)
        | BcodeEvent::SessionLive(_)
        | BcodeEvent::RuntimeWork(_)
        | BcodeEvent::Workflow(_)
        | BcodeEvent::SessionViewResyncRequired { .. }
        | BcodeEvent::SessionCatalogUpdated { .. } => None,
    }
}

/// Canonical sequence of a durable user message: a semantically accepted submission.
#[must_use]
pub const fn accepted_submission(input: &LiveInput) -> Option<u64> {
    match input {
        LiveInput::Durable(event) => match event.kind {
            SessionEventKind::UserMessage { .. } => Some(event.sequence),
            _ => None,
        },
        LiveInput::Stream(_) => None,
    }
}

/// The unit of the user message with canonical `sequence`: the resident item that sequence
/// produced, else the event-owned item identity the session view gives plain user messages
/// (the item is not resident while the window is detached, T27).
#[must_use]
pub fn submission_unit(engine: &TranscriptEngine, sequence: u64) -> UnitKey {
    engine
        .session_view()
        .snapshot()
        .transcript
        .items
        .iter()
        .rev()
        .find(|item| {
            item.sequence == Some(sequence)
                && matches!(item.kind, TranscriptViewItemKind::UserMessage { .. })
        })
        .map_or_else(
            || UnitKey::Item(TranscriptViewItemId::event(sequence)),
            |item| UnitKey::Item(item.id.clone()),
        )
}

impl TranscriptV2Host {
    /// Apply one live input for the attached session (W2, W11), then the submission it
    /// accepts when `own_submission` says it accepts one of this client's pending submissions
    /// (T2, T27, C8, C10). Inputs are held while a window replacement is in flight. Returns
    /// whether the transcript changed.
    pub(super) fn apply_live_input(
        &mut self,
        input: LiveInput,
        own_submission: bool,
        at: Instant,
    ) -> bool {
        if self.engine.window().in_flight.replace.is_some() {
            if self.data.held.len() < LIVE_BUFFER_EVENTS {
                self.data.held.push(HeldInput {
                    input,
                    own_submission,
                });
            } else {
                self.data.held_overflowed = true;
            }
            return false;
        }
        let accepted = accepted_submission(&input).filter(|_| own_submission);
        let engine = &mut self.engine;
        let mut update = engine.apply_live(input, at);
        if let Some(sequence) = accepted {
            let item = submission_unit(engine, sequence);
            let accepted = engine.submission_accepted(item, at);
            update.requests.extend(accepted.requests);
            update.invalidated |= accepted.invalidated;
        }
        let changed = update.invalidated;
        self.absorb(update);
        if changed {
            self.data.content_changed();
        }
        changed
    }

    /// Apply inputs held during a window replacement once none is in flight any more. An
    /// overflowed hold re-attaches the tail of a live window (W10). Returns whether the
    /// transcript changed.
    pub(super) fn release_held_inputs(&mut self, at: Instant) -> bool {
        if self.engine.window().in_flight.replace.is_some() {
            return false;
        }
        let held = std::mem::take(&mut self.data.held);
        let overflowed = std::mem::replace(&mut self.data.held_overflowed, false);
        let mut changed = false;
        for held in held {
            changed |= self.apply_live_input(held.input, held.own_submission, at);
        }
        if overflowed && self.engine.window().mode == WindowMode::Live {
            let update = self.engine.attach(at);
            self.absorb(update);
            changed = true;
        }
        changed
    }

    /// Continuity was re-established after the event stream was lost: a live window reloads
    /// the tail (W10), which also rehydrates in-flight live state; a detached window loads
    /// newer history on demand and needs nothing. Returns whether a request was issued.
    pub(super) fn resynchronize(&mut self, at: Instant) -> bool {
        if self.engine.window().mode != WindowMode::Live {
            return false;
        }
        self.data.held.clear();
        self.data.held_overflowed = false;
        let update = self.engine.attach(at);
        self.absorb(update);
        true
    }

    /// Reduce one navigation intent (keys, timeline, focus traversal). Window requests it
    /// issues are executed by the host. Returns whether the transcript changed.
    pub fn navigate(&mut self, intent: NavIntent, at: Instant) -> bool {
        let update = self.engine.intent(intent, at);
        let changed = update.invalidated || !update.requests.is_empty();
        self.absorb(update);
        changed
    }

    /// The reader's position to save when the session is left (T21): `None` while following
    /// the latest content or before anything is shown.
    #[must_use]
    pub fn reading_position(&self) -> Option<ReadingPosition> {
        let engine = &self.engine;
        let anchor = match &engine.nav().mode {
            NavMode::Following => return None,
            NavMode::Holding { item, .. } => ReadingAnchor::top(item.clone()),
            NavMode::Reading { anchor } | NavMode::TailSpace { anchor } => anchor.clone(),
        };
        // A boundary row has no sequence and is not stable across windows: it stands for the
        // top of the resident edge unit it borders.
        let window = engine.window();
        let (sequence, anchor) = match &anchor.unit {
            UnitKey::Boundary(edge) => {
                let sequence = match edge {
                    BoundaryEdge::Older => window.first_sequence,
                    BoundaryEdge::Newer => window.last_sequence,
                }?;
                let unit = engine.units().unit_for_sequence(sequence)?.clone();
                (sequence, ReadingAnchor::top(unit))
            }
            unit => (engine.units().sequence(unit)?, anchor.clone()),
        };
        Some(ReadingPosition::At { sequence, anchor })
    }

    /// Timeline jump to a canonical sequence, resident or not (`Around` window, W8, W10).
    pub fn jump_to_sequence(&mut self, sequence: u64, at: Instant) -> bool {
        self.navigate(
            NavIntent::JumpToSequence {
                sequence,
                placement: Placement::ItemTopAtTop,
            },
            at,
        )
    }

    /// Start the history requests the engine is still waiting for as host effects, one per
    /// request. Requests the engine no longer awaits (superseded by a replacement, an
    /// eviction or a cancelled jump) are dropped; their results would be stale.
    pub(super) fn take_history_effects(&mut self) -> Vec<TuiEffect> {
        let requests = std::mem::take(&mut self.pending_requests);
        let Some(session_id) = self.session else {
            return Vec::new();
        };
        let engine = &self.engine;
        let resident_last = engine.window().last_sequence;
        let epoch = self.data.epoch();
        requests
            .into_iter()
            .filter(|request| engine.is_awaiting(request))
            .map(|request: HostRequest| TuiEffect::LoadTranscriptV2History {
                load: HistoryLoad {
                    session_id,
                    request,
                    resident_last,
                    epoch,
                },
            })
            .collect()
    }

    /// Record that content was applied outside live input (page and window results).
    pub(super) const fn record_content_change(&mut self) {
        self.data.content_changed();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use bcode_ipc::Event as BcodeEvent;
    use bcode_session_models::{
        ClientId, SessionEvent, SessionEventKind, SessionId, SessionLiveEvent, SessionLiveEventKind,
    };
    use bcode_session_view_models::TranscriptViewItemId;
    use bcode_tui_transcript::engine::TranscriptEngine;
    use bcode_tui_transcript::navigation::MotionConfig;
    use bcode_tui_transcript::units::UnitKey;
    use bcode_tui_transcript::viewport::ViewportGeometry;
    use bcode_tui_transcript::window::LiveInput;

    use super::{accepted_submission, live_input, submission_unit};

    fn event(session_id: SessionId, sequence: u64, kind: SessionEventKind) -> SessionEvent {
        SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind,
        }
    }

    fn user(session_id: SessionId, sequence: u64) -> SessionEvent {
        event(
            session_id,
            sequence,
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: "hello".to_owned(),
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
            },
        )
    }

    #[test]
    fn only_transcript_events_of_the_attached_session_are_live_input() {
        let session = SessionId::new();
        let other = SessionId::new();
        assert!(matches!(
            live_input(session, &BcodeEvent::Session(user(session, 3))),
            Some(LiveInput::Durable(event)) if event.sequence == 3
        ));
        assert!(live_input(session, &BcodeEvent::Session(user(other, 3))).is_none());
        assert!(live_input(session, &BcodeEvent::RuntimeWork(user(session, 4))).is_none());
        let live = SessionLiveEvent {
            session_id: session,
            kind: SessionLiveEventKind::AssistantTextDelta {
                turn_id: "turn".to_owned(),
                segment_id: "segment".to_owned(),
                segment_order: 0,
                text: "partial".to_owned(),
            },
        };
        assert!(matches!(
            live_input(session, &BcodeEvent::SessionLive(live)),
            Some(LiveInput::Stream(_))
        ));
        assert!(live_input(session, &BcodeEvent::SessionCatalogUpdated { revision: 1 }).is_none());
    }

    #[test]
    fn durable_user_messages_accept_their_submission() {
        let session = SessionId::new();
        let accepted = LiveInput::Durable(Box::new(user(session, 9)));
        assert_eq!(accepted_submission(&accepted), Some(9));
        let assistant = LiveInput::Durable(Box::new(event(
            session,
            10,
            SessionEventKind::AssistantMessage {
                text: "reply".to_owned(),
            },
        )));
        assert_eq!(accepted_submission(&assistant), None);
    }

    #[test]
    fn submission_unit_is_the_resident_user_message_or_its_event_identity() {
        let mut engine =
            TranscriptEngine::new(ViewportGeometry::new(40, 10), MotionConfig::INSTANT);
        let session = SessionId::new();
        assert_eq!(
            submission_unit(&engine, 5),
            UnitKey::Item(TranscriptViewItemId::event(5))
        );
        let _ = engine.apply_live(
            LiveInput::Durable(Box::new(user(session, 5))),
            Instant::now(),
        );
        let item = &engine.session_view().snapshot().transcript.items[0];
        assert_eq!(submission_unit(&engine, 5), UnitKey::Item(item.id.clone()));
    }
}
