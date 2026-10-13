//! Behavioral control producer integration tests.
use super::{Fact, control, page};

#[tokio::test]
async fn gates_produce_real_timed_commitments_and_guard_unacknowledged_sources() {
    let definition = bcode_workflow::WorkflowBuilder::new(
        "gates",
        bcode_workflow::Step::<u32, u32>::input("input")
            .then(bcode_workflow::Step::<u32, u32>::approval("approve")),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let (_temp, db) =
        super::super::diagnostics::tests::admitted_definition(true, &definition).await;
    let owner = crate::WorkflowExecutionAuthority {
        target_artifact_id: "artifact".into(),
        daemon_instance_id: "daemon".into(),
        generation: 1,
        fencing_token: "secret".into(),
    };
    crate::representation::attempts::claim_authority(&*db, "run", &owner)
        .await
        .unwrap();
    let id = crate::representation::graph::activation_id(&*db, "run", "input")
        .await
        .unwrap()
        .unwrap();
    crate::representation::graph::gates::resolve_observed(
        &*db,
        &owner,
        "run",
        "input",
        &id,
        1,
        Some(&serde_json::json!(42)),
    )
    .await
    .unwrap();
    let approval = crate::representation::graph::activation_id(&*db, "run", "approve")
        .await
        .unwrap()
        .unwrap();
    crate::representation::graph::gates::deny_observed(
        &*db, &owner, "run", "approve", &approval, 1,
    )
    .await
    .unwrap();
    let first = page(&*db, 1, "run", 0, None, 100).await.unwrap();
    assert_eq!(
        first.control_coverage,
        bcode_workflow::WorkflowSemanticHistoryCoverage::CompleteFromAdmission
    );
    let facts = first
        .events
        .iter()
        .filter_map(|event| serde_json::from_value::<Fact>(event.payload["fact"].clone()).ok())
        .collect::<Vec<_>>();
    for expected in [
        control::Operation::GateResolved,
        control::Operation::OutputCommitted,
        control::Operation::GateDenied,
    ] {
        assert!(
            facts
                .iter()
                .any(|fact| matches!(fact,Fact::Control{operation,..}if *operation==expected))
        );
    }
    assert!(first.events.iter().all(|e| e.created_at_ms > 0));
    crate::representation::graph::gates::deny_observed(
        &*db, &owner, "run", "approve", &approval, 1,
    )
    .await
    .unwrap();
    assert_eq!(
        page(&*db, 1, "run", 0, None, 100)
            .await
            .unwrap()
            .through_sequence,
        first.through_sequence
    );
    assert_unacknowledged_sources_reject(&*db).await;
}

async fn assert_unacknowledged_sources_reject(db: &dyn switchy_database::Database) {
    assert!(
        db.insert("workflow_outputs_v2")
            .value("output_id", "unacknowledged")
            .value("run_id", "run")
            .value("activation_id", "unknown")
            .value("output_json", "private")
            .execute(db)
            .await
            .is_err()
    );
    assert!(
        db.insert("workflow_repeat_cycles_v2")
            .value("identity", "unacknowledged")
            .value("run_id", "run")
            .value("value", 1_i64)
            .execute(db)
            .await
            .is_err()
    );
    assert!(
        db.insert("workflow_control_failures_v2")
            .value("identity", "unacknowledged")
            .value("run_id", "run")
            .value("value", 1_i64)
            .execute(db)
            .await
            .is_err()
    );
}
