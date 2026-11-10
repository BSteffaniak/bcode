//! Window manager tests, one or more per rule (W1–W13, W7a, SD1–SD3), plus the phase-5
//! work counters (no full rebuild on paging).

use bcode_session_models::{
    ClientId, SessionEvent, SessionEventKind, SessionId, TurnAdmissionMetadata,
};
use bcode_session_view::SessionView;
use bcode_session_view_models::{TranscriptChange, TranscriptChanges};

use super::*;
use crate::units::structure::FlatStructure;
use crate::units::{BoundaryEdge, UnitKey, Units};

/// Canonical history: odd sequences are user messages (turn starts), even ones assistant
/// messages, so every turn is two events.
struct History {
    id: SessionId,
    events: Vec<SessionEvent>,
}

impl History {
    fn new(count: u64) -> Self {
        let id = SessionId::new();
        let mut history = Self {
            id,
            events: Vec::new(),
        };
        for _ in 0..count {
            history.push();
        }
        history
    }

    fn event(&self, sequence: u64) -> SessionEvent {
        let kind = if sequence % 2 == 1 {
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: format!("user {sequence}"),
                admission: TurnAdmissionMetadata::default(),
            }
        } else {
            SessionEventKind::AssistantMessage {
                text: format!("assistant {sequence}"),
            }
        };
        SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id: self.id,
            provenance: None,
            kind,
        }
    }

    fn push(&mut self) -> SessionEvent {
        let event = self.event(self.events.len() as u64 + 1);
        self.events.push(event.clone());
        event
    }

    fn last(&self) -> u64 {
        self.events.len() as u64
    }

    /// Events with sequences in `first..=last`.
    fn range(&self, first: u64, last: u64) -> Vec<SessionEvent> {
        let index = |sequence: u64| usize::try_from(sequence).expect("fixture index fits usize");
        self.events[index(first - 1)..index(last)].to_vec()
    }

    fn window(
        &self,
        generation: u64,
        target: WindowRequest,
        first: u64,
        last: u64,
    ) -> HistoryWindowResult {
        HistoryWindowResult {
            generation,
            target,
            events: self.range(first, last),
            has_older: first > 1,
            has_newer: last < self.last(),
            first_available_sequence: Some(1),
            last_available_sequence: Some(self.last()),
        }
    }

    fn page(
        &self,
        generation: u64,
        direction: PageDirection,
        first: u64,
        last: u64,
    ) -> HistoryPageResult {
        HistoryPageResult {
            generation,
            direction,
            events: self.range(first, last),
            has_more: match direction {
                PageDirection::Older => first > 1,
                PageDirection::Newer => last < self.last(),
            },
            unaligned: false,
        }
    }
}

/// Attach with a tail window and grow it with older pages to `first..=last` (W10: one window
/// holds at most 512 events, so larger resident ranges are reached by paging).
fn attach_tail(history: &History, first: u64) -> (WindowManager, SessionView) {
    let mut window = WindowManager::new();
    let mut session = SessionView::new();
    let issued = window.request_window(WindowRequest::Tail);
    let last = history.last();
    let result = history.window(
        issued.generation,
        WindowRequest::Tail,
        first.max(last.saturating_sub(PAGE_EVENTS - 1)),
        last,
    );
    window
        .apply_window(&result, &mut session)
        .expect("tail window");
    grow(&mut window, &mut session, history, first, last);
    (window, session)
}

/// Attach with an `Around` window of at most 512 events centered in `first..=last`, grown with
/// pages to exactly `first..=last`.
fn attach_around(history: &History, first: u64, last: u64) -> (WindowManager, SessionView) {
    let mut window = WindowManager::new();
    let mut session = SessionView::new();
    let request = WindowRequest::Around {
        sequence: u64::midpoint(first, last),
    };
    let issued = window.request_window(request);
    let result = history.window(issued.generation, request, first, last);
    window
        .apply_window(&result, &mut session)
        .expect("around window");
    grow(&mut window, &mut session, history, first, last);
    (window, session)
}

/// Page the window out to `first..=last` with pages of at most 512 events.
fn grow(
    window: &mut WindowManager,
    session: &mut SessionView,
    history: &History,
    first: u64,
    last: u64,
) {
    if last - first < PAGE_EVENTS {
        // The window itself holds the range (aligned to whole turns, W4).
        return;
    }
    while window
        .summary()
        .first_sequence
        .is_some_and(|start| start > first)
    {
        let issued = window.request_page(PageDirection::Older).expect("page");
        let start = window.summary().first_sequence.expect("resident");
        let page = history.page(
            issued.generation,
            PageDirection::Older,
            first.max(start.saturating_sub(PAGE_EVENTS)),
            start - 1,
        );
        let mut page = page;
        page.has_more = page.events.first().is_some_and(|event| event.sequence > 1);
        window.apply_page(&page, session).expect("older page");
    }
    while window.summary().last_sequence.is_some_and(|end| end < last) {
        let issued = window.request_page(PageDirection::Newer).expect("page");
        let end = window.summary().last_sequence.expect("resident");
        let page = history.page(
            issued.generation,
            PageDirection::Newer,
            end + 1,
            last.min(end + PAGE_EVENTS),
        );
        window.apply_page(&page, session).expect("newer page");
    }
}

fn item_sequences(session: &SessionView) -> Vec<u64> {
    session
        .snapshot()
        .transcript
        .items
        .iter()
        .filter_map(|item| item.sequence)
        .collect()
}

fn inserted_count(changes: &TranscriptChanges) -> usize {
    changes
        .changes
        .iter()
        .map(|change| match change {
            TranscriptChange::Inserted { ids, .. } => ids.len(),
            _ => 0,
        })
        .sum()
}

fn removed_count(changes: &TranscriptChanges) -> usize {
    changes
        .changes
        .iter()
        .map(|change| match change {
            TranscriptChange::Removed { ids } => ids.len(),
            _ => 0,
        })
        .sum()
}

