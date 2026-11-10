//! Behaviour of the bounded history loader against a daemon double.

use std::cell::RefCell;

use bcode_session_models::{
    SessionEvent, SessionEventKind, SessionHistoryDirection, SessionHistoryPage,
    SessionHistoryQuery, SessionId,
};

use super::loader::{
    HistoryLane, HistoryLoadError, HistoryLoadRequest, HistoryLoaded, load_history,
};
use super::{IssuedRequest, PageCaps, PageDirection, PageRequest, WindowRequest};
use crate::engine::HostRequest;

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

/// Poll a future whose reads all complete immediately.
fn ready<F: std::future::Future>(future: F) -> F::Output {
    let future = std::pin::pin!(future);
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    match future.poll(&mut context) {
        std::task::Poll::Ready(output) => output,
        std::task::Poll::Pending => unreachable!("the daemon double answers immediately"),
    }
}

/// A daemon double over sequences `first..=last`, recording every query.
struct Daemon {
    session_id: SessionId,
    first: u64,
    last: u64,
    queries: RefCell<Vec<SessionHistoryQuery>>,
}

impl Daemon {
    fn new(last: u64) -> Self {
        Self::spanning(1, last)
    }

    fn spanning(first: u64, last: u64) -> Self {
        Self {
            session_id: SessionId::new(),
            first,
            last,
            queries: RefCell::new(Vec::new()),
        }
    }

    /// Every query was bounded by the window caps; returns the number of reads.
    fn bounded_reads(&self) -> usize {
        let queries = self.queries.borrow();
        assert!(
            queries
                .iter()
                .all(|query| query.limit <= PageCaps::WINDOW.max_events_scanned),
            "{queries:?}"
        );
        queries.len()
    }

    fn read(&self, query: SessionHistoryQuery) -> SessionHistoryPage {
        self.queries.borrow_mut().push(query);
        let all = (self.first..=self.last).collect::<Vec<_>>();
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
        let load = HistoryLoadRequest {
            session_id: self.session_id,
            request,
            resident_last,
        };
        ready(load_history(load, |query| {
            std::future::ready(Ok::<_, ()>(self.read(query)))
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
    assert_eq!(
        result.first_available_sequence,
        Some(1),
        "session start probed"
    );
    assert_eq!(
        daemon.bounded_reads(),
        2,
        "one window read and one bound probe"
    );
    let probe = daemon.queries.borrow()[1];
    assert_eq!(
        (probe.cursor, probe.limit, probe.direction),
        (None, 1, SessionHistoryDirection::Forward)
    );
}

#[test]
fn window_bounds_are_the_session_bounds_not_the_resident_edges() {
    let daemon = Daemon::spanning(101, 5_000);
    let HistoryLoaded::Window(tail) = daemon.load(window(WindowRequest::Tail), None) else {
        panic!("window result");
    };
    assert_eq!(
        (tail.first_available_sequence, tail.last_available_sequence),
        (Some(101), Some(5_000))
    );
    let HistoryLoaded::Window(head) = daemon.load(window(WindowRequest::Head), None) else {
        panic!("window result");
    };
    assert_eq!(sequences(&head.events), (101, 612, 512));
    assert_eq!(
        (head.first_available_sequence, head.last_available_sequence),
        (Some(101), Some(5_000))
    );
    let HistoryLoaded::Window(around) =
        daemon.load(window(WindowRequest::Around { sequence: 2_500 }), None)
    else {
        panic!("window result");
    };
    assert_eq!(
        (
            around.first_available_sequence,
            around.last_available_sequence
        ),
        (Some(101), Some(5_000))
    );
    // Tail: window + start probe; Head: window + end probe; Around: two halves + two
    // probes. No read scans more than one window.
    assert_eq!(daemon.bounded_reads(), 2 + 2 + 4);
}

#[test]
fn a_window_reaching_both_session_ends_needs_no_probe() {
    let daemon = Daemon::new(40);
    let HistoryLoaded::Window(result) = daemon.load(window(WindowRequest::Tail), None) else {
        panic!("window result");
    };
    assert_eq!(
        (
            result.first_available_sequence,
            result.last_available_sequence
        ),
        (Some(1), Some(40))
    );
    assert_eq!(daemon.bounded_reads(), 1);
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
    assert_eq!(
        result.last_available_sequence,
        Some(2_000),
        "session end probed"
    );
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
    let HistoryLoaded::Window(empty) = daemon.load(window(WindowRequest::LiveState), None) else {
        panic!("window result");
    };
    assert!(empty.events.is_empty(), "no resident tail reads nothing");
}

#[test]
fn requests_run_in_their_lane() {
    assert_eq!(
        HistoryLane::of(&page(PageRequest::Older { before_sequence: 9 })),
        HistoryLane::Older
    );
    assert_eq!(
        HistoryLane::of(&page(PageRequest::Newer { after_sequence: 9 })),
        HistoryLane::Newer
    );
    for request in [
        WindowRequest::Head,
        WindowRequest::Tail,
        WindowRequest::Around { sequence: 3 },
    ] {
        assert_eq!(HistoryLane::of(&window(request)), HistoryLane::Window);
    }
    assert_eq!(
        HistoryLane::of(&window(WindowRequest::LiveState)),
        HistoryLane::LiveState
    );
}

#[test]
fn foreign_unordered_out_of_range_or_empty_more_pages_are_rejected() {
    let session_id = SessionId::new();
    let other = SessionId::new();
    let load = |request| HistoryLoadRequest {
        session_id,
        request,
        resident_last: None,
    };
    let answer = |page: SessionHistoryPage, request| {
        ready(load_history(load(request), |_| {
            std::future::ready(Ok::<_, ()>(page.clone()))
        }))
    };
    let page = |owner, sequences: &[u64], has_more| SessionHistoryPage {
        session_id: owner,
        events: sequences
            .iter()
            .map(|sequence| event(owner, *sequence))
            .collect(),
        compatibility_issues: Vec::new(),
        next_cursor: None,
        has_more,
    };
    let tail = window(WindowRequest::Tail);
    for invalid in [
        page(other, &[1], false),
        page(session_id, &[2, 1], false),
        page(session_id, &[3, 3], false),
        page(session_id, &[], true),
    ] {
        assert_eq!(
            answer(invalid, tail),
            Err(HistoryLoadError::InconsistentPage)
        );
    }
    let mut foreign_event = page(session_id, &[1, 2], false);
    foreign_event.events[1].session_id = other;
    assert_eq!(
        answer(foreign_event, tail),
        Err(HistoryLoadError::InconsistentPage)
    );
    // An `Older` page must lie strictly before the cursor.
    assert_eq!(
        answer(
            page(session_id, &[9, 10], false),
            page_request(PageRequest::Older {
                before_sequence: 10
            })
        ),
        Err(HistoryLoadError::InconsistentPage)
    );
}

fn page_request(request: PageRequest) -> HostRequest {
    page(request)
}

#[test]
fn reader_errors_are_returned_unchanged_and_stop_the_load() {
    let session_id = SessionId::new();
    let mut reads = 0;
    let result = ready(load_history(
        HistoryLoadRequest {
            session_id,
            request: window(WindowRequest::Around { sequence: 5 }),
            resident_last: None,
        },
        |_| {
            reads += 1;
            std::future::ready(Err::<SessionHistoryPage, _>("offline"))
        },
    ));
    assert_eq!(result, Err(HistoryLoadError::Read("offline")));
    assert_eq!(reads, 1, "the second half is not read after a failure");
    assert_eq!(
        HistoryLoadError::<&str>::InconsistentPage.to_string(),
        "inconsistent transcript history page"
    );
}
