//! Explicit cost projection maintenance contracts.

use serde::{Deserialize, Serialize};

/// Inclusive start and exclusive end of a request-usage timestamp range (Unix milliseconds).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCostRange {
    /// Include requests first observed at or after this time.
    pub from_timestamp_ms: u64,
    /// Exclude requests first observed at or after this time.
    pub to_timestamp_ms: u64,
}

impl SessionCostRange {
    /// Validate a nonempty representable timestamp interval.
    ///
    /// # Errors
    ///
    /// Returns an error for reversed, empty, or unrepresentable ranges.
    pub fn validate(self) -> Result<(), String> {
        if self.from_timestamp_ms >= self.to_timestamp_ms
            || self.to_timestamp_ms > i64::MAX.cast_unsigned()
        {
            return Err("cost range must have 0 <= from < to <= i64::MAX".to_owned());
        }
        Ok(())
    }

    /// Whether a request timestamp is in this half-open range.
    #[must_use]
    pub const fn contains(self, timestamp_ms: u64) -> bool {
        timestamp_ms >= self.from_timestamp_ms && timestamp_ms < self.to_timestamp_ms
    }
}

/// Result of atomically repricing a session's derived request contributions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRepriceReport {
    /// Session whose usage was valued.
    pub session_id: super::SessionId,
    /// Timestamp scope applied to first-observed request usage.
    pub range: SessionCostRange,
    /// Number of request contributions replaced, including unavailable estimates.
    /// This counts request attempts, not conversational turns.
    pub repriced_requests: u64,
    /// Caller-supplied catalog snapshot revision, for diagnostics.
    pub catalog_revision: String,
    /// SHA-256 of the supplied catalog document, not a mutable catalog label.
    pub catalog_digest: String,
    /// New cumulative projection. Canonical events are unchanged.
    pub summary: super::SessionUsageSummary,
}