#[test]
fn w10_tail_attach_is_one_window_request_and_live() {
    let history = History::new(200);
    let mut window = WindowManager::new();
    let mut session = SessionView::new();
    let issued = window.request_window(WindowRequest::Tail);
    assert_eq!(issued.request, WindowRequest::Tail);
    assert_eq!(
        window.summary().in_flight.replace,
        Some(WindowRequest::Tail)
    );
    assert_eq!(
        window.summary().boundary_status(BoundaryEdge::Older),
        BoundaryStatus::Loading
    );
    let result = history.window(issued.generation, WindowRequest::Tail, 101, 200);
    let incorporated = window.apply_window(&result, &mut session).expect("tail");
    assert_eq!(inserted_count(&incorporated.changes), 100);
    let summary = window.summary();
    assert_eq!(summary.mode, WindowMode::Live);
    assert_eq!(
        (summary.first_sequence, summary.last_sequence),
        (Some(101), Some(200))
    );
    assert_eq!(summary.event_count, 100);
    assert!(summary.has_older && !summary.has_newer);
    assert_eq!(summary.in_flight.replace, None);
    assert_eq!(
        summary.boundary_status(BoundaryEdge::Older),
        BoundaryStatus::MoreAvailable
    );
    assert_eq!(item_sequences(&session).first(), Some(&101));
}

#[test]
fn w5_prefetch_requests_one_page_per_side_within_distance() {
    let history = History::new(400);
    let (mut window, _session) = attach_tail(&history, 301);
    // 48 rows is the minimum distance: 60 rows above the viewport need nothing.
    assert_eq!(
        window.poll_prefetch(
            EdgeRows {
                before: 60,
                after: 0
            },
            10
        ),
        Vec::new()
    );
    let issued = window.poll_prefetch(
        EdgeRows {
            before: 47,
            after: 0,
        },
        10,
    );
    assert_eq!(
        issued,
        vec![IssuedRequest {
            generation: window.summary().generation,
            request: PageRequest::Older {
                before_sequence: 301
            }
        }]
    );
    assert!(window.summary().in_flight.older);
    // At most one request in flight per direction.
    assert_eq!(
        window.poll_prefetch(
            EdgeRows {
                before: 0,
                after: 0
            },
            10
        ),
        Vec::new()
    );
    // A live window never requests newer pages.
    assert!(window.request_page(PageDirection::Newer).is_none());
}

#[test]
fn w5_prefetch_distance_scales_with_viewport() {
    let history = History::new(400);
    let (mut window, _session) = attach_tail(&history, 301);
    // Viewport 40: distance 80 rows.
    assert_eq!(
        window
            .poll_prefetch(
                EdgeRows {
                    before: 79,
                    after: 0
                },
                40
            )
            .len(),
        1
    );
}

#[test]
fn w3_w4_older_page_is_incorporated_aligned_in_one_update() {
    let history = History::new(400);
    let (mut window, mut session) = attach_tail(&history, 301);
    let issued = window.request_page(PageDirection::Older).expect("page");
    // The host returns 250..=300; 250 is an assistant event (mid-turn) and is trimmed.
    let page = history.page(issued.generation, PageDirection::Older, 250, 300);
    let incorporated = window.apply_page(&page, &mut session).expect("page");
    assert_eq!(incorporated.direction, PageDirection::Older);
    assert!(!incorporated.exhausted);
    assert_eq!(window.summary().first_sequence, Some(251));
    assert_eq!(inserted_count(&incorporated.changes), 50);
    assert_eq!(removed_count(&incorporated.changes), 0);
    assert!(
        incorporated
            .changes
            .changes
            .iter()
            .all(|change| !matches!(change, TranscriptChange::Revised { .. })),
        "a prepend leaves resident items untouched"
    );
    assert_eq!(item_sequences(&session).first(), Some(&251));
    assert!(!window.summary().in_flight.older);
}

#[test]
fn w13_exhausting_older_page_marks_session_start() {
    let history = History::new(100);
    let (mut window, mut session) = attach_tail(&history, 51);
    let issued = window.request_page(PageDirection::Older).expect("page");
    let page = history.page(issued.generation, PageDirection::Older, 1, 50);
    let incorporated = window.apply_page(&page, &mut session).expect("page");
    assert!(incorporated.exhausted);
    assert!(window.summary().contains_head());
    assert_eq!(window.summary().first_available_sequence, Some(1));
    assert_eq!(
        window.summary().boundary_status(BoundaryEdge::Older),
        BoundaryStatus::Exhausted
    );
    assert_eq!(window.request_page(PageDirection::Older), None);
}

#[test]
fn w5_token_fencing_drops_stale_and_superseded_results() {
    let history = History::new(400);
    let (mut window, mut session) = attach_tail(&history, 301);
    let page_request = window.request_page(PageDirection::Older).expect("page");
    // A replacement cancels in-flight pages by generation.
    let jump = window.request_window(WindowRequest::Head);
    assert!(!window.summary().in_flight.older);
    let stale = history.page(page_request.generation, PageDirection::Older, 251, 300);
    assert_eq!(
        window.apply_page(&stale, &mut session),
        Err(StaleResult::Generation)
    );
    // A result for a request that is not in flight is dropped.
    let unrequested = history.page(jump.generation, PageDirection::Older, 251, 300);
    assert_eq!(
        window.apply_page(&unrequested, &mut session),
        Err(StaleResult::NotInFlight)
    );
    // A window result for another target is dropped; the matching one is applied.
    let wrong = history.window(jump.generation, WindowRequest::Tail, 301, 400);
    assert_eq!(
        window.apply_window(&wrong, &mut session),
        Err(StaleResult::NotInFlight)
    );
    let head = history.window(jump.generation, WindowRequest::Head, 1, 100);
    let incorporated = window.apply_window(&head, &mut session).expect("head");
    assert!(incorporated.mode_changed);
    assert_eq!(window.summary().mode, WindowMode::Detached);
    assert!(window.summary().has_newer && !window.summary().has_older);
    assert!(!window.summary().contains_tail(), "W13/C31");
}

#[test]
fn sd1_live_duplicates_are_never_reapplied() {
    let mut history = History::new(10);
    let (mut window, mut session) = attach_tail(&history, 1);
    let event = history.push();
    assert!(matches!(
        window.apply_live(LiveInput::Durable(Box::new(event.clone())), &mut session),
        LiveOutcome::Applied(_)
    ));
    assert_eq!(
        window.apply_live(LiveInput::Durable(Box::new(event)), &mut session),
        LiveOutcome::Duplicate
    );
    assert_eq!(window.summary().last_sequence, Some(11));
    assert_eq!(window.event_count(), 11);
}

