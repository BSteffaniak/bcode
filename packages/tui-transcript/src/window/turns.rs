//! Turn alignment of canonical event ranges (W1, W4).
//!
//! A *safe window start* is an event at which a resident window may begin: the start of a turn
//! (a user message). Page and window results are trimmed to safe boundaries when one exists
//! inside the result, so window edges are whole turns and later pages never complete an edge
//! item in place (BUG-B5). A result with no safe boundary inside (one turn larger than the
//! caps, or a session without turn starts) is accepted unaligned (W4, SD2 fallback).

use bcode_session_models::SessionEvent;

use super::{PAGE_EVENTS, WindowRequest};

/// Whether a window may begin at this event (a turn start). The same definition the
/// `SessionView` window operations align prepends with (SD2).
pub use bcode_session_view::window_ops::is_safe_window_start;

/// An aligned sub-range of a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Aligned {
    /// First kept event index.
    pub start: usize,
    /// End of kept events (exclusive).
    pub end: usize,
    /// Whether events before the kept range exist (in the result or beyond it).
    pub has_older: bool,
    /// Whether events after the kept range exist (in the result or beyond it).
    pub has_newer: bool,
}

/// Trim an `Older` page so it begins at a turn start.
///
/// Leading events before the first safe start are dropped when more history exists before the
/// page (they arrive complete with the next page). Without a safe start, or when the page
/// reaches the session start, the page is kept whole.
#[must_use]
pub fn align_older_page(events: &[SessionEvent], has_more: bool) -> Aligned {
    let start = if has_more {
        events
            .iter()
            .position(is_safe_window_start)
            .unwrap_or_default()
    } else {
        0
    };
    Aligned {
        start,
        end: events.len(),
        has_older: has_more || start > 0,
        has_newer: false,
    }
}

/// Trim a `Newer` page so it ends before an incomplete trailing turn.
///
/// Trailing events from the last safe start on are dropped when more history exists after the
/// page and that start is not the page's first event. Otherwise the page is kept whole.
#[must_use]
pub fn align_newer_page(events: &[SessionEvent], has_more: bool) -> Aligned {
    let end = if has_more {
        match events.iter().rposition(is_safe_window_start) {
            Some(last) if last > 0 => last,
            _ => events.len(),
        }
    } else {
        events.len()
    };
    Aligned {
        start: 0,
        end,
        has_older: false,
        has_newer: has_more || end < events.len(),
    }
}

/// Trim a replacement window to whole turns on both sides (W4, W8, W10).
#[must_use]
pub fn align_window(events: &[SessionEvent], has_older: bool, has_newer: bool) -> Aligned {
    let older = align_older_page(events, has_older);
    let tail = &events[older.start..];
    let newer = align_newer_page(tail, has_newer);
    Aligned {
        start: older.start,
        end: older.start + newer.end,
        has_older: older.has_older,
        has_newer: newer.has_newer,
    }
}

/// Bound a window result to one window request (W8, W10) and trim it to whole turns (W4).
///
/// A result larger than one window's event cap ([`PAGE_EVENTS`]) is first cut to the cap: a
/// `Head` window keeps its first events, a `Tail` (and `LiveState`) window its last events, and
/// an `Around` window the events centered on its target sequence (W8: the first newer or older
/// page after a timeline jump never triggers eviction, BUG-B1). The cut sides report more
/// history. The bounded range is then aligned to turn starts on both sides; an `Around` window
/// never trims the turn containing its target, so the reader can land on it (T18) — when that
/// turn has no start inside the result, the edge is kept unaligned (W4, SD2 fallback).
#[must_use]
pub fn align_window_result(
    events: &[SessionEvent],
    request: WindowRequest,
    has_older: bool,
    has_newer: bool,
) -> Aligned {
    let cap = usize::try_from(PAGE_EVENTS).unwrap_or(usize::MAX);
    let len = events.len();
    let (low, high) = if len <= cap {
        (0, len)
    } else {
        match request {
            WindowRequest::Head => (0, cap),
            WindowRequest::Tail | WindowRequest::LiveState => (len - cap, len),
            WindowRequest::Around { sequence } => {
                let target = target_index(events, sequence);
                let high = target.saturating_sub(cap / 2).saturating_add(cap).min(len);
                (high - cap, high)
            }
        }
    };
    let bounded = &events[low..high];
    let has_older = has_older || low > 0;
    let has_newer = has_newer || high < len;
    let aligned = match request {
        WindowRequest::Around { sequence } if !bounded.is_empty() => align_around(
            bounded,
            target_index(bounded, sequence),
            has_older,
            has_newer,
        ),
        _ => align_window(bounded, has_older, has_newer),
    };
    Aligned {
        start: low + aligned.start,
        end: low + aligned.end,
        ..aligned
    }
}

/// Index of the event holding `sequence`, or of the nearest event after it (the last event
/// when the sequence lies beyond the result). `events` must not be empty.
fn target_index(events: &[SessionEvent], sequence: u64) -> usize {
    events
        .partition_point(|event| event.sequence < sequence)
        .min(events.len().saturating_sub(1))
}

/// Trim to whole turns on both sides without dropping the turn of `events[target]`.
fn align_around(
    events: &[SessionEvent],
    target: usize,
    has_older: bool,
    has_newer: bool,
) -> Aligned {
    let start = if has_older {
        events[..=target]
            .iter()
            .position(is_safe_window_start)
            .unwrap_or_default()
    } else {
        0
    };
    let end = if has_newer {
        events[target + 1..]
            .iter()
            .rposition(is_safe_window_start)
            .map_or(events.len(), |offset| target + 1 + offset)
    } else {
        events.len()
    };
    Aligned {
        start,
        end,
        has_older: has_older || start > 0,
        has_newer: has_newer || end < events.len(),
    }
}

