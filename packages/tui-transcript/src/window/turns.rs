//! Turn alignment of canonical event ranges (W1, W4).
//!
//! A *safe window start* is an event at which a resident window may begin: the start of a turn
//! (a user message). Page and window results are trimmed to safe boundaries when one exists
//! inside the result, so window edges are whole turns and later pages never complete an edge
//! item in place (BUG-B5). A result with no safe boundary inside (one turn larger than the
//! caps, or a session without turn starts) is accepted unaligned (W4, SD2 fallback).

use bcode_session_models::{SessionEvent, SessionEventKind};

/// Whether a window may begin at this event (a turn start).
#[must_use]
pub const fn is_safe_window_start(event: &SessionEvent) -> bool {
    matches!(event.kind, SessionEventKind::UserMessage { .. })
}

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

#[cfg(test)]
mod tests {
    use bcode_session_models::{ClientId, SessionId, TurnAdmissionMetadata};

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
}
