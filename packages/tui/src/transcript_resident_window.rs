//! Resident transcript event window for bounded TUI memory.

#[cfg(test)]
use bcode_session_models::SessionHistoryCursor;
use bcode_session_models::{SessionEvent, SessionEventKind};

/// Policy for trimming resident transcript events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptWindowPolicy {
    /// Maximum resident event count before trimming is considered.
    pub max_events: usize,
    /// Target resident event count after trimming.
    pub target_events: usize,
    /// Whether UX state currently allows dropping off-screen older events.
    pub allow_trim: bool,
}

/// Result of a resident transcript-window trim attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptWindowTrim {
    /// Number of events dropped from the start of the resident window.
    pub dropped_event_count: usize,
    /// New oldest resident event sequence after trimming.
    pub new_oldest_sequence: Option<u64>,
}

impl TranscriptWindowTrim {
    /// Return whether events were dropped.
    #[must_use]
    pub const fn trimmed(self) -> bool {
        self.dropped_event_count > 0
    }
}

/// Resident transcript-affecting session events retained by the TUI.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TranscriptResidentWindow {
    events: Vec<SessionEvent>,
    dropped_before_sequence: Option<u64>,
}

impl TranscriptResidentWindow {
    /// Create a resident transcript window from replayed events.
    #[cfg(test)]
    #[must_use]
    pub fn new(events: &[SessionEvent]) -> Self {
        Self {
            events: events.to_vec(),
            dropped_before_sequence: None,
        }
    }

    /// Return resident events.
    #[must_use]
    pub fn events(&self) -> &[SessionEvent] {
        &self.events
    }

    /// Return resident event count.
    #[cfg(test)]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.events.len()
    }

    /// Replace resident events with a bounded transcript window.
    pub fn replace_window(&mut self, events: &[SessionEvent]) {
        self.events.clear();
        self.events.extend_from_slice(events);
        self.dropped_before_sequence = events.first().map(|event| event.sequence);
    }

    /// Append one live event to the resident window.
    pub fn append_live_event(&mut self, event: &SessionEvent) {
        self.events.push(event.clone());
    }

    /// Append replayed events to the resident window.
    pub fn append_history(&mut self, events: &[SessionEvent]) {
        self.events.extend_from_slice(events);
    }

    /// Prepend older history to the resident window.
    pub fn prepend_older_history(&mut self, events: &[SessionEvent]) {
        self.events.splice(0..0, events.iter().cloned());
        if let Some(first) = self.events.first() {
            self.dropped_before_sequence = self
                .dropped_before_sequence
                .filter(|sequence| *sequence < first.sequence);
        }
    }

    /// Return oldest resident event sequence.
    #[cfg(test)]
    #[must_use]
    pub fn oldest_sequence(&self) -> Option<u64> {
        self.events.first().map(|event| event.sequence)
    }

    /// Return newest resident event sequence.
    #[must_use]
    pub fn newest_sequence(&self) -> Option<u64> {
        self.events.last().map(|event| event.sequence)
    }

    /// Return a cursor that can reload dropped older events.
    #[cfg(test)]
    #[must_use]
    pub fn dropped_before_cursor(&self) -> Option<SessionHistoryCursor> {
        self.dropped_before_sequence
            .map(|sequence| SessionHistoryCursor {
                sequence: sequence.saturating_sub(1),
            })
    }

    /// Trim old resident events according to policy and safe transcript boundaries.
    pub fn trim_if_allowed(&mut self, policy: TranscriptWindowPolicy) -> TranscriptWindowTrim {
        if !policy.allow_trim || self.events.len() <= policy.max_events {
            return no_trim();
        }
        let Some(cut_index) = safe_trim_start_index(&self.events, policy.target_events) else {
            return no_trim();
        };
        if cut_index == 0 || cut_index >= self.events.len() {
            return no_trim();
        }
        let new_oldest_sequence = self.events[cut_index].sequence;
        self.events.drain(..cut_index);
        self.dropped_before_sequence = Some(new_oldest_sequence);
        TranscriptWindowTrim {
            dropped_event_count: cut_index,
            new_oldest_sequence: Some(new_oldest_sequence),
        }
    }
}

const fn no_trim() -> TranscriptWindowTrim {
    TranscriptWindowTrim {
        dropped_event_count: 0,
        new_oldest_sequence: None,
    }
}

fn safe_trim_start_index(events: &[SessionEvent], target_keep_events: usize) -> Option<usize> {
    if events.len() <= target_keep_events {
        return None;
    }
    let minimum_start = events.len().saturating_sub(target_keep_events);
    events
        .iter()
        .enumerate()
        .skip(minimum_start)
        .find_map(|(index, event)| is_safe_window_start(event).then_some(index))
}

const fn is_safe_window_start(event: &SessionEvent) -> bool {
    matches!(event.kind, SessionEventKind::UserMessage { .. })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_models::{
        CURRENT_SESSION_EVENT_SCHEMA_VERSION, ClientId, SessionEventKind, SessionId,
    };

    fn event(sequence: u64, kind: SessionEventKind) -> SessionEvent {
        SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: 1,
            session_id: SessionId::new(),
            provenance: None,
            kind,
        }
    }

    fn user(sequence: u64) -> SessionEvent {
        event(
            sequence,
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: format!("user {sequence}"),
            },
        )
    }

    fn assistant(sequence: u64) -> SessionEvent {
        event(
            sequence,
            SessionEventKind::AssistantMessage {
                text: format!("assistant {sequence}"),
            },
        )
    }

    #[test]
    fn trims_only_to_user_message_boundary() {
        let events = [
            user(1),
            assistant(2),
            user(3),
            assistant(4),
            user(5),
            assistant(6),
        ];
        let mut window = TranscriptResidentWindow::new(&events);

        let trim = window.trim_if_allowed(TranscriptWindowPolicy {
            max_events: 4,
            target_events: 3,
            allow_trim: true,
        });

        assert_eq!(trim.dropped_event_count, 4);
        assert_eq!(trim.new_oldest_sequence, Some(5));
        assert_eq!(window.oldest_sequence(), Some(5));
        assert_eq!(
            window.dropped_before_cursor(),
            Some(SessionHistoryCursor { sequence: 4 })
        );
    }

    #[test]
    fn does_not_trim_when_disallowed() {
        let events = [user(1), assistant(2), user(3), assistant(4), user(5)];
        let mut window = TranscriptResidentWindow::new(&events);

        let trim = window.trim_if_allowed(TranscriptWindowPolicy {
            max_events: 2,
            target_events: 1,
            allow_trim: false,
        });

        assert!(!trim.trimmed());
        assert_eq!(window.len(), events.len());
    }
}
