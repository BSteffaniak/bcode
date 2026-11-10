//! Pushed live transcript input for the browser engine, as bounded long-poll batches.
//!
//! Each leased browser consumer ([`super::browser_consumers`]) owns one [`LiveFeed`]: the
//! canonical durable events and live-only stream events its daemon attachment receives for the
//! session, in arrival order, retained in a bounded ring. A browser polls with a resume cursor
//! (`epoch`, `offset`) and receives the retained inputs after it.
//!
//! Transport contract (retention, acknowledgement, replay and conflict behaviour):
//!
//! * Retention: at most [`LIVE_RETAINED_EVENTS`] inputs and [`LIVE_RETAINED_BYTES`] serialized
//!   bytes per session, only while the consumer lease lives. A feed epoch identifies one
//!   unbroken daemon attachment; every (re)attachment, daemon resync signal or over-sized
//!   input starts a new epoch.
//! * Acknowledgement: the cursor of the previous batch. Polling with it replays nothing older.
//! * Replay: re-polling with an older cursor of the current epoch returns the same retained
//!   inputs again, so retries after a lost response re-deliver rather than skip. Durable
//!   duplicates are dropped by the engine by canonical sequence (SD1); live-only inputs carry no
//!   sequence and are delivered at most once per cursor advance.
//! * Conflict: a cursor of another epoch, ahead of the feed, or older than the retained ring
//!   gets [`Continuity::Gap`] and the current cursor with no inputs. The browser then
//!   rehydrates with bounded history reads (`Window(LiveState)` or `Window(Tail)`); a gap is
//!   never papered over.
//! * Start: a poll without a cursor returns [`Continuity::Start`] and the current cursor at
//!   once. Subscribing before the initial tail read leaves no gap: every input after the
//!   cursor is delivered and earlier ones are in the tail.
//!
//! Long-poll rather than server-sent events: `HyperChad`'s SSE endpoint broadcasts renderer
//! events to renderer subscribers by scope, without per-subscriber cursors or replay, and its
//! application routes return one bounded `Content`. A long-poll keeps this route on the same
//! authentication, origin, size and error-envelope plumbing as every other browser route and
//! makes resume explicit.

use std::{collections::VecDeque, sync::Mutex, time::Duration};

use bcode_session_models::{SessionEvent, SessionId, SessionLiveEvent};
use hyperchad::{renderer::Content, router::RouteRequest};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use super::{error, json_content, parse_body};
use crate::HyperChadAppState;

/// Inputs one live batch carries at most.
pub const LIVE_BATCH_EVENTS: usize = 256;
/// Serialized input bytes one live batch carries at most (one input may use all of it).
pub const LIVE_BATCH_BYTES: usize = 512 * 1024;
/// Inputs a session's feed retains for resume.
pub const LIVE_RETAINED_EVENTS: usize = 1_024;
/// Serialized input bytes a session's feed retains for resume.
pub const LIVE_RETAINED_BYTES: usize = 2 * 1024 * 1024;
/// How long one poll waits for new input before answering with an empty batch.
pub const LIVE_WAIT: Duration = Duration::from_secs(15);

/// Resume position in one feed epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiveCursor {
    /// Feed epoch: one unbroken daemon attachment of the consumer.
    pub epoch: SessionId,
    /// Offset of the last input delivered (0 before the first input of the epoch).
    pub offset: u64,
}

/// How a batch relates to the cursor it answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Continuity {
    /// No cursor was given; subscription starts at the returned cursor.
    Start,
    /// The inputs directly follow the request cursor.
    Continuous,
    /// Inputs between the request cursor and the returned cursor are unavailable.
    Gap,
}

/// One live transcript input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LiveEvent {
    /// A canonical durable event delivered live.
    Durable {
        /// The canonical event.
        event: Box<SessionEvent>,
    },
    /// A live-only event (streams, drafts, live reasoning).
    Stream {
        /// The live-only event.
        event: Box<SessionLiveEvent>,
    },
}

/// Response of the live route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveBatch {
    /// Browser application wire version (currently 1).
    pub schema_version: u16,
    /// Relation to the request cursor.
    pub continuity: Continuity,
    /// Cursor to poll with next.
    pub cursor: LiveCursor,
    /// Inputs in arrival order.
    pub events: Vec<LiveEvent>,
    /// More retained inputs follow `cursor`; poll again at once.
    pub more: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LiveRequest {
    session_id: SessionId,
    #[serde(default)]
    cursor: Option<LiveCursor>,
}

struct Entry {
    offset: u64,
    bytes: usize,
    event: LiveEvent,
}

