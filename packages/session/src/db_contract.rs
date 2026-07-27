//! Strict current writer-contract validation.

use crate::db::{SessionDbError, SessionDbResult};
use switchy::database::{Database, query::FilterableQuery};

pub async fn validate_writer_contract(
    db: &dyn Database,
    expected_writer_epoch: u32,
    expected_schema_version: u32,
    legacy_writer_epoch: u32,
    contract_id: i32,
) -> SessionDbResult<()> {
    let row = db
        .select("session_storage_contract")
        .columns(&["schema_version", "writer_epoch"])
        .where_eq("contract_id", contract_id)
        .execute_first(db)
        .await?;
    let Some(row) = row.as_ref() else {
        return Err(SessionDbError::WriterIncompatible {
            actual: Some(u64::from(legacy_writer_epoch)),
            expected: u64::from(expected_writer_epoch),
        });
    };
    let schema_version = non_negative_u64(row, "schema_version")?;
    if schema_version != u64::from(expected_schema_version) {
        return Err(SessionDbError::ProjectionIncompatible {
            projection: "session_storage_contract",
            actual: schema_version,
            expected: u64::from(expected_schema_version),
        });
    }
    let actual = non_negative_u64(row, "writer_epoch")?;
    let expected = u64::from(expected_writer_epoch);
    if actual != expected {
        return Err(SessionDbError::WriterIncompatible {
            actual: Some(actual),
            expected,
        });
    }
    Ok(())
}

fn non_negative_u64(row: &switchy::database::Row, column: &str) -> SessionDbResult<u64> {
    let value = row
        .get(column)
        .and_then(|value| value.as_i64())
        .ok_or_else(|| SessionDbError::InvalidRow {
            column: column.to_owned(),
        })?;
    if value.is_negative() {
        return Err(SessionDbError::InvalidRow {
            column: column.to_owned(),
        });
    }
    Ok(value.cast_unsigned())
}
