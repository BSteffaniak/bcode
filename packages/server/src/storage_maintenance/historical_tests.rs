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
    assert_ne!(
        backups.backups,
        [] as [bcode_session_migration::RetainedMigrationBackupDiagnosis; 0]
    );
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

/// Rewrite only Bcode session identity fields, never native turn/frame/task identities.
fn rebind_harness_fixture_session(value: &mut serde_json::Value, source: &str, target: &str) {
    match value {
        serde_json::Value::Object(fields) => {
            for (key, value) in fields {
                if key == "session_id" && value.as_str() == Some(source) {
                    *value = target.into();
                } else {
                    rebind_harness_fixture_session(value, source, target);
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                rebind_harness_fixture_session(value, source, target);
            }
        }
        _ => {}
    }
}

async fn harness_history_fixture(
    fixture: &str,
    writer_epoch: i64,
) -> (tempfile::TempDir, ServerState, SessionId, Vec<SessionEvent>) {
    let (root, state, id, _, _) = tests::fixture(0).await;
    let target_id = id.to_string();
    let payloads: Vec<String> = fixture
        .lines()
        .map(|line| {
            let mut value: serde_json::Value =
                serde_json::from_str(line).expect("fixture envelope");
            let source_id = value["session_id"]
                .as_str()
                .expect("fixture session")
                .to_owned();
            rebind_harness_fixture_session(&mut value, &source_id, &target_id);
            value.to_string()
        })
        .collect();
    let expected: Vec<_> = payloads
        .iter()
        .map(|payload| {
            let source: serde_json::Value = serde_json::from_str(payload).expect("fixture row");
            let event_kind = source["kind"]
                .as_object()
                .expect("fixture kind")
                .keys()
                .next()
                .expect("fixture variant")
                .clone();
            bcode_session_migration::normalize_canonical_row(
                &bcode_session_migration_target::CanonicalRow {
                    sequence: source["sequence"].as_u64().expect("fixture sequence"),
                    event_kind,
                    schema_version: u16::try_from(
                        source["schema_version"].as_u64().expect("fixture schema"),
                    )
                    .expect("schema fits durable version"),
                    payload: payload.clone(),
                },
            )
            .expect("migration-owned fixture normalization")
            .event
        })
        .collect();
    for event in &expected {
        assert_eq!(event.session_id, id);
        if let SessionEventKind::ExternalHarnessOperation { operation } = &event.kind {
            assert_eq!(operation.owner.session_id, id);
            operation
                .validate()
                .expect("all nested fixture correlations rebound");
        }
    }
    let db = bcode_session::db::SessionDb::open_existing_turso_in_root(id, root.path())
        .await
        .expect("historical target DB");
    db.database()
        .delete("events")
        .execute(db.database())
        .await
        .expect("replace history");
    db.database()
        .delete("session_migration_receipts")
        .execute(db.database())
        .await
        .expect("clear receipts");
    for payload in payloads {
        let value: serde_json::Value = serde_json::from_str(&payload).expect("event envelope");
        let kind = value["kind"]
            .as_object()
            .expect("kind")
            .keys()
            .next()
            .expect("variant");
        db.database()
            .insert("events")
            .value("event_seq", value["sequence"].as_i64().expect("sequence"))
            .value("event_type", kind.clone())
            .value(
                "schema_version",
                value["schema_version"].as_i64().expect("schema"),
            )
            .value(
                "created_at_ms",
                value["timestamp_ms"].as_i64().expect("timestamp"),
            )
            .value("payload", payload)
            .execute(db.database())
            .await
            .expect("historical canonical row");
    }
    db.database()
        .update("session_storage_contract")
        .value("writer_epoch", writer_epoch)
        .execute(db.database())
        .await
        .expect("historical writer");
    db.database().close().await.expect("close seeded DB");
    drop(db);
    (root, state, id, expected)
}

/// Expected fixture locators, not a second business projector. Production strict validation
/// independently checks the complete set via the current target's canonical key generator.
async fn harness_fixture_index_rows(
    db: &bcode_session::db::SessionDb,
) -> std::collections::BTreeSet<(String, i64, i64)> {
    use switchy::database::query::FilterableQuery as _;
    let rows = db
        .database()
        .select("turn_evidence")
        .columns(&["turn_id", "segment_order", "event_seq"])
        .where_eq("evidence_kind", "harness_operation_v1")
        .execute(db.database())
        .await
        .expect("actual harness indexes");
    let actual: std::collections::BTreeSet<_> = rows
        .iter()
        .map(|row| {
            (
                row.get("turn_id")
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .expect("key"),
                row.get("segment_order")
                    .and_then(|v| v.as_i64())
                    .expect("order"),
                row.get("event_seq")
                    .and_then(|v| v.as_i64())
                    .expect("sequence"),
            )
        })
        .collect();
    assert_eq!(actual.len(), rows.len(), "no duplicate locators");
    actual
}

async fn assert_harness_fixture_indexes(db: &bcode_session::db::SessionDb, schema: u16) {
    use bcode_session_models::external_backend::external_backend_key;
    use std::collections::BTreeSet;
    let actual = harness_fixture_index_rows(db).await;
    let (result, settled, gap, page_sequences): (i64, i64, i64, Vec<i64>) = if schema == 53 {
        (6, 7, 8, (5..=8).collect())
    } else {
        (8, 10, 11, (5..=11).collect())
    };
    for (namespace, identity, sequences) in [
        ("submission_acceptance", "10:submission:attempt", vec![5]),
        ("accepted_turn", "turn", vec![5]),
        ("turn_result", "turn", vec![result]),
        ("settled", "turn", vec![settled]),
        ("evidence_gap", "1", vec![gap]),
        ("terminal_coverage_v2", "current", page_sequences),
        (
            "native_terminal",
            "turn",
            if schema == 54 { vec![9] } else { vec![] },
        ),
        (
            "submission_native_frame_v2",
            "1:1:8:consumed:14:consumed-frame:10:submission:attempt",
            if schema == 54 { vec![6] } else { vec![] },
        ),
        (
            "submission_native_frame_v2",
            "1:1:15:result_observed:12:result-frame:10:submission:attempt",
            if schema == 54 { vec![7] } else { vec![] },
        ),
    ] {
        let key = external_backend_key(namespace, identity);
        let selected: BTreeSet<_> = actual
            .iter()
            .filter(|(id, _, _)| id == &key)
            .map(|(_, order, sequence)| (*order, *sequence))
            .collect();
        let expected: BTreeSet<_> = sequences
            .into_iter()
            .map(|sequence| (sequence, sequence))
            .collect();
        assert_eq!(
            selected, expected,
            "exact {namespace}/{identity} fixture keys"
        );
    }
    // Derived directories and native-reference memberships are not lifecycle evidence.
    // Reject exact semantic families rather than text inside proof identities.
    for kind in ["terminal", "coverage", "work", "continuation"] {
        for scope in [None, Some("unknown"), Some("attempt:10:submission:attempt")] {
            let namespace = scope.map_or_else(
                || format!("execution_v3:1:{kind}"),
                |scope| format!("execution_v3:1:inventory:{scope}:{kind}"),
            );
            let prefix = external_backend_key(&namespace, "");
            assert!(
                actual.iter().all(|(key, _, _)| !key.starts_with(&prefix)),
                "invented execution lifecycle locator in {namespace}"
            );
        }
    }
    let canonical = db
        .all_events_strict()
        .await
        .expect("canonical lifecycle absence");
    for event in &canonical {
        match &event.kind {
            SessionEventKind::ExternalHarnessOperation { operation } => {
                if let bcode_session_models::external_harness::ExternalHarnessOperationRecord::ObservationPage { page, .. } = &operation.record {
                    assert!(page.observations.iter().all(|observation| !matches!(
                        observation.kind, bcode_harness_models::HarnessObservationKind::Execution { .. }
                    )), "historical page gained an execution observation");
                }
            }
            SessionEventKind::ExternalObservationRecorded { observation, .. } => {
                assert!(!matches!(observation.kind, bcode_harness_models::HarnessObservationKind::Execution { .. }),
                    "historical standalone evidence gained an execution observation");
            }
            _ => {}
        }
    }
    if schema == 53 {
        assert!(actual.iter().all(|(key, _, _)| {
            !key.starts_with(&external_backend_key("native_terminal", ""))
                && !key.starts_with(&external_backend_key("submission_native_frame_v2", ""))
        }));
    }
}

async fn assert_historical_harness_receipt(
    db: &bcode_session::db::SessionDb,
    id: bcode_session_models::SessionId,
    writer: i64,
    expected: &[bcode_session_models::SessionEvent],
) {
    let rows = db
        .database()
        .select("session_migration_receipts")
        .columns(&["receipt"])
        .execute(db.database())
        .await
        .expect("durable receipts");
    assert_eq!(rows.len(), 1);
    let receipt_payload = rows[0]
        .get("receipt")
        .and_then(|v| v.as_str().map(str::to_owned))
        .expect("receipt payload");
    let receipt: serde_json::Value =
        serde_json::from_str(&receipt_payload).expect("receipt envelope");
    assert_eq!(receipt["source_writer_epoch"], writer);
    assert_eq!(
        receipt["target_writer_epoch"],
        bcode_session_models::CURRENT_SESSION_STORAGE_WRITER_EPOCH
    );
    assert_eq!(receipt["session_id"], id.to_string());
    let mut steps = if writer == 18 {
        vec![
            "session-writer-epoch-18-to-19",
            "session-writer-epoch-19-to-20",
            "session-writer-epoch-20-to-21",
            "session-writer-epoch-21-to-22",
            "session-writer-epoch-22-to-23",
        ]
    } else if writer == 19 {
        vec![
            "session-writer-epoch-19-to-20",
            "session-writer-epoch-20-to-21",
            "session-writer-epoch-21-to-22",
            "session-writer-epoch-22-to-23",
        ]
    } else if writer == 21 {
        vec![
            "session-writer-epoch-21-to-22",
            "session-writer-epoch-22-to-23",
        ]
    } else if writer == 22 {
        vec!["session-writer-epoch-22-to-23"]
    } else {
        assert_eq!(writer, 20, "explicit source contract required");
        vec![
            "session-writer-epoch-20-to-21",
            "session-writer-epoch-21-to-22",
            "session-writer-epoch-22-to-23",
        ]
    };
    steps.push("session-writer-epoch-23-to-25");
    steps.push("session-writer-epoch-25-to-26");
    steps.push("session-writer-epoch-26-to-27");
    steps.push("session-writer-epoch-27-to-28");
    assert_eq!(receipt["migration_step_ids"], serde_json::json!(steps));
    for field in ["source_event_count", "target_event_count"] {
        assert_eq!(receipt[field], serde_json::json!(expected.len()));
    }
    for field in ["source_event_tail", "target_event_tail"] {
        assert_eq!(
            receipt[field],
            serde_json::json!(expected.last().expect("tail").sequence)
        );
    }
}

/// Compare all source semantics exactly while independently reading canonical host roots.
async fn assert_migrated_harness_semantics(
    db: &bcode_session::db::SessionDb,
    actual: &[SessionEvent],
    expected: &[SessionEvent],
) {
    use bcode_session_models::external_harness::{HarnessInventoryKind, HarnessInventoryScope};
    assert_eq!(actual.len(), expected.len());
    let mut semantic = actual.to_vec();
    let mut owner = None;
    for event in &mut semantic {
        if let SessionEventKind::ExternalHarnessOperation { operation } = &mut event.kind {
            assert!(
                !operation.inventory_commitments.is_empty(),
                "host roots persisted canonically"
            );
            for commitment in &operation.inventory_commitments {
                assert_eq!(commitment.revision, event.sequence);
                // Directory and native-reference roots authenticate derived storage, not
                // lifecycle outcomes. Canonical/public inventory reads below verify emptiness
                // of the terminal/work/continuation/coverage dimensions separately.
                if commitment.snapshot_key.ends_with(":inventory_directory_v1") {
                    assert!(commitment.root.is_some(), "directory committed canonically");
                    assert!(commitment.summary.identities > 0);
                }
            }
            owner = Some(operation.owner.clone());
            operation.inventory_commitments.clear();
        }
    }
    assert_eq!(
        semantic, expected,
        "all source evidence survives canonical root enrichment"
    );
    let owner = owner.expect("fixture harness owner");
    for kind in [
        HarnessInventoryKind::Terminal,
        HarnessInventoryKind::Coverage,
        HarnessInventoryKind::Work,
        HarnessInventoryKind::Continuation,
    ] {
        let page = db
            .harness_inventory_page(&owner, &HarnessInventoryScope::Uncorrelated, kind, None)
            .await
            .expect("public authenticated root read");
        assert_eq!(
            page.entries,
            [] as [bcode_session_models::HarnessInventoryEntry; 0]
        );
        assert!(page.inventory_exhausted);
        assert!(page.next_cursor.is_none());
        assert_eq!(page.commitment.summary.identities, 0);
        assert!(page.commitment.root.is_none());
        assert!(
            actual.iter().any(|event| {
                event.sequence == page.commitment.revision
                    && matches!(&event.kind,
                SessionEventKind::ExternalHarnessOperation { operation }
                    if operation.inventory_commitments.contains(&page.commitment))
            }),
            "public read must authenticate the exact canonical commitment"
        );
    }
}

async fn check_historical_harness_cold_upgrade(fixture: &str, writer: i64, schema: u16) {
    let (root, state, id, expected) = Box::pin(harness_history_fixture(fixture, writer)).await;
    let legacy = bcode_session::db::SessionDb::open_existing_turso_in_root(id, root.path())
        .await
        .expect("non-mutating maintenance inspection");
    let source_rows = legacy
        .canonical_rows_page(0, 256)
        .await
        .expect("source rows");
    assert!(
        legacy.validate_read_readiness().await.is_err(),
        "normal read readiness cannot upgrade legacy storage"
    );
    assert!(
        legacy.validate_write_readiness().await.is_err(),
        "legacy writers require maintenance"
    );
    assert_eq!(
        legacy.storage_writer_epoch().await.unwrap(),
        u64::try_from(writer).unwrap()
    );
    assert_eq!(
        legacy.canonical_rows_page(0, 256).await.unwrap(),
        source_rows
    );
    legacy.database().close().await.unwrap();
    drop(legacy);
    let before = std::fs::read(root.path().join(id.to_string()).join("storage-access.bin"))
        .expect("original access age");
    Box::pin(maintain_session_at(
        &state,
        root.path(),
        id,
        None,
        crate::current_time_ms(),
    ))
    .await
    .expect("production cold-maintenance upgrade");
    assert!(matches!(
        crate::session_migration_execution::classify_session_storage_for_maintenance(
            id,
            root.path()
        )
        .await,
        crate::session_migration_execution::MaintenanceStorageClassification::Current
    ));
    assert_eq!(
        std::fs::read(root.path().join(id.to_string()).join("storage-access.bin"))
            .expect("preserved access age"),
        before
    );
    let db = bcode_session::db::SessionDb::open_existing_turso_in_root(id, root.path())
        .await
        .expect("strict current reopen");
    db.validate_read_readiness()
        .await
        .expect("current read readiness");
    db.validate_write_readiness()
        .await
        .expect("current writer readiness");
    assert_eq!(
        db.storage_writer_epoch().await.expect("writer epoch"),
        u64::from(bcode_session_models::CURRENT_SESSION_STORAGE_WRITER_EPOCH)
    );
    let actual = db
        .all_events_strict()
        .await
        .expect("strict canonical history");
    assert_migrated_harness_semantics(&db, &actual, &expected).await;
    assert!(
        actual.iter().all(|event| event.schema_version
            == bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION)
    );
    assert_harness_fixture_indexes(&db, schema).await;
    assert_historical_harness_receipt(&db, id, writer, &expected).await;
    db.database().close().await.expect("close migrated DB");
    drop(db);
    let reopened = bcode_session::db::SessionDb::open_existing_turso_in_root(id, root.path())
        .await
        .expect("second strict reopen");
    reopened
        .validate_read_readiness()
        .await
        .expect("reopened read readiness");
    let reopened_events = reopened
        .all_events_strict()
        .await
        .expect("reopened history");
    assert_eq!(
        reopened_events, actual,
        "canonical commitments survive reopening exactly"
    );
    assert_migrated_harness_semantics(&reopened, &reopened_events, &expected).await;
    assert_harness_fixture_indexes(&reopened, schema).await;
    reopened
        .database()
        .close()
        .await
        .expect("close reopened DB");
}

#[tokio::test]
async fn historical_schema53_harness_cold_maintenance_preserves_legacy_evidence() {
    Box::pin(check_historical_harness_cold_upgrade(
        include_str!("../../../session-migration/fixtures/stores/epoch-18-schema-53-harness.jsonl"),
        18,
        53,
    ))
    .await;
}

#[tokio::test]
async fn reconstructed_writer21_contract_upgrades_and_reopens_without_losing_evidence() {
    // Reconstructed compatibility input, not an authentic writer-21 export.
    Box::pin(check_historical_harness_cold_upgrade(
        include_str!("../../../session-migration/fixtures/stores/epoch-19-schema-54-harness.jsonl"),
        21,
        54,
    ))
    .await;
}

#[tokio::test]
async fn reconstructed_writer22_contract_upgrades_and_reopens_without_losing_evidence() {
    // Reconstructed compatibility input, not an authentic writer-22 export.
    Box::pin(check_historical_harness_cold_upgrade(
        include_str!("../../../session-migration/fixtures/stores/epoch-19-schema-54-harness.jsonl"),
        22,
        54,
    ))
    .await;
}

#[tokio::test]
async fn historical_schema54_harness_cold_maintenance_preserves_v2_terminal_and_frames() {
    Box::pin(check_historical_harness_cold_upgrade(
        include_str!("../../../session-migration/fixtures/stores/epoch-19-schema-54-harness.jsonl"),
        19,
        54,
    ))
    .await;
}
