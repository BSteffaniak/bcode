//! Real-file tests of the scheduler's bounded maintenance pass.

use super::*;
use bcode_session_models::{
    SessionEventKind, ToolArtifact, ToolArtifactRef, ToolInvocationResult,
    ToolInvocationResultRecord,
};
use std::path::PathBuf;

#[tokio::test]
async fn daemon_maintenance_never_falls_back_to_offline_authority() {
    let (root, state, id, artifacts, _) = fixture(1).await;
    let registration = state
        .storage_daemon_registration
        .lock()
        .expect("lock")
        .take()
        .expect("registration");
    registration.finish().expect("clean empty registry");
    let future = super::super::current_time_ms() + 31 * 86_400_000;
    assert!(operation_cancellation(&state).is_err());
    assert!(
        maintain_session_at(&state, root.path(), id, None, future)
            .await
            .is_err()
    );
    drop(state);
    assert!(artifacts.join("recording-000").is_file());
}

#[tokio::test]
async fn maintenance_pass_ignores_unrelated_daemon() {
    let (root, state, id, artifacts, _) = fixture(2).await;
    let registry = bcode_session::storage_admission::StorageAdmissionRegistry::open(root.path())
        .expect("registry");
    let foreign = registry.register_daemon(SessionId::new()).expect("foreign");
    let future = super::super::current_time_ms() + 31 * 86_400_000;
    maintain_session_at(&state, root.path(), id, None, future)
        .await
        .expect("unrelated daemon allowed");
    assert!(artifacts.join("recording-000").is_dir());
    foreign.finish().expect("foreign drained");
    maintain_session_at(&state, root.path(), id, None, future)
        .await
        .expect("local admitted");
    assert!(artifacts.join("recording-000").is_dir());
    assert!(artifacts.join("recording-001").is_dir());
    state.fail_storage_tracking();
    assert!(operation_cancellation(&state).is_err());
    drop(state);
}

#[tokio::test]
async fn automatic_publication_recovers_abandoned_readers_but_not_active_readers() {
    let (root, state, id, artifacts, _) = fixture(1).await;
    let registry =
        bcode_session::storage_admission::StorageAdmissionRegistry::open_session(root.path(), id)
            .expect("registry");
    let participant = SessionId::new();
    let read = registry.admit_read(participant).expect("reader");
    let future = super::super::current_time_ms() + 31 * 86_400_000;
    maintain_session_at(&state, root.path(), id, None, future)
        .await
        .expect("deferred active candidate");
    assert!(artifacts.join("recording-000").is_file());
    drop(read);
    maintain_session_at(&state, root.path(), id, None, future)
        .await
        .expect("recover abandoned candidate");
    // This test's clock is 31 days ahead of real recovery time, so the reset age is eligible.
    assert!(artifacts.join("recording-000").is_dir());
    drop(state);
    assert!(registry.check_health(4096).is_ok());
}

#[tokio::test]
async fn automatic_publication_resumes_after_successful_reader_completion() {
    let (root, state, id, artifacts, _) = fixture(1).await;
    let registry =
        bcode_session::storage_admission::StorageAdmissionRegistry::open_session(root.path(), id)
            .expect("registry");
    let participant = SessionId::new();
    let read = registry.admit_read(participant).expect("reader");
    read.complete().expect("tracked reader");
    registry.retire(participant).expect("retire");
    let future = super::super::current_time_ms() + 31 * 86_400_000;
    maintain_session_at(&state, root.path(), id, None, future)
        .await
        .expect("eligible");
    drop(state);
    assert!(artifacts.join("recording-000").is_dir());
}

