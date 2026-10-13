use super::*;
mod snapshot;
use crate::representation::{
    self as rep,
    controls::{GateDecision, GateRequest, resolve_gate},
};
async fn fixture() -> (Box<dyn Database>, crate::WorkflowExecutionAuthority) {
    let db = rep::tests::database().await;
    rep::install(&*db).await.unwrap();
    let definition = bcode_workflow::WorkflowBuilder::new(
        "waits",
        bcode_workflow::Step::<u32, u32>::input("input")
            .then(bcode_workflow::Step::<u32, u32>::approval("approval")),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let mut run = rep::tests::run("waits", false);
    let id =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("waits", &definition).unwrap();
    run.definition_id = id.definition_id;
    run.definition_version = id.definition_version;
    rep::admit(&*db, &run, &definition).await.unwrap();
    let owner = crate::WorkflowExecutionAuthority {
        target_artifact_id: "a".into(),
        daemon_instance_id: "d".into(),
        generation: 1,
        fencing_token: "f".into(),
    };
    rep::attempts::claim_authority(&*db, "waits", &owner)
        .await
        .unwrap();
    (db, owner)
}
#[tokio::test]
async fn waits_are_exact_stable_and_resolution_feedback_is_idempotent() {
    let (db, owner) = fixture().await;
    let initial = page(&*db, "waits", 10).await.unwrap();
    assert_eq!(initial.gates.len(), 1);
    assert_eq!(initial, page(&*db, "waits", 10).await.unwrap());
    let gate = &initial.gates[0];
    assert_eq!(gate.input, serde_json::json!(1));
    assert!(gate.requested_at_ms > 0);
    let mut request = GateRequest {
        version: 1,
        run_id: "waits".into(),
        node_id: gate.node_id.clone(),
        activation_id: gate.activation_id.clone(),
        expected_revision: initial.revision,
        decision: GateDecision::Input(serde_json::json!(9)),
    };
    let first = resolve_gate(&*db, &owner, &request).await.unwrap();
    assert!(!first.duplicate);
    assert_eq!(first.activated.len(), 1);
    let duplicate = resolve_gate(&*db, &owner, &request).await.unwrap();
    assert!(duplicate.duplicate);
    assert!(duplicate.activated.is_empty());
    request.decision = GateDecision::Input(serde_json::json!(10));
    assert!(resolve_gate(&*db, &owner, &request).await.is_err());
    let current = page(&*db, "waits", 10).await.unwrap();
    assert_eq!(current.gates.len(), 1);
    assert_eq!(current.gates[0].kind, GateKind::Approval);
    assert_eq!(current.gates[0].input, serde_json::json!(9));
    assert!(current.gates[0].response_schema.is_none());
    request.node_id = current.gates[0].node_id.clone();
    request.activation_id = current.gates[0].activation_id.clone();
    request.decision = GateDecision::Approve;
    resolve_gate(&*db, &owner, &request).await.unwrap();
    assert!(page(&*db, "waits", 10).await.unwrap().gates.is_empty());
}
#[tokio::test]
async fn cancellation_and_unknown_contract_do_not_expose_actions() {
    let (db, owner) = fixture().await;
    assert!(page(&*db, "waits", 0).await.is_err());
    assert!(
        page_after(&*db, "waits", None, Some("input"), 1)
            .await
            .is_err()
    );
    rep::safety::request_cancellation(&*db, "waits", &owner, 5)
        .await
        .unwrap();
    assert!(page(&*db, "waits", 10).await.unwrap().gates.is_empty());
    db.update("workflow_representation_contract")
        .value("version", i64::MAX)
        .where_eq("representation", 2_i64)
        .execute(&*db)
        .await
        .unwrap();
    assert!(matches!(
        page(&*db, "waits", 10).await,
        Err(Error::MaintenanceRequired)
    ));
}