#[cfg(test)]
mod tests {
    use bcode_session_models::{ClientId, SessionEventKind, SessionId, TurnAdmissionMetadata};

    use super::*;

    fn event(sequence: u64, user: bool) -> SessionEvent {
        SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id: SessionId::new(),
            provenance: None,
            kind: if user {
                SessionEventKind::UserMessage {
                    client_id: ClientId::new(),
                    text: format!("user {sequence}"),
                    admission: TurnAdmissionMetadata::default(),
                }
            } else {
                SessionEventKind::AssistantMessage {
                    text: format!("assistant {sequence}"),
                }
            },
        }
    }

    /// Events 1..=n where every `turn`-th event (starting at the first) is a user message.
    fn session(n: u64, turn: u64) -> Vec<SessionEvent> {
        (1..=n).map(|s| event(s, (s - 1) % turn == 0)).collect()
    }

    #[test]
    fn w4_older_page_starts_at_a_turn_start() {
        // Events 3..=8 with turns starting at 1, 4, 7: drop 3.
        let events = session(8, 3).split_off(2);
        let aligned = align_older_page(&events, true);
        assert_eq!(events[aligned.start].sequence, 4);
        assert!(aligned.has_older);
        // At the session start the page is kept whole.
        assert_eq!(align_older_page(&events, false).start, 0);
    }

    #[test]
    fn w4_newer_page_ends_before_an_incomplete_turn() {
        let events = session(8, 3);
        let aligned = align_newer_page(&events, true);
        assert_eq!(aligned.end, 6, "turn starting at 7 is incomplete");
        assert!(aligned.has_newer);
        assert_eq!(align_newer_page(&events, false).end, 8);
    }

    #[test]
    fn w4_a_turn_larger_than_the_page_is_accepted_unaligned() {
        let events = (10..20).map(|s| event(s, false)).collect::<Vec<_>>();
        assert_eq!(align_older_page(&events, true).start, 0);
        assert_eq!(align_newer_page(&events, true).end, events.len());
    }

    #[test]
    fn w8_window_is_trimmed_to_whole_turns_on_both_sides() {
        let events = session(12, 3).split_off(1); // 2..=12, turns at 4, 7, 10
        let aligned = align_window(&events, true, true);
        let kept = &events[aligned.start..aligned.end];
        assert_eq!(kept.first().map(|e| e.sequence), Some(4));
        assert_eq!(kept.last().map(|e| e.sequence), Some(9));
        assert!(aligned.has_older && aligned.has_newer);
    }

    #[test]
    fn w8_oversized_around_window_is_bounded_and_centered_on_the_target() {
        // 1,024 events (a V1-sized timeline window), turns of 4 events starting at 1, 5, ….
        let events = session(1_024, 4);
        let request = WindowRequest::Around { sequence: 600 };
        let aligned = align_window_result(&events, request, true, true);
        let kept = &events[aligned.start..aligned.end];
        assert!(kept.len() <= 512, "W8: at most one window of events");
        assert!(
            kept.iter().any(|event| event.sequence == 600),
            "target resident"
        );
        assert!(
            is_safe_window_start(&kept[0]),
            "leading edge is a turn start"
        );
        let next = events[aligned.end].sequence;
        assert!(
            is_safe_window_start(&events[aligned.end]),
            "trailing edge ends before the turn starting at {next}"
        );
        assert!(aligned.has_older && aligned.has_newer);
    }

    #[test]
    fn w10_oversized_head_and_tail_windows_keep_their_session_edge() {
        let events = session(1_024, 4);
        let head = align_window_result(&events, WindowRequest::Head, false, false);
        assert_eq!(head.start, 0);
        assert!(head.end <= 512 && head.has_newer && !head.has_older);
        let tail = align_window_result(&events, WindowRequest::Tail, false, false);
        assert_eq!(tail.end, events.len());
        assert!(events.len() - tail.start <= 512 && tail.has_older && !tail.has_newer);
        assert!(is_safe_window_start(&events[tail.start]));
    }

    #[test]
    fn w4_around_window_never_trims_the_target_turn() {
        // One turn of 40 events starting at 1, then turns of 3: target 30 lies in the first turn
        // whose start is inside the result; target 2 in a turn starting before the result.
        let mut events = (1..=40).map(|s| event(s, s == 1)).collect::<Vec<_>>();
        events.extend((41..=60).map(|s| event(s, (s - 41) % 3 == 0)));
        let around = |events: &[SessionEvent], sequence| {
            let aligned =
                align_window_result(events, WindowRequest::Around { sequence }, true, true);
            events[aligned.start..aligned.end]
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>()
        };
        let kept = around(&events, 30);
        assert_eq!(
            kept.first(),
            Some(&1),
            "the target's turn starts the window"
        );
        assert!(kept.contains(&30));
        // The result starts mid-turn: plain alignment would drop the target's turn.
        let tail = events.split_off(1);
        let kept = around(&tail, 2);
        assert!(kept.contains(&2), "unaligned leading edge keeps the target");
        // The target sits in the last, incomplete turn: it is kept whole.
        let kept = around(&tail, 59);
        assert!(kept.contains(&59) && kept.contains(&60));
    }
}