#[test]
fn w2_detached_window_does_not_apply_live_events() {
    let mut history = History::new(300);
    let (mut window, mut session) = attach_around(&history, 101, 200);
    let before = session.snapshot().transcript.clone();
    let event = history.push();
    assert_eq!(
        window.apply_live(LiveInput::Durable(Box::new(event)), &mut session),
        LiveOutcome::Detached
    );
    assert_eq!(session.snapshot().transcript, before);
    assert_eq!(window.summary().latest_known_sequence, Some(301));
}

#[test]
fn w11_newer_page_reaching_the_tail_goes_live_with_buffered_events() {
    let mut history = History::new(300);
    let (mut window, mut session) = attach_around(&history, 201, 290);
    let issued = window
        .poll_prefetch(
            EdgeRows {
                before: 500,
                after: 0,
            },
            10,
        )
        .pop()
        .expect("newer page");
    // The window's incomplete-looking last turn (289–290) was trimmed (W4).
    assert_eq!(
        issued.request,
        PageRequest::Newer {
            after_sequence: 288
        }
    );
    // Live events while the newer page is in flight are buffered.
    let live = history.push();
    assert_eq!(
        window.apply_live(LiveInput::Durable(Box::new(live)), &mut session),
        LiveOutcome::Buffered
    );
    let mut page = history.page(issued.generation, PageDirection::Newer, 289, 300);
    page.has_more = false;
    let incorporated = window.apply_page(&page, &mut session).expect("page");
    assert!(incorporated.exhausted && incorporated.mode_changed);
    assert_eq!(incorporated.follow_up, None);
    assert_eq!(window.summary().mode, WindowMode::Live);
    assert_eq!(window.summary().last_sequence, Some(301));
    assert_eq!(item_sequences(&session).last(), Some(&301));
}

#[test]
fn w11_overflowed_buffer_rehydrates_live_state_instead() {
    let mut history = History::new(300);
    let (mut window, mut session) = attach_around(&history, 201, 290);
    let issued = window
        .request_page(PageDirection::Newer)
        .expect("newer page");
    for _ in 0..=LIVE_BUFFER_EVENTS {
        let event = history.push();
        let _ = window.apply_live(LiveInput::Durable(Box::new(event)), &mut session);
    }
    let mut page = history.page(issued.generation, PageDirection::Newer, 289, 300);
    page.has_more = false;
    let incorporated = window.apply_page(&page, &mut session).expect("page");
    let follow_up = incorporated.follow_up.expect("live-state request");
    assert_eq!(follow_up.request, WindowRequest::LiveState);
    assert!(!incorporated.mode_changed);
    // Boundary(Newer) shows loading until the live state is incorporated.
    assert_eq!(
        window.summary().boundary_status(BoundaryEdge::Newer),
        BoundaryStatus::Loading
    );
    let live_state = HistoryWindowResult {
        generation: follow_up.generation,
        target: WindowRequest::LiveState,
        events: history.range(301, history.last()),
        has_older: true,
        has_newer: false,
        first_available_sequence: Some(1),
        last_available_sequence: Some(history.last()),
    };
    let applied = window
        .apply_window(&live_state, &mut session)
        .expect("live state");
    assert!(applied.mode_changed);
    assert_eq!(window.summary().mode, WindowMode::Live);
    assert_eq!(window.summary().last_sequence, Some(history.last()));
}

#[test]
fn w11_live_state_read_short_of_the_tail_stays_detached_and_keeps_paging() {
    let mut history = History::new(300);
    let (mut window, mut session) = attach_around(&history, 201, 290);
    let issued = window
        .request_page(PageDirection::Newer)
        .expect("newer page");
    for _ in 0..=LIVE_BUFFER_EVENTS {
        let event = history.push();
        let _ = window.apply_live(LiveInput::Durable(Box::new(event)), &mut session);
    }
    let mut page = history.page(issued.generation, PageDirection::Newer, 289, 300);
    page.has_more = false;
    let follow_up = window
        .apply_page(&page, &mut session)
        .expect("page")
        .follow_up
        .expect("live-state request");
    // One bounded read covers only part of the overflowed live events.
    let read_last = 300 + u64::try_from(LIVE_BUFFER_EVENTS).expect("small") / 2;
    let live_state = HistoryWindowResult {
        generation: follow_up.generation,
        target: WindowRequest::LiveState,
        events: history.range(301, read_last),
        has_older: false,
        has_newer: true,
        first_available_sequence: None,
        last_available_sequence: None,
    };
    let applied = window
        .apply_window(&live_state, &mut session)
        .expect("live state");
    // Going live would leave a gap between the read and later live events (W11).
    assert!(!applied.mode_changed);
    assert_eq!(window.summary().mode, WindowMode::Detached);
    assert!(window.summary().has_newer);
    assert_eq!(window.summary().last_sequence, Some(read_last));
    assert_eq!(item_sequences(&session).last(), Some(&read_last));
    let next = window
        .request_page(PageDirection::Newer)
        .expect("newer paging continues");
    assert_eq!(
        next.request,
        PageRequest::Newer {
            after_sequence: read_last
        }
    );
}

#[test]
fn w11_live_discontinuity_during_the_last_newer_page_rehydrates_instead_of_going_live() {
    let mut history = History::new(300);
    let (mut window, mut session) = attach_around(&history, 201, 290);
    let issued = window
        .request_page(PageDirection::Newer)
        .expect("newer page");
    // 301 arrives live and is buffered; the host's feed then skips 302 (a gap) and delivers 303.
    let buffered = history.push();
    let _ = window.apply_live(LiveInput::Durable(Box::new(buffered)), &mut session);
    let _skipped = history.push();
    assert!(window.interrupt_live_buffer());
    let after_gap = history.push();
    let _ = window.apply_live(LiveInput::Durable(Box::new(after_gap)), &mut session);
    let mut page = history.page(issued.generation, PageDirection::Newer, 289, 300);
    page.has_more = false;
    let incorporated = window.apply_page(&page, &mut session).expect("page");
    // Going live here would append 303 directly after 300 and lose 301 and 302 for good.
    assert!(!incorporated.mode_changed);
    assert_eq!(window.summary().mode, WindowMode::Detached);
    let follow_up = incorporated.follow_up.expect("live-state request");
    assert_eq!(follow_up.request, WindowRequest::LiveState);
    let live_state = HistoryWindowResult {
        generation: follow_up.generation,
        target: WindowRequest::LiveState,
        events: history.range(301, history.last()),
        has_older: false,
        has_newer: false,
        first_available_sequence: None,
        last_available_sequence: None,
    };
    window
        .apply_window(&live_state, &mut session)
        .expect("live state");
    assert_eq!(window.summary().mode, WindowMode::Live);
    let sequences = item_sequences(&session);
    assert_eq!(&sequences[sequences.len() - 3..], [301, 302, 303]);
}

