//! Synchronous boundary for structured database operations and exact typed row decoding.
use crate::WorkflowStoreError;
use switchy_database::query::SelectQuery;
use switchy_database::{Database, DatabaseTransaction, DatabaseValue, Row, TransactionMode};
use switchy_database_connection::{SqliteAccess, SqliteOpenOptions, open_sqlite_rusqlite};

/// Execute a synchronous store operation without borrowing the caller's Tokio cooperative budget.
/// The future retains its original database lease and normal mutex exclusion.
pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
    futures::executor::block_on(tokio::task::unconstrained(future))
}

pub fn begin(
    database: &dyn Database,
    mode: TransactionMode,
) -> Result<Box<dyn DatabaseTransaction>, WorkflowStoreError> {
    Ok(block_on(database.begin_transaction_with_mode(mode))?)
}

pub fn commit(transaction: Box<dyn DatabaseTransaction>) -> Result<(), WorkflowStoreError> {
    Ok(block_on(transaction.commit())?)
}

pub fn open(
    path: &std::path::Path,
    access: SqliteAccess,
    busy_timeout: std::time::Duration,
    foreign_keys: bool,
) -> Result<switchy_database::rusqlite::RusqliteDatabase, WorkflowStoreError> {
    let options = SqliteOpenOptions {
        access,
        allow_uri: false,
        busy_timeout,
        foreign_keys,
        connections: std::num::NonZeroUsize::MIN,
    };
    Ok(open_sqlite_rusqlite(path, &options)?)
}

pub fn rows(
    database: &dyn Database,
    query: &SelectQuery<'_>,
) -> Result<Vec<Row>, WorkflowStoreError> {
    Ok(block_on(database.query(query))?)
}

pub fn text(row: &Row, column: &str) -> Result<String, WorkflowStoreError> {
    match row.get(column) {
        Some(DatabaseValue::String(value)) => Ok(value),
        _ => Err(invalid_column(column)),
    }
}

pub fn optional_text(row: &Row, column: &str) -> Result<Option<String>, WorkflowStoreError> {
    match row.get(column) {
        Some(DatabaseValue::Null) => Ok(None),
        Some(DatabaseValue::String(value)) => Ok(Some(value)),
        _ => Err(invalid_column(column)),
    }
}

pub fn optional_unsigned(row: &Row, column: &str) -> Result<Option<u64>, WorkflowStoreError> {
    match row.get(column) {
        Some(DatabaseValue::Null) => Ok(None),
        Some(DatabaseValue::Int64(value)) if value >= 0 => Ok(Some(value.cast_unsigned())),
        _ => Err(invalid_column(column)),
    }
}

pub fn integer_value(value: u64) -> Result<DatabaseValue, WorkflowStoreError> {
    i64::try_from(value).map(DatabaseValue::Int64).map_err(|_| {
        WorkflowStoreError::InvalidData("workflow integer exceeds SQLite storage range".into())
    })
}

pub fn optional_integer_value(value: Option<u64>) -> Result<DatabaseValue, WorkflowStoreError> {
    value
        .map(integer_value)
        .transpose()
        .map(|value| value.unwrap_or(DatabaseValue::Null))
}

pub fn unsigned(row: &Row, column: &str) -> Result<u64, WorkflowStoreError> {
    match row.get(column) {
        Some(DatabaseValue::Int64(value)) if value >= 0 => Ok(value.cast_unsigned()),
        _ => Err(invalid_column(column)),
    }
}

fn invalid_column(column: &str) -> WorkflowStoreError {
    WorkflowStoreError::InvalidData(format!("invalid workflow database column: {column}"))
}

/// Build canonical named projections for a bounded query's plain columns.
#[must_use]
pub fn projections(columns: &[&str]) -> Vec<switchy_database::query::Projection> {
    columns
        .iter()
        .map(|column| {
            switchy_database::query::Projection::new(switchy_database::query::identifier(column))
                .alias(*column)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn synchronous_queries_do_not_exhaust_the_callers_cooperative_budget() {
        let db = open(
            std::path::Path::new(":memory:"),
            SqliteAccess::ReadWriteCreate,
            std::time::Duration::from_secs(1),
            true,
        )
        .expect("database");
        let transaction = begin(&db, TransactionMode::Deferred).expect("lease");
        for _ in 0..512 {
            assert!(!block_on(transaction.trigger_exists("missing")).expect("typed probe"));
        }
        commit(transaction).expect("release lease");
        assert!(block_on(db.sqlite_is_autocommit()).expect("released"));
    }
}
