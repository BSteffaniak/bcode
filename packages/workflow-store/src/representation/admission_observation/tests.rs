use super::super::tests::{database, run};
use super::*;

async fn fixture() -> (Box<dyn Database>, WorkflowExecutionAuthority, Observation) {
    let db = database().await;
    super::super::install(&*db).await.unwrap();
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await
        .unwrap();
    install_in(&*tx).await.unwrap();
    tx.commit().await.unwrap();
    let definition = bcode_workflow::WorkflowBuilder::new(
        "diagnostic",
        bcode_workflow::Step::task("source", |value: u32, _| async move { Ok(value) }),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let mut request = run("diagnostic", false);
    let identity =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("diagnostic", &definition)
            .unwrap();
    request.definition_id = identity.definition_id;
    request.definition_version = identity.definition_version;
    super::super::admit(&*db, &request, &definition)
        .await
        .unwrap();
    let owner = WorkflowExecutionAuthority {
        target_artifact_id: "artifact".into(),
        daemon_instance_id: "daemon".into(),
        generation: 1,
        fencing_token: "token".into(),
    };
    super::super::attempts::claim_authority(&*db, &request.run_id, &owner)
        .await
        .unwrap();
    let activation = super::super::graph::activation_id(&*db, &request.run_id, "source")
        .await
        .unwrap()
        .unwrap();
    let prepared = PreparedAttempt {
        run_id: request.run_id.clone(),
        node_id: "source".into(),
        activation_id: activation.clone(),
        attempt: 1,
        side_effect: bcode_workflow::DispatchSideEffect::Mutating,
        intent: serde_json::json!({}),
        prepared_at_ms: 1,
    };
    let dispatch = super::super::attempts::prepare(&*db, &owner, &prepared)
        .await
        .unwrap();
    let value = Observation {
        version: VERSION,
        run_id: request.run_id,
        node_id: "source".into(),
        activation_id: activation,
        attempt: 1,
        dispatch_identity: dispatch,
        phase: Phase::OwnerDispatch,
        category: Category::OwnerError,
        observed_at_ms: 2,
    };
    (db, owner, value)
}

#[tokio::test]
async fn duplicate_safe_conflict_and_foreign_authority_preserve_preparation() {
    let (db, owner, mut value) = fixture().await;
    record(&*db, &owner, &value).await.unwrap();
    value.observed_at_ms = 3;
    record(&*db, &owner, &value).await.unwrap();
    let rows = inspect(&*db, &value.run_id, None, 10).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].observed_at_ms, 2);
    value.category = Category::OwnerAccessDeferred;
    assert!(matches!(
        record(&*db, &owner, &value).await,
        Err(Error::InvalidAdmission)
    ));
    let mut foreign = owner;
    foreign.generation += 1;
    assert!(matches!(
        record(&*db, &foreign, &value).await,
        Err(Error::AuthorityConflict)
    ));
    assert!(
        super::super::attempts::owner_boundary::prepared(
            &*db,
            &value.run_id,
            &value.dispatch_identity
        )
        .await
        .unwrap()
        .is_some()
    );
    assert!(
        super::super::attempts::owner_boundary::receipt(&*db, &value.dispatch_identity)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        super::super::attempts::owner_boundary::terminal_outcome(
            &*db,
            &value.run_id,
            &value.dispatch_identity
        )
        .await
        .unwrap()
        .is_none()
    );
}

#[tokio::test]
async fn future_contract_is_preserved_and_rejected_without_repair() {
    let (db, owner, value) = fixture().await;
    db.update(CONTRACT)
        .value("version", 2_i64)
        .where_eq("contract_id", 1_i64)
        .execute(&*db)
        .await
        .unwrap();
    assert!(matches!(
        record(&*db, &owner, &value).await,
        Err(Error::MaintenanceRequired)
    ));
    assert!(matches!(
        inspect(&*db, &value.run_id, None, 10).await,
        Err(Error::MaintenanceRequired)
    ));
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await
        .unwrap();
    assert!(matches!(
        install_in(&*tx).await,
        Err(Error::MaintenanceRequired)
    ));
}
