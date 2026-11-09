//! Window operation tests (SD1–SD7): behaviour, bounded work and differential equality with a
//! from-scratch rebuild of the same event range.

use bcode_session_models::{
    ClientId, ModelTurnOutcome, SessionEvent, SessionEventKind, SessionId, SessionLiveEvent,
    SessionLiveEventKind, TurnAdmissionMetadata,
};
use bcode_session_view_models::{
    TranscriptChange, TranscriptChanges, TranscriptViewDocument, TranscriptViewItem,
    TranscriptViewItemId, TranscriptViewPatchOp,
};

use super::{MAX_PAGE_EVENTS, WindowOpError, is_safe_window_start};
use crate::SessionView;

/// Deterministic generator (64-bit LCG); tests need no randomness dependency.
struct Lcg(u64);

impl Lcg {
    const fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    const fn below(&mut self, bound: u64) -> u64 {
        let bound = if bound == 0 { 1 } else { bound };
        self.next() % bound
    }

    fn below_usize(&mut self, bound: u64) -> usize {
        usize::try_from(self.below(bound)).expect("small bound fits usize")
    }
}

/// A canonical session of whole turns with turn-local references only.
struct History {
    id: SessionId,
    events: Vec<SessionEvent>,
}

impl History {
    fn new(turns: u64, seed: u64) -> Self {
        let mut history = Self {
            id: SessionId::new(),
            events: Vec::new(),
        };
        let mut rng = Lcg(seed);
        for turn in 0..turns {
            history.turn(turn, &mut rng);
        }
        history
    }

