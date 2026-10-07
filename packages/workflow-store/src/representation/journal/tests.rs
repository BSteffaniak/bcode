//! Behavioral observer tests: ordinary typed writers need no journal ceremony.
use super::*;
use switchy_database::query::FilterableQuery;

async fn fixture() -> (tempfile::TempDir, Box<dyn Database>) {
    let temp = tempfile::tempdir().unwrap();
    let legacy = crate::WorkflowStore::create_coexistence_baseline(temp.path()).unwrap();
    let path = legacy.path().to_path_buf();
    drop(legacy);
    let db = switchy_database_connection::init_sqlite_rusqlite(Some(&path)).unwrap();
    super::super::install(&*db).await.unwrap();
    (temp, db)
}

async fn graph(db: &dyn Database, run: &str) {
    db.insert("workflow_graphs_v2")
        .value("run_id", run)
        .value("revision", 1_i64)
        .value("terminal_json", DatabaseValue::Null)
        .value("definition_json", "private-not-observed")
        .value("edges_json", "private-not-observed")
        .execute(db)
        .await
        .unwrap();
}

#[tokio::test]
async fn transactional_text_bounds_preserve_source_commits_and_page_rows() {
    let (_temp, db) = fixture().await;
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await
        .unwrap();
    enable_admission(&*tx, "bounded").await.unwrap();
    db.insert("workflow_runs_v2")
        .value("run_id", "bounded")
        .value("single_active", false)
        .value("status", "x".repeat(513))
        .value("updated_at_ms", 1_i64)
        .value("admission_json", "private")
        .value("definition_json", "private")
        .value("definition_sha256", "private")
        .value("consumed", 0_i64)
        .value("active_attempts", 0_i64)
        .execute(&*tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    for value in ["é".repeat(256), "é".repeat(257)] {
        let tx = db
            .begin_transaction_with_mode(TransactionMode::Immediate)
            .await
            .unwrap();
        tx.update("workflow_runs_v2")
            .value("status", value)
            .where_eq("run_id", "bounded")
            .execute(&*tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
    let first = page(&*db, "bounded", 0, None, 1).await.unwrap();
    assert_eq!(first.observations[0].operation, "insert");
    assert_eq!(
        first.observations[0].facts["status"],
        ScalarFact::Unavailable(FactUnavailableReason::Oversized)
    );
    let second = page(
        &*db,
        "bounded",
        first.next_after_sequence.unwrap(),
        Some(first.through_sequence),
        1,
    )
    .await
    .unwrap();
    assert_eq!(
        second.observations[0].facts["status"],
        ScalarFact::Text("é".repeat(256))
    );
    let third = page(
        &*db,
        "bounded",
        second.next_after_sequence.unwrap(),
        Some(first.through_sequence),
        1,
    )
    .await
    .unwrap();
    assert_eq!(third.observations[0].operation, "update");
    assert_eq!(
        third.observations[0].facts["status"],
        ScalarFact::Unavailable(FactUnavailableReason::Oversized)
    );
    assert!(third.next_after_sequence.is_none());
    let source = db
        .select("workflow_runs_v2")
        .columns(&["status"])
        .where_eq("run_id", "bounded")
        .limit(1)
        .execute(&*db)
        .await
        .unwrap();
    assert_eq!(
        source[0].get("status"),
        Some(DatabaseValue::String("é".repeat(257)))
    );
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await
        .unwrap();
    tx.delete("workflow_runs_v2")
        .where_eq("run_id", "bounded")
        .execute(&*tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let deleted = page(&*db, "bounded", first.through_sequence, None, 1)
        .await
        .unwrap();
    assert_eq!(deleted.observations[0].operation, "delete");
    assert_eq!(
        deleted.observations[0].facts["status"],
        ScalarFact::Unavailable(FactUnavailableReason::Oversized)
    );
}

#[tokio::test]
async fn old_writer_large_numeric_and_timestamp_text_never_enters_journal() {
    let temp = tempfile::tempdir().unwrap();
    let legacy = crate::WorkflowStore::create_coexistence_baseline(temp.path()).unwrap();
    let path = legacy.path().to_path_buf();
    drop(legacy);
    let db = switchy_database_connection::init_sqlite_rusqlite(Some(&path)).unwrap();
    super::super::install(&*db).await.unwrap();
    let writer = switchy_database_connection::init_sqlite_rusqlite(Some(&path)).unwrap();
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await
        .unwrap();
    enable_admission(&*tx, "numeric-bound").await.unwrap();
    tx.commit().await.unwrap();
    let large = "not-an-integer".repeat(100_000);
    let tx = writer
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await
        .unwrap();
    tx.insert("workflow_runs_v2")
        .value("run_id", "numeric-bound")
        .value("single_active", false)
        .value("status", "running")
        .value("updated_at_ms", large.clone())
        .value("admission_json", "private")
        .value("definition_json", "private")
        .value("definition_sha256", "private")
        .value("consumed", large.clone())
        .value("active_attempts", 0_i64)
        .execute(&*tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let tx = writer
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await
        .unwrap();
    tx.update("workflow_runs_v2")
        .value("updated_at_ms", 42_i64)
        .value("consumed", 7_i64)
        .where_eq("run_id", "numeric-bound")
        .execute(&*tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let first = page(&*db, "numeric-bound", 0, None, 1).await.unwrap();
    let observation = &first.observations[0];
    assert_eq!(
        observation.facts["consumed"],
        ScalarFact::Unavailable(FactUnavailableReason::UnsupportedType)
    );
    assert_eq!(
        observation.facts["updated_at_ms"],
        ScalarFact::Unavailable(FactUnavailableReason::UnsupportedType)
    );
    assert_eq!(observation.timestamp_ms, None);
    assert_eq!(observation.timestamp_source, None);
    let second = page(
        &*db,
        "numeric-bound",
        first.next_after_sequence.unwrap(),
        Some(first.through_sequence),
        1,
    )
    .await
    .unwrap();
    assert_eq!(
        second.observations[0].facts["consumed"],
        ScalarFact::Integer(7)
    );
    assert_eq!(
        second.observations[0].facts["previous_updated_at_ms"],
        ScalarFact::Unavailable(FactUnavailableReason::UnsupportedType)
    );
    assert_eq!(second.observations[0].timestamp_ms, None);
    let stored = db
        .select(EVENTS)
        .where_eq("run_id", "numeric-bound")
        .limit(3)
        .execute(&*db)
        .await
        .unwrap();
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[0].get("count"), Some(DatabaseValue::Null));
    assert_eq!(stored[0].get("timestamp_ms"), Some(DatabaseValue::Null));
    assert_eq!(
        stored[1].get("previous_timestamp_ms"),
        Some(DatabaseValue::Null)
    );
    for row in stored {
        for (_, value) in row.columns {
            if let DatabaseValue::String(value) = value {
                assert!(value.len() <= 512);
            }
        }
    }
}

#[tokio::test]
async fn unsupported_integer_fact_preserves_committed_observation() {
    let (_temp, db) = fixture().await;
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await
        .unwrap();
    enable_admission(&*tx, "unsupported").await.unwrap();
    tx.insert("workflow_graphs_v2")
        .value("run_id", "unsupported")
        .value("revision", DatabaseValue::Real64(1.5))
        .value("terminal_json", DatabaseValue::Null)
        .value("definition_json", "private")
        .value("edges_json", "private")
        .execute(&*tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let observed = page(&*db, "unsupported", 0, None, 1).await.unwrap();
    assert_eq!(observed.observations[0].operation, "insert");
    assert_eq!(
        observed.observations[0].facts["revision"],
        ScalarFact::Unavailable(FactUnavailableReason::UnsupportedType)
    );
}

#[test]
fn text_status_decoding_distinguishes_null_unavailable_and_damage() {
    let row = |status, value| switchy_database::Row {
        columns: vec![
            (
                "source_table".into(),
                DatabaseValue::String("workflow_runs_v2".into()),
            ),
            ("operation".into(), DatabaseValue::String("insert".into())),
            ("run_id".into(), DatabaseValue::String("bounded".into())),
            ("sequence".into(), DatabaseValue::Int64(1)),
            ("status_status".into(), DatabaseValue::Int64(status)),
            ("status".into(), value),
            ("count".into(), DatabaseValue::Int64(7)),
            ("count_status".into(), DatabaseValue::Int64(1)),
            ("active_attempts_status".into(), DatabaseValue::Int64(0)),
            ("timestamp_ms_status".into(), DatabaseValue::Int64(0)),
            ("active_attempts".into(), DatabaseValue::Null),
            ("timestamp_ms".into(), DatabaseValue::Null),
            ("previous_timestamp_ms".into(), DatabaseValue::Null),
        ],
    };
    for (status, expected) in [
        (0, ScalarFact::Null),
        (2, ScalarFact::Unavailable(FactUnavailableReason::Oversized)),
        (
            3,
            ScalarFact::Unavailable(FactUnavailableReason::UnsupportedType),
        ),
    ] {
        let decoded = observers::decode(&row(status, DatabaseValue::Null)).unwrap();
        assert_eq!(decoded.facts["status"], expected);
        assert_eq!(decoded.facts["consumed"], ScalarFact::Integer(7));
        assert_eq!(decoded.facts["active_attempts"], ScalarFact::Null);
    }
    for (status, value) in [
        (4, DatabaseValue::Null),
        (1, DatabaseValue::Null),
        (0, DatabaseValue::String("not-null".into())),
        (1, DatabaseValue::String("x".repeat(513))),
        (3, DatabaseValue::Int64(7)),
    ] {
        assert!(matches!(
            observers::decode(&row(status, value)),
            Err(Error::MaintenanceRequired)
        ));
    }
    let mut missing = row(0, DatabaseValue::Null);
    missing.columns.retain(|(name, _)| name != "status_status");
    assert!(matches!(
        observers::decode(&missing),
        Err(Error::MaintenanceRequired)
    ));
}

#[tokio::test]
async fn rollback_opt_in_filter_and_keyset_pages_are_truthful() {
    let (_temp, db) = fixture().await;
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await
        .unwrap();
    enable_admission(&*tx, "rolled-back").await.unwrap();
    graph(&*tx, "rolled-back").await;
    tx.rollback().await.unwrap();
    assert_eq!(
        page(&*db, "rolled-back", 0, None, 1).await.unwrap().origin,
        CoverageOrigin::NotEnabled
    );
    graph(&*db, "independent").await;
    assert!(
        page(&*db, "independent", 0, None, 1)
            .await
            .unwrap()
            .observations
            .is_empty()
    );
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await
        .unwrap();
    enable_admission(&*tx, "observed").await.unwrap();
    graph(&*tx, "observed").await;
    tx.commit().await.unwrap();
    db.update("workflow_graphs_v2")
        .value("revision", 2_i64)
        .where_eq("run_id", "observed")
        .execute(&*db)
        .await
        .unwrap();
    let first = page(&*db, "observed", 0, None, 1).await.unwrap();
    assert_eq!(first.origin, CoverageOrigin::Admission);
    assert_eq!(first.observations[0].operation, "insert");
    assert_eq!(first.observations[0].timestamp_ms, None);
    assert_eq!(
        first.observations[0].facts["revision"],
        ScalarFact::Integer(1)
    );
    let cursor = first.next_after_sequence.unwrap();
    db.delete("workflow_graphs_v2")
        .where_eq("run_id", "observed")
        .execute(&*db)
        .await
        .unwrap();
    let second = page(&*db, "observed", cursor, Some(first.through_sequence), 1)
        .await
        .unwrap();
    assert_eq!(second.observations[0].operation, "update");
    assert_eq!(
        second.observations[0].facts["revision"],
        ScalarFact::Integer(2)
    );
    assert!(second.next_after_sequence.is_none());
    let final_page = page(&*db, "observed", second.through_sequence, None, 1)
        .await
        .unwrap();
    assert_eq!(final_page.observations[0].operation, "delete");
    assert!(final_page.observations[0].sequence > cursor);
    assert!(
        !serde_json::to_string(&first)
            .unwrap()
            .contains("private-not-observed")
    );
}

#[tokio::test]
async fn installation_is_idempotent_and_conflicting_version_fails_closed() {
    let (_temp, db) = fixture().await;
    install(&*db).await.unwrap();
    assert!(
        db.update(CONTRACT)
            .value("version", 2_i64)
            .where_eq("version", VERSION)
            .execute(&*db)
            .await
            .is_err()
    );
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await
        .unwrap();
    enable_admission(&*tx, "immutable").await.unwrap();
    graph(&*tx, "immutable").await;
    tx.commit().await.unwrap();
    for table in [COVERAGE, EVENTS] {
        assert!(
            db.delete(table)
                .where_eq("run_id", "immutable")
                .execute(&*db)
                .await
                .is_err()
        );
        assert!(
            db.update(table)
                .value("run_id", "rewritten")
                .where_eq("run_id", "immutable")
                .execute(&*db)
                .await
                .is_err()
        );
    }
    assert_eq!(
        page(&*db, "immutable", 0, None, 100)
            .await
            .unwrap()
            .observations
            .len(),
        1
    );
    assert!(
        db.delete(CONTRACT)
            .where_eq("version", VERSION)
            .execute(&*db)
            .await
            .is_err()
    );
    let versions = db.select(CONTRACT).limit(2).execute(&*db).await.unwrap();
    assert_eq!(versions.len(), 1);
    assert_eq!(
        versions[0].get("version"),
        Some(DatabaseValue::Int64(VERSION))
    );
    db.insert(CONTRACT)
        .value("version", 2_i64)
        .execute(&*db)
        .await
        .unwrap();
    assert!(matches!(
        install(&*db).await,
        Err(Error::MaintenanceRequired)
    ));
    assert!(matches!(
        page(&*db, "historical", 0, None, 1).await,
        Err(Error::MaintenanceRequired)
    ));
}

#[tokio::test]
async fn independent_writer_connection_emits_without_journal_api() {
    let temp = tempfile::tempdir().unwrap();
    let legacy = crate::WorkflowStore::create_coexistence_baseline(temp.path()).unwrap();
    let path = legacy.path().to_path_buf();
    drop(legacy);
    let db = switchy_database_connection::init_sqlite_rusqlite(Some(&path)).unwrap();
    super::super::install(&*db).await.unwrap();
    let writer = switchy_database_connection::init_sqlite_rusqlite(Some(&path)).unwrap();
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await
        .unwrap();
    enable_admission(&*tx, "old-writer").await.unwrap();
    tx.commit().await.unwrap();
    graph(&*writer, "old-writer").await;
    let observed = page(&*db, "old-writer", 0, None, 100).await.unwrap();
    assert_eq!(observed.observations.len(), 1);
    assert_eq!(observed.observations[0].operation, "insert");
}

#[tokio::test]
async fn admission_opt_in_is_atomic_and_idempotent_without_backfill() {
    let (_temp, db) = fixture().await;
    let definition = bcode_workflow::WorkflowBuilder::new(
        "journal-admission",
        bcode_workflow::Step::task("work", |value: u32, _| async move { Ok(value) }),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let mut request = super::super::tests::run("new-admission", false);
    let exact =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("test", &definition).unwrap();
    request.definition_id = exact.definition_id;
    request.definition_version = exact.definition_version;
    let json = serde_json::to_string(&definition).unwrap();
    db.insert("workflow_definitions")
        .value("definition_id", request.definition_id.as_str())
        .value("version", i64::from(request.definition_version))
        .value("definition_json", json.clone())
        .value("checksum_sha256", crate::sha256_hex(json.as_bytes()))
        .execute(&*db)
        .await
        .unwrap();
    let mut default_request = request.clone();
    default_request.run_id = "default-admission".into();
    super::super::admit_registered(&*db, &default_request, &definition)
        .await
        .unwrap();
    let default_page = page(&*db, &default_request.run_id, 0, None, 100)
        .await
        .unwrap();
    assert_eq!(default_page.origin, CoverageOrigin::NotEnabled);
    assert!(default_page.observations.is_empty());
    assert!(
        super::super::admit_registered_observed(&*db, &default_request, &definition, true)
            .await
            .unwrap()
            .already_present
    );
    assert_eq!(
        page(&*db, &default_request.run_id, 0, None, 100)
            .await
            .unwrap(),
        default_page
    );
    let mut direct_request = request.clone();
    direct_request.run_id = "direct-admission".into();
    super::super::admit(&*db, &direct_request, &definition)
        .await
        .unwrap();
    assert_eq!(
        page(&*db, &direct_request.run_id, 0, None, 100)
            .await
            .unwrap()
            .origin,
        CoverageOrigin::NotEnabled
    );
    assert!(
        !super::super::admit_registered_observed(&*db, &request, &definition, true)
            .await
            .unwrap()
            .already_present
    );
    let first = page(&*db, &request.run_id, 0, None, 100).await.unwrap();
    assert_eq!(first.origin, CoverageOrigin::Admission);
    assert!(
        first
            .observations
            .iter()
            .any(|row| row.source_table == super::super::RUNS && row.operation == "insert")
    );
    assert!(
        super::super::admit_registered_observed(&*db, &request, &definition, true)
            .await
            .unwrap()
            .already_present
    );
    assert_eq!(
        page(&*db, &request.run_id, 0, None, 100).await.unwrap(),
        first
    );
    assert!(
        db.delete(COVERAGE)
            .where_eq("run_id", request.run_id.as_str())
            .execute(&*db)
            .await
            .is_err()
    );
    assert!(
        super::super::admit(&*db, &request, &definition)
            .await
            .unwrap()
            .already_present
    );
    assert_eq!(
        page(&*db, &request.run_id, 0, None, 100).await.unwrap(),
        first
    );
}

#[test]
fn unchanged_source_timestamp_is_not_presented_as_transition_time() {
    use switchy_database::Row;
    let row = |operation: &str, previous: DatabaseValue| Row {
        columns: vec![
            ("sequence".into(), DatabaseValue::Int64(1)),
            ("run_id".into(), DatabaseValue::String("run".into())),
            (
                "source_table".into(),
                DatabaseValue::String("workflow_runs_v2".into()),
            ),
            ("operation".into(), DatabaseValue::String(operation.into())),
            ("status".into(), DatabaseValue::String("paused".into())),
            ("status_status".into(), DatabaseValue::Int64(1)),
            ("count".into(), DatabaseValue::Int64(1)),
            ("count_status".into(), DatabaseValue::Int64(1)),
            ("active_attempts_status".into(), DatabaseValue::Int64(1)),
            ("timestamp_ms_status".into(), DatabaseValue::Int64(1)),
            (
                "previous_timestamp_ms_status".into(),
                DatabaseValue::Int64(1),
            ),
            ("active_attempts".into(), DatabaseValue::Int64(0)),
            ("timestamp_ms".into(), DatabaseValue::Int64(42)),
            (
                "timestamp_source".into(),
                DatabaseValue::String("updated_at_ms".into()),
            ),
            ("previous_timestamp_ms".into(), previous),
        ],
    };
    let unchanged = observers::decode(&row("update", DatabaseValue::Int64(42))).unwrap();
    assert_eq!(unchanged.timestamp_ms, None);
    assert_eq!(unchanged.timestamp_source, None);
    let changed = observers::decode(&row("update", DatabaseValue::Int64(41))).unwrap();
    assert_eq!(changed.timestamp_ms, Some(42));
    assert_eq!(changed.timestamp_source.as_deref(), Some("updated_at_ms"));
}
