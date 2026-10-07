//! Read-only maintenance backup verification behavior.
use super::{open_verified_backup, verify_reset_backup};
use switchy_database::Database;
use switchy_database::schema::{Column, DataType, create_table};
use switchy_database_connection::{SqliteAccess, SqliteOpenOptions, open_sqlite_rusqlite};

#[test]
fn missing_or_corrupt_backup_is_preserved_and_refused() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("missing.db");
    assert!(verify_reset_backup(&missing).is_err());
    assert!(!missing.exists());

    let corrupt = directory.path().join("corrupt.db");
    let bytes = b"not a SQLite database";
    std::fs::write(&corrupt, bytes).unwrap();
    assert!(verify_reset_backup(&corrupt).is_err());
    assert_eq!(std::fs::read(&corrupt).unwrap(), bytes);
}

#[test]
fn valid_backup_is_verified_without_mutation_and_handle_is_read_only() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("backup.db");
    let options = SqliteOpenOptions {
        access: SqliteAccess::ReadWriteCreate,
        allow_uri: false,
        busy_timeout: std::time::Duration::from_secs(5),
        foreign_keys: false,
        connections: std::num::NonZeroUsize::MIN,
    };
    let writer = open_sqlite_rusqlite(&path, &options).unwrap();
    let transaction =
        super::database::begin(&writer, switchy_database::TransactionMode::Immediate).unwrap();
    futures::executor::block_on(
        create_table("backup_fixture")
            .column(Column {
                name: "identity".into(),
                data_type: DataType::Int,
                nullable: false,
                default: None,
                auto_increment: false,
            })
            .execute(&*transaction),
    )
    .unwrap();
    super::database::commit(transaction).unwrap();
    drop(writer);
    let before = std::fs::read(&path).unwrap();
    assert_eq!(
        verify_reset_backup(&path).unwrap(),
        super::sha256_hex(&before)
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);

    let reader = open_verified_backup(&path).unwrap();
    assert!(
        futures::executor::block_on(
            reader
                .insert("backup_fixture")
                .value("identity", 1_i64)
                .execute(&reader)
        )
        .is_err()
    );
    let transaction =
        super::database::begin(&reader, switchy_database::TransactionMode::Deferred).unwrap();
    let rows = super::database::rows(
        &*transaction,
        &switchy_database::query::select("backup_fixture")
            .columns(&["identity"])
            .limit(1),
    )
    .unwrap();
    assert!(rows.is_empty());
    super::database::commit(transaction).unwrap();
    drop(reader);
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn structured_baseline_initialization_preserves_both_compatibility_contracts() {
    use switchy_database::query::FilterableQuery as _;
    for version in [42_u32, 43] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("workflow.db");
        let db = super::database::open(
            &path,
            SqliteAccess::ReadWriteCreate,
            std::time::Duration::ZERO,
            true,
        )
        .unwrap();
        super::schema::initialize(&db, version).unwrap();
        super::storage_compatibility::verify(&db).unwrap();
        let rows = super::database::rows(
            &db,
            &switchy_database::query::select("workflow_store_contract")
                .where_eq("contract_id", 1_i64)
                .limit(1),
        )
        .unwrap();
        assert_eq!(
            super::database::unsigned(&rows[0], "schema_version").unwrap(),
            u64::from(version)
        );
        let columns = futures::executor::block_on(db.get_table_columns("workflow_runs")).unwrap();
        assert_eq!(
            columns
                .iter()
                .find(|column| column.name == "concurrency_cap")
                .unwrap()
                .nullable,
            version == 43
        );
        let violations = futures::executor::block_on(db.sqlite_foreign_key_check()).unwrap();
        assert!(violations.is_empty());
    }
}

#[test]
fn typed_lock_errors_retain_retry_classification() {
    for error in [
        switchy_database::DatabaseError::Busy,
        switchy_database::DatabaseError::Locked,
    ] {
        assert!(
            super::WorkflowStoreError::StructuredDatabase(error)
                .is_transient_initialization_failure()
        );
    }
    assert!(
        !super::WorkflowStoreError::InvalidData("corrupt contract".into())
            .is_transient_initialization_failure()
    );
}
