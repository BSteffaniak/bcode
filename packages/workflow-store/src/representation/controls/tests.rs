use super::*;
#[tokio::test]
async fn user_pause_is_durable_fenced_and_does_not_reset_cancellation() {
    let db = super::super::tests::database().await;
    super::super::install(&*db).await.unwrap();
    let definition = bcode_workflow::WorkflowBuilder::new(
        "control",
        bcode_workflow::Step::task("seed", |v: u32, _| async move { Ok(v) }),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let mut run = super::super::tests::run("control", false);
    let identity =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("control", &definition).unwrap();
    run.definition_id = identity.definition_id;
    run.definition_version = identity.definition_version;
    super::super::admit(&*db, &run, &definition).await.unwrap();
    let owner = WorkflowExecutionAuthority {
        target_artifact_id: "a".into(),
        daemon_instance_id: "d".into(),
        generation: 1,
        fencing_token: "f".into(),
    };
    super::super::attempts::claim_authority(&*db, "control", &owner)
        .await
        .unwrap();
    let mut request = PauseRequest {
        version: 1,
        run_id: "control".into(),
        expected_revision: 1,
        paused: true,
    };
    assert!(!user_paused(&*db, "control").await.unwrap());
    set_pause(&*db, &owner, &request).await.unwrap();
    set_pause(&*db, &owner, &request).await.unwrap();
    assert!(user_paused(&*db, "control").await.unwrap());
    let stale = WorkflowExecutionAuthority {
        generation: 2,
        ..owner.clone()
    };
    assert!(matches!(
        set_pause(&*db, &stale, &request).await,
        Err(Error::AuthorityConflict)
    ));
    request.expected_revision = 2;
    assert!(set_pause(&*db, &owner, &request).await.is_err());
    request.expected_revision = 1;
    request.version = 2;
    assert!(set_pause(&*db, &owner, &request).await.is_err());
    super::super::safety::request_cancellation(&*db, "control", &owner, 7)
        .await
        .unwrap();
    request.version = 1;
    request.paused = false;
    set_pause(&*db, &owner, &request).await.unwrap();
    assert!(!user_paused(&*db, "control").await.unwrap());
    assert_eq!(
        super::super::safety::cancellation_requested_at(&*db, "control")
            .await
            .unwrap(),
        Some(7)
    );
    let invalid = GateRequest {
        version: 2,
        run_id: "control".into(),
        node_id: "seed".into(),
        activation_id: "unknown".into(),
        expected_revision: 1,
        decision: GateDecision::Approve,
    };
    assert!(resolve_gate(&*db, &owner, &invalid).await.is_err());
}