#[test]
fn w11_live_discontinuity_during_a_live_state_read_keeps_the_window_detached() {
    let mut history = History::new(300);
    let (mut window, mut session) = attach_around(&history, 201, 290);
    let issued = window
        .request_page(PageDirection::Newer)
        .expect("newer page");
    for _ in 0..=LIVE_BUFFER_EVENTS {
        let event = history.push();
        let _ = window.apply_live(LiveInput::Durable(Box::new(event)), &mut session);
    }
    let mut page = history.page(issued.generation, PageDirection::Newer, 289, 300);
    page.has_more = false;
    let follow_up = window
        .apply_page(&page, &mut session)
        .expect("page")
        .follow_up
        .expect("live-state request");
    // The read was served before the host's feed skipped inputs.
    let read_last = history.last();
    let _skipped = history.push();
    assert!(window.interrupt_live_buffer());
    let after_gap = history.push();
    let _ = window.apply_live(LiveInput::Durable(Box::new(after_gap)), &mut session);
    let live_state = HistoryWindowResult {
        generation: follow_up.generation,
        target: WindowRequest::LiveState,
        events: history.range(301, read_last),
        has_older: false,
        has_newer: false,
        first_available_sequence: None,
        last_available_sequence: None,
    };
    let applied = window
        .apply_window(&live_state, &mut session)
        .expect("live state");
    assert!(!applied.mode_changed);
    assert_eq!(window.summary().mode, WindowMode::Detached);
    assert_eq!(window.summary().last_sequence, Some(read_last));
    let next = window
        .request_page(PageDirection::Newer)
        .expect("newer paging continues past the gap");
    assert_eq!(
        next.request,
        PageRequest::Newer {
            after_sequence: read_last
        }
    );
}

#[test]
fn live_discontinuity_without_an_in_flight_rehydration_is_a_no_op() {
    let history = History::new(300);
    let (mut window, _session) = attach_around(&history, 201, 290);
    assert!(!window.interrupt_live_buffer());
}

#[test]
fn w11_newer_page_with_more_history_discards_the_buffer() {
    let mut history = History::new(400);
    let (mut window, mut session) = attach_around(&history, 101, 200);
    let issued = window.request_page(PageDirection::Newer).expect("page");
    let live = history.push();
    assert_eq!(
        window.apply_live(LiveInput::Durable(Box::new(live)), &mut session),
        LiveOutcome::Buffered
    );
    let page = history.page(issued.generation, PageDirection::Newer, 199, 250);
    let incorporated = window.apply_page(&page, &mut session).expect("page");
    assert!(!incorporated.exhausted);
    assert_eq!(window.summary().mode, WindowMode::Detached);
    // The page's last turn (249–250) is trimmed like any edge that may continue (W4).
    assert_eq!(window.summary().last_sequence, Some(248));
    assert!(!window.contains_sequence(401));
}

/// Units with every non-boundary unit `rows` tall, derived from the session document.
fn units_for(session: &SessionView, mode: WindowMode) -> Units {
    let mut units = Units::new();
    let _ = units.apply(
        &TranscriptChanges::default(),
        &session.snapshot().transcript.items,
        mode,
        &FlatStructure,
    );
    units
}

fn row_of(units: &Units, key: &UnitKey) -> u64 {
    let index = units.index_of(key).expect("resident unit");
    // Boundary rows are 1; item units are 2 rows each.
    if index == 0 {
        0
    } else {
        1 + 2 * (index as u64 - 1)
    }
}

fn height(key: &UnitKey) -> u64 {
    if key.is_boundary() { 1 } else { 2 }
}

#[test]
fn w7_w9_eviction_after_prepend_trims_the_tail_and_detaches() {
    let history = History::new(1400);
    let (mut window, mut session) = attach_tail(&history, 401);
    let issued = window.request_page(PageDirection::Older).expect("page");
    let page = history.page(issued.generation, PageDirection::Older, 1, 400);
    let _ = window.apply_page(&page, &mut session).expect("page");
    assert_eq!(window.event_count(), 1400);

    let units = units_for(&session, window.summary().mode);
    // The reader sits on sequence 405 near the old window start.
    let anchor = units.unit_for_sequence(405).expect("anchor").clone();
    let context = EvictionContext {
        viewport_top: row_of(&units, &anchor),
        viewport_height: 20,
        anchor: Some(&anchor),
        pinned: &[],
        deferred: false,
        resident_rows: 0,
    };
    let outcome = window
        .evict(&context, &units, &height, &mut session)
        .expect("eviction");
    assert_eq!(
        outcome.side,
        PageDirection::Newer,
        "opposite of the prepend"
    );
    assert!(outcome.mode_changed, "W9: tail eviction detaches");
    assert!(!outcome.unaligned);
    assert_eq!(window.summary().mode, WindowMode::Detached);
    assert!(window.summary().has_newer);
    assert!(window.event_count() <= RESIDENT_TARGET_EVENTS + 2);
    assert!(window.contains_sequence(405), "anchor stays resident");
    // Every fixture event produces one item; eviction removes exactly their items.
    assert_eq!(
        removed_count(&outcome.changes) as u64,
        outcome.removed_events
    );
    // The cut is at a turn start: the first evicted sequence is a user message.
    let last = window.summary().last_sequence.expect("resident");
    assert_eq!((last + 1) % 2, 1, "evicted tail starts at a turn start");
}

