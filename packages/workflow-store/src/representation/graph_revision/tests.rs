use super::super::{
    attempts, graph,
    tests::{database, run},
};
use super::*;
async fn fixture() -> (
    Box<dyn Database>,
    WorkflowExecutionAuthority,
    WorkflowDefinition,
) {
    fixture_with_binding(None).await
}

async fn fixture_with_binding(
    binding: Option<&crate::WorkflowRunPackageBinding>,
) -> (
    Box<dyn Database>,
    WorkflowExecutionAuthority,
    WorkflowDefinition,
) {
    let db = database().await;
    super::super::install(&*db).await.unwrap();
    let definition = bcode_workflow::WorkflowBuilder::new(
        "edit",
        bcode_workflow::Step::task("first", |n: u32, _| async move { Ok(n) }).then(
            bcode_workflow::Step::task("last", |n: u32, _| async move { Ok(n) }),
        ),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let mut request = run("edit", false);
    let id =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("edit", &definition).unwrap();
    request.definition_id = id.definition_id;
    request.definition_version = id.definition_version;
    let tx = db.begin_transaction().await.unwrap();
    super::super::admit_package_in(&*tx, &request, &definition, binding)
        .await
        .unwrap();
    super::super::hierarchy::register_root(&*tx, &request)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let owner = WorkflowExecutionAuthority {
        target_artifact_id: "a".into(),
        daemon_instance_id: "d".into(),
        generation: 1,
        fencing_token: "f".into(),
    };
    attempts::claim_authority(&*db, "edit", &owner)
        .await
        .unwrap();
    (db, owner, definition)
}
#[tokio::test]
async fn bound_revision_retries_preserve_admission_and_unknown_execution_is_rejected() {
    let binding = crate::WorkflowRunPackageBinding {
        version: 1,
        package_id: "test/edit".into(),
        lock_digest: "a".repeat(64),
    };
    let (db, owner, definition) = fixture_with_binding(Some(&binding)).await;
    let batch = edit(&definition);
    assert_eq!(publish(&*db, &owner, &batch).await.unwrap(), 2);
    assert_eq!(publish(&*db, &owner, &batch).await.unwrap(), 2);
    assert_eq!(
        super::super::admission_projection::package_binding(&*db, "edit")
            .await
            .unwrap(),
        Some(binding.clone())
    );
    let rows = db
        .select(super::super::RUNS)
        .columns(&["admission_json"])
        .where_eq("run_id", "edit")
        .limit(1)
        .execute(&*db)
        .await
        .unwrap();
    let Some(DatabaseValue::String(json)) = rows[0].get("admission_json") else {
        panic!("admission");
    };
    let old_reader: crate::NewWorkflowRun = serde_json::from_str(&json).unwrap();
    assert_eq!(
        old_reader.definition_id,
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("edit", &definition)
            .unwrap()
            .definition_id
    );
    let mut future = binding;
    future.version = 999;
    let (db, owner, definition) = fixture_with_binding(Some(&future)).await;
    assert!(matches!(
        publish(&*db, &owner, &edit(&definition)).await,
        Err(Error::UnsupportedLifecycle)
    ));
    assert_eq!(current(&*db, "edit").await.unwrap().0, 1);
}

fn edit(definition: &WorkflowDefinition) -> WorkflowRunGraphEditBatch {
    let mut node = definition.nodes["last"].clone();
    node.id = "worker".into();
    node.name = "worker".into();
    WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: "edit".into(),
        expected_revision: 1,
        mutation_id: "insert-worker".into(),
        edits: vec![
            Edit::AddNode {
                node,
                entry: false,
                exit: false,
            },
            Edit::ReplaceEdge {
                edge_id: 1,
                edge: EdgeDefinition {
                    from: "first".into(),
                    to: "worker".into(),
                    kind: bcode_workflow::EdgeKind::Direct,
                    transform: None,
                },
            },
            Edit::AddEdge {
                edge_id: 2,
                edge: EdgeDefinition {
                    from: "worker".into(),
                    to: "last".into(),
                    kind: bcode_workflow::EdgeKind::Direct,
                    transform: None,
                },
            },
        ],
        reconciliation: vec![],
    }
}
#[tokio::test]
async fn pending_reconnect_is_revision_checked_idempotent_and_preserves_admission() {
    let (db, owner, definition) = fixture().await;
    let before = db
        .select(super::super::RUNS)
        .where_eq("run_id", "edit")
        .execute(&*db)
        .await
        .unwrap()[0]
        .get("definition_json");
    let batch = edit(&definition);
    assert_eq!(publish(&*db, &owner, &batch).await.unwrap(), 2);
    assert_eq!(publish(&*db, &owner, &batch).await.unwrap(), 2);
    let mut stale = batch.clone();
    stale.mutation_id = "stale".into();
    assert!(publish(&*db, &owner, &stale).await.is_err());
    let mut conflict = batch;
    conflict.edits.pop();
    assert!(publish(&*db, &owner, &conflict).await.is_err());
    assert_eq!(current(&*db, "edit").await.unwrap().1.nodes.len(), 3);
    assert_eq!(
        graph::ready(&*db, "edit", 2, None, 10).await.unwrap().len(),
        1
    );
    assert!(graph::ready(&*db, "edit", 1, None, 10).await.is_err());
    assert_eq!(
        db.select(super::super::RUNS)
            .where_eq("run_id", "edit")
            .execute(&*db)
            .await
            .unwrap()[0]
            .get("definition_json"),
        before
    );
}
#[tokio::test]
async fn active_incident_changes_fail_but_independent_pending_changes_retain() {
    let (db, owner, definition) = fixture().await;
    let prepared = crate::PreparedAttempt {
        run_id: "edit".into(),
        node_id: "first".into(),
        activation_id: graph::activation_id(&*db, "edit", "first")
            .await
            .unwrap()
            .unwrap(),
        attempt: 1,
        side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({}),
        prepared_at_ms: 1,
    };
    attempts::prepare(&*db, &owner, &prepared).await.unwrap();
    let mut batch = edit(&definition);
    batch.reconciliation = vec![Reconcile::Retain {
        activation_id: graph::activation_id(&*db, "edit", "first")
            .await
            .unwrap()
            .unwrap(),
    }];
    assert!(matches!(
        publish(&*db, &owner, &batch).await,
        Err(Error::UnsupportedLifecycle)
    ));
    let mut last = definition.nodes["last"].clone();
    last.name = "renamed pending".into();
    batch.mutation_id = "pending".into();
    batch.edits = vec![Edit::ReplaceNode {
        node: last,
        entry: false,
        exit: true,
    }];
    assert_eq!(publish(&*db, &owner, &batch).await.unwrap(), 2);
    settle_retained(&*db, &owner, &prepared).await;
    assert_eq!(
        graph::ready(&*db, "edit", 2, None, 10).await.unwrap()[0].input,
        serde_json::json!(12)
    );
    assert_eq!(
        graph::output(&*db, "edit", "first").await.unwrap(),
        Some(serde_json::json!(12))
    );
    assert_eq!(
        attempts::prepare(&*db, &owner, &prepared).await.unwrap(),
        prepared.dispatch_identity()
    );
    assert_eq!(
        attempts::pending(&*db, "edit", None, 10)
            .await
            .unwrap()
            .len(),
        0
    );
}

