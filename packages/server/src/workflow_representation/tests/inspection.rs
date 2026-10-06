use super::*;

#[tokio::test]
async fn intermediate_exit_is_not_read_as_terminal_output() {
    let (_root, state) = fixture_with_store(true);
    admit_extension(&state).await;
    let db = open(&crate::workflow_store_path(&state), true).unwrap();
    // Dynamic revision may temporarily have no exit while the run is still nonterminal.
    let boundary = representation::graph_edges::Boundary {
        entries: vec!["worker".into()],
        exits: vec![],
        output: bcode_workflow::ValueSchema::of::<u32>(),
    };
    db.update("workflow_definition_boundary_v2")
        .value("boundary_json", serde_json::to_string(&boundary).unwrap())
        .execute(&db)
        .await
        .unwrap();
    let result = inspection::inspect(&state, "new", 100)
        .await
        .unwrap()
        .unwrap();
    assert!(result.terminal_output.is_none());
}

#[tokio::test]
async fn inspection_routes_both_families_and_marks_unavailable_evidence() {
    let (_root, state) = fixture_with_store(true);
    admit_extension(&state).await;
    let inspected = inspection::inspect(&state, "new", 100)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inspected.run.run_id, "new");
    assert_eq!(inspected.graph.unwrap().nodes[0].node.id, "worker");
    assert!(inspected.unavailable_collections.contains(&"events".into()));
    assert!(inspected.terminal_output.is_none());
    assert!(
        inspection::inspect(&state, "legacy", 100)
            .await
            .unwrap()
            .is_none()
    );
    let rows = discovery::list(&state, 100).await.unwrap();
    assert!(rows.iter().any(|run| run.run_id == "legacy"));
    assert!(rows.iter().any(|run| run.run_id == "new"));
    assert!(discovery::events(&state, "new", None, 100).await.is_err());
    assert!(
        discovery::attempts(&state, "new", None, 100)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn graph_pages_reject_stale_revision_and_do_not_fallback_to_legacy() {
    let (_root, state) = fixture_with_store(true);
    admit_extension(&state).await;
    let mut request = bcode_workflow::WorkflowRunGraphPageRequest {
        run_id: "new".into(),
        expected_revision: 1,
        after_node_id: None,
        after_edge_id: None,
        limit: 1,
    };
    let graph = inspection_graph::page(&state, &request).await.unwrap();
    assert!(graph.nodes_complete && graph.edges_complete);
    request.expected_revision = 2;
    assert!(inspection_graph::page(&state, &request).await.is_err());
    request.run_id = "missing".into();
    assert!(inspection_graph::page(&state, &request).await.is_err());
}
