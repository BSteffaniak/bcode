//! Real Turso migration transactions using public session target APIs.
//!
//! No provider, native process, raw SQL, or alternate projector is used. The historical
//! fixture reconstructs released writer-23/schema-55 canonical rows in current physical
//! tables; it does not claim that a current writer emitted a historical database.
//! These tests require the selected-family publication (writer25/schema57) or later.
//! The coordinator-owned manifest must expose these workspace dev-dependencies:
//! `bcode_session`, `bcode_session_migration`, `bcode_harness_models`, `bcode_metrics`,
//! `serde_json`, `tempfile`, `tokio` (macros/rt), and `switchy` (database).

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bcode_harness_models::{
    HARNESS_SCHEMA_VERSION, HarnessAuthBinding, HarnessAuthCustody, HarnessCapabilities,
    HarnessCapability, HarnessInstance, HarnessModelEvidence, HarnessModelSelection,
    HarnessModelsRequest, HarnessModelsRequestKind, HarnessModelsResponse,
    HarnessModelsResponseKind, HarnessOpenRequest, HarnessPreparedSession, HarnessSessionBinding,
    HarnessSessionTarget, HarnessSupportLevel,
};
use bcode_session::{
    SessionManager,
    db::{SessionDb, SessionDbResult},
    lease::{
        SessionLeaseOwnerContext, acquire_maintenance_session_write_lock,
        acquire_session_maintenance_guard,
    },
};
use bcode_session_migration::{
    MIGRATION_STEPS, build_target_receipt, normalize_canonical_event,
    normalize_canonical_row_for_writer_epoch, plan_writer_epoch_migration_with_registry,
    validate_writer_finalization,
};
use bcode_session_migration_target::{
    CURRENT_EVENT_SCHEMA, CURRENT_WRITER_EPOCH, CanonicalRow, MigrationPolicyCallbacks,
};
use bcode_session_models::{
    ExternalHarnessClaim, ExternalHarnessOwner, ExternalHarnessRequest, SessionEvent,
    SessionEventKind, SessionId,
};
use std::{path::Path, sync::Arc};
use switchy::database::{DatabaseValue, query::FilterableQuery};

fn policy(writer: u32, fail_receipt: bool) -> MigrationPolicyCallbacks {
    MigrationPolicyCallbacks {
        normalize: Arc::new(move |row| {
            normalize_canonical_row_for_writer_epoch(row, writer).map_err(|error| error.to_string())
        }),
        build_receipt: Arc::new(move |facts| {
            if fail_receipt {
                return Err("deliberate failure after canonical/projector validation".into());
            }
            build_target_receipt(facts)
        }),
    }
}

async fn migrate(
    root: &Path,
    session: SessionId,
    writer: u32,
    fail_receipt: bool,
) -> SessionDbResult<SessionDb> {
    let maintenance = acquire_session_maintenance_guard(root, session).unwrap();
    let write = acquire_maintenance_session_write_lock(&maintenance, root, session).unwrap();
    SessionDb::migrate_turso_in_root_observed_with_operation(
        session,
        root,
        &maintenance,
        &write,
        bcode_metrics::MetricsRegistry::disabled(),
        None,
        None,
        policy(writer, fail_receipt),
    )
    .await
}

fn renamed(session: SessionId, sequence: u64) -> SessionEvent {
    SessionEvent {
        schema_version: CURRENT_EVENT_SCHEMA,
        sequence,
        timestamp_ms: sequence + 1,
        session_id: session,
        provenance: None,
        kind: SessionEventKind::SessionRenamed {
            name: Some("after migration".into()),
        },
    }
}

/// Seed released event representations directly, never relabel current selected evidence.
async fn released_store(root: &Path, session: SessionId) -> Vec<CanonicalRow> {
    assert_eq!((CURRENT_WRITER_EPOCH, CURRENT_EVENT_SCHEMA), (26, 58));
    let db = SessionDb::open_turso_in_root(session, root).await.unwrap();
    let kinds = [
        serde_json::json!({"session_created": {
            "name": "reconstructed released writer 23",
            "working_directory": root
        }}),
        serde_json::json!({"session_renamed": {"name": "retained historical name"}}),
    ];
    for (sequence, kind) in kinds.into_iter().enumerate() {
        let sequence = i64::try_from(sequence).unwrap();
        let payload = serde_json::json!({
            "schema_version": 55,
            "sequence": sequence,
            "timestamp_ms": sequence + 1,
            "session_id": session,
            "provenance": null,
            "kind": kind
        });
        let event_type = payload["kind"].as_object().unwrap().keys().next().unwrap();
        db.database()
            .insert("events")
            .value("event_seq", DatabaseValue::Int64(sequence))
            .value("event_type", event_type.as_str())
            .value("schema_version", DatabaseValue::Int32(55))
            .value("created_at_ms", DatabaseValue::Int64(sequence + 1))
            .value("payload", payload.to_string())
            .execute(db.database())
            .await
            .unwrap();
    }
    db.database()
        .update("session_storage_contract")
        .value("writer_epoch", DatabaseValue::Int64(23))
        .where_eq("contract_id", 1)
        .execute(db.database())
        .await
        .unwrap();
    db.canonical_rows_page(0, 32).await.unwrap()
}

