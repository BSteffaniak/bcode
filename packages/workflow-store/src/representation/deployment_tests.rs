use super::*;

#[tokio::test]
async fn explicit_identity_preserves_exact_content_and_hash_identity_is_verified() {
    let db = super::tests::database().await;
    install(&*db).await.unwrap();
    let definition = bcode_workflow::WorkflowBuilder::new(
        "explicit",
        bcode_workflow::Step::task("seed", |v: u32, _| async move { Ok(v) }),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let mut request = super::tests::run("explicit", false);
    request.definition_id = "baseline-coexistence".into();
    request.definition_version = 1;
    admit(&*db, &request, &definition).await.unwrap();
    admit(&*db, &request, &definition).await.unwrap();
    let rows = db
        .select(RUNS)
        .where_eq("run_id", "explicit")
        .execute(&*db)
        .await
        .unwrap();
    let json = serde_json::to_string(&definition).unwrap();
    assert_eq!(
        rows[0].get("definition_sha256"),
        Some(DatabaseValue::String(crate::sha256_hex(json.as_bytes())))
    );
    let mut changed = definition.clone();
    changed.nodes.get_mut("seed").unwrap().configuration = serde_json::json!({"changed":true});
    assert!(admit(&*db, &request, &changed).await.is_err());
    let hash = bcode_workflow::WorkflowDefinitionIdentity::for_definition("explicit", &definition)
        .unwrap();
    request.run_id = "hashed".into();
    request.definition_id = hash.definition_id;
    request.definition_version = hash.definition_version;
    assert!(admit(&*db, &request, &changed).await.is_err());
    admit(&*db, &request, &definition).await.unwrap();
}

#[tokio::test]
async fn production_initialization_preserves_baseline_handles_during_install_and_admission() {
    let temp = tempfile::tempdir().unwrap();
    let original = crate::WorkflowStore::initialize_in_state_dir(temp.path(), 1).unwrap();
    let retained = crate::WorkflowStore::open_in_state_dir(temp.path()).unwrap();
    let db = switchy_database_connection::init_sqlite_rusqlite(Some(original.path())).unwrap();
    assert_eq!(
        admission_family(&*db).await.unwrap(),
        AdmissionFamily::Baseline42Installable
    );
    install(&*db).await.unwrap();
    let definition = bcode_workflow::WorkflowBuilder::new(
        "test",
        bcode_workflow::Step::task("seed", |v: u32, _| async move { Ok(v) }),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let mut request = tests::run("new-production", false);
    let identity =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("test", &definition).unwrap();
    request.definition_id = identity.definition_id;
    request.definition_version = identity.definition_version;
    admit(&*db, &request, &definition).await.unwrap();
    assert!(matches!(
        runs::lookup(&*db, &request.run_id).await.unwrap(),
        runs::RunLookup::Available(_)
    ));
    let reopened = crate::WorkflowStore::initialize_in_state_dir(temp.path(), 2).unwrap();
    assert_eq!(retained.path(), reopened.path());
    assert_eq!(crate::detected_store_schema(&retained.connection), Some(42));
    assert_eq!(crate::detected_store_schema(&original.connection), Some(42));
    assert_eq!(
        admission_family(&*db).await.unwrap(),
        AdmissionFamily::Installed
    );
}

#[tokio::test]
async fn production_initialization_never_downgrades_existing_43() {
    let temp = tempfile::tempdir().unwrap();
    // The historical explicit open creates the shipped compatibility3 schema.
    let historical = crate::WorkflowStore::open_in_state_dir(temp.path()).unwrap();
    assert_eq!(
        crate::detected_store_schema(&historical.connection),
        Some(43)
    );
    let initialized = crate::WorkflowStore::initialize_in_state_dir(temp.path(), 2).unwrap();
    assert_eq!(
        crate::detected_store_schema(&initialized.connection),
        Some(43)
    );
    let db = switchy_database_connection::init_sqlite_rusqlite(Some(initialized.path())).unwrap();
    assert_eq!(
        admission_family(&*db).await.unwrap(),
        AdmissionFamily::Historical43
    );
    assert!(install(&*db).await.is_err());
    assert_eq!(
        crate::detected_store_schema(&historical.connection),
        Some(43)
    );
}
