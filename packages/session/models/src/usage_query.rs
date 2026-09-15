//! Bounded, normalized request-accounting reads. Compatibility follows the session API.

use serde::{Deserialize, Serialize};

use crate::{SessionCostRange, SessionTokenUsage};

/// Maximum request contributions inspected in one normal read.
pub const MAX_SESSION_USAGE_PAGE_SIZE: u32 = 256;

/// Accounting generation; not a durable transport resume token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionUsageGeneration {
    /// Canonical checkpoint represented by the projection.
    pub through_sequence: Option<u64>,
    /// Independently advancing valuation revision.
    pub cost_revision: u64,
}

/// Bounded accounting query. Empty pages can have a continuation because the scan is bounded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionUsageQuery {
    /// First-observed request time, inclusive start and exclusive end.
    pub range: SessionCostRange,
    /// Exclusive opaque contribution key from the preceding page.
    pub after: Option<String>,
    /// Maximum contributions inspected, including those outside the range.
    pub limit: u32,
    /// Required generation when continuing a report; changed state requires restarting.
    pub generation: Option<SessionUsageGeneration>,
}

impl SessionUsageQuery {
    /// Validate the bounded query before accessing storage.
    ///
    /// # Errors
    /// Returns an error for invalid ranges, excessive cursors, or unsupported page sizes.
    pub fn validate(&self) -> Result<(), String> {
        self.range.validate()?;
        if self.limit == 0 || self.limit > MAX_SESSION_USAGE_PAGE_SIZE {
            return Err("usage page limit must be between 1 and 256".into());
        }
        if self
            .after
            .as_ref()
            .is_some_and(|key| key.is_empty() || key.len() > 4096)
        {
            return Err("invalid usage continuation key".into());
        }
        if self.after.is_some() && self.generation.is_none() {
            return Err("usage continuation requires an accounting generation".into());
        }
        Ok(())
    }
}

/// One deduplicated request contribution, with no private provider evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionUsageEntry {
    /// Stable session-local contribution identity (also supports unattributed observations).
    pub key: String,
    /// Immutable first-observed request timestamp.
    pub first_observed_at_ms: u64,
    /// Latest normalized usage and independently stored valuation.
    pub usage: SessionTokenUsage,
}

/// One coherent bounded page of accounting contributions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionUsagePage {
    /// Source generation shared by every entry.
    pub generation: SessionUsageGeneration,
    /// Matching contributions, in contribution-key order.
    pub entries: Vec<SessionUsageEntry>,
    /// Number of source contributions examined, including nonmatching requests.
    pub scanned: u32,
    /// Continue even when entries are empty; absent only at the end of the source.
    pub next_after: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query() -> SessionUsageQuery {
        SessionUsageQuery {
            range: SessionCostRange {
                from_timestamp_ms: 0,
                to_timestamp_ms: 100,
            },
            after: None,
            limit: MAX_SESSION_USAGE_PAGE_SIZE,
            generation: None,
        }
    }

    #[test]
    fn rejects_invalid_bounds_and_unfenced_continuations() {
        assert!(query().validate().is_ok());
        for limit in [0, MAX_SESSION_USAGE_PAGE_SIZE + 1, u32::MAX] {
            assert!(SessionUsageQuery { limit, ..query() }.validate().is_err());
        }
        for after in [String::new(), "x".repeat(4097), "request".into()] {
            assert!(
                SessionUsageQuery {
                    after: Some(after),
                    ..query()
                }
                .validate()
                .is_err()
            );
        }
        let generation = SessionUsageGeneration {
            through_sequence: Some(4),
            cost_revision: 2,
        };
        assert!(
            SessionUsageQuery {
                after: Some("request".into()),
                generation: Some(generation),
                ..query()
            }
            .validate()
            .is_ok()
        );
        assert!(
            SessionUsageQuery {
                range: SessionCostRange {
                    from_timestamp_ms: 100,
                    to_timestamp_ms: 100
                },
                ..query()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn portable_query_round_trips_and_rejects_unknown_fields() {
        let value = serde_json::to_value(query()).unwrap();
        let decoded: SessionUsageQuery = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(decoded, query());
        let mut future = value;
        future["future_filter"] = serde_json::json!(true);
        assert!(serde_json::from_value::<SessionUsageQuery>(future).is_err());
    }
}
