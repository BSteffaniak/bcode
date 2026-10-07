use super::{Category, owner_category};
use bcode_workflow_store::WorkflowStoreError;

#[test]
fn classification_never_retains_private_owner_messages() {
    assert_eq!(
        owner_category(&WorkflowStoreError::OwnerAccessDeferred),
        Category::OwnerAccessDeferred
    );
    let category = owner_category(&WorkflowStoreError::InvalidData(
        "secret-token private-path provider-wire".into(),
    ));
    assert_eq!(category, Category::OwnerError);
    assert_eq!(
        serde_json::to_value(category).unwrap(),
        serde_json::json!("owner_error")
    );
}

mod fixture;

#[tokio::test]
async fn dispatch_failure_is_durable_without_terminalizing_or_retrying() {
    let (root, state, db, owner, request) = Box::pin(fixture::prepare()).await;
    let before = bcode_workflow_store::representation::attempts::owner_boundary::prepared(
        &db,
        &request.activation.run_id,
        &request.dispatch_identity,
    )
    .await
    .unwrap()
    .expect("prepared before failure");
    assert!(
        super::super::dispatch_and_record(&state, &owner, &request, &fixture::RejectingOwner)
            .await
            .is_err()
    );
    let rows = bcode_workflow_store::representation::admission_observation::inspect(
        &db,
        &request.activation.run_id,
        None,
        10,
    )
    .await
    .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].category, Category::OwnerError);
    assert_eq!(rows[0].dispatch_identity, request.dispatch_identity);
    let boundary = bcode_workflow_store::representation::attempts::owner_boundary::receipt(
        &db,
        &request.dispatch_identity,
    )
    .await
    .unwrap();
    assert!(boundary.is_none());
    assert!(
        bcode_workflow_store::representation::attempts::owner_boundary::terminal_outcome(
            &db,
            &request.activation.run_id,
            &request.dispatch_identity
        )
        .await
        .unwrap()
        .is_none()
    );
    let after = bcode_workflow_store::representation::attempts::owner_boundary::prepared(
        &db,
        &request.activation.run_id,
        &request.dispatch_identity,
    )
    .await
    .unwrap()
    .expect("preparation retained");
    assert_eq!(before, after);
    drop(db);
    drop(state);
    drop(root);
}

#[tokio::test]
async fn stale_authority_cannot_record_failure_or_change_preparation() {
    let (root, state, db, mut owner, request) = Box::pin(fixture::prepare()).await;
    let before = bcode_workflow_store::representation::attempts::owner_boundary::prepared(
        &db,
        &request.activation.run_id,
        &request.dispatch_identity,
    )
    .await
    .unwrap();
    owner.generation += 1;
    assert!(
        super::record(
            &state,
            &owner,
            &request,
            bcode_workflow_store::representation::admission_observation::Phase::OwnerDispatch,
            Category::OwnerError,
        )
        .await
        .is_err()
    );
    let observations = bcode_workflow_store::representation::admission_observation::inspect(
        &db,
        &request.activation.run_id,
        None,
        10,
    )
    .await
    .unwrap();
    assert!(observations.is_empty());
    let after = bcode_workflow_store::representation::attempts::owner_boundary::prepared(
        &db,
        &request.activation.run_id,
        &request.dispatch_identity,
    )
    .await
    .unwrap();
    assert_eq!(before, after);
    drop(db);
    drop(state);
    drop(root);
}
