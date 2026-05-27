//! Human-readable time and duration formatting helpers.

use std::time::{SystemTime, UNIX_EPOCH};

/// Return the current Unix timestamp in milliseconds.
#[must_use]
pub fn unix_time_millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Format a millisecond duration for display.
#[must_use]
pub fn format_millis(ms: u64) -> String {
    format_duration_nanos(u128::from(ms).saturating_mul(1_000_000))
}

/// Format an elapsed duration between optional Unix millisecond timestamps.
#[must_use]
pub fn format_elapsed_millis(
    started_at_ms: Option<u64>,
    finished_at_ms: Option<u64>,
) -> Option<String> {
    let started_at_ms = started_at_ms?;
    let end_at_ms = finished_at_ms.unwrap_or_else(unix_time_millis_now);
    Some(format_millis(end_at_ms.saturating_sub(started_at_ms)))
}

/// Format a nanosecond duration using compact, human-friendly units.
#[must_use]
pub fn format_duration_nanos(nanos: u128) -> String {
    if nanos < 1_000 {
        return format!("{nanos}ns");
    }
    if nanos < 1_000_000 {
        return format_decimal_unit(nanos, 1_000, "µs");
    }
    if nanos < 1_000_000_000 {
        return format_decimal_unit(nanos, 1_000_000, "ms");
    }

    if nanos < 60_000_000_000 {
        return format_decimal_unit(nanos, 1_000_000_000, "s");
    }
    let total_seconds = nanos / 1_000_000_000;
    let minutes = total_seconds / 60;
    let seconds = total_seconds % 60;
    if minutes < 60 {
        return if seconds == 0 {
            format!("{minutes}m")
        } else {
            format!("{minutes}m {seconds}s")
        };
    }
    let hours = minutes / 60;
    let minutes = minutes % 60;
    if hours < 24 {
        return if minutes == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h {minutes}m")
        };
    }
    let days = hours / 24;
    let hours = hours % 24;
    if hours == 0 {
        format!("{days}d")
    } else {
        format!("{days}d {hours}h")
    }
}

fn format_decimal_unit(nanos: u128, unit_nanos: u128, suffix: &str) -> String {
    let whole = nanos / unit_nanos;
    let remainder = nanos % unit_nanos;
    if whole >= 100 || remainder == 0 {
        return format!("{whole}{suffix}");
    }
    let decimal = remainder.saturating_mul(10) / unit_nanos;
    if decimal == 0 {
        format!("{whole}{suffix}")
    } else {
        format!("{whole}.{decimal}{suffix}")
    }
}
