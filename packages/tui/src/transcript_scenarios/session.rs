//! Fake canonical session history and the marker text scenarios read back from frames.
//!
//! Every scenario item carries one marker token per source line (`S00042L003` = sequence 42,
//! line 3). Frames are observed by finding these tokens in the committed transcript area, so
//! observations do not depend on any engine's internal row bookkeeping.

use std::collections::BTreeMap;

use bcode_session_models::{SessionEvent, SessionEventKind, SessionId};

use super::driver::{HistoryRequest, HistoryResponse, PageLimits};

/// One source line of one scenario item, as rendered in a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Marker {
    /// Canonical event sequence of the item.
    pub sequence: u64,
    /// Zero-based source line within the item.
    pub line: u32,
}

impl Marker {
    /// Build a marker.
    #[must_use]
    pub const fn new(sequence: u64, line: u32) -> Self {
        Self { sequence, line }
    }

    /// Render the marker token.
    #[must_use]
    pub fn token(self) -> String {
        format!("S{:05}L{:03}", self.sequence, self.line)
    }

    /// Parse one whitespace-delimited token, ignoring surrounding punctuation.
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        let token = token.trim_matches(|character: char| !character.is_ascii_alphanumeric());
        let rest = token.strip_prefix('S')?;
        let (sequence, line) = rest.split_once('L')?;
        if sequence.len() < 5
            || line.len() < 3
            || !sequence.bytes().all(|byte| byte.is_ascii_digit())
            || !line.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        Some(Self {
            sequence: sequence.parse().ok()?,
            line: line.parse().ok()?,
        })
    }
}

/// Canonical history served by the scenario's fake daemon.
#[derive(Debug, Clone)]
pub struct ScenarioSession {
    id: SessionId,
    events: Vec<SessionEvent>,
    /// Marker line count of every item event, keyed by sequence.
    item_lines: BTreeMap<u64, u32>,
}

impl Default for ScenarioSession {
    fn default() -> Self {
        Self::new()
    }
}

impl ScenarioSession {
    /// Empty session with a fresh identity.
    #[must_use]
    pub fn new() -> Self {
        Self {
            id: SessionId::new(),
            events: Vec::new(),
            item_lines: BTreeMap::new(),
        }
    }

    /// Session of `count` assistant items with `lines` marker lines each.
    #[must_use]
    pub fn with_assistant_items(count: usize, lines: u32) -> Self {
        let mut session = Self::new();
        for _ in 0..count {
            session.push_assistant(lines);
        }
        session
    }

    /// Session of `turns` turns, each a one-line user message followed by one assistant item
    /// with `assistant_lines` marker lines. User messages are the safe window starts.
    #[must_use]
    pub fn with_turns(turns: usize, assistant_lines: u32) -> Self {
        let mut session = Self::new();
        for _ in 0..turns {
            session.push_user(1);
            session.push_assistant(assistant_lines);
        }
        session
    }

    /// Session identity.
    #[must_use]
    pub const fn id(&self) -> SessionId {
        self.id
    }

    /// All canonical events in sequence order.
    #[must_use]
    pub fn events(&self) -> &[SessionEvent] {
        &self.events
    }

    /// Sequence the next appended event receives.
    #[must_use]
    pub fn next_sequence(&self) -> u64 {
        self.events
            .last()
            .map_or(1, |event| event.sequence.saturating_add(1))
    }

    /// Last canonical sequence, if any.
    #[must_use]
    pub fn last_sequence(&self) -> Option<u64> {
        self.events.last().map(|event| event.sequence)
    }

    /// Marker line count of the item at `sequence`, if it is a scenario item.
    #[must_use]
    pub fn item_lines(&self, sequence: u64) -> Option<u32> {
        self.item_lines.get(&sequence).copied()
    }

    /// Last marker of the item at `sequence`.
    #[must_use]
    pub fn last_marker(&self, sequence: u64) -> Option<Marker> {
        self.item_lines(sequence)
            .map(|lines| Marker::new(sequence, lines.saturating_sub(1)))
    }

    /// Append one assistant item with `lines` marker lines and return its event.
    pub fn push_assistant(&mut self, lines: u32) -> SessionEvent {
        let sequence = self.next_sequence();
        let text = item_text(sequence, lines);
        self.item_lines.insert(sequence, lines);
        self.push_kind(SessionEventKind::AssistantMessage { text })
    }

    /// Append one user message with `lines` marker lines (plain text, one per line).
    pub fn push_user(&mut self, lines: u32) -> SessionEvent {
        let sequence = self.next_sequence();
        let text = (0..lines.max(1))
            .map(|line| marker_line(sequence, line))
            .collect::<Vec<_>>()
            .join("\n");
        self.item_lines.insert(sequence, lines.max(1));
        self.push_kind(SessionEventKind::UserMessage {
            client_id: bcode_session_models::ClientId::new(),
            text,
            admission: bcode_session_models::TurnAdmissionMetadata::default(),
        })
    }

    /// Append one streaming assistant delta carrying marker lines `lines` of the streaming item
    /// keyed `item` (by convention the sequence of the item's first delta).
    pub fn push_assistant_delta(&mut self, item: u64, lines: std::ops::Range<u32>) -> SessionEvent {
        let continuation = lines.start > 0;
        let mut text = lines
            .clone()
            .map(|line| marker_line(item, line))
            .collect::<Vec<_>>()
            .join("\\\n");
        if continuation {
            text.insert_str(0, "\\\n");
        }
        let total = self
            .item_lines
            .get(&item)
            .copied()
            .unwrap_or(0)
            .max(lines.end);
        self.item_lines.insert(item, total);
        self.push_kind(SessionEventKind::AssistantDelta { text })
    }

