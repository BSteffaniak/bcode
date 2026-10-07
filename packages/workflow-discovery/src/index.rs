//! Synchronous request-local access to structured Switchy storage.

use switchy_database::schema::{Column, DataType, create_table};
use switchy_database::{Database, DatabaseValue, Row};

use crate::WorkflowDiscoveryError;

/// Open private disposable storage; returns typed opener errors.
pub fn open(path: &std::path::Path) -> Result<Box<dyn Database>, WorkflowDiscoveryError> {
    Ok(Box::new(
        switchy_database_connection::init_sqlite_disposable(path)?,
    ))
}

/// Install one private index table; returns schema execution errors.
pub fn table(
    db: &dyn Database,
    name: &str,
    columns: &[(&str, DataType)],
    keys: &[&str],
) -> Result<(), WorkflowDiscoveryError> {
    let mut query = create_table(name);
    for (name, data_type) in columns {
        query = query.column(Column {
            name: (*name).into(),
            data_type: data_type.clone(),
            nullable: false,
            default: None,
            auto_increment: false,
        });
    }
    futures::executor::block_on(query.primary_key_columns(keys.iter().copied()).execute(db))?;
    Ok(())
}

/// Decode a required text column, rejecting inconsistent index rows.
pub fn text(row: &Row, name: &str) -> Result<String, WorkflowDiscoveryError> {
    match row.get(name) {
        Some(DatabaseValue::String(value)) => Ok(value),
        _ => Err(WorkflowDiscoveryError::Invalid(format!(
            "invalid index text: {name}"
        ))),
    }
}

/// Decode a nonnegative integer column, rejecting inconsistent index rows.
pub fn integer(row: &Row, name: &str) -> Result<usize, WorkflowDiscoveryError> {
    match row.get(name) {
        Some(DatabaseValue::Int64(value)) => usize::try_from(value)
            .map_err(|_| WorkflowDiscoveryError::Invalid(format!("invalid index integer: {name}"))),
        _ => Err(WorkflowDiscoveryError::Invalid(format!(
            "invalid index integer: {name}"
        ))),
    }
}
