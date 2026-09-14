//! Application-owned lifecycle for registered storage reads.
//!
//! This adapter is deliberately not enabled until all relevant read entry points can acquire it
//! before exposing content. Errors must not be interpreted as permission to schedule from stale age.

use bcode_session::storage_admission::{StorageAdmissionRegistry, StorageReadAdmission};
use bcode_session_models::SessionId;
use std::io;
use std::path::PathBuf;

/// An operation registered durably before content access, retained through access persistence.
pub struct RegisteredStorageRead {
    registry: StorageAdmissionRegistry,
    participant: SessionId,
    admission: StorageReadAdmission,
}

impl RegisteredStorageRead {
    /// Register one operation without running blocking lock/sync calls on the async executor.
    ///
    /// # Errors
    /// Returns contention or registry IO/compatibility failures. No content may have been exposed
    /// under this admission if it fails; the caller decides whether to defer optional maintenance.
    pub async fn begin(root: PathBuf) -> io::Result<Self> {
        tokio::task::spawn_blocking(move || {
            let registry = StorageAdmissionRegistry::open(&root)?;
            let participant = SessionId::new();
            let admission = registry.admit_read(participant)?;
            Ok(Self {
                registry,
                participant,
                admission,
            })
        })
        .await
        .map_err(|_| io::Error::other("storage read registration task failed"))?
    }

    /// Finish after durable access tracking succeeds; release and retire the clean participant.
    ///
    /// A failed/cancelled content operation should drop this guard, retaining dirty evidence.
    /// Retirement contention preserves a clean record and is reported instead of ignored.
    ///
    /// # Errors
    /// Returns tracking-state, sync, lock contention, or retirement failures.
    pub async fn complete(self) -> io::Result<()> {
        tokio::task::spawn_blocking(move || {
            self.admission.complete()?;
            self.registry.retire(self.participant)
        })
        .await
        .map_err(|_| io::Error::other("storage read completion task failed"))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn registered_read_lifecycle_retires_success_but_preserves_abandonment() {
        let root = tempfile::tempdir().expect("root");
        let read = RegisteredStorageRead::begin(root.path().to_path_buf())
            .await
            .expect("registered");
        let registry = StorageAdmissionRegistry::open(root.path()).expect("registry");
        assert!(registry.admit_maintenance(10).is_err());
        read.complete().await.expect("complete");
        drop(registry.admit_maintenance(1).expect("empty clean registry"));
        drop(
            RegisteredStorageRead::begin(root.path().to_path_buf())
                .await
                .expect("abandoned"),
        );
        assert!(registry.admit_maintenance(10).is_err());
    }
}