async fn settle_retained(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    prepared: &crate::PreparedAttempt,
) {
    let identity = prepared.dispatch_identity();
    attempts::attach_receipt(
        db,
        owner,
        &crate::DispatchReceipt {
            run_id: "edit".into(),
            node_id: "first".into(),
            activation_id: prepared.activation_id.clone(),
            attempt: 1,
            dispatch_identity: identity.clone(),
            receipt: serde_json::json!({}),
            admitted_at_ms: 2,
        },
    )
    .await
    .unwrap();
    attempts::settle(
        db,
        owner,
        "edit",
        &identity,
        attempts::TerminalOutcome::Succeeded,
        3,
    )
    .await
    .unwrap();
    graph::settle_output(db, owner, "edit", "first", 2, &serde_json::json!(12))
        .await
        .unwrap();
}

#[tokio::test]
async fn active_source_binds_two_workers_and_joins_without_redispatch() {
    let (db, owner, definition) = fixture().await;
    let source = graph::activation_id(&*db, "edit", "first")
        .await
        .unwrap()
        .unwrap();
    let prepared = crate::PreparedAttempt {
        run_id: "edit".into(),
        node_id: "first".into(),
        activation_id: source.clone(),
        attempt: 1,
        side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({}),
        prepared_at_ms: 1,
    };
    let dispatch = attempts::prepare(&*db, &owner, &prepared).await.unwrap();
    let batch = delegation(&definition, &source);
    assert_eq!(publish(&*db, &owner, &batch).await.unwrap(), 2);
    assert_eq!(
        graph::activation_id(&*db, "edit", "first").await.unwrap(),
        Some(source)
    );
    assert_eq!(
        attempts::prepare(&*db, &owner, &prepared).await.unwrap(),
        dispatch
    );
    settle_retained(&*db, &owner, &prepared).await;
    let ready = graph::ready(&*db, "edit", 2, None, 10).await.unwrap();
    assert_eq!(ready.len(), 2);
    assert!(ready.iter().all(|a| a.input == serde_json::json!(12)));
    for (node, value) in [("right", 20), ("left", 10)] {
        let activation = graph::activation_id(&*db, "edit", node)
            .await
            .unwrap()
            .unwrap();
        let work = crate::PreparedAttempt {
            run_id: "edit".into(),
            node_id: node.into(),
            activation_id: activation.clone(),
            attempt: 1,
            side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
            intent: serde_json::json!({}),
            prepared_at_ms: 4,
        };
        let id = attempts::prepare(&*db, &owner, &work).await.unwrap();
        attempts::attach_receipt(
            &*db,
            &owner,
            &crate::DispatchReceipt {
                run_id: "edit".into(),
                node_id: node.into(),
                activation_id: activation,
                attempt: 1,
                dispatch_identity: id.clone(),
                receipt: serde_json::json!({}),
                admitted_at_ms: 5,
            },
        )
        .await
        .unwrap();
        attempts::settle(
            &*db,
            &owner,
            "edit",
            &id,
            attempts::TerminalOutcome::Succeeded,
            6,
        )
        .await
        .unwrap();
        let settled =
            graph::settle_output(&*db, &owner, "edit", node, 2, &serde_json::json!(value))
                .await
                .unwrap();
        if node == "left" {
            assert!(settled.graph_output.is_none());
            assert_eq!(
                graph::output(&*db, "edit", "join").await.unwrap(),
                Some(serde_json::json!([10, 20]))
            );
        }
    }
    assert_eq!(
        graph::output(&*db, "edit", "first").await.unwrap(),
        Some(serde_json::json!(12))
    );
    assert_eq!(definition.nodes.len(), 2);
}