    fn push(&mut self, kind: SessionEventKind) {
        let sequence = self.last() + 1;
        self.events.push(SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence * 10,
            session_id: self.id,
            provenance: None,
            kind,
        });
    }

    fn user(&mut self, text: &str) {
        self.push(SessionEventKind::UserMessage {
            client_id: ClientId::new(),
            text: text.to_owned(),
            admission: TurnAdmissionMetadata::default(),
        });
    }

    fn tool(&mut self, id: &str) {
        self.push(SessionEventKind::ToolCallRequested {
            tool_call_id: id.to_owned(),
            producer_plugin_id: None,
            tool_name: "read".to_owned(),
            arguments_json: "{}".to_owned(),
            working_directory: None,
        });
    }

    fn turn(&mut self, turn: u64, rng: &mut Lcg) {
        let turn_id = format!("turn-{turn}");
        self.user(&format!("question {turn}"));
        self.push(SessionEventKind::ModelTurnStarted {
            turn_id: turn_id.clone(),
        });
        if rng.below(2) == 0 {
            self.push(SessionEventKind::AssistantReasoningMessage {
                text: format!("thinking {turn}"),
            });
        }
        for call in 0..rng.below(3) {
            self.tool(&format!("call-{turn}-{call}"));
        }
        if rng.below(4) == 0 {
            self.push(SessionEventKind::SystemMessage {
                text: format!("note {turn}"),
            });
        }
        self.push(SessionEventKind::AssistantMessage {
            text: format!("answer {turn}"),
        });
        let outcome = if rng.below(5) == 0 {
            ModelTurnOutcome::Error
        } else {
            ModelTurnOutcome::Completed
        };
        self.push(SessionEventKind::ModelTurnFinished {
            turn_id,
            outcome,
            message: None,
        });
    }

    /// A session mixing plain turns with tool-heavy turns: tool calls with lifecycle, durable
    /// contributions, results and resolved interactions (exchanges).
    fn rich(turns: u64, seed: u64) -> Self {
        let mut history = Self {
            id: SessionId::new(),
            events: Vec::new(),
        };
        let mut rng = Lcg(seed);
        for turn in 0..turns {
            if rng.below(3) == 0 {
                history.turn(turn, &mut rng);
            } else {
                let calls = 1 + rng.below(3);
                history.tool_turn(turn, calls);
            }
        }
        history
    }

    fn lifecycle(&mut self, id: &str, stage: bcode_session_models::ToolInvocationLifecycleStage) {
        self.push(SessionEventKind::ToolInvocationLifecycle {
            event: bcode_session_models::ToolInvocationLifecycleEvent {
                invocation_id: id.to_owned(),
                sequence: self.last() + 1,
                stage,
                message: None,
                metadata: serde_json::Value::Null,
            },
        });
    }

    /// A requested and resolved tool exchange (a permission or question interaction).
    fn exchange(&mut self, id: &str, exchange: &str) {
        self.push(SessionEventKind::ToolExchangeRequested {
            request: bcode_session_models::ToolExchangeRequest {
                invocation_id: id.to_owned(),
                exchange_id: exchange.to_owned(),
                producer_id: "test.producer".to_owned(),
                schema: "test.question/schema".to_owned(),
                schema_version: 1,
                payload: serde_json::json!({"question": exchange}),
                response_policy: bcode_session_models::ToolExchangeResponsePolicy::Required,
            },
        });
        self.push(SessionEventKind::ToolExchangeResolved {
            event: bcode_session_models::ToolExchangeResolutionEvent {
                invocation_id: id.to_owned(),
                exchange_id: exchange.to_owned(),
                resolution: bcode_session_models::ToolExchangeResolution::Responded {
                    payload: serde_json::json!({"answer": exchange}),
                },
            },
        });
    }

    fn tool_turn(&mut self, turn: u64, calls: u64) {
        use bcode_session_models::ToolInvocationLifecycleStage;
        let turn_id = format!("turn-{turn}");
        self.user(&format!("question {turn}"));
        self.push(SessionEventKind::ModelTurnStarted {
            turn_id: turn_id.clone(),
        });
        for call in 0..calls {
            let id = format!("call-{turn}-{call}");
            self.tool(&id);
            self.lifecycle(&id, ToolInvocationLifecycleStage::Started);
            if call == 0 {
                self.exchange(&id, &format!("exchange-{turn}"));
            }
            self.push(SessionEventKind::ToolContribution {
                event: bcode_session_models::ToolContributionEvent {
                    invocation_id: id.clone(),
                    contribution_id: "surface".to_owned(),
                    sequence: 1,
                    producer_id: "test.producer".to_owned(),
                    schema: "test.surface/schema".to_owned(),
                    schema_version: 1,
                    operation: bcode_session_models::ToolContributionOperation::Upsert,
                    persistence: bcode_session_models::ToolContributionPersistence::Durable,
                    artifact: None,
                    payload: serde_json::json!({"call": id}),
                },
            });
            self.push(SessionEventKind::ToolInvocationResultRecorded {
                record: bcode_session_models::ToolInvocationResultRecord {
                    invocation_id: id.clone(),
                    model_output: format!("output {id}"),
                    is_error: false,
                    presentation: None,
                    result: None,
                    content: Vec::new(),
                },
            });
            self.lifecycle(&id, ToolInvocationLifecycleStage::Completed);
        }
        self.push(SessionEventKind::AssistantMessage {
            text: format!("answer {turn}"),
        });
        self.push(SessionEventKind::ModelTurnFinished {
            turn_id,
            outcome: ModelTurnOutcome::Completed,
            message: None,
        });
    }

    /// Sequences of every turn start, ascending.
    fn turn_starts(&self) -> Vec<u64> {
        self.events
            .iter()
            .filter(|event| is_safe_window_start(event))
            .map(|event| event.sequence)
            .collect()
    }

    fn last(&self) -> u64 {
        self.events.len() as u64
    }

    /// Events with sequences in `first..=last`.
    fn range(&self, first: u64, last: u64) -> Vec<SessionEvent> {
        self.events
            .iter()
            .filter(|event| (first..=last).contains(&event.sequence))
            .cloned()
            .collect()
    }

    fn event(&self, sequence: u64) -> &SessionEvent {
        &self.events[usize::try_from(sequence - 1).expect("fixture index fits usize")]
    }

    fn live_delta(&self, turn: &str, text: &str) -> SessionLiveEvent {
        SessionLiveEvent {
            session_id: self.id,
            kind: SessionLiveEventKind::AssistantTextDelta {
                turn_id: turn.to_owned(),
                segment_id: "live".to_owned(),
                segment_order: 0,
                text: text.to_owned(),
            },
        }
    }

    /// Sequence range `(first, last)` of the whole turns with indices `first_turn..end_turn`.
    fn turns(&self, starts: &[u64], first_turn: usize, end_turn: usize) -> (u64, u64) {
        let first = starts[first_turn];
        let last = starts
            .get(end_turn)
            .map_or_else(|| self.last(), |next| next - 1);
        (first, last)
    }
}