#[tokio::test]
async fn manual_sweep_continues_after_missing_artifact_through_history() {
    use bcode_session_models::{
        ArtifactCompressionFailureReason, StorageCompressionRequest, StorageCompressionTier,
    };
    let (root, state, id, artifacts, _) = fixture(2).await;
    std::fs::remove_file(artifacts.join("recording-000")).expect("missing fixture");
    state
        .sessions
        .append_event(
            id,
            SessionEventKind::SystemMessage {
                text: "compressible history ".repeat(10000),
            },
        )
        .await
        .expect("history");
    let history = state.sessions.session_history(id).await.expect("history");
    state
        .sessions
        .release_session_ownership(id)
        .await
        .expect("release");
    let mut request = StorageCompressionRequest {
        session_id: id,
        as_of_ms: crate::current_time_ms(),
        minimum_age_ms: None,
        tier: StorageCompressionTier::Light,
        dry_run: false,
        cursor: None,
    };
    let first = crate::session_compression::compress_page(&state, request.clone())
        .await
        .expect("missing");
    assert_eq!(first.failures, 1);
    assert_eq!(
        first.artifact_failure.expect("reason").reason,
        ArtifactCompressionFailureReason::ContentMissing
    );
    assert!(first.next.is_some());
    request.cursor = first.next;
    let mut history_saved = 0;
    loop {
        let page = crate::session_compression::compress_page(&state, request.clone())
            .await
            .expect("continue");
        assert_eq!(page.failures, 0);
        history_saved += page.history_payload_bytes_saved;
        request.cursor = page.next;
        if request.cursor.is_none() {
            break;
        }
    }
    assert!(history_saved > 0);
    assert!(!artifacts.join("recording-000").exists());
    assert!(artifacts.join("recording-001").is_dir());
    assert_eq!(
        state
            .sessions
            .session_history(id)
            .await
            .expect("preserved history"),
        history
    );
    drop(state);
    drop(root);
}

async fn fixture(count: usize) -> (tempfile::TempDir, ServerState, SessionId, PathBuf, Vec<u8>) {
    let root = tempfile::tempdir().expect("root");
    let sessions = bcode_session::SessionManager::persistent(root.path()).expect("manager");
    let summary = sessions
        .create_session(None, root.path().to_path_buf())
        .await
        .expect("session");
    let id = summary.id;
    let artifacts = root.path().join("session-artifacts").join(id.to_string());
    std::fs::create_dir_all(&artifacts).expect("artifacts");
    let bytes = "terminal 世界\n".repeat(20_000).into_bytes();
    let mut refs = Vec::new();
    for index in 0..count {
        let key = format!("recording-{index:03}");
        std::fs::write(artifacts.join(&key), &bytes).expect("raw");
        refs.push(ToolArtifactRef {
            key: key.clone(),
            content_type: Some("application/octet-stream".into()),
            storage_uri: Some(key),
            byte_len: Some(bytes.len() as u64),
            metadata: Some(serde_json::json!({"complete": true, "availability": "complete"})),
        });
    }
    sessions
        .append_event(
            id,
            SessionEventKind::ToolInvocationResultRecorded {
                record: ToolInvocationResultRecord {
                    invocation_id: "fixture".into(),
                    model_output: "done".into(),
                    is_error: false,
                    presentation: None,
                    content: vec![],
                    result: Some(ToolInvocationResult::Artifact {
                        artifact: Box::new(ToolArtifact {
                            artifact_id: "artifact".into(),
                            producer_plugin_id: "fixture".into(),
                            schema: "fixture".into(),
                            schema_version: 1,
                            tool_call_id: None,
                            title: None,
                            metadata: serde_json::Value::Null,
                            refs,
                        }),
                    }),
                },
            },
        )
        .await
        .expect("finalization");
    sessions
        .record_storage_access(
            id,
            bcode_session::storage_access::StorageAccessKind::History,
        )
        .await
        .expect("access");
    sessions
        .release_session_ownership(id)
        .await
        .expect("release");
    let state = crate::tests::test_server_state(sessions);
    crate::storage_read_admission::register_startup(&state, root.path().to_path_buf()).await;
    (root, state, id, artifacts, bytes)
}

