//! Bounded canonical history pages for the browser transcript engine.
//!
//! The browser executes engine history requests (`HostRequest::{Page, Window}`) with the same
//! loader as the native host (`bcode_tui_transcript::window::load_history`); each of its
//! reads is one request to this route. The route is the daemon's indexed, non-replaying
//! `session_history_page` read with the engine's per-read cap: it never replays, repairs or
//! reindexes the event log, and it holds no browser state. The loader runs in the browser
//! because this host stays free of terminal crates; the read it performs is the application
//! boundary both hosts share.
//!
//! Byte budget: a read is bounded by event count and by serialized bytes. The daemon's page
//! is cut to the events nearest the cursor that fit [`MAX_BROWSER_HISTORY_PAGE_BYTES`], and a
//! cut page reports more history (`has_more`, `next_cursor` at the first event left out), so
//! the loader's ordinary paging continues past it. The nearest event alone may exceed the page
//! budget up to [`MAX_BROWSER_HISTORY_EVENT_BYTES`] (one event per response); a larger event
//! is answered with the typed `history_event_too_large` error, which the browser surfaces as
//! a degraded transcript instead of retrying.

use bcode_session_models::{
    SessionHistoryCursor, SessionHistoryDirection, SessionHistoryPage, SessionHistoryQuery,
    SessionId,
};
use hyperchad::{renderer::Content, router::RouteRequest};
use serde::{Deserialize, Serialize};

use super::{BACKEND_TIMEOUT, MAX_RESPONSE_BYTES, error, json_content_within, parse_body};
use crate::HyperChadAppState;

/// Events one browser history read may request: one engine page or window read
/// (`PageCaps::max_events_scanned`).
pub const MAX_BROWSER_HISTORY_READ_EVENTS: usize = 512;
/// Response bytes reserved beyond the events for the envelope and page fields.
const ENVELOPE_BYTES: usize = 64 * 1024;
/// Serialized bytes of the events (and their compatibility diagnostics) of one page with
/// more than one event: a page fits the ordinary browser response limit.
pub const MAX_BROWSER_HISTORY_PAGE_BYTES: usize = MAX_RESPONSE_BYTES - ENVELOPE_BYTES;
/// Serialized bytes one canonical event may use when it is returned alone.
pub const MAX_BROWSER_HISTORY_EVENT_BYTES: usize = 16 * 1024 * 1024;
/// Largest history page response (one event of [`MAX_BROWSER_HISTORY_EVENT_BYTES`]).
pub const MAX_BROWSER_HISTORY_RESPONSE_BYTES: usize =
    MAX_BROWSER_HISTORY_EVENT_BYTES + ENVELOPE_BYTES;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryPageRequest {
    session_id: SessionId,
    query: SessionHistoryQuery,
}

/// Response of the history page route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserHistoryPage {
    /// Browser application wire version (currently 1).
    pub schema_version: u16,
    /// The daemon's bounded page, events in ascending sequence order.
    pub page: SessionHistoryPage,
}

/// Serialized length of `value`, counted without buffering it.
fn encoded_len(value: &impl Serialize) -> usize {
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
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value).map_or(usize::MAX, |()| counter.0)
}

/// Cut `page`, read in `direction`, to the events nearest the cursor that fit the byte budget
/// (module documentation). A cut page reports more history, with `next_cursor` at the nearest
/// event left out; events keep ascending order and their compatibility diagnostics.
///
/// # Errors
///
/// Returns `history_event_too_large` if the nearest event alone exceeds
/// [`MAX_BROWSER_HISTORY_EVENT_BYTES`].
fn fit_page(
    mut page: SessionHistoryPage,
    direction: SessionHistoryDirection,
) -> Result<SessionHistoryPage, &'static str> {
    let sizes = page
        .events
        .iter()
        .map(|event| {
            let issues = page
                .compatibility_issues
                .iter()
                .filter(|issue| issue.sequence == event.sequence)
                .map(encoded_len)
                .fold(0_usize, usize::saturating_add);
            encoded_len(event).saturating_add(issues).saturating_add(1)
        })
        .collect::<Vec<_>>();
    let nearest_first: Box<dyn Iterator<Item = &usize>> = match direction {
        SessionHistoryDirection::Backward => Box::new(sizes.iter().rev()),
        SessionHistoryDirection::Forward => Box::new(sizes.iter()),
    };
    let mut kept = 0_usize;
    let mut bytes = 0_usize;
    for &size in nearest_first {
        if kept == 0 {
            if size > MAX_BROWSER_HISTORY_EVENT_BYTES {
                return Err("history_event_too_large");
            }
        } else if bytes.saturating_add(size) > MAX_BROWSER_HISTORY_PAGE_BYTES {
            break;
        }
        kept += 1;
        bytes = bytes.saturating_add(size);
        if bytes > MAX_BROWSER_HISTORY_PAGE_BYTES {
            // One oversized event travels alone.
            break;
        }
    }
    let total = page.events.len();
    if kept == total {
        return Ok(page);
    }
    let next = match direction {
        SessionHistoryDirection::Backward => {
            let dropped = page.events.drain(..total - kept).collect::<Vec<_>>();
            dropped.last().map(|event| event.sequence)
        }
        SessionHistoryDirection::Forward => {
            let dropped = page.events.split_off(kept);
            dropped.first().map(|event| event.sequence)
        }
    };
    let events = &page.events;
    page.compatibility_issues.retain(|issue| {
        events
            .binary_search_by_key(&issue.sequence, |event| event.sequence)
            .is_ok()
    });
    page.next_cursor = next.map(|sequence| SessionHistoryCursor { sequence });
    page.has_more = true;
    Ok(page)
}