const LIVE_ITEM: &str = "assistant-turn:turn-live:segment:live";

fn rebuilt(events: &[SessionEvent]) -> SessionView {
    let mut view = SessionView::new();
    let _ = view.replace_history_window_detached(events);
    view
}

fn items(view: &SessionView) -> &[TranscriptViewItem] {
    &view.snapshot().transcript.items
}

fn ids(view: &SessionView) -> Vec<TranscriptViewItemId> {
    items(view).iter().map(|item| item.id.clone()).collect()
}

fn inserted(changes: &TranscriptChanges) -> Vec<TranscriptViewItemId> {
    changes
        .changes
        .iter()
        .filter_map(|change| match change {
            TranscriptChange::Inserted { ids, .. } => Some(ids.clone()),
            _ => None,
        })
        .flatten()
        .collect()
}

fn removed(changes: &TranscriptChanges) -> Vec<TranscriptViewItemId> {
    changes
        .changes
        .iter()
        .filter_map(|change| match change {
            TranscriptChange::Removed { ids } => Some(ids.clone()),
            _ => None,
        })
        .flatten()
        .collect()
}

/// SD7: the patch derived from `changes` turns `previous` into the view's document.
fn assert_patch_round_trip(
    view: &SessionView,
    previous: &TranscriptViewDocument,
    changes: &TranscriptChanges,
) {
    let patch = view.transcript_patch(previous, changes);
    let mut document = previous.clone();
    document
        .apply_patch(&patch)
        .expect("derived patch applies to the previous document");
    let current = &view.snapshot().transcript;
    assert_eq!(document.items, current.items, "patch yields the new items");
    assert_eq!(document.revision, current.revision);
}

#[test]
fn sd2_prepend_equals_a_rebuild_of_the_combined_range() {
    for seed in 1..=8 {
        let history = History::new(40, seed);
        let starts = history.turn_starts();
        let mut rng = Lcg(seed * 31);
        let mut first_turn = 30;
        let (first, last) = history.turns(&starts, first_turn, starts.len());
        let mut view = rebuilt(&history.range(first, last));
        while first_turn > 0 {
            let take = 1 + rng.below_usize(5);
            let page_first_turn = first_turn.saturating_sub(take);
            let (page_first, page_last) = history.turns(&starts, page_first_turn, first_turn);
            let page = history.range(page_first, page_last);
            let before = view.window_op_counters();
            let previous = view.snapshot().transcript.clone();
            let changes = view.prepend_history_page(&page).expect("aligned page");
            let after = view.window_op_counters();
            assert_eq!(
                after.events_reduced - before.events_reduced,
                page.len() as u64,
                "only the page is reduced; no resident event is replayed"
            );
            assert_eq!(after.replacements, before.replacements, "no rebuild");
            first_turn = page_first_turn;
            let expected = rebuilt(&history.range(page_first, last));
            assert_eq!(items(&view), items(&expected), "seed {seed}");
            assert_eq!(view.window_first_sequence(), Some(page_first));
            assert_eq!(view.snapshot().latest_sequence, Some(last));
            assert_patch_round_trip(&view, &previous, &changes);
            let page_ids = inserted(&changes);
            assert_eq!(page_ids, ids(&view)[..page_ids.len()].to_vec());
        }
    }
}

