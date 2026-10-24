//! Released historical input reaches cold maintenance without foreground attachment.

use super::*;
use bcode_session_models::{CURRENT_SESSION_EVENT_SCHEMA_VERSION, SessionEvent, SessionEventKind};

const HISTORICAL: &str =
    include_str!("../../../session-migration/fixtures/stores/schema-28-tool-context.jsonl");

/// Materialize the sanitized released schema-28 fixture for migration acceptance tests.
pub async fn historical_fixture() -> (
    tempfile::TempDir,
    ServerState,
    SessionId,
    std::path::PathBuf,
    Vec<SessionEvent>,
) {
    let (root, mut state, id, artifacts, _) = tests::fixture(1).await;
    // Keep enabled metrics even when the coordinator's test fixture defaults change.
    state.metrics = bcode_metrics::MetricsRegistry::default();
    state
        .sessions
        .append_event(
            id,
            SessionEventKind::SystemMessage {
                text: "compressible historical cold content ".repeat(10000),
            },
        )
        .await
        .expect("long history");
    let current = state
        .sessions
        .session_history(id)
        .await
        .expect("current history");
    state
        .sessions
        .release_session_ownership(id)
        .await
        .expect("release");
    let mut payloads: Vec<String> = HISTORICAL
        .lines()
        .map(|line| {
            let mut event: serde_json::Value =
                serde_json::from_str(line).expect("released fixture");
            event["session_id"] = serde_json::json!(id);
            serde_json::to_string(&event).expect("fixture payload")
        })
        .collect();
    for event in current.into_iter().skip(1) {
        let mut event = event;
        event.sequence = payloads.len() as u64;
        payloads.push(serde_json::to_string(&event).expect("current tail"));
    }
    let expected = payloads
        .iter()
        .map(|payload| {
            bcode_session_migration::normalize_canonical_event(
                payload,
                CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                |payload| serde_json::from_str(payload).map_err(|error| error.to_string()),
            )
            .expect("released normalization")
            .event
        })
        .collect();
    let db = bcode_session::db::SessionDb::open_existing_turso_in_root(id, root.path())
        .await
        .expect("fixture db");
    db.database()
        .delete("events")
        .execute(db.database())
        .await
        .expect("replace fixture events");
    for payload in payloads {
        let event: serde_json::Value = serde_json::from_str(&payload).expect("event envelope");
        let sequence = event["sequence"].as_i64().expect("sequence");
        let kind = event["kind"]
            .as_object()
            .expect("kind")
            .keys()
            .next()
            .expect("variant");
        db.database()
            .insert("events")
            .value("event_seq", sequence)
            .value("event_type", kind.clone())
            .value(
                "schema_version",
                event["schema_version"].as_i64().expect("schema"),
            )
            .value(
                "created_at_ms",
                event["timestamp_ms"].as_i64().expect("timestamp"),
            )
            .value("payload", payload)
            .execute(db.database())
            .await
            .expect("released event");
    }
    db.database()
        .update("session_storage_contract")
        .value("writer_epoch", 2)
        .execute(db.database())
        .await
        .expect("released writer");
    db.database().close().await.expect("close fixture db");
    drop(db);
    (root, state, id, artifacts, expected)
}

#[tokio::test]
async fn historical_cold_upgrade_then_artifact_and_history_compression_preserves_age_and_backups() {
    let (root, state, id, artifacts, expected) = Box::pin(historical_fixture()).await;
    let access = root.path().join(id.to_string()).join("storage-access.bin");
    let before = std::fs::read(&access).expect("access evidence");
    let now = crate::current_time_ms() + 31 * 86_400_000;
    maintain_session_at(&state, root.path(), id, None, now)
        .await
        .unwrap_or_else(|error| {
            panic!(
                "upgrade and artifact compression: {error}; terminal={:?}",
                state.storage_worker.terminal(id)
            )
        });
    assert!(artifacts.join("recording-000").is_dir());
    maintain_history(&state, root.path(), id, 0, None, now)
        .await
        .expect("history compression");
    assert!(
        state
            .metrics
            .snapshot()
            .counters
            .get("storage.maintenance.history_payload_bytes_saved")
            .copied()
            .unwrap_or_default()
            > 0
    );
    assert_eq!(std::fs::read(&access).expect("unchanged age"), before);
    let backups = bcode_session_migration::retained_migration_backups(root.path(), id, 256)
        .expect("retained backups");
    assert!(!backups.backups.is_empty());
    let db = bcode_session::db::SessionDb::open_existing_turso_in_root(id, root.path())
        .await
        .expect("current db");
    let page = db
        .history_page(bcode_session_models::SessionHistoryQuery {
            cursor: None,
            direction: bcode_session_models::SessionHistoryDirection::Forward,
            limit: 100,
        })
        .await
        .expect("strict history");
    assert_eq!(page.events, expected);
    db.validate_write_readiness()
        .await
        .expect("write readiness");
    db.database().close().await.expect("close current");
    drop(db);
    // The maintenance route has no actor-load, model-call or attachment requirement.
    assert!(state.attached_client_sessions.lock().await.is_empty());
    assert!(state.session_runtimes.lock().await.is_empty());
    state
        .sessions
        .append_event(
            id,
            SessionEventKind::SystemMessage {
                text: "continued write after maintenance".into(),
            },
        )
        .await
        .expect("continued current write");
    let history = state
        .sessions
        .session_history(id)
        .await
        .expect("continued history");
    assert_eq!(&history[..expected.len()], expected.as_slice());
    assert_eq!(history.len(), expected.len() + 1);
    state
        .sessions
        .release_session_ownership(id)
        .await
        .expect("release continued writer");
}

#[tokio::test]
async fn historical_unknown_age_upgrades_before_current_only_initialization_and_waits_to_compress()
{
    let (root, state, id, artifacts, _) = Box::pin(historical_fixture()).await;
    let access = root.path().join(id.to_string()).join("storage-access.bin");
    std::fs::remove_file(&access).expect("unknown age fixture");
    let now = crate::current_time_ms();
    maintain_session_at(&state, root.path(), id, None, now)
        .await
        .unwrap_or_else(|error| {
            panic!(
                "upgrade and initialize: {error}; terminal={:?}",
                state.storage_worker.terminal(id)
            )
        });
    assert!(artifacts.join("recording-000").is_file());
    let initialized = std::fs::read(&access).expect("initialized after upgrade");
    assert!(matches!(
        crate::session_migration_execution::classify_session_storage_for_maintenance(
            id,
            root.path()
        )
        .await,
        crate::session_migration_execution::MaintenanceStorageClassification::Current
    ));
    maintain_session_at(&state, root.path(), id, None, now + 31 * 86_400_000)
        .await
        .expect("eventually cold");
    assert!(artifacts.join("recording-000").is_dir());
    assert_eq!(std::fs::read(&access).expect("age preserved"), initialized);
}
