//! Connection, schema creation and the synchronous boundary over Switchy's async API.

use std::path::Path;
use std::time::Duration;

use switchy_database::query::FilterableQuery as _;
use switchy_database::schema::{Column, DataType, create_index, create_table};
use switchy_database::{Database, DatabaseValue, TransactionMode};
use switchy_database_connection::{SqliteAccess, SqliteOpenOptions, open_sqlite_rusqlite};

use crate::{FeedbackStoreError, SCHEMA_VERSION};

/// Run a store future to completion on the calling thread.
///
/// Store calls come from synchronous plugin service handlers; the future keeps its database
/// lease and runs without borrowing the caller's Tokio cooperative budget.
pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
    futures::executor::block_on(tokio::task::unconstrained(future))
}

pub fn open(
    path: &Path,
    busy_timeout: Duration,
) -> Result<switchy_database::rusqlite::RusqliteDatabase, FeedbackStoreError> {
    let options = SqliteOpenOptions {
        access: SqliteAccess::ReadWriteCreate,
        allow_uri: false,
        busy_timeout,
        foreign_keys: true,
        connections: std::num::NonZeroUsize::MIN,
    };
    Ok(open_sqlite_rusqlite(path, &options)?)
}

/// Create the schema on first open and reject unknown versions without modifying them.
pub async fn initialize(database: &dyn Database) -> Result<(), FeedbackStoreError> {
    let tx = database
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await?;
    let existing = tx
        .select("sqlite_master")
        .columns(&["name"])
        .where_eq("type", "table")
        .where_eq("name", "feedback_meta")
        .execute(&*tx)
        .await?;
    if existing.is_empty() {
        create_schema(&*tx).await?;
        tx.insert("feedback_meta")
            .value("id", 1)
            .value("version", SCHEMA_VERSION)
            .value("intake_session_id", DatabaseValue::Null)
            .execute(&*tx)
            .await?;
    }
    let meta = tx
        .select("feedback_meta")
        .where_eq("id", 1)
        .execute_first(&*tx)
        .await?
        .ok_or_else(|| FeedbackStoreError::Corrupt("missing meta row".into()))?;
    let version = meta
        .get("version")
        .and_then(|value| value.as_i64())
        .ok_or_else(|| FeedbackStoreError::Corrupt("schema version".into()))?;
    if version != SCHEMA_VERSION {
        tx.rollback().await?;
        return Err(FeedbackStoreError::UnsupportedSchema(version));
    }
    tx.commit().await?;
    Ok(())
}

async fn create_schema(db: &dyn Database) -> Result<(), FeedbackStoreError> {
    create_table("feedback_meta")
        .columns(vec![
            column("id", DataType::BigInt, false),
            column("version", DataType::BigInt, false),
            column("intake_session_id", DataType::Text, true),
        ])
        .primary_key("id")
        .check_integer_equals("id", 1)
        .execute(db)
        .await?;
    create_table("feedback_items")
        .columns(vec![
            Column {
                auto_increment: true,
                ..column("id", DataType::BigInt, false)
            },
            column("app", DataType::Text, false),
            column("text", DataType::Text, false),
            column("created_at_ms", DataType::BigInt, false),
            column("updated_at_ms", DataType::BigInt, false),
            column("triage_state", DataType::Text, false),
            column("triage_json", DataType::Text, true),
            column("status", DataType::Text, false),
            column("status_change_json", DataType::Text, true),
        ])
        .primary_key("id")
        .execute(db)
        .await?;
    create_table("feedback_facets")
        .columns(vec![
            column("item_id", DataType::BigInt, false),
            column("key", DataType::Text, false),
            column("value", DataType::Text, false),
            column("sensitive", DataType::BigInt, false),
        ])
        .primary_key_columns(["item_id", "key"])
        .foreign_key_column("item_id", "feedback_items", "id")
        .execute(db)
        .await?;
    create_table("feedback_links")
        .columns(vec![
            column("from_id", DataType::BigInt, false),
            column("to_id", DataType::BigInt, false),
            column("kind", DataType::Text, false),
        ])
        .primary_key_columns(["from_id", "to_id", "kind"])
        .foreign_key_column("from_id", "feedback_items", "id")
        .foreign_key_column("to_id", "feedback_items", "id")
        .execute(db)
        .await?;
    create_table("feedback_attachments")
        .columns(vec![
            column("item_id", DataType::BigInt, false),
            column("ordinal", DataType::BigInt, false),
            column("name", DataType::Text, false),
            column("kind", DataType::Text, false),
            column("media_type", DataType::Text, false),
            column("bytes", DataType::BigInt, false),
            column("description", DataType::Text, true),
            column("sensitive", DataType::BigInt, false),
            column("stored_path", DataType::Text, false),
            column("created_at_ms", DataType::BigInt, false),
        ])
        .primary_key_columns(["item_id", "ordinal"])
        .foreign_key_column("item_id", "feedback_items", "id")
        .execute(db)
        .await?;
    for (name, table, columns) in [
        ("feedback_items_app", "feedback_items", vec!["app", "id"]),
        (
            "feedback_items_status",
            "feedback_items",
            vec!["status", "id"],
        ),
        (
            "feedback_facets_key_value",
            "feedback_facets",
            vec!["key", "value"],
        ),
    ] {
        create_index(name)
            .table(table)
            .columns(columns)
            .execute(db)
            .await?;
    }
    Ok(())
}

fn column(name: &str, data_type: DataType, nullable: bool) -> Column {
    Column {
        name: name.into(),
        nullable,
        auto_increment: false,
        data_type,
        default: None,
    }
}