#[test]
fn w7_protected_band_and_pins_survive_eviction() {
    let history = History::new(1400);
    let (mut window, mut session) = attach_around(&history, 1, 1000);
    let issued = window.request_page(PageDirection::Newer).expect("page");
    let page = history.page(issued.generation, PageDirection::Newer, 999, 1300);
    let _ = window.apply_page(&page, &mut session).expect("page");
    let units = units_for(&session, window.summary().mode);
    let anchor = units.unit_for_sequence(1001).expect("anchor").clone();
    let pinned = units.unit_for_sequence(801).expect("pinned").clone();
    let context = EvictionContext {
        viewport_top: row_of(&units, &anchor),
        viewport_height: 20,
        anchor: Some(&anchor),
        pinned: std::slice::from_ref(&pinned),
        deferred: false,
        resident_rows: 0,
    };
    let outcome = window
        .evict(&context, &units, &height, &mut session)
        .expect("eviction");
    assert_eq!(outcome.side, PageDirection::Older);
    assert!(!outcome.mode_changed);
    // Pinned unit, anchor and the band (48 rows = 24 units = 24 events above the viewport)
    // stay resident.
    assert!(window.contains_sequence(801));
    assert!(window.contains_sequence(1001));
    assert!(window.contains_sequence(1001 - 24));
    let first = window.summary().first_sequence.expect("resident");
    assert_eq!(first % 2, 1, "window starts at a turn start");
}

#[test]
fn w7_eviction_is_deferred_and_waits_for_the_trigger() {
    let history = History::new(1400);
    let (mut window, mut session) = attach_tail(&history, 401);
    let units = units_for(&session, window.summary().mode);
    let anchor = units.unit_for_sequence(1001).expect("anchor").clone();
    let mut context = EvictionContext {
        viewport_top: row_of(&units, &anchor),
        viewport_height: 20,
        anchor: Some(&anchor),
        pinned: &[],
        deferred: false,
        resident_rows: 0,
    };
    // 1,000 events: at or below the trigger nothing is evicted.
    assert!(
        window
            .evict(&context, &units, &height, &mut session)
            .is_none()
    );
    let issued = window.request_page(PageDirection::Older).expect("page");
    let page = history.page(issued.generation, PageDirection::Older, 201, 400);
    let _ = window.apply_page(&page, &mut session).expect("page");
    let units = units_for(&session, window.summary().mode);
    context.viewport_top = row_of(&units, &anchor);
    context.deferred = true;
    assert!(
        window
            .evict(&context, &units, &height, &mut session)
            .is_none()
    );
    assert_eq!(window.event_count(), 1200);
}

#[test]
fn w7a_ceiling_blocks_requests_while_eviction_cannot_make_room() {
    let history = History::new(1600);
    let (mut window, mut session) = attach_tail(&history, 601);
    let issued = window.request_page(PageDirection::Older).expect("page");
    let page = history.page(issued.generation, PageDirection::Older, 401, 600);
    let _ = window.apply_page(&page, &mut session).expect("page");
    assert_eq!(window.event_count(), 1200);
    // Eviction deferred: 1,200 + 512 > 1,536, so no page may be requested.
    assert_eq!(window.request_page(PageDirection::Older), None);
    assert_eq!(
        window.poll_prefetch(
            EdgeRows {
                before: 0,
                after: 0
            },
            10
        ),
        Vec::new()
    );
    // W7a: the boundary waiting for room shows `loading` (W13).
    assert!(window.summary().ceiling_blocked.older);
    assert_eq!(
        window.summary().boundary_status(BoundaryEdge::Older),
        BoundaryStatus::Loading
    );
}

#[test]
fn w7_single_oversized_turn_is_cut_unaligned() {
    // One user message followed by 1,299 assistant events: one turn larger than the target.
    let mut history = History::new(0);
    for sequence in 1..=1300_u64 {
        let mut event = history.event(if sequence == 1 { 1 } else { 2 });
        event.sequence = sequence;
        event.timestamp_ms = sequence;
        history.events.push(event);
    }
    // Reached by reading upward: a bounded tail window, then older pages (W10).
    let (mut window, mut session) = attach_tail(&history, 1);
    let mut session_units = units_for(&session, window.summary().mode);
    let anchor = session_units
        .unit_for_sequence(2)
        .cloned()
        .expect("reader near the turn start");
    let context = EvictionContext {
        viewport_top: row_of(&session_units, &anchor),
        viewport_height: 20,
        anchor: Some(&anchor),
        pinned: &[],
        deferred: false,
        resident_rows: 0,
    };
    let outcome = window
        .evict(&context, &session_units, &height, &mut session)
        .expect("eviction");
    assert!(outcome.unaligned);
    assert_eq!(
        outcome.side,
        PageDirection::Newer,
        "opposite of the prepends"
    );
    assert!(window.event_count() <= RESIDENT_TARGET_EVENTS);
    session_units = units_for(&session, window.summary().mode);
    assert!(session_units.contains(&anchor));
}

#[test]
fn w12_estimate_uses_smoothed_rows_per_event() {
    let history = History::new(1000);
    let (mut window, _session) = attach_tail(&history, 501);
    window.observe_rows(1000);
    let rate = window.summary().rows_per_event_estimate;
    assert!(
        rate > 1.0 && rate < 2.0,
        "smoothed toward 2.0 from 1.0: {rate}"
    );
    let estimate = window.summary().position_estimate(1000);
    assert_eq!(estimate.resident_rows, 1000);
    assert!(estimate.rows_before_window > 500);
    assert_eq!(estimate.rows_after_window, 0);
    let bmux = window.bmux_estimate(1000);
    assert_eq!(bmux.after, 0);
    assert!(bmux.before > 500);
}

#[test]
fn live_stream_changes_are_drained_by_key() {
    let mut history = History::new(4);
    let (mut window, mut session) = attach_tail(&history, 1);
    let event = history.push();
    let LiveOutcome::Applied(changes) =
        window.apply_live(LiveInput::Durable(Box::new(event)), &mut session)
    else {
        panic!("live window applies live events");
    };
    assert_eq!(inserted_count(&changes), 1);
    // Nothing changed since: the next drain is empty.
    assert!(window.sync_document(&mut session).is_empty());
}

/// Items of a from-scratch rebuild of `first..=last`, for differential checks.
fn rebuilt_items(
    history: &History,
    first: u64,
    last: u64,
) -> Vec<bcode_session_view_models::TranscriptViewItem> {
    let mut session = SessionView::new();
    let _ = session.replace_history_window_detached(&history.range(first, last));
    session.snapshot().transcript.items.clone()
}

