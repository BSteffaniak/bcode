use std::time::Instant;

use bcode_model::{ProviderAttemptOutcome, ProviderAttemptTiming, ProviderTurnEvent};

#[derive(Debug, Default)]
pub struct Recorder {
    ordinal: u64,
    enabled: bool,
    waiting_ms: u64,
    active: Option<(Instant, ProviderAttemptTiming)>,
    last_chunk_ms: Option<u64>,
}

impl Recorder {
    pub(super) fn begin(&mut self, enabled: bool) {
        self.enabled = enabled;
        self.last_chunk_ms = None;
        if !enabled {
            self.active = None;
            self.waiting_ms = 0;
            return;
        }
        self.ordinal = self.ordinal.saturating_add(1);
        self.active = Some((
            Instant::now(),
            ProviderAttemptTiming {
                attempt: self.ordinal,
                retry_wait_before_ms: std::mem::take(&mut self.waiting_ms),
                auth_ready_ms: None,
                http_send_ms: None,
                response_headers_ms: None,
                first_semantic_ms: None,
                first_output_ms: None,
                last_output_ms: None,
                first_output_bytes_ms: None,
                last_output_bytes_ms: None,
                output_event_count: Some(0),
                output_delta_bytes: Some(0),
                stream_chunk_count: Some(0),
                stream_body_bytes: Some(0),
                max_inter_chunk_gap_ms: None,
                stream_terminal_ms: None,
                terminal_ms: 0,
                outcome: ProviderAttemptOutcome::Failed,
            },
        ));
    }