async fn rows(db: &SessionDb, table: &str, columns: &[&str]) -> Vec<Vec<DatabaseValue>> {
    let mut values = db
        .database()
        .select(table)
        .columns(columns)
        .execute(db.database())
        .await
        .unwrap()
        .iter()
        .map(|row| {
            columns
                .iter()
                .map(|column| row.get(column).expect("declared snapshot column"))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    // Query order is unspecified. Sorting debug representations only orders exact typed
    // values for comparison; it does not interpret persisted text as implementation code.
    values.sort_by_key(|row| format!("{row:?}"));
    values
}

async fn transaction_state(db: &SessionDb) -> Vec<Vec<Vec<DatabaseValue>>> {
    let mut state = Vec::new();
    for (table, columns) in [
        (
            "events",
            &["event_seq", "event_type", "schema_version", "payload"][..],
        ),
        (
            "session_storage_contract",
            &["contract_id", "schema_version", "writer_epoch"][..],
        ),
        (
            "projection_checkpoints",
            &["projection_name", "last_event_seq"][..],
        ),
        (
            "turn_evidence",
            &["turn_id", "evidence_kind", "event_seq"][..],
        ),
        (
            "snapshots",
            &[
                "snapshot_name",
                "last_event_seq",
                "schema_version",
                "payload",
            ][..],
        ),
        (
            "model_context_projection_state",
            &["projection_id", "schema_version", "last_event_seq"][..],
        ),
        (
            "model_context_entries",
            &["event_seq", "event_type", "payload"][..],
        ),
        (
            "session_migration_receipts",
            &[
                "operation_id",
                "source_writer_epoch",
                "target_writer_epoch",
                "receipt",
            ][..],
        ),
    ] {
        state.push(rows(db, table, columns).await);
    }
    state
}

#[tokio::test]
async fn released_55_migrates_to_current_losslessly_and_remains_writable_after_reopen() {
    let root = tempfile::tempdir().unwrap();
    let session = SessionId::new();
    let source = released_store(root.path(), session).await;
    let db = migrate(root.path(), session, 23, false).await.unwrap();
    assert_eq!(
        db.storage_writer_epoch().await.unwrap(),
        u64::from(CURRENT_WRITER_EPOCH)
    );
    let converted = db.canonical_rows_page(0, 32).await.unwrap();
    assert_eq!(source.len(), converted.len());
    for (before, after) in source.iter().zip(&converted) {
        assert_eq!(before.sequence, after.sequence);
        assert_eq!(before.event_kind, after.event_kind);
        assert_eq!(
            (before.schema_version, after.schema_version),
            (55, CURRENT_EVENT_SCHEMA)
        );
        let mut expected: SessionEvent = serde_json::from_str(&before.payload).unwrap();
        expected.schema_version = CURRENT_EVENT_SCHEMA;
        let actual: SessionEvent = serde_json::from_str(&after.payload).unwrap();
        assert_eq!(actual, expected);
    }
    let receipts = db
        .database()
        .select("session_migration_receipts")
        .columns(&["receipt"])
        .execute(db.database())
        .await
        .unwrap();
    assert_eq!(receipts.len(), 1);
    let Some(DatabaseValue::String(receipt)) = receipts[0].get("receipt") else {
        panic!("migration must persist a typed receipt");
    };
    let receipt: bcode_session_migration_target::MigrationReceipt =
        serde_json::from_str(&receipt).unwrap();
    assert_eq!(
        (receipt.source_writer_epoch, receipt.target_writer_epoch),
        (23, CURRENT_WRITER_EPOCH)
    );
    assert_eq!(
        receipt.migration_step_ids,
        [
            "session-writer-epoch-23-to-25",
            "session-writer-epoch-25-to-26"
        ]
    );
    assert_eq!(
        (receipt.source_event_count, receipt.target_event_count),
        (2, 2)
    );
    assert_eq!(receipt.converted_events.get("55:session_created"), Some(&1));
    assert_eq!(receipt.converted_events.get("55:session_renamed"), Some(&1));
    assert!(receipt.retired_known_events.is_empty());
    db.validate_write_readiness().await.unwrap();
    db.append_event(&renamed(session, 2)).await.unwrap();
    drop(db);
    let db = SessionDb::open_existing_turso_in_root(session, root.path())
        .await
        .unwrap();
    db.validate_write_readiness().await.unwrap();
    db.append_event(&renamed(session, 3)).await.unwrap();
    assert_eq!(db.last_event_sequence().await.unwrap(), Some(3));
}

#[tokio::test]
async fn corrupt_historical_tail_rolls_back_prior_conversion_and_projection_work() {
    let root = tempfile::tempdir().unwrap();
    let session = SessionId::new();
    released_store(root.path(), session).await;
    let db = SessionDb::open_existing_turso_in_root(session, root.path())
        .await
        .unwrap();
    db.database()
        .update("events")
        .value("payload", "{corrupt-tail")
        .where_eq("event_seq", DatabaseValue::Int64(1))
        .execute(db.database())
        .await
        .unwrap();
    let before = transaction_state(&db).await;
    drop(db);
    assert!(migrate(root.path(), session, 23, false).await.is_err());
    let db = SessionDb::open_existing_turso_in_root(session, root.path())
        .await
        .unwrap();
    assert_eq!(transaction_state(&db).await, before);
    assert_eq!(db.storage_writer_epoch().await.unwrap(), 23);
}

#[tokio::test]
async fn failure_after_strict_validation_rolls_back_canonical_rows_indexes_and_writer() {
    let root = tempfile::tempdir().unwrap();
    let session = SessionId::new();
    released_store(root.path(), session).await;
    let db = SessionDb::open_existing_turso_in_root(session, root.path())
        .await
        .unwrap();
    let before = transaction_state(&db).await;
    drop(db);
    let error = migrate(root.path(), session, 23, true).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("deliberate failure after canonical/projector validation")
    );
    let db = SessionDb::open_existing_turso_in_root(session, root.path())
        .await
        .unwrap();
    assert_eq!(transaction_state(&db).await, before);
    assert_eq!(db.storage_writer_epoch().await.unwrap(), 23);
    drop(db);
    let db = migrate(root.path(), session, 23, false).await.unwrap();
    db.validate_write_readiness().await.unwrap();
}

#[tokio::test]
async fn reserved_sources_and_targets_preserve_database_state() {
    for (writer, schema) in [(24, 55), (23, 56)] {
        let root = tempfile::tempdir().unwrap();
        let session = SessionId::new();
        released_store(root.path(), session).await;
        let db = SessionDb::open_existing_turso_in_root(session, root.path())
            .await
            .unwrap();
        db.database()
            .update("session_storage_contract")
            .value("writer_epoch", DatabaseValue::Int64(i64::from(writer)))
            .where_eq("contract_id", 1)
            .execute(db.database())
            .await
            .unwrap();
        if schema == 56 {
            let source = db.canonical_rows_page(0, 1).await.unwrap().remove(0);
            let mut payload: serde_json::Value = serde_json::from_str(&source.payload).unwrap();
            payload["schema_version"] = 56.into();
            db.database()
                .update("events")
                .value("schema_version", DatabaseValue::Int32(56))
                .value("payload", payload.to_string())
                .where_eq("event_seq", DatabaseValue::Int64(0))
                .execute(db.database())
                .await
                .unwrap();
        }
        let before = transaction_state(&db).await;
        drop(db);
        assert!(migrate(root.path(), session, writer, false).await.is_err());
        let db = SessionDb::open_existing_turso_in_root(session, root.path())
            .await
            .unwrap();
        assert_eq!(transaction_state(&db).await, before);
        assert!(plan_writer_epoch_migration_with_registry(23, 24, &MIGRATION_STEPS).is_err());
        assert!(plan_writer_epoch_migration_with_registry(24, 24, &MIGRATION_STEPS).is_err());
        assert!(validate_writer_finalization(24, true).is_err());
        assert!(
            normalize_canonical_event("{}", 56, |_| { panic!("reserved target must not decode") })
                .is_err()
        );
        assert_eq!(transaction_state(&db).await, before);
    }
}

fn selected_request(session: SessionId, root: &Path) -> HarnessModelsRequest {
    HarnessModelsRequest {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: "exact-selected-open".into(),
        instance: HarnessInstance {
            instance_id: "migration-fixture".into(),
            plugin_id: "migration-fixture".into(),
            executable: "/not-invoked".into(),
            config_namespace: "migration-fixture".into(),
            auth: HarnessAuthBinding {
                provider_id: "migration-fixture".into(),
                profile: "native".into(),
                credential_namespace: "migration-fixture".into(),
                custody: HarnessAuthCustody::NativeManaged,
            },
        },
        kind: HarnessModelsRequestKind::OpenSelected {
            open: Box::new(HarnessOpenRequest {
                open_id: "exact-selected-open".into(),
                session_id: session.to_string(),
                generation: 1,
                working_directory: root.to_string_lossy().into_owned(),
                resume: None,
                parent_assistance: None,
            }),
            selection: HarnessModelSelection {
                model_id: "synthetic-exact-choice".into(),
            },
        },
    }
}

fn capabilities() -> HarnessCapabilities {
    let c = HarnessCapability {
        support: HarnessSupportLevel::Unsupported,
        constraints: vec![],
    };
    HarnessCapabilities {
        model_switch: bcode_harness_models::HarnessCapability::unsupported(),
        permission_mode: bcode_harness_models::HarnessCapability::unsupported(),
        resume: c.clone(),
        steering: c.clone(),
        approval: c.clone(),
        questions: c.clone(),
        assistance: c.clone(),
        interrupt: c.clone(),
        authoritative_settlement: c.clone(),
        background_continuation: c.clone(),
        authentication: c,
    }
}

async fn rebuild_selected_index(db: &SessionDb, root: &Path, session: SessionId) {
    let maintenance = acquire_session_maintenance_guard(root, session).unwrap();
    let write = acquire_maintenance_session_write_lock(&maintenance, root, session).unwrap();
    db.reindex_external_harness_evidence(&maintenance, &write)
        .await
        .unwrap();
    drop(write);
    drop(maintenance);
}

async fn verify_selected_corruption_rollback(
    db: SessionDb,
    root: &Path,
    session: SessionId,
    before: &[CanonicalRow],
    retained: &ExternalHarnessRequest,
) {
    let completion = before
        .iter()
        .find(|row| row.payload.contains("\"delivery_completed\""))
        .expect("canonical completion");
    let mut broken: serde_json::Value = serde_json::from_str(&completion.payload).unwrap();
    broken["kind"]["external_harness_operation"]["operation"]["record"]["response"]["kind"]["response"]
        ["evidence"]["requested_model_id"] = "wrong-choice".into();
    db.database()
        .update("events")
        .value("payload", broken.to_string())
        .where_eq(
            "event_seq",
            DatabaseValue::Int64(i64::try_from(completion.sequence).unwrap()),
        )
        .execute(db.database())
        .await
        .unwrap();
    assert!(
        db.external_harness_retained_request(retained)
            .await
            .is_err()
    );
    let corrupt_before = transaction_state(&db).await;
    drop(db);
    assert!(
        migrate(root, session, CURRENT_WRITER_EPOCH, false)
            .await
            .is_err()
    );
    let db = SessionDb::open_existing_turso_in_root(session, root)
        .await
        .unwrap();
    assert_eq!(transaction_state(&db).await, corrupt_before);
    assert_eq!(
        db.storage_writer_epoch().await.unwrap(),
        u64::from(CURRENT_WRITER_EPOCH)
    );
}

async fn verify_selected_index_rebuild(
    root: &Path,
    session: SessionId,
    request: &HarnessModelsRequest,
    response: &HarnessModelsResponse,
) -> (SessionDb, Vec<CanonicalRow>, ExternalHarnessRequest) {
    let retained = ExternalHarnessRequest::from_models_request(request).unwrap();
    let db = SessionDb::open_existing_turso_in_root(session, root)
        .await
        .unwrap();
    let before = db.canonical_rows_page(0, 32).await.unwrap();
    let expected = db
        .external_harness_retained_request(&retained)
        .await
        .unwrap();
    let Some(ExternalHarnessClaim::Recorded(recorded)) = &expected else {
        panic!("selected result must be indexed as recorded evidence");
    };
    assert_eq!(
        serde_json::to_value(recorded.models_response(&retained).unwrap()).unwrap(),
        serde_json::to_value(response).unwrap()
    );
    let mut conflict = retained.clone();
    if let bcode_session_models::ExternalHarnessRequestKind::OpenSelected(selected) =
        &mut conflict.kind
        && let HarnessModelsRequestKind::OpenSelected { selection, .. } = &mut selected.request.kind
    {
        selection.model_id = "different-choice".into();
    }
    assert!(
        db.external_harness_retained_request(&conflict)
            .await
            .is_err()
    );
    drop(db);
    let db = migrate(root, session, CURRENT_WRITER_EPOCH, false)
        .await
        .unwrap();
    rebuild_selected_index(&db, root, session).await;
    assert_eq!(db.canonical_rows_page(0, 32).await.unwrap(), before);
    assert_eq!(
        db.external_harness_retained_request(&retained)
            .await
            .unwrap(),
        expected
    );
    db.validate_write_readiness().await.unwrap();
    drop(db);
    let db = SessionDb::open_existing_turso_in_root(session, root)
        .await
        .unwrap();
    assert_eq!(
        db.external_harness_retained_request(&retained)
            .await
            .unwrap(),
        expected
    );
    let evidence_before = rows(
        &db,
        "turn_evidence",
        &["turn_id", "evidence_kind", "event_seq"],
    )
    .await;
    let completion_key = bcode_session_models::external_backend::external_backend_key(
        "completion",
        &retained.request_id,
    );
    db.database()
        .update("turn_evidence")
        .value("event_seq", DatabaseValue::Int64(0))
        .where_eq("turn_id", completion_key)
        .where_eq("evidence_kind", "harness_operation_v1")
        .execute(db.database())
        .await
        .unwrap();
    assert!(
        db.external_harness_retained_request(&retained)
            .await
            .is_err()
    );
    let maintenance = acquire_session_maintenance_guard(root, session).unwrap();
    let write = acquire_maintenance_session_write_lock(&maintenance, root, session).unwrap();
    db.reindex_external_harness_evidence(&maintenance, &write)
        .await
        .unwrap();
    drop(write);
    drop(maintenance);
    assert_eq!(
        rows(
            &db,
            "turn_evidence",
            &["turn_id", "evidence_kind", "event_seq"]
        )
        .await,
        evidence_before
    );
    assert_eq!(db.canonical_rows_page(0, 32).await.unwrap(), before);
    assert_eq!(
        db.external_harness_retained_request(&retained)
            .await
            .unwrap(),
        expected
    );
    (db, before, retained)
}

#[tokio::test]
async fn selected_evidence_survives_exact_index_rebuild_reopen_and_corruption_rollback() {
    for confirmed in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let session = SessionId::new();
        let owner = ExternalHarnessOwner {
            session_id: session,
            generation: 1,
            artifact_id: "migration-artifact".into(),
            daemon_id: "migration-daemon".into(),
        };
        let manager = SessionManager::persistent_with_metrics_and_lease_owner(
            root.path(),
            bcode_metrics::MetricsRegistry::disabled(),
            SessionLeaseOwnerContext {
                build_fingerprint: Some(owner.artifact_id.clone()),
                daemon_instance_id: Some(owner.daemon_id.clone()),
                ..SessionLeaseOwnerContext::default()
            },
        )
        .unwrap();
        let request = selected_request(session, root.path());
        manager
            .prepare_external_harness_selected_open(&owner, &request, capabilities(), None)
            .await
            .unwrap();
        assert!(matches!(
            manager
                .claim_external_harness_selected_open(&owner, &request)
                .await
                .unwrap(),
            ExternalHarnessClaim::New
        ));
        let target = if confirmed {
            HarnessSessionTarget::Confirmed(HarnessSessionBinding {
                schema_version: HARNESS_SCHEMA_VERSION,
                session_id: session.to_string(),
                instance_id: request.instance.instance_id.clone(),
                native_session_id: "recorded-native-session".into(),
                native_version: "synthetic-native-v1".into(),
                generation: 1,
            })
        } else {
            HarnessSessionTarget::Prepared(HarnessPreparedSession {
                open_id: request.request_id.clone(),
                session_id: session.to_string(),
                instance_id: request.instance.instance_id.clone(),
                generation: 1,
                native_version: "synthetic-native-v1".into(),
                requested_native_session_id: None,
            })
        };
        let response = HarnessModelsResponse {
            schema_version: HARNESS_SCHEMA_VERSION,
            request_id: request.request_id.clone(),
            instance_id: request.instance.instance_id.clone(),
            kind: HarnessModelsResponseKind::Selected {
                target,
                evidence: HarnessModelEvidence {
                    requested_model_id: "synthetic-exact-choice".into(),
                    effective_model_id: confirmed.then(|| "recorded-effective-choice".into()),
                },
            },
        };
        manager
            .complete_external_harness_selected_open(&owner, &request, &response)
            .await
            .unwrap();
        manager
            .release_idle_session_resources(session)
            .await
            .unwrap();
        drop(manager);
        let (db, before, retained) =
            verify_selected_index_rebuild(root.path(), session, &request, &response).await;
        verify_selected_corruption_rollback(db, root.path(), session, &before, &retained).await;
    }
}
