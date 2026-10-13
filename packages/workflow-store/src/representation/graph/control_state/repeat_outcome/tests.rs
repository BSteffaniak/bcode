use super::*;
use crate::representation as rep;

async fn fixture() -> (Box<dyn Database>, crate::WorkflowExecutionAuthority) {
    let db = rep::tests::database().await;
    rep::install(&*db).await.unwrap();
    let definition = bcode_workflow::WorkflowBuilder::new(
        "outcomes",
        bcode_workflow::Step::task("work", |v: serde_json::Value, _| async move { Ok(v) })
            .repeat_while(
                "repeat",
                bcode_workflow::field::<serde_json::Value>("done").eq(false),
                5,
            ),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let identity =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("outcomes", &definition)
            .unwrap();
    let mut run = rep::tests::run("outcomes", false);
    run.definition_id = identity.definition_id;
    run.definition_version = identity.definition_version;
    run.input = Some(serde_json::json!({"done": false}));
    rep::admit(&*db, &run, &definition).await.unwrap();
    let owner = crate::WorkflowExecutionAuthority {
        target_artifact_id: "a".into(),
        daemon_instance_id: "d".into(),
        generation: 1,
        fencing_token: "f".into(),
    };
    rep::attempts::claim_authority(&*db, "outcomes", &owner)
        .await
        .unwrap();
    (db, owner)
}

#[tokio::test]
async fn actual_repeat_records_are_fenced_idempotent_bounded_and_atomic() {
    let (db, owner) = fixture().await;
    let outcome = bcode_workflow::WorkflowRepeatOutcome {
        version: bcode_workflow::WORKFLOW_REPEAT_OUTCOME_VERSION,
        outcome: bcode_workflow::WorkflowRepeatOutcomeKind::ConditionCleared,
        iterations_completed: 1,
        max_iterations: 5,
        cycle_cap: 5,
        effective_iteration_bound: 5,
        value: serde_json::json!(1),
    };
    let tx = db
        .begin_transaction_with_mode(switchy_database::TransactionMode::Immediate)
        .await
        .unwrap();
    for id in ["a", "b"] {
        let output = crate::ValidatedOutput {
            output_id: format!("{id}:control-output"),
            run_id: "outcomes".into(),
            node_id: "repeat".into(),
            activation_id: id.into(),
            schema_id: "outcome".into(),
            schema_version: 1,
            value: serde_json::to_value(&outcome).unwrap(),
            artifact_reference: None,
            created_at_ms: 17,
        };
        tx.insert("workflow_outputs_v2")
            .value("output_id", output.output_id.as_str())
            .value("run_id", "outcomes")
            .value("activation_id", id)
            .value("output_json", serde_json::to_string(&output).unwrap())
            .execute(&*tx)
            .await
            .unwrap();
        record_control_in(&*tx, &owner, "outcomes", id, &outcome)
            .await
            .unwrap();
        record_control_in(&*tx, &owner, "outcomes", id, &outcome)
            .await
            .unwrap();
        let stale = crate::WorkflowExecutionAuthority {
            generation: 2,
            ..owner.clone()
        };
        assert!(
            record_control_in(&*tx, &stale, "outcomes", id, &outcome)
                .await
                .is_err()
        );
    }
    let (rows, next) = page_in(&*tx, "outcomes", None, 1).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].created_at_ms, 17);
    assert_eq!(rows[0].outcome, outcome.outcome);
    let (tail, next_tail) = page_in(&*tx, "outcomes", next.as_deref(), 1).await.unwrap();
    assert_eq!(tail.len(), 1);
    assert!(next_tail.is_none());
    tx.update(TABLE)
        .value("version", 99_i64)
        .where_eq("output_id", rows[0].output_id.as_str())
        .execute(&*tx)
        .await
        .unwrap();
    assert!(page_in(&*tx, "outcomes", None, 2).await.is_err());
    tx.rollback().await.unwrap();
    let tx = db.begin_transaction().await.unwrap();
    assert!(
        page_in(&*tx, "outcomes", None, 2)
            .await
            .unwrap()
            .0
            .is_empty()
    );
    tx.commit().await.unwrap();
}
