//! Non-mutating structured compatibility verification.
use crate::{WORKFLOW_STORE_SCHEMA_VERSION, WorkflowStoreError, database};
use switchy_database::Database;
use switchy_database::query::{FilterableQuery as _, select};

fn contract(
    db: &dyn Database,
    table: &str,
    column: &str,
) -> Result<Option<u32>, WorkflowStoreError> {
    let columns = [column];
    let rows = match database::rows(
        db,
        &select(table)
            .columns(&columns)
            .where_eq("contract_id", 1_i64)
            .limit(1),
    ) {
        Ok(rows) => rows,
        Err(error) if error.is_transient_initialization_failure() => return Err(error),
        Err(_) => return Ok(None),
    };
    Ok(rows
        .first()
        .and_then(|row| database::unsigned(row, column).ok())
        .and_then(|value| u32::try_from(value).ok()))
}

pub fn verify(db: &dyn Database) -> Result<(), WorkflowStoreError> {
    let actual = contract(db, "workflow_store_contract", "schema_version")?;
    let compatibility = contract(db, "workflow_storage_compatibility", "compatibility")?;
    if !matches!(
        (actual, compatibility),
        (Some(42..), Some(2)) | (Some(43..), Some(3))
    ) {
        return Err(WorkflowStoreError::UnsupportedStore {
            actual,
            expected: WORKFLOW_STORE_SCHEMA_VERSION,
        });
    }
    crate::recovery::verify(db)?;
    for (table, columns) in [
        (
            "workflow_continuations",
            &[
                "successor_run_id",
                "predecessor_run_id",
                "request_json",
                "lineage_json",
            ][..],
        ),
        (
            "workflow_run_packages",
            &["run_id", "package_id", "lock_digest"][..],
        ),
    ] {
        database::rows(db, &select(table).columns(columns).limit(0))?;
    }
    Ok(())
}
