//! Generic temporal invalidation helpers for rendered time labels.

use std::time::{Duration, Instant, SystemTime};

use crate::time_format::{format_millis, unix_time_millis};

/// Return the instant when an elapsed label should next invalidate, capped by `max_interval`.
#[must_use]
pub fn next_elapsed_invalidation_capped(
    started_at_ms: u64,
    finished_at_ms: Option<u64>,
    now_instant: Instant,
    now_system: SystemTime,
    max_interval: Duration,
) -> Option<Instant> {
    if finished_at_ms.is_some() {
        return None;
    }
    let now_ms = unix_time_millis(now_system);
    let elapsed_ms = now_ms.saturating_sub(started_at_ms);
    let current = format_millis(elapsed_ms);
    let next_ms = next_elapsed_change_ms(elapsed_ms, &current);
    let next_label_at =
        now_instant + Duration::from_millis(next_ms.saturating_sub(elapsed_ms).max(1));
    Some(next_label_at.min(now_instant + max_interval))
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
