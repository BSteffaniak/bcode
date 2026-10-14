use super::*;

#[tokio::test]
async fn current_public_capabilities_drive_eval_capture_without_legacy_events() {
    let (_root, state) = fixture_with_store(true);
    admit_extension_with_observations(&state, true).await;
    let inspected = inspection::inspect(&state, "new", 100)
        .await
        .unwrap()
        .unwrap();
    assert!(inspected.unavailable_collections.contains(&"events".into()));
    let wire = serde_json::to_value(&inspected).unwrap();
    let capabilities: bcode_workflow::WorkflowRunInspection = serde_json::from_value(wire).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().to_path_buf();
    let handle = tokio::runtime::Handle::current();
    let state = std::sync::Arc::new(state);
    tokio::task::spawn_blocking(move || {
        bcode_eval::capture_workflow_history(&capabilities, &path, |args, _, _| {
            let after = args
                .windows(2)
                .find(|pair| pair[0] == "--after-sequence")
                .map(|pair| pair[1].parse::<u64>().unwrap());
            let through = args
                .windows(2)
                .find(|pair| pair[0] == "--through-sequence")
                .map(|pair| pair[1].parse::<u64>().unwrap());
            match args[1].as_str() {
                "observations" => Ok(serde_json::to_value(
                    handle
                        .block_on(discovery::observations(&state, "new", after, through, 100))
                        .unwrap(),
                )
                .unwrap()),
                "attempts" => Ok(serde_json::to_value(
                    handle
                        .block_on(discovery::attempts(&state, "new", None, 100))
                        .unwrap(),
                )
                .unwrap()),
                "semantic-history-run" => Ok(serde_json::to_value(
                    handle
                        .block_on(discovery::semantic_history(
                            &state,
                            &bcode_workflow::WorkflowSemanticHistoryQuery {
                                version: bcode_workflow::WORKFLOW_SEMANTIC_HISTORY_VERSION,
                                scope: bcode_workflow::WorkflowSemanticHistoryScope::AttemptsV1,
                                run_id: "new".into(),
                                cursor: None,
                                limit: 100,
                            },
                        ))
                        .unwrap(),
                )
                .unwrap()),
                "lifecycle-history-run" => Ok(serde_json::to_value(
                    handle
                        .block_on(discovery::lifecycle_history(
                            &state,
                            &bcode_workflow::WorkflowLifecycleHistoryQuery {
                                version: bcode_workflow::WORKFLOW_LIFECYCLE_HISTORY_VERSION,
                                scope:
                                    bcode_workflow::WorkflowLifecycleHistoryScope::RunLifecycleV1,
                                run_id: "new".into(),
                                cursor: None,
                                limit: 100,
                            },
                        ))
                        .unwrap(),
                )
                .unwrap()),
                "public-events" => Ok(serde_json::to_value(
                    handle
                        .block_on(discovery::committed_history(
                            &state,
                            &bcode_workflow::WorkflowCommittedHistoryQuery {
                                version: bcode_workflow::WORKFLOW_COMMITTED_HISTORY_VERSION,
                                run_id: "new".into(),
                                after_sequence: after.unwrap_or(0),
                                through_sequence: through,
                                limit: 100,
                            },
                        ))
                        .unwrap(),
                )
                .unwrap()),
                other => panic!("unsupported collection requested: {other}"),
            }
        })
        .unwrap();
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path.join("capture-completeness.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["boundaries"]["events"]["status"], "unsupported");
        assert_eq!(manifest["boundaries"]["observations"]["status"], "complete");
        assert_eq!(manifest["complete"], false);
        assert!(!path.join("workflow-events-0000.json").exists());
    })
    .await
    .unwrap();
}

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
