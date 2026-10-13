use super::*;

#[tokio::test]
async fn definition_child_uses_common_owner_and_returns_provider_output() {
    let (_root, _sessions, state, definition) = typed_provider_fixture().await;
    let mut cleanup = FixtureCleanup::new(&state);
    let db = open(&crate::workflow_store_path(&state), true).expect("db");
    let guard = owner::local_authority(&state, "new")
        .await
        .expect("authority")
        .expect("owner");
    let context = representation::hierarchy::parent_context(&db, &guard.authority, "new")
        .await
        .expect("context");
    let identity =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("child-agent", &definition)
            .expect("identity");
    state
        .workflow_store
        .lock()
        .expect("store")
        .persist_definition(
            &identity.definition_id,
            identity.definition_version,
            &definition,
        )
        .expect("register child");
    let mut parent_definition = definition.clone();
    let node = parent_definition.nodes.get_mut("agent").expect("node");
    node.kind = bcode_workflow::NodeKind::WorkflowCall;
    node.configuration = serde_json::to_value(bcode_workflow::WorkflowCallConfiguration {
        version: bcode_workflow::WORKFLOW_CALL_VERSION,
        target: bcode_workflow::WorkflowCallTarget::Definition { identity },
        input: None,
        output: None,
    })
    .expect("config");
    let parent_identity = bcode_workflow::WorkflowDefinitionIdentity::for_definition(
        "child-parent",
        &parent_definition,
    )
    .expect("parent identity");
    let mut parent = context.admission;
    parent.run_id = "child-parent".into();
    parent.definition_id = parent_identity.definition_id;
    parent.definition_version = parent_identity.definition_version;
    parent.limits.node_execution_cap = 2;
    representation::admit(&db, &parent, &parent_definition)
        .await
        .expect("fixture admission; capability remains gated");
    Box::pin(tokio::time::timeout(
        std::time::Duration::from_secs(10),
        async {
            loop {
                driver::drive(&state, "child-parent")
                    .await
                    .expect("parent drive");
                let page = representation::hierarchy::children(&db, "child-parent", None, 32)
                    .await
                    .expect("children");
                for link in page.children {
                    driver::drive(&state, &link.child_run_id)
                        .await
                        .expect("child drive");
                }
                if status(&state, "child-parent")
                    .await
                    .expect("status")
                    .expect("parent")
                    .status
                    == bcode_workflow::RunStatus::Completed
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        },
    ))
    .await
    .expect("child completes without recursive parent lock");
    let children = representation::hierarchy::children(&db, "child-parent", None, 32)
        .await
        .expect("children");
    assert_eq!(children.children.len(), 1);
    assert_eq!(children.children[0].depth, 2);
    cleanup.finish().await;
    drop(cleanup);
}

#[tokio::test]
async fn normal_definition_child_completion_wakes_parent() {
    let (_root, _sessions, state, definition) = typed_provider_fixture().await;
    let mut cleanup = FixtureCleanup::new(&state);
    let identity =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("normal-child", &definition)
            .expect("identity");
    state
        .workflow_store
        .lock()
        .expect("store")
        .persist_definition(
            &identity.definition_id,
            identity.definition_version,
            &definition,
        )
        .expect("child definition");
    let mut parent = definition;
    let call = parent.nodes.get_mut("agent").expect("call");
    call.kind = bcode_workflow::NodeKind::WorkflowCall;
    call.configuration = serde_json::to_value(bcode_workflow::WorkflowCallConfiguration {
        version: bcode_workflow::WORKFLOW_CALL_VERSION,
        target: bcode_workflow::WorkflowCallTarget::Definition { identity },
        input: None,
        output: None,
    })
    .expect("call config");
    state.start_workflow_driver().await;
    super::fanout::start_public(&state, "normal-parent", parent, serde_json::json!(1), None).await;
    let result = super::fanout::wait_public(&state, "normal-parent").await;
    result.expect("child result reaches parent through ordinary scheduler");
    cleanup.finish().await;
    drop(cleanup);
}

#[tokio::test]
async fn public_allowance_grant_is_atomic_and_exact_retry_is_unchanged() {
    let (_root, _sessions, state, _) = typed_provider_fixture().await;
    let mut cleanup = FixtureCleanup::new(&state);
    let db = open(&crate::workflow_store_path(&state), false).expect("db");
    let guard = owner::local_authority(&state, "new")
        .await
        .expect("authority")
        .expect("owner");
    let before = representation::hierarchy::parent_context(&db, &guard.authority, "new")
        .await
        .expect("context");
    let expected = before.effective_limits.node_execution_cap;
    let action = bcode_workflow::WorkflowRunControlAction::IncreaseExecutionAllowance {
        expected_cap: expected,
        target_cap: expected + 1,
    };
    let first = crate::workflow_operations::control_exact_run(&state, "new", action)
        .await
        .expect("grant");
    let duplicate = crate::workflow_operations::control_exact_run(&state, "new", action)
        .await
        .expect("duplicate");
    assert!(first.1);
    assert!(!duplicate.1);
    let after = representation::hierarchy::parent_context(&db, &guard.authority, "new")
        .await
        .expect("context");
    assert_eq!(after.effective_limits.node_execution_cap, expected + 1);
    assert_eq!(
        after.effective_limits.retry_cap,
        before.effective_limits.retry_cap
    );
    cleanup.finish().await;
    drop(cleanup);
}
