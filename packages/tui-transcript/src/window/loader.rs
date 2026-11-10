//! Bounded history loading for engine [`HostRequest`]s (spec sections 3.2 and 5, rules W5,
//! W6, W8, W10, W11 and W12).
//!
//! The engine issues [`HostRequest`]s as data. A host executes each one with [`load_history`]
//! over a reader of bounded canonical history pages (`session_history_page`: a daemon client,
//! a managed session command handle, or a browser transport of the same read). Every host
//! runs this one implementation; it performs no I/O itself. Turn alignment, generation fencing
//! and incorporation are the engine's (W3, W4); this module only reads canonical events and
//! validates their shape.
//!
//! Every request reads at most [`PageCaps::max_events_scanned`] canonical events
//! ([`PageCaps::PAGE`] for pages, [`PageCaps::WINDOW`] for windows) per read. The reader bounds
//! the payload of one read; item caps are applied by turn alignment in the engine.
//!
//! Reads map to indexed, non-replaying history reads (session persistence architecture: normal
//! history paths read bounded pages and never replay or repair the event log):
//!
//! * `Tail`: one backward read from the newest event.
//! * `Head`: one forward read from the session start.
//! * `Around(sequence)`: one backward read ending at `sequence` and one forward read after it
//!   (W8: half the window on each side before turn alignment).
//! * `Older` / `Newer` pages: one read strictly before the first / after the last resident
//!   sequence (the cursor).
//! * `LiveState`: one forward read after the resident tail (W11).
//!
//! Window results always carry both session bounds (first and last available sequence) for the
//! presentation-only position estimate (W12, X6). A bound the window read does not reach is
//! read with one single-event probe at that end of the session (`limit = 1`, still an indexed
//! bounded read), so a window in the middle of a long session places the scrollbar within the
//! whole session instead of within the resident rows.

use std::future::Future;

pub use bcode_session_models::{
    SessionEvent, SessionHistoryCursor, SessionHistoryDirection, SessionHistoryPage,
    SessionHistoryQuery, SessionId,
};

use super::{
    HistoryPageResult, HistoryWindowResult, PageCaps, PageDirection, PageRequest, WindowRequest,
};
use crate::engine::HostRequest;

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

impl HistoryLane {
    /// Lane `request` runs in.
    #[must_use]
    pub const fn of(request: &HostRequest) -> Self {
        match request {
            HostRequest::Page(issued) => match issued.request {
                PageRequest::Older { .. } => Self::Older,
                PageRequest::Newer { .. } => Self::Newer,
            },
            HostRequest::Window(issued) => match issued.request {
                WindowRequest::LiveState => Self::LiveState,
                WindowRequest::Around { .. } | WindowRequest::Head | WindowRequest::Tail => {
                    Self::Window
                }
            },
        }
    }
}

/// One engine history request to execute for a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryLoadRequest {
    /// Session the request reads.
    pub session_id: SessionId,
    /// The engine request, stamped with its window generation.
    pub request: HostRequest,
    /// Newest resident canonical sequence when the request was started; `LiveState` reads the
    /// events after it (W11).
    pub resident_last: Option<u64>,
}

/// A loaded history request, ready for the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryLoaded {
    /// A page result ([`crate::engine::TranscriptEngine::apply_page`]).
    Page(HistoryPageResult),
    /// A window result ([`crate::engine::TranscriptEngine::apply_window`]).
    Window(HistoryWindowResult),
}

/// Why [`load_history`] failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryLoadError<E> {
    /// The reader failed (transport, daemon or authorization failure).
    Read(E),
    /// A page belongs to another session, contains events of another session, is not in
    /// strictly ascending sequence order, lies outside the requested range, or reports more
    /// history without returning any.
    InconsistentPage,
}

impl<E: std::fmt::Display> std::fmt::Display for HistoryLoadError<E> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(error) => error.fmt(formatter),
            Self::InconsistentPage => formatter.write_str("inconsistent transcript history page"),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for HistoryLoadError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read(error) => Some(error),
            Self::InconsistentPage => None,
        }
    }
}

