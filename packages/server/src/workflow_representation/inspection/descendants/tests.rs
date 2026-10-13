use super::*;
use crate::workflow_representation::{open, tests::typed_provider_fixture};

async fn family() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    std::sync::Arc<crate::ServerState>,
) {
    let (root, sessions, state, definition) = typed_provider_fixture().await;
    let db = open(&crate::workflow_store_path(&state), true).unwrap();
    let authority = representation::attempts::owner_boundary::execution_authority(&db, "new")
        .await
        .unwrap()
        .unwrap();
    let context = representation::hierarchy::parent_context(&db, &authority, "new")
        .await
        .unwrap();
    let identity =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("child", &definition).unwrap();
    let target = bcode_workflow::WorkflowCallTarget::Definition {
        identity: identity.clone(),
    };
    let mut parent_definition = bcode_workflow::WorkflowBuilder::new(
        "parent",
        bcode_workflow::parallel_named(
            "join",
            bcode_workflow::Step::task("agent", |v: u32, _| async move { Ok(v) }),
            bcode_workflow::Step::task("sibling", |v: u32, _| async move { Ok(v) }),
        ),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    for id in ["agent", "sibling"] {
        let node = parent_definition.nodes.get_mut(id).unwrap();
        node.kind = bcode_workflow::NodeKind::WorkflowCall;
        node.configuration = serde_json::to_value(bcode_workflow::WorkflowCallConfiguration {
            version: bcode_workflow::WORKFLOW_CALL_VERSION,
            target: target.clone(),
            input: None,
            output: None,
        })
        .unwrap();
    }
    let parent_identity =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("parent", &parent_definition)
            .unwrap();
    let mut parent = context.admission;
    parent.run_id = "parent".into();
    parent.definition_id = parent_identity.definition_id;
    parent.definition_version = parent_identity.definition_version;
    parent.limits.node_execution_cap = 8;
    representation::admit(&db, &parent, &parent_definition)
        .await
        .unwrap();
    for node_id in ["agent", "sibling"] {
        let activation = representation::graph::activation_id(&db, "parent", node_id)
            .await
            .unwrap()
            .unwrap();
        let child_id = bcode_workflow_store::workflow_child_run_id(
            "parent",
            "parent",
            &activation,
            1,
            &target,
        );
        representation::attempts::prepare(
            &db,
            &authority,
            &bcode_workflow_store::PreparedAttempt {
                run_id: "parent".into(),
                node_id: node_id.into(),
                activation_id: activation.clone(),
                attempt: 1,
                side_effect: bcode_workflow::DispatchSideEffect::Mutating,
                intent: serde_json::json!({}),
                prepared_at_ms: 1,
            },
        )
        .await
        .unwrap();
        let mut child = parent.clone();
        child.run_id = child_id.clone();
        child.definition_id = identity.definition_id.clone();
        child.definition_version = identity.definition_version;
        child.binding = None;
        let link = bcode_workflow::WorkflowRunLink {
            version: bcode_workflow_store::WORKFLOW_RUN_LINK_VERSION,
            root_run_id: "parent".into(),
            parent_run_id: "parent".into(),
            parent_node_id: node_id.into(),
            parent_activation_id: activation,
            parent_attempt: 1,
            child_run_id: child_id,
            target: target.clone(),
            depth: 2,
            created_at_ms: parent.created_at_ms,
        };
        representation::hierarchy::admit_child(&db, &authority, &link, &child, &definition)
            .await
            .unwrap();
    }
    (root, sessions, state)
}

#[tokio::test]
async fn descendant_rows_are_real_and_limit_does_not_certify_exhaustion() {
    use switchy_database::Database;
    let (_root, _sessions, state) = family().await;
    let db = open(&crate::workflow_store_path(&state), false).unwrap();
    let tx = db.begin_transaction().await.unwrap();
    let children = representation::hierarchy::children_in(&*tx, "parent", None, 100)
        .await
        .unwrap();
    let (rows, complete) = inspect(&*tx, "parent", &children.children, true, 100)
        .await
        .unwrap();
    assert!(complete);
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows.iter()
            .map(|row| row.run.run_id.as_str())
            .collect::<Vec<_>>(),
        children
            .children
            .iter()
            .map(|link| link.child_run_id.as_str())
            .collect::<Vec<_>>()
    );
    assert!(
        rows.iter().all(
            |row| row.link.parent_run_id == "parent" && row.link.child_run_id == row.run.run_id
        )
    );
    let (rows, complete) = inspect(&*tx, "parent", &children.children, true, 1)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(!complete);
    let children = representation::hierarchy::children_in(&*tx, "parent", None, 1)
        .await
        .unwrap();
    assert!(children.next_child_run_id.is_some());
    let (_, complete) = inspect(&*tx, "parent", &children.children, false, 100)
        .await
        .unwrap();
    assert!(!complete);
    tx.commit().await.unwrap();
}