struct FeedState {
    epoch: SessionId,
    head: u64,
    entries: VecDeque<Entry>,
    bytes: usize,
}

impl FeedState {
    /// Append one input of `bytes` serialized bytes, evicting the oldest beyond retention.
    fn append(&mut self, event: LiveEvent, bytes: usize) {
        self.head = self.head.saturating_add(1);
        self.bytes += bytes;
        self.entries.push_back(Entry {
            offset: self.head,
            bytes,
            event,
        });
        while self.entries.len() > LIVE_RETAINED_EVENTS || self.bytes > LIVE_RETAINED_BYTES {
            let Some(evicted) = self.entries.pop_front() else {
                break;
            };
            self.bytes -= evicted.bytes;
        }
    }

    const fn cursor(&self) -> LiveCursor {
        LiveCursor {
            epoch: self.epoch,
            offset: self.head,
        }
    }
}

/// Bounded live input ring of one session consumer.
pub struct LiveFeed {
    state: Mutex<FeedState>,
    version: watch::Sender<u64>,
}

impl Default for LiveFeed {
    fn default() -> Self {
        Self {
            state: Mutex::new(FeedState {
                epoch: SessionId::new(),
                head: 0,
                entries: VecDeque::new(),
                bytes: 0,
            }),
            version: watch::channel(0).0,
        }
    }
}

impl LiveFeed {
    fn state(&self) -> std::sync::MutexGuard<'_, FeedState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn changed(&self) {
        self.version
            .send_modify(|version| *version = version.wrapping_add(1));
    }

    /// Start a new epoch: earlier cursors get [`Continuity::Gap`].
    pub fn reset(&self) {
        {
            let mut state = self.state();
            state.epoch = SessionId::new();
            state.head = 0;
            state.entries.clear();
            state.bytes = 0;
        }
        self.changed();
    }

    /// Record the transcript input of a daemon event for `session`; other events are ignored.
    pub fn observe(&self, session: SessionId, event: &bcode_ipc::Event) {
        let event = match event {
            bcode_ipc::Event::Session(event) if event.session_id == session => LiveEvent::Durable {
                event: Box::new(event.clone()),
            },
            bcode_ipc::Event::SessionLive(event) if event.session_id == session => {
                LiveEvent::Stream {
                    event: Box::new(event.clone()),
                }
            }
            bcode_ipc::Event::SessionViewResyncRequired { session_id }
                if *session_id == session =>
            {
                return self.reset();
            }
            _ => return,
        };
        self.push(event);
    }

    fn push(&self, event: LiveEvent) {
        let mut counter = Counter(0);
        if serde_json::to_writer(&mut counter, &event).is_err() || counter.0 > LIVE_BATCH_BYTES {
            // An input no batch can carry is a gap, recovered by bounded history reads.
            return self.reset();
        }
        self.state().append(event, counter.0);
        self.changed();
    }

    /// The batch answering `cursor`, or `None` when nothing follows it yet.
    fn read(&self, cursor: Option<LiveCursor>) -> Option<LiveBatch> {
        let state = self.state();
        let batch = |continuity, cursor, events, more| LiveBatch {
            schema_version: 1,
            continuity,
            cursor,
            events,
            more,
        };
        let Some(cursor) = cursor else {
            return Some(batch(Continuity::Start, state.cursor(), Vec::new(), false));
        };
        let head = state.head;
        let first_retained = state
            .entries
            .front()
            .map_or_else(|| head.saturating_add(1), |entry| entry.offset);
        let known = cursor.epoch == state.epoch
            && cursor.offset <= head
            && cursor.offset.saturating_add(1) >= first_retained;
        if !known {
            return Some(batch(Continuity::Gap, state.cursor(), Vec::new(), false));
        }
        if cursor.offset == state.head {
            return None;
        }
        let mut events = Vec::new();
        let mut bytes = 0;
        let mut offset = cursor.offset;
        for entry in state
            .entries
            .iter()
            .filter(|entry| entry.offset > cursor.offset)
        {
            if events.len() == LIVE_BATCH_EVENTS
                || (!events.is_empty() && bytes + entry.bytes > LIVE_BATCH_BYTES)
            {
                break;
            }
            bytes += entry.bytes;
            offset = entry.offset;
            events.push(entry.event.clone());
        }
        let next = LiveCursor {
            epoch: state.epoch,
            offset,
        };
        Some(batch(
            Continuity::Continuous,
            next,
            events,
            offset < state.head,
        ))
    }

    /// Answer `cursor`, waiting up to `wait` for input; an empty continuous batch on timeout.
    async fn poll(&self, cursor: Option<LiveCursor>, wait: Duration) -> LiveBatch {
        // Subscribe before reading so an input arriving in between still wakes the wait.
        let mut version = self.version.subscribe();
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            version.borrow_and_update();
            if let Some(batch) = self.read(cursor) {
                return batch;
            }
            let woke = tokio::time::timeout_at(deadline, version.changed()).await;
            if !matches!(woke, Ok(Ok(()))) {
                return self.read(cursor).unwrap_or_else(|| LiveBatch {
                    schema_version: 1,
                    continuity: Continuity::Continuous,
                    cursor: cursor.unwrap_or_else(|| self.state().cursor()),
                    events: Vec::new(),
                    more: false,
                });
            }
        }
    }
}

