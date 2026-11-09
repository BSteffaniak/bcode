//! History requests of the V2 engine executed as host effects (spec sections 3.2 and 5,
//! rules W5, W6, W10 and W11).
//!
//! The engine issues [`HostRequest`]s as data. The host turns each into one
//! [`crate::effects::TuiEffect::LoadTranscriptV2History`] effect carrying a [`HistoryLoad`],
//! which reads bounded canonical event ranges through the same daemon history API the V1
//! effects use (`session_history_page`), and delivers a [`HistoryLoaded`] result stamped with
//! the request's window generation. Turn alignment, generation fencing and incorporation are
//! the engine's (W3, W4); this module only reads canonical events and validates their shape.
//!
//! Every request reads at most [`PageCaps::max_events_scanned`] canonical events
//! ([`PageCaps::PAGE`] for pages, [`PageCaps::WINDOW`] for windows). The daemon bounds the
//! payload of one read; item caps are applied by turn alignment in the engine.

use std::future::Future;

use bcode_client::ClientError;
use bcode_session_models::{
    SessionEvent, SessionHistoryCursor, SessionHistoryDirection, SessionHistoryPage,
    SessionHistoryQuery, SessionId,
};
use bcode_tui_transcript::engine::HostRequest;
use bcode_tui_transcript::window::{
    HistoryPageResult, HistoryWindowResult, PageCaps, PageDirection, PageRequest, WindowRequest,
};

/// Concurrency lane of a history request: requests in one lane supersede each other, requests
/// in different lanes run concurrently (W5: one page per direction, one replacement).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HistoryLane {
    /// `Older` pages.
    Older,
    /// `Newer` pages.
    Newer,
    /// `Head`, `Tail` and `Around` window replacements.
    Window,
    /// `LiveState` rehydration (W11).
    LiveState,
}

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
        match self.request {
            HostRequest::Page(issued) => match issued.request {
                PageRequest::Older { .. } => HistoryLane::Older,
                PageRequest::Newer { .. } => HistoryLane::Newer,
            },
            HostRequest::Window(issued) => match issued.request {
                WindowRequest::LiveState => HistoryLane::LiveState,
                WindowRequest::Around { .. } | WindowRequest::Head | WindowRequest::Tail => {
                    HistoryLane::Window
                }
            },
        }
    }
}

/// A loaded history request, ready for the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryLoaded {
    /// A page result ([`bcode_tui_transcript::engine::TranscriptEngine::apply_page`]).
    Page(HistoryPageResult),
    /// A window result ([`bcode_tui_transcript::engine::TranscriptEngine::apply_window`]).
    Window(HistoryWindowResult),
}

/// Execute `load` with `read`, one bounded canonical history read per call (two for
/// `Around`, one on each side of the target).
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
pub async fn load_history<F, Fut>(
    load: HistoryLoad,
    mut read: F,
) -> Result<HistoryLoaded, ClientError>
where
    F: FnMut(SessionHistoryQuery) -> Fut,
    Fut: Future<Output = Result<SessionHistoryPage, ClientError>>,
{
    let mut read = Reader {
        read: &mut read,
        session_id: load.session_id,
    };
    match load.request {
        HostRequest::Page(issued) => {
            let caps = PageCaps::PAGE.max_events_scanned;
            let (direction, page) = match issued.request {
                PageRequest::Older { before_sequence } => {
                    let page = match before_sequence.checked_sub(1) {
                        Some(last) => read.range(Some(last), caps, Backward).await?,
                        None => EventRange::default(),
                    };
                    (PageDirection::Older, page)
                }
                PageRequest::Newer { after_sequence } => {
                    let first = after_sequence.saturating_add(1);
                    (
                        PageDirection::Newer,
                        read.range(Some(first), caps, Forward).await?,
                    )
                }
            };
            Ok(HistoryLoaded::Page(HistoryPageResult {
                generation: issued.generation,
                direction,
                events: page.events,
                has_more: page.has_more,
                unaligned: false,
            }))
        }
        HostRequest::Window(issued) => {
            let caps = PageCaps::WINDOW.max_events_scanned;
            let (events, has_older, has_newer) = match issued.request {
                WindowRequest::Tail => {
                    let page = read.range(None, caps, Backward).await?;
                    (page.events, page.has_more, false)
                }
                WindowRequest::Head => {
                    let page = read.range(None, caps, Forward).await?;
                    (page.events, false, page.has_more)
                }
                WindowRequest::Around { sequence } => {
                    let half = caps / 2;
                    let older = read.range(Some(sequence), half, Backward).await?;
                    let after = Some(sequence.saturating_add(1));
                    let newer = read.range(after, caps - half, Forward).await?;
                    let mut events = older.events;
                    events.extend(newer.events);
                    (events, older.has_more, newer.has_more)
                }
                WindowRequest::LiveState => {
                    let page = match load.resident_last {
                        Some(last) => {
                            read.range(Some(last.saturating_add(1)), caps, Forward)
                                .await?
                        }
                        None => EventRange::default(),
                    };
                    (page.events, false, page.has_more)
                }
            };
            Ok(HistoryLoaded::Window(window_result(
                issued.generation,
                issued.request,
                events,
                has_older,
                has_newer,
            )))
        }
    }
}

