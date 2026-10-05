use super::super::{
    attempts, graph, safety,
    tests::{database, run},
};
use super::*;
async fn admitted() -> (Box<dyn Database>, WorkflowExecutionAuthority, String) {
    let db = database().await;
    super::super::install(&*db).await.unwrap();
    let definition = bcode_workflow::WorkflowBuilder::new(
        "run",
        bcode_workflow::Step::task("work", |v: u32, _| async move { Ok(v) }),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let identity =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("run", &definition).unwrap();
    let mut request = run("run", false);
    request.parent_session_id = Some("session".into());
    request.definition_id = identity.definition_id;
    request.definition_version = identity.definition_version;
    super::super::admit(&*db, &request, &definition)
        .await
        .unwrap();
    let owner = WorkflowExecutionAuthority {
        target_artifact_id: "artifact".into(),
        daemon_instance_id: "owner".into(),
        generation: 1,
        fencing_token: "token".into(),
    };
    (db, owner, definition.entries[0].clone())
}
#[tokio::test]
async fn detachment_changes_only_association_and_needs_no_owner() {
    let (db, _, _) = admitted().await;
    let before = db.select(RUNS).execute(&*db).await.unwrap()[0].clone();
    assert!(detach(&*db, "run", "wrong").await.is_err());
    assert!(detach(&*db, "run", "session").await.unwrap());
    assert!(!detach(&*db, "run", "session").await.unwrap());
    let after = db.select(RUNS).execute(&*db).await.unwrap()[0].clone();
    for field in [
        "admission_json",
        "definition_json",
        "authority_json",
        "status",
        "consumed",
        "active_attempts",
    ] {
        assert_eq!(before.get(field), after.get(field));
    }
    assert!(matches!(
        lookup(&*db, "run").await.unwrap(),
        RunLookup::Available(RunObservation {
            parent_session_id: None,
            ..
        })
    ));
    assert_eq!(catalog(&*db, None, 1).await.unwrap().entries.len(), 1);
    assert!(
        catalog(&*db, Some("run"), 1)
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    db.update(super::super::CONTRACT)
        .value("version", 999_i64)
        .where_eq("representation", 2_i64)
        .execute(&*db)
        .await
        .unwrap();
    assert_eq!(
        lookup(&*db, "run").await.unwrap(),
        RunLookup::UnsupportedRepresentation
    );
    assert!(
        !catalog(&*db, None, 1)
            .await
            .unwrap()
            .representation_supported
    );
    assert!(!detach(&*db, "run", "session").await.unwrap());
    db.update(super::super::CONTRACT)
        .value("version", 999_i64)
        .where_eq("representation", 0_i64)
        .execute(&*db)
        .await
        .unwrap();
    assert!(detach(&*db, "run", "session").await.is_err());
}
async fn prepared(db: &dyn Database, owner: &WorkflowExecutionAuthority, node: &str) -> String {
    attempts::claim_authority(db, "run", owner).await.unwrap();
    let request = crate::PreparedAttempt {
        run_id: "run".into(),
        node_id: node.into(),
        activation_id: graph::activation_id(db, "run", node)
            .await
            .unwrap()
            .unwrap(),
        attempt: 1,
        side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({}),
        prepared_at_ms: 1,
    };
    attempts::prepare(db, owner, &request).await.unwrap()
}
async fn settled(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    node: &str,
    id: &str,
    outcome: attempts::TerminalOutcome,
) {
    attempts::attach_receipt(
        db,
        owner,
        &crate::DispatchReceipt {
            run_id: "run".into(),
            node_id: node.into(),
            activation_id: graph::activation_id(db, "run", node)
                .await
                .unwrap()
                .unwrap(),
            attempt: 1,
            dispatch_identity: id.into(),
            receipt: serde_json::json!({}),
            admitted_at_ms: 2,
        },
    )
    .await
    .unwrap();
    attempts::settle(db, owner, "run", id, outcome, 3)
        .await
        .unwrap();
}
#[tokio::test]
async fn observing_active_attempts_preserves_controller_scan_version() {
    let (db, owner, node) = admitted().await;
    let id = prepared(&*db, &owner, &node).await;
    let before = graph::parallel_progress::scan_version(&*db, "run")
        .await
        .unwrap();
    for _ in 0..64 {
        assert_eq!(terminalize(&*db, &owner, "run").await.unwrap(), None);
        assert_eq!(
            graph::parallel_progress::scan_version(&*db, "run")
                .await
                .unwrap(),
            before
        );
    }
    settled(
        &*db,
        &owner,
        &node,
        &id,
        attempts::TerminalOutcome::Succeeded,
    )
    .await;
    let after = graph::parallel_progress::scan_version(&*db, "run")
        .await
        .unwrap();
    assert_eq!(after.revision, before.revision);
    assert!(after.epoch > before.epoch);
}

#[tokio::test]
async fn cancellation_waits_for_receipts_and_terminal_outcomes_are_stable() {
    let (db, owner, node) = admitted().await;
    let id = prepared(&*db, &owner, &node).await;
    safety::request_cancellation(&*db, "run", &owner, 2)
        .await
        .unwrap();
    assert_eq!(terminalize(&*db, &owner, "run").await.unwrap(), None);
    settled(
        &*db,
        &owner,
        &node,
        &id,
        attempts::TerminalOutcome::Succeeded,
    )
    .await;
    assert_eq!(
        terminalize(&*db, &owner, "run").await.unwrap(),
        Some(RunState::Cancelled)
    );
    assert!(
        graph::settle_output(&*db, &owner, "run", &node, 1, &serde_json::json!(1))
            .await
            .is_err()
    );
    assert_eq!(
        terminalize(&*db, &owner, "run").await.unwrap(),
        Some(RunState::Cancelled)
    );
}
#[tokio::test]
async fn success_requires_graph_output_and_failure_does_not() {
    for failed in [false, true] {
        let (db, owner, node) = admitted().await;
        let id = prepared(&*db, &owner, &node).await;
        settled(
            &*db,
            &owner,
            &node,
            &id,
            if failed {
                attempts::TerminalOutcome::Failed
            } else {
                attempts::TerminalOutcome::Succeeded
            },
        )
        .await;
        if !failed {
            assert_eq!(terminalize(&*db, &owner, "run").await.unwrap(), None);
            graph::settle_output(&*db, &owner, "run", &node, 1, &serde_json::json!(1))
                .await
                .unwrap();
        }
        assert_eq!(
            terminalize(&*db, &owner, "run").await.unwrap(),
            Some(if failed {
                RunState::Failed
            } else {
                RunState::Completed
            })
        );
        let stale = WorkflowExecutionAuthority {
            generation: 2,
            ..owner
        };
        assert!(matches!(
            terminalize(&*db, &stale, "run").await,
            Err(Error::AuthorityConflict)
        ));
    }
}