/// Execute `load` with `read`, one bounded canonical history read at a time.
///
/// A page takes one read (an `Around` window two, one on each side of the target), plus one
/// single-event probe for each session bound a window read does not reach (module
/// documentation).
///
/// `read` performs one bounded `session_history_page` read for `load.session_id`. No query it
/// receives asks for more than [`PageCaps::max_events_scanned`] events.
///
/// # Errors
///
/// * [`HistoryLoadError::Read`] with any error `read` returns.
/// * [`HistoryLoadError::InconsistentPage`] if a page belongs to another session, contains
///   events of another session, is not in strictly ascending sequence order, lies outside the
///   requested range, or reports more history without returning any.
pub async fn load_history<F, Fut, E>(
    load: HistoryLoadRequest,
    mut read: F,
) -> Result<HistoryLoaded, HistoryLoadError<E>>
where
    F: FnMut(SessionHistoryQuery) -> Fut,
    Fut: Future<Output = Result<SessionHistoryPage, E>>,
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
                    return Ok(HistoryLoaded::Window(HistoryWindowResult {
                        generation: issued.generation,
                        target: issued.request,
                        last_available_sequence: (!page.has_more)
                            .then(|| page.events.last().map(|event| event.sequence))
                            .flatten(),
                        events: page.events,
                        has_older: false,
                        has_newer: page.has_more,
                        first_available_sequence: None,
                    }));
                }
            };
            let first_available_sequence = if has_older {
                read.edge(Forward).await?
            } else {
                events.first().map(|event| event.sequence)
            };
            let last_available_sequence = if has_newer {
                read.edge(Backward).await?
            } else {
                events.last().map(|event| event.sequence)
            };
            Ok(HistoryLoaded::Window(window_result(
                issued.generation,
                issued.request,
                events,
                (has_older, has_newer),
                (first_available_sequence, last_available_sequence),
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

impl<F, Fut, E> Reader<'_, F>
where
    F: FnMut(SessionHistoryQuery) -> Fut,
    Fut: Future<Output = Result<SessionHistoryPage, E>>,
{
    /// One bounded canonical read starting at `cursor` (inclusive) in `direction`, validated
    /// and returned in ascending order.
    async fn range(
        &mut self,
        cursor: Option<u64>,
        limit: usize,
        direction: SessionHistoryDirection,
    ) -> Result<EventRange, HistoryLoadError<E>> {
        let session_id = self.session_id;
        let page = (self.read)(SessionHistoryQuery {
            cursor: cursor.map(|sequence| SessionHistoryCursor { sequence }),
            limit: limit.max(1),
            direction,
        })
        .await
        .map_err(HistoryLoadError::Read)?;
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
            return Err(HistoryLoadError::InconsistentPage);
        }
        Ok(EventRange {
            events: page.events,
            has_more: page.has_more,
        })
    }

    /// The sequence at one end of the session (`Forward`: first, `Backward`: last), read with
    /// a single-event probe; `None` for an empty session.
    async fn edge(
        &mut self,
        direction: SessionHistoryDirection,
    ) -> Result<Option<u64>, HistoryLoadError<E>> {
        let probe = self.range(None, 1, direction).await?;
        Ok(probe.events.first().map(|event| event.sequence))
    }
}

/// A window result with its session bounds (W12 hints). A bound is never inside the returned
/// events: a probe that raced a concurrent append or disagrees with the window is widened to
/// the window's own edge, since the estimate is presentation only.
fn window_result(
    generation: u64,
    target: WindowRequest,
    events: Vec<SessionEvent>,
    (has_older, has_newer): (bool, bool),
    (first, last): (Option<u64>, Option<u64>),
) -> HistoryWindowResult {
    let window_first = events.first().map(|event| event.sequence);
    let window_last = events.last().map(|event| event.sequence);
    let first_available_sequence = match (first, window_first) {
        (Some(first), Some(window)) => Some(first.min(window)),
        (first, window) => first.or(window),
    };
    let last_available_sequence = match (last, window_last) {
        (Some(last), Some(window)) => Some(last.max(window)),
        (last, window) => last.or(window),
    };
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
