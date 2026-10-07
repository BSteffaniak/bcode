use super::{ValidatedOutput, WorkflowExecutionAuthority, WorkflowExecutionSessionLink};
use bcode_workflow::{WorkflowRunGraphEdit as Edit, WorkflowRunGraphReconciliation as Reconcile};
use bcode_workflow_store::representation::{execution_graph, graph};
use switchy_database::{Database, query::FilterableQuery};

pub(super) async fn generated_publication(
    db: &dyn switchy_database::Database,
    owner: &WorkflowExecutionAuthority,
    source: &WorkflowExecutionSessionLink,
    definition: &bcode_workflow::WorkflowDefinition,
) {
    let mut worker = definition.nodes["source"].clone();
    worker.id = "worker".into();
    let mut integration = worker.clone();
    integration.id = "integrate".into();
    integration.output = bcode_workflow::ValueSchema::of::<serde_json::Value>();
    let edge = |from: &str, to: &str| bcode_workflow::EdgeDefinition {
        from: from.into(),
        to: to.into(),
        kind: bcode_workflow::EdgeKind::Direct,
        transform: None,
    };
    let mut reconnect = edge("integrate", "evaluate");
    reconnect.transform = Some(bcode_workflow::WorkflowTransform {
        version: bcode_workflow::WORKFLOW_TRANSFORM_VERSION,
        expression: bcode_workflow::WorkflowTransformExpression::Input {
            source: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_CURRENT.into(),
            path: "source".into(),
        },
        output: definition.nodes["source"].output.clone(),
    });
    let batch = bcode_workflow::WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: source.run_id.clone(),
        expected_revision: 1,
        mutation_id: "generated-publication".into(),
        edits: vec![
            Edit::AddNode {
                node: worker,
                entry: false,
                exit: false,
            },
            Edit::AddNode {
                node: integration,
                entry: false,
                exit: false,
            },
            Edit::AddEdge {
                edge_id: 2,
                edge: edge("source", "worker"),
            },
            Edit::AddEdge {
                edge_id: 3,
                edge: edge("worker", "integrate"),
            },
            Edit::ReplaceEdge {
                edge_id: 1,
                edge: reconnect,
            },
        ],
        reconciliation: vec![Reconcile::RetainWithBindings {
            activation_id: source.activation_id.clone(),
            edge_ids: vec![2],
        }],
    };
    assert!(
        execution_graph::stage_run_graph_edit_from_execution(db, &batch, owner, source, 2)
            .await
            .unwrap()
    );
    assert_eq!(
        execution_graph::publication_from_execution(db, &batch, owner, source)
            .await
            .unwrap(),
        2
    );
}

pub(super) async fn seed_contributors(
    state: &super::ServerState,
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    run: &super::NewWorkflowRun,
    parent: super::SessionId,
    definition: &bcode_workflow::WorkflowDefinition,
) -> (super::SessionId, u64, u64) {
    let worker = super::dispatch(state, db, owner, run, "worker", parent).await;
    let evidence_source = worker.session_id.parse().unwrap();
    let (request_sequence, result_sequence) = super::seed_receipt(state, evidence_source).await;
    generated_complete(
        db,
        owner,
        &worker,
        &definition.nodes["source"].output,
        serde_json::json!(1),
    )
    .await;
    let sibling = super::dispatch(state, db, owner, run, "sibling", parent).await;
    super::seed_receipt(state, sibling.session_id.parse().unwrap()).await;
    generated_complete(
        db,
        owner,
        &sibling,
        &definition.nodes["source"].output,
        serde_json::json!(1),
    )
    .await;
    let integration = super::dispatch(state, db, owner, run, "integrate", parent).await;
    generated_complete(
        db,
        owner,
        &integration,
        &bcode_workflow::ValueSchema::of::<serde_json::Value>(),
        serde_json::json!({"source":1,"worker":"integrated"}),
    )
    .await;
    (evidence_source, request_sequence, result_sequence)
}

pub(super) async fn generated_complete(
    db: &dyn switchy_database::Database,
    owner: &WorkflowExecutionAuthority,
    link: &WorkflowExecutionSessionLink,
    schema: &bcode_workflow::ValueSchema,
    value: serde_json::Value,
) {
    graph::completion::complete(
        db,
        owner,
        2,
        &bcode_workflow_store::dispatch_identity(
            &link.run_id,
            &link.node_id,
            &link.activation_id,
            link.attempt,
        ),
        &ValidatedOutput {
            output_id: format!("output-{}", link.node_id),
            run_id: link.run_id.clone(),
            node_id: link.node_id.clone(),
            activation_id: link.activation_id.clone(),
            schema_id: schema.type_name.clone(),
            schema_version: 1,
            value,
            artifact_reference: None,
            created_at_ms: 3,
        },
    )
    .await
    .unwrap();
}

impl super::Fixture {
    pub async fn damage_publication(&self, corrupt: bool) {
        let db = switchy_database_connection::open_sqlite_rusqlite(
            &self.path,
            &switchy_database_connection::SqliteOpenOptions {
                access: switchy_database_connection::SqliteAccess::ReadWrite,
                allow_uri: false,
                busy_timeout: std::time::Duration::ZERO,
                foreign_keys: true,
                connections: std::num::NonZeroUsize::MIN,
            },
        )
        .unwrap();
        if corrupt {
            db.update("workflow_graph_revisions_v2")
                .value("request_json", "unknown-corrupt-contract")
                .where_eq("run_id", "contributor-application")
                .where_eq("revision", 2_i64)
                .execute(&db)
                .await
                .unwrap();
        } else {
            db.delete("workflow_graph_revisions_v2")
                .where_eq("run_id", "contributor-application")
                .where_eq("revision", 2_i64)
                .execute(&db)
                .await
                .unwrap();
        }
    }
}
