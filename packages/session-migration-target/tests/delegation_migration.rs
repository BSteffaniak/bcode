//! Writer-28 stores catch up to the writer-29 delegation contract through migration.
//!
//! Writer 28 emitted event schema 59. Schema 60 / writer 29 adds the optional native delegation
//! intent retained in child creation and the `always_ask` turn permission mode; neither adds a
//! projection column. The 28-to-29 edge preserves canonical payloads and rebuilds every current
//! projection. Released admissions keep their recorded permission mode, and historical inputs
//! carrying schema-60-only evidence are rejected rather than reinterpreted.
//! The writer-28 state is reconstructed from a current store by relabeling canonical rows to
//! schema 59. No provider, native process, raw SQL, or alternate projector is used.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bcode_session::{
    SessionManager,
    db::SessionDb,
    lease::{acquire_maintenance_session_write_lock, acquire_session_maintenance_guard},
};
use bcode_session_migration::{build_target_receipt, normalize_canonical_row_for_writer_epoch};
use bcode_session_migration_target::{
    CURRENT_EVENT_SCHEMA, CURRENT_WRITER_EPOCH, MigrationPolicyCallbacks,
};
use bcode_session_models::{
    ClientId, SessionEventKind, SessionId, TurnAdmissionMetadata, TurnExecutionOptions,
    TurnPermissionMode,
};
use std::{path::Path, sync::Arc};
use switchy::database::{DatabaseValue, query::FilterableQuery};

const SOURCE_WRITER: u32 = 28;
const SOURCE_SCHEMA: u16 = 59;

fn user_message(text: &str, permission_mode: TurnPermissionMode) -> SessionEventKind {
    SessionEventKind::UserMessage {
        client_id: ClientId::new(),
        text: text.into(),
        admission: TurnAdmissionMetadata {
            execution: TurnExecutionOptions {
                permission_mode,
                ..TurnExecutionOptions::default()
            },
            ..TurnAdmissionMetadata::default()
        },
    }
}

async fn session_with_messages(root: &Path, modes: &[TurnPermissionMode]) -> SessionId {
    let manager = SessionManager::persistent(root).unwrap();
    let session = manager.create_session(None, root.into()).await.unwrap();
    for (index, mode) in modes.iter().enumerate() {
        manager
            .append_event(
                session.id,
                user_message(&format!("released-{index}"), *mode),
            )
            .await
            .unwrap();
    }
    manager.shutdown_catalog_updates().await;
    drop(manager);
    session.id
}

/// Reconstruct a writer-28 store: schema-59 canonical rows and an unchanged projection schema.
async fn reconstruct_writer_28(root: &Path, session: SessionId) {
    let db = SessionDb::open_turso_in_root(session, root).await.unwrap();
    for row in db.canonical_rows_page(0, 64).await.unwrap() {
        assert_eq!(row.schema_version, CURRENT_EVENT_SCHEMA);
        let mut payload: serde_json::Value = serde_json::from_str(&row.payload).unwrap();
        payload["schema_version"] = serde_json::json!(SOURCE_SCHEMA);
        db.database()
            .update("events")
            .value(
                "schema_version",
                DatabaseValue::Int32(i32::from(SOURCE_SCHEMA)),
            )
            .value("payload", payload.to_string())
            .where_eq(
                "event_seq",
                DatabaseValue::Int64(i64::try_from(row.sequence).unwrap()),
            )
            .execute(db.database())
            .await
            .unwrap();
    }
    db.database()
        .update("session_storage_contract")
        .value(
            "writer_epoch",
            DatabaseValue::Int64(i64::from(SOURCE_WRITER)),
        )
        .where_eq("contract_id", 1)
        .execute(db.database())
        .await
        .unwrap();
}

async fn migrate(root: &Path, session: SessionId) -> Result<SessionDb, String> {
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
        MigrationPolicyCallbacks {
            normalize: Arc::new(|row| {
                normalize_canonical_row_for_writer_epoch(row, SOURCE_WRITER)
                    .map_err(|error| error.to_string())
            }),
            build_receipt: Arc::new(build_target_receipt),
        },
    )
    .await
    .map_err(|error| error.to_string())
}

fn permission_modes(rows: &[bcode_session_migration_target::CanonicalRow]) -> Vec<String> {
    rows.iter()
        .filter_map(|row| {
            let payload: serde_json::Value = serde_json::from_str(&row.payload).unwrap();
            payload
                .pointer("/kind/user_message/admission/execution/permission_mode")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
        })
        .collect()
}

#[tokio::test]
async fn writer_28_store_migrates_to_delegation_writer_losslessly_and_stays_writable() {
    let root = tempfile::tempdir().unwrap();
    let session = session_with_messages(
        root.path(),
        &[TurnPermissionMode::Enforce, TurnPermissionMode::Bypass],
    )
    .await;
    reconstruct_writer_28(root.path(), session).await;

    let db = migrate(root.path(), session).await.unwrap();
    assert_eq!(
        db.storage_writer_epoch().await.unwrap(),
        u64::from(CURRENT_WRITER_EPOCH)
    );
    let rows = db.canonical_rows_page(0, 64).await.unwrap();
    assert_ne!(rows.len(), 0);
    assert!(
        rows.iter()
            .all(|row| row.schema_version == CURRENT_EVENT_SCHEMA)
    );
    assert_eq!(permission_modes(&rows), ["enforce", "bypass"]);
    drop(db);

    let manager = SessionManager::persistent(root.path()).unwrap();
    manager
        .append_event(
            session,
            user_message("current", TurnPermissionMode::AlwaysAsk),
        )
        .await
        .unwrap();
    manager.shutdown_catalog_updates().await;
    drop(manager);

    let reopened = SessionDb::open_existing_turso_in_root(session, root.path())
        .await
        .unwrap();
    reopened.validate_write_readiness().await.unwrap();
    let rows = reopened.canonical_rows_page(0, 64).await.unwrap();
    assert_eq!(permission_modes(&rows), ["enforce", "bypass", "always_ask"]);
}

#[tokio::test]
async fn writer_28_store_carrying_always_ask_is_rejected_rather_than_guessed() {
    let root = tempfile::tempdir().unwrap();
    let session = session_with_messages(root.path(), &[TurnPermissionMode::AlwaysAsk]).await;
    reconstruct_writer_28(root.path(), session).await;

    let Err(error) = migrate(root.path(), session).await else {
        panic!("historical always-ask admission is not a released representation");
    };
    assert!(
        error.contains("event schema 60"),
        "unexpected rejection: {error}"
    );
}