fn delegation(definition: &WorkflowDefinition, source: &str) -> WorkflowRunGraphEditBatch {
    let task =
        |name: &str| bcode_workflow::Step::task(name, |value: u32, _| async move { Ok(value) });
    let workers = bcode_workflow::WorkflowBuilder::new(
        "workers",
        bcode_workflow::parallel_named("join", task("left"), task("right")),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let mut edits = vec![
        Edit::RemoveEdge { edge_id: 1 },
        Edit::RemoveNode {
            node_id: "last".into(),
        },
    ];
    for node in workers.nodes.values() {
        edits.push(Edit::AddNode {
            node: node.clone(),
            entry: false,
            exit: false,
        });
    }
    let edge = |to: &str| EdgeDefinition {
        from: "first".into(),
        to: to.into(),
        kind: bcode_workflow::EdgeKind::Direct,
        transform: None,
    };
    edits.push(Edit::AddEdge {
        edge_id: 2,
        edge: edge("left"),
    });
    edits.push(Edit::AddEdge {
        edge_id: 3,
        edge: edge("right"),
    });
    for (i, edge) in workers.edges.iter().enumerate() {
        edits.push(Edit::AddEdge {
            edge_id: 4 + u64::try_from(i).unwrap(),
            edge: edge.clone(),
        });
    }
    let mut last = definition.nodes["last"].clone();
    last.input = bcode_workflow::ValueSchema::of::<(u32, u32)>();
    edits.push(Edit::AddNode {
        node: last,
        entry: false,
        exit: true,
    });
    edits.push(Edit::AddEdge {
        edge_id: 6,
        edge: EdgeDefinition {
            from: "join".into(),
            to: "last".into(),
            kind: bcode_workflow::EdgeKind::Direct,
            transform: None,
        },
    });
    WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: "edit".into(),
        expected_revision: 1,
        mutation_id: "delegate".into(),
        edits,
        reconciliation: vec![Reconcile::RetainWithBindings {
            activation_id: source.into(),
            edge_ids: vec![2, 3],
        }],
    }
}

