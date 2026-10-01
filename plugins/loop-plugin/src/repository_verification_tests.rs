use super::*;

fn request() -> Request {
    parse(json!({"run_id":"run","expected_revision":1,"bind_source_activation":"active","mutation_id":"verify","repository_target":{"version":1,"commit":"a".repeat(40)},"cwd":".","commands":[["cargo","test"]],"report_objective":"Check the complete original goal","reconciliation":[]})).unwrap()
}

fn context() -> workflow::WorkflowExecutionContext {
    let schema = workflow::ValueSchema::of::<Value>();
    let Edit::AddNode { node, .. } = verification_node(
        "coordinator".into(),
        workflow::NodeKind::Agent,
        schema.clone(),
        schema,
        Value::Null,
    ) else {
        unreachable!()
    };
    workflow::WorkflowExecutionContext {
        run_id: "run".into(),
        node_id: "coordinator".into(),
        activation_id: "active".into(),
        attempt: 0,
        execution_allowance: None,
        output: None,
        outputs: vec![],
        graph: workflow::WorkflowRunGraphInspection {
            revision: 1,
            next_edge_id: Some(10),
            nodes: vec![workflow::WorkflowRunGraphNodeInspection {
                revision: 1,
                node,
                entry: true,
                exit: false,
            }],
            edges: vec![workflow::WorkflowRunGraphEdgeInspection {
                revision: 1,
                edge_id: 1,
                edge: verification_edge("coordinator".into(), "evaluate".into(), None),
            }],
            nodes_complete: true,
            edges_complete: true,
        },
    }
}

#[test]
fn repository_handoff_is_direct_dependent_and_preserves_source() {
    let edit = lower(&request(), &context()).unwrap();
    let producer = edit
        .edits
        .iter()
        .find_map(|e| match e {
            Edit::AddNode { node, .. } if node.kind == workflow::NodeKind::PluginBlock => {
                Some(node)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(
        producer.dataflow,
        workflow::WorkflowNodeDataflowPolicy::Direct
    );
    let block: workflow::WorkflowBlockDefinition =
        serde_json::from_value(producer.configuration.clone()).unwrap();
    assert_eq!(
        (block.plugin_id.as_str(), block.operation.as_str()),
        ("bcode.shell", "exec")
    );
    assert!(block.preparation_required);
    assert!(block.authorization.explicit_grant_required);
    let transform = edit
        .edits
        .iter()
        .find_map(|e| match e {
            Edit::AddEdge { edge, .. } if edge.to == producer.id => edge.transform.as_ref(),
            _ => None,
        })
        .unwrap();
    let plan = transform.evaluate(&[]).unwrap();
    assert_eq!(
        plan["repository_target"],
        json!(request().repository_target)
    );
    assert_eq!(plan["commands"][0]["argv"], json!(["cargo", "test"]));
    let transform = edit
        .edits
        .iter()
        .find_map(|e| match e {
            Edit::ReplaceEdge { edge, .. } => edge.transform.as_ref(),
            _ => None,
        })
        .unwrap();
    let source =
        json!({"stop_condition":"PINNED","implementation_prompt":"original","condition_met":false});
    let collected = json!([[source,{"untrusted":"shell result"}],{"condition_met":true,"stop_condition":"replacement"}]);
    assert_eq!(
        transform
            .evaluate(&[workflow::WorkflowTransformInput {
                name: "current",
                value: &collected
            }])
            .unwrap(),
        source
    );
    assert!(
        matches!(&edit.reconciliation[0],workflow::WorkflowRunGraphReconciliation::RetainWithBindings {edge_ids,..} if edge_ids == &[10,11])
    );
}

#[test]
fn repository_handoff_reconnects_a_normal_collaborating_goal() {
    let input = super::super::LoopWorkflowInput::new(
        "Implement useful repository work".into(),
        "Pinned original condition".into(),
        2,
    )
    .unwrap();
    let spec =
        super::super::collaborating_goal_spec(&super::super::loop_workflow_spec(&input).unwrap())
            .unwrap();
    let mut definition = spec.definition().clone();
    let mut execution = context();
    execution.node_id = "loop.implementation".into();
    execution.graph.nodes[0].node = definition.nodes[&execution.node_id].clone();
    let index = definition
        .edges
        .iter()
        .position(|e| e.from == execution.node_id)
        .unwrap();
    execution.graph.edges[0].edge = definition.edges[index].clone();
    let edit = lower(&request(), &execution).unwrap();
    for change in edit.edits {
        match change {
            Edit::AddNode { node, .. } => {
                definition.nodes.insert(node.id.clone(), node);
            }
            Edit::AddEdge { edge, .. } => definition.edges.push(edge),
            Edit::ReplaceEdge { edge, .. } => definition.edges[index] = edge,
            _ => panic!("unexpected edit"),
        }
    }
    workflow::WorkflowSpec::<super::super::LoopWorkflowIteration>::from_definition(
        "repository-handoff",
        definition,
    )
    .unwrap();
}

#[test]
fn repeated_repository_handoff_preserves_original_source_and_rejects_report_selection() {
    let mut execution = context();
    let first = lower(&request(), &execution).unwrap();
    let edge = first
        .edits
        .iter()
        .find_map(|edit| match edit {
            Edit::ReplaceEdge { edge, .. } => Some(edge.clone()),
            _ => None,
        })
        .unwrap();
    execution.node_id = edge.from.clone();
    execution.graph.nodes[0].node.id = edge.from.clone();
    execution.graph.edges[0].edge = edge;
    let second = lower(&request(), &execution).unwrap();
    let transform = second
        .edits
        .iter()
        .find_map(|edit| match edit {
            Edit::ReplaceEdge { edge, .. } => edge.transform.as_ref(),
            _ => None,
        })
        .unwrap();
    let original = json!({"stop_condition":"unchanged", "condition_met":false});
    let first_result = json!([[original, {"checks":"old"}], {"report":"old"}]);
    let repeated = json!([[first_result, {"checks":"new"}], {"report":"new"}]);
    assert_eq!(
        transform
            .evaluate(&[workflow::WorkflowTransformInput {
                name: "current",
                value: &repeated
            }])
            .unwrap(),
        original
    );
    let transform = execution.graph.edges[0].edge.transform.as_mut().unwrap();
    let workflow::WorkflowTransformExpression::SelectedInput { selector, .. } =
        &mut transform.expression
    else {
        panic!("expected selector")
    };
    selector.segments[1] = workflow::WorkflowValueSelectorSegment::Index { index: 1 };
    assert!(lower(&request(), &execution).is_err());
}

#[test]
fn repository_handoff_rejects_stale_context_ambiguous_successors_and_invalid_targets() {
    let mut execution = context();
    execution.graph.revision = 2;
    assert!(lower(&request(), &execution).is_err());
    execution = context();
    execution.graph.edges_complete = false;
    assert!(lower(&request(), &execution).is_err());
    execution = context();
    execution.graph.edges.push(execution.graph.edges[0].clone());
    assert!(lower(&request(), &execution).is_err());
    let mut value = json!(request());
    value["repository_target"]["version"] = json!(99);
    assert!(parse(value).is_err());
    let mut value = json!(request());
    value["cwd"] = json!("../outside");
    assert!(parse(value).is_err());
}