#[test]
fn phase5_older_paging_and_eviction_perform_no_full_rebuild() {
    let history = History::new(3000);
    let (mut window, mut session) = attach_tail(&history, 2901);
    let attached = window.work();
    assert_eq!(
        attached.replacements, 1,
        "attach is the one window reduction"
    );
    let mut reduced = attached.events_reduced;
    let mut pages = 0;
    while window.summary().has_older {
        let issued = window.request_page(PageDirection::Older).expect("page");
        let first = window.summary().first_sequence.expect("resident");
        let page = history.page(
            issued.generation,
            PageDirection::Older,
            first.saturating_sub(200).max(1),
            first - 1,
        );
        let incorporated = window.apply_page(&page, &mut session).expect("page");
        pages += 1;
        let work = window.work();
        assert_eq!(
            work.events_reduced - reduced,
            inserted_count(&incorporated.changes) as u64,
            "only the page's events are reduced"
        );
        reduced = work.events_reduced;
        // Evict the far tail like the engine does after incorporation (W7).
        let units = units_for(&session, window.summary().mode);
        let anchor = units
            .unit_for_sequence(window.summary().first_sequence.expect("resident") + 1)
            .expect("anchor")
            .clone();
        let context = EvictionContext {
            viewport_top: row_of(&units, &anchor),
            viewport_height: 20,
            anchor: Some(&anchor),
            pinned: &[],
            deferred: false,
            resident_rows: 0,
        };
        let _ = window.evict(&context, &units, &height, &mut session);
        assert!(window.event_count() <= RESIDENT_CEILING_EVENTS);
        let summary = window.summary();
        assert_eq!(
            session.snapshot().transcript.items,
            rebuilt_items(
                &history,
                summary.first_sequence.expect("resident"),
                summary.last_sequence.expect("resident")
            ),
            "the spliced projection equals a rebuild of the resident range"
        );
    }
    let work = window.work();
    assert_eq!(work.replacements, 1, "paging never rebuilds the window");
    assert_eq!(work.fallbacks, 0);
    assert_eq!(work.prepends, pages);
    assert!(work.evictions > 0, "the tail was evicted on the way up");
    assert_eq!(window.summary().first_sequence, Some(1));
    assert_eq!(window.summary().mode, WindowMode::Detached);
}

#[test]
fn phase5_newer_paging_appends_without_rebuild_and_goes_live() {
    let history = History::new(1200);
    let (mut window, mut session) = attach_around(&history, 1, 400);
    assert_eq!(window.summary().mode, WindowMode::Detached);
    while window.summary().mode == WindowMode::Detached {
        let issued = window.request_page(PageDirection::Newer).expect("page");
        let last = window.summary().last_sequence.expect("resident");
        let page = history.page(
            issued.generation,
            PageDirection::Newer,
            last + 1,
            (last + 300).min(history.last()),
        );
        let _ = window.apply_page(&page, &mut session).expect("page");
    }
    let work = window.work();
    assert_eq!(work.replacements, 1);
    assert_eq!(work.fallbacks, 0);
    assert_eq!(work.appends, 3);
    assert_eq!(work.events_reduced, history.last());
    assert_eq!(
        session.snapshot().transcript.items,
        rebuilt_items(&history, 1, history.last())
    );
}

#[test]
fn sd2_unaligned_page_falls_back_to_one_replacement() {
    let history = History::new(400);
    let (mut window, mut session) = attach_tail(&history, 301);
    let issued = window.request_page(PageDirection::Older).expect("page");
    // One turn larger than the caps: the host marks the page unaligned and it starts mid-turn.
    let mut page = history.page(issued.generation, PageDirection::Older, 250, 300);
    page.unaligned = true;
    let _ = window.apply_page(&page, &mut session).expect("page");
    assert_eq!(window.summary().first_sequence, Some(250));
    assert_eq!(
        window.work().fallbacks,
        0,
        "the page itself ends at a turn start"
    );
    let issued = window.request_page(PageDirection::Older).expect("page");
    let page = history.page(issued.generation, PageDirection::Older, 201, 249);
    let incorporated = window.apply_page(&page, &mut session).expect("page");
    let work = window.work();
    assert_eq!(
        work.fallbacks, 1,
        "the resident start is mid-turn: SD2 fallback"
    );
    assert_eq!(work.replacements, 2);
    assert_eq!(
        removed_count(&incorporated.changes),
        0,
        "deterministic ids: the fallback keeps every resident item (P1)"
    );
    assert_eq!(
        session.snapshot().transcript.items,
        rebuilt_items(&history, 201, 400)
    );
}

// ---- review regressions (phase 5) ----

fn stream_delta(history: &History, text: &str) -> LiveInput {
    LiveInput::Stream(Box::new(bcode_session_models::SessionLiveEvent {
        session_id: history.id,
        kind: bcode_session_models::SessionLiveEventKind::AssistantTextDelta {
            turn_id: "turn-live".to_owned(),
            segment_id: "live".to_owned(),
            segment_order: 0,
            text: text.to_owned(),
        },
    }))
}

fn resident_sequences(window: &WindowManager) -> Vec<u64> {
    window.events().map(|event| event.sequence).collect()
}

