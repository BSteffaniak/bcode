//! Publication-only delegator binding preserves exact authored candidates.
use super::*;

#[test]
fn execution_parent_binding_is_separate_from_candidate_and_rejects_forgery() {
    let schema = ValueSchema {
        type_name: "unit".into(),
        schema: serde_json::json!({"type":"null"}),
    };
    let node = NodeDefinition {
        id: "worker".into(),
        name: "worker".into(),
        kind: NodeKind::Agent,
        dataflow: WorkflowNodeDataflowPolicy::default(),
        input: schema.clone(),
        output: schema,
        resources: Vec::new(),
        configuration: serde_json::json!({"version":5}),
    };
    let batch = WorkflowRunGraphEditBatch {
        version: WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: "run".into(),
        expected_revision: 1,
        mutation_id: "delegation".into(),
        edits: vec![WorkflowRunGraphEdit::AddNode {
            node,
            entry: true,
            exit: true,
        }],
        reconciliation: Vec::new(),
    };
    reject_execution_parent_claims(&batch).unwrap();
    let admitted = bind_execution_parent(&batch, "actual-worker-session").unwrap();
    assert_ne!(admitted, batch);
    reject_execution_parent_claims(&batch).unwrap();
    assert!(reject_execution_parent_claims(&admitted).is_err());
    let WorkflowRunGraphEdit::AddNode { node, .. } = &admitted.edits[0] else {
        panic!("node");
    };
    let mut authored = WorkflowDefinition {
        schema_version: 1,
        name: "authored".into(),
        input: node.input.clone(),
        output: node.output.clone(),
        nodes: BTreeMap::from([(node.id.clone(), node.clone())]),
        entries: vec![node.id.clone()],
        exits: vec![node.id.clone()],
        edges: Vec::new(),
    };
    assert!(reject_definition_execution_parent_claims(&authored).is_err());
    authored
        .nodes
        .get_mut("worker")
        .unwrap()
        .configuration
        .as_object_mut()
        .unwrap()
        .remove("execution_parent");
    reject_definition_execution_parent_claims(&authored).unwrap();
    let binding: WorkflowExecutionParentBinding =
        serde_json::from_value(node.configuration["execution_parent"].clone()).unwrap();
    assert_eq!(binding.version, 1);
    assert_eq!(binding.session_id, "actual-worker-session");
    assert!(bind_execution_parent(&batch, "").is_err());
}
