use super::*;
#[tokio::test]
async fn snapshot_pages_exact_activations_and_capacity() {
    let db = super::super::tests::database().await;
    super::super::install(&*db).await.unwrap();
    let mut definition = bcode_workflow::WorkflowBuilder::new(
        "snapshot",
        bcode_workflow::Step::task("a", |v: u32, _| async move { Ok(v) }),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let mut other = definition.nodes["a"].clone();
    other.id = "b".into();
    definition.nodes.insert("b".into(), other);
    definition.entries.push("b".into());
    let mut run = super::super::tests::run("snapshot", false);
    run.limits.concurrency_cap = Some(3);
    let id = bcode_workflow::WorkflowDefinitionIdentity::for_definition("snapshot", &definition)
        .unwrap();
    run.definition_id = id.definition_id;
    run.definition_version = id.definition_version;
    super::super::admit(&*db, &run, &definition).await.unwrap();
    let first = snapshot(&*db, "snapshot", None, None, 1).await.unwrap();
    assert_eq!(first.concurrency_cap, Some(3));
    assert_eq!(first.occupied, 0);
    assert_eq!(first.activations.len(), 1);
    assert!(first.activations[0].latest_attempt.is_none());
    let cursor = first.next_activation_id.as_deref().unwrap();
    let second = snapshot(&*db, "snapshot", Some(first.revision), Some(cursor), 1)
        .await
        .unwrap();
    assert!(second.next_activation_id.is_none());
    assert_ne!(
        first.activations[0].activation_id,
        second.activations[0].activation_id
    );
    assert!(
        snapshot(&*db, "snapshot", None, Some(cursor), 1)
            .await
            .is_err()
    );
    assert!(snapshot(&*db, "snapshot", Some(2), None, 1).await.is_err());
    assert!(snapshot(&*db, "snapshot", None, None, 0).await.is_err());
    let owner = crate::WorkflowExecutionAuthority {
        target_artifact_id: "a".into(),
        daemon_instance_id: "d".into(),
        generation: 1,
        fencing_token: "f".into(),
    };
    super::super::attempts::claim_authority(&*db, "snapshot", &owner)
        .await
        .unwrap();
    let activation = &first.activations[0];
    let attempt = crate::PreparedAttempt {
        run_id: "snapshot".into(),
        node_id: activation.node_id.clone(),
        activation_id: activation.activation_id.clone(),
        attempt: 1,
        side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({}),
        prepared_at_ms: 1,
    };
    let dispatch = super::super::attempts::prepare(&*db, &owner, &attempt)
        .await
        .unwrap();
    let prepared = snapshot(&*db, "snapshot", None, None, 1).await.unwrap();
    assert_eq!(prepared.occupied, 1);
    assert_eq!(
        prepared.activations[0].latest_attempt.as_ref().unwrap(),
        &AttemptObservation {
            attempt: 1,
            dispatch_identity: dispatch,
            receipt_present: false,
            terminal: None
        }
    );
}
