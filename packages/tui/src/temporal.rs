//! Generic temporal invalidation helpers for rendered time labels.

use std::time::{Duration, Instant, SystemTime};

use crate::time_format::{format_millis, unix_time_millis};

/// Return the instant when an elapsed millisecond label should next invalidate.
#[must_use]
pub fn next_elapsed_invalidation(
    started_at_ms: u64,
    finished_at_ms: Option<u64>,
    now_instant: Instant,
    now_system: SystemTime,
) -> Option<Instant> {
    if finished_at_ms.is_some() {
        return None;
    }
    let now_ms = unix_time_millis(now_system);
    let elapsed_ms = now_ms.saturating_sub(started_at_ms);
    let current = format_millis(elapsed_ms);
    let next_ms = next_elapsed_change_ms(elapsed_ms, &current);
    Some(now_instant + Duration::from_millis(next_ms.saturating_sub(elapsed_ms).max(1)))
}

fn next_elapsed_change_ms(elapsed_ms: u64, current: &str) -> u64 {
    let mut probe = elapsed_ms.saturating_add(1);
    let mut step = 1;
    loop {
        if format_millis(probe) != current {
            return probe;
        }
        probe = probe.saturating_add(step);
        step = (step.saturating_mul(2)).min(60_000);
    }
}
