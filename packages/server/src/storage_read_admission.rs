//! Application-owned lifecycle for registered storage reads.
//!
//! Persistent reads register against the target session before consuming content. Registration
//! failures reject the read; they never authorize an unregistered fallback.

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
    /// Acquire session-scoped admission before content access.
    ///
    /// In-memory sessions require no persistent admission. Persistent registration failures return
    /// an error before content can be consumed, without poisoning unrelated sessions.
    ///
    /// # Errors
    /// Returns a retryable storage error when registration cannot be acquired.
    pub async fn for_session(
        state: &super::ServerState,
        session_id: SessionId,
    ) -> Result<Option<Self>, bcode_session::SessionError> {
        let Some(root) = state.sessions.session_store_root() else {
            return Ok(None);
        };
        // Register before lookup: existence can change, and metadata failure is not proof that
        // this operation cannot consume persistent content.
        Self::begin(root, session_id).await.map(Some).map_err(|_| {
            bcode_session::db::SessionDbError::Io(io::Error::new(
                io::ErrorKind::WouldBlock,
                "session storage admission unavailable; retry after coordination recovers",
            ))
            .into()
        })
    }

    /// Persist access before completing admission. Failure leaves a durable dirty participant.
    pub async fn finish_history(self, state: &super::ServerState, session_id: SessionId) {
        self.finish_consumption(
            state,
            session_id,
            bcode_session::storage_access::StorageAccessKind::History,
        )
        .await;
    }

    /// Persist the category's access timestamp before retiring a successful read participant.
    pub async fn finish_consumption(
        self,
        state: &super::ServerState,
        session_id: SessionId,
        kind: bcode_session::storage_access::StorageAccessKind,
    ) {
        if state
            .sessions
            .record_storage_access(session_id, kind)
            .await
            .is_ok()
        {
            if self.complete().await.is_err() {
                state.fail_storage_tracking();
                tracing::warn!("storage read participant retirement deferred");
            }
        } else {
            state.fail_storage_tracking();
            tracing::warn!("storage access tracking failed; participant remains dirty");
        }
    }

    /// Retire an ordinary failed read after all its content work has finished.
    ///
    /// No content was returned to the caller, so access tracking is unnecessary. Cancellation
    /// must not call this: dropping admission continues to preserve dirty crash evidence.
    ///
    /// # Errors
    /// Returns the original read error unchanged. Retirement failures preserve evidence and
    /// are logged rather than replacing the operation's error.
    pub async fn finish_failed<T, E>(
        admission: &mut Option<Self>,
        result: Result<T, E>,
    ) -> Result<T, E> {
        if result.is_err()
            && let Some(admission) = admission.take()
            && admission.complete().await.is_err()
        {
            tracing::warn!("failed storage read participant retirement deferred");
        }
        result
    }

    /// Register one operation without running blocking lock/sync calls on the async executor.
    ///
    /// # Errors
    /// Returns contention or registry IO/compatibility failures. No content may have been exposed
    /// under this admission if it fails; the caller decides whether to defer optional maintenance.
    pub async fn begin(root: PathBuf, session_id: SessionId) -> io::Result<Self> {
        tokio::task::spawn_blocking(move || {
            let registry = StorageAdmissionRegistry::open_session(&root, session_id)?;
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

    /// Finish after durable access tracking succeeds; retire while shared admission is still held.
    ///
    /// A failed/cancelled content operation should drop this guard, retaining dirty evidence.
    /// Other active readers do not prevent completion or cause clean participant accumulation.
    ///
    /// # Errors
    /// Returns tracking-state, sync, lock contention, or retirement failures.
    pub async fn complete(self) -> io::Result<()> {
        tokio::task::spawn_blocking(move || {
            self.registry
                .complete_read(self.participant, self.admission)
        })
        .await
        .map_err(|_| io::Error::other("storage read completion task failed"))?
    }
}

/// Register before startup services can consume session content.
/// Registration failure disables optional maintenance without preventing unrelated startup.
pub(super) async fn register_startup(state: &super::ServerState, root: PathBuf) {
    let fallback_root = root.clone();
    let queue = bcode_metrics::startup::phase("storage_admission.blocking_queue_wait");
    let registered = tokio::task::spawn_blocking(move || {
        queue.finish();
        let registry = bcode_metrics::startup::measure("storage_admission.registry_open", || {
            StorageAdmissionRegistry::open(&root)
        })?;
        if bcode_metrics::startup::measure("storage_admission.retire_completed_daemons", || {
            registry.retire_completed_daemons(4096)
        })
        .is_err()
        {
            tracing::debug!(
                "completed storage registrations could not be retired; preserving registry"
            );
        }
        bcode_metrics::startup::measure("storage_admission.register_daemon", || {
            registry.register_daemon(SessionId::new())
        })
    })
    .await;
    if let Ok(Ok(registration)) = registered {
        *state
            .storage_daemon_registration
            .lock()
            .expect("storage registration lock") = Some(registration);
    } else {
        block_unregistered_reads(state, fallback_root).await;
        tracing::warn!("storage daemon registration unavailable; maintenance is disabled");
    }
}

/// Disable maintenance before allowing reads without daemon or operation registration.
///
/// Failure to persist the blocker is not a compatibility proof: the global dispatch gate must
/// remain closed until an independent fence covers these readers.
pub(super) async fn block_unregistered_reads(state: &super::ServerState, root: PathBuf) -> bool {
    state.fail_storage_tracking();
    let blocked = tokio::task::spawn_blocking(move || {
        StorageAdmissionRegistry::open(&root)?.block_maintenance_for_fallback()
    })
    .await;
    if matches!(blocked, Ok(Ok(()))) {
        tracing::warn!("unregistered storage reads durably disabled maintenance");
        true
    } else {
        tracing::warn!("storage fallback fence unavailable; automatic scheduling remains disabled");
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn repeated_missing_artifact_reads_retire_admission_without_changing_history() {
        let root = tempfile::tempdir().expect("root");
        let sessions = bcode_session::SessionManager::persistent(root.path()).expect("manager");
        let session = sessions
            .create_session(None, root.path().to_path_buf())
            .await
            .expect("session");
        let before = sessions.session_history(session.id).await.expect("history");
        let state = crate::tests::test_server_state(sessions);
        let registry =
            StorageAdmissionRegistry::open_session(root.path(), session.id).expect("registry");
        for _ in 0..32 {
            assert!(
                crate::read_session_artifact_range(
                    &state,
                    session.id,
                    "missing",
                    "recording",
                    0,
                    64,
                )
                .await
                .is_err()
            );
            drop(
                registry
                    .admit_maintenance(1)
                    .expect("only the gate remains"),
            );
        }
        assert_eq!(
            state
                .sessions
                .session_history(session.id)
                .await
                .expect("history"),
            before
        );
        drop(state);
    }

    #[tokio::test]
    async fn failed_history_lookup_retires_but_dropped_read_preserves_evidence() {
        let root = tempfile::tempdir().expect("root");
        let sessions = bcode_session::SessionManager::persistent(root.path()).expect("manager");
        let state = crate::tests::test_server_state(sessions);
        let id = SessionId::new();
        for _ in 0..16 {
            assert!(
                crate::session_operations::complete_history(
                    &state,
                    bcode_session_models::ClientId::new(),
                    id,
                )
                .await
                .is_err()
            );
            let registry =
                StorageAdmissionRegistry::open_session(root.path(), id).expect("registry");
            drop(
                registry
                    .admit_maintenance(1)
                    .expect("failed lookup retired"),
            );
        }
        drop(
            RegisteredStorageRead::begin(root.path().to_path_buf(), id)
                .await
                .expect("read"),
        );
        let registry = StorageAdmissionRegistry::open_session(root.path(), id).expect("registry");
        assert!(registry.admit_maintenance(16).is_err());
    }

    #[tokio::test]
    async fn maintenance_contention_rejects_read_until_authority_is_released() {
        let root = tempfile::tempdir().expect("root");
        let sessions = bcode_session::SessionManager::persistent(root.path()).expect("manager");
        let session = sessions
            .create_session(None, root.path().to_path_buf())
            .await
            .expect("create");
        let expected = sessions.session_history(session.id).await.expect("history");
        sessions
            .release_session_ownership(session.id)
            .await
            .expect("release");
        let before = bcode_session::storage_access::observe_session_access(root.path(), session.id)
            .expect("access");
        let registry =
            StorageAdmissionRegistry::open_session(root.path(), session.id).expect("registry");
        let maintenance = registry.admit_maintenance(16).expect("maintenance");
        let state = crate::tests::test_server_state(sessions);
        let client = bcode_session_models::ClientId::new();
        assert!(
            crate::session_operations::complete_history(&state, client, session.id)
                .await
                .is_err()
        );
        assert_eq!(
            bcode_session::storage_access::observe_session_access(root.path(), session.id)
                .expect("access unchanged"),
            before
        );
        drop(maintenance);
        assert_eq!(
            crate::session_operations::complete_history(&state, client, session.id)
                .await
                .expect("retry after release"),
            expected
        );
        assert!(
            !state
                .storage_tracking_failed
                .load(std::sync::atomic::Ordering::SeqCst)
        );
        drop(state);
    }

    #[tokio::test]
    async fn double_failure_rejects_history_and_retry_preserves_content() {
        let root = tempfile::tempdir().expect("root");
        let sessions = bcode_session::SessionManager::persistent(root.path()).expect("manager");
        let session = sessions
            .create_session(None, root.path().to_path_buf())
            .await
            .expect("create");
        sessions
            .append_event(
                session.id,
                bcode_session_models::SessionEventKind::SystemMessage {
                    text: "history must remain readable after coordination recovers".into(),
                },
            )
            .await
            .expect("append");
        let expected = sessions.session_history(session.id).await.expect("history");
        sessions
            .release_session_ownership(session.id)
            .await
            .expect("release");
        let before = bcode_session::storage_access::observe_session_access(root.path(), session.id)
            .expect("access");
        let obstruction = root.path().join("storage-admission-sessions-v1");
        std::fs::write(&obstruction, b"unavailable registry").expect("obstruct");
        let state = crate::tests::test_server_state(sessions);
        let client = bcode_session_models::ClientId::new();
        assert!(
            crate::session_operations::complete_history(&state, client, session.id)
                .await
                .is_err()
        );
        assert_eq!(
            bcode_session::storage_access::observe_session_access(root.path(), session.id)
                .expect("unchanged access"),
            before
        );
        assert_eq!(
            std::fs::read(&obstruction).expect("preserved"),
            b"unavailable registry"
        );
        std::fs::remove_file(&obstruction).expect("remove test obstruction");
        assert_eq!(
            crate::session_operations::complete_history(&state, client, session.id)
                .await
                .expect("retry"),
            expected
        );
        assert!(
            !state
                .storage_tracking_failed
                .load(std::sync::atomic::Ordering::SeqCst)
        );
        drop(state);
    }

    #[tokio::test]
    async fn missing_session_still_registers_before_storage_lookup() {
        let root = tempfile::tempdir().expect("root");
        let sessions = bcode_session::SessionManager::persistent(root.path()).expect("manager");
        let state = crate::tests::test_server_state(sessions);
        let id = SessionId::new();
        let read = RegisteredStorageRead::for_session(&state, id)
            .await
            .expect("admission available")
            .expect("admitted");
        let registry = StorageAdmissionRegistry::open_session(root.path(), id).expect("registry");
        assert!(registry.admit_maintenance(16).is_err());
        read.complete().await.expect("complete empty lookup");
        drop(state);
        drop(registry.admit_maintenance(16).expect("released"));
        assert!(!root.path().join(id.to_string()).exists());
    }

    #[tokio::test]
    async fn startup_registration_blocks_foreign_maintenance_until_shutdown() {
        let root = tempfile::tempdir().expect("root");
        let state = crate::tests::test_server_state(bcode_session::SessionManager::default());
        register_startup(&state, root.path().to_path_buf()).await;
        let registry = StorageAdmissionRegistry::open(root.path()).expect("registry");
        assert!(registry.admit_maintenance(16).is_err());
        assert!(
            !state
                .storage_tracking_failed
                .load(std::sync::atomic::Ordering::SeqCst)
        );
        state.request_shutdown();
        drop(state);
        drop(registry.admit_maintenance(16).expect("clean shutdown"));
    }

    #[tokio::test]
    async fn unregistered_startup_persists_blocker_across_restart() {
        let root = tempfile::tempdir().expect("root");
        let state = crate::tests::test_server_state(bcode_session::SessionManager::default());
        block_unregistered_reads(&state, root.path().to_path_buf()).await;
        assert!(
            state
                .storage_tracking_failed
                .load(std::sync::atomic::Ordering::SeqCst)
        );
        state.request_shutdown();
        drop(state);
        let registry = StorageAdmissionRegistry::open(root.path()).expect("restart");
        assert!(registry.admit_maintenance(16).is_err());
    }

    #[tokio::test]
    async fn unavailable_fallback_keeps_local_tracking_failed() {
        let root = tempfile::NamedTempFile::new().expect("not a directory");
        let state = crate::tests::test_server_state(bcode_session::SessionManager::default());
        register_startup(&state, root.path().to_path_buf()).await;
        assert!(
            state
                .storage_tracking_failed
                .load(std::sync::atomic::Ordering::SeqCst)
        );
        drop(state);
        assert_eq!(std::fs::metadata(root.path()).expect("preserved").len(), 0);
    }

    #[tokio::test]
    async fn healthy_shutdown_cleans_only_after_last_state_owner_releases() {
        let root = tempfile::tempdir().expect("root");
        let registry = StorageAdmissionRegistry::open(root.path()).expect("registry");
        let state = std::sync::Arc::new(crate::tests::test_server_state(
            bcode_session::SessionManager::default(),
        ));
        *state.storage_daemon_registration.lock().expect("lock") =
            Some(registry.register_daemon(SessionId::new()).expect("daemon"));
        let outstanding = std::sync::Arc::clone(&state);
        state.request_shutdown();
        drop(state);
        assert!(registry.admit_maintenance(16).is_err());
        drop(outstanding);
        drop(
            registry
                .admit_maintenance(16)
                .expect("clean after final release"),
        );
        assert_eq!(registry.retire_completed_daemons(16).expect("retire"), 1);
    }

    #[tokio::test]
    async fn shutdown_with_outstanding_operation_token_leaves_dirty_record() {
        let root = tempfile::tempdir().expect("root");
        let registry = StorageAdmissionRegistry::open(root.path()).expect("registry");
        let state = crate::tests::test_server_state(bcode_session::SessionManager::default());
        let daemon = registry.register_daemon(SessionId::new()).expect("daemon");
        let token = daemon.acknowledgement().expect("token");
        *state.storage_daemon_registration.lock().expect("lock") = Some(daemon);
        state.request_shutdown();
        drop(state);
        assert!(token.check().is_err());
        drop(token);
        assert!(registry.admit_maintenance(16).is_err());
    }

    #[tokio::test]
    async fn tracking_failure_invalidates_live_acknowledgement_and_survives_restart() {
        let root = tempfile::tempdir().expect("root");
        let registry = StorageAdmissionRegistry::open(root.path()).expect("registry");
        let registration = registry
            .register_daemon(SessionId::new())
            .expect("registration");
        let state = crate::tests::test_server_state(bcode_session::SessionManager::default());
        *state.storage_daemon_registration.lock().expect("lock") = Some(registration);
        {
            let mut held = state.storage_daemon_registration.lock().expect("lock");
            drop(
                registry
                    .admit_acknowledged(16, held.as_mut().expect("registered"))
                    .expect("healthy acknowledgement"),
            );
        }
        state.fail_storage_tracking();
        assert!(
            state
                .storage_tracking_failed
                .load(std::sync::atomic::Ordering::SeqCst)
        );
        {
            let mut held = state.storage_daemon_registration.lock().expect("lock");
            assert!(!held.as_ref().expect("registered").healthy());
            assert!(
                registry
                    .admit_acknowledged(16, held.as_mut().expect("registered"))
                    .is_err()
            );
            drop(held);
        }
        drop(state);
        drop(registry);
        let reopened = StorageAdmissionRegistry::open(root.path()).expect("restart");
        assert!(reopened.admit_maintenance(16).is_err());
    }

    #[tokio::test]
    async fn registered_read_lifecycle_retires_success_but_preserves_abandonment() {
        let root = tempfile::tempdir().expect("root");
        let id = SessionId::new();
        let read = RegisteredStorageRead::begin(root.path().to_path_buf(), id)
            .await
            .expect("registered");
        let registry = StorageAdmissionRegistry::open_session(root.path(), id).expect("registry");
        assert!(registry.admit_maintenance(10).is_err());
        read.complete().await.expect("complete");
        drop(registry.admit_maintenance(1).expect("empty clean registry"));
        drop(
            RegisteredStorageRead::begin(root.path().to_path_buf(), id)
                .await
                .expect("abandoned"),
        );
        assert!(registry.admit_maintenance(10).is_err());
    }
}
