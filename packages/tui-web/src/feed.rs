//! Wire types of the browser transcript feed served by `bcode web`.
//!
//! The browser engine executes its history requests with the shared bounded loader
//! ([`bcode_tui_transcript::window::load_history`]); each read the loader performs is one
//! [`HistoryPageRequest`] to [`HISTORY_PAGE_ROUTE`]. Live transcript input is polled from
//! [`LIVE_ROUTE`] with a resume [`LiveCursor`]:
//!
//! * Subscribe first (no cursor, [`Continuity::Start`]), then load the tail window: no input
//!   between the two is lost, and durable duplicates are dropped by the engine by sequence.
//! * Poll again with each returned cursor. Re-polling with an older cursor after a lost
//!   response re-delivers the same inputs in the same order.
//! * [`Continuity::Gap`] means inputs between the old cursor and the new one are unavailable
//!   (backend reattachment, resync, or retention exceeded): rehydrate with a bounded history
//!   request (`Window(LiveState)` while live, else nothing beyond the engine's own requests)
//!   and continue from the returned cursor.
//!
//! Snapshots requested with [`SLIM_SNAPSHOT_QUERY`] omit transcript items; permissions,
//! interactions, queue, model status and lifecycle stay on the snapshot path.
//!
//! These are transport types only; the payloads are the engine's own canonical inputs.

use bcode_tui_transcript::window::{
    LiveInput, PageCaps, SessionEvent, SessionHistoryPage, SessionHistoryQuery, SessionId,
    SessionLiveEvent,
};
use serde::{Deserialize, Serialize};

/// Route of one bounded canonical history read (`POST`, JSON [`HistoryPageRequest`]).
pub const HISTORY_PAGE_ROUTE: &str = "/api/session-view/history-page";
/// Route of the live input long-poll (`POST`, JSON [`LiveRequest`]).
pub const LIVE_ROUTE: &str = "/api/session-view/live";
/// Snapshot query parameter that omits transcript items.
pub const SLIM_SNAPSHOT_QUERY: &str = "transcript=omit";
/// Events one history read requests at most: the engine's per-read scan cap.
pub const MAX_HISTORY_READ_EVENTS: usize = PageCaps::WINDOW.max_events_scanned;
// Pages and windows share the per-read cap.
const _: () = assert!(MAX_HISTORY_READ_EVENTS >= PageCaps::PAGE.max_events_scanned);
/// Wire version this browser understands.
pub const FEED_SCHEMA_VERSION: u16 = 1;
/// Largest [`HISTORY_PAGE_ROUTE`] response: one canonical event of up to 16 MiB travels
/// alone; pages of several events stay within [`crate::MAX_RESPONSE_BYTES`].
pub const MAX_HISTORY_RESPONSE_BYTES: usize = 16 * 1024 * 1024 + 64 * 1024;
/// Error code of a history read whose nearest event exceeds the history response bound;
/// retrying cannot succeed.
pub const HISTORY_EVENT_TOO_LARGE: &str = "history_event_too_large";

/// One bounded canonical history read, as the loader issues it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct HistoryPageRequest {
    /// Session to read.
    pub session_id: SessionId,
    /// The loader's query.
    pub query: SessionHistoryQuery,
}

/// One live poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct LiveRequest {
    /// Session to poll.
    pub session_id: SessionId,
    /// Cursor of the previous batch; `None` subscribes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<LiveCursor>,
}

/// Resume position in one backend feed epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveCursor {
    /// Feed epoch: one unbroken backend attachment.
    pub epoch: SessionId,
    /// Offset of the last input delivered in the epoch.
    pub offset: u64,
}

/// How a batch relates to the cursor it answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Continuity {
    /// Subscription starts at the returned cursor.
    Start,
    /// The inputs directly follow the request cursor.
    Continuous,
    /// Inputs between the request cursor and the returned cursor are unavailable.
    Gap,
}