#[test]
fn append_equals_a_rebuild_without_replaying_the_window() {
    for seed in 1..=8 {
        let history = History::new(30, seed);
        let starts = history.turn_starts();
        let mut rng = Lcg(seed * 17);
        let mut end_turn = 5;
        let (first, last) = history.turns(&starts, 0, end_turn);
        let mut view = rebuilt(&history.range(first, last));
        while end_turn < starts.len() {
            let next_end = (end_turn + 1 + rng.below_usize(6)).min(starts.len());
            let (page_first, page_last) = history.turns(&starts, end_turn, next_end);
            let page = history.range(page_first, page_last);
            let before = view.window_op_counters();
            let previous = view.snapshot().transcript.clone();
            let changes = view.append_history_page(&page).expect("newer page");
            let after = view.window_op_counters();
            assert_eq!(
                after.events_reduced - before.events_reduced,
                page.len() as u64
            );
            end_turn = next_end;
            let expected = rebuilt(&history.range(first, page_last));
            assert_eq!(items(&view), items(&expected), "seed {seed}");
            assert_patch_round_trip(&view, &previous, &changes);
            assert!(
                view.transcript_patch(&previous, &changes)
                    .transcript
                    .iter()
                    .all(|op| !matches!(op, TranscriptViewPatchOp::Reset { .. })),
                "tail insertion derives Append operations"
            );
        }
    }
}

/// Random page/eviction sequences: after every operation the projection equals a from-scratch
/// rebuild of the resident event range, and each derived patch round-trips (SD7).
#[test]
fn differential_random_page_and_eviction_sequences() {
    for seed in 1..=12 {
        differential_run(&History::new(60, seed), seed);
    }
}

/// The same differential over tool-heavy sessions: tool lifecycles, durable contributions,
/// results and resolved interactions, evicted and paged back in repeatedly.
#[test]
fn differential_random_paging_of_tool_heavy_sessions() {
    for seed in 1..=12 {
        differential_run(&History::rich(60, seed), seed);
    }
}

fn differential_run(history: &History, seed: u64) {
    {
        let starts = history.turn_starts();
        let turns = starts.len();
        let mut rng = Lcg(seed * 101 + 7);
        let mut first_turn = 25;
        let mut end_turn = 32;
        let (first, last) = history.turns(&starts, first_turn, end_turn);
        let mut view = rebuilt(&history.range(first, last));
        let replacements = view.window_op_counters().replacements;
        for _ in 0..40 {
            let previous = view.snapshot().transcript.clone();
            let changes = match rng.below(4) {
                0 if first_turn > 0 => {
                    let page_first = first_turn.saturating_sub(1 + rng.below_usize(4));
                    let (a, b) = history.turns(&starts, page_first, first_turn);
                    first_turn = page_first;
                    view.prepend_history_page(&history.range(a, b))
                        .expect("prepend")
                }
                1 if end_turn < turns => {
                    let next_end = (end_turn + 1 + rng.below_usize(4)).min(turns);
                    let (a, b) = history.turns(&starts, end_turn, next_end);
                    end_turn = next_end;
                    view.append_history_page(&history.range(a, b))
                        .expect("append")
                }
                2 if end_turn - first_turn > 2 => {
                    first_turn += 1 + rng.below_usize(2);
                    view.evict_before_sequence(starts[first_turn])
                }
                3 if end_turn - first_turn > 2 => {
                    end_turn -= 1 + rng.below_usize(2);
                    view.evict_from_sequence(starts[end_turn])
                }
                _ => continue,
            };
            let (a, b) = history.turns(&starts, first_turn, end_turn);
            let expected = rebuilt(&history.range(a, b));
            assert_eq!(items(&view), items(&expected), "seed {seed}");
            assert_eq!(view.window_first_sequence(), Some(a));
            assert_eq!(view.snapshot().latest_sequence, Some(b));
            assert_eq!(
                view.snapshot().tools.keys().collect::<Vec<_>>(),
                expected.snapshot().tools.keys().collect::<Vec<_>>(),
                "evicted tool aggregates are removed"
            );
            let interactions = |view: &SessionView| {
                view.snapshot()
                    .interactions
                    .iter()
                    .map(|interaction| interaction.interaction_id.clone())
                    .collect::<std::collections::BTreeSet<_>>()
            };
            assert_eq!(
                interactions(&view),
                interactions(&expected),
                "interaction state stays bounded by the resident window"
            );
            assert_patch_round_trip(&view, &previous, &changes);
            assert_eq!(
                view.window_op_counters().replacements,
                replacements,
                "paging and eviction never rebuild"
            );
        }
    }
}