#[tokio::test]
async fn scheduler_compresses_eligible_artifacts_and_continues_past_first_page() {
    let (root, state, id, artifacts, bytes) = fixture(18).await;
    let now = super::super::current_time_ms();
    assert_eq!(
        maintain_session_at(&state, root.path(), id, None, now)
            .await
            .expect("hot"),
        None
    );
    assert!(artifacts.join("recording-000").is_file());
    let future = now + 6 * 86_400_000;
    let cursor = maintain_session_at(&state, root.path(), id, None, future)
        .await
        .expect("first page");
    assert!(cursor.is_some());
    assert!(artifacts.join("recording-015").is_dir());
    assert!(artifacts.join("recording-016").is_file());
    let later_path = artifacts.join("later-recording");
    std::fs::write(&later_path, &bytes).expect("later artifact");
    let later = state.sessions.append_event(id, SessionEventKind::ToolInvocationResultRecorded {
        record: ToolInvocationResultRecord {
            invocation_id: "later".into(), model_output: "done".into(), is_error: false, presentation: None, content: vec![],
            result: Some(ToolInvocationResult::Artifact { artifact: Box::new(ToolArtifact {
                artifact_id: "z-later".into(), producer_plugin_id: "fixture".into(), schema: "fixture".into(), schema_version: 1,
                tool_call_id: None, title: None, metadata: serde_json::Value::Null,
                refs: vec![ToolArtifactRef { key: "recording".into(), content_type: None, storage_uri: Some("later-recording".into()), byte_len: Some(bytes.len() as u64), metadata: Some(serde_json::json!({"complete": true, "availability": "complete"})) }],
            }) }),
        },
    }).await.expect("new finalization");
    assert!(later.sequence > cursor.as_ref().expect("cursor").through_sequence);
    state
        .sessions
        .release_session_ownership(id)
        .await
        .expect("release");
    assert_eq!(
        maintain_session_at(&state, root.path(), id, cursor, future)
            .await
            .expect("second page"),
        None
    );
    assert!(
        later_path.is_file(),
        "current sweep excludes later finalization"
    );
    drop(state);
    for index in 0..18 {
        let path = artifacts.join(format!("recording-{index:03}"));
        assert!(path.is_dir());
        assert_eq!(
            bcode_session::artifact_storage::read_artifact_range(&artifacts, &path, 262_140, 80)
                .expect("transparent read")
                .1,
            bytes[262_140..262_220]
        );
    }
}

#[tokio::test]
async fn scheduler_recent_access_and_damage_defer_but_cold_content_reaches_deep_tier() {
    let (root, state, id, artifacts, bytes) = fixture(1).await;
    let future = super::super::current_time_ms() + 31 * 86_400_000;
    let access = root.path().join(id.to_string()).join("storage-access.bin");
    let mut tracking = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&access)
        .expect("tracking");
    bcode_session::storage_access::record_access(
        &mut tracking,
        bcode_session::storage_access::StorageAccessKind::Artifact,
        future,
    )
    .expect("recent access");
    drop(tracking);
    assert_eq!(
        maintain_session_at(&state, root.path(), id, None, future)
            .await
            .expect("recent"),
        None
    );
    assert!(artifacts.join("recording-000").is_file());
    std::fs::write(&access, b"damaged").expect("damage");
    assert!(
        maintain_session_at(&state, root.path(), id, None, future)
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(&access).expect("preserved"), b"damaged");
    assert!(artifacts.join("recording-000").is_file());
    // Explicit test-fixture reset, not runtime repair.
    let mut tracking = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .truncate(true)
        .open(&access)
        .expect("fixture reset");
    bcode_session::storage_access::record_access(
        &mut tracking,
        bcode_session::storage_access::StorageAccessKind::History,
        1,
    )
    .expect("old fixture");
    drop(tracking);
    assert_eq!(
        maintain_session_at(&state, root.path(), id, None, future)
            .await
            .expect("cold"),
        None
    );
    drop(state);
    let path = artifacts.join("recording-000");
    assert!(path.is_dir());
    let mut expected = std::io::Cursor::new(Vec::new());
    bcode_session::artifact_compression::encode_artifact(
        &mut bytes.as_slice(),
        &mut expected,
        bytes.len() as u64,
        ArtifactCompression::Deep,
        || Ok(()),
    )
    .expect("deep encoding");
    assert_eq!(
        std::fs::read(path.join("content.v1.zstd")).expect("published"),
        expected.into_inner()
    );
}

#[tokio::test]
async fn worker_compresses_and_preserves_continued_writes() {
    verify_worker_history(false).await;
}

#[tokio::test]
async fn application_upgrade_then_worker_preserves_history_and_writes() {
    verify_worker_history(true).await;
}