/// W1, W11: a tail eviction while a live-state read is in flight cancels that read. Its result
/// continues after the old tail, so applying it would leave a hole in a window marked live.
#[test]
fn w11_tail_eviction_cancels_an_in_flight_live_state_read() {
    let mut history = History::new(2000);
    let (mut window, mut session) = attach_around(&history, 1001, 1990);
    let issued = window
        .request_page(PageDirection::Newer)
        .expect("newer page");
    for _ in 0..=LIVE_BUFFER_EVENTS {
        let event = history.push();
        let _ = window.apply_live(LiveInput::Durable(Box::new(event)), &mut session);
    }
    let end = window.summary().last_sequence.expect("resident");
    let mut page = history.page(issued.generation, PageDirection::Newer, end + 1, 2000);
    page.has_more = false;
    let follow_up = window
        .apply_page(&page, &mut session)
        .expect("page")
        .follow_up
        .expect("live-state request (buffer overflowed)");
    // The reader scrolls up while the live-state read is in flight.
    let issued = window
        .request_page(PageDirection::Older)
        .expect("older page");
    let first = window.summary().first_sequence.expect("resident");
    let page = history.page(
        issued.generation,
        PageDirection::Older,
        first - 512,
        first - 1,
    );
    let _ = window.apply_page(&page, &mut session).expect("older page");
    let units = units_for(&session, window.summary().mode);
    let anchor = units
        .unit_for_sequence(first - 511)
        .expect("anchor")
        .clone();
    let context = EvictionContext {
        viewport_top: row_of(&units, &anchor),
        viewport_height: 20,
        anchor: Some(&anchor),
        pinned: &[],
        deferred: false,
        resident_rows: 0,
    };
    let outcome = window
        .evict(&context, &units, &height, &mut session)
        .expect("eviction");
    assert_eq!(outcome.side, PageDirection::Newer);
    let tail = window.summary().last_sequence.expect("resident");
    assert!(tail < 2000, "the old tail was evicted");

    let live_state = HistoryWindowResult {
        generation: follow_up.generation,
        target: WindowRequest::LiveState,
        events: history.range(2001, 2000 + PAGE_EVENTS),
        has_older: true,
        has_newer: false,
        first_available_sequence: Some(1),
        last_available_sequence: Some(history.last()),
    };
    assert_eq!(
        window.apply_window(&live_state, &mut session),
        Err(StaleResult::NotInFlight),
        "the cancelled read is dropped"
    );
    let resident = resident_sequences(&window);
    assert!(
        resident.windows(2).all(|pair| pair[1] == pair[0] + 1),
        "the resident window has no hole"
    );
    assert_eq!(window.summary().mode, WindowMode::Detached);
    assert!(window.summary().has_newer);
    assert!(!window.summary().in_flight.newer);
    let next = window
        .request_page(PageDirection::Newer)
        .expect("newer paging resumes from the new tail");
    assert_eq!(
        next.request,
        PageRequest::Newer {
            after_sequence: tail
        }
    );
}

/// W11: a stream that started while an intermediate `Newer` page was in flight is still in
/// flight when the next `Newer` request is issued, so reaching the tail rehydrates live state
/// instead of applying only the deltas buffered during the last page.
#[test]
fn w11_stream_seen_during_an_intermediate_newer_page_rehydrates_live_state() {
    let history = History::new(1400);
    let (mut window, mut session) = attach_around(&history, 101, 400);
    let issued = window.request_page(PageDirection::Newer).expect("page");
    assert_eq!(
        window.apply_live(stream_delta(&history, "hello "), &mut session),
        LiveOutcome::Buffered
    );
    let last = window.summary().last_sequence.expect("resident");
    let page = history.page(
        issued.generation,
        PageDirection::Newer,
        last + 1,
        last + 400,
    );
    let incorporated = window.apply_page(&page, &mut session).expect("page");
    assert!(
        !incorporated.exhausted,
        "more history: the buffer is discarded"
    );

    let issued = window.request_page(PageDirection::Newer).expect("page");
    assert_eq!(
        window.apply_live(stream_delta(&history, "world"), &mut session),
        LiveOutcome::Buffered
    );
    let last = window.summary().last_sequence.expect("resident");
    let mut page = history.page(issued.generation, PageDirection::Newer, last + 1, 1400);
    page.has_more = false;
    let incorporated = window.apply_page(&page, &mut session).expect("page");
    assert_eq!(
        incorporated.follow_up.map(|issued| issued.request),
        Some(WindowRequest::LiveState),
        "the in-flight stream is rehydrated"
    );
    assert!(!incorporated.mode_changed);
}

/// W2, W12: durable live events buffered while a `Newer` page is in flight are recorded as
/// newer activity even when the page reports more history and the buffer is discarded.
#[test]
fn w2_events_buffered_during_a_newer_page_with_more_history_are_recorded() {
    let mut history = History::new(1400);
    let (mut window, mut session) = attach_around(&history, 101, 400);
    assert_eq!(window.summary().newest_known_sequence(), Some(1400));
    let issued = window.request_page(PageDirection::Newer).expect("page");
    let live = history.push();
    assert_eq!(
        window.apply_live(LiveInput::Durable(Box::new(live)), &mut session),
        LiveOutcome::Buffered
    );
    let last = window.summary().last_sequence.expect("resident");
    let page = history.page(issued.generation, PageDirection::Newer, last + 1, 800);
    let incorporated = window.apply_page(&page, &mut session).expect("page");
    assert!(!incorporated.exhausted);
    assert_eq!(window.summary().newest_known_sequence(), Some(1401));
}

/// W7, W7a: while eviction is deferred, durable events applied without a page request (here
/// the buffered catch-up of a newer page reaching the tail) cannot keep the window above the
/// 1,536-event ceiling; it is trimmed back to the ceiling, never past the reader.
#[test]
fn w7a_ceiling_applies_while_eviction_is_deferred() {
    let mut history = History::new(1536);
    let (mut window, mut session) = attach_around(&history, 1, 1024);
    let issued = window
        .request_page(PageDirection::Newer)
        .expect("room for one page");
    for _ in 0..LIVE_BUFFER_EVENTS {
        let event = history.push();
        assert_eq!(
            window.apply_live(LiveInput::Durable(Box::new(event)), &mut session),
            LiveOutcome::Buffered
        );
    }
    let end = window.summary().last_sequence.expect("resident");
    let mut page = history.page(issued.generation, PageDirection::Newer, end + 1, 1536);
    page.has_more = false;
    let incorporated = window.apply_page(&page, &mut session).expect("page");
    assert!(incorporated.mode_changed, "the buffered catch-up goes live");
    assert!(window.event_count() > RESIDENT_CEILING_EVENTS);

    let units = units_for(&session, window.summary().mode);
    let anchor = units
        .unit_for_sequence(history.last())
        .expect("reader at the tail")
        .clone();
    let context = EvictionContext {
        viewport_top: row_of(&units, &anchor),
        viewport_height: 20,
        anchor: Some(&anchor),
        pinned: &[],
        deferred: true,
        resident_rows: 0,
    };
    let outcome = window
        .evict(&context, &units, &height, &mut session)
        .expect("the ceiling applies while deferred");
    assert_eq!(outcome.side, PageDirection::Older);
    assert!(window.event_count() <= RESIDENT_CEILING_EVENTS);
    assert!(
        window.event_count() > RESIDENT_MAX_EVENTS,
        "a deferred eviction trims only back to the ceiling"
    );
    assert!(window.contains_sequence(history.last()));
    assert_eq!(
        session.snapshot().transcript.items,
        rebuilt_items(
            &history,
            window.summary().first_sequence.expect("resident"),
            history.last()
        )
    );
}