#[test]
fn sd1_overlapping_pages_are_rejected_and_never_reapplied() {
    let history = History::new(10, 3);
    let starts = history.turn_starts();
    let mut view = rebuilt(&history.range(starts[4], history.last()));
    let before = ids(&view);
    let counters = view.window_op_counters();
    let overlapping = history.range(starts[2], starts[4]);
    assert_eq!(
        view.prepend_history_page(&overlapping),
        Err(WindowOpError::Overlap)
    );
    let resident = history.range(history.last() - 2, history.last());
    assert_eq!(
        view.append_history_page(&resident),
        Err(WindowOpError::Overlap)
    );
    assert_eq!(ids(&view), before, "rejected pages change nothing");
    assert_eq!(view.window_op_counters(), counters);
    // A durable event already resident is ignored when delivered live.
    view.apply_window_event(history.event(history.last()));
    assert_eq!(view.window_op_counters(), counters);
    assert!(view.drain_transcript_changes().is_empty());
}

#[test]
fn sd2_unaligned_window_start_falls_back_to_an_identical_replacement() {
    let history = History::new(10, 5);
    let starts = history.turn_starts();
    // The window starts one event into a turn.
    let window_first = starts[5] + 1;
    let mut view = rebuilt(&history.range(window_first, history.last()));
    let page = history.range(starts[3], window_first - 1);
    assert_eq!(
        view.prepend_history_page(&page),
        Err(WindowOpError::UnalignedBoundary)
    );
    let surviving = ids(&view);
    let changes = view.replace_history_window_detached(&history.range(starts[3], history.last()));
    assert!(
        surviving.iter().all(|id| ids(&view).contains(id)),
        "deterministic ids keep identity anchors across the fallback (P1)"
    );
    assert!(
        removed(&changes).iter().all(|id| !surviving.contains(id)),
        "unchanged items are not reported removed"
    );
    assert_eq!(view.window_op_counters().replacements, 2);
}

#[test]
fn sd2_shared_keyed_state_across_the_boundary_is_not_spliced() {
    let mut history = History::new(0, 1);
    history.user("first");
    history.tool("shared");
    let second = history.last() + 1;
    // The newer turn touches the older turn's tool invocation.
    history.user("again");
    history.tool("shared");
    let mut view = rebuilt(&history.range(second, history.last()));
    let before = ids(&view);
    assert_eq!(
        view.prepend_history_page(&history.range(1, second - 1)),
        Err(WindowOpError::UnalignedBoundary)
    );
    assert_eq!(ids(&view), before);
}

#[test]
fn malformed_and_foreign_pages_are_rejected() {
    let history = History::new(6, 2);
    let starts = history.turn_starts();
    let mut view = rebuilt(&history.range(starts[3], history.last()));
    let mut reversed = history.range(starts[1], starts[3] - 1);
    reversed.reverse();
    assert_eq!(
        view.prepend_history_page(&reversed),
        Err(WindowOpError::Bounds)
    );
    let mut foreign = history.range(starts[1], starts[3] - 1);
    for event in &mut foreign {
        event.session_id = SessionId::new();
    }
    assert_eq!(
        view.prepend_history_page(&foreign),
        Err(WindowOpError::SessionMismatch)
    );
    let oversized = (1..=MAX_PAGE_EVENTS as u64 + 1)
        .map(|sequence| {
            let mut event = history.event(2).clone();
            event.sequence = history.last() + sequence;
            event
        })
        .collect::<Vec<_>>();
    assert_eq!(
        view.append_history_page(&oversized),
        Err(WindowOpError::Bounds)
    );
}