/// Counts serialized bytes without allocating them.
struct Counter(usize);

impl std::io::Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl HyperChadAppState {
    pub(super) async fn browser_live(&self, request: RouteRequest) -> Content {
        self.browser_live_with_wait(request, LIVE_WAIT).await
    }

    async fn browser_live_with_wait(&self, request: RouteRequest, wait: Duration) -> Content {
        if !self.authorizes(&request) {
            return error("unauthorized");
        }
        let live: LiveRequest = match parse_body(&request) {
            Ok(value) => value,
            Err(code) => return error(code),
        };
        // Bound concurrent waiters without an unbounded queue; a busy poll simply retries.
        let Some(_waiter) = self.browser_consumers.admit_live_waiter(live.session_id) else {
            return error("browser_busy");
        };
        let Ok(Ok(feed)) = tokio::time::timeout(
            super::BACKEND_TIMEOUT,
            self.browser_consumers.renew(&self.client, live.session_id),
        )
        .await
        else {
            return error("session_unavailable");
        };
        json_content(&feed.poll(live.cursor, wait).await)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn durable(session: SessionId, sequence: u64, text: &str) -> bcode_ipc::Event {
        bcode_ipc::Event::Session(SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id: session,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::AssistantMessage { text: text.into() },
        })
    }

    fn sequences(batch: &LiveBatch) -> Vec<u64> {
        batch
            .events
            .iter()
            .map(|event| match event {
                LiveEvent::Durable { event } => event.sequence,
                LiveEvent::Stream { .. } => 0,
            })
            .collect()
    }

    #[test]
    fn start_then_resume_replays_from_the_cursor_and_tolerates_retries() {
        let session = SessionId::new();
        let feed = LiveFeed::default();
        let start = feed.read(None).unwrap();
        assert_eq!(start.continuity, Continuity::Start);
        assert_eq!(start.events.len(), 0);
        assert!(feed.read(Some(start.cursor)).is_none(), "nothing yet: wait");
        for sequence in 1..=3 {
            feed.observe(session, &durable(session, sequence, "x"));
        }
        feed.observe(SessionId::new(), &durable(SessionId::new(), 9, "foreign"));
        let first = feed.read(Some(start.cursor)).unwrap();
        assert_eq!(first.continuity, Continuity::Continuous);
        assert_eq!(sequences(&first), [1, 2, 3]);
        assert!(!first.more);
        // A lost response is retried with the old cursor: the same inputs again, in order.
        let retried = feed.read(Some(start.cursor)).unwrap();
        assert_eq!(retried, first);
        feed.observe(session, &durable(session, 4, "y"));
        let next = feed.read(Some(first.cursor)).unwrap();
        assert_eq!(sequences(&next), [4]);
    }

    #[test]
    fn foreign_evicted_or_future_cursors_report_a_gap_without_inputs() {
        let session = SessionId::new();
        let feed = LiveFeed::default();
        let start = feed.read(None).unwrap().cursor;
        for sequence in 1..=u64::try_from(LIVE_RETAINED_EVENTS).unwrap() + 2 {
            feed.observe(session, &durable(session, sequence, "x"));
        }
        let evicted = feed.read(Some(start)).unwrap();
        assert_eq!(evicted.continuity, Continuity::Gap);
        assert_eq!(evicted.events.len(), 0);
        let resumed = feed.read(Some(evicted.cursor));
        assert!(resumed.is_none(), "the gap cursor is current");
        let foreign = LiveCursor {
            epoch: SessionId::new(),
            offset: 1,
        };
        assert_eq!(
            feed.read(Some(foreign)).unwrap().continuity,
            Continuity::Gap
        );
        let ahead = LiveCursor {
            offset: evicted.cursor.offset + 1,
            ..evicted.cursor
        };
        assert_eq!(feed.read(Some(ahead)).unwrap().continuity, Continuity::Gap);
        // A reattachment or resync starts a new epoch.
        feed.observe(
            session,
            &bcode_ipc::Event::SessionViewResyncRequired {
                session_id: session,
            },
        );
        let after_resync = feed.read(Some(evicted.cursor)).unwrap();
        assert_eq!(after_resync.continuity, Continuity::Gap);
        assert_ne!(after_resync.cursor.epoch, evicted.cursor.epoch);
    }

