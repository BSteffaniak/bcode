#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
//! Versioned, renderer-neutral usage reports. Amounts are estimates, not invoices.

use bcode_session_models::{SessionCostRange, SessionId, SessionUsageEntry};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Independently evolving reporting contract version.
pub const USAGE_VERSION: u32 = 1;

/// Exact recorded model identity; unknown identity is never resolved through aliases.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct UsageModel {
    /// Recorded catalog provider.
    pub provider: Option<String>,
    /// Recorded catalog model.
    pub model: Option<String>,
}

/// Bounded reporting request. Source selection is restricted to the serving state location.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageQuery {
    /// Must equal `USAGE_VERSION`.
    pub version: u32,
    /// First-observed request interval, UTC milliseconds.
    pub range: SessionCostRange,
    /// Empty selects every recorded model.
    pub models: BTreeSet<UsageModel>,
    /// Empty selects every provider.
    pub providers: BTreeSet<String>,
    /// Optional drill-down scope.
    pub session_id: Option<SessionId>,
    /// UTC-aligned bucket duration; at most 366 buckets per report.
    pub bucket_ms: u64,
    /// Exclusive reporting row cursor.
    pub after: Option<u64>,
    /// Required index revision for continuation; changes require restarting.
    pub revision: Option<u64>,
    /// At most 256 rows per page.
    pub limit: u32,
}

impl UsageQuery {
    /// Check supported compatibility and bounded request size.
    /// # Errors
    /// Rejects unsupported versions, ranges, filters, bucket counts, and page sizes.
    pub fn validate(&self) -> Result<(), String> {
        if self.after.is_some() && self.revision.is_none() {
            return Err("usage continuation requires an index revision".into());
        }
        if self.providers.iter().any(|value| value.len() > 4096)
            || self.models.iter().any(|identity| {
                identity
                    .provider
                    .as_ref()
                    .is_some_and(|value| value.len() > 4096)
                    || identity
                        .model
                        .as_ref()
                        .is_some_and(|value| value.len() > 4096)
            })
        {
            return Err("oversized usage filter".into());
        }
        self.range.validate()?;
        if self.version != USAGE_VERSION
            || self.limit == 0
            || self.limit > 256
            || self.bucket_ms == 0
            || self.models.len() > 256
            || self.providers.len() > 256
        {
            return Err("unsupported or oversized usage query".into());
        }
        if self.range.to_timestamp_ms / self.bucket_ms
            - self.range.from_timestamp_ms / self.bucket_ms
            > 365
        {
            return Err("usage query exceeds 366 time buckets".into());
        }
        Ok(())
    }
}

/// Checked aggregates; missing token buckets remain visibly incomplete.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageTotals {
    /// Number of represented attempts.
    pub requests: u64,
    /// Attempts with a complete estimate.
    pub priced_requests: u64,
    /// Known estimated subtotal per currency; never currency-converted.
    pub cost_micros: BTreeMap<String, u64>,
    /// Sum of known input counts (includes cache input according to normalized semantics).
    pub input_tokens: u64,
    /// Sum of known output counts.
    pub output_tokens: u64,
    /// Sum of known cache-read counts; not added to input totals again.
    pub cache_read_tokens: u64,
    /// Sum of known cache-write counts.
    pub cache_write_tokens: u64,
    /// Requests missing either input or output counts.
    pub incomplete_token_requests: u64,
}

/// A model comparison row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageModelRow {
    /// Exact grouping identity.
    pub model: UsageModel,
    /// Known totals and coverage.
    pub totals: UsageTotals,
}

/// A time series bucket with model-separated values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageBucket {
    /// Inclusive UTC bucket boundary.
    pub timestamp_ms: u64,
    /// Model comparisons within this bucket.
    pub models: Vec<UsageModelRow>,
}

/// One request drill-down row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageRequestRow {
    /// Reporting generation-local row identity.
    pub ordinal: u64,
    /// Owning session.
    pub session_id: SessionId,
    /// Normalized request and stored valuation, without private evidence.
    pub entry: SessionUsageEntry,
}

/// Consistent report from one explicitly collected generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageReport {
    /// Snapshot index revision. Not a durable resume token.
    pub revision: u64,
    /// Contract version.
    pub version: u32,
    /// Known filtered totals.
    pub totals: UsageTotals,
    /// All model rows represented by this report page.
    pub models: Vec<UsageModelRow>,
    /// Bounded time series.
    pub buckets: Vec<UsageBucket>,
    /// Request details for the selected page.
    pub requests: Vec<UsageRequestRow>,
    /// Continue request export/drill-down from this ordinal.
    pub next_after: Option<u64>,
    /// Explicit coverage/status description. Reports are never provider invoices.
    pub coverage: String,
}