async fn upgrade_worker_fixture(state: &Arc<ServerState>, root: &std::path::Path, id: SessionId) {
    let db = bcode_session::db::SessionDb::open_existing_turso_in_root(id, root)
        .await
        .expect("fixture db");
    db.database()
        .update("session_storage_contract")
        .value("writer_epoch", 9)
        .execute(db.database())
        .await
        .expect("epoch nine fixture");
    db.database().close().await.expect("close fixture");
    drop(db);
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let snapshot = crate::session_operations::prepare_open(state, id)
                .await
                .unwrap_or_else(|_| panic!("application upgrade rejected"));
            if let Some(outcome) = snapshot.outcome {
                assert!(matches!(
                    outcome,
                    bcode_session_models::SessionOpenTerminalOutcome::Ready
                ));
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("upgrade completes");
    state
        .sessions
        .release_session_ownership(id)
        .await
        .expect("release upgraded session");
    let db = bcode_session::db::SessionDb::open_existing_turso_in_root(id, root)
        .await
        .expect("upgraded db");
    assert_eq!(db.storage_writer_epoch().await.expect("writer epoch"), 10);
    db.database().close().await.expect("close upgraded db");
}

async fn verify_worker_history(upgrade: bool) {
    let (root, mut state, id, artifacts, bytes) = fixture(1).await;
    state
        .startup_config
        .session_storage
        .maintenance_interval_secs = 1;
    for _ in 0..20 {
        state
            .sessions
            .append_event(
                id,
                SessionEventKind::SystemMessage {
                    text: "worker history 世界 ".repeat(20_000),
                },
            )
            .await
            .expect("append");
    }
    let expected = state.sessions.session_history(id).await.expect("history");
    state
        .sessions
        .release_session_ownership(id)
        .await
        .expect("release");
    let state = Arc::new(state);
    if upgrade {
        upgrade_worker_fixture(&state, root.path(), id).await;
    }
    let worker = tokio::spawn(run_with_clock(Arc::clone(&state), || {
        super::super::current_time_ms() + 31 * 86_400_000
    }));
    let completed = tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            if state
                .metrics
                .snapshot()
                .counters
                .get("storage.maintenance.history_payload_bytes_saved")
                .copied()
                .unwrap_or_default()
                > 1_048_576
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    state.request_shutdown();
    tokio::time::timeout(Duration::from_secs(10), worker)
        .await
        .expect("shutdown")
        .expect("worker");
    drop(state);
    completed.expect("worker compressed history");
    assert!(artifacts.join("recording-000").is_dir());
    assert_eq!(
        bcode_session::artifact_storage::read_artifact_range(
            &artifacts,
            &artifacts.join("recording-000"),
            0,
            u32::try_from(bytes.len()).expect("fixture length"),
        )
        .expect("transparent artifact read")
        .1,
        bytes,
    );
    let sessions = bcode_session::SessionManager::persistent(root.path()).expect("reopen");
    assert_eq!(
        sessions.session_history(id).await.expect("preserved"),
        expected
    );
    sessions
        .append_event(
            id,
            SessionEventKind::SystemMessage {
                text: "continued after automatic compression".into(),
            },
        )
        .await
        .expect("continued write");
    assert_eq!(
        sessions.session_history(id).await.expect("history").len(),
        expected.len() + 1
    );
    sessions
        .release_session_ownership(id)
        .await
        .expect("release");
}

#[tokio::test]
async fn constructed_server_starts_storage_worker_and_releases_registration() {
    let (root, state, id, artifacts, _) = fixture(1).await;
    state
        .storage_daemon_registration
        .lock()
        .expect("registration lock")
        .take()
        .expect("fixture registration")
        .finish()
        .expect("retire fixture");
    let access = root.path().join(id.to_string()).join("storage-access.bin");
    std::fs::remove_file(&access).expect("remove tracking");
    let socket_dir = tempfile::tempdir().expect("socket directory");
    let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket_dir.path().join("storage.sock"));
    let listener = crate::LocalIpcListener::bind(&endpoint).expect("listener");
    let state = Arc::new(state);
    let server_state = Arc::clone(&state);
    let shutdown = bcode_agent_runtime::CancellationToken::new();
    let signal = shutdown.clone();
    let server = tokio::spawn(async move {
        crate::run_constructed_server(
            server_state,
            listener,
            &bcode_config::DaemonConfig::default(),
            false,
            &[],
            shutdown,
            None,
        )
        .await
    });
    let dispatched = tokio::time::timeout(Duration::from_secs(15), async {
        while !access.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    signal.cancel();
    tokio::time::timeout(Duration::from_secs(15), server)
        .await
        .expect("bounded shutdown")
        .expect("server task")
        .expect("server");
    drop(state);
    dispatched.expect("startup launched storage worker");
    assert!(artifacts.join("recording-000").is_file());
    let registry = bcode_session::storage_admission::StorageAdmissionRegistry::open(root.path())
        .expect("registry");
    let _maintenance = registry
        .admit_maintenance(4096)
        .expect("clean shutdown registration");
}

#[tokio::test]
async fn worker_dispatches_tracking_initialization_and_stops() {
    let (root, state, id, artifacts, _) = fixture(1).await;
    let access = root.path().join(id.to_string()).join("storage-access.bin");
    std::fs::remove_file(&access).expect("remove tracking");
    let state = Arc::new(state);
    let worker = tokio::spawn(run(Arc::clone(&state)));
    let dispatched = tokio::time::timeout(Duration::from_secs(10), async {
        while !access.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    state.request_shutdown();
    tokio::time::timeout(Duration::from_secs(2), worker)
        .await
        .expect("bounded shutdown")
        .expect("worker");
    drop(state);
    dispatched.expect("worker dispatched without bypassing readiness");
    assert!(artifacts.join("recording-000").is_file());
}

#[tokio::test]
async fn readiness_requires_registration_and_healthy_tracking() {
    let (_root, state, _id, _artifacts, _) = fixture(1).await;
    assert!(tracking_readiness(&state));
    state
        .storage_tracking_failed
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(!tracking_readiness(&state));
    state
        .storage_tracking_failed
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let registration = state
        .storage_daemon_registration
        .lock()
        .expect("lock")
        .take();
    assert!(!tracking_readiness(&state));
    drop(registration);
}

#[tokio::test]
async fn worker_shutdown_before_first_poll_starts_no_compression() {
    let (root, state, id, artifacts, _) = fixture(1).await;
    let state = Arc::new(state);
    state.request_shutdown();
    tokio::time::timeout(Duration::from_secs(2), run(Arc::clone(&state)))
        .await
        .expect("worker stops");
    drop(state);
    assert!(artifacts.join("recording-000").is_file());
    assert!(
        !artifacts
            .join(".recording-000.compression-pending")
            .exists()
    );
    assert!(
        root.path()
            .join(id.to_string())
            .join("session.db")
            .is_file()
    );
}

#[tokio::test]
async fn worker_waiting_between_ticks_stops_on_shutdown() {
    let (_root, state, _id, artifacts, _) = fixture(1).await;
    let state = Arc::new(state);
    let worker = tokio::spawn(run(Arc::clone(&state)));
    tokio::task::yield_now().await;
    state.request_shutdown();
    tokio::time::timeout(Duration::from_secs(2), worker)
        .await
        .expect("bounded shutdown")
        .expect("worker");
    drop(state);
    assert!(artifacts.join("recording-000").is_file());
}

#[tokio::test]
async fn missing_tracking_initializes_once_without_compressing_or_refreshing_age() {
    let (root, state, id, artifacts, _) = fixture(1).await;
    let access = root.path().join(id.to_string()).join("storage-access.bin");
    std::fs::remove_file(&access).expect("remove fixture tracking");
    let now = super::super::current_time_ms();
    assert_eq!(
        maintain_session_at(&state, root.path(), id, None, now)
            .await
            .expect("initialize"),
        None
    );
    let initial = std::fs::read(&access).expect("initialized");
    assert!(artifacts.join("recording-000").is_file());
    assert_eq!(
        maintain_session_at(&state, root.path(), id, None, now + 86_400_000)
            .await
            .expect("young"),
        None
    );
    assert_eq!(std::fs::read(&access).expect("unchanged"), initial);
    assert_eq!(
        maintain_session_at(&state, root.path(), id, None, now + 6 * 86_400_000)
            .await
            .expect("eligible"),
        None
    );
    drop(state);
    assert!(artifacts.join("recording-000").is_dir());
}

#[tokio::test]
async fn scheduler_respects_disable_and_live_ownership() {
    let (root, state, id, artifacts, _) = fixture(1).await;
    let mut config = bcode_config::BcodeConfig::default();
    config.session_storage.enabled = false;
    state.session_configs.lock().await.insert(id, config);
    let future = super::super::current_time_ms() + 31 * 86_400_000;
    assert_eq!(
        maintain_session_at(&state, root.path(), id, None, future)
            .await
            .expect("disabled"),
        None
    );
    assert!(artifacts.join("recording-000").is_file());
    state.session_configs.lock().await.remove(&id);
    let owner = state
        .sessions
        .acquire_session_ownership(id, bcode_session::SessionOwnershipKind::RuntimeWork)
        .await
        .expect("owner");
    assert!(
        maintain_session_at(&state, root.path(), id, None, future)
            .await
            .is_err()
    );
    drop(owner);
    drop(state);
    assert!(artifacts.join("recording-000").is_file());
}
