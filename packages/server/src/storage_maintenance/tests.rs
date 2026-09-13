//! Real-file tests of the scheduler's bounded maintenance pass.

use super::*;
use bcode_session_models::{
    SessionEventKind, ToolArtifact, ToolArtifactRef, ToolInvocationResult,
    ToolInvocationResultRecord,
};
use std::path::PathBuf;

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
    assert_eq!(
        maintain_session_at(&state, root.path(), id, cursor, future)
            .await
            .expect("second page"),
        None
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