#[tokio::test]
async fn replacing_pending_activation_retires_history_and_increments_generation() {
    let (db, owner, definition) = fixture().await;
    let old = graph::ready(&*db, "edit", 1, None, 10)
        .await
        .unwrap()
        .remove(0);
    let mut node = definition.nodes["first"].clone();
    node.name = "replacement".into();
    let batch = WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: "edit".into(),
        expected_revision: 1,
        mutation_id: "replace-pending".into(),
        edits: vec![Edit::ReplaceNode {
            node,
            entry: true,
            exit: false,
        }],
        reconciliation: vec![],
    };
    publish(&*db, &owner, &batch).await.unwrap();
    let new = graph::ready(&*db, "edit", 2, None, 10)
        .await
        .unwrap()
        .remove(0);
    assert_ne!(old.activation_id, new.activation_id);
    assert_eq!(new.generation, old.generation + 1);
    assert!(
        !graph::is_ready_activation(&*db, "edit", "first", &old.activation_id)
            .await
            .unwrap()
    );
    assert!(
        graph::is_ready_activation(&*db, "edit", "first", &new.activation_id)
            .await
            .unwrap()
    );
    let history = db
        .select("workflow_activations_v2")
        .where_eq("run_id", "edit")
        .where_eq("node_id", "first")
        .execute(&*db)
        .await
        .unwrap();
    assert_eq!(history.len(), 2);
}

#[tokio::test]
async fn edge_pages_preserve_edit_identity_and_never_reuse_retired_highwater() {
    let (db, owner, definition) = fixture().await;
    let mut batch = edit(&definition);
    for operation in &mut batch.edits {
        if let Edit::AddEdge { edge_id, .. } = operation {
            *edge_id = 99;
        }
    }
    publish(&*db, &owner, &batch).await.unwrap();
    let page = super::super::graph_edges::page(&*db, "edit", 2, None, None, 10)
        .await
        .unwrap();
    assert_eq!(
        page.iter().map(|edge| edge.ordinal).collect::<Vec<_>>(),
        vec![1, 99]
    );
    let retire = WorkflowRunGraphEditBatch {
        version: batch.version,
        run_id: "edit".into(),
        expected_revision: 2,
        mutation_id: "retire".into(),
        edits: vec![
            Edit::RemoveEdge { edge_id: 99 },
            Edit::RemoveNode {
                node_id: "last".into(),
            },
            Edit::ReplaceNode {
                node: current(&*db, "edit").await.unwrap().1.nodes["worker"].clone(),
                entry: false,
                exit: true,
            },
        ],
        reconciliation: vec![],
    };
    publish(&*db, &owner, &retire).await.unwrap();
    assert_eq!(
        super::super::graph_edges::next_edge_id(&*db, "edit")
            .await
            .unwrap(),
        Some(100)
    );
    assert_eq!(
        super::super::graph_edges::page(&*db, "edit", 3, None, None, 10)
            .await
            .unwrap()[0]
            .ordinal,
        1
    );
}