use SessionHistoryDirection::{Backward, Forward};

/// Canonical events of one validated read.
#[derive(Debug, Default)]
struct EventRange {
    events: Vec<SessionEvent>,
    has_more: bool,
}

/// Bounded canonical reads of one session.
struct Reader<'a, F> {
    read: &'a mut F,
    session_id: SessionId,
}

impl<F, Fut> Reader<'_, F>
where
    F: FnMut(SessionHistoryQuery) -> Fut,
    Fut: Future<Output = Result<SessionHistoryPage, ClientError>>,
{
    /// One bounded canonical read starting at `cursor` (inclusive) in `direction`, validated
    /// and returned in ascending order.
    async fn range(
        &mut self,
        cursor: Option<u64>,
        limit: usize,
        direction: SessionHistoryDirection,
    ) -> Result<EventRange, ClientError> {
        let session_id = self.session_id;
        let page = (self.read)(SessionHistoryQuery {
            cursor: cursor.map(|sequence| SessionHistoryCursor { sequence }),
            limit: limit.max(1),
            direction,
        })
        .await?;
        let in_range = |sequence: u64| {
            cursor.is_none_or(|cursor| match direction {
                Forward => sequence >= cursor,
                Backward => sequence <= cursor,
            })
        };
        if page.session_id != session_id
            || page
                .events
                .iter()
                .any(|event| event.session_id != session_id || !in_range(event.sequence))
            || page
                .events
                .windows(2)
                .any(|pair| pair[0].sequence >= pair[1].sequence)
            || (page.has_more && page.events.is_empty())
        {
            return Err(ClientError::Protocol(
                "inconsistent transcript history page".to_owned(),
            ));
        }
        Ok(EventRange {
            events: page.events,
            has_more: page.has_more,
        })
    }
}

