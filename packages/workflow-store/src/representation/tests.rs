use super::*;

pub(super) async fn database() -> Box<dyn Database> {
    let db = switchy_database_connection::init_sqlite_rusqlite(None).unwrap();
    for (table, field, value) in [
        ("workflow_storage_compatibility", "compatibility", 2_i64),
        ("workflow_store_contract", "schema_version", 42_i64),
    ] {
        create_table(table)
            .column(column("contract_id", DataType::Int, false))
            .primary_key("contract_id")
            .column(column(field, DataType::Int, false))
            .execute(&*db)
            .await
            .unwrap();
        db.insert(table)
            .value("contract_id", 1_i64)
            .value(field, value)
            .execute(&*db)
            .await
            .unwrap();
    }
    let mut table = create_table("workflow_runs")
        .column(column("run_id", DataType::Text, false))
        .primary_key("run_id");
    for field in ["owner_plugin_id", "workflow_kind", "scope_key"] {
        table = table.column(column(field, DataType::Text, true));
    }
    table
        .column(column("single_active", DataType::Bool, false))
        .column(column("status", DataType::Text, false))
        .column(column("concurrency_cap", DataType::Int, false))
        .execute(&*db)
        .await
        .unwrap();
    create_table("workflow_replacement_intents")
        .column(column("old_run_id", DataType::Text, false))
        .primary_key("old_run_id")
        .column(column("successor_run_id", DataType::Text, false))
        .execute(&*db)
        .await
        .unwrap();
    db
}

pub(super) fn run(id: &str, single: bool) -> super::super::NewWorkflowRun {
    super::super::NewWorkflowRun {
        run_id: id.into(),
        definition_id: "test".into(),
        definition_version: 1,
        workspace_snapshot: "workspace".into(),
        parent_session_id: None,
        parent_session_generation: None,
        binding: Some(bcode_workflow::WorkflowRunBinding {
            owner_plugin_id: "test".into(),
            workflow_kind: "test".into(),
            scope_key: "scope".into(),
            display_label: None,
            single_active: single,
        }),
        authored_provenance: None,
        input: Some(serde_json::json!(1)),
        execution_authority: None,
        created_at_ms: 1,
        authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
            version: 1,
            provider_id: "test".into(),
            profile_id: "build".into(),
            policy_digest_sha256: "a".repeat(64),
        },
        authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
        limits: bcode_workflow::WorkflowRunLimits::default(),
    }
}

async fn legacy(
    db: &dyn Database,
    id: &str,
    single: bool,
    status: &str,
) -> Result<(), switchy_database::DatabaseError> {
    db.insert("workflow_runs")
        .value("run_id", id)
        .value("owner_plugin_id", "test")
        .value("workflow_kind", "test")
        .value("scope_key", "scope")
        .value("single_active", single)
        .value("status", status)
        .value("concurrency_cap", 8_i64)
        .execute(db)
        .await?;
    Ok(())
}

#[tokio::test]
async fn install_is_additive_idempotent_and_preserves_legacy_caps() {
    let db = database().await;
    legacy(&*db, "old", true, "running").await.unwrap();
    install(&*db).await.unwrap();
    install(&*db).await.unwrap();
    assert_eq!(
        identity(&*db, "old").await.unwrap(),
        Some(RunRepresentation::Legacy)
    );
    assert_eq!(identity(&*db, "absent").await.unwrap(), None);
    let rows = db.select("workflow_runs").execute(&*db).await.unwrap();
    assert_eq!(
        rows[0].get("concurrency_cap"),
        Some(DatabaseValue::Int64(8))
    );
    // This is the old writer's operation: no awareness of the extension required.
    legacy(&*db, "independent", false, "running").await.unwrap();
    db.update("workflow_storage_compatibility")
        .value("compatibility", 3_i64)
        .where_eq("contract_id", 1_i64)
        .execute(&*db)
        .await
        .unwrap();
    assert!(matches!(
        install(&*db).await,
        Err(RepresentationError::UnsupportedStore)
    ));
}

async fn insert_v2(
    db: &dyn Database,
    id: &str,
    single: bool,
) -> Result<(), switchy_database::DatabaseError> {
    db.insert(RUNS)
        .value("run_id", id)
        .value("owner_plugin_id", "test")
        .value("workflow_kind", "test")
        .value("scope_key", "scope")
        .value("single_active", single)
        .value("status", "paused")
        .value("updated_at_ms", 1_i64)
        .value("authority_json", DatabaseValue::Null)
        .value("consumed", 0_i64)
        .value("active_attempts", 0_i64)
        .value("admission_json", "{}")
        .value("definition_json", "{}")
        .value("definition_sha256", crate::sha256_hex(b"{}"))
        .execute(db)
        .await?;
    Ok(())
}

