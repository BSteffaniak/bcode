use super::*;
use switchy_database::query::FilterableQuery;

pub(in crate::representation::journal) async fn admitted(
    enabled: bool,
) -> (tempfile::TempDir, Box<dyn Database>) {
    let definition = bcode_workflow::WorkflowBuilder::new(
        "diagnostics",
        bcode_workflow::Step::task("work", |value: u32, _| async move { Ok(value) }),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    admitted_definition(enabled, &definition).await
}

pub(in crate::representation::journal) async fn admitted_definition(
    enabled: bool,
    definition: &bcode_workflow::WorkflowDefinition,
) -> (tempfile::TempDir, Box<dyn Database>) {
    let temp = tempfile::tempdir().unwrap();
    let store = crate::WorkflowStore::create_coexistence_baseline(temp.path()).unwrap();
    let path = store.path().to_path_buf();
    drop(store);
    let db = switchy_database_connection::init_sqlite_rusqlite(Some(&path)).unwrap();
    crate::representation::install(&*db).await.unwrap();
    super::super::install(&*db).await.unwrap();
    let mut request = crate::representation::tests::run("run", false);
    request.binding.as_mut().unwrap().display_label = Some("SECRET_AUTH_OWNER_PAYLOAD".into());
    let exact =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("test", definition).unwrap();
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
    crate::representation::admit_registered_observed(&*db, &request, definition, enabled)
        .await
        .unwrap();
    (temp, db)
}

#[tokio::test]
async fn optin_admission_drains_pinned_diagnostics_over_one_hundred_without_secrets() {
    let (_temp, db) = admitted(true).await;
    for index in 0..205 {
        db.update("workflow_runs_v2")
            .value("updated_at_ms", 1000_i64 + index)
            .where_eq("run_id", "run")
            .execute(&*db)
            .await
            .unwrap();
    }
    let first = page(&*db, 1, "run", 0, None, 100).await.unwrap();
    assert_eq!(first.origin, CoverageOrigin::Admission);
    assert_eq!(first.records.len(), 100);
    let through = first.through_sequence;
    db.update("workflow_runs_v2")
        .value("updated_at_ms", 9999_i64)
        .where_eq("run_id", "run")
        .execute(&*db)
        .await
        .unwrap();
    let mut records = first.records;
    let mut next = first.next_after_sequence;
    while let Some(after) = next {
        let part = page(&*db, 1, "run", after, Some(through), 100)
            .await
            .unwrap();
        assert_eq!(part.through_sequence, through);
        records.extend(part.records);
        next = part.next_after_sequence;
    }
    assert!(records.len() > 205);
    let encoded = serde_json::to_string(&records).unwrap();
    assert!(!encoded.contains("SECRET_AUTH_OWNER_PAYLOAD"));
    assert!(
        records
            .iter()
            .any(|record| matches!(record,DiagnosticRecord::Event { event }
        if event.event_type=="row_transition_v1" && event.created_at_ms==1000))
    );
    assert!(
        !records
            .iter()
            .any(|record| matches!(record,DiagnosticRecord::Event { event }
        if event.created_at_ms==9999))
    );
    assert!(
        records
            .iter()
            .any(|record| matches!(record, DiagnosticRecord::TimestampUnavailable { .. }))
    );
}

#[tokio::test]
async fn unknown_version_future_pin_and_missing_origin_are_not_complete() {
    let (_temp, db) = admitted(false).await;
    let missing = page(&*db, 1, "run", 0, None, 100).await.unwrap();
    assert_eq!(missing.origin, CoverageOrigin::NotEnabled);
    assert!(missing.records.is_empty());
    assert!(page(&*db, 2, "run", 0, None, 100).await.is_err());
    assert!(page(&*db, 1, "run", 0, Some(100), 100).await.is_err());
    assert!(page(&*db, 1, "run", 1, Some(0), 100).await.is_err());
}