    fn elapsed(start: Instant) -> u64 {
        u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    pub(super) const fn waited(&mut self, elapsed: u64) {
        if self.enabled {
            self.waiting_ms = self.waiting_ms.saturating_add(elapsed);
        }
    }

    pub(super) fn cancelled_wait(&mut self) -> Option<ProviderAttemptTiming> {
        if !self.enabled {
            return None;
        }
        self.begin(true);
        self.finish(ProviderAttemptOutcome::Cancelled)
    }

    pub(super) fn auth_ready(&mut self) {
        if let Some((start, timing)) = &mut self.active {
            timing
                .auth_ready_ms
                .get_or_insert_with(|| Self::elapsed(*start));
        }
    }

    pub(super) fn send(&mut self) {
        if let Some((start, timing)) = &mut self.active {
            timing
                .http_send_ms
                .get_or_insert_with(|| Self::elapsed(*start));
        }
    }

    pub(super) fn headers(&mut self) {
        if let Some((start, timing)) = &mut self.active {
            timing
                .response_headers_ms
                .get_or_insert_with(|| Self::elapsed(*start));
        }
    }

    pub(super) fn observe(&mut self, event: &ProviderTurnEvent) {
        use bcode_model::ProviderOutputEvent;
        let (output, semantic, bytes) = match event {
            ProviderTurnEvent::TextDelta { text }
            | ProviderTurnEvent::Output {
                event: ProviderOutputEvent::TextDelta { text },
                ..
            } => (true, true, text.len()),
            ProviderTurnEvent::ToolCallDelta { delta, .. }
            | ProviderTurnEvent::Output {
                event: ProviderOutputEvent::ToolCallDelta { delta, .. },
                ..
            } => (true, true, delta.len()),
            ProviderTurnEvent::ToolCallStarted { .. }
            | ProviderTurnEvent::ToolCallFinished { .. }
            | ProviderTurnEvent::Output {
                event:
                    ProviderOutputEvent::ToolCallStarted { .. }
                    | ProviderOutputEvent::ToolCallFinished { .. },
                ..
            } => (true, true, 0),
            ProviderTurnEvent::ReasoningDelta { .. }
            | ProviderTurnEvent::ReasoningActivity { .. }
            | ProviderTurnEvent::Output {
                event: ProviderOutputEvent::ReasoningActivity { .. },
                ..
            } => (false, true, 0),
            _ => (false, false, 0),
        };
        if let Some((start, _)) = self.active.as_ref() {
            self.observe_at(Self::elapsed(*start), output, semantic, bytes);
        }
    }

    fn observe_at(&mut self, elapsed: u64, output: bool, semantic: bool, bytes: usize) {
        if let Some((_, timing)) = &mut self.active {
            if semantic {
                timing.first_semantic_ms.get_or_insert(elapsed);
            }
            if output {
                timing.first_output_ms.get_or_insert(elapsed);
                timing.last_output_ms = Some(elapsed);
                timing.output_event_count =
                    Some(timing.output_event_count.unwrap_or(0).saturating_add(1));
                timing.output_delta_bytes = Some(
                    timing
                        .output_delta_bytes
                        .unwrap_or(0)
                        .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX)),
                );
                if bytes > 0 {
                    timing.first_output_bytes_ms.get_or_insert(elapsed);
                    timing.last_output_bytes_ms = Some(elapsed);
                }
            }
        }
    }

    pub(super) fn chunk(&mut self, bytes: usize) {
        if let Some((start, _)) = self.active.as_ref() {
            self.chunk_at(Self::elapsed(*start), bytes);
        }
    }

    fn chunk_at(&mut self, elapsed: u64, bytes: usize) {
        if let Some((_, timing)) = &mut self.active {
            timing.stream_chunk_count =
                Some(timing.stream_chunk_count.unwrap_or(0).saturating_add(1));
            timing.stream_body_bytes = Some(
                timing
                    .stream_body_bytes
                    .unwrap_or(0)
                    .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX)),
            );
            if let Some(previous) = self.last_chunk_ms.replace(elapsed) {
                timing.max_inter_chunk_gap_ms = Some(
                    timing
                        .max_inter_chunk_gap_ms
                        .unwrap_or(0)
                        .max(elapsed.saturating_sub(previous)),
                );
            }
        }
    }

    pub(super) fn stream_terminal(&mut self) {
        if let Some((start, timing)) = &mut self.active {
            timing
                .stream_terminal_ms
                .get_or_insert_with(|| Self::elapsed(*start));
        }
    }

    pub(super) fn finish(
        &mut self,
        outcome: ProviderAttemptOutcome,
    ) -> Option<ProviderAttemptTiming> {
        let (start, mut timing) = self.active.take()?;
        timing.terminal_ms = Self::elapsed(start);
        timing.outcome = outcome;
        Some(timing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_positioned_output_is_observed_without_extra_events() {
        use bcode_model::{ProviderOutputEvent, TurnOutputPosition};
        let turn = super::super::TurnState::default();
        turn.timing.lock().unwrap().begin(true);
        turn.push(ProviderTurnEvent::output(
            TurnOutputPosition::new(0),
            ProviderOutputEvent::TextDelta {
                text: "fixture".into(),
            },
        ));
        let timing = turn
            .timing
            .lock()
            .unwrap()
            .finish(ProviderAttemptOutcome::Completed)
            .unwrap();
        assert_eq!(timing.output_event_count, Some(1));
        assert_eq!(timing.output_delta_bytes, Some(7));
        assert!(timing.first_semantic_ms.is_some());
        assert!(timing.last_output_ms.is_some());
        assert_eq!(turn.events.lock().unwrap().len(), 1);
    }

    #[test]
    fn reasoning_activity_does_not_count_as_output_bytes() {
        let mut recorder = Recorder::default();
        recorder.begin(true);
        recorder.observe(&ProviderTurnEvent::ReasoningDelta {
            text: "synthetic".into(),
        });
        let timing = recorder.finish(ProviderAttemptOutcome::Failed).unwrap();
        assert!(timing.first_semantic_ms.is_some());
        assert_eq!(timing.first_output_ms, None);
        assert_eq!(timing.output_event_count, Some(0));
        assert_eq!(timing.output_delta_bytes, Some(0));
        assert_eq!(timing.last_output_bytes_ms, None);
    }

    #[test]
    fn legacy_snapshots_preserve_missing_stream_measurements() {
        let legacy = serde_json::json!({
            "attempt": 1, "retry_wait_before_ms": 0,
            "auth_ready_ms": null, "http_send_ms": null, "response_headers_ms": null,
            "first_semantic_ms": null, "first_output_ms": null,
            "terminal_ms": 12, "outcome": "cancelled"
        });
        let timing: ProviderAttemptTiming = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(timing.output_event_count, None);
        assert_eq!(timing.stream_body_bytes, None);
        assert_eq!(timing.stream_terminal_ms, None);
        assert_eq!(serde_json::to_value(timing).unwrap(), legacy);
    }

    #[test]
    fn positioned_and_legacy_output_measure_the_same_bytes() {
        use bcode_model::{ProviderOutputEvent, TurnOutputPosition};
        let mut legacy = Recorder::default();
        let mut positioned = Recorder::default();
        legacy.begin(true);
        positioned.begin(true);
        legacy.observe(&ProviderTurnEvent::TextDelta { text: "é".into() });
        positioned.observe(&ProviderTurnEvent::output(
            TurnOutputPosition::new(0),
            ProviderOutputEvent::TextDelta { text: "é".into() },
        ));
        legacy.observe(&ProviderTurnEvent::ToolCallDelta {
            call_id: "fixture".into(),
            delta: "{}".into(),
        });
        positioned.observe(&ProviderTurnEvent::output(
            TurnOutputPosition::new(1),
            ProviderOutputEvent::ToolCallDelta {
                call_id: "fixture".into(),
                delta: "{}".into(),
            },
        ));
        let left = legacy.finish(ProviderAttemptOutcome::Completed).unwrap();
        let right = positioned
            .finish(ProviderAttemptOutcome::Completed)
            .unwrap();
        assert_eq!(left.output_delta_bytes, Some(4));
        assert_eq!(left.output_delta_bytes, right.output_delta_bytes);
        assert_eq!(left.output_event_count, right.output_event_count);
        assert!(right.first_semantic_ms.is_some());
        assert!(right.first_output_bytes_ms.is_some());
    }

    #[test]
    fn chunk_gaps_output_lifecycle_and_interrupted_terminal_are_distinct() {
        let mut recorder = Recorder::default();
        recorder.begin(true);
        recorder.chunk_at(10, 5);
        recorder.chunk_at(30, 7);
        recorder.chunk_at(35, 3);
        recorder.observe_at(12, false, true, 0);
        recorder.observe_at(15, true, true, 0);
        recorder.observe_at(32, true, true, 4);
        recorder.observe_at(34, true, true, 0);
        let timing = recorder.finish(ProviderAttemptOutcome::Cancelled).unwrap();
        assert_eq!(timing.first_semantic_ms, Some(12));
        assert_eq!(timing.first_output_ms, Some(15));
        assert_eq!(timing.first_output_bytes_ms, Some(32));
        assert_eq!(timing.last_output_bytes_ms, Some(32));
        assert_eq!(timing.last_output_ms, Some(34));
        assert_eq!(timing.output_event_count, Some(3));
        assert_eq!(timing.output_delta_bytes, Some(4));
        assert_eq!(timing.stream_chunk_count, Some(3));
        assert_eq!(timing.stream_body_bytes, Some(15));
        assert_eq!(timing.max_inter_chunk_gap_ms, Some(20));
        assert_eq!(timing.stream_terminal_ms, None);
        assert!(recorder.finish(ProviderAttemptOutcome::Cancelled).is_none());
        recorder.begin(true);
        recorder.stream_terminal();
        let completed = recorder.finish(ProviderAttemptOutcome::Completed).unwrap();
        assert_eq!(completed.output_event_count, Some(0));
        assert_eq!(completed.max_inter_chunk_gap_ms, None);
        assert!(completed.stream_terminal_ms.unwrap() <= completed.terminal_ms);
    }

    #[test]
    fn default_off_and_terminal_is_once() {
        let mut recorder = Recorder::default();
        recorder.begin(false);
        assert!(recorder.finish(ProviderAttemptOutcome::Completed).is_none());
        recorder.begin(true);
        let timing = recorder.finish(ProviderAttemptOutcome::Cancelled).unwrap();
        assert_eq!(timing.attempt, 1);
        assert_eq!(timing.outcome, ProviderAttemptOutcome::Cancelled);
        assert_eq!(timing.http_send_ms, None);
        assert_eq!(timing.response_headers_ms, None);
        assert_eq!(timing.first_output_ms, None);
        assert!(recorder.finish(ProviderAttemptOutcome::Failed).is_none());
    }

    #[test]
    fn timing_ignores_metadata_and_preserves_retry_wait() {
        let mut recorder = Recorder::default();
        recorder.begin(true);
        recorder.observe(&ProviderTurnEvent::ProviderMetadata {
            key: "synthetic".into(),
            value: "not timing".into(),
        });
        let timing = recorder.finish(ProviderAttemptOutcome::Failed).unwrap();
        assert_eq!(timing.first_semantic_ms, None);
        assert_eq!(timing.first_output_ms, None);
        recorder.waited(12);
        let interrupted = recorder.cancelled_wait().unwrap();
        assert_eq!(interrupted.retry_wait_before_ms, 12);
        assert_eq!(interrupted.outcome, ProviderAttemptOutcome::Cancelled);
        assert_eq!(interrupted.http_send_ms, None);
        recorder.waited(12);
        recorder.begin(true);
        let timing = recorder.finish(ProviderAttemptOutcome::Cancelled).unwrap();
        assert_eq!(timing.retry_wait_before_ms, 12);
        assert_eq!(timing.auth_ready_ms, None);
        let json = serde_json::to_value(&timing).unwrap();
        assert!(json["auth_ready_ms"].is_null());
        assert_eq!(
            serde_json::from_value::<ProviderAttemptTiming>(json).unwrap(),
            timing
        );
    }

    #[test]
    fn repeated_output_does_not_emit_or_replace_first_milestone() {
        let mut recorder = Recorder::default();
        recorder.begin(true);
        recorder.auth_ready();
        recorder.send();
        recorder.headers();
        recorder.observe(&ProviderTurnEvent::TextDelta {
            text: "synthetic".into(),
        });
        let first = recorder.active.as_ref().unwrap().1.first_output_ms;
        for _ in 0..1000 {
            recorder.observe(&ProviderTurnEvent::TextDelta {
                text: "synthetic".into(),
            });
        }
        let timing = recorder.finish(ProviderAttemptOutcome::Completed).unwrap();
        assert_eq!(timing.first_output_ms, first);
        assert!(timing.auth_ready_ms <= timing.http_send_ms);
        assert!(timing.http_send_ms <= timing.response_headers_ms);
        assert!(timing.response_headers_ms <= timing.first_output_ms);
        assert!(timing.first_output_ms.unwrap() <= timing.terminal_ms);
        recorder.begin(true);
        assert_eq!(
            recorder
                .finish(ProviderAttemptOutcome::Failed)
                .unwrap()
                .attempt,
            2
        );
    }
}
