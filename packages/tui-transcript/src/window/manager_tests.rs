//! Window manager tests, one or more per rule (W1–W13, W7a, SD1, SD5).

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

/// Attach with a tail window of `first..=last` (W10).
fn attach_tail(history: &History, first: u64) -> (WindowManager, SessionView) {
    let mut window = WindowManager::new();
    let mut session = SessionView::new();
    let issued = window.request_window(WindowRequest::Tail);
    let result = history.window(
        issued.generation,
        WindowRequest::Tail,
        first,
        history.last(),
    );
    window
        .apply_window(&result, &mut session)
        .expect("tail window");
    (window, session)
}

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
    (window, session)
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
    assert_eq!(
        window.summary().boundary_status(BoundaryEdge::Older),
        BoundaryStatus::MoreAvailable
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
    let (mut window, mut session) = attach_tail(&history, 1);
    let mut session_units = units_for(&session, window.summary().mode);
    let anchor = session_units
        .entries()
        .last()
        .map(|entry| entry.key.clone())
        .expect("units");
    let context = EvictionContext {
        viewport_top: row_of(&session_units, &anchor),
        viewport_height: 20,
        anchor: Some(&anchor),
        pinned: &[],
        deferred: false,
    };
    let outcome = window
        .evict(&context, &session_units, &height, &mut session)
        .expect("eviction");
    assert!(outcome.unaligned);
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
fn sd5_live_stream_changes_are_derived_by_key() {
    let mut history = History::new(4);
    let (mut window, mut session) = attach_tail(&history, 1);
    let event = history.push();
    let LiveOutcome::Applied(changes) =
        window.apply_live(LiveInput::Durable(Box::new(event)), &mut session)
    else {
        panic!("live window applies live events");
    };
    assert_eq!(inserted_count(&changes), 1);
    // Nothing changed since: the next diff is empty.
    assert!(window.sync_document(&session).is_empty());
}