/// A decoded live batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveBatch {
    /// Relation to the request cursor.
    pub continuity: Continuity,
    /// Cursor to poll with next.
    pub cursor: LiveCursor,
    /// Inputs for [`bcode_tui_transcript::engine::TranscriptEngine::apply_live`], in arrival
    /// order.
    pub inputs: Vec<LiveInput>,
    /// More inputs follow `cursor`; poll again at once.
    pub more: bool,
}

/// Why a feed response was not decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedError {
    /// The backend answered with an error envelope code (for example `browser_busy`).
    Server(String),
    /// The response uses a wire version this browser does not understand.
    UnsupportedVersion(u16),
    /// The response exceeds [`crate::MAX_RESPONSE_BYTES`].
    TooLarge,
    /// The response is not a feed response.
    Malformed,
}

impl std::fmt::Display for FeedError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Server(code) => write!(formatter, "backend error: {code}"),
            Self::UnsupportedVersion(version) => {
                write!(formatter, "unsupported feed version {version}")
            }
            Self::TooLarge => formatter.write_str("feed response too large"),
            Self::Malformed => formatter.write_str("malformed feed response"),
        }
    }
}

impl std::error::Error for FeedError {}

#[derive(Deserialize)]
#[serde(untagged)]
enum Reply<T> {
    Error { schema_version: u16, error: String },
    Ok(T),
}

#[derive(Deserialize)]
struct HistoryPageResponse {
    schema_version: u16,
    page: SessionHistoryPage,
}

#[derive(Deserialize)]
struct LiveResponse {
    schema_version: u16,
    continuity: Continuity,
    cursor: LiveCursor,
    events: Vec<LiveEvent>,
    more: bool,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum LiveEvent {
    Durable { event: Box<SessionEvent> },
    Stream { event: Box<SessionLiveEvent> },
}

impl From<LiveEvent> for LiveInput {
    fn from(event: LiveEvent) -> Self {
        match event {
            LiveEvent::Durable { event } => Self::Durable(event),
            LiveEvent::Stream { event } => Self::Stream(event),
        }
    }
}

fn decode<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
    limit: usize,
    version: impl Fn(&T) -> u16,
) -> Result<T, FeedError> {
    if bytes.len() > limit {
        return Err(FeedError::TooLarge);
    }
    match serde_json::from_slice::<Reply<T>>(bytes).map_err(|_| FeedError::Malformed)? {
        Reply::Error {
            schema_version: FEED_SCHEMA_VERSION,
            error,
        } => Err(FeedError::Server(error)),
        Reply::Error { schema_version, .. } => Err(FeedError::UnsupportedVersion(schema_version)),
        Reply::Ok(value) if version(&value) == FEED_SCHEMA_VERSION => Ok(value),
        Reply::Ok(value) => Err(FeedError::UnsupportedVersion(version(&value))),
    }
}

/// Decode a [`HISTORY_PAGE_ROUTE`] response into the page the loader's reader returns.
///
/// # Errors
///
/// * [`FeedError::Server`] for a backend error envelope.
/// * [`FeedError::UnsupportedVersion`] for another wire version.
/// * [`FeedError::TooLarge`] above [`MAX_HISTORY_RESPONSE_BYTES`].
/// * [`FeedError::Malformed`] for anything else.
pub fn decode_history_page(bytes: &[u8]) -> Result<SessionHistoryPage, FeedError> {
    decode(
        bytes,
        MAX_HISTORY_RESPONSE_BYTES,
        |response: &HistoryPageResponse| response.schema_version,
    )
    .map(|response| response.page)
}