#[tokio::test]
async fn guards_preserve_directional_bindings_and_cross_family_identity() {
    let db = database().await;
    install(&*db).await.unwrap();
    insert_v2(&*db, "new", true).await.unwrap();
    assert!(legacy(&*db, "new", false, "running").await.is_err());
    assert!(legacy(&*db, "conflict", true, "running").await.is_err());
    legacy(&*db, "allowed", false, "running").await.unwrap();
    assert!(insert_v2(&*db, "allowed", false).await.is_err());
    assert!(
        db.update("workflow_runs")
            .value("single_active", true)
            .where_eq("run_id", "allowed")
            .execute(&*db)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn replacement_reserves_successor_and_terminal_predecessor_binding() {
    let db = database().await;
    install(&*db).await.unwrap();
    legacy(&*db, "old", true, "completed").await.unwrap();
    db.insert("workflow_replacement_intents")
        .value("old_run_id", "old")
        .value("successor_run_id", "reserved")
        .execute(&*db)
        .await
        .unwrap();
    assert!(insert_v2(&*db, "reserved", false).await.is_err());
    assert!(insert_v2(&*db, "bound", true).await.is_err());
    insert_v2(&*db, "unbound", false).await.unwrap();
    assert!(
        db.update("workflow_replacement_intents")
            .value("successor_run_id", "unbound")
            .where_eq("old_run_id", "old")
            .execute(&*db)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn unknown_version_is_not_overwritten() {
    let db = database().await;
    install(&*db).await.unwrap();
    db.update(CONTRACT)
        .value("version", 999_i64)
        .where_eq("representation", 2_i64)
        .execute(&*db)
        .await
        .unwrap();
    assert!(matches!(
        install(&*db).await,
        Err(RepresentationError::MaintenanceRequired)
    ));
    assert!(matches!(
        identity(&*db, "any").await,
        Err(RepresentationError::MaintenanceRequired)
    ));
    let definition = bcode_workflow::WorkflowBuilder::new(
        "admission-test",
        bcode_workflow::Step::task("work", |value: u32, _| async move { Ok(value + 1) }),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let _ = definition;
}

#[tokio::test]
async fn admission_is_idempotent_uncapped_and_never_executable() {
    let db = database().await;
    install(&*db).await.unwrap();
    let definition = bcode_workflow::WorkflowBuilder::new(
        "admission-test",
        bcode_workflow::Step::task("work", |value: u32, _| async move { Ok(value + 1) }),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let mut request = run("admitted", false);
    let exact =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("test", &definition).unwrap();
    request.definition_id = exact.definition_id;
    request.definition_version = exact.definition_version;
    let receipt = admit(&*db, &request, &definition).await.unwrap();
    assert!(!receipt.already_present);
    assert_eq!(receipt.dispatch, DispatchAvailability::UnsupportedLifecycle);
    assert!(
        admit(&*db, &request, &definition)
            .await
            .unwrap()
            .already_present
    );
    assert_eq!(
        identity(&*db, "admitted").await.unwrap(),
        Some(RunRepresentation::Version2)
    );
    assert!(
        db.select("workflow_runs")
            .execute(&*db)
            .await
            .unwrap()
            .is_empty()
    );
    let rows = db.select(RUNS).execute(&*db).await.unwrap();
    let Some(DatabaseValue::String(json)) = rows[0].get("admission_json") else {
        panic!("admission")
    };
    let stored: super::super::NewWorkflowRun = serde_json::from_str(&json).unwrap();
    assert_eq!(stored.limits.concurrency_cap, None);
    assert!(stored.execution_authority.is_none());
    let mut conflict = request;
    conflict.limits.concurrency_cap = Some(8);
    assert!(matches!(
        admit(&*db, &conflict, &definition).await,
        Err(RepresentationError::InvalidAdmission)
    ));
}

#[tokio::test]
async fn family_selection_separates_installation_from_execution_readiness() {
    let db = database().await;
    assert_eq!(
        admission_family(&*db).await.unwrap(),
        AdmissionFamily::Baseline42Installable
    );
    assert!(!supports_admission(&*db).await.unwrap());
    install(&*db).await.unwrap();
    assert_eq!(
        admission_family(&*db).await.unwrap(),
        AdmissionFamily::Installed
    );
    assert_eq!(
        dispatch_availability(),
        DispatchAvailability::UnsupportedLifecycle
    );
    db.update(CONTRACT)
        .value("version", 999_i64)
        .where_eq("representation", 2_i64)
        .execute(&*db)
        .await
        .unwrap();
    assert!(matches!(
        admission_family(&*db).await,
        Err(RepresentationError::MaintenanceRequired)
    ));
    let historical = database().await;
    // Typed contract fixture only, not conversion of a real workflow database.
    historical
        .update("workflow_store_contract")
        .value("schema_version", 43_i64)
        .where_eq("contract_id", 1_i64)
        .execute(&*historical)
        .await
        .unwrap();
    historical
        .update("workflow_storage_compatibility")
        .value("compatibility", 3_i64)
        .where_eq("contract_id", 1_i64)
        .execute(&*historical)
        .await
        .unwrap();
    assert_eq!(
        admission_family(&*historical).await.unwrap(),
        AdmissionFamily::Historical43
    );
}