/// W7: an eviction found impossible (the only permitted side is pinned) is not planned again
/// while its inputs are unchanged; any change of the inputs plans again.
#[test]
fn w7_impossible_eviction_is_not_replanned_for_unchanged_inputs() {
    let history = History::new(1400);
    let (mut window, mut session) = attach_tail(&history, 401);
    let issued = window.request_page(PageDirection::Older).expect("page");
    let page = history.page(issued.generation, PageDirection::Older, 201, 400);
    let _ = window.apply_page(&page, &mut session).expect("page");
    assert_eq!(window.event_count(), 1200);
    let units = units_for(&session, window.summary().mode);
    let anchor = units.unit_for_sequence(601).expect("anchor").clone();
    // The tail (the side opposite the prepend) is pinned, so nothing can be evicted.
    let tail = units.unit_for_sequence(1400).expect("tail").clone();
    let mut context = EvictionContext {
        viewport_top: row_of(&units, &anchor),
        viewport_height: 20,
        anchor: Some(&anchor),
        pinned: std::slice::from_ref(&tail),
        deferred: false,
        resident_rows: 2400,
    };
    for _ in 0..5 {
        assert!(
            window
                .evict(&context, &units, &height, &mut session)
                .is_none()
        );
    }
    assert_eq!(window.work().eviction_plans, 1, "planned once");
    context.viewport_top += 1;
    assert!(
        window
            .evict(&context, &units, &height, &mut session)
            .is_none()
    );
    assert_eq!(window.work().eviction_plans, 2, "changed inputs plan again");
}

/// A tool-heavy history: every turn has tool calls with a resolved interaction, results and
/// lifecycle events.
fn tool_history(turns: u64) -> History {
    let mut history = History::new(0);
    let id = history.id;
    let push = |history: &mut History, kind: SessionEventKind| {
        let sequence = history.last() + 1;
        history.events.push(SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id: id,
            provenance: None,
            kind,
        });
    };
    for turn in 0..turns {
        push(
            &mut history,
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: format!("user {turn}"),
                admission: TurnAdmissionMetadata::default(),
            },
        );
        for call in 0..2 {
            let call_id = format!("call-{turn}-{call}");
            push(
                &mut history,
                SessionEventKind::ToolCallRequested {
                    tool_call_id: call_id.clone(),
                    producer_plugin_id: None,
                    tool_name: "read".to_owned(),
                    arguments_json: "{}".to_owned(),
                    working_directory: None,
                },
            );
            push(
                &mut history,
                SessionEventKind::ToolExchangeRequested {
                    request: bcode_session_models::ToolExchangeRequest {
                        invocation_id: call_id.clone(),
                        exchange_id: format!("exchange-{turn}-{call}"),
                        producer_id: "test.producer".to_owned(),
                        schema: "test.question/schema".to_owned(),
                        schema_version: 1,
                        payload: serde_json::json!({"turn": turn}),
                        response_policy: bcode_session_models::ToolExchangeResponsePolicy::Required,
                    },
                },
            );
            push(
                &mut history,
                SessionEventKind::ToolExchangeResolved {
                    event: bcode_session_models::ToolExchangeResolutionEvent {
                        invocation_id: call_id.clone(),
                        exchange_id: format!("exchange-{turn}-{call}"),
                        resolution: bcode_session_models::ToolExchangeResolution::Responded {
                            payload: serde_json::json!({"answer": turn}),
                        },
                    },
                },
            );
            push(
                &mut history,
                SessionEventKind::ToolInvocationResultRecorded {
                    record: bcode_session_models::ToolInvocationResultRecord {
                        invocation_id: call_id,
                        model_output: "done".to_owned(),
                        is_error: false,
                        presentation: None,
                        result: None,
                        content: Vec::new(),
                    },
                },
            );
        }
        push(
            &mut history,
            SessionEventKind::AssistantMessage {
                text: format!("assistant {turn}"),
            },
        );
    }
    history
}

/// Phase 5 exit over tool-heavy content: after a jump away from a region and back to the tail,
/// paging up through the previously resident region stays incremental (no SD2 fallback) and
/// equals a rebuild of the resident range after every page.
#[test]
fn phase5_tool_heavy_paging_after_a_jump_is_incremental() {
    let history = tool_history(200);
    let mut window = WindowManager::new();
    let mut session = SessionView::new();
    for target in [WindowRequest::Head, WindowRequest::Tail] {
        let issued = window.request_window(target);
        let (first, last) = match target {
            WindowRequest::Head => (1, PAGE_EVENTS),
            _ => (history.last() - PAGE_EVENTS + 1, history.last()),
        };
        let result = history.window(issued.generation, target, first, last);
        let _ = window.apply_window(&result, &mut session).expect("window");
    }
    let attached = window.work();
    assert_eq!(attached.replacements, 2);
    while window.summary().has_older {
        let issued = window.request_page(PageDirection::Older).expect("page");
        let first = window.summary().first_sequence.expect("resident");
        let mut page = history.page(
            issued.generation,
            PageDirection::Older,
            first.saturating_sub(300).max(1),
            first - 1,
        );
        page.has_more = page.events.first().is_some_and(|event| event.sequence > 1);
        let _ = window.apply_page(&page, &mut session).expect("page");
        let units = units_for(&session, window.summary().mode);
        let first = window.summary().first_sequence.expect("resident");
        let anchor = units.unit_for_sequence(first).expect("anchor").clone();
        let context = EvictionContext {
            viewport_top: row_of(&units, &anchor),
            viewport_height: 20,
            anchor: Some(&anchor),
            pinned: &[],
            deferred: false,
            resident_rows: 0,
        };
        let _ = window.evict(&context, &units, &height, &mut session);
        let summary = window.summary();
        assert_eq!(
            session.snapshot().transcript.items,
            rebuilt_items(
                &history,
                summary.first_sequence.expect("resident"),
                summary.last_sequence.expect("resident")
            ),
            "the projection equals a rebuild of the resident range"
        );
    }
    let work = window.work();
    assert_eq!(work.fallbacks, 0, "no SD2 fallback: {work:?}");
    assert_eq!(
        work.replacements, attached.replacements,
        "paging never rebuilds"
    );
    assert!(work.prepends > 0 && work.evictions > 0);
    assert_eq!(window.summary().first_sequence, Some(1));
}