    #[test]
    fn batches_and_retention_are_bounded() {
        let session = SessionId::new();
        let feed = LiveFeed::default();
        let start = feed.read(None).unwrap().cursor;
        for sequence in 1..=300 {
            feed.observe(session, &durable(session, sequence, "x"));
        }
        let first = feed.read(Some(start)).unwrap();
        assert_eq!(first.events.len(), LIVE_BATCH_EVENTS);
        assert!(first.more);
        let rest = feed.read(Some(first.cursor)).unwrap();
        assert_eq!(rest.events.len(), 300 - LIVE_BATCH_EVENTS);
        assert!(!rest.more);
        // Large inputs: each batch stays within the byte budget, the ring within its own.
        let large = "x".repeat(200 * 1024);
        let before = feed.read(None).unwrap().cursor;
        for sequence in 301..=320 {
            feed.observe(session, &durable(session, sequence, &large));
        }
        let state = feed.state();
        assert!(state.bytes <= LIVE_RETAINED_BYTES);
        drop(state);
        assert_eq!(feed.read(Some(before)).unwrap().continuity, Continuity::Gap);
        let oldest = feed.state().entries.front().unwrap().offset - 1;
        let cursor = LiveCursor {
            epoch: before.epoch,
            offset: oldest,
        };
        let batch = feed.read(Some(cursor)).unwrap();
        let bytes: usize = batch
            .events
            .iter()
            .map(|event| serde_json::to_vec(event).unwrap().len())
            .sum();
        assert!(!batch.events.is_empty() && bytes <= LIVE_BATCH_BYTES);
        assert!(batch.more, "the rest follows in later batches");
        // An input larger than any batch is a gap, never a truncated input.
        let epoch = feed.read(None).unwrap().cursor.epoch;
        feed.observe(
            session,
            &durable(session, 400, &"x".repeat(LIVE_BATCH_BYTES)),
        );
        assert_ne!(feed.read(None).unwrap().cursor.epoch, epoch);
    }

    #[tokio::test]
    async fn polls_wake_on_input_and_time_out_with_an_empty_batch() {
        let session = SessionId::new();
        let feed = Arc::new(LiveFeed::default());
        let start = feed.read(None).unwrap().cursor;
        let idle = feed.poll(Some(start), Duration::from_millis(10)).await;
        assert_eq!(
            (idle.continuity, idle.cursor, idle.events.len()),
            (Continuity::Continuous, start, 0)
        );
        let waiting = tokio::spawn({
            let feed = Arc::clone(&feed);
            async move { feed.poll(Some(start), Duration::from_secs(10)).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        feed.observe(session, &durable(session, 1, "woke"));
        let woke = tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(sequences(&woke), [1]);
    }

    #[tokio::test]
    async fn live_route_rejects_before_backend_access() {
        let state = HyperChadAppState::new(bcode_client::BcodeClient::default_endpoint(), "secret");
        let decode = |content: Content| {
            let Content::Raw { data, .. } = content else {
                panic!("expected raw JSON")
            };
            serde_json::from_slice::<serde_json::Value>(&data).unwrap()["error"].clone()
        };
        let request = |token: &str, method: &str, body: &str| {
            let mut request = RouteRequest::from_path(
                &format!("/api/session-view/live?token={token}"),
                hyperchad::router::RequestInfo::default(),
            );
            request.method = method.parse().unwrap();
            request
                .headers
                .insert("content-type".to_owned(), "application/json".to_owned());
            request.body = Some(Arc::new(body.as_bytes().to_vec().into()));
            request
        };
        let session = SessionId::new();
        let valid = serde_json::json!({"session_id": session}).to_string();
        for (token, method, body, expected) in [
            ("wrong", "POST", valid.as_str(), "unauthorized"),
            ("secret", "GET", valid.as_str(), "method_not_allowed"),
            ("secret", "POST", "{}", "invalid_request"),
            (
                "secret",
                "POST",
                &serde_json::json!({"session_id": session, "extra": 1}).to_string(),
                "invalid_request",
            ),
            (
                "secret",
                "POST",
                &" ".repeat(super::super::MAX_REQUEST_BYTES + 1),
                "request_too_large",
            ),
        ] {
            assert_eq!(
                decode(
                    state
                        .browser_live_with_wait(request(token, method, body), Duration::ZERO)
                        .await
                ),
                expected
            );
        }
    }
}
