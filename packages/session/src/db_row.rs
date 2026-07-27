//! Strict typed accessors for current database rows.

use crate::db::{SessionDbError, SessionDbResult};

pub fn required_string(row: &switchy::database::Row, column: &str) -> SessionDbResult<String> {
    row.get(column)
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .ok_or_else(|| SessionDbError::InvalidRow {
            column: column.to_owned(),
        })
}

#[must_use]
pub fn optional_string(row: &switchy::database::Row, column: &str) -> Option<String> {
    row.get(column)
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
}

pub fn required_i64(row: &switchy::database::Row, column: &str) -> SessionDbResult<i64> {
    row.get(column)
        .and_then(|value| value.as_i64())
        .ok_or_else(|| SessionDbError::InvalidRow {
            column: column.to_owned(),
        })
}

pub fn required_non_negative_u64(
    row: &switchy::database::Row,
    column: &str,
) -> SessionDbResult<u64> {
    let value = required_i64(row, column)?;
    if value.is_negative() {
        return Err(SessionDbError::InvalidRow {
            column: column.to_owned(),
        });
    }
    Ok(value.cast_unsigned())
}

#[must_use]
pub fn optional_i64(row: &switchy::database::Row, column: &str) -> Option<i64> {
    row.get(column).and_then(|value| value.as_i64())
}

#[must_use]
pub const fn i64_to_u64(value: i64) -> u64 {
    if value.is_negative() {
        0
    } else {
        value.cast_unsigned()
    }
}
