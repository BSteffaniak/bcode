use std::time::Instant;

use bcode_model::{ProviderAttemptOutcome, ProviderAttemptTiming, ProviderTurnEvent};

#[derive(Debug, Default)]
pub struct Recorder {
    ordinal: u64,
    enabled: bool,
    waiting_ms: u64,
    active: Option<(Instant, ProviderAttemptTiming)>,
}

impl Recorder {
    pub(super) fn begin(&mut self, enabled: bool) {
        self.enabled = enabled;
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
        let output = matches!(
            event,
            ProviderTurnEvent::TextDelta { .. }
                | ProviderTurnEvent::ToolCallStarted { .. }
                | ProviderTurnEvent::ToolCallDelta { .. }
                | ProviderTurnEvent::ToolCallFinished { .. }
        );
        let semantic = output
            || matches!(
                event,
                ProviderTurnEvent::ReasoningDelta { .. }
                    | ProviderTurnEvent::ReasoningActivity { .. }
            );
        if let Some((start, timing)) = &mut self.active {
            let elapsed = Self::elapsed(*start);
            if semantic {
                timing.first_semantic_ms.get_or_insert(elapsed);
            }
            if output {
                timing.first_output_ms.get_or_insert(elapsed);
            }
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