/// A window result with session bounds known from the read: the first available sequence when
/// nothing older exists, the last available sequence when nothing newer exists (W12 hints).
fn window_result(
    generation: u64,
    target: WindowRequest,
    events: Vec<SessionEvent>,
    has_older: bool,
    has_newer: bool,
) -> HistoryWindowResult {
    let first_available_sequence = (!has_older)
        .then(|| events.first().map(|event| event.sequence))
        .flatten();
    let last_available_sequence = (!has_newer)
        .then(|| events.last().map(|event| event.sequence))
        .flatten();
    HistoryWindowResult {
        generation,
        target,
        events,
        has_older,
        has_newer,
        first_available_sequence,
        last_available_sequence,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use bcode_client::ClientError;
    use bcode_session_models::{
        SessionEvent, SessionEventKind, SessionHistoryDirection, SessionHistoryPage,
        SessionHistoryQuery, SessionId,
    };
    use bcode_tui_transcript::engine::HostRequest;
    use bcode_tui_transcript::window::{IssuedRequest, PageDirection, PageRequest, WindowRequest};

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

    /// A daemon double over sequences `1..=last`, recording every query.
    struct Daemon {
        session_id: SessionId,
        last: u64,
        queries: RefCell<Vec<SessionHistoryQuery>>,
    }

    impl Daemon {
        fn new(last: u64) -> Self {
            Self {
                session_id: SessionId::new(),
                last,
                queries: RefCell::new(Vec::new()),
            }
        }

        fn read(&self, query: SessionHistoryQuery) -> SessionHistoryPage {
            self.queries.borrow_mut().push(query);
            let all = (1..=self.last).collect::<Vec<_>>();
            let selected: Vec<u64> = match query.direction {
                SessionHistoryDirection::Forward => all
                    .into_iter()
                    .filter(|sequence| {
                        query
                            .cursor
                            .is_none_or(|cursor| *sequence >= cursor.sequence)
                    })
                    .collect(),
                SessionHistoryDirection::Backward => {
                    let mut selected: Vec<u64> = all
                        .into_iter()
                        .rev()
                        .filter(|sequence| {
                            query
                                .cursor
                                .is_none_or(|cursor| *sequence <= cursor.sequence)
                        })
                        .collect();
                    selected.truncate(query.limit + 1);
                    selected.reverse();
                    let has_more = selected.len() > query.limit;
                    if has_more {
                        selected.remove(0);
                    }
                    return self.page(selected, has_more);
                }
            };
            let has_more = selected.len() > query.limit;
            self.page(selected.into_iter().take(query.limit).collect(), has_more)
        }

        fn page(&self, sequences: Vec<u64>, has_more: bool) -> SessionHistoryPage {
            SessionHistoryPage {
                session_id: self.session_id,
                events: sequences
                    .into_iter()
                    .map(|sequence| event(self.session_id, sequence))
                    .collect(),
                compatibility_issues: Vec::new(),
                next_cursor: None,
                has_more,
            }
        }

        fn load(&self, request: HostRequest, resident_last: Option<u64>) -> HistoryLoaded {
            let load = HistoryLoad {
                session_id: self.session_id,
                request,
                resident_last,
                epoch: 0,
            };
            block_on(load_history(load, |query| {
                std::future::ready(Ok(self.read(query)))
            }))
            .expect("history loads")
        }
    }

    fn window(request: WindowRequest) -> HostRequest {
        HostRequest::Window(IssuedRequest {
            generation: 7,
            request,
        })
    }

    fn page(request: PageRequest) -> HostRequest {
        HostRequest::Page(IssuedRequest {
            generation: 7,
            request,
        })
    }

    fn sequences(events: &[SessionEvent]) -> (u64, u64, usize) {
        (
            events.first().map_or(0, |event| event.sequence),
            events.last().map_or(0, |event| event.sequence),
            events.len(),
        )
    }

    #[test]
    fn tail_window_reads_one_bounded_window_ending_at_the_latest_event() {
        let daemon = Daemon::new(2_000);
        let HistoryLoaded::Window(result) = daemon.load(window(WindowRequest::Tail), None) else {
            panic!("window result");
        };
        assert_eq!(result.generation, 7);
        assert_eq!(result.target, WindowRequest::Tail);
        assert_eq!(sequences(&result.events), (1_489, 2_000, 512));
        assert!(result.has_older);
        assert!(!result.has_newer);
        assert_eq!(result.last_available_sequence, Some(2_000));
        assert_eq!(result.first_available_sequence, None);
        assert_eq!(daemon.queries.borrow().len(), 1, "one read (W10)");
    }

    #[test]
    fn head_window_starts_at_the_session_start() {
        let daemon = Daemon::new(2_000);
        let HistoryLoaded::Window(result) = daemon.load(window(WindowRequest::Head), None) else {
            panic!("window result");
        };
        assert_eq!(sequences(&result.events), (1, 512, 512));
        assert!(!result.has_older);
        assert!(result.has_newer);
        assert_eq!(result.first_available_sequence, Some(1));
    }

    #[test]
    fn around_window_is_centered_on_the_target_sequence() {
        let daemon = Daemon::new(2_000);
        let HistoryLoaded::Window(result) =
            daemon.load(window(WindowRequest::Around { sequence: 1_000 }), None)
        else {
            panic!("window result");
        };
        assert_eq!(sequences(&result.events), (745, 1_256, 512));
        assert!(result.events.iter().any(|event| event.sequence == 1_000));
        assert!(result.has_older && result.has_newer);
    }

    #[test]
    fn pages_read_strictly_beyond_the_resident_edges() {
        let daemon = Daemon::new(2_000);
        let HistoryLoaded::Page(older) = daemon.load(
            page(PageRequest::Older {
                before_sequence: 1_000,
            }),
            None,
        ) else {
            panic!("page result");
        };
        assert_eq!(older.direction, PageDirection::Older);
        assert_eq!(sequences(&older.events), (488, 999, 512));
        assert!(older.has_more);
        let HistoryLoaded::Page(newer) = daemon.load(
            page(PageRequest::Newer {
                after_sequence: 1_800,
            }),
            None,
        ) else {
            panic!("page result");
        };
        assert_eq!(sequences(&newer.events), (1_801, 2_000, 200));
        assert!(!newer.has_more);
        let HistoryLoaded::Page(start) =
            daemon.load(page(PageRequest::Older { before_sequence: 0 }), None)
        else {
            panic!("page result");
        };
        assert!(start.events.is_empty() && !start.has_more);
    }

    #[test]
    fn live_state_reads_events_after_the_resident_tail() {
        let daemon = Daemon::new(40);
        let HistoryLoaded::Window(result) = daemon.load(window(WindowRequest::LiveState), Some(30))
        else {
            panic!("window result");
        };
        assert_eq!(result.target, WindowRequest::LiveState);
        assert_eq!(sequences(&result.events), (31, 40, 10));
    }

    #[test]
    fn requests_run_in_their_lane() {
        let lane = |request| {
            HistoryLoad {
                session_id: SessionId::new(),
                request,
                resident_last: None,
                epoch: 0,
            }
            .lane()
        };
        assert_eq!(
            lane(page(PageRequest::Older { before_sequence: 9 })),
            HistoryLane::Older
        );
        assert_eq!(
            lane(page(PageRequest::Newer { after_sequence: 9 })),
            HistoryLane::Newer
        );
        assert_eq!(lane(window(WindowRequest::Head)), HistoryLane::Window);
        assert_eq!(
            lane(window(WindowRequest::LiveState)),
            HistoryLane::LiveState
        );
    }

    #[test]
    fn foreign_or_unordered_pages_are_rejected() {
        let session_id = SessionId::new();
        let load = HistoryLoad {
            session_id,
            request: window(WindowRequest::Tail),
            resident_last: None,
            epoch: 0,
        };
        let foreign = block_on(load_history(load, |_| {
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
        let unordered = block_on(load_history(load, |_| {
            std::future::ready(Ok(SessionHistoryPage {
                session_id,
                events: vec![event(session_id, 2), event(session_id, 1)],
                compatibility_issues: Vec::new(),
                next_cursor: None,
                has_more: false,
            }))
        }));
        assert!(matches!(unordered, Err(ClientError::Protocol(_))));
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime")
            .block_on(future)
    }
}