#[test]
fn sd3_head_eviction_keeps_live_only_items_and_removes_owned_aggregates() {
    let history = History::new(8, 4);
    let starts = history.turn_starts();
    let mut view = SessionView::new();
    let _ = view.replace_history_window(&history.range(1, history.last()));
    view.apply_live_event(&history.live_delta("turn-live", "streaming text"));
    let live = TranscriptViewItemId::new(LIVE_ITEM);
    let _ = view.drain_transcript_changes();
    assert!(ids(&view).contains(&live));
    let evicted_tools = view
        .snapshot()
        .tools
        .keys()
        .filter(|key| {
            let turn = key
                .trim_start_matches("call-")
                .split('-')
                .next()
                .and_then(|turn| turn.parse::<usize>().ok())
                .expect("fixture tool id");
            turn < 4
        })
        .cloned()
        .collect::<Vec<_>>();
    let changes = view.evict_before_sequence(starts[4]);
    assert!(
        ids(&view).contains(&live),
        "live-only items stay on head eviction"
    );
    assert!(!removed(&changes).contains(&live));
    assert!(
        items(&view)
            .iter()
            .filter_map(|item| item.sequence)
            .all(|sequence| sequence >= starts[4])
    );
    assert!(
        evicted_tools
            .iter()
            .all(|tool| !view.snapshot().tools.contains_key(tool)),
        "aggregates owned only by evicted items are removed"
    );
}

#[test]
fn sd3_tail_eviction_discards_live_only_items_and_reopens_the_tail() {
    let history = History::new(8, 6);
    let starts = history.turn_starts();
    let mut view = SessionView::new();
    let _ = view.replace_history_window(&history.range(1, history.last()));
    view.apply_live_event(&history.live_delta("turn-live", "streaming text"));
    let live = TranscriptViewItemId::new(LIVE_ITEM);
    let changes = view.evict_from_sequence(starts[5]);
    assert!(
        removed(&changes).contains(&live),
        "W9 discards live-only items"
    );
    assert_eq!(view.snapshot().latest_sequence, Some(starts[5] - 1));
    // The evicted turns can be appended again (detached paging back down).
    let page = history.range(starts[5], history.last());
    let _ = view.append_history_page(&page).expect("re-append");
    assert_eq!(
        items(&view),
        items(&rebuilt(&history.range(1, history.last())))
    );
}

#[test]
fn unaligned_tail_eviction_requires_a_replacement_for_the_next_append() {
    let history = History::new(8, 6);
    let starts = history.turn_starts();
    let mut view = rebuilt(&history.range(1, history.last()));
    let _ = view.evict_from_sequence(starts[5] + 1);
    assert_eq!(
        view.append_history_page(&history.range(starts[5] + 1, history.last())),
        Err(WindowOpError::UnalignedBoundary)
    );
}

#[test]
fn evict_by_item_id_removes_whole_turns() {
    let history = History::new(8, 8);
    let starts = history.turn_starts();
    let mut view = rebuilt(&history.range(1, history.last()));
    let mid_turn = items(&view)
        .iter()
        .find(|item| {
            item.sequence
                .is_some_and(|sequence| sequence > starts[3] && sequence < starts[4])
        })
        .map(|item| item.id.clone())
        .expect("an item inside turn 3");
    let _ = view.evict_before(&mid_turn);
    assert_eq!(
        items(&view),
        items(&rebuilt(&history.range(starts[3], history.last())))
    );
    let last_kept = TranscriptViewItemId::event(starts[5]);
    let _ = view.evict_after(&last_kept);
    assert_eq!(
        items(&view),
        items(&rebuilt(&history.range(starts[3], starts[6] - 1)))
    );
}

