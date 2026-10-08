//! Content-free, ephemeral session progress observations, reusable outside workflows.

use serde::{Deserialize, Serialize};

/// Compatibility version of the independent session progress snapshot contract.
pub const SESSION_PROGRESS_VERSION: u32 = 2;

/// A non-owning observation, not a durable checkpoint or proof of subtree coverage.
///
/// Consumers must reject unsupported versions. An available marker measures only
/// progress delivered by the host's recognized-progress hooks; it does not certify
/// hook coverage, session ownership, or discovery completeness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionProgressSnapshot {
    /// Independent contract version.
    pub version: u32,
    /// Exact queried session identity, never an aggregate subtree identity.
    pub session_id: String,
    /// Content-free live observation; absence is never an idle marker.
    pub observation: SessionProgressObservation,
}

/// Ephemeral observation continuity and substantive-progress count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionProgressObservation {
    /// At least one recognized progress signal was observed and retained.
    Available {
        /// Opaque continuity identity, replaced on restart, eviction, or overflow.
        epoch: String,
        /// Increases only when the host delivers recognized substantive progress.
        counter: u64,
        /// Actual host time of the last accepted substantive signal, never a poll timestamp.
        /// None means host time was unavailable; counters remain the continuity authority.
        last_progress_at_ms: Option<u64>,
    },
    /// Silence cannot be distinguished from missing observation coverage.
    Unavailable {
        /// Normalized reason without content or private diagnostic details.
        reason: SessionProgressUnavailableReason,
    },
}

/// Why a progress baseline cannot be compared safely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionProgressUnavailableReason {
    /// No retained observation exists; includes eviction without unbounded tombstones.
    NotObservedOrEvicted,
    /// The serving host cannot verify that its live hooks cover this session.
    CoverageUnavailable,
}

impl SessionProgressSnapshot {
    /// Compare same-session, same-epoch observations of this supported version.
    ///
    /// `Some(true)` means verified counter advancement, `Some(false)` means an
    /// unchanged marker, and `None` means unknown continuity. An unchanged marker
    /// alone is not proof of idleness: discovery and hook coverage remain separate.
    #[must_use]
    pub fn advanced_since(&self, previous: &Self) -> Option<bool> {
        if self.version != SESSION_PROGRESS_VERSION
            || previous.version != SESSION_PROGRESS_VERSION
            || self.session_id != previous.session_id
        {
            return None;
        }
        match (&self.observation, &previous.observation) {
            (
                SessionProgressObservation::Available { epoch, counter, .. },
                SessionProgressObservation::Available {
                    epoch: previous_epoch,
                    counter: previous_counter,
                    ..
                },
            ) if epoch == previous_epoch && counter >= previous_counter => {
                Some(counter > previous_counter)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(epoch: &str, counter: u64) -> SessionProgressSnapshot {
        SessionProgressSnapshot {
            version: SESSION_PROGRESS_VERSION,
            session_id: "session".into(),
            observation: SessionProgressObservation::Available {
                epoch: epoch.into(),
                counter,
                last_progress_at_ms: Some(123),
            },
        }
    }

    #[test]
    fn compares_only_verified_monotonic_continuity() {
        let baseline = snapshot("first", 2);
        assert_eq!(baseline.advanced_since(&baseline), Some(false));
        assert_eq!(snapshot("first", 3).advanced_since(&baseline), Some(true));
        assert_eq!(snapshot("first", 1).advanced_since(&baseline), None);
        assert_eq!(snapshot("second", 3).advanced_since(&baseline), None);
        let mut other = snapshot("first", 3);
        other.session_id = "other".into();
        assert_eq!(other.advanced_since(&baseline), None);
        other = snapshot("first", 3);
        other.version += 1;
        assert_eq!(other.advanced_since(&baseline), None);
        assert_eq!(baseline.advanced_since(&other), None);
    }

    #[test]
    fn missing_observation_never_compares_as_idle_or_progress() {
        let baseline = snapshot("first", 2);
        let unavailable = SessionProgressSnapshot {
            observation: SessionProgressObservation::Unavailable {
                reason: SessionProgressUnavailableReason::NotObservedOrEvicted,
            },
            ..baseline.clone()
        };
        assert_eq!(unavailable.advanced_since(&baseline), None);
        assert_eq!(baseline.advanced_since(&unavailable), None);
        assert_eq!(unavailable.advanced_since(&unavailable), None);
    }

    #[test]
    fn serializes_without_content_and_rejects_unknown_observation_variants() {
        let value = snapshot("first", 2);
        let encoded = serde_json::to_value(&value).unwrap();
        assert_eq!(
            encoded,
            serde_json::json!({
                "version": SESSION_PROGRESS_VERSION,
                "session_id": "session",
                "observation": {"status": "available", "epoch": "first", "counter": 2, "last_progress_at_ms": 123}
            })
        );
        assert_eq!(
            serde_json::from_value::<SessionProgressSnapshot>(encoded).unwrap(),
            value
        );
        assert!(
            serde_json::from_value::<SessionProgressSnapshot>(serde_json::json!({
                "version": SESSION_PROGRESS_VERSION,
                "session_id": "session",
                "observation": {"status": "future"}
            }))
            .is_err()
        );
    }
}
