//! Writer-27 stores catch up to the writer-28 account-reference contract through migration.
//!
//! Writer 27 emitted event schema 58 and had no `session_state.account_reference` projection
//! column. The 27-to-28 edge preserves canonical payloads, applies the additive column migration,
//! and rebuilds every current projection. Released selections carry no explicit account, so the
//! projected reference is empty and later account-bearing selections persist normally.
//! The writer-27 state is reconstructed from a current store by relabeling canonical rows to
//! schema 58 and removing exactly the schema objects writer 27 never had. No provider, native
//! process, raw SQL, or alternate projector is used.

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
use bcode_session_models::{ModelSelectionSource, SessionEventKind, SessionId};
use std::{path::Path, sync::Arc};
use switchy::database::{DatabaseValue, query::FilterableQuery};

const SOURCE_WRITER: u32 = 27;
const SOURCE_SCHEMA: u16 = 58;
const ACCOUNT_COLUMN_MIGRATION: &str = "083_session_state_account_reference_column";

fn model_changed(model: &str, account_reference: Option<&str>) -> SessionEventKind {
    SessionEventKind::ModelChanged {
        provider: "bcode.openai-compatible".into(),
        model: model.into(),
        selection_source: ModelSelectionSource::UserExplicit,
        external_auth_binding: None,
        account_reference: account_reference.map(Into::into),
    }
}

async fn session_with_explicit_model(root: &Path) -> SessionId {
    let manager = SessionManager::persistent(root).unwrap();
    let session = manager.create_session(None, root.into()).await.unwrap();
    manager
        .append_event(session.id, model_changed("released-model", None))
        .await
        .unwrap();
    manager.shutdown_catalog_updates().await;
    drop(manager);
    session.id
}

/// Reconstruct a writer-27 store: schema-58 canonical rows and no account projection column.
async fn reconstruct_writer_27(root: &Path, session: SessionId) {
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
    switchy::database::schema::alter_table("session_state")
        .drop_column("account_reference".into())
        .execute(db.database())
        .await
        .unwrap();
    db.database()
        .delete("__bcode_session_migrations")
        .where_eq("id", ACCOUNT_COLUMN_MIGRATION)
        .execute(db.database())
        .await
        .unwrap();
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

async fn migrate(root: &Path, session: SessionId) -> SessionDb {
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
    .unwrap()
}

#[tokio::test]
async fn writer_27_store_migrates_to_account_reference_writer_and_stays_writable() {
    let root = tempfile::tempdir().unwrap();
    let session = session_with_explicit_model(root.path()).await;
    reconstruct_writer_27(root.path(), session).await;

    let db = migrate(root.path(), session).await;
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
    drop(db);

    let manager = SessionManager::persistent(root.path()).unwrap();
    let selection = manager.current_runtime_selection(session).await.unwrap();
    assert_eq!(selection.model_id.as_deref(), Some("released-model"));
    assert_eq!(
        selection.model_selection_source,
        ModelSelectionSource::UserExplicit
    );
    assert_eq!(selection.account_reference, None);
    manager
        .append_event(session, model_changed("account-model", Some("go-account")))
        .await
        .unwrap();
    manager.shutdown_catalog_updates().await;
    drop(manager);

    let reopened = SessionManager::persistent(root.path()).unwrap();
    let selection = reopened.current_runtime_selection(session).await.unwrap();
    assert_eq!(selection.model_id.as_deref(), Some("account-model"));
    assert_eq!(selection.account_reference.as_deref(), Some("go-account"));
    reopened.shutdown_catalog_updates().await;
}