#[test]
fn drain_reports_accumulated_live_changes_once() {
    let history = History::new(6, 10);
    let mut view = SessionView::new();
    let _ = view.replace_history_window(&history.range(1, 10));
    let previous = view.snapshot().transcript.clone();
    for event in history.range(11, history.last()) {
        view.apply_window_event(&event);
    }
    view.apply_live_event(&history.live_delta("turn-live", "a"));
    let changes = view.drain_transcript_changes();
    assert_eq!(changes.base_revision, previous.revision);
    assert_eq!(changes.revision, view.snapshot().transcript.revision);
    let new_ids = ids(&view)
        .into_iter()
        .filter(|id| !previous.items.iter().any(|item| item.id == *id))
        .collect::<Vec<_>>();
    assert_eq!(inserted(&changes), new_ids);
    assert_patch_round_trip(&view, &previous, &changes);
    assert!(view.drain_transcript_changes().is_empty(), "drained once");

    let previous = view.snapshot().transcript.clone();
    view.apply_live_event(&history.live_delta("turn-live", "b"));
    let changes = view.drain_transcript_changes();
    let revision = items(&view)
        .iter()
        .find(|item| item.id.get() == LIVE_ITEM)
        .map(|item| item.revision)
        .expect("live item");
    assert_eq!(
        changes.changes,
        vec![TranscriptChange::Revised {
            id: TranscriptViewItemId::new(LIVE_ITEM),
            revision,
        }]
    );
    assert_patch_round_trip(&view, &previous, &changes);
}

#[test]
fn undrained_changes_are_reported_by_the_next_operation() {
    let history = History::new(10, 12);
    let starts = history.turn_starts();
    let mut view = rebuilt(&history.range(starts[5], starts[8] - 1));
    let previous = view.snapshot().transcript.clone();
    view.apply_window_event(history.event(starts[8]));
    let changes = view
        .prepend_history_page(&history.range(starts[3], starts[5] - 1))
        .expect("prepend");
    assert_eq!(changes.base_revision, previous.revision);
    assert!(inserted(&changes).contains(&TranscriptViewItemId::event(starts[8])));
    assert_patch_round_trip(&view, &previous, &changes);
}

#[test]
fn replacement_reports_content_changes_at_an_unchanged_revision() {
    let history = History::new(6, 14);
    let starts = history.turn_starts();
    let mut view = rebuilt(&history.range(starts[2], history.last()));
    let previous = view.snapshot().transcript.clone();
    let mut edited = history.range(starts[2], history.last());
    edited[0].kind = SessionEventKind::UserMessage {
        client_id: ClientId::new(),
        text: "edited".to_owned(),
        admission: TurnAdmissionMetadata::default(),
    };
    let changes = view.replace_history_window_detached(&edited);
    let id = TranscriptViewItemId::event(starts[2]);
    assert!(
        changes
            .changes
            .iter()
            .any(|change| matches!(change, TranscriptChange::Revised { id: revised, .. } if *revised == id)),
        "{changes:?}"
    );
    assert_patch_round_trip(&view, &previous, &changes);
}

#[test]
fn moved_items_are_reported_as_removed_and_reinserted() {
    let mirror = ["a", "b", "c", "d"]
        .map(|id| (TranscriptViewItemId::new(id), 1))
        .to_vec();
    let new = ["a", "c", "b", "d"]
        .map(|id| TranscriptViewItem {
            id: TranscriptViewItemId::new(id),
            revision: 1,
            sequence: None,
            timestamp_ms: None,
            output_location: None,
            streaming: false,
            kind: bcode_session_view_models::TranscriptViewItemKind::AssistantMessage {
                message: bcode_session_view_models::ChatMessageView::markdown(id),
            },
        })
        .to_vec();
    let changes = super::diff_items(&mirror, &new, None);
    assert_eq!(changes.len(), 2, "{changes:?}");
    let TranscriptChange::Removed { ids } = &changes[0] else {
        panic!("removal first: {changes:?}");
    };
    assert_eq!(ids.len(), 1, "one item moved");
    assert!(matches!(&changes[1], TranscriptChange::Inserted { ids: moved, .. } if moved == ids));
}

