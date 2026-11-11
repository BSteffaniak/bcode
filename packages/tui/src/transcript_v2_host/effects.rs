//! History requests of the V2 engine executed as host effects (spec sections 3.2 and 5,
//! rules W5, W6, W10 and W11).
//!
//! The engine issues [`HostRequest`]s as data. The host turns each into one
//! [`crate::effects::TuiEffect::LoadTranscriptV2History`] effect carrying a [`HistoryLoad`],
//! and executes it with the engine crate's bounded history loader
//! ([`bcode_tui_transcript::window::load_history`], shared with the browser host) over the
//! daemon history API (`session_history_page`). The result is delivered as a
//! [`HistoryLoaded`] stamped with the request's window generation and the host attach epoch.
//! Reads are bounded and non-replaying; see the loader for the exact reads per request.

use std::future::Future;

use bcode_client::ClientError;
use bcode_session_models::{SessionHistoryPage, SessionHistoryQuery, SessionId};
use bcode_tui_transcript::engine::HostRequest;
pub use bcode_tui_transcript::window::{HistoryLane, HistoryLoaded};
use bcode_tui_transcript::window::{HistoryLoadError, HistoryLoadRequest};

/// One engine history request to execute for a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryLoad {
    /// Session the request reads.
    pub session_id: SessionId,
    /// The engine request, stamped with its window generation.
    pub request: HostRequest,
    /// Newest resident canonical sequence when the request was started; `LiveState` reads the
    /// events after it (W11).
    pub resident_last: Option<u64>,
    /// Host attach epoch the request was issued in. Window generations restart with every
    /// attach, so results of an earlier attach of the same session are fenced by the epoch.
    pub epoch: u64,
}

impl HistoryLoad {
    /// Lane the request runs in.
    #[must_use]
    pub const fn lane(&self) -> HistoryLane {
        HistoryLane::of(&self.request)
    }
}

/// Execute `load` with `read` through the shared bounded loader
/// ([`bcode_tui_transcript::window::load_history`]).
///
/// `read` performs `session_history_page` for `load.session_id` (a daemon client or a managed
/// session command handle).
///
/// # Errors
///
/// * Any error `read` returns (transport, daemon or authorization failures).
/// * [`ClientError::Protocol`] if a page belongs to another session, contains events of another
///   session, is not in strictly ascending sequence order, lies outside the requested range,
///   or reports more history without returning any.
pub async fn load_history<F, Fut>(load: HistoryLoad, read: F) -> Result<HistoryLoaded, ClientError>
where
    F: FnMut(SessionHistoryQuery) -> Fut,
    Fut: Future<Output = Result<SessionHistoryPage, ClientError>>,
{
    let request = HistoryLoadRequest {
        session_id: load.session_id,
        request: load.request,
        resident_last: load.resident_last,
    };
    bcode_tui_transcript::window::load_history(request, read)
        .await
        .map_err(|error| match error {
            HistoryLoadError::Read(error) => error,
            inconsistent @ HistoryLoadError::InconsistentPage => {
                ClientError::Protocol(inconsistent.to_string())
            }
        })
}

#[cfg(test)]
mod tests {
    use bcode_client::ClientError;
    use bcode_session_models::{
        SessionEvent, SessionEventKind, SessionHistoryPage, SessionHistoryQuery, SessionId,
    };
    use bcode_tui_transcript::engine::HostRequest;
    use bcode_tui_transcript::window::{IssuedRequest, PageRequest, WindowRequest};

    use super::{HistoryLane, HistoryLoad, HistoryLoaded, load_history};

    fn event(session_id: SessionId, sequence: u64) -> SessionEvent {
        SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind: SessionEventKind::AssistantMessage {
                text: format!("event {sequence}"),
            },
        }
    }

    fn tail(session_id: SessionId) -> HistoryLoad {
        HistoryLoad {
            session_id,
            request: HostRequest::Window(IssuedRequest {
                generation: 7,
                request: WindowRequest::Tail,
            }),
            resident_last: None,
            epoch: 0,
        }
    }

    #[test]
    fn daemon_pages_load_through_the_shared_loader() {
        let session_id = SessionId::new();
        let mut queries = Vec::new();
        let loaded = block_on(load_history(
            tail(session_id),
            |query: SessionHistoryQuery| {
                queries.push(query);
                std::future::ready(Ok(SessionHistoryPage {
                    session_id,
                    events: vec![event(session_id, 1), event(session_id, 2)],
                    compatibility_issues: Vec::new(),
                    next_cursor: None,
                    has_more: false,
                }))
            },
        ))
        .expect("history loads");
        let HistoryLoaded::Window(window) = loaded else {
            panic!("window result");
        };
        assert_eq!((window.generation, window.events.len()), (7, 2));
        assert_eq!(
            queries.len(),
            1,
            "a window reaching both ends needs no probe"
        );
    }

    #[test]
    fn requests_run_in_their_lane() {
        let lane = |request| {
            HistoryLoad {
                request,
                ..tail(SessionId::new())
            }
            .lane()
        };
        let page = |request| {
            HostRequest::Page(IssuedRequest {
                generation: 7,
                request,
            })
        };
        assert_eq!(
            lane(page(PageRequest::Older { before_sequence: 9 })),
            HistoryLane::Older
        );
        assert_eq!(
            lane(page(PageRequest::Newer { after_sequence: 9 })),
            HistoryLane::Newer
        );
        assert_eq!(lane(tail(SessionId::new()).request), HistoryLane::Window);
    }

    #[test]
    fn inconsistent_pages_and_read_failures_are_client_errors() {
        let session_id = SessionId::new();
        let foreign = block_on(load_history(tail(session_id), |_| {
            let other = SessionId::new();
            std::future::ready(Ok(SessionHistoryPage {
                session_id: other,
                events: vec![event(other, 1)],
                compatibility_issues: Vec::new(),
                next_cursor: None,
                has_more: false,
            }))
        }));
        assert!(matches!(foreign, Err(ClientError::Protocol(_))));
        let failed = block_on(load_history(tail(session_id), |_| {
            std::future::ready(Err(ClientError::UnexpectedResponse))
        }));
        assert!(matches!(failed, Err(ClientError::UnexpectedResponse)));
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime")
            .block_on(future)
    }
}