const fn validate(request: &HistoryPageRequest) -> Result<(), &'static str> {
    match request.query.limit {
        0 => Err("invalid_request"),
        limit if limit > MAX_BROWSER_HISTORY_READ_EVENTS => Err("history_limit_exceeded"),
        _ => Ok(()),
    }
}

impl HyperChadAppState {
    pub(super) async fn browser_history_page(&self, request: RouteRequest) -> Content {
        if !self.authorizes(&request) {
            return error("unauthorized");
        }
        let read: HistoryPageRequest = match parse_body(&request) {
            Ok(value) => value,
            Err(code) => return error(code),
        };
        if let Err(code) = validate(&read) {
            return error(code);
        }
        // Lanes read concurrently (older, newer, window); excess reads retry, never queue.
        let Some(_read_slot) = self.browser_consumers.admit_history_read(read.session_id) else {
            return error("browser_busy");
        };
        match tokio::time::timeout(
            BACKEND_TIMEOUT,
            self.client
                .session_history_page(read.session_id, read.query),
        )
        .await
        {
            Ok(Ok(page)) if page.session_id == read.session_id => {
                match fit_page(page, read.query.direction) {
                    Ok(page) => json_content_within(
                        &BrowserHistoryPage {
                            schema_version: 1,
                            page,
                        },
                        MAX_BROWSER_HISTORY_RESPONSE_BYTES,
                    ),
                    Err(code) => error(code),
                }
            }
            Ok(_) | Err(_) => error("history_unavailable"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(
        session_id: SessionId,
        sequence: u64,
        bytes: usize,
    ) -> bcode_session_models::SessionEvent {
        bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::AssistantMessage {
                text: "x".repeat(bytes),
            },
        }
    }

    fn page(session_id: SessionId, sizes: &[usize], has_more: bool) -> SessionHistoryPage {
        SessionHistoryPage {
            session_id,
            events: sizes
                .iter()
                .zip(1_u64..)
                .map(|(&bytes, sequence)| event(session_id, sequence, bytes))
                .collect(),
            compatibility_issues: Vec::new(),
            next_cursor: None,
            has_more,
        }
    }

    fn sequences(page: &SessionHistoryPage) -> Vec<u64> {
        page.events.iter().map(|event| event.sequence).collect()
    }

    fn response_len(page: SessionHistoryPage) -> usize {
        let Content::Raw { data, .. } = json_content_within(
            &BrowserHistoryPage {
                schema_version: 1,
                page,
            },
            MAX_BROWSER_HISTORY_RESPONSE_BYTES,
        ) else {
            panic!("expected JSON")
        };
        let value: serde_json::Value = serde_json::from_slice(&data).unwrap();
        assert!(value.get("error").is_none(), "{value}");
        data.len()
    }

    #[test]
    fn pages_beyond_the_byte_budget_keep_the_events_nearest_the_cursor_and_report_more() {
        let session = SessionId::new();
        // 512 events of 8 KiB: about 4 MiB, twice the browser response limit.
        let sizes = vec![8 * 1024; MAX_BROWSER_HISTORY_READ_EVENTS];
        let tail = fit_page(
            page(session, &sizes, false),
            SessionHistoryDirection::Backward,
        )
        .unwrap();
        let kept = sequences(&tail);
        assert!(kept.len() < sizes.len() && !kept.is_empty());
        assert_eq!(
            kept.last(),
            Some(&512),
            "a backward read keeps the newest events"
        );
        assert!(kept.windows(2).all(|pair| pair[0] + 1 == pair[1]));
        assert!(tail.has_more);
        assert_eq!(
            tail.next_cursor.map(|cursor| cursor.sequence),
            Some(kept[0] - 1)
        );
        assert!(response_len(tail) <= MAX_RESPONSE_BYTES);

        let head = fit_page(
            page(session, &sizes, true),
            SessionHistoryDirection::Forward,
        )
        .unwrap();
        let kept = sequences(&head);
        assert_eq!(
            kept.first(),
            Some(&1),
            "a forward read keeps the oldest events"
        );
        assert!(head.has_more);
        assert_eq!(
            head.next_cursor.map(|cursor| cursor.sequence),
            kept.last().map(|last| last + 1)
        );
        assert!(response_len(head) <= MAX_RESPONSE_BYTES);

        let small = page(session, &[16, 16], false);
        assert_eq!(
            fit_page(small.clone(), SessionHistoryDirection::Backward),
            Ok(small),
            "pages within the budget are unchanged"
        );
    }

    #[test]
    fn one_oversized_event_travels_alone_and_a_larger_one_is_a_typed_error() {
        let session = SessionId::new();
        let large = MAX_RESPONSE_BYTES + 1024;
        let alone = fit_page(
            page(session, &[16, 16, large], false),
            SessionHistoryDirection::Backward,
        )
        .unwrap();
        assert_eq!(sequences(&alone), [3]);
        assert!(alone.has_more);
        let bytes = response_len(alone);
        assert!(bytes > MAX_RESPONSE_BYTES && bytes <= MAX_BROWSER_HISTORY_RESPONSE_BYTES);
        // The page after it continues normally.
        let before = fit_page(
            page(session, &[16, 16], false),
            SessionHistoryDirection::Backward,
        )
        .unwrap();
        assert_eq!(sequences(&before), [1, 2]);

        assert_eq!(
            fit_page(
                page(session, &[MAX_BROWSER_HISTORY_EVENT_BYTES + 1], false),
                SessionHistoryDirection::Forward,
            ),
            Err("history_event_too_large")
        );
    }

    #[tokio::test]
    async fn history_page_route_enforces_auth_shape_and_caps_before_backend_access() {
        let state = HyperChadAppState::new(bcode_client::BcodeClient::default_endpoint(), "secret");
        let router = crate::router_from_state(state);
        let query = |limit: usize| {
            serde_json::json!({
                "session_id": SessionId::new(),
                "query": {"cursor": null, "limit": limit, "direction": "backward"},
            })
            .to_string()
        };
        for (token, method, content_type, body, expected) in [
            ("wrong", "POST", "application/json", query(1), "unauthorized"),
            ("secret", "GET", "application/json", query(1), "method_not_allowed"),
            ("secret", "POST", "text/plain", query(1), "invalid_content_type"),
            ("secret", "POST", "application/json", "{}".to_owned(), "invalid_request"),
            (
                "secret",
                "POST",
                "application/json",
                serde_json::json!({"session_id": SessionId::new(), "query": {"limit": 1, "direction": "forward"}, "raw": true}).to_string(),
                "invalid_request",
            ),
            ("secret", "POST", "application/json", query(0), "invalid_request"),
            (
                "secret",
                "POST",
                "application/json",
                query(MAX_BROWSER_HISTORY_READ_EVENTS + 1),
                "history_limit_exceeded",
            ),
            (
                "secret",
                "POST",
                "application/json",
                " ".repeat(super::super::MAX_REQUEST_BYTES + 1),
                "request_too_large",
            ),
        ] {
            let mut request = RouteRequest::from_path(
                &format!("/api/session-view/history-page?token={token}"),
                hyperchad::router::RequestInfo::default(),
            );
            request.method = method.parse().unwrap();
            request
                .headers
                .insert("content-type".to_owned(), content_type.to_owned());
            request.body = Some(std::sync::Arc::new(body.into_bytes().into()));
            let Content::Raw { data, .. } = router.navigate(request).await.unwrap().unwrap()
            else {
                panic!("expected JSON")
            };
            let value: serde_json::Value = serde_json::from_slice(&data).unwrap();
            assert_eq!(value["error"], expected);
            assert_eq!(value["schema_version"], 1);
        }
    }
}
