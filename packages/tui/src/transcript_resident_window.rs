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

    /// Trim only complete turns below a surviving reader, leaving enough resident
    /// events for the current viewport and subsequent ordinary scrolling.
    pub fn trim_newer_offscreen(
        &mut self,
        max_events: usize,
        target_events: usize,
        reader_sequence: u64,
        viewport_rows: usize,
    ) -> Option<u64> {
        if self.events.len() <= max_events {
            return None;
        }
        let reader_index = self
            .events
            .iter()
            .position(|event| event.sequence == reader_sequence)?;
        let minimum_end = reader_index.saturating_add(viewport_rows).saturating_add(1);
        let preferred_end = target_events.max(minimum_end);
        if preferred_end >= self.events.len() {
            return None;
        }
        // Starting the discarded suffix at a user message keeps the final retained
        // turn intact. No safe boundary means we defer eviction rather than split it.
        let end = self
            .events
            .iter()
            .enumerate()
            .skip(preferred_end)
            .find_map(|(index, event)| is_safe_window_start(event).then_some(index))
            .or_else(|| {
                // A single long turn can cover the preferred suffix. Retain
                // the reader and its scroll buffer, but evict at an earlier
                // complete turn if one is still safely off-screen.
                self.events
                    .iter()
                    .enumerate()
                    .take(preferred_end)
                    .skip(minimum_end)
                    .rev()
                    .find_map(|(index, event)| is_safe_window_start(event).then_some(index))
            })?;
        if end >= self.events.len() {
            return None;
        }
        let first_dropped = self.events[end].sequence;
        self.events.truncate(end);
        Some(first_dropped)
    }

    /// Trim complete off-screen older turns without dropping a visible source.
    /// The caller chooses the first visible source event and a conservative
    /// event buffer to retain before it. Returns the new oldest sequence.
    pub fn trim_older_offscreen(
        &mut self,
        max_events: usize,
        target_events: usize,
        first_visible_sequence: u64,
        buffer_events: usize,
    ) -> Option<u64> {
        if self.events.len() <= max_events {
            return None;
        }
        let visible_index = self
            .events
            .iter()
            .position(|event| event.sequence == first_visible_sequence)?;
        let latest_safe_cut = visible_index.saturating_sub(buffer_events);
        let preferred_cut = self.events.len().saturating_sub(target_events);
        // Prefer the requested window size when a complete turn starts there.
        // Otherwise retain the nearest earlier complete turn rather than
        // abandoning safe off-screen eviction (or cutting through a turn).
        let cut = self
            .events
            .iter()
            .enumerate()
            .take(latest_safe_cut.saturating_add(1))
            .skip(preferred_cut)
            .find_map(|(index, event)| is_safe_window_start(event).then_some(index))
            .or_else(|| {
                self.events
                    .iter()
                    .enumerate()
                    .take(preferred_cut.min(latest_safe_cut.saturating_add(1)))
                    .rev()
                    .find_map(|(index, event)| is_safe_window_start(event).then_some(index))
            })?;
        if cut == 0 || cut >= self.events.len() {
            return None;
        }
        let new_oldest_sequence = self.events[cut].sequence;
        self.events.drain(..cut);
        self.dropped_before_sequence = Some(new_oldest_sequence);
        Some(new_oldest_sequence)
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
    let preferred_start = events.len().saturating_sub(target_keep_events);
    events
        .iter()
        .enumerate()
        .skip(preferred_start)
        .find_map(|(index, event)| is_safe_window_start(event).then_some(index))
        .or_else(|| {
            // The preferred suffix can lie inside a long turn. Keep that turn
            // intact while still releasing complete off-screen turns before it.
            events
                .iter()
                .enumerate()
                .take(preferred_start)
                .rev()
                .find_map(|(index, event)| is_safe_window_start(event).then_some(index))
        })
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
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
            },
        )
    }

    fn assistant(sequence: u64) -> SessionEvent {
        event(
            sequence,
            SessionEventKind::AssistantResponseSegment {
                turn_id: "turn-1".to_owned(),
                segment_id: format!("segment-{sequence}"),
                segment_order: u32::try_from(sequence).expect("test sequence fits"),
                text: format!("assistant {sequence}"),
            },
        )
    }

    #[test]
    fn older_trim_retains_visible_reader_and_safe_turn_boundary() {
        let events = (1..=1200).map(user).collect::<Vec<_>>();
        let mut window = TranscriptResidentWindow::new(&events);
        assert_eq!(window.trim_older_offscreen(1024, 512, 1190, 48), Some(689));
        assert_eq!(window.oldest_sequence(), Some(689));
        assert_eq!(window.newest_sequence(), Some(1200));
        assert!(window.events().iter().any(|event| event.sequence == 1190));
        assert_eq!(window.trim_older_offscreen(1024, 512, 1190, 48), None);
    }

    #[test]
    fn older_trim_makes_safe_progress_even_when_reader_is_inside_target_suffix() {
        let events = (1..=1200).map(user).collect::<Vec<_>>();
        let mut window = TranscriptResidentWindow::new(&events);
        // The reader is safely beyond the beginning, but retaining it and the
        // scroll buffer requires more than the preferred 512-event window.
        assert_eq!(window.trim_older_offscreen(1024, 512, 600, 48), Some(552));
        assert_eq!(window.oldest_sequence(), Some(552));
        assert!(window.events().iter().any(|event| event.sequence == 600));
        assert!(window.events().len() < events.len());
    }

    #[test]
    fn older_trim_uses_earlier_complete_turn_when_preferred_range_has_none() {
        let mut events = (1..=1200).map(user).collect::<Vec<_>>();
        // Only the turn at 600 is a safe start before the reader; the
        // preferred [689, 1100] range contains no complete turn boundary.
        for event in &mut events[600..1120] {
            *event = assistant(event.sequence);
        }
        let mut window = TranscriptResidentWindow::new(&events);
        assert_eq!(window.trim_older_offscreen(1024, 512, 1150, 48), Some(600));
        assert_eq!(window.oldest_sequence(), Some(600));
        assert!(window.events().iter().any(|event| event.sequence == 1150));
    }

    #[test]
    fn older_trim_defers_when_reader_is_near_start() {
        let events = (1..=1200).map(user).collect::<Vec<_>>();
        let mut window = TranscriptResidentWindow::new(&events);
        assert_eq!(window.trim_older_offscreen(1024, 512, 20, 48), None);
    }

    #[test]
    fn newer_trim_preserves_reader_and_complete_turn_boundary() {
        let events = (1..=1200).map(user).collect::<Vec<_>>();
        let mut window = TranscriptResidentWindow::new(&events);
        let first_dropped = window.trim_newer_offscreen(1024, 512, 2, 48);
        assert_eq!(first_dropped, Some(513));
        assert_eq!(window.newest_sequence(), Some(512));
        assert_eq!(window.oldest_sequence(), Some(1));
        assert!(window.events().iter().any(|event| event.sequence == 2));
        assert_eq!(window.trim_newer_offscreen(1024, 512, 2, 48), None);
    }

    #[test]
    fn newer_trim_uses_earlier_complete_turn_when_preferred_suffix_has_none() {
        let mut events = (1..=1200).map(user).collect::<Vec<_>>();
        // A complete off-screen turn at 200 is safe; the preferred suffix
        // begins at 512 and contains no subsequent complete turn boundary.
        for event in &mut events[200..] {
            *event = assistant(event.sequence);
        }
        let mut window = TranscriptResidentWindow::new(&events);
        assert_eq!(window.trim_newer_offscreen(1024, 512, 20, 48), Some(200));
        assert_eq!(window.newest_sequence(), Some(199));
        assert!(window.events().iter().any(|event| event.sequence == 20));
    }

    #[test]
    fn newer_trim_defers_when_reader_is_near_end_or_no_safe_turn() {
        let events = (1..=1200).map(user).collect::<Vec<_>>();
        let mut window = TranscriptResidentWindow::new(&events);
        assert_eq!(window.trim_newer_offscreen(1024, 512, 1190, 48), None);
        let mut window = TranscriptResidentWindow::new(
            &events
                .iter()
                .map(|event| assistant(event.sequence))
                .collect::<Vec<_>>(),
        );
        assert_eq!(window.trim_newer_offscreen(1024, 512, 2, 48), None);
    }

    #[test]
    fn tail_trim_uses_earlier_complete_turn_when_target_suffix_is_one_turn() {
        let mut events = (1..=1200).map(user).collect::<Vec<_>>();
        for event in &mut events[600..] {
            *event = assistant(event.sequence);
        }
        let mut window = TranscriptResidentWindow::new(&events);
        let trim = window.trim_if_allowed(TranscriptWindowPolicy {
            max_events: 1024,
            target_events: 512,
            allow_trim: true,
        });
        assert_eq!(trim.new_oldest_sequence, Some(600));
        assert_eq!(window.oldest_sequence(), Some(600));
        assert_eq!(window.newest_sequence(), Some(1200));
        assert_eq!(
            window
                .trim_if_allowed(TranscriptWindowPolicy {
                    max_events: 1024,
                    target_events: 512,
                    allow_trim: true,
                })
                .dropped_event_count,
            0
        );
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
    fn large_invocation_history_remains_bounded_by_resident_window_policy() {
        const TURNS: u64 = 512;
        const TARGET_EVENTS: usize = 128;
        let mut events = Vec::new();
        for turn in 0..TURNS {
            let user_sequence = turn.saturating_mul(3).saturating_add(1);
            events.push(user(user_sequence));
            events.push(event(
                user_sequence.saturating_add(1),
                SessionEventKind::ToolCallRequested {
                    tool_call_id: format!("call-{turn}"),
                    producer_plugin_id: Some("test.plugin".to_owned()),
                    tool_name: "test.tool".to_owned(),
                    arguments_json: "{}".to_owned(),
                    working_directory: None,
                },
            ));
            events.push(assistant(user_sequence.saturating_add(2)));
        }
        let mut window = TranscriptResidentWindow::new(&events);
        let trim = window.trim_if_allowed(TranscriptWindowPolicy {
            max_events: 256,
            target_events: TARGET_EVENTS,
            allow_trim: true,
        });

        assert!(trim.trimmed());
        assert!(window.len() <= TARGET_EVENTS);
        assert!(matches!(
            window.events().first().map(|event| &event.kind),
            Some(SessionEventKind::UserMessage { .. })
        ));
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
