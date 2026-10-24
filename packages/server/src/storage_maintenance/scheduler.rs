//! Bounded round-robin scheduling and per-session retry suppression.

use super::MaintenanceCursor;
use bcode_session_models::SessionId;
use std::collections::{BTreeMap, VecDeque};

const MAX_PENDING: usize = 32;
const DISCOVERY_PAGE: usize = 16;
const MAX_RETRIES: usize = 256;
const RETRY_BASE_MS: u64 = 30_000;
const RETRY_MAX_MS: u64 = 3_600_000;
const COMPLETED_PAUSE_MS: u64 = 300_000;

#[derive(Clone, Copy)]
struct Retry {
    failures: u32,
    after_ms: u64,
}

#[derive(Default)]
pub(super) struct Scheduler {
    pending: VecDeque<(SessionId, MaintenanceCursor)>,
    retries: BTreeMap<SessionId, Retry>,
}

impl Scheduler {
    pub(super) fn needs_discovery(&self) -> bool {
        self.pending.len() <= MAX_PENDING - DISCOVERY_PAGE
    }

    pub(super) fn discover(&mut self, ids: impl IntoIterator<Item = SessionId>, now: u64) {
        for id in ids.into_iter().take(DISCOVERY_PAGE) {
            if self.pending.len() == MAX_PENDING {
                break;
            }
            if self.pending.iter().any(|(queued, _)| *queued == id)
                || self
                    .retries
                    .get(&id)
                    .is_some_and(|retry| now < retry.after_ms)
            {
                continue;
            }
            self.pending
                .push_back((id, MaintenanceCursor::Artifacts(None)));
        }
    }

    pub(super) fn next(&mut self) -> Option<(SessionId, MaintenanceCursor)> {
        self.pending.pop_front()
    }

    pub(super) fn finish(
        &mut self,
        id: SessionId,
        outcome: Result<Option<MaintenanceCursor>, String>,
        now: u64,
    ) -> Option<u64> {
        let (failures, delay) = match outcome {
            Ok(Some(cursor)) => {
                // One page per turn: a large session cannot monopolize the worker.
                self.retries.remove(&id);
                self.pending.push_back((id, cursor));
                return None;
            }
            Ok(None) => (0, COMPLETED_PAUSE_MS),
            Err(_) => {
                let failures = self
                    .retries
                    .get(&id)
                    .map_or(1, |retry| retry.failures.saturating_add(1));
                let delay =
                    RETRY_BASE_MS.saturating_mul(1_u64 << failures.saturating_sub(1).min(7));
                (failures, delay.min(RETRY_MAX_MS))
            }
        };
        if self.retries.len() == MAX_RETRIES && !self.retries.contains_key(&id) {
            // Retry state is advisory, never durable authority. Evict the earliest deadline;
            // do not let one unavailable store grow daemon memory with catalog size.
            if let Some(oldest) = self
                .retries
                .iter()
                .min_by_key(|(_, retry)| retry.after_ms)
                .map(|(id, _)| *id)
            {
                self.retries.remove(&oldest);
            }
        }
        self.retries.insert(
            id,
            Retry {
                failures,
                after_ms: now.saturating_add(delay),
            },
        );
        Some(now.saturating_add(delay))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_session_yields_to_newly_discovered_sessions_and_deduplicates() {
        let mut scheduler = Scheduler::default();
        let large = SessionId::new();
        let other = SessionId::new();
        scheduler.discover([large], 0);
        let (id, _) = scheduler.next().expect("first");
        let _ = scheduler.finish(
            id,
            Ok(Some(MaintenanceCursor::History {
                start: 1,
                through: Some(100),
            })),
            0,
        );
        assert!(scheduler.needs_discovery());
        scheduler.discover([large, other, other], 0);
        assert_eq!(scheduler.next().expect("continuation").0, large);
        assert_eq!(scheduler.next().expect("other").0, other);
        assert!(scheduler.next().is_none());
    }

    #[test]
    fn failures_back_off_exponentially_and_success_resets_the_streak() {
        let mut scheduler = Scheduler::default();
        let id = SessionId::new();
        let _ = scheduler.finish(id, Err("owner".into()), 0);
        scheduler.discover([id], RETRY_BASE_MS - 1);
        assert!(scheduler.next().is_none());
        scheduler.discover([id], RETRY_BASE_MS);
        assert_eq!(scheduler.next().expect("retry").0, id);
        let _ = scheduler.finish(id, Err("owner".into()), RETRY_BASE_MS);
        assert_eq!(scheduler.retries[&id].after_ms, 3 * RETRY_BASE_MS);
        let _ = scheduler.finish(id, Ok(None), 3 * RETRY_BASE_MS);
        let _ = scheduler.finish(id, Err("owner".into()), 0);
        assert_eq!(scheduler.retries[&id].after_ms, RETRY_BASE_MS);
        for _ in 0..100 {
            let _ = scheduler.finish(id, Err("owner".into()), 0);
        }
        assert_eq!(scheduler.retries[&id].after_ms, RETRY_MAX_MS);
    }

    #[test]
    fn queue_and_retry_memory_remain_bounded() {
        let mut scheduler = Scheduler::default();
        for _ in 0..100 {
            scheduler.discover((0..DISCOVERY_PAGE).map(|_| SessionId::new()), 0);
        }
        assert_eq!(scheduler.pending.len(), MAX_PENDING);
        for _ in 0..1000 {
            let _ = scheduler.finish(SessionId::new(), Err("unavailable".into()), 0);
        }
        assert_eq!(scheduler.retries.len(), MAX_RETRIES);
    }
}