    /// Append one canonical event that produces no transcript item (a client attachment).
    pub fn push_silent(&mut self) -> SessionEvent {
        self.push_kind(SessionEventKind::ClientAttached {
            client_id: bcode_session_models::ClientId::new(),
        })
    }

    /// Append one arbitrary canonical event and return it.
    pub fn push_kind(&mut self, kind: SessionEventKind) -> SessionEvent {
        let sequence = self.next_sequence();
        let event = SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id: self.id,
            provenance: None,
            kind,
        };
        self.events.push(event.clone());
        event
    }

    /// The newest `count` events.
    #[must_use]
    pub fn latest(&self, count: usize) -> &[SessionEvent] {
        &self.events[self.events.len().saturating_sub(count)..]
    }

    /// Answer one history request from the canonical log.
    #[must_use]
    pub fn answer(&self, request: &HistoryRequest) -> HistoryResponse {
        match *request {
            HistoryRequest::Older {
                before_sequence,
                limits,
            } => {
                let end = self
                    .events
                    .partition_point(|event| event.sequence < before_sequence);
                let taken = self.page_len(self.events[..end].iter().rev(), limits);
                let start = end - taken;
                self.window(start, end)
            }
            HistoryRequest::Newer {
                after_sequence,
                limits,
            } => {
                let start = self
                    .events
                    .partition_point(|event| event.sequence <= after_sequence);
                let taken = self.page_len(self.events[start..].iter(), limits);
                self.window(start, start + taken)
            }
            HistoryRequest::Around {
                sequence,
                before_events,
                after_events,
            } => {
                let split = self
                    .events
                    .partition_point(|event| event.sequence <= sequence);
                self.window(
                    split.saturating_sub(before_events),
                    split.saturating_add(after_events).min(self.events.len()),
                )
            }
            HistoryRequest::Head { max_events } => {
                self.window(0, max_events.min(self.events.len()))
            }
            HistoryRequest::Tail { max_events } => self.window(
                self.events.len().saturating_sub(max_events),
                self.events.len(),
            ),
        }
    }

    fn window(&self, start: usize, end: usize) -> HistoryResponse {
        HistoryResponse {
            events: self.events[start..end].to_vec(),
            has_older: start > 0,
            has_newer: end < self.events.len(),
        }
    }

    /// Number of events a page takes walking away from the resident boundary.
    fn page_len<'a>(
        &self,
        events: impl Iterator<Item = &'a SessionEvent>,
        limits: PageLimits,
    ) -> usize {
        let mut taken = 0_usize;
        let mut items = 0_usize;
        let mut rows = 0_usize;
        for event in events {
            if taken >= limits.max_events || items >= limits.max_items {
                break;
            }
            if items >= limits.target_items && rows >= limits.target_rows {
                break;
            }
            taken += 1;
            if let Some(lines) = self.item_lines(event.sequence) {
                items += 1;
                // Marker lines plus one separator row: an estimate, like the daemon's.
                rows += usize::try_from(lines)
                    .unwrap_or(usize::MAX)
                    .saturating_add(1);
            }
        }
        taken
    }
}

/// Markdown body of one scenario item: one hard-broken marker line per source line.
fn item_text(sequence: u64, lines: u32) -> String {
    (0..lines.max(1))
        .map(|line| marker_line(sequence, line))
        .collect::<Vec<_>>()
        .join("\\\n")
}

/// Text of one marker line.
fn marker_line(sequence: u64, line: u32) -> String {
    format!(
        "{} item {sequence} line {line}",
        Marker::new(sequence, line).token()
    )
}

#[cfg(test)]
mod tests {
    use super::{Marker, ScenarioSession};
    use crate::transcript_scenarios::driver::{HistoryRequest, PageLimits};

    #[test]
    fn markers_round_trip_and_reject_lookalikes() {
        let marker = Marker::new(42, 3);
        assert_eq!(Marker::parse(&marker.token()), Some(marker));
        assert_eq!(Marker::parse("(S00042L003)"), Some(marker));
        assert_eq!(Marker::parse("S42L3"), None);
        assert_eq!(Marker::parse("SxxxxxLyyy"), None);
    }

    #[test]
    fn fake_daemon_pages_respect_bounds_and_targets() {
        let session = ScenarioSession::with_assistant_items(40, 3);
        let limits = PageLimits {
            target_items: 4,
            target_rows: 24,
            max_items: 16,
            max_events: 512,
        };
        let older = session.answer(&HistoryRequest::Older {
            before_sequence: 31,
            limits,
        });
        let sequences = older
            .events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>();
        assert_eq!(sequences, (25..31).collect::<Vec<_>>());
        assert!(older.has_older);
        assert!(older.has_newer);
        let newer = session.answer(&HistoryRequest::Newer {
            after_sequence: 38,
            limits,
        });
        assert_eq!(newer.events.len(), 2);
        assert!(!newer.has_newer);
        let around = session.answer(&HistoryRequest::Around {
            sequence: 20,
            before_events: 5,
            after_events: 5,
        });
        assert_eq!(around.events.first().map(|event| event.sequence), Some(16));
        assert_eq!(around.events.last().map(|event| event.sequence), Some(25));
        let head = session.answer(&HistoryRequest::Head { max_events: 3 });
        assert_eq!(head.events.len(), 3);
        assert!(!head.has_older && head.has_newer);
        let tail = session.answer(&HistoryRequest::Tail { max_events: 3 });
        assert_eq!(tail.events.last().map(|event| event.sequence), Some(40));
        assert!(tail.has_older && !tail.has_newer);
    }
}