/// Evicted resolved interactions come back with their turn: paging an evicted turn back in
/// equals a from-scratch rebuild of the range, on both sides.
#[test]
fn sd3_evicted_interactions_return_when_their_turn_is_paged_back() {
    let history = History::rich(0, 1);
    let mut history = history;
    for turn in 0..12 {
        history.tool_turn(turn, 2);
    }
    let starts = history.turn_starts();
    let interaction = TranscriptViewItemId::interaction("exchange-4");
    let mut view = rebuilt(&history.range(1, history.last()));
    assert!(ids(&view).contains(&interaction));

    let _ = view.evict_before_sequence(starts[6]);
    assert!(!ids(&view).contains(&interaction));
    let changes = view
        .prepend_history_page(&history.range(starts[3], starts[6] - 1))
        .expect("isolated older turns");
    assert!(inserted(&changes).contains(&interaction));
    assert_eq!(
        items(&view),
        items(&rebuilt(&history.range(starts[3], history.last())))
    );

    let _ = view.evict_from_sequence(starts[4]);
    assert!(!ids(&view).contains(&interaction));
    let _ = view
        .append_history_page(&history.range(starts[4], history.last()))
        .expect("newer turns");
    assert!(ids(&view).contains(&interaction));
    assert_eq!(
        items(&view),
        items(&rebuilt(&history.range(starts[3], history.last())))
    );
    assert_eq!(view.window_op_counters().replacements, 1, "no fallback");
}

/// A replacement is the rebuild of its own range: interactions and tool invocations of the
/// previous window neither stay on screen nor fence the new window's paging, so paging back
/// into the previously resident range stays incremental (no SD2 fallback).
#[test]
fn replacement_after_a_jump_pages_back_into_the_old_range_incrementally() {
    for keep_live in [false, true] {
        let mut history = History::rich(0, 1);
        for turn in 0..12 {
            history.tool_turn(turn, 2);
        }
        let starts = history.turn_starts();
        let replace = |view: &mut SessionView, events: &[SessionEvent]| {
            if keep_live {
                view.replace_history_window(events)
            } else {
                view.replace_history_window_detached(events)
            }
        };
        let mut view = SessionView::new();
        let _ = replace(&mut view, &history.range(1, starts[4] - 1));
        // The jump: a window of later turns.
        let _ = replace(&mut view, &history.range(starts[8], history.last()));
        assert_eq!(
            items(&view),
            items(&rebuilt(&history.range(starts[8], history.last()))),
            "keep_live {keep_live}: the replacement shows only its own range"
        );
        for turn in (0..8).rev() {
            let page = history.range(starts[turn], starts[turn + 1] - 1);
            let _ = view
                .prepend_history_page(&page)
                .unwrap_or_else(|error| panic!("keep_live {keep_live}, turn {turn}: {error}"));
            assert_eq!(
                items(&view),
                items(&rebuilt(&history.range(starts[turn], history.last()))),
                "keep_live {keep_live}, turn {turn}"
            );
        }
        assert_eq!(view.window_op_counters().replacements, 2);
    }
}

/// Draining is O(1) without a change and updates the ledger in place when only revisions
/// changed (streaming deltas); only membership changes rescan the window.
#[test]
fn drains_rescan_the_window_only_for_membership_changes() {
    let history = History::new(20, 3);
    let mut view = SessionView::new();
    let _ = view.replace_history_window(&history.range(1, history.last()));
    let scans = view.window_op_counters().ledger_scans;
    assert!(view.drain_transcript_changes().is_empty());
    assert_eq!(view.window_op_counters().ledger_scans, scans);

    view.apply_live_event(&history.live_delta("turn-live", "a"));
    assert_eq!(
        inserted(&view.drain_transcript_changes()),
        vec![TranscriptViewItemId::new(LIVE_ITEM)]
    );
    let scans = view.window_op_counters().ledger_scans;
    for delta in 0..50 {
        let previous = view.snapshot().transcript.clone();
        view.apply_live_event(&history.live_delta("turn-live", &format!(" {delta}")));
        let changes = view.drain_transcript_changes();
        assert!(
            matches!(changes.changes.as_slice(), [TranscriptChange::Revised { id, .. }] if id.get() == LIVE_ITEM),
            "{changes:?}"
        );
        assert_patch_round_trip(&view, &previous, &changes);
        assert!(view.drain_transcript_changes().is_empty());
    }
    assert_eq!(
        view.window_op_counters().ledger_scans,
        scans,
        "revisions and empty drains never rescan the window"
    );
}
