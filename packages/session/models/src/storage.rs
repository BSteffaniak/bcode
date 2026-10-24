//! Portable physical session-storage measurements.

use serde::{Deserialize, Serialize};

/// Filesystem bytes for one storage category, not database row or logical content bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStorageBytes {
    /// Number of regular files measured. Hard links are counted per directory entry.
    pub files: u64,
    /// Sum of file lengths, including sparse extents.
    pub file_bytes: u64,
    /// Allocated filesystem bytes, when the platform exposes this measurement.
    pub allocated_bytes: Option<u64>,
}

/// Disposable, secret-safe observation of daemon-owned background storage maintenance.
///
/// This is not a recovery receipt or a promise of durable progress. Missing per-session details
/// may mean the worker has not visited the session or its bounded observation was evicted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageMaintenanceStatus {
    /// Whether startup policy permits background scheduling.
    pub enabled: bool,
    /// Whether the daemon's maintenance task is currently running.
    pub worker_running: bool,
    /// Most recent observed attempt in Unix milliseconds.
    pub last_attempt_ms: Option<u64>,
    /// Normalized outcome, without filesystem paths or raw backend errors.
    pub outcome: Option<String>,
    /// Normalized eligibility or deferral reason.
    pub reason: Option<String>,
    /// Earliest scheduled retry in Unix milliseconds, when known.
    pub next_retry_ms: Option<u64>,
}

/// A bounded, non-transactional observation of one session's storage.
///
/// Versioned by the enclosing application protocol. This is diagnostic information, not
/// canonical validation or an estimate of achievable compression savings. Files may change during
/// measurement. Search provider indexes and the global catalog are not session-local files and are
/// excluded; database bytes include both canonical events and derived projections.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStorageUsage {
    /// Canonical database and its database-engine sidecars.
    pub database: SessionStorageBytes,
    /// Session-owned terminal recordings, tool output, and other artifact files.
    pub artifacts: SessionStorageBytes,
    /// Other session-local files, including ownership metadata.
    pub other: SessionStorageBytes,
    /// Retained migration recovery copies outside the canonical session directory.
    ///
    /// These consume additional space; compression savings do not imply net savings while a
    /// recovery copy remains retained. Measurement shares the overall traversal budget.
    #[serde(default)]
    pub retained_backups: SessionStorageBytes,
    /// Whether migration-owned discovery found every retained backup for this session.
    ///
    /// `None` means external backup discovery was not performed. `Some(false)` means reported
    /// backup bytes are a lower bound, independently of canonical file traversal completeness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_discovery_complete: Option<bool>,
    /// Disposable daemon maintenance observation, when supplied by the application.
    ///
    /// Absence means no observation is available, not that maintenance is disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintenance: Option<StorageMaintenanceStatus>,
    /// Directory entries visited, including directories and skipped entries.
    pub visited_entries: u32,
    /// Entries that were not safe or possible to inspect. Their contents are excluded.
    pub skipped_entries: u32,
    /// Whether traversal exhausted the requested entry budget before finishing.
    pub budget_exhausted: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn historical_storage_observation_has_no_invented_maintenance_state() {
        let value = serde_json::json!({
            "database": { "files": 1, "file_bytes": 4096, "allocated_bytes": null },
            "artifacts": { "files": 0, "file_bytes": 0, "allocated_bytes": null },
            "other": { "files": 0, "file_bytes": 0, "allocated_bytes": null },
            "visited_entries": 1,
            "skipped_entries": 0,
            "budget_exhausted": false
        });
        let observation: SessionStorageUsage = serde_json::from_value(value).expect("decode");
        assert_eq!(observation.retained_backups, SessionStorageBytes::default());
        assert!(observation.maintenance.is_none());
    }

    #[test]
    fn maintenance_and_backup_cost_survive_storage_observation_round_trip() {
        let observation = SessionStorageUsage {
            retained_backups: SessionStorageBytes {
                files: 3,
                file_bytes: 8192,
                allocated_bytes: Some(12_288),
            },
            maintenance: Some(StorageMaintenanceStatus {
                enabled: true,
                worker_running: true,
                last_attempt_ms: Some(100),
                outcome: Some("deferred".into()),
                reason: Some("busy".into()),
                next_retry_ms: Some(200),
            }),
            ..Default::default()
        };
        let decoded: SessionStorageUsage =
            serde_json::from_slice(&serde_json::to_vec(&observation).expect("encode"))
                .expect("decode");
        assert_eq!(decoded, observation);
    }
}
