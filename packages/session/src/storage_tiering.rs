//! Pure eligibility decisions for lossless artifact tier transitions.
//!
//! This is not an authorization grant. The maintenance caller must acquire and recheck durable
//! ownership, compatibility, finalization, and unchanged content before any side effect.

use std::time::Duration;

/// Physical representation temperature; ordering expresses compression effort, not read latency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StorageTier {
    /// Uncompressed storage.
    Hot,
    /// Light compression.
    Light,
    /// Deep compression.
    Deep,
}

/// Validated inactivity thresholds, supplied by an application or plugin-owned scheduler.
#[derive(Debug, Clone, Copy)]
pub struct StorageTieringPolicy {
    enabled: bool,
    light_after: Duration,
    deep_after: Duration,
}

/// Current facts required to propose a transition. Unknown timestamps are not filesystem atime.
#[derive(Debug, Clone, Copy)]
pub struct ArtifactTieringFacts {
    /// Current authoritative representation, already compatibility-validated by the caller.
    pub current: StorageTier,
    /// Successful meaningful read time, or conservative initialization of tracking, in Unix ms.
    pub last_access_ms: Option<u64>,
    /// Finalization time of the immutable content, in Unix ms.
    pub finalized_at_ms: Option<u64>,
    /// Whether finalization and completeness have been verified.
    pub finalized: bool,
    /// Whether durable ownership has been verified as released; unknown ownership is false.
    pub ownership_released: bool,
}

impl StorageTieringPolicy {
    /// Construct a validated policy. Thresholds are elapsed durations independent of time zones.
    ///
    /// # Errors
    ///
    /// Rejects zero, unordered, or sub-millisecond thresholds even when scheduling is disabled.
    pub fn new(
        enabled: bool,
        light_after: Duration,
        deep_after: Duration,
    ) -> Result<Self, &'static str> {
        if light_after.is_zero()
            || deep_after <= light_after
            || !light_after.subsec_nanos().is_multiple_of(1_000_000)
            || !deep_after.subsec_nanos().is_multiple_of(1_000_000)
        {
            return Err("storage thresholds require 0 < light < deep and whole milliseconds");
        }
        Ok(Self {
            enabled,
            light_after,
            deep_after,
        })
    }

    /// Propose a colder representation, never promotion or recompression of the current tier.
    ///
    /// Missing timestamps, clock rollback, active/incomplete content, or unknown ownership defer
    /// without mutation. A recent read does not eagerly rewrite cold content. Finalization counts
    /// as activity even when an older access record exists. Threshold boundaries are inclusive.
    #[must_use]
    pub fn transition(self, facts: ArtifactTieringFacts, now_ms: u64) -> Option<StorageTier> {
        if !self.enabled || !facts.finalized || !facts.ownership_released {
            return None;
        }
        let last_activity = facts.last_access_ms?.max(facts.finalized_at_ms?);
        let age = Duration::from_millis(now_ms.checked_sub(last_activity)?);
        let target = if age >= self.deep_after {
            StorageTier::Deep
        } else if age >= self.light_after {
            StorageTier::Light
        } else {
            StorageTier::Hot
        };
        (target > facts.current).then_some(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 86_400_000;

    fn policy(enabled: bool) -> StorageTieringPolicy {
        StorageTieringPolicy::new(
            enabled,
            Duration::from_millis(5 * DAY),
            Duration::from_millis(30 * DAY),
        )
        .expect("policy")
    }

    fn facts() -> ArtifactTieringFacts {
        ArtifactTieringFacts {
            current: StorageTier::Hot,
            last_access_ms: Some(0),
            finalized_at_ms: Some(0),
            finalized: true,
            ownership_released: true,
        }
    }

    #[test]
    fn thresholds_are_inclusive_and_reads_never_promote() {
        for (now, expected) in [
            (5 * DAY - 1, None),
            (5 * DAY, Some(StorageTier::Light)),
            (30 * DAY - 1, Some(StorageTier::Light)),
            (30 * DAY, Some(StorageTier::Deep)),
        ] {
            assert_eq!(policy(true).transition(facts(), now), expected);
        }
        let mut current = facts();
        current.current = StorageTier::Light;
        assert_eq!(policy(true).transition(current, 5 * DAY), None);
        assert_eq!(
            policy(true).transition(current, 30 * DAY),
            Some(StorageTier::Deep)
        );
        current.current = StorageTier::Deep;
        current.last_access_ms = Some(30 * DAY);
        assert_eq!(policy(true).transition(current, 30 * DAY), None);
        assert_eq!(policy(true).transition(current, u64::MAX), None);
    }

    #[test]
    fn uncertain_active_or_disabled_candidates_defer() {
        let base = facts();
        for candidate in [
            ArtifactTieringFacts {
                last_access_ms: None,
                ..base
            },
            ArtifactTieringFacts {
                finalized_at_ms: None,
                ..base
            },
            ArtifactTieringFacts {
                finalized: false,
                ..base
            },
            ArtifactTieringFacts {
                ownership_released: false,
                ..base
            },
            ArtifactTieringFacts {
                last_access_ms: Some(u64::MAX),
                ..base
            },
            ArtifactTieringFacts {
                finalized_at_ms: Some(30 * DAY),
                ..base
            },
        ] {
            assert_eq!(policy(true).transition(candidate, 30 * DAY), None);
        }
        assert_eq!(policy(false).transition(base, u64::MAX), None);
    }

    #[test]
    fn invalid_thresholds_are_rejected_without_defaults() {
        for (light, deep) in [
            (Duration::ZERO, Duration::from_secs(1)),
            (Duration::from_secs(1), Duration::from_secs(1)),
            (Duration::from_secs(2), Duration::from_secs(1)),
            (Duration::from_nanos(1), Duration::from_secs(1)),
        ] {
            assert!(StorageTieringPolicy::new(false, light, deep).is_err());
        }
    }
}
