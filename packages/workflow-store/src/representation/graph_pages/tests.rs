use super::super::tests::{database, run};
use super::*;
#[tokio::test]
async fn graph_larger_than_old_envelope_has_bounded_exact_node_pages() {
    let db = database().await;
    super::super::install(&*db).await.unwrap();
    let mut definition = bcode_workflow::WorkflowBuilder::new(
        "large",
        bcode_workflow::Step::task("seed", |v: u32, _| async move { Ok(v) }),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let seed = definition.nodes["seed"].clone();
    definition.nodes.clear();
    definition.entries.clear();
    definition.edges.clear();
    for index in 0..160 {
        let mut node = seed.clone();
        node.id = format!("n{index:04}");
        node.configuration = serde_json::json!({"context":"x".repeat(8192)});
        definition.nodes.insert(node.id.clone(), node);
        if index > 0 {
            definition.edges.push(bcode_workflow::EdgeDefinition {
                from: format!("n{:04}", index - 1),
                to: format!("n{index:04}"),
                kind: bcode_workflow::EdgeKind::Direct,
                transform: None,
            });
        }
    }
    definition.entries = vec!["n0000".into()];
    definition.exits = vec!["n0159".into()];
    assert!(serde_json::to_vec(&definition).unwrap().len() > 1_048_576);
    let mut request = run("large", false);
    let identity =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("large", &definition).unwrap();
    request.definition_id = identity.definition_id;
    request.definition_version = identity.definition_version;
    assert!(
        super::super::admit_with_policy(
            &*db,
            &request,
            &definition,
            super::super::GraphAdmissionPolicy {
                max_snapshot_bytes: Some(1024)
            }
        )
        .await
        .is_err()
    );
    super::super::admit(&*db, &request, &definition)
        .await
        .unwrap();
    assert_eq!(nodes(&*db, "large", 1, None, 32).await.unwrap().len(), 32);
    let (revision, ready) = super::super::graph::ready_nodes(&*db, "large", None, 32)
        .await
        .unwrap();
    assert_eq!(revision, 1);
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].1.id, "n0000");
    assert_eq!(
        node(&*db, "large", 1, "n0159").await.unwrap().unwrap().id,
        "n0159"
    );
}
