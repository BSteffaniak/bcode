use super::*;

/// Extra ambiguous edges are canonical fixtures; malformed unselected tail is not read.
pub(super) async fn ambiguous_edges(db: &dyn Database, caller: &WorkflowExecutionSessionLink) {
    let edges = super::super::super::graph_edges::edges(
        db,
        &caller.run_id,
        1,
        &caller.node_id,
        super::super::super::graph_edges::EdgeDirection::Incoming,
        None,
        1,
    )
    .await
    .unwrap();
    let mut edge = edges[0].edge.clone();
    edge.from = "sibling".into();
    let json = serde_json::to_string(&edge).unwrap();
    for ordinal in 100_i64..=102 {
        db.insert("workflow_definition_edges_v2")
            .value("identity", format!("ambiguous-{ordinal}"))
            .value("run_id", caller.run_id.as_str())
            .value("revision", 1_i64)
            .value("ordinal", ordinal)
            .value("source", "sibling")
            .value("target", caller.node_id.as_str())
            .value("is_back", false)
            .value(
                "edge_json",
                if ordinal == 102 {
                    "malformed-unselected-tail"
                } else {
                    &json
                },
            )
            .execute(db)
            .await
            .unwrap();
    }
}