/// Decode a [`LIVE_ROUTE`] response.
///
/// # Errors
///
/// * [`FeedError::Server`] for a backend error envelope.
/// * [`FeedError::UnsupportedVersion`] for another wire version.
/// * [`FeedError::TooLarge`] above [`crate::MAX_RESPONSE_BYTES`].
/// * [`FeedError::Malformed`] for anything else.
pub fn decode_live_batch(bytes: &[u8]) -> Result<LiveBatch, FeedError> {
    decode(
        bytes,
        crate::MAX_RESPONSE_BYTES,
        |response: &LiveResponse| response.schema_version,
    )
    .map(|response| LiveBatch {
        continuity: response.continuity,
        cursor: response.cursor,
        inputs: response.events.into_iter().map(LiveInput::from).collect(),
        more: response.more,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_tui_transcript::window::SessionHistoryDirection;

    #[test]
    fn requests_serialize_to_the_route_shapes() {
        let session_id = SessionId::new();
        let request = HistoryPageRequest {
            session_id,
            query: SessionHistoryQuery {
                cursor: None,
                limit: MAX_HISTORY_READ_EVENTS,
                direction: SessionHistoryDirection::Backward,
            },
        };
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["query"]["direction"], "backward");
        assert_eq!(value["session_id"], serde_json::json!(session_id));
        let subscribe = serde_json::to_value(LiveRequest {
            session_id,
            cursor: None,
        })
        .unwrap();
        assert_eq!(subscribe, serde_json::json!({"session_id": session_id}));
    }

    #[test]
    fn live_batches_decode_into_engine_inputs_in_order() {
        let session_id = SessionId::new();
        let epoch = SessionId::new();
        let body = serde_json::json!({
            "schema_version": 1,
            "continuity": "continuous",
            "cursor": {"epoch": epoch, "offset": 2},
            "events": [
                {"type": "durable", "event": {
                    "schema_version": 1, "sequence": 4, "timestamp_ms": 4,
                    "session_id": session_id,
                    "kind": {"assistant_message": {"text": "hi"}}
                }},
                {"type": "stream", "event": {
                    "session_id": session_id,
                    "kind": {"assistant_text_delta": {"turn_id": "t",
                        "segment_id": "s", "segment_order": 0, "text": "x"}}
                }}
            ],
            "more": false,
        });
        let batch = decode_live_batch(body.to_string().as_bytes()).unwrap();
        assert_eq!(batch.continuity, Continuity::Continuous);
        assert_eq!(batch.cursor, LiveCursor { epoch, offset: 2 });
        assert!(matches!(&batch.inputs[0], LiveInput::Durable(event) if event.sequence == 4));
        assert!(matches!(&batch.inputs[1], LiveInput::Stream(_)));
    }

    #[test]
    fn error_envelopes_versions_and_bounds_are_explicit() {
        assert_eq!(
            decode_live_batch(br#"{"schema_version":1,"error":"browser_busy"}"#),
            Err(FeedError::Server("browser_busy".into()))
        );
        assert_eq!(
            decode_history_page(br#"{"schema_version":2,"error":"x"}"#),
            Err(FeedError::UnsupportedVersion(2))
        );
        let session_id = SessionId::new();
        let future = serde_json::json!({
            "schema_version": 2,
            "page": {"session_id": session_id, "events": [], "has_more": false}
        });
        assert_eq!(
            decode_history_page(future.to_string().as_bytes()),
            Err(FeedError::UnsupportedVersion(2))
        );
        let current = serde_json::json!({
            "schema_version": 1,
            "page": {"session_id": session_id, "events": [], "has_more": false}
        });
        let page = decode_history_page(current.to_string().as_bytes()).unwrap();
        assert_eq!(page.session_id, session_id);
        assert_eq!(decode_history_page(b"[]"), Err(FeedError::Malformed));
        assert_eq!(
            decode_history_page(&vec![b' '; MAX_HISTORY_RESPONSE_BYTES + 1]),
            Err(FeedError::TooLarge)
        );
        assert_eq!(
            decode_live_batch(&vec![b' '; crate::MAX_RESPONSE_BYTES + 1]),
            Err(FeedError::TooLarge)
        );
    }
}
