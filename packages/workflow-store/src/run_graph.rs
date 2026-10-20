//! Run-owned graph materialization and bounded inspection.

use super::{WorkflowStore, WorkflowStoreError};
use bcode_workflow::{EdgeDefinition, NodeDefinition, WorkflowDefinition};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

type CompletedResultEdges = BTreeMap<String, (u64, u64, EdgeDefinition)>;
use switchy_database::query::{FilterableQuery, SortDirection};
use switchy_database::{Database, DatabaseValue};

fn graph_boolean(row: &switchy_database::Row, column: &str) -> Result<bool, WorkflowStoreError> {
    match row.get(column) {
        Some(DatabaseValue::Int64(0)) => Ok(false),
        Some(DatabaseValue::Int64(1)) => Ok(true),
        _ => Err(WorkflowStoreError::InvalidData(
            "workflow graph node revision or boundary flags are invalid".into(),
        )),
    }
}

fn graph_stored_identity(
    row: &switchy_database::Row,
    column: &str,
) -> Result<String, WorkflowStoreError> {
    let value = super::database::text(row, column).map_err(|_| {
        WorkflowStoreError::InvalidData(format!(
            "{column} must contain 1..={} bytes; invalid graph identity storage",
            super::MAX_ID_BYTES
        ))
    })?;
    super::validate_id(column, &value)?;
    Ok(value)
}

fn graph_endpoint_identity(
    row: &switchy_database::Row,
    column: &str,
) -> Result<String, WorkflowStoreError> {
    graph_stored_identity(row, column)
        .map_err(|error| WorkflowStoreError::InvalidData(format!("invalid edge endpoint: {error}")))
}

fn optional_graph_unsigned(
    row: &switchy_database::Row,
    column: &str,
) -> Result<Option<u64>, WorkflowStoreError> {
    match row.get(column) {
        Some(DatabaseValue::Null) => Ok(None),
        _ => super::database::unsigned(row, column).map(Some),
    }
}

const GRAPH_PAGE_LIMIT: usize = 100;

/// Publication disposition and verified delegator share one immutable commit scope.
#[derive(Clone, Copy)]
struct PublicationBinding<'a> {
    retain_active: bool,
    immediate_parent: Option<&'a str>,
}

const fn editable_connected_kind(kind: bcode_workflow::NodeKind) -> bool {
    matches!(
        kind,
        bcode_workflow::NodeKind::Task
            | bcode_workflow::NodeKind::Agent
            | bcode_workflow::NodeKind::PluginBlock
            | bcode_workflow::NodeKind::Input
            | bcode_workflow::NodeKind::Approval
            | bcode_workflow::NodeKind::Parallel
            | bcode_workflow::NodeKind::WorkflowCall
    )
}

#[cfg(test)]
fn graph_test_database() -> switchy_database::rusqlite::RusqliteDatabase {
    super::database::open(
        std::path::Path::new(":memory:"),
        switchy_database_connection::SqliteAccess::ReadWriteCreate,
        std::time::Duration::from_secs(1),
        false,
    )
    .expect("database")
}

#[cfg(test)]
fn graph_test_table(connection: &dyn Database, table: &str, columns: &[(&str, bool)]) {
    use switchy_database::schema::{Column, DataType, create_table};
    let mut statement = create_table(table).columns(
        columns
            .iter()
            .map(|(name, integer)| Column {
                name: (*name).to_string(),
                nullable: true,
                auto_increment: false,
                data_type: if *integer {
                    DataType::BigInt
                } else {
                    DataType::Text
                },
                default: None,
            })
            .collect(),
    );
    if table == "workflow_run_graph_edges" {
        statement = statement.primary_key_columns(["run_id", "edge_id", "revision"]);
    }
    crate::database::block_on(statement.execute(connection)).expect("fixture table");
}

#[cfg(test)]
mod reconciliation_tests {
    use super::*;

    #[test]
    fn affected_controller_requires_running_member_disposition() {
        let connection = graph_test_database();
        graph_test_table(
            &connection,
            "workflow_activations",
            &[
                ("run_id", false),
                ("node_id", false),
                ("activation_id", false),
                ("status", false),
            ],
        );
        graph_test_table(
            &connection,
            "workflow_fan_out_members",
            &[
                ("run_id", false),
                ("controller_node_id", false),
                ("member_activation_id", false),
                ("status", false),
            ],
        );
        crate::database::block_on(
            connection
                .insert("workflow_fan_out_members")
                .value("run_id", "run")
                .value("controller_node_id", "controller")
                .value("member_activation_id", "member")
                .value("status", "running")
                .execute(&connection),
        )
        .unwrap();
        let mut request = bcode_workflow::WorkflowRunGraphEditBatch {
            version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
            run_id: "run".to_string(),
            mutation_id: "edit".to_string(),
            expected_revision: 1,
            edits: vec![bcode_workflow::WorkflowRunGraphEdit::RemoveNode {
                node_id: "controller".to_string(),
            }],
            reconciliation: vec![],
        };
        let before = super::super::database::rows(
            &connection,
            &connection.select("workflow_fan_out_members"),
        )
        .unwrap();
        assert!(validate_affected_work(&connection, &request, &[]).is_err());
        request
            .reconciliation
            .push(bcode_workflow::WorkflowRunGraphReconciliation::Retain {
                activation_id: "member".to_string(),
            });
        validate_affected_work(&connection, &request, &[]).expect("explicit member disposition");
        assert_eq!(
            before,
            super::super::database::rows(
                &connection,
                &connection.select("workflow_fan_out_members")
            )
            .unwrap()
        );
    }

    fn repeated_target_fixture() -> switchy_database::rusqlite::RusqliteDatabase {
        let connection = graph_test_database();
        graph_test_table(
            &connection,
            "workflow_activations",
            &[
                ("run_id", false),
                ("node_id", false),
                ("activation_id", false),
                ("status", false),
                ("dependency_generation", true),
            ],
        );
        for (node, identity, status, generation) in [
            ("source", "source-1", "running", 1),
            ("target", "target-0", "completed", 0),
        ] {
            crate::database::block_on(
                connection
                    .insert("workflow_activations")
                    .value("run_id", "run")
                    .value("node_id", node)
                    .value("activation_id", identity)
                    .value("status", status)
                    .value("dependency_generation", DatabaseValue::Int64(generation))
                    .execute(&connection),
            )
            .unwrap();
        }
        connection
    }

    #[test]
    fn repeated_target_requires_settled_older_generation() {
        let connection = repeated_target_fixture();
        let edge = EdgeDefinition {
            from: "source".into(),
            to: "target".into(),
            kind: bcode_workflow::EdgeKind::Direct,
            transform: None,
        };
        let mut request = bcode_workflow::WorkflowRunGraphEditBatch {
            version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
            run_id: "run".into(),
            mutation_id: "edit".into(),
            expected_revision: 1,
            edits: vec![],
            reconciliation: vec![],
        };
        assert!(
            !WorkflowStore::target_is_only_from_prior_generations(&connection, &request, &edge)
                .unwrap()
        );
        request.reconciliation.push(
            bcode_workflow::WorkflowRunGraphReconciliation::RetainWithBindings {
                activation_id: "source-1".into(),
                edge_ids: vec![1],
            },
        );
        assert!(
            WorkflowStore::target_is_only_from_prior_generations(&connection, &request, &edge)
                .unwrap()
        );
        for (status, generation) in [
            ("pending", 0),
            ("running", 0),
            ("waiting_approval", 0),
            ("waiting_input", 0),
            ("waiting_mutation_approval", 0),
            ("unknown", 0),
            ("completed", 1),
            ("completed", 2),
        ] {
            crate::database::block_on(
                connection
                    .update("workflow_activations")
                    .value("status", status)
                    .value("dependency_generation", DatabaseValue::Int64(generation))
                    .where_eq("node_id", "target")
                    .execute(&connection),
            )
            .unwrap();
            assert!(
                !WorkflowStore::target_is_only_from_prior_generations(&connection, &request, &edge)
                    .unwrap(),
                "{status}/{generation}"
            );
        }
        crate::database::block_on(
            connection
                .update("workflow_activations")
                .value("dependency_generation", DatabaseValue::Int64(0))
                .where_eq("node_id", "target")
                .execute(&connection),
        )
        .unwrap();
        crate::database::block_on(
            connection
                .update("workflow_activations")
                .value("status", "completed")
                .where_eq("node_id", "source")
                .execute(&connection),
        )
        .unwrap();
        assert!(
            !WorkflowStore::target_is_only_from_prior_generations(&connection, &request, &edge)
                .unwrap()
        );
    }

    #[test]
    fn affected_dependencies_follow_current_and_new_edges_without_looping() {
        let edge = |from: &str, to: &str| EdgeDefinition {
            from: from.to_string(),
            to: to.to_string(),
            kind: bcode_workflow::EdgeKind::default(),
            transform: None,
        };
        let current = vec![
            RunGraphEdge {
                revision: 1,
                edge_id: 0,
                edge: edge("first", "second"),
            },
            RunGraphEdge {
                revision: 1,
                edge_id: 1,
                edge: edge("second", "third"),
            },
            RunGraphEdge {
                revision: 1,
                edge_id: 2,
                edge: edge("third", "first"),
            },
        ];
        let request = bcode_workflow::WorkflowRunGraphEditBatch {
            version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
            run_id: "run".to_string(),
            mutation_id: "edit".to_string(),
            expected_revision: 1,
            edits: vec![bcode_workflow::WorkflowRunGraphEdit::ReplaceEdge {
                edge_id: 1,
                edge: edge("second", "new"),
            }],
            reconciliation: vec![],
        };
        let mut affected = BTreeSet::from(["first"]);
        expand_affected_dependencies(&mut affected, &request, &current);
        assert_eq!(
            affected,
            BTreeSet::from(["first", "second", "third", "new"])
        );
    }
}

#[derive(Clone, Copy)]
pub enum EdgeEndpoint<'a> {
    Source(&'a str),
    Target(&'a str),
}

/// An immutable executable node revision in a run-owned graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunGraphNode {
    /// Revision at which this node representation was admitted.
    pub revision: u64,
    /// Exact executable node data.
    pub node: NodeDefinition,
    /// Whether the initial plan admits this node as an entry.
    pub entry: bool,
    /// Whether the initial plan declares this node an exit.
    pub exit: bool,
}

/// One immutable edge representation in an admitted run graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunGraphEdge {
    /// Revision at which this edge representation was admitted.
    pub revision: u64,
    /// Stable identity assigned during initial materialization.
    pub edge_id: u64,
    /// Exact admitted edge, including transforms and control flow.
    pub edge: EdgeDefinition,
}

pub fn initialize_retirement(connection: &dyn Database) -> Result<(), WorkflowStoreError> {
    use switchy_database::schema::{
        DataType, SchemaComparison as C, SchemaExpression as E, alter_table, create_index,
    };
    for (table, retirement_index, live_index, identity) in [
        (
            "workflow_run_graph_nodes",
            "workflow_run_graph_nodes_retirement",
            "workflow_run_graph_nodes_live",
            "node_id",
        ),
        (
            "workflow_run_graph_edges",
            "workflow_run_graph_edges_retirement",
            "workflow_run_graph_edges_live",
            "edge_id",
        ),
    ] {
        crate::database::block_on(
            alter_table(table)
                .add_column_check(
                    "retired_at_revision".to_string(),
                    DataType::BigInt,
                    true,
                    None,
                    E::column("retired_at_revision").compare(C::Greater, E::column("revision")),
                )
                .execute(connection),
        )?;
        crate::database::block_on(
            create_index(retirement_index)
                .table(table)
                .columns(vec!["run_id", "retired_at_revision"])
                .execute(connection),
        )?;
        crate::database::block_on(
            create_index(live_index)
                .table(table)
                .columns(vec!["run_id", identity])
                .predicate(E::column("retired_at_revision").compare(C::Is, E::Null))
                .execute(connection),
        )?;
    }
    Ok(())
}

fn initialize_pending_publications(connection: &dyn Database) -> Result<(), WorkflowStoreError> {
    use switchy_database::schema::{Column, DataType, create_index, create_table};
    crate::database::block_on(
        create_table("workflow_pending_publications")
            .if_not_exists(true)
            .columns(vec![
                Column {
                    name: "run_id".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
                Column {
                    name: "mutation_id".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
                Column {
                    name: "expected_revision".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::BigInt,
                    default: None,
                },
                Column {
                    name: "accepted_at_ms".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::BigInt,
                    default: None,
                },
            ])
            .primary_key_columns(["run_id", "mutation_id"])
            .composite_foreign_key(
                ["run_id", "mutation_id"],
                "workflow_graph_edit_candidates",
                ["run_id", "mutation_id"],
            )
            .execute(connection),
    )?;
    crate::database::block_on(
        create_table("workflow_publication_cancellations")
            .if_not_exists(true)
            .columns(vec![
                Column {
                    name: "run_id".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
                Column {
                    name: "mutation_id".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
                Column {
                    name: "dispatch_identity".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
            ])
            .primary_key_columns(["run_id", "mutation_id", "dispatch_identity"])
            .foreign_key_column(
                "dispatch_identity",
                "workflow_attempts",
                "dispatch_identity",
            )
            .composite_foreign_key(
                ["run_id", "mutation_id"],
                "workflow_pending_publications",
                ["run_id", "mutation_id"],
            )
            .execute(connection),
    )?;
    crate::database::block_on(
        create_index("workflow_publication_cancellation_attempt")
            .table("workflow_publication_cancellations")
            .if_not_exists(true)
            .columns(vec!["dispatch_identity"])
            .execute(connection),
    )?;
    Ok(())
}

pub fn initialize_edit_candidates(connection: &dyn Database) -> Result<(), WorkflowStoreError> {
    initialize_pending_publications(connection)?;
    initialize_workflow_dispatch_handoffs(connection)?;
    initialize_workflow_fan_out_running_controller(connection)?;
    initialize_workflow_activations_running_node(connection)?;
    initialize_workflow_activations_identity(connection)?;
    initialize_workflow_activation_graph_bindings(connection)?;
    initialize_workflow_graph_edit_candidates(connection)?;
    initialize_workflow_graph_edit_publications(connection)?;
    initialize_workflow_retained_edge_bindings(connection)?;
    initialize_workflow_leaf_retentions(connection)?;
    initialize_workflow_graph_edit_validations(connection)?;
    initialize_workflow_graph_edit_nodes(connection)?;
    initialize_workflow_graph_edit_edges(connection)?;
    Ok(())
}

fn initialize_workflow_dispatch_handoffs(
    connection: &dyn Database,
) -> Result<(), WorkflowStoreError> {
    use switchy_database::schema::{
        Column, DataType, SchemaComparison as C, SchemaExpression as E, create_table,
    };
    crate::database::block_on(
        create_table("workflow_dispatch_handoffs")
            .if_not_exists(true)
            .columns(vec![
                Column {
                    name: "dispatch_identity".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
                Column {
                    name: "handed_off".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::BigInt,
                    default: None,
                },
            ])
            .primary_key_columns(["dispatch_identity"])
            .foreign_key_column(
                "dispatch_identity",
                "workflow_attempts",
                "dispatch_identity",
            )
            .check(E::Or(vec![
                E::column("handed_off").compare(C::Equal, E::Integer(0)),
                E::column("handed_off").compare(C::Equal, E::Integer(1)),
            ]))
            .execute(connection),
    )?;
    Ok(())
}

fn initialize_workflow_fan_out_running_controller(
    connection: &dyn Database,
) -> Result<(), WorkflowStoreError> {
    use switchy_database::schema::{SchemaComparison as C, SchemaExpression as E, create_index};
    crate::database::block_on(
        create_index("workflow_fan_out_running_controller")
            .table("workflow_fan_out_members")
            .if_not_exists(true)
            .columns(vec!["run_id", "controller_node_id", "member_activation_id"])
            .predicate(E::column("status").compare(C::Equal, E::Text("running".to_string())))
            .execute(connection),
    )?;
    Ok(())
}

fn initialize_workflow_activations_running_node(
    connection: &dyn Database,
) -> Result<(), WorkflowStoreError> {
    use switchy_database::schema::{SchemaComparison as C, SchemaExpression as E, create_index};
    crate::database::block_on(
        create_index("workflow_activations_running_node")
            .table("workflow_activations")
            .if_not_exists(true)
            .columns(vec!["run_id", "node_id", "activation_id"])
            .predicate(E::column("status").compare(C::Equal, E::Text("running".to_string())))
            .execute(connection),
    )?;
    Ok(())
}

fn initialize_workflow_activations_identity(
    connection: &dyn Database,
) -> Result<(), WorkflowStoreError> {
    use switchy_database::schema::create_index;
    crate::database::block_on(
        create_index("workflow_activations_identity")
            .table("workflow_activations")
            .if_not_exists(true)
            .columns(vec!["run_id", "activation_id"])
            .execute(connection),
    )?;
    Ok(())
}

fn initialize_workflow_activation_graph_bindings(
    connection: &dyn Database,
) -> Result<(), WorkflowStoreError> {
    use switchy_database::schema::{
        Column, DataType, SchemaComparison as C, SchemaExpression as E, create_table,
    };
    crate::database::block_on(
        create_table("workflow_activation_graph_bindings")
            .if_not_exists(true)
            .columns(vec![
                Column {
                    name: "run_id".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
                Column {
                    name: "node_id".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
                Column {
                    name: "activation_id".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
                Column {
                    name: "graph_revision".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::BigInt,
                    default: None,
                },
            ])
            .primary_key_columns(["run_id", "node_id", "activation_id"])
            .check(E::column("graph_revision").compare(C::Greater, E::Integer(0)))
            .composite_foreign_key(
                ["run_id", "node_id", "activation_id"],
                "workflow_activations",
                ["run_id", "node_id", "activation_id"],
            )
            .execute(connection),
    )?;
    Ok(())
}

fn initialize_workflow_graph_edit_candidates(
    connection: &dyn Database,
) -> Result<(), WorkflowStoreError> {
    use switchy_database::schema::{
        Column, DataType, SchemaComparison as C, SchemaExpression as E, create_table,
    };
    crate::database::block_on(
        create_table("workflow_graph_edit_candidates")
            .if_not_exists(true)
            .columns(vec![
                Column {
                    name: "run_id".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
                Column {
                    name: "mutation_id".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
                Column {
                    name: "expected_revision".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::BigInt,
                    default: None,
                },
                Column {
                    name: "request_json".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
                Column {
                    name: "authority_json".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
                Column {
                    name: "created_at_ms".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::BigInt,
                    default: None,
                },
            ])
            .primary_key_columns(["run_id", "mutation_id"])
            .foreign_key_column("run_id", "workflow_runs", "run_id")
            .check(E::column("expected_revision").compare(C::Greater, E::Integer(0)))
            .execute(connection),
    )?;
    Ok(())
}

fn initialize_workflow_graph_edit_publications(
    connection: &dyn Database,
) -> Result<(), WorkflowStoreError> {
    use switchy_database::schema::{
        Column, DataType, SchemaComparison as C, SchemaExpression as E, create_table,
    };
    crate::database::block_on(
        create_table("workflow_graph_edit_publications")
            .if_not_exists(true)
            .columns(vec![
                Column {
                    name: "run_id".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
                Column {
                    name: "mutation_id".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
                Column {
                    name: "revision".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::BigInt,
                    default: None,
                },
            ])
            .primary_key_columns(["run_id", "mutation_id"])
            .check(E::column("revision").compare(C::Greater, E::Integer(1)))
            .composite_foreign_key(
                ["run_id", "mutation_id"],
                "workflow_graph_edit_candidates",
                ["run_id", "mutation_id"],
            )
            .execute(connection),
    )?;
    Ok(())
}

fn initialize_workflow_retained_edge_bindings(
    connection: &dyn Database,
) -> Result<(), WorkflowStoreError> {
    use switchy_database::schema::{
        Column, DataType, SchemaComparison as C, SchemaExpression as E, create_table,
    };
    crate::database::block_on(
        create_table("workflow_retained_edge_bindings")
            .if_not_exists(true)
            .columns(vec![
                Column {
                    name: "run_id".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
                Column {
                    name: "mutation_id".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
                Column {
                    name: "graph_revision".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::BigInt,
                    default: None,
                },
                Column {
                    name: "edge_id".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::BigInt,
                    default: None,
                },
                Column {
                    name: "edge_revision".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::BigInt,
                    default: None,
                },
                Column {
                    name: "source_node_id".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
                Column {
                    name: "source_activation_id".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
            ])
            .primary_key_columns(["run_id", "graph_revision", "edge_id"])
            .check(E::column("graph_revision").compare(C::Greater, E::Integer(1)))
            .composite_foreign_key(
                ["run_id", "mutation_id"],
                "workflow_graph_edit_publications",
                ["run_id", "mutation_id"],
            )
            .composite_foreign_key(
                ["run_id", "edge_id", "edge_revision"],
                "workflow_run_graph_edges",
                ["run_id", "edge_id", "revision"],
            )
            .composite_foreign_key(
                ["run_id", "source_node_id", "source_activation_id"],
                "workflow_activations",
                ["run_id", "node_id", "activation_id"],
            )
            .execute(connection),
    )?;
    Ok(())
}

fn initialize_workflow_leaf_retentions(
    connection: &dyn Database,
) -> Result<(), WorkflowStoreError> {
    use switchy_database::schema::{
        Column, DataType, SchemaComparison as C, SchemaExpression as E, create_table,
    };
    crate::database::block_on(
        create_table("workflow_leaf_retentions")
            .if_not_exists(true)
            .columns(vec![
                Column {
                    name: "run_id".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
                Column {
                    name: "node_id".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
                Column {
                    name: "activation_id".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
                Column {
                    name: "revision".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::BigInt,
                    default: None,
                },
            ])
            .primary_key_columns(["run_id", "node_id", "activation_id", "revision"])
            .check(E::column("revision").compare(C::Greater, E::Integer(1)))
            .composite_foreign_key(
                ["run_id", "node_id", "activation_id"],
                "workflow_activations",
                ["run_id", "node_id", "activation_id"],
            )
            .execute(connection),
    )?;
    Ok(())
}

fn initialize_workflow_graph_edit_validations(
    connection: &dyn Database,
) -> Result<(), WorkflowStoreError> {
    use switchy_database::schema::{
        Column, DataType, SchemaComparison as C, SchemaExpression as E, create_table,
    };
    crate::database::block_on(
        create_table("workflow_graph_edit_validations")
            .if_not_exists(true)
            .columns(vec![
                Column {
                    name: "run_id".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
                Column {
                    name: "mutation_id".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
                Column {
                    name: "expected_revision".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::BigInt,
                    default: None,
                },
            ])
            .primary_key_columns(["run_id", "mutation_id"])
            .check(E::column("expected_revision").compare(C::Greater, E::Integer(0)))
            .composite_foreign_key(
                ["run_id", "mutation_id"],
                "workflow_graph_edit_candidates",
                ["run_id", "mutation_id"],
            )
            .execute(connection),
    )?;
    Ok(())
}

fn initialize_workflow_graph_edit_nodes(
    connection: &dyn Database,
) -> Result<(), WorkflowStoreError> {
    use switchy_database::schema::{
        Column, DataType, SchemaComparison as C, SchemaExpression as E, create_table,
    };
    crate::database::block_on(
        create_table("workflow_graph_edit_nodes")
            .if_not_exists(true)
            .columns(vec![
                Column {
                    name: "run_id".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
                Column {
                    name: "mutation_id".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
                Column {
                    name: "node_id".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
                Column {
                    name: "node_json".to_string(),
                    nullable: true,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
                Column {
                    name: "is_entry".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::BigInt,
                    default: None,
                },
                Column {
                    name: "is_exit".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::BigInt,
                    default: None,
                },
            ])
            .primary_key_columns(["run_id", "mutation_id", "node_id"])
            .check(E::Or(vec![
                E::column("is_entry").compare(C::Equal, E::Integer(0)),
                E::column("is_entry").compare(C::Equal, E::Integer(1)),
            ]))
            .check(E::Or(vec![
                E::column("is_exit").compare(C::Equal, E::Integer(0)),
                E::column("is_exit").compare(C::Equal, E::Integer(1)),
            ]))
            .composite_foreign_key(
                ["run_id", "mutation_id"],
                "workflow_graph_edit_validations",
                ["run_id", "mutation_id"],
            )
            .execute(connection),
    )?;
    Ok(())
}

fn initialize_workflow_graph_edit_edges(
    connection: &dyn Database,
) -> Result<(), WorkflowStoreError> {
    use switchy_database::schema::{
        Column, DataType, SchemaComparison as C, SchemaExpression as E, create_table,
    };
    crate::database::block_on(
        create_table("workflow_graph_edit_edges")
            .if_not_exists(true)
            .columns(vec![
                Column {
                    name: "run_id".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
                Column {
                    name: "mutation_id".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
                Column {
                    name: "edge_id".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::BigInt,
                    default: None,
                },
                Column {
                    name: "edge_json".to_string(),
                    nullable: true,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
            ])
            .primary_key_columns(["run_id", "mutation_id", "edge_id"])
            .check(E::column("edge_id").compare(C::GreaterEqual, E::Integer(0)))
            .composite_foreign_key(
                ["run_id", "mutation_id"],
                "workflow_graph_edit_validations",
                ["run_id", "mutation_id"],
            )
            .execute(connection),
    )?;
    Ok(())
}

/// Structural validation outcome; neither variant authorizes graph publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunGraphCandidateValidation {
    /// The bounded candidate graph passed domain structural validation.
    Validated,
    /// The graph exceeds one validation slice and requires incremental validation.
    RequiresIncrementalValidation,
}

impl WorkflowStore {
    /// Read an activation's explicitly recorded admission graph revision.
    ///
    /// Missing activations and historical admissions without this fact return `None`.
    /// The executable node revision is not a substitute for the admitted graph revision.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid identities, inconsistent or future bindings, missing
    /// executable data, or database failures. This bounded read never reconstructs history.
    pub fn activation_admitted_graph_revision(
        &self,
        run_id: &str,
        node_id: &str,
        activation_id: &str,
    ) -> Result<Option<u64>, WorkflowStoreError> {
        validate_activation_node_request(run_id, node_id, activation_id)?;
        let transaction = super::database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let rows = super::database::rows(
            transaction.as_ref(),
            &transaction
                .select("workflow_activation_graph_bindings")
                .columns(&["graph_revision"])
                .where_eq("run_id", run_id)
                .where_eq("node_id", node_id)
                .where_eq("activation_id", activation_id)
                .limit(1),
        )?;
        let revision = rows
            .first()
            .map(|row| super::database::unsigned(row, "graph_revision"))
            .transpose()?;
        if let Some(revision) = revision {
            let current = graph_revision(transaction.as_ref(), run_id)?.ok_or_else(|| {
                WorkflowStoreError::InvalidData("activation graph is missing".to_string())
            })?;
            let node = activation_node_record_in_snapshot(
                transaction.as_ref(),
                run_id,
                node_id,
                activation_id,
            )?
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData("activation executable is missing".to_string())
            })?;
            if revision == 0 || revision > current || node.revision > revision {
                return Err(WorkflowStoreError::InvalidData(
                    "invalid activation admission graph revision".to_string(),
                ));
            }
        }
        super::database::commit(transaction)?;
        Ok(revision)
    }

    /// Read the exact candidate's publication outcome without mutating durable state.
    ///
    /// Returns `None` for candidates that have not been accepted or committed. A committed
    /// result takes precedence over later graph revisions. Conflicted acceptance retains
    /// all cancellation intents; this read never rebases or revokes them.
    ///
    /// # Errors
    /// Rejects stale authority, missing graphs, inconsistent revisions, or database errors.
    pub fn run_graph_publication_status(
        &self,
        run_id: &str,
        mutation_id: &str,
        authority: &super::WorkflowExecutionAuthority,
    ) -> Result<Option<bcode_workflow::WorkflowRunGraphPublicationStatus>, WorkflowStoreError> {
        let transaction = super::database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let status = Self::run_graph_publication_status_in(
            transaction.as_ref(),
            run_id,
            mutation_id,
            authority,
        )?;
        super::database::commit(transaction)?;
        Ok(status)
    }

    fn run_graph_publication_status_in(
        transaction: &dyn Database,
        run_id: &str,
        mutation_id: &str,
        authority: &super::WorkflowExecutionAuthority,
    ) -> Result<Option<bcode_workflow::WorkflowRunGraphPublicationStatus>, WorkflowStoreError> {
        use bcode_workflow::WorkflowRunGraphPublicationStatus;
        Self::verify_execution_authority_in(transaction, run_id, authority)?;
        let current_revision = graph_revision(transaction, run_id)?
            .ok_or_else(|| WorkflowStoreError::InvalidData("publication graph missing".into()))?;
        let rows = super::database::rows(
            transaction,
            &transaction
                .select("workflow_graph_edit_publications")
                .columns(&["revision"])
                .where_eq("run_id", run_id)
                .where_eq("mutation_id", mutation_id)
                .limit(1),
        )?;
        if let Some(row) = rows.first() {
            let revision = super::database::unsigned(row, "revision")?;
            if revision > current_revision {
                return Err(WorkflowStoreError::InvalidData(
                    "publication revision exceeds graph".into(),
                ));
            }
            return Ok(Some(WorkflowRunGraphPublicationStatus::Committed {
                revision,
            }));
        }
        let rows = super::database::rows(
            transaction,
            &transaction
                .select("workflow_pending_publications")
                .columns(&["expected_revision"])
                .where_eq("run_id", run_id)
                .where_eq("mutation_id", mutation_id)
                .limit(1),
        )?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let expected_revision = super::database::unsigned(row, "expected_revision")?;
        if expected_revision > current_revision {
            return Err(WorkflowStoreError::InvalidData(
                "accepted revision exceeds graph".into(),
            ));
        }
        let mut terminal_conflict = false;
        if let Some(candidate) =
            Self::staged_run_graph_edit_in(transaction, run_id, mutation_id, authority)?
        {
            for disposition in candidate.reconciliation {
                let bcode_workflow::WorkflowRunGraphReconciliation::Cancel { activation_id } =
                    disposition
                else {
                    continue;
                };
                let rows = super::database::rows(
                    transaction,
                    &transaction
                        .select("workflow_activations")
                        .columns(&["activation_id"])
                        .where_eq("run_id", run_id)
                        .where_eq("activation_id", activation_id)
                        .where_or(switchy_database::boxed![
                            switchy_database::query::where_eq("status", "completed"),
                            switchy_database::query::where_eq("status", "failed"),
                            switchy_database::query::where_eq("status", "skipped")
                        ])
                        .limit(1),
                )?;
                terminal_conflict |= !rows.is_empty();
            }
        }
        Ok(Some(
            if expected_revision == current_revision && !terminal_conflict {
                WorkflowRunGraphPublicationStatus::Pending { expected_revision }
            } else {
                WorkflowRunGraphPublicationStatus::Conflicted {
                    expected_revision,
                    current_revision,
                }
            },
        ))
    }

    /// Accept an exact staged candidate and its receipt-backed cancellation intents atomically.
    ///
    /// This does not publish a revision or signal owners. The caller must authorize the exact
    /// candidate before acceptance. Duplicate acceptance preserves the original attempt set.
    ///
    /// # Errors
    /// Rejects stale authority/revision, invalid candidates, non-receipt-backed or linked targets,
    /// and persistence failures. Rejection rolls back acceptance and all intents.
    pub fn accept_pending_run_graph_publication(
        &self,
        run_id: &str,
        mutation_id: &str,
        authority: &super::WorkflowExecutionAuthority,
        accepted_at_ms: u64,
    ) -> Result<bool, WorkflowStoreError> {
        let transaction = super::database::begin(
            &self.connection,
            switchy_database::TransactionMode::Immediate,
        )?;
        let result = Self::accept_pending_publication(
            run_id,
            mutation_id,
            authority,
            accepted_at_ms,
            transaction.as_ref(),
        )?;
        super::database::commit(transaction)?;
        Ok(result)
    }

    /// Accept an authorized candidate and return its lifecycle status atomically.
    ///
    /// # Errors
    /// Rejects stale authority, invalid candidates, or persistence failures.
    pub fn accept_run_graph_publication(
        &self,
        run_id: &str,
        mutation_id: &str,
        authority: &super::WorkflowExecutionAuthority,
        accepted_at_ms: u64,
    ) -> Result<bcode_workflow::WorkflowRunGraphPublicationStatus, WorkflowStoreError> {
        let transaction = super::database::begin(
            &self.connection,
            switchy_database::TransactionMode::Immediate,
        )?;
        if let Some(status) = Self::run_graph_publication_status_in(
            transaction.as_ref(),
            run_id,
            mutation_id,
            authority,
        )? {
            return Ok(status);
        }
        Self::accept_pending_publication(
            run_id,
            mutation_id,
            authority,
            accepted_at_ms,
            transaction.as_ref(),
        )?;
        let status = Self::run_graph_publication_status_in(
            transaction.as_ref(),
            run_id,
            mutation_id,
            authority,
        )?
        .ok_or_else(|| WorkflowStoreError::InvalidData("accepted publication missing".into()))?;
        super::database::commit(transaction)?;
        Ok(status)
    }

    /// Accept cancellation intents only while the authenticated execution caller remains active.
    ///
    /// # Errors
    /// Rejects inactive callers, stale authority, invalid candidates, or persistence failures.
    pub fn accept_pending_run_graph_publication_from_execution(
        &self,
        mutation_id: &str,
        authority: &super::WorkflowExecutionAuthority,
        caller: &super::WorkflowExecutionSessionLink,
        accepted_at_ms: u64,
    ) -> Result<bcode_workflow::WorkflowRunGraphPublicationStatus, WorkflowStoreError> {
        let transaction = super::database::begin(
            &self.connection,
            switchy_database::TransactionMode::Immediate,
        )?;
        Self::verify_execution_authority_in(transaction.as_ref(), &caller.run_id, authority)?;
        Self::verify_active_graph_edit_caller_in(transaction.as_ref(), &caller.run_id, caller)?;
        let status = Self::run_graph_publication_status_in(
            transaction.as_ref(),
            &caller.run_id,
            mutation_id,
            authority,
        )?;
        let outcome = if let Some(status) = status {
            status
        } else {
            Self::accept_pending_publication(
                &caller.run_id,
                mutation_id,
                authority,
                accepted_at_ms,
                transaction.as_ref(),
            )?;
            Self::bind_authenticated_candidate_parent(
                transaction.as_ref(),
                &caller.run_id,
                mutation_id,
                authority,
                &caller.session_id,
            )?;
            Self::run_graph_publication_status_in(
                transaction.as_ref(),
                &caller.run_id,
                mutation_id,
                authority,
            )?
            .ok_or_else(|| WorkflowStoreError::InvalidData("accepted publication missing".into()))?
        };
        super::database::commit(transaction)?;
        Ok(outcome)
    }

    fn accept_pending_publication(
        run_id: &str,
        mutation_id: &str,
        authority: &super::WorkflowExecutionAuthority,
        accepted_at_ms: u64,
        transaction: &dyn Database,
    ) -> Result<bool, WorkflowStoreError> {
        let request = Self::staged_run_graph_edit_in(transaction, run_id, mutation_id, authority)?
            .ok_or_else(|| WorkflowStoreError::InvalidData("candidate not found".into()))?;
        let exists = !super::database::rows(
            transaction,
            &transaction
                .select("workflow_pending_publications")
                .columns(&["mutation_id"])
                .where_eq("run_id", run_id)
                .where_eq("mutation_id", mutation_id)
                .limit(1),
        )?
        .is_empty();
        if exists {
            return Ok(false);
        }
        if Self::validate_run_graph_edit_in_transaction(
            run_id,
            mutation_id,
            authority,
            transaction,
        )? != RunGraphCandidateValidation::Validated
        {
            return Err(WorkflowStoreError::InvalidData(
                "candidate requires incremental validation".into(),
            ));
        }
        crate::database::block_on(
            transaction
                .insert("workflow_pending_publications")
                .value("run_id", run_id)
                .value("mutation_id", mutation_id)
                .value(
                    "expected_revision",
                    DatabaseValue::Int64(i64::try_from(request.expected_revision).map_err(
                        |_| WorkflowStoreError::InvalidData("invalid graph revision".to_string()),
                    )?),
                )
                .value(
                    "accepted_at_ms",
                    DatabaseValue::Int64(i64::try_from(accepted_at_ms).map_err(|_| {
                        WorkflowStoreError::InvalidData("invalid acceptance timestamp".to_string())
                    })?),
                )
                .execute(transaction),
        )?;
        Self::persist_publication_cancellation_targets(transaction, &request)?;
        super::append_event_in(transaction, run_id, "graph_publication_accepted",
            &serde_json::json!({"mutation_id": mutation_id, "expected_revision": request.expected_revision}).to_string(), accepted_at_ms)?;
        Ok(true)
    }

    fn persist_publication_cancellation_targets(
        transaction: &dyn Database,
        request: &bcode_workflow::WorkflowRunGraphEditBatch,
    ) -> Result<(), WorkflowStoreError> {
        let run_id = request.run_id.as_str();
        let mutation_id = request.mutation_id.as_str();
        let mut count = 0;
        for disposition in &request.reconciliation {
            let bcode_workflow::WorkflowRunGraphReconciliation::Cancel { activation_id } =
                disposition
            else {
                continue;
            };
            if publication_local_cancellation(transaction, run_id, activation_id)?
                || publication_fan_out_local_target(transaction, request, activation_id)?
            {
                continue;
            }
            let controllers = request
                .reconciliation
                .iter()
                .filter_map(|item| match item {
                    bcode_workflow::WorkflowRunGraphReconciliation::Cancel { activation_id } => {
                        Some(DatabaseValue::String(activation_id.clone()))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            let linked = !super::database::rows(
                transaction,
                &transaction
                    .select("workflow_run_links")
                    .columns(&["parent_activation_id"])
                    .where_eq("parent_run_id", run_id)
                    .where_eq("parent_activation_id", activation_id.as_str())
                    .limit(1),
            )?
            .is_empty();
            let foreign_member = !super::database::rows(
                transaction,
                &transaction
                    .select("workflow_fan_out_members")
                    .columns(&["member_activation_id"])
                    .where_eq("run_id", run_id)
                    .where_eq("member_activation_id", activation_id.as_str())
                    .where_not_in("controller_activation_id", controllers)
                    .limit(1),
            )?
            .is_empty();
            let identities = if linked || foreign_member {
                Vec::new()
            } else {
                super::database::rows(
                    transaction,
                    &transaction
                        .select("workflow_attempts")
                        .project_as(
                            switchy_database::query::bounded_text(
                                switchy_database::query::identifier("dispatch_identity"),
                                super::MAX_ID_BYTES as u64,
                                DatabaseValue::Null,
                            ),
                            "dispatch_identity",
                        )
                        .where_eq("run_id", run_id)
                        .where_eq("activation_id", activation_id.as_str())
                        .where_or(switchy_database::boxed![
                            switchy_database::query::where_eq("status", "admitted"),
                            switchy_database::query::where_eq("status", "running")
                        ])
                        .where_not_eq("receipt_json", DatabaseValue::Null)
                        .limit(2),
                )?
                .iter()
                .map(|row| super::database::text(row, "dispatch_identity"))
                .collect::<Result<Vec<_>, _>>()?
            };
            let [identity] = identities.as_slice() else {
                return Err(WorkflowStoreError::InvalidData("pending publication requires one receipt-backed unlinked attempt per cancellation".into()));
            };
            crate::database::block_on(
                transaction
                    .insert("workflow_publication_cancellations")
                    .value("run_id", run_id)
                    .value("mutation_id", mutation_id)
                    .value("dispatch_identity", identity.as_str())
                    .execute(transaction),
            )?;
            count += 1;
        }
        if count == 0 {
            return Err(WorkflowStoreError::InvalidData(
                "pending publication requires cancellation targets".into(),
            ));
        }
        Ok(())
    }

    /// Validate a persisted edit against one bounded snapshot of the committed graph.
    ///
    /// This validates structure, not execution reconciliation or caller permissions. Large graphs
    /// remain preserved and explicitly require incremental validation rather than being truncated.
    /// Successful validation is persisted for this candidate and graph revision, but never
    /// authorizes publication or bypasses subsequent ownership and reconciliation checks.
    ///
    /// # Errors
    ///
    /// Returns an error for stale ownership/revision, damaged state, invalid structural edits,
    /// missing candidates, or database failures.
    pub fn validate_staged_run_graph_edit(
        &self,
        run_id: &str,
        mutation_id: &str,
        authority: &super::WorkflowExecutionAuthority,
    ) -> Result<RunGraphCandidateValidation, WorkflowStoreError> {
        let transaction = super::database::begin(
            &self.connection,
            switchy_database::TransactionMode::Immediate,
        )?;
        let result = Self::validate_run_graph_edit_in_transaction(
            run_id,
            mutation_id,
            authority,
            transaction.as_ref(),
        )?;
        super::database::commit(transaction)?;
        Ok(result)
    }

    fn validate_run_graph_edit_in_transaction(
        run_id: &str,
        mutation_id: &str,
        authority: &super::WorkflowExecutionAuthority,
        transaction: &dyn Database,
    ) -> Result<RunGraphCandidateValidation, WorkflowStoreError> {
        let request = Self::staged_run_graph_edit_in(transaction, run_id, mutation_id, authority)?
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData("graph edit candidate not found".to_string())
            })?;
        ensure_run_accepts_graph_edits(transaction, run_id)?;
        validate_reconciliation_targets(transaction, &request)?;
        // Node-local errors must not be hidden behind the graph paging boundary.
        // Global structure and execution reconciliation still require validation below.
        for edit in &request.edits {
            match edit {
                bcode_workflow::WorkflowRunGraphEdit::AddNode { node, .. }
                | bcode_workflow::WorkflowRunGraphEdit::ReplaceNode { node, .. } => node
                    .validate_structure()
                    .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?,
                bcode_workflow::WorkflowRunGraphEdit::AddEdge { edge, .. }
                | bcode_workflow::WorkflowRunGraphEdit::ReplaceEdge { edge, .. } => edge
                    .validate_structure()
                    .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?,
                bcode_workflow::WorkflowRunGraphEdit::RemoveNode { .. }
                | bcode_workflow::WorkflowRunGraphEdit::RemoveEdge { .. } => {}
            }
        }
        let page = Self::current_run_graph_page_in(
            transaction,
            run_id,
            Some(request.expected_revision),
            None,
            None,
            GRAPH_PAGE_LIMIT,
        )?;
        if !page.nodes_complete || !page.edges_complete {
            return Ok(RunGraphCandidateValidation::RequiresIncrementalValidation);
        }
        validate_affected_work(transaction, &request, &page.edges)?;
        let mut graph = Self::candidate_definition_in(transaction, run_id, &page.nodes)?;
        let mut edges = page
            .edges
            .into_iter()
            .map(|record| (record.edge_id, record.edge))
            .collect::<std::collections::BTreeMap<_, _>>();
        for edit in &request.edits {
            apply_candidate_edit(&mut graph, &mut edges, edit)?;
        }
        graph.edges = edges.values().cloned().collect();
        validate_retained_bindings(transaction, &request, &graph, &edges)?;
        graph
            .validate()
            .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
        let updated = crate::database::block_on(
            transaction
                .update("workflow_graph_edit_validations")
                .value(
                    "expected_revision",
                    DatabaseValue::Int64(i64::try_from(request.expected_revision).map_err(
                        |_| WorkflowStoreError::InvalidData("invalid graph revision".to_string()),
                    )?),
                )
                .where_eq("run_id", run_id)
                .where_eq("mutation_id", mutation_id)
                .execute(transaction),
        )?;
        if updated.is_empty() {
            crate::database::block_on(
                transaction
                    .insert("workflow_graph_edit_validations")
                    .value("run_id", run_id)
                    .value("mutation_id", mutation_id)
                    .value(
                        "expected_revision",
                        DatabaseValue::Int64(i64::try_from(request.expected_revision).map_err(
                            |_| {
                                WorkflowStoreError::InvalidData(
                                    "invalid graph revision".to_string(),
                                )
                            },
                        )?),
                    )
                    .execute(transaction),
            )?;
        }
        persist_candidate_delta(transaction, &request, &graph, &edges)?;
        Ok(RunGraphCandidateValidation::Validated)
    }

    fn candidate_definition_in(
        transaction: &dyn Database,
        run_id: &str,
        nodes: &[RunGraphNode],
    ) -> Result<WorkflowDefinition, WorkflowStoreError> {
        let runs = super::database::rows(
            transaction,
            &transaction
                .select("workflow_runs")
                .project_as(
                    switchy_database::query::bounded_text(
                        switchy_database::query::identifier("definition_id"),
                        super::MAX_ID_BYTES as u64,
                        DatabaseValue::Null,
                    ),
                    "definition_id",
                )
                .project(switchy_database::query::identifier("definition_version"))
                .where_eq("run_id", run_id)
                .limit(1),
        )?;
        let run = runs.first().ok_or_else(|| {
            WorkflowStoreError::InvalidData("validation run is missing".to_string())
        })?;
        let definition_id = super::database::text(run, "definition_id")?;
        let version = super::database::unsigned(run, "definition_version")?;
        let definitions = super::database::rows(
            transaction,
            &transaction
                .select("workflow_definitions")
                .project_as(
                    switchy_database::query::bounded_text(
                        switchy_database::query::identifier("definition_json"),
                        super::MAX_INLINE_JSON_BYTES as u64,
                        DatabaseValue::Null,
                    ),
                    "definition_json",
                )
                .where_eq("definition_id", definition_id)
                .where_eq(
                    "version",
                    DatabaseValue::Int64(i64::try_from(version).map_err(|_| {
                        WorkflowStoreError::InvalidData("invalid definition version".to_string())
                    })?),
                )
                .limit(1),
        )?;
        let definition = definitions.first().ok_or_else(|| {
            WorkflowStoreError::InvalidData("validation definition is missing".to_string())
        })?;
        let payload = super::database::text(definition, "definition_json")?;
        let mut graph: WorkflowDefinition = serde_json::from_str(&payload)?;
        graph.nodes.clear();
        graph.entries.clear();
        graph.exits.clear();
        for record in nodes {
            if record.entry {
                graph.entries.push(record.node.id.clone());
            }
            if record.exit {
                graph.exits.push(record.node.id.clone());
            }
            graph
                .nodes
                .insert(record.node.id.clone(), record.node.clone());
        }
        Ok(graph)
    }

    /// Read a staged edit under current execution authority without publishing it.
    ///
    /// The stored envelope and provenance are validated in one snapshot. Same-artifact
    /// verified ownership transfer rebinds provenance atomically without changing intent;
    /// this read never transfers authority or authorizes publication.
    ///
    /// # Errors
    ///
    /// Returns an error for stale ownership, invalid identities, unsupported or damaged candidate
    /// data, inconsistent indexed revision, or persistence failure. Reads never repair state.
    pub fn staged_run_graph_edit(
        &self,
        run_id: &str,
        mutation_id: &str,
        authority: &super::WorkflowExecutionAuthority,
    ) -> Result<Option<bcode_workflow::WorkflowRunGraphEditBatch>, WorkflowStoreError> {
        let transaction = super::database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let request =
            Self::staged_run_graph_edit_in(transaction.as_ref(), run_id, mutation_id, authority)?;
        super::database::commit(transaction)?;
        Ok(request)
    }

    /// Rebind retained candidates within the same-artifact ownership transfer transaction.
    /// Request bytes, revision and mutation identity are never rewritten or restaged.
    pub(super) fn transfer_graph_edit_candidates_in(
        connection: &dyn Database,
        run_id: &str,
        expected: &super::WorkflowExecutionAuthority,
        replacement: &super::WorkflowExecutionAuthority,
    ) -> Result<(), WorkflowStoreError> {
        Self::verify_execution_authority_in(connection, run_id, expected)?;
        let replacement_json = super::bounded_json("graph edit authority", replacement)?;
        let mut after = String::new();
        loop {
            let rows = super::database::rows(
                connection,
                &connection
                    .select("workflow_graph_edit_candidates")
                    .project(switchy_database::query::identifier("mutation_id"))
                    .where_eq("run_id", run_id)
                    .where_gt("mutation_id", after.as_str())
                    .sort("mutation_id", SortDirection::Asc)
                    .limit(64),
            )?;
            if rows.is_empty() {
                break;
            }
            for row in rows {
                let mutation_id = graph_stored_identity(&row, "mutation_id")?;
                // Validates the complete bounded envelope and its exact prior authority.
                Self::staged_run_graph_edit_in(connection, run_id, &mutation_id, expected)?
                    .ok_or_else(|| {
                        WorkflowStoreError::InvalidData(
                            "graph edit candidate disappeared during transfer".into(),
                        )
                    })?;
                let changed = super::database::block_on(
                    connection.exec_update_count(
                        &connection
                            .update("workflow_graph_edit_candidates")
                            .value("authority_json", replacement_json.as_str())
                            .where_eq("run_id", run_id)
                            .where_eq("mutation_id", mutation_id.as_str()),
                    ),
                )?;
                if changed != 1 {
                    return Err(WorkflowStoreError::InvalidData(
                        "graph edit candidate transfer lost its ownership race".into(),
                    ));
                }
                after = mutation_id;
            }
        }
        Ok(())
    }

    fn staged_run_graph_edit_in(
        connection: &dyn Database,
        run_id: &str,
        mutation_id: &str,
        authority: &super::WorkflowExecutionAuthority,
    ) -> Result<Option<bcode_workflow::WorkflowRunGraphEditBatch>, WorkflowStoreError> {
        super::validate_id("run_id", run_id)?;
        super::validate_id("mutation_id", mutation_id)?;
        Self::verify_execution_authority_in(connection, run_id, authority)?;
        let rows = super::database::rows(
            connection,
            &connection
                .select("workflow_graph_edit_candidates")
                .project_as(
                    switchy_database::query::bounded_text(
                        switchy_database::query::identifier("request_json"),
                        super::MAX_INLINE_JSON_BYTES as u64,
                        DatabaseValue::Null,
                    ),
                    "request_json",
                )
                .project_as(
                    switchy_database::query::bounded_text(
                        switchy_database::query::identifier("authority_json"),
                        super::MAX_INLINE_JSON_BYTES as u64,
                        DatabaseValue::Null,
                    ),
                    "authority_json",
                )
                .project(switchy_database::query::identifier("expected_revision"))
                .project(switchy_database::query::identifier("created_at_ms"))
                .where_eq("run_id", run_id)
                .where_eq("mutation_id", mutation_id)
                .limit(1),
        )?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let payload = super::database::text(row, "request_json")?;
        let provenance = super::database::text(row, "authority_json")?;
        let revision = super::database::unsigned(row, "expected_revision")?;
        let _created_at_ms = super::database::unsigned(row, "created_at_ms")?;
        let request: bcode_workflow::WorkflowRunGraphEditBatch = serde_json::from_str(&payload)?;
        request
            .validate()
            .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
        if request.run_id != run_id
            || request.mutation_id != mutation_id
            || serde_json::from_str::<super::WorkflowExecutionAuthority>(&provenance)? != *authority
            || request.expected_revision != revision
        {
            return Err(WorkflowStoreError::InvalidData(
                "inconsistent graph edit candidate".to_string(),
            ));
        }
        Ok(Some(request))
    }

    /// Publish a validated graph while the run has no active activations.
    ///
    /// Connected graphs are limited to direct chains with never-admitted targets and no
    /// controllers. Reconciliation requests require the retained publication operation.
    /// Returns the committed revision, including on duplicate delivery.
    /// The caller must authorize the edit before calling this method.
    ///
    /// # Errors
    ///
    /// Returns an error for stale authority/revision, active work, unsupported topology,
    /// invalid candidates, or persistence failure. Failed publication leaves no partial graph.
    pub fn publish_quiescent_run_graph_edit(
        &mut self,
        run_id: &str,
        mutation_id: &str,
        authority: &super::WorkflowExecutionAuthority,
        created_at_ms: u64,
    ) -> Result<u64, WorkflowStoreError> {
        self.publish_leaf_run_graph_edit(run_id, mutation_id, authority, created_at_ms, false, None)
    }

    /// Publish an edit while retaining unchanged work or cancelling unstarted work.
    /// Newly added entry nodes are admitted atomically using the run input and current limits.
    /// Existing nodes and their historical activations are never implicitly restarted.
    ///
    /// Cancellation permits pending activations, workflow-owned input/approval gates,
    /// and prepared attempts with durable proof that owner handoff has not occurred.
    /// Linked work, handed-off attempts, and historical ambiguous preparations fail closed.
    /// Gate answers and dispatch handoff serialize with publication in store transactions.
    /// Mutation-approval waits still require operation-owner reconciliation.
    ///
    /// Admission bindings remain historical. Settlement consumes the publication's retained
    /// identities. Direct chains with new or explicitly retained active targets and source
    /// bindings are supported; retained targets require unchanged incoming edges.
    /// Newly added parallel joins require newly added members and exact tuple member schemas.
    /// Existing controllers and edits to retained nodes remain unsupported. Direct dependency
    /// joins use existing all-predecessor readiness and consistent-input validation.
    /// The caller must authorize the operation before invoking this method.
    ///
    /// # Errors
    ///
    /// Returns an error for stale authority/revision, missing retention, changed active nodes,
    /// unsupported topology, invalid candidates, or persistence failure.
    pub fn publish_retained_leaf_run_graph_edit(
        &mut self,
        run_id: &str,
        mutation_id: &str,
        authority: &super::WorkflowExecutionAuthority,
        created_at_ms: u64,
    ) -> Result<u64, WorkflowStoreError> {
        self.publish_leaf_run_graph_edit(run_id, mutation_id, authority, created_at_ms, true, None)
    }

    /// Publish a retained-leaf candidate on behalf of an authenticated active execution.
    ///
    /// The application must authorize publication separately from staging. Caller verification
    /// and publication share one transaction, preventing settlement from racing this check.
    ///
    /// # Errors
    /// Returns an error for mismatched or inactive callers, stale authority, invalid candidates,
    /// unsupported reconciliation/topology, or persistence failure.
    pub fn publish_retained_leaf_run_graph_edit_from_execution(
        &mut self,
        mutation_id: &str,
        authority: &super::WorkflowExecutionAuthority,
        caller: &super::WorkflowExecutionSessionLink,
        created_at_ms: u64,
    ) -> Result<u64, WorkflowStoreError> {
        self.publish_leaf_run_graph_edit(
            &caller.run_id,
            mutation_id,
            authority,
            created_at_ms,
            true,
            Some(caller),
        )
    }

    /// Read a bounded page of accepted publication identities for owner reconciliation.
    ///
    /// Advance with the last identity returned; an empty page ends the sweep. Includes
    /// committed and conflicted acceptance so filtering cannot hide unbounded scan work.
    ///
    /// # Errors
    /// Rejects stale authority, invalid page limits or identities, and database failures.
    pub fn accepted_run_graph_publication_page(
        &self,
        run_id: &str,
        authority: &super::WorkflowExecutionAuthority,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<String>, WorkflowStoreError> {
        super::validate_id("run_id", run_id)?;
        if let Some(after) = after {
            super::validate_id("mutation_id", after)?;
        }
        if limit == 0 || limit > 1_000 {
            return Err(WorkflowStoreError::InvalidData(
                "publication page limit must be 1..=1000".into(),
            ));
        }
        let transaction = super::database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        Self::verify_execution_authority_in(transaction.as_ref(), run_id, authority)?;
        let rows = super::database::rows(
            transaction.as_ref(),
            &transaction
                .select("workflow_pending_publications")
                .project_as(
                    switchy_database::query::bounded_text(
                        switchy_database::query::identifier("mutation_id"),
                        super::MAX_ID_BYTES as u64,
                        DatabaseValue::Null,
                    ),
                    "mutation_id",
                )
                .where_eq("run_id", run_id)
                .where_gt("mutation_id", after.unwrap_or(""))
                .sort("mutation_id", SortDirection::Asc)
                .limit(limit as usize),
        )?;
        let identities = rows
            .iter()
            .map(|row| super::database::text(row, "mutation_id"))
            .collect::<Result<Vec<_>, _>>()?;
        super::database::commit(transaction)?;
        Ok(identities)
    }

    /// Finalize an accepted candidate after every exact cancellation target has settled.
    /// Conflicts and still-active targets return their existing status without publication.
    ///
    /// # Errors
    /// Rejects stale authority, absent acceptance, unsupported reconciliation, or damaged state.
    pub fn finalize_pending_run_graph_publication(
        &self,
        run_id: &str,
        mutation_id: &str,
        authority: &super::WorkflowExecutionAuthority,
        created_at_ms: u64,
    ) -> Result<bcode_workflow::WorkflowRunGraphPublicationStatus, WorkflowStoreError> {
        use bcode_workflow::WorkflowRunGraphPublicationStatus as Status;
        let transaction = super::database::begin(
            &self.connection,
            switchy_database::TransactionMode::Immediate,
        )?;
        let status = Self::run_graph_publication_status_in(
            transaction.as_ref(),
            run_id,
            mutation_id,
            authority,
        )?
        .ok_or_else(|| WorkflowStoreError::InvalidData("publication was not accepted".into()))?;
        if !matches!(status, Status::Pending { .. }) {
            return Ok(status);
        }
        let request = Self::staged_run_graph_edit_in(
            transaction.as_ref(),
            run_id,
            mutation_id,
            authority,
        )?
        .ok_or_else(|| WorkflowStoreError::InvalidData("accepted candidate missing".into()))?;
        for disposition in &request.reconciliation {
            if let bcode_workflow::WorkflowRunGraphReconciliation::Cancel { activation_id } =
                disposition
                && !accepted_cancellation_settled(transaction.as_ref(), &request, activation_id)?
                && !publication_local_cancellation(transaction.as_ref(), run_id, activation_id)?
                && !publication_fan_out_local_target(transaction.as_ref(), &request, activation_id)?
            {
                let terminal = !super::database::rows(
                    transaction.as_ref(),
                    &transaction
                        .select("workflow_activations")
                        .columns(&["activation_id"])
                        .where_eq("run_id", run_id)
                        .where_eq("activation_id", activation_id.as_str())
                        .where_in(
                            "status",
                            vec![
                                DatabaseValue::String("completed".to_string()),
                                DatabaseValue::String("failed".to_string()),
                                DatabaseValue::String("skipped".to_string()),
                            ],
                        )
                        .limit(1),
                )?
                .is_empty();
                if terminal {
                    return Err(WorkflowStoreError::InvalidData("publication cancellation conflicts with a terminal activation; revise dispositions".into()));
                }
                return Ok(status);
            }
        }
        let revision = Self::publish_leaf_run_graph_edit_in_transaction(
            run_id,
            mutation_id,
            authority,
            created_at_ms,
            PublicationBinding {
                retain_active: true,
                immediate_parent: None,
            },
            transaction.as_ref(),
        )?;
        super::database::commit(transaction)?;
        Ok(Status::Committed { revision })
    }

    fn publish_leaf_run_graph_edit(
        &self,
        run_id: &str,
        mutation_id: &str,
        authority: &super::WorkflowExecutionAuthority,
        created_at_ms: u64,
        retain_active: bool,
        caller: Option<&super::WorkflowExecutionSessionLink>,
    ) -> Result<u64, WorkflowStoreError> {
        let transaction = super::database::begin(
            &self.connection,
            switchy_database::TransactionMode::Immediate,
        )?;
        if let Some(caller) = caller {
            Self::verify_active_graph_edit_caller_in(transaction.as_ref(), run_id, caller)?;
        }
        let revision = Self::publish_leaf_run_graph_edit_in_transaction(
            run_id,
            mutation_id,
            authority,
            created_at_ms,
            PublicationBinding {
                retain_active,
                immediate_parent: caller.map(|link| link.session_id.as_str()),
            },
            transaction.as_ref(),
        )?;
        super::database::commit(transaction)?;
        Ok(revision)
    }

    fn bind_authenticated_candidate_parent(
        transaction: &dyn Database,
        run_id: &str,
        mutation_id: &str,
        authority: &super::WorkflowExecutionAuthority,
        session_id: &str,
    ) -> Result<(), WorkflowStoreError> {
        let request = Self::staged_run_graph_edit_in(transaction, run_id, mutation_id, authority)?
            .ok_or_else(|| WorkflowStoreError::InvalidData("candidate missing".into()))?;
        let admitted = bcode_workflow::bind_execution_parent(&request, session_id)
            .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
        let page = Self::current_run_graph_page_in(
            transaction,
            run_id,
            Some(request.expected_revision),
            None,
            None,
            GRAPH_PAGE_LIMIT,
        )?;
        if !page.nodes_complete || !page.edges_complete {
            return Err(WorkflowStoreError::InvalidData(
                "execution-parent binding requires bounded complete publication validation".into(),
            ));
        }
        let run = Self::run_summary_in(transaction, run_id)?
            .ok_or_else(|| WorkflowStoreError::InvalidData("run missing".into()))?;
        let definition =
            Self::definition_in(transaction, &run.definition_id, run.definition_version)?
                .ok_or_else(|| WorkflowStoreError::InvalidData("definition missing".into()))?;
        let mut graph: WorkflowDefinition = serde_json::from_str(&definition.definition_json)?;
        graph.nodes.clear();
        graph.entries.clear();
        graph.exits.clear();
        for record in page.nodes {
            if record.entry {
                graph.entries.push(record.node.id.clone());
            }
            if record.exit {
                graph.exits.push(record.node.id.clone());
            }
            graph.nodes.insert(record.node.id.clone(), record.node);
        }
        let mut edges = page
            .edges
            .into_iter()
            .map(|record| (record.edge_id, record.edge))
            .collect();
        for edit in &admitted.edits {
            apply_candidate_edit(&mut graph, &mut edges, edit)?;
        }
        persist_candidate_delta(transaction, &admitted, &graph, &edges)?;
        Ok(())
    }

    fn publish_leaf_run_graph_edit_in_transaction(
        run_id: &str,
        mutation_id: &str,
        authority: &super::WorkflowExecutionAuthority,
        created_at_ms: u64,
        binding: PublicationBinding<'_>,
        transaction: &dyn Database,
    ) -> Result<u64, WorkflowStoreError> {
        let PublicationBinding {
            retain_active,
            immediate_parent,
        } = binding;
        let request = Self::staged_run_graph_edit_in(transaction, run_id, mutation_id, authority)?
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData("graph edit candidate not found".to_string())
            })?;
        let rows = super::database::rows(
            transaction,
            &transaction
                .select("workflow_graph_edit_publications")
                .columns(&["revision"])
                .where_eq("run_id", run_id)
                .where_eq("mutation_id", mutation_id)
                .limit(1),
        )?;
        let published = rows
            .first()
            .map(|row| super::database::unsigned(row, "revision"))
            .transpose()?;
        if let Some(revision) = published {
            return Ok(revision);
        }
        ensure_run_accepts_graph_edits(transaction, run_id)?;
        let active = !super::database::rows(
            transaction,
            &transaction
                .select("workflow_activations")
                .columns(&["activation_id"])
                .where_eq("run_id", run_id)
                .where_not_in(
                    "status",
                    vec![
                        DatabaseValue::String("completed".to_string()),
                        DatabaseValue::String("failed".to_string()),
                        DatabaseValue::String("cancelled".to_string()),
                        DatabaseValue::String("skipped".to_string()),
                    ],
                )
                .limit(1),
        )?
        .is_empty();
        // Validate the candidate and its active dispositions before applying cancellation.
        if Self::validate_run_graph_edit_in_transaction(
            run_id,
            mutation_id,
            authority,
            transaction,
        )? != RunGraphCandidateValidation::Validated
        {
            return Err(WorkflowStoreError::InvalidData(
                "publication requires incremental graph validation".to_string(),
            ));
        }
        let retentions = if retain_active {
            cancel_unstarted_leaf_activations(transaction, &request, created_at_ms)?;
            validate_leaf_retention(transaction, &request)?
        } else if active || !request.reconciliation.is_empty() {
            return Err(WorkflowStoreError::InvalidData(
                "publication requires execution reconciliation".to_string(),
            ));
        } else {
            Vec::new()
        };
        Self::validate_published_capabilities(transaction, &request)?;
        Self::validate_published_call_targets(transaction, &request)?;
        Self::validate_leaf_connected_graph(transaction, &request)?;
        if let Some(parent) = immediate_parent {
            Self::bind_authenticated_candidate_parent(
                transaction,
                run_id,
                mutation_id,
                authority,
                parent,
            )?;
        }
        let revision =
            persist_graph_publication(transaction, &request, &retentions, created_at_ms)?;
        Ok(revision)
    }

    fn validate_leaf_connected_graph(
        transaction: &dyn Database,
        request: &bcode_workflow::WorkflowRunGraphEditBatch,
    ) -> Result<(), WorkflowStoreError> {
        let page = Self::current_run_graph_page_in(
            transaction,
            &request.run_id,
            Some(request.expected_revision),
            None,
            None,
            GRAPH_PAGE_LIMIT,
        )?;
        let replaced = request
            .edits
            .iter()
            .filter_map(|edit| match edit {
                bcode_workflow::WorkflowRunGraphEdit::AddEdge { edge_id, .. }
                | bcode_workflow::WorkflowRunGraphEdit::ReplaceEdge { edge_id, .. }
                | bcode_workflow::WorkflowRunGraphEdit::RemoveEdge { edge_id } => Some(*edge_id),
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        let connected = page
            .edges
            .iter()
            .any(|edge| !replaced.contains(&edge.edge_id))
            || request.edits.iter().any(|edit| {
                matches!(
                    edit,
                    bcode_workflow::WorkflowRunGraphEdit::AddEdge { .. }
                        | bcode_workflow::WorkflowRunGraphEdit::ReplaceEdge { .. }
                )
            });
        if connected {
            Self::validate_connected_publication_in(transaction, request)?;
        }
        Ok(())
    }

    fn validate_published_capabilities(
        transaction: &dyn Database,
        request: &bcode_workflow::WorkflowRunGraphEditBatch,
    ) -> Result<(), WorkflowStoreError> {
        let run = Self::run_summary_in(transaction, &request.run_id)?
            .ok_or_else(|| WorkflowStoreError::InvalidData("publication run missing".into()))?;
        for edit in &request.edits {
            let (bcode_workflow::WorkflowRunGraphEdit::AddNode { node, .. }
            | bcode_workflow::WorkflowRunGraphEdit::ReplaceNode { node, .. }) = edit
            else {
                continue;
            };
            let capability = match node.kind {
                bcode_workflow::NodeKind::Agent => {
                    let config: bcode_workflow::WorkflowPromptConfiguration =
                        serde_json::from_value(node.configuration.clone())?;
                    config.tool_capability
                }
                bcode_workflow::NodeKind::PluginBlock => {
                    let block: bcode_workflow::WorkflowBlockDefinition =
                        serde_json::from_value(node.configuration.clone())?;
                    block.authorization.capability
                }
                _ => bcode_workflow::WorkflowToolCapability::Disabled,
            };
            if capability > run.authorization_ceiling {
                return Err(WorkflowStoreError::InvalidData(
                    "published node requirements exceed run authorization ceiling".into(),
                ));
            }
        }
        Ok(())
    }

    fn validate_published_call_targets(
        transaction: &dyn Database,
        request: &bcode_workflow::WorkflowRunGraphEditBatch,
    ) -> Result<(), WorkflowStoreError> {
        for edit in &request.edits {
            let (bcode_workflow::WorkflowRunGraphEdit::AddNode { node, .. }
            | bcode_workflow::WorkflowRunGraphEdit::ReplaceNode { node, .. }) = edit
            else {
                continue;
            };
            if node.kind != bcode_workflow::NodeKind::WorkflowCall {
                continue;
            }
            let call: bcode_workflow::WorkflowCallConfiguration =
                serde_json::from_value(node.configuration.clone())?;
            call.validate()
                .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
            let local_identity;
            let identity = if let bcode_workflow::WorkflowCallTarget::PackageLocal { reference } =
                &call.target
            {
                let binding =
                    Self::package_binding_in(transaction, &request.run_id)?.ok_or_else(|| {
                        WorkflowStoreError::InvalidData(
                            "local graph call requires bound run".into(),
                        )
                    })?;
                let lock = Self::package_lock_on(transaction, &binding.0, &binding.1)?.ok_or_else(
                    || WorkflowStoreError::InvalidData("parent package unavailable".into()),
                )?;
                let member = lock
                    .members
                    .iter()
                    .find(|member| member.member_id == reference.member_id)
                    .ok_or_else(|| {
                        WorkflowStoreError::InvalidData("member absent from bound lock".into())
                    })?;
                local_identity = member.definition_identity.clone();
                &local_identity
            } else {
                call.target
                    .definition_identity()
                    .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?
            };
            let target = Self::definition_in(
                transaction,
                &identity.definition_id,
                identity.definition_version,
            )?
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData("published call target is unavailable".to_owned())
            })?;
            let definition: bcode_workflow::WorkflowDefinition =
                serde_json::from_str(&target.definition_json)?;
            if let bcode_workflow::WorkflowCallTarget::PackageLocal { reference } = &call.target
                && (
                    reference.workflow_id.as_str(),
                    &reference.input,
                    &reference.output,
                ) != (
                    identity.kind.as_str(),
                    &definition.input,
                    &definition.output,
                )
            {
                return Err(WorkflowStoreError::InvalidData(
                    "local call declared interface differs from bound definition".into(),
                ));
            }
            if node.input != definition.input || node.output != definition.output {
                return Err(WorkflowStoreError::InvalidData(
                    "published call target interface mismatch".to_owned(),
                ));
            }
        }
        Ok(())
    }

    // Connected publication admits direct dependencies, including one output feeding
    // multiple new targets. Retained targets require unchanged incoming edges.
    // Historical inputs and executables remain immutable; admitted parallel joins
    // retain their executable and incoming-edge identities.
    #[allow(clippy::too_many_lines)]
    #[cfg(test)]
    pub(super) fn validate_connected_publication(
        &self,
        request: &bcode_workflow::WorkflowRunGraphEditBatch,
    ) -> Result<(), WorkflowStoreError> {
        let transaction = super::database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        Self::validate_connected_publication_in(transaction.as_ref(), request)?;
        super::database::commit(transaction)
    }

    #[allow(clippy::too_many_lines)]
    fn validate_connected_publication_in(
        transaction: &dyn Database,
        request: &bcode_workflow::WorkflowRunGraphEditBatch,
    ) -> Result<(), WorkflowStoreError> {
        let page = Self::current_run_graph_page_in(
            transaction,
            &request.run_id,
            Some(request.expected_revision),
            None,
            None,
            GRAPH_PAGE_LIMIT,
        )?;
        if !page.nodes_complete || !page.edges_complete {
            return Err(WorkflowStoreError::InvalidData(
                "publication requires incremental graph validation".to_string(),
            ));
        }
        let original_nodes = page.nodes.clone();
        let original_edges = page.edges.clone();
        let mut nodes = page
            .nodes
            .into_iter()
            .map(|record| (record.node.id.clone(), (record.node, record.entry)))
            .collect::<std::collections::BTreeMap<_, _>>();
        let mut edges = page
            .edges
            .into_iter()
            .map(|record| (record.edge_id, record.edge))
            .collect::<std::collections::BTreeMap<_, _>>();
        for edit in &request.edits {
            use bcode_workflow::WorkflowRunGraphEdit as Edit;
            match edit {
                Edit::AddNode { node, entry, .. } | Edit::ReplaceNode { node, entry, .. } => {
                    nodes.insert(node.id.clone(), (node.clone(), *entry));
                }
                Edit::RemoveNode { node_id } => {
                    nodes.remove(node_id);
                }
                Edit::AddEdge { edge_id, edge } | Edit::ReplaceEdge { edge_id, edge } => {
                    edges.insert(*edge_id, edge.clone());
                }
                Edit::RemoveEdge { edge_id } => {
                    edges.remove(edge_id);
                }
            }
        }
        let invalid = || {
            WorkflowStoreError::InvalidData(
            "connected publication requires direct schema-compatible dependencies and reconciled targets".to_string(),
        )
        };
        for previous in original_nodes
            .iter()
            .filter(|record| !editable_connected_kind(record.node.kind))
        {
            if nodes
                .get(&previous.node.id)
                .is_none_or(|(node, entry)| node != &previous.node || *entry != previous.entry)
            {
                return Err(invalid());
            }
            for edge in original_edges.iter().filter(|edge| {
                edge.edge.from == previous.node.id || edge.edge.to == previous.node.id
            }) {
                if edges.get(&edge.edge_id) != Some(&edge.edge) {
                    return Err(invalid());
                }
            }
            if edges.iter().any(|(id, edge)| {
                (edge.from == previous.node.id || edge.to == previous.node.id)
                    && !original_edges
                        .iter()
                        .any(|old| old.edge_id == *id && old.edge == *edge)
            }) {
                return Err(invalid());
            }
        }
        for previous in original_edges
            .iter()
            .filter(|record| record.edge.kind != bcode_workflow::EdgeKind::Direct)
        {
            if edges.get(&previous.edge_id) != Some(&previous.edge) {
                return Err(invalid());
            }
        }
        for (node, entry) in nodes.values() {
            if !editable_connected_kind(node.kind) {
                let previous = Self::current_run_graph_node_in_snapshot(
                    transaction,
                    &request.run_id,
                    &node.id,
                )?
                .ok_or_else(invalid)?;
                if previous.node != *node || previous.entry != *entry {
                    return Err(invalid());
                }
            }
        }
        let added = request
            .edits
            .iter()
            .filter_map(|edit| match edit {
                bcode_workflow::WorkflowRunGraphEdit::AddNode { node, .. } => {
                    Some(node.id.as_str())
                }
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        for (node, entry) in nodes
            .values()
            .filter(|(node, _)| node.kind == bcode_workflow::NodeKind::Parallel)
        {
            let members = bcode_workflow::parallel_join_member_ids(node)
                .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
            let member_schemas = members
                .iter()
                .map(|member| {
                    nodes
                        .get(*member)
                        .map(|(node, _)| node.output.schema.clone())
                })
                .collect::<Option<Vec<_>>>()
                .ok_or_else(invalid)?;
            if node
                .configuration
                .get("failure_policy")
                .and_then(serde_json::Value::as_str)
                == Some("collect_outcomes")
            {
                let schemas = members
                    .iter()
                    .map(|id| {
                        Ok((
                            id.to_string(),
                            bcode_workflow::workflow_member_outcome_schema(&nodes[*id].0.output)
                                .map_err(|error| {
                                    WorkflowStoreError::InvalidData(error.to_string())
                                })?,
                        ))
                    })
                    .collect::<Result<std::collections::BTreeMap<_, _>, WorkflowStoreError>>()?;
                let expected =
                    bcode_workflow::named_result_schema(node.input.type_name.clone(), &schemas)
                        .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
                if node.input != expected
                    || edges
                        .values()
                        .any(|edge| edge.to == node.id && edge.transform.is_some())
                {
                    return Err(invalid());
                }
            } else {
                let tuple = node
                    .input
                    .schema
                    .get("prefixItems")
                    .and_then(serde_json::Value::as_array)
                    .ok_or_else(invalid)?;
                if tuple.len() != member_schemas.len()
                    || !member_schemas.iter().zip(tuple).all(|(member, nested)| {
                        bcode_workflow::parallel_member_schema_matches(member, nested)
                    })
                {
                    return Err(invalid());
                }
            }
            let admitted = !super::database::rows(
                transaction,
                &transaction
                    .select("workflow_activations")
                    .columns(&["activation_id"])
                    .where_eq("run_id", request.run_id.as_str())
                    .where_eq("node_id", node.id.as_str())
                    .limit(1),
            )?
            .is_empty();
            // Planned controllers may change while their members execute. Once admitted,
            // the controller's executable and tuple inputs belong to that activation.
            if !added.contains(node.id.as_str()) && admitted {
                let previous = Self::current_run_graph_node_in_snapshot(
                    transaction,
                    &request.run_id,
                    &node.id,
                )?
                .ok_or_else(invalid)?;
                if previous.node != *node || previous.entry != *entry {
                    return Err(invalid());
                }
                for (edge_id, edge) in edges.iter().filter(|(_, edge)| edge.to == node.id) {
                    if Self::current_run_graph_edge_in_snapshot(
                        transaction,
                        &request.run_id,
                        *edge_id,
                        &mut BTreeSet::new(),
                    )?
                    .is_none_or(|old| old.edge != *edge)
                    {
                        return Err(invalid());
                    }
                }
            }
            if *entry
                || node.input != node.output
                || edges.values().filter(|edge| edge.to == node.id).count() != members.len()
                || members.iter().any(|member| {
                    !edges
                        .values()
                        .any(|edge| edge.from == *member && edge.to == node.id)
                })
            {
                return Err(invalid());
            }
        }
        for (edge_id, edge) in &edges {
            let (source, _) = nodes.get(&edge.from).ok_or_else(invalid)?;
            let (target, entry) = nodes.get(&edge.to).ok_or_else(invalid)?;
            if edge.kind != bcode_workflow::EdgeKind::Direct {
                let previous = Self::current_run_graph_edge_in_snapshot(
                    transaction,
                    &request.run_id,
                    *edge_id,
                    &mut BTreeSet::new(),
                )?
                .ok_or_else(invalid)?;
                if previous.edge != *edge {
                    return Err(invalid());
                }
                for node in [source, target] {
                    let previous = Self::current_run_graph_node_in_snapshot(
                        transaction,
                        &request.run_id,
                        &node.id,
                    )?
                    .ok_or_else(invalid)?;
                    if previous.node != *node {
                        return Err(invalid());
                    }
                }
                continue;
            }
            // Typed direct transforms use the same canonical validation and runtime
            // evaluation as authored edges. They do not exempt an admitted target
            // from retention or permit changes to controller incident edges.
            let output = edge
                .transform
                .as_ref()
                .map_or(&source.output, |value| &value.output);
            if (target.kind != bcode_workflow::NodeKind::Parallel && *output != target.input)
                || *entry
            {
                return Err(invalid());
            }
            // An unchanged historical dependency does not admit or restart work.
            // Initialization may already have settled before an implementation
            // activation revises unrelated planned work. Retention is required for
            // affected targets, not every completed target in the existing graph.
            if original_edges
                .iter()
                .any(|previous| previous.edge_id == *edge_id && previous.edge == *edge)
                && [source, target].iter().all(|node| {
                    original_nodes.iter().any(|previous| {
                        previous.node == **node
                            && nodes
                                .get(&node.id)
                                .is_some_and(|(_, entry)| *entry == previous.entry)
                    })
                })
            {
                continue;
            }
            let admitted = !super::database::rows(
                transaction,
                &transaction
                    .select("workflow_activations")
                    .columns(&["activation_id"])
                    .where_eq("run_id", request.run_id.as_str())
                    .where_eq("node_id", edge.to.as_str())
                    .limit(1),
            )?
            .is_empty();
            if admitted {
                Self::validate_retained_chain_target(transaction, request, *edge_id, edge)?;
            }
        }
        Ok(())
    }

    fn validate_retained_chain_target(
        transaction: &dyn Database,
        request: &bcode_workflow::WorkflowRunGraphEditBatch,
        edge_id: u64,
        edge: &EdgeDefinition,
    ) -> Result<(), WorkflowStoreError> {
        let invalid = || {
            WorkflowStoreError::InvalidData(
            "admitted chain target requires unchanged incoming edge and explicit active retention".to_string(),
        )
        };
        let previous = Self::current_run_graph_edge_in_snapshot(
            transaction,
            &request.run_id,
            edge_id,
            &mut BTreeSet::new(),
        )?
        .ok_or_else(invalid)?;
        if previous.edge != *edge {
            if previous.edge.to == edge.to
                && Self::target_is_only_from_prior_generations(
                    transaction,
                    request,
                    &previous.edge,
                )?
            {
                return Ok(());
            }
            return Err(invalid());
        }
        // Retention validation already proves that active executable revisions are
        // unchanged. Only one active historical admission may identify this target;
        // completed/cancelled work must never be implicitly restarted or reused.
        let rows = super::database::rows(
            transaction,
            &transaction
                .select("workflow_activations")
                .project_as(
                    switchy_database::query::bounded_text(
                        switchy_database::query::identifier("activation_id"),
                        super::MAX_ID_BYTES as u64,
                        DatabaseValue::Null,
                    ),
                    "activation_id",
                )
                .project_as(
                    switchy_database::query::bounded_text(
                        switchy_database::query::identifier("status"),
                        super::MAX_ID_BYTES as u64,
                        DatabaseValue::Null,
                    ),
                    "status",
                )
                .where_eq("run_id", request.run_id.as_str())
                .where_eq("node_id", edge.to.as_str())
                .limit(2),
        )?;
        let row = rows.first().ok_or_else(invalid)?;
        let activation_id = super::database::text(row, "activation_id")?;
        let status = super::database::text(row, "status")?;
        if rows.len() != 1
            || !matches!(
                status.as_str(),
                "pending"
                    | "running"
                    | "waiting_input"
                    | "waiting_approval"
                    | "waiting_mutation_approval"
            )
            || !request
                .reconciliation
                .iter()
                .any(|disposition| match disposition {
                    bcode_workflow::WorkflowRunGraphReconciliation::Retain {
                        activation_id: retained,
                    }
                    | bcode_workflow::WorkflowRunGraphReconciliation::RetainWithBindings {
                        activation_id: retained,
                        ..
                    } => retained == &activation_id,
                    bcode_workflow::WorkflowRunGraphReconciliation::Cancel { .. } => false,
                })
        {
            return Err(invalid());
        }
        Ok(())
    }

    // Reconnecting an evaluator after a repeat must not mistake a settled older
    // generation for the future target. The replaced edge's explicitly retained
    // active source proves the generation; node identity alone grants no reuse.
    fn target_is_only_from_prior_generations(
        transaction: &dyn Database,
        request: &bcode_workflow::WorkflowRunGraphEditBatch,
        previous: &EdgeDefinition,
    ) -> Result<bool, WorkflowStoreError> {
        let mut generation = None;
        for disposition in &request.reconciliation {
            let bcode_workflow::WorkflowRunGraphReconciliation::RetainWithBindings {
                activation_id,
                ..
            } = disposition
            else {
                continue;
            };
            let rows = super::database::rows(
                transaction,
                &transaction
                    .select("workflow_activations")
                    .columns(&["dependency_generation"])
                    .where_eq("run_id", request.run_id.as_str())
                    .where_eq("node_id", previous.from.as_str())
                    .where_eq("activation_id", activation_id.as_str())
                    .where_in(
                        "status",
                        vec![
                            DatabaseValue::String("pending".to_string()),
                            DatabaseValue::String("running".to_string()),
                            DatabaseValue::String("waiting_input".to_string()),
                            DatabaseValue::String("waiting_approval".to_string()),
                            DatabaseValue::String("waiting_mutation_approval".to_string()),
                        ],
                    )
                    .limit(1),
            )?;
            let source = rows
                .first()
                .map(|row| super::database::unsigned(row, "dependency_generation"))
                .transpose()?;
            if let Some(source) = source
                && generation.replace(source).is_some()
            {
                return Ok(false);
            }
        }
        let Some(generation) = generation else {
            return Ok(false);
        };
        let terminal = vec![
            DatabaseValue::String("completed".to_string()),
            DatabaseValue::String("failed".to_string()),
            DatabaseValue::String("cancelled".to_string()),
            DatabaseValue::String("skipped".to_string()),
        ];
        let rows = super::database::rows(
            transaction,
            &transaction
                .select("workflow_activations")
                .columns(&["activation_id"])
                .where_eq("run_id", request.run_id.as_str())
                .where_eq("node_id", previous.to.as_str())
                .where_or(switchy_database::boxed![
                    switchy_database::query::where_gte(
                        "dependency_generation",
                        DatabaseValue::Int64(i64::try_from(generation).map_err(|_| {
                            WorkflowStoreError::InvalidData(
                                "invalid dependency generation".to_string(),
                            )
                        })?)
                    ),
                    switchy_database::query::where_not_in("status", terminal)
                ])
                .limit(1),
        )?;
        let unsafe_target = !rows.is_empty();
        Ok(!unsafe_target)
    }

    /// Durably stage a live graph edit without publishing executable topology.
    ///
    /// Returns `true` for a new candidate and `false` for identical duplicate delivery.
    /// Candidates preserve their admitting authority and do not authorize execution or bypass
    /// structural validation. Publication and reconciliation are separate required operations.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid or oversized requests, stale ownership or revision,
    /// conflicting duplicate identity, terminal/cancelling runs, or persistence failure.
    pub fn stage_run_graph_edit(
        &mut self,
        request: &bcode_workflow::WorkflowRunGraphEditBatch,
        authority: &super::WorkflowExecutionAuthority,
        created_at_ms: u64,
    ) -> Result<bool, WorkflowStoreError> {
        self.stage_run_graph_edit_for_caller(request, authority, created_at_ms, None)
    }

    /// Stage a candidate only while its exact calling execution session remains active.
    ///
    /// The application must authenticate the caller and authorize the edit before invoking this
    /// operation. A session link alone does not grant permission. Link and active-attempt checks
    /// share the candidate transaction so settlement cannot race with admission.
    ///
    /// # Errors
    /// Returns an error for a mismatched session link, inactive attempt, stale authority,
    /// invalid candidate, conflicting duplicate/revision, or persistence failure.
    pub fn stage_run_graph_edit_from_execution(
        &mut self,
        request: &bcode_workflow::WorkflowRunGraphEditBatch,
        authority: &super::WorkflowExecutionAuthority,
        caller: &super::WorkflowExecutionSessionLink,
        created_at_ms: u64,
    ) -> Result<bool, WorkflowStoreError> {
        self.stage_run_graph_edit_for_caller(request, authority, created_at_ms, Some(caller))
    }

    /// Read an active execution's graph and verify its durable link in one snapshot.
    ///
    /// # Errors
    /// Rejects stale callers, foreign authority, unqualified continuation cursors,
    /// invalid bounds, revision conflicts, and unavailable or damaged graph state.
    pub fn execution_context_graph_page(
        &self,
        caller: &super::WorkflowExecutionSessionLink,
        authority: &super::WorkflowExecutionAuthority,
        request: &bcode_workflow::WorkflowExecutionContextRequest,
    ) -> Result<
        (
            RunGraphPage,
            Option<bcode_workflow::WorkflowOutputInspection>,
            Vec<super::WorkflowOutputSummary>,
        ),
        WorkflowStoreError,
    > {
        if !(1..=100).contains(&request.limit) {
            return Err(WorkflowStoreError::InvalidData(
                "execution context limit must be between 1 and 100".to_owned(),
            ));
        }
        if request.expected_revision.is_none()
            && (request.after_node_id.is_some() || request.after_edge_id.is_some())
        {
            return Err(WorkflowStoreError::InvalidData(
                "execution context continuation requires a revision".to_owned(),
            ));
        }
        let transaction = super::database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        Self::verify_execution_authority_in(transaction.as_ref(), &caller.run_id, authority)?;
        Self::verify_active_graph_edit_caller_in(transaction.as_ref(), &caller.run_id, caller)?;
        if request.source_local && request.after_node_id.is_some() {
            return Err(WorkflowStoreError::InvalidData(
                "source-local context does not accept a node cursor".to_owned(),
            ));
        }
        let page = if request.source_local {
            let revision =
                graph_revision(transaction.as_ref(), &caller.run_id)?.ok_or_else(|| {
                    WorkflowStoreError::InvalidData("workflow run graph not found".to_owned())
                })?;
            if request
                .expected_revision
                .is_some_and(|expected| expected != revision)
            {
                return Err(WorkflowStoreError::InvalidData(
                    "workflow graph revision conflict".to_owned(),
                ));
            }
            let node = Self::current_run_graph_node_in_snapshot(
                transaction.as_ref(),
                &caller.run_id,
                &caller.node_id,
            )?
            .ok_or_else(|| WorkflowStoreError::InvalidData("source node unavailable".to_owned()))?;
            let edges = Self::current_run_graph_edges_in_snapshot(
                transaction.as_ref(),
                &caller.run_id,
                revision,
                request.after_edge_id,
                request.limit,
                Some(EdgeEndpoint::Source(&caller.node_id)),
            )?;
            let edges_complete = edges.len() < request.limit
                || Self::current_run_graph_edges_in_snapshot(
                    transaction.as_ref(),
                    &caller.run_id,
                    revision,
                    edges.last().map(|edge| edge.edge_id),
                    1,
                    Some(EdgeEndpoint::Source(&caller.node_id)),
                )?
                .is_empty();
            let next_edge_id = Self::next_run_graph_edge_id(transaction.as_ref(), &caller.run_id)?;
            RunGraphPage {
                revision,
                next_edge_id,
                nodes: vec![node],
                edges,
                nodes_complete: true,
                edges_complete,
            }
        } else {
            Self::current_run_graph_page_in(
                transaction.as_ref(),
                &caller.run_id,
                request.expected_revision,
                request.after_node_id.as_deref(),
                request.after_edge_id,
                request.limit,
            )?
        };
        let output = request
            .output_id
            .as_deref()
            .map(|id| Self::inspect_exact_output_in(transaction.as_ref(), &caller.run_id, id))
            .transpose()?;
        let outputs = Self::output_summaries_after_in(
            transaction.as_ref(),
            &caller.run_id,
            request.after_output_id.as_deref(),
            request.limit,
        )?;
        super::database::commit(transaction)?;
        Ok((page, output, outputs))
    }

    // Both callers hold a transaction on this connection through their eventual commit.
    fn verify_active_graph_edit_caller_in(
        connection: &dyn Database,
        run_id: &str,
        caller: &super::WorkflowExecutionSessionLink,
    ) -> Result<(), WorkflowStoreError> {
        super::validate_execution_session_link(caller)?;
        let stored = Self::execution_session_link_in(
            connection,
            &caller.run_id,
            &caller.node_id,
            &caller.activation_id,
            caller.attempt,
        )?;
        if caller.run_id != run_id || stored.as_ref() != Some(caller) {
            return Err(WorkflowStoreError::InvalidData(
                "run edit caller link mismatch".to_string(),
            ));
        }
        let activations = super::database::rows(
            connection,
            &connection
                .select("workflow_activations")
                .columns(&["activation_id"])
                .where_eq("run_id", caller.run_id.as_str())
                .where_eq("node_id", caller.node_id.as_str())
                .where_eq("activation_id", caller.activation_id.as_str())
                .where_eq("status", "running")
                .limit(1),
        )?;
        let attempts = super::database::rows(
            connection,
            &connection
                .select("workflow_attempts")
                .columns(&["attempt"])
                .where_eq("run_id", caller.run_id.as_str())
                .where_eq("node_id", caller.node_id.as_str())
                .where_eq("activation_id", caller.activation_id.as_str())
                .where_eq("attempt", DatabaseValue::Int64(i64::from(caller.attempt)))
                .where_or(switchy_database::boxed![
                    switchy_database::query::where_eq("status", "prepared"),
                    switchy_database::query::where_eq("status", "admitted")
                ])
                .limit(1),
        )?;
        if activations.is_empty() || attempts.is_empty() {
            return Err(WorkflowStoreError::InvalidData(
                "run edit caller is no longer active".to_string(),
            ));
        }
        Ok(())
    }

    fn stage_run_graph_edit_for_caller(
        &self,
        request: &bcode_workflow::WorkflowRunGraphEditBatch,
        authority: &super::WorkflowExecutionAuthority,
        created_at_ms: u64,
        caller: Option<&super::WorkflowExecutionSessionLink>,
    ) -> Result<bool, WorkflowStoreError> {
        bcode_workflow::reject_execution_parent_claims(request)
            .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
        request
            .validate()
            .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
        super::validate_id("run_id", &request.run_id)?;
        super::validate_id("mutation_id", &request.mutation_id)?;
        let payload = super::bounded_json("graph edit candidate", request)?;
        let authority_json = super::bounded_json("graph edit authority", authority)?;
        let transaction = super::database::begin(
            &self.connection,
            switchy_database::TransactionMode::Immediate,
        )?;
        Self::verify_execution_authority_in(transaction.as_ref(), &request.run_id, authority)?;
        if let Some(caller) = caller {
            Self::verify_active_graph_edit_caller_in(
                transaction.as_ref(),
                &request.run_id,
                caller,
            )?;
        }
        if let Some(existing) = Self::staged_run_graph_edit_in(
            transaction.as_ref(),
            &request.run_id,
            &request.mutation_id,
            authority,
        )? {
            if existing != *request {
                return Err(WorkflowStoreError::InvalidData(
                    "conflicting graph edit mutation identity".to_string(),
                ));
            }
            super::database::commit(transaction)?;
            return Ok(false);
        }
        if graph_revision(transaction.as_ref(), &request.run_id)? != Some(request.expected_revision)
        {
            return Err(WorkflowStoreError::InvalidData(
                "workflow graph revision conflict".to_string(),
            ));
        }
        ensure_run_accepts_graph_edits(transaction.as_ref(), &request.run_id)?;
        crate::database::block_on(
            transaction
                .insert("workflow_graph_edit_candidates")
                .value("run_id", request.run_id.as_str())
                .value("mutation_id", request.mutation_id.as_str())
                .value(
                    "expected_revision",
                    DatabaseValue::Int64(i64::try_from(request.expected_revision).map_err(
                        |_| WorkflowStoreError::InvalidData("invalid graph revision".to_string()),
                    )?),
                )
                .value("request_json", payload)
                .value("authority_json", authority_json)
                .value(
                    "created_at_ms",
                    DatabaseValue::Int64(i64::try_from(created_at_ms).map_err(|_| {
                        WorkflowStoreError::InvalidData("invalid creation timestamp".to_string())
                    })?),
                )
                .execute(transaction.as_ref()),
        )?;
        super::append_event_in(transaction.as_ref(), &request.run_id, "graph_edit_staged",
            &serde_json::json!({"mutation_id": request.mutation_id, "expected_revision": request.expected_revision}).to_string(), created_at_ms)?;
        super::database::commit(transaction)?;
        Ok(true)
    }
}

fn ensure_run_accepts_graph_edits(
    connection: &dyn Database,
    run_id: &str,
) -> Result<(), WorkflowStoreError> {
    let rows = super::database::rows(
        connection,
        &connection
            .select("workflow_runs")
            .project_as(
                switchy_database::query::bounded_text(
                    switchy_database::query::identifier("status"),
                    super::MAX_ID_BYTES as u64,
                    DatabaseValue::Null,
                ),
                "status",
            )
            .project(switchy_database::query::identifier(
                "cancellation_requested_at_ms",
            ))
            .where_eq("run_id", run_id)
            .limit(1),
    )?;
    let accepts = rows
        .first()
        .map(|row| {
            Ok::<_, WorkflowStoreError>(
                matches!(
                    super::database::text(row, "status")?.as_str(),
                    "running" | "paused"
                ) && matches!(
                    row.get("cancellation_requested_at_ms"),
                    Some(DatabaseValue::Null)
                ),
            )
        })
        .transpose()?
        .unwrap_or(false);
    if !accepts {
        return Err(WorkflowStoreError::InvalidData(
            "run does not accept graph edits".to_string(),
        ));
    }
    Ok(())
}

fn validate_affected_work(
    connection: &dyn Database,
    request: &bcode_workflow::WorkflowRunGraphEditBatch,
    current_edges: &[RunGraphEdge],
) -> Result<(), WorkflowStoreError> {
    use bcode_workflow::{
        WorkflowRunGraphEdit as Edit, WorkflowRunGraphReconciliation as Reconciliation,
    };
    let mut affected = BTreeSet::new();
    for edit in &request.edits {
        match edit {
            Edit::AddNode { node, .. } | Edit::ReplaceNode { node, .. } => {
                affected.insert(node.id.as_str());
            }
            Edit::RemoveNode { node_id } => {
                affected.insert(node_id.as_str());
            }
            Edit::AddEdge { edge, .. } => {
                affected.insert(edge.to.as_str());
            }
            Edit::ReplaceEdge { edge_id, edge } => {
                affected.insert(edge.to.as_str());
                if let Some(old) = current_edges.iter().find(|old| old.edge_id == *edge_id) {
                    affected.insert(old.edge.to.as_str());
                }
            }
            Edit::RemoveEdge { edge_id } => {
                if let Some(old) = current_edges.iter().find(|old| old.edge_id == *edge_id) {
                    affected.insert(old.edge.to.as_str());
                }
            }
        }
    }
    expand_affected_dependencies(&mut affected, request, current_edges);
    let dispositions: BTreeSet<_> = request
        .reconciliation
        .iter()
        .map(|item| {
            let (Reconciliation::Retain { activation_id }
            | Reconciliation::RetainWithBindings { activation_id, .. }
            | Reconciliation::Cancel { activation_id }) = item;
            activation_id.as_str()
        })
        .collect();
    for node_id in affected {
        let mut remaining = bcode_workflow::MAX_WORKFLOW_RUN_GRAPH_EDITS + 1;
        for (table, node_column, identity_column) in [
            ("workflow_activations", "node_id", "activation_id"),
            (
                "workflow_fan_out_members",
                "controller_node_id",
                "member_activation_id",
            ),
        ] {
            if remaining == 0 {
                break;
            }
            let rows = super::database::rows(
                connection,
                &connection
                    .select(table)
                    .project_as(
                        switchy_database::query::bounded_text(
                            switchy_database::query::identifier(identity_column),
                            super::MAX_ID_BYTES as u64,
                            DatabaseValue::Null,
                        ),
                        "identity",
                    )
                    .where_eq("run_id", request.run_id.as_str())
                    .where_eq(node_column, node_id)
                    .where_eq("status", "running")
                    .limit(remaining),
            )?;
            remaining -= rows.len();
            for row in rows {
                let identity = super::database::text(&row, "identity")?;
                if !dispositions.contains(identity.as_str()) {
                    return Err(WorkflowStoreError::InvalidData(
                        "affected running activation requires explicit graph reconciliation"
                            .to_string(),
                    ));
                }
            }
        }
    }
    Ok(())
}

fn expand_affected_dependencies<'a>(
    affected: &mut BTreeSet<&'a str>,
    request: &'a bcode_workflow::WorkflowRunGraphEditBatch,
    current_edges: &'a [RunGraphEdge],
) {
    use bcode_workflow::WorkflowRunGraphEdit as Edit;
    let mut dependencies = std::collections::BTreeMap::<&str, BTreeSet<&str>>::new();
    for edge in
        current_edges
            .iter()
            .map(|record| &record.edge)
            .chain(request.edits.iter().filter_map(|edit| match edit {
                Edit::AddEdge { edge, .. } | Edit::ReplaceEdge { edge, .. } => Some(edge),
                _ => None,
            }))
    {
        dependencies
            .entry(edge.from.as_str())
            .or_default()
            .insert(edge.to.as_str());
    }
    let mut pending: Vec<_> = affected.iter().copied().collect();
    while let Some(node) = pending.pop() {
        if let Some(targets) = dependencies.get(node) {
            for &target in targets {
                if affected.insert(target) {
                    pending.push(target);
                }
            }
        }
    }
}

pub fn retained_edge_activation(
    connection: &dyn Database,
    run_id: &str,
    graph_revision: u64,
    edge: &RunGraphEdge,
) -> Result<Option<String>, WorkflowStoreError> {
    let integer = |value| {
        i64::try_from(value).map(DatabaseValue::Int64).map_err(|_| {
            WorkflowStoreError::InvalidData("invalid graph storage integer".to_string())
        })
    };
    let rows = super::database::rows(
        connection,
        &connection
            .select("workflow_retained_edge_bindings")
            .project(switchy_database::query::identifier("edge_revision"))
            .project_as(
                switchy_database::query::bounded_text(
                    switchy_database::query::identifier("source_node_id"),
                    super::MAX_ID_BYTES as u64,
                    DatabaseValue::Null,
                ),
                "source_node_id",
            )
            .project_as(
                switchy_database::query::bounded_text(
                    switchy_database::query::identifier("source_activation_id"),
                    256,
                    DatabaseValue::Null,
                ),
                "source_activation_id",
            )
            .project_as(
                switchy_database::query::bounded_text(
                    switchy_database::query::identifier("mutation_id"),
                    super::MAX_ID_BYTES as u64,
                    DatabaseValue::Null,
                ),
                "mutation_id",
            )
            .where_eq("run_id", run_id)
            .where_eq("graph_revision", integer(graph_revision)?)
            .where_eq("edge_id", integer(edge.edge_id)?)
            .limit(1),
    )?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let mutation = super::database::text(row, "mutation_id")?;
    let publication = super::database::rows(
        connection,
        &connection
            .select("workflow_graph_edit_publications")
            .columns(&["revision"])
            .where_eq("run_id", run_id)
            .where_eq("mutation_id", mutation)
            .limit(1),
    )?;
    if super::database::unsigned(row, "edge_revision")? != edge.revision
        || super::database::text(row, "source_node_id")? != edge.edge.from
        || publication
            .first()
            .map(|row| super::database::unsigned(row, "revision"))
            .transpose()?
            != Some(graph_revision)
    {
        return Err(WorkflowStoreError::InvalidData(
            "retained edge binding disagrees with committed publication or edge".to_string(),
        ));
    }
    let identity = super::database::text(row, "source_activation_id")?;
    super::validate_id("activation_id", &identity)?;
    Ok(Some(identity))
}

pub fn persist_retained_edge_bindings(
    transaction: &dyn Database,
    request: &bcode_workflow::WorkflowRunGraphEditBatch,
    revision: u64,
) -> Result<(), WorkflowStoreError> {
    for disposition in &request.reconciliation {
        let bcode_workflow::WorkflowRunGraphReconciliation::RetainWithBindings {
            activation_id,
            edge_ids,
        } = disposition
        else {
            continue;
        };
        for edge_id in edge_ids {
            let stored_revision = i64::try_from(revision).map_err(|_| {
                WorkflowStoreError::InvalidData("invalid graph revision".to_string())
            })?;
            let stored_edge = i64::try_from(*edge_id).map_err(|_| {
                WorkflowStoreError::InvalidData("invalid edge identity".to_string())
            })?;
            let rows = super::database::rows(
                transaction,
                &transaction
                    .select("workflow_run_graph_edges")
                    .project(switchy_database::query::identifier("revision"))
                    .project_as(
                        switchy_database::query::bounded_text(
                            switchy_database::query::identifier("source_node_id"),
                            super::MAX_ID_BYTES as u64,
                            DatabaseValue::Null,
                        ),
                        "source_node_id",
                    )
                    .where_eq("run_id", request.run_id.as_str())
                    .where_eq("edge_id", DatabaseValue::Int64(stored_edge))
                    .where_lte("revision", DatabaseValue::Int64(stored_revision))
                    .where_eq("retired_at_revision", DatabaseValue::Null)
                    .limit(2),
            )?;
            if rows.len() != 1 {
                return Err(WorkflowStoreError::InvalidData(
                    "retained edge publication requires one exact source and committed edge"
                        .to_string(),
                ));
            }
            let row = &rows[0];
            let source = super::database::text(row, "source_node_id")?;
            let edge_revision = super::database::unsigned(row, "revision")?;
            let activations = super::database::rows(
                transaction,
                &transaction
                    .select("workflow_activations")
                    .columns(&["activation_id"])
                    .where_eq("run_id", request.run_id.as_str())
                    .where_eq("node_id", source.as_str())
                    .where_eq("activation_id", activation_id.as_str())
                    .limit(2),
            )?;
            if activations.len() != 1 {
                return Err(WorkflowStoreError::InvalidData(
                    "retained edge publication requires one exact source and committed edge"
                        .to_string(),
                ));
            }
            crate::database::block_on(
                transaction
                    .insert("workflow_retained_edge_bindings")
                    .value("run_id", request.run_id.as_str())
                    .value("mutation_id", request.mutation_id.as_str())
                    .value("graph_revision", DatabaseValue::Int64(stored_revision))
                    .value("edge_id", DatabaseValue::Int64(stored_edge))
                    .value(
                        "edge_revision",
                        DatabaseValue::Int64(i64::try_from(edge_revision).map_err(|_| {
                            WorkflowStoreError::InvalidData("invalid edge revision".to_string())
                        })?),
                    )
                    .value("source_node_id", source)
                    .value("source_activation_id", activation_id.as_str())
                    .execute(transaction),
            )?;
        }
    }
    Ok(())
}

pub fn admit_completed_result_successors(
    transaction: &dyn Database,
    request: &bcode_workflow::WorkflowRunGraphEditBatch,
    revision: u64,
    created_at_ms: u64,
) -> Result<(), WorkflowStoreError> {
    let mut admissions = Vec::new();
    let mut claimed_targets = std::collections::BTreeMap::new();
    let mut sources = BTreeSet::new();
    for disposition in &request.reconciliation {
        let bcode_workflow::WorkflowRunGraphReconciliation::RetainWithBindings {
            activation_id,
            edge_ids,
        } = disposition
        else {
            continue;
        };
        let rows = super::database::rows(
            transaction,
            &transaction
                .select("workflow_activations")
                .project_as(
                    switchy_database::query::bounded_text(
                        switchy_database::query::identifier("node_id"),
                        super::MAX_ID_BYTES as u64,
                        DatabaseValue::Null,
                    ),
                    "node_id",
                )
                .project(switchy_database::query::identifier("dependency_generation"))
                .where_eq("run_id", request.run_id.as_str())
                .where_eq("activation_id", activation_id.as_str())
                .where_eq("status", "completed")
                .limit(1),
        )?;
        let Some(row) = rows.first() else {
            continue;
        };
        let node_id = super::database::text(row, "node_id")?;
        let generation = super::database::unsigned(row, "dependency_generation")?;
        if !sources.insert(activation_id) {
            return Err(WorkflowStoreError::InvalidData(
                "completed-result admission requires an unambiguous input source disposition"
                    .to_string(),
            ));
        }
        let mut output =
            super::stored_activation_output(transaction, &request.run_id, &node_id, activation_id)?;
        // Admission is new work; the stored source output and its timestamp remain untouched.
        output.created_at_ms = created_at_ms;
        let selected = completed_result_selected_edges(
            transaction,
            request,
            revision,
            activation_id,
            edge_ids,
            &output,
        )?;
        for (target, (_, _, edge)) in &selected {
            if !super::successor_dependencies_ready(transaction, &output, target, generation)? {
                continue;
            }
            let node = WorkflowStore::current_run_graph_node_in_snapshot(
                transaction,
                &request.run_id,
                target,
            )?
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData("completed-result target is missing".to_string())
            })?;
            let input =
                super::activation_input(transaction, &output, &node.node, generation, edge)?;
            if claimed_targets
                .insert((target.clone(), generation), input.clone())
                .is_some_and(|previous| previous != input)
            {
                return Err(WorkflowStoreError::InvalidData(
                    "completed-result admission requires consistent inputs for each target and generation".to_string(),
                ));
            }
        }
        admissions.push((output, generation, selected));
    }
    // Resolve the whole bounded reconciliation batch before inserting any successor.
    // Otherwise request ordering silently selects the first completed source's input.
    for (output, generation, selected) in admissions {
        super::materialize_selected_successors(
            transaction,
            &output,
            generation,
            revision,
            selected.keys().cloned().collect(),
            &selected,
        )?;
    }
    Ok(())
}

fn completed_result_selected_edges(
    transaction: &dyn Database,
    request: &bcode_workflow::WorkflowRunGraphEditBatch,
    revision: u64,
    activation_id: &str,
    edge_ids: &[u64],
    output: &super::ValidatedOutput,
) -> Result<CompletedResultEdges, WorkflowStoreError> {
    let mut selected = std::collections::BTreeMap::new();
    for edge_id in edge_ids {
        let integer = |value| {
            i64::try_from(value).map(DatabaseValue::Int64).map_err(|_| {
                WorkflowStoreError::InvalidData("invalid graph storage integer".to_string())
            })
        };
        let rows = super::database::rows(
            transaction,
            &transaction
                .select("workflow_retained_edge_bindings")
                .columns(&["edge_revision"])
                .where_eq("run_id", request.run_id.as_str())
                .where_eq("graph_revision", integer(revision)?)
                .where_eq("edge_id", integer(*edge_id)?)
                .where_eq("source_activation_id", activation_id)
                .limit(1),
        )?;
        let row = rows.first().ok_or_else(|| {
            WorkflowStoreError::InvalidData("retained edge binding is missing".to_string())
        })?;
        let edge_revision = super::database::unsigned(row, "edge_revision")?;
        let record = WorkflowStore::current_run_graph_edge_in_snapshot(
            transaction,
            &request.run_id,
            *edge_id,
            &mut BTreeSet::new(),
        )?
        .filter(|record| record.revision == edge_revision)
        .ok_or_else(|| {
            WorkflowStoreError::InvalidData("retained edge binding is not live".to_string())
        })?;
        let edge = record.edge;
        let enabled = match &edge.kind {
            bcode_workflow::EdgeKind::Direct => true,
            bcode_workflow::EdgeKind::Conditional {
                predicate,
                expected,
            } => super::evaluate_predicate(predicate, &output.value)? == *expected,
            bcode_workflow::EdgeKind::Back { .. } | bcode_workflow::EdgeKind::Retry { .. } => false,
        };
        if enabled
            && selected
                .insert(edge.to.clone(), (*edge_id, edge_revision, edge))
                .is_some()
        {
            return Err(WorkflowStoreError::InvalidData(
                "completed result requires one selected edge per target".to_string(),
            ));
        }
    }
    Ok(selected)
}

fn persist_leaf_retentions(
    transaction: &dyn Database,
    run_id: &str,
    revision: u64,
    retentions: &[(String, String)],
) -> Result<(), WorkflowStoreError> {
    for (node_id, activation_id) in retentions {
        crate::database::block_on(
            transaction
                .insert("workflow_leaf_retentions")
                .value("run_id", run_id)
                .value("node_id", node_id.as_str())
                .value("activation_id", activation_id.as_str())
                .value(
                    "revision",
                    DatabaseValue::Int64(i64::try_from(revision).map_err(|_| {
                        WorkflowStoreError::InvalidData("invalid graph revision".to_string())
                    })?),
                )
                .execute(transaction),
        )?;
    }
    Ok(())
}

fn admit_added_leaf_entries(
    transaction: &dyn Database,
    request: &bcode_workflow::WorkflowRunGraphEditBatch,
    revision: u64,
    created_at_ms: u64,
) -> Result<(), WorkflowStoreError> {
    for edit in &request.edits {
        let bcode_workflow::WorkflowRunGraphEdit::AddNode {
            node, entry: true, ..
        } = edit
        else {
            continue;
        };
        let input = super::transform_run_json(
            transaction,
            &request.run_id,
            super::output::TransformRunField::Input,
        )?
        .as_deref()
        .map(serde_json::from_str)
        .transpose()?
        .unwrap_or(serde_json::Value::Null);
        let activation = super::NewActivation {
            run_id: request.run_id.clone(),
            node_id: node.id.clone(),
            activation_id: super::activation_identity(&request.run_id, &node.id, 0),
            dependency_generation: 0,
            input: Some(input),
            created_at_ms,
        };
        super::validate_activation(&activation)?;
        super::enforce_activation_limits(transaction, &activation)?;
        super::insert_activation_bound_to_node(
            transaction,
            &activation,
            super::activation_status_for_node(node),
            node,
            revision,
        )?;
        super::record_activation_graph_binding(
            transaction,
            &request.run_id,
            &node.id,
            &activation.activation_id,
            revision,
        )?;
    }
    Ok(())
}

// Local cancellation remains deferred until publication; recheck after external owners settle.
fn publication_local_cancellation(
    connection: &dyn Database,
    run_id: &str,
    activation_id: &str,
) -> Result<bool, WorkflowStoreError> {
    let rows = super::database::rows(
        connection,
        &connection
            .select("workflow_activations")
            .project_as(
                switchy_database::query::bounded_text(
                    switchy_database::query::identifier("node_id"),
                    super::MAX_ID_BYTES as u64,
                    DatabaseValue::Null,
                ),
                "node_id",
            )
            .where_eq("run_id", run_id)
            .where_eq("activation_id", activation_id)
            .where_eq("output_id", DatabaseValue::Null)
            .where_or(switchy_database::boxed![
                switchy_database::query::where_eq("status", "pending"),
                switchy_database::query::where_eq("status", "waiting_input"),
                switchy_database::query::where_eq("status", "waiting_approval")
            ])
            .limit(1),
    )?;
    let Some(row) = rows.first() else {
        return Ok(false);
    };
    let node = super::database::text(row, "node_id")?;
    for (table, run_column, activation_column) in [
        ("workflow_attempts", "run_id", "activation_id"),
        (
            "workflow_run_links",
            "parent_run_id",
            "parent_activation_id",
        ),
    ] {
        if !super::database::rows(
            connection,
            &connection
                .select(table)
                .columns(&[activation_column])
                .where_eq(run_column, run_id)
                .where_eq(activation_column, activation_id)
                .limit(1),
        )?
        .is_empty()
        {
            return Ok(false);
        }
    }
    Ok(super::database::rows(
        connection,
        &connection
            .select("workflow_fan_out_members")
            .columns(&["member_activation_id"])
            .where_eq("run_id", run_id)
            .where_or(switchy_database::boxed![
                switchy_database::query::where_eq("member_activation_id", activation_id),
                switchy_database::query::where_eq("controller_node_id", node)
            ])
            .limit(1),
    )?
    .is_empty())
}

fn publication_fan_out_local_target(
    connection: &dyn Database,
    request: &bcode_workflow::WorkflowRunGraphEditBatch,
    activation_id: &str,
) -> Result<bool, WorkflowStoreError> {
    let rows = super::database::rows(
        connection,
        &connection
            .select("workflow_fan_out_members")
            .project_as(
                switchy_database::query::bounded_text(
                    switchy_database::query::identifier("controller_activation_id"),
                    super::MAX_ID_BYTES as u64,
                    DatabaseValue::Null,
                ),
                "controller_activation_id",
            )
            .where_eq("run_id", request.run_id.as_str())
            .where_or(switchy_database::boxed![
                switchy_database::query::where_eq("member_activation_id", activation_id),
                switchy_database::query::where_eq("controller_activation_id", activation_id)
            ])
            .limit(1),
    )?;
    let controller = rows
        .first()
        .map(|row| super::database::text(row, "controller_activation_id"))
        .transpose()?;
    let Some(controller) = controller else {
        return Ok(false);
    };
    if !request.reconciliation.iter().any(|item| matches!(item, bcode_workflow::WorkflowRunGraphReconciliation::Cancel { activation_id } if *activation_id == controller)) { return Ok(false); }
    Ok(!fan_out_has_external_work(
        connection,
        &request.run_id,
        activation_id,
    )?)
}

fn never_handed_off_preparation(
    connection: &dyn Database,
    run_id: &str,
    activation_id: &str,
) -> Result<bool, WorkflowStoreError> {
    use switchy_database::query::{join_columns, qualified_column as q};
    Ok(!super::database::rows(
        connection,
        &connection
            .select("workflow_attempts")
            .project(q("workflow_attempts", "dispatch_identity"))
            .joins(vec![join_columns(
                "workflow_dispatch_handoffs",
                q("workflow_dispatch_handoffs", "dispatch_identity"),
                q("workflow_attempts", "dispatch_identity"),
                false,
            )])
            .where_eq("run_id", run_id)
            .where_eq("activation_id", activation_id)
            .where_eq("status", "prepared")
            .where_eq("receipt_json", DatabaseValue::Null)
            .where_eq("handed_off", DatabaseValue::Int64(0))
            .limit(1),
    )?
    .is_empty())
}

fn fan_out_has_external_work(
    connection: &dyn Database,
    run_id: &str,
    activation_id: &str,
) -> Result<bool, WorkflowStoreError> {
    use switchy_database::query::{join_columns, qualified_column as q, where_eq, where_not_eq};
    let attempts = super::database::rows(
        connection,
        &connection
            .select("workflow_attempts")
            .project(q("workflow_attempts", "dispatch_identity"))
            .left_joins(vec![join_columns(
                "workflow_dispatch_handoffs",
                q("workflow_dispatch_handoffs", "dispatch_identity"),
                q("workflow_attempts", "dispatch_identity"),
                true,
            )])
            .where_eq("run_id", run_id)
            .where_eq("activation_id", activation_id)
            .where_or(switchy_database::boxed![
                where_not_eq("status", "prepared"),
                where_not_eq("receipt_json", DatabaseValue::Null),
                where_eq("handed_off", DatabaseValue::Null),
                where_not_eq("handed_off", DatabaseValue::Int64(0))
            ])
            .limit(1),
    )?;
    let links = super::database::rows(
        connection,
        &connection
            .select("workflow_run_links")
            .columns(&["parent_activation_id"])
            .where_eq("parent_run_id", run_id)
            .where_eq("parent_activation_id", activation_id)
            .limit(1),
    )?;
    Ok(!attempts.is_empty() || !links.is_empty())
}

fn record_fan_out_publication_cancellation(
    transaction: &dyn Database,
    request: &bcode_workflow::WorkflowRunGraphEditBatch,
    controller: &str,
    created_at_ms: u64,
) -> Result<(), WorkflowStoreError> {
    super::append_event_in(
        transaction,
        &request.run_id,
        "fan_out_publication_cancelled",
        &serde_json::json!({"activation_id": controller, "mutation_id": request.mutation_id})
            .to_string(),
        created_at_ms,
    )
}

fn validate_fan_out_retirement_members(
    connection: &dyn Database,
    run_id: &str,
    controller: &str,
) -> Result<(), WorkflowStoreError> {
    let mut cursor: Option<String> = None;
    loop {
        let mut query = connection
            .select("workflow_fan_out_members")
            .project_as(
                switchy_database::query::bounded_text(
                    switchy_database::query::identifier("member_activation_id"),
                    super::MAX_ID_BYTES as u64,
                    DatabaseValue::Null,
                ),
                "member_activation_id",
            )
            .project_as(
                switchy_database::query::bounded_text(
                    switchy_database::query::identifier("member_node_id"),
                    super::MAX_ID_BYTES as u64,
                    DatabaseValue::Null,
                ),
                "member_node_id",
            )
            .project_as(
                switchy_database::query::bounded_text(
                    switchy_database::query::identifier("status"),
                    super::MAX_ID_BYTES as u64,
                    DatabaseValue::Null,
                ),
                "status",
            )
            .where_eq("run_id", run_id)
            .where_eq("controller_activation_id", controller)
            .sort("member_activation_id", SortDirection::Asc)
            .limit(GRAPH_PAGE_LIMIT);
        if let Some(cursor) = cursor.as_deref() {
            query = query.where_gt("member_activation_id", cursor);
        }
        let rows = super::database::rows(connection, &query)?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            let identity = super::database::text(&row, "member_activation_id")?;
            let node = super::database::text(&row, "member_node_id")?;
            let status = super::database::text(&row, "status")?;
            let has_output = !super::database::rows(
                connection,
                &connection
                    .select("workflow_fan_out_members")
                    .columns(&["member_activation_id"])
                    .where_eq("run_id", run_id)
                    .where_eq("controller_activation_id", controller)
                    .where_eq("member_activation_id", identity.as_str())
                    .where_not_eq("output_json", DatabaseValue::Null)
                    .limit(1),
            )?
            .is_empty();
            let outcome_matches = if matches!(status.as_str(), "completed" | "failed") {
                !super::database::rows(
                    connection,
                    &connection
                        .select("workflow_activations")
                        .columns(&["activation_id"])
                        .where_eq("run_id", run_id)
                        .where_eq("activation_id", identity.as_str())
                        .where_eq("node_id", node)
                        .where_eq("status", status.as_str())
                        .limit(1),
                )?
                .is_empty()
            } else {
                true
            };
            if !matches!(
                status.as_str(),
                "pending" | "waiting" | "running" | "completed" | "failed"
            ) || !outcome_matches
                || has_output != (status == "completed")
            {
                return Err(WorkflowStoreError::InvalidData(
                    "fan-out retirement requires consistent member outcomes".into(),
                ));
            }
            cursor = Some(identity);
        }
    }
    Ok(())
}

fn cancel_never_handed_off_attempts(
    transaction: &dyn Database,
    run_id: &str,
    activation_id: &str,
    created_at_ms: u64,
) -> Result<(), WorkflowStoreError> {
    use switchy_database::query::{join_columns, qualified_column as q};
    let rows = super::database::rows(
        transaction,
        &transaction
            .select("workflow_attempts")
            .project_as(
                switchy_database::query::bounded_text(
                    q("workflow_attempts", "dispatch_identity"),
                    super::MAX_ID_BYTES as u64,
                    DatabaseValue::Null,
                ),
                "dispatch_identity",
            )
            .joins(vec![join_columns(
                "workflow_dispatch_handoffs",
                q("workflow_dispatch_handoffs", "dispatch_identity"),
                q("workflow_attempts", "dispatch_identity"),
                false,
            )])
            .where_eq("run_id", run_id)
            .where_eq("activation_id", activation_id)
            .where_eq("status", "prepared")
            .where_eq("receipt_json", DatabaseValue::Null)
            .where_eq("handed_off", DatabaseValue::Int64(0))
            .limit(bcode_workflow::MAX_WORKFLOW_RUN_GRAPH_EDITS + 1),
    )?;
    if rows.len() > bcode_workflow::MAX_WORKFLOW_RUN_GRAPH_EDITS {
        return Err(WorkflowStoreError::InvalidData(
            "too many prepared cancellation attempts".to_string(),
        ));
    }
    for row in rows {
        let identity = super::database::text(&row, "dispatch_identity")?;
        crate::database::block_on(
            transaction
                .update("workflow_attempts")
                .value("status", "cancelled")
                .value(
                    "terminal_at_ms",
                    DatabaseValue::Int64(i64::try_from(created_at_ms).map_err(|_| {
                        WorkflowStoreError::InvalidData(
                            "invalid cancellation timestamp".to_string(),
                        )
                    })?),
                )
                .where_eq("dispatch_identity", identity)
                .where_eq("status", "prepared")
                .where_eq("receipt_json", DatabaseValue::Null)
                .execute(transaction),
        )?;
    }
    Ok(())
}

fn release_cancelled_resources(
    transaction: &dyn Database,
    run_id: &str,
    activation_id: &str,
    created_at_ms: u64,
) -> Result<(), WorkflowStoreError> {
    crate::database::block_on(
        transaction
            .update("workflow_resource_leases")
            .value(
                "released_at_ms",
                DatabaseValue::Int64(i64::try_from(created_at_ms).map_err(|_| {
                    WorkflowStoreError::InvalidData("invalid cancellation timestamp".to_string())
                })?),
            )
            .where_eq("run_id", run_id)
            .where_eq("activation_id", activation_id)
            .where_eq("released_at_ms", DatabaseValue::Null)
            .execute(transaction),
    )?;
    Ok(())
}

fn cancel_unstarted_fan_outs(
    transaction: &dyn Database,
    request: &bcode_workflow::WorkflowRunGraphEditBatch,
    created_at_ms: u64,
) -> Result<BTreeSet<String>, WorkflowStoreError> {
    let cancelled = request
        .reconciliation
        .iter()
        .filter_map(|item| match item {
            bcode_workflow::WorkflowRunGraphReconciliation::Cancel { activation_id } => {
                Some(activation_id.as_str())
            }
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let mut handled = BTreeSet::new();
    for controller in &cancelled {
        let members = unstarted_fan_out_members(transaction, &request.run_id, controller)?;
        if members.is_empty()
            && super::database::rows(
                transaction,
                &transaction
                    .select("workflow_fan_out_members")
                    .columns(&["member_activation_id"])
                    .where_eq("run_id", request.run_id.as_str())
                    .where_eq("controller_activation_id", *controller)
                    .limit(1),
            )?
            .is_empty()
        {
            continue;
        }
        validate_fan_out_retirement_members(transaction, &request.run_id, controller)?;
        if members.len() > bcode_workflow::MAX_WORKFLOW_RUN_GRAPH_EDITS {
            return Err(WorkflowStoreError::InvalidData(
                "fan-out cancellation requires bounded member reconciliation".into(),
            ));
        }
        let controller_identity = (*controller).to_string();
        validate_unstarted_fan_out_cancellation(
            transaction,
            request,
            &cancelled,
            &controller_identity,
            &members,
        )?;
        for identity in members
            .iter()
            .chain(std::slice::from_ref(&controller_identity))
        {
            cancel_never_handed_off_attempts(
                transaction,
                &request.run_id,
                identity,
                created_at_ms,
            )?;
            crate::database::block_on(
                transaction
                    .update("workflow_activations")
                    .value("status", "cancelled")
                    .where_eq("run_id", request.run_id.as_str())
                    .where_eq("activation_id", identity.as_str())
                    .where_eq("output_id", DatabaseValue::Null)
                    .execute(transaction),
            )?;
            release_cancelled_resources(transaction, &request.run_id, identity, created_at_ms)?;
            handled.insert(identity.clone());
        }
        crate::database::block_on(
            transaction
                .update("workflow_fan_out_members")
                .value("status", "cancelled")
                .value(
                    "terminal_at_ms",
                    DatabaseValue::Int64(i64::try_from(created_at_ms).map_err(|_| {
                        WorkflowStoreError::InvalidData(
                            "invalid cancellation timestamp".to_string(),
                        )
                    })?),
                )
                .where_eq("run_id", request.run_id.as_str())
                .where_eq("controller_activation_id", *controller)
                .where_not_in(
                    "status",
                    vec![
                        DatabaseValue::String("completed".to_string()),
                        DatabaseValue::String("failed".to_string()),
                    ],
                )
                .execute(transaction),
        )?;
        record_fan_out_publication_cancellation(transaction, request, controller, created_at_ms)?;
    }
    Ok(handled)
}

fn unstarted_fan_out_members(
    transaction: &dyn Database,
    run_id: &str,
    controller: &str,
) -> Result<Vec<String>, WorkflowStoreError> {
    let rows = super::database::rows(
        transaction,
        &transaction
            .select("workflow_fan_out_members")
            .project_as(
                switchy_database::query::bounded_text(
                    switchy_database::query::identifier("member_activation_id"),
                    super::MAX_ID_BYTES as u64,
                    DatabaseValue::Null,
                ),
                "member_activation_id",
            )
            .where_eq("run_id", run_id)
            .where_eq("controller_activation_id", controller)
            .where_not_in(
                "status",
                vec![
                    DatabaseValue::String("completed".to_string()),
                    DatabaseValue::String("failed".to_string()),
                ],
            )
            .limit(bcode_workflow::MAX_WORKFLOW_RUN_GRAPH_EDITS + 1),
    )?;
    rows.iter()
        .map(|row| super::database::text(row, "member_activation_id"))
        .collect::<Result<Vec<_>, _>>()
}

fn validate_unstarted_fan_out_cancellation(
    transaction: &dyn Database,
    request: &bcode_workflow::WorkflowRunGraphEditBatch,
    cancelled: &BTreeSet<&str>,
    controller: &str,
    members: &[String],
) -> Result<(), WorkflowStoreError> {
    for identity in members
        .iter()
        .map(String::as_str)
        .chain(std::slice::from_ref(&controller).iter().copied())
    {
        if accepted_cancellation_settled(transaction, request, identity)? {
            continue;
        }
        let rows = super::database::rows(
            transaction,
            &transaction
                .select("workflow_activations")
                .project_as(
                    switchy_database::query::bounded_text(
                        switchy_database::query::identifier("status"),
                        super::MAX_ID_BYTES as u64,
                        DatabaseValue::Null,
                    ),
                    "status",
                )
                .where_eq("run_id", request.run_id.as_str())
                .where_eq("activation_id", identity)
                .limit(1),
        )?;
        let status = rows
            .first()
            .map(|row| super::database::text(row, "status"))
            .transpose()?;
        let unsafe_work = fan_out_has_external_work(transaction, &request.run_id, identity)?;
        let prepared = never_handed_off_preparation(transaction, &request.run_id, identity)?;
        if unsafe_work
            || status.as_deref().is_none_or(|status| {
                !(status == "waiting" && identity != controller)
                    && (!cancelled.contains(identity)
                        || if identity == controller {
                            status != "running"
                        } else {
                            !(matches!(status, "pending" | "waiting_input" | "waiting_approval")
                                || status == "running" && prepared)
                        })
            })
        {
            return Err(WorkflowStoreError::InvalidData("fan-out cancellation requires explicit unstarted member dispositions without attempts or linked work".into()));
        }
    }
    Ok(())
}

fn cancel_unstarted_leaf_activations(
    transaction: &dyn Database,
    request: &bcode_workflow::WorkflowRunGraphEditBatch,
    created_at_ms: u64,
) -> Result<(), WorkflowStoreError> {
    let fan_outs = cancel_unstarted_fan_outs(transaction, request, created_at_ms)?;
    for disposition in &request.reconciliation {
        let bcode_workflow::WorkflowRunGraphReconciliation::Cancel { activation_id } = disposition
        else {
            continue;
        };
        if fan_outs.contains(activation_id)
            || accepted_cancellation_settled(transaction, request, activation_id)?
        {
            continue;
        }
        cancel_never_handed_off_attempts(
            transaction,
            &request.run_id,
            activation_id,
            created_at_ms,
        )?;
        let valid = leaf_cancellation_is_unstarted(transaction, &request.run_id, activation_id)?;
        if !valid {
            return Err(WorkflowStoreError::InvalidData("publication cancellation requires unstarted work or never-handed-off preparation without linked work".to_string()));
        }
        let changed = crate::database::block_on(
            transaction
                .update("workflow_activations")
                .value("status", "cancelled")
                .where_eq("run_id", request.run_id.as_str())
                .where_eq("activation_id", activation_id.as_str())
                .where_eq("output_id", DatabaseValue::Null)
                .execute(transaction),
        )?;
        if changed.len() != 1 {
            return Err(WorkflowStoreError::InvalidData(
                "ambiguous cancellation activation".to_string(),
            ));
        }
        release_cancelled_resources(transaction, &request.run_id, activation_id, created_at_ms)?;
        super::append_event_in(transaction, &request.run_id, "graph_activation_cancelled",
            &serde_json::json!({"activation_id": activation_id, "mutation_id": request.mutation_id}).to_string(),
            created_at_ms)?;
    }
    Ok(())
}

fn validate_leaf_retention(
    connection: &dyn Database,
    request: &bcode_workflow::WorkflowRunGraphEditBatch,
) -> Result<Vec<(String, String)>, WorkflowStoreError> {
    let mut retentions = Vec::new();
    let mut retained = BTreeSet::new();
    for disposition in &request.reconciliation {
        let activation_id = match disposition {
            bcode_workflow::WorkflowRunGraphReconciliation::Cancel { .. } => continue,
            bcode_workflow::WorkflowRunGraphReconciliation::Retain { activation_id }
            | bcode_workflow::WorkflowRunGraphReconciliation::RetainWithBindings {
                activation_id,
                ..
            } => activation_id,
        };
        if matches!(
            disposition,
            bcode_workflow::WorkflowRunGraphReconciliation::Retain { .. }
        ) || matches!(
            disposition,
            bcode_workflow::WorkflowRunGraphReconciliation::RetainWithBindings { .. }
        ) {
            let completed = !super::database::rows(
                connection,
                &connection
                    .select("workflow_activations")
                    .columns(&["activation_id"])
                    .where_eq("run_id", request.run_id.as_str())
                    .where_eq("activation_id", activation_id.as_str())
                    .where_eq("status", "completed")
                    .where_not_eq("output_id", DatabaseValue::Null)
                    .limit(1),
            )?
            .is_empty();
            // Candidate validation verifies completed results before this active-work pass.
            // They are bound to edges at publication, not given a new admission or retention row.
            if completed {
                continue;
            }
        }
        retained.insert(activation_id.as_str());
    }
    let rows = super::database::rows(
        connection,
        &connection
            .select("workflow_activations")
            .project_as(
                switchy_database::query::bounded_text(
                    switchy_database::query::identifier("node_id"),
                    super::MAX_ID_BYTES as u64,
                    DatabaseValue::Null,
                ),
                "node_id",
            )
            .project_as(
                switchy_database::query::bounded_text(
                    switchy_database::query::identifier("activation_id"),
                    super::MAX_ID_BYTES as u64,
                    DatabaseValue::Null,
                ),
                "activation_id",
            )
            .where_eq("run_id", request.run_id.as_str())
            .where_not_in(
                "status",
                vec![
                    DatabaseValue::String("completed".to_string()),
                    DatabaseValue::String("failed".to_string()),
                    DatabaseValue::String("cancelled".to_string()),
                    DatabaseValue::String("skipped".to_string()),
                ],
            )
            .limit(bcode_workflow::MAX_WORKFLOW_RUN_GRAPH_EDITS + 1),
    )?;
    for row in rows {
        let node_id = super::database::text(&row, "node_id")?;
        let activation_id = super::database::text(&row, "activation_id")?;
        let changed = !super::database::rows(
            connection,
            &connection
                .select("workflow_graph_edit_nodes")
                .columns(&["node_id"])
                .where_eq("run_id", request.run_id.as_str())
                .where_eq("mutation_id", request.mutation_id.as_str())
                .where_eq("node_id", node_id.as_str())
                .limit(1),
        )?
        .is_empty();
        if changed || !retained.remove(activation_id.as_str()) {
            return Err(WorkflowStoreError::InvalidData(
                "leaf publication requires explicit retention of every unchanged active node"
                    .to_string(),
            ));
        }
        reconciled_activation_exit(connection, &request.run_id, &node_id, &activation_id)?;
        retentions.push((node_id, activation_id));
    }
    if !retained.is_empty() {
        return Err(WorkflowStoreError::InvalidData(
            "retention does not identify active leaf work".to_string(),
        ));
    }
    Ok(retentions)
}

fn leaf_cancellation_is_unstarted(
    transaction: &dyn Database,
    run_id: &str,
    activation_id: &str,
) -> Result<bool, WorkflowStoreError> {
    let rows = super::database::rows(
        transaction,
        &transaction
            .select("workflow_activations")
            .project_as(
                switchy_database::query::bounded_text(
                    switchy_database::query::identifier("node_id"),
                    super::MAX_ID_BYTES as u64,
                    DatabaseValue::Null,
                ),
                "node_id",
            )
            .project_as(
                switchy_database::query::bounded_text(
                    switchy_database::query::identifier("status"),
                    super::MAX_ID_BYTES as u64,
                    DatabaseValue::Null,
                ),
                "status",
            )
            .where_eq("run_id", run_id)
            .where_eq("activation_id", activation_id)
            .where_eq("output_id", DatabaseValue::Null)
            .limit(2),
    )?;
    Ok(if let [row] = rows.as_slice() {
        let status = super::database::text(row, "status")?;
        let node_id = super::database::text(row, "node_id")?;
        let (cancelled_attempt, unsafe_attempt) =
            leaf_cancellation_attempt_evidence(transaction, run_id, activation_id)?;
        let links = super::database::rows(
            transaction,
            &transaction
                .select("workflow_run_links")
                .columns(&["parent_activation_id"])
                .where_eq("parent_run_id", run_id)
                .where_eq("parent_activation_id", activation_id)
                .limit(1),
        )?;
        let members = super::database::rows(
            transaction,
            &transaction
                .select("workflow_fan_out_members")
                .columns(&["member_activation_id"])
                .where_eq("run_id", run_id)
                .where_or(switchy_database::boxed![
                    switchy_database::query::where_eq("member_activation_id", activation_id),
                    switchy_database::query::where_eq("controller_node_id", node_id)
                ])
                .limit(1),
        )?;
        matches!(
            status.as_str(),
            "pending" | "running" | "waiting_input" | "waiting_approval"
        ) && (status != "running" || cancelled_attempt)
            && !unsafe_attempt
            && links.is_empty()
            && members.is_empty()
    } else {
        false
    })
}

fn leaf_cancellation_attempt_evidence(
    transaction: &dyn Database,
    run_id: &str,
    activation_id: &str,
) -> Result<(bool, bool), WorkflowStoreError> {
    use switchy_database::query::{join_columns, qualified_column as q};
    let cancelled_attempt = !super::database::rows(
        transaction,
        &transaction
            .select("workflow_attempts")
            .project(q("workflow_attempts", "dispatch_identity"))
            .joins(vec![join_columns(
                "workflow_dispatch_handoffs",
                q("workflow_dispatch_handoffs", "dispatch_identity"),
                q("workflow_attempts", "dispatch_identity"),
                false,
            )])
            .where_eq("run_id", run_id)
            .where_eq("activation_id", activation_id)
            .where_eq("status", "cancelled")
            .where_eq("handed_off", DatabaseValue::Int64(0))
            .limit(1),
    )?
    .is_empty();
    let unsafe_attempt = !super::database::rows(
        transaction,
        &transaction
            .select("workflow_attempts")
            .project(q("workflow_attempts", "dispatch_identity"))
            .left_joins(vec![join_columns(
                "workflow_dispatch_handoffs",
                q("workflow_dispatch_handoffs", "dispatch_identity"),
                q("workflow_attempts", "dispatch_identity"),
                true,
            )])
            .where_eq("run_id", run_id)
            .where_eq("activation_id", activation_id)
            .where_or(switchy_database::boxed![
                switchy_database::query::where_not_eq("status", "cancelled"),
                switchy_database::query::where_eq("handed_off", DatabaseValue::Null),
                switchy_database::query::where_not_eq("handed_off", DatabaseValue::Int64(0))
            ])
            .limit(1),
    )?
    .is_empty();
    Ok((cancelled_attempt, unsafe_attempt))
}

fn accepted_cancellation_settled(
    connection: &dyn Database,
    request: &bcode_workflow::WorkflowRunGraphEditBatch,
    activation_id: &str,
) -> Result<bool, WorkflowStoreError> {
    use switchy_database::query::{join_columns, qualified_column as q};
    let rows = super::database::rows(
        connection,
        &connection
            .select("workflow_publication_cancellations")
            .project(q("workflow_publication_cancellations", "dispatch_identity"))
            .joins(vec![
                join_columns(
                    "workflow_pending_publications",
                    q("workflow_pending_publications", "run_id"),
                    q("workflow_publication_cancellations", "run_id"),
                    false,
                ),
                join_columns(
                    "workflow_attempts",
                    q("workflow_attempts", "dispatch_identity"),
                    q("workflow_publication_cancellations", "dispatch_identity"),
                    false,
                ),
                join_columns(
                    "workflow_activations",
                    q("workflow_activations", "activation_id"),
                    q("workflow_attempts", "activation_id"),
                    false,
                ),
            ])
            .where_eq_expression(
                q("workflow_pending_publications", "mutation_id"),
                q("workflow_publication_cancellations", "mutation_id"),
            )
            .where_eq_expression(
                q("workflow_activations", "run_id"),
                q("workflow_attempts", "run_id"),
            )
            .where_eq_expression(
                q("workflow_activations", "node_id"),
                q("workflow_attempts", "node_id"),
            )
            .where_eq_expression(
                q("workflow_publication_cancellations", "run_id"),
                DatabaseValue::String(request.run_id.clone()),
            )
            .where_eq_expression(
                q("workflow_publication_cancellations", "mutation_id"),
                DatabaseValue::String(request.mutation_id.clone()),
            )
            .where_eq_expression(
                q("workflow_pending_publications", "expected_revision"),
                DatabaseValue::Int64(i64::try_from(request.expected_revision).map_err(|_| {
                    WorkflowStoreError::InvalidData("invalid graph revision".to_string())
                })?),
            )
            .where_eq_expression(
                q("workflow_attempts", "run_id"),
                DatabaseValue::String(request.run_id.clone()),
            )
            .where_eq_expression(
                q("workflow_attempts", "activation_id"),
                DatabaseValue::String(activation_id.to_string()),
            )
            .where_eq_expression(
                q("workflow_attempts", "status"),
                DatabaseValue::String("cancelled".to_string()),
            )
            .where_eq_expression(
                q("workflow_activations", "status"),
                DatabaseValue::String("cancelled".to_string()),
            )
            .where_eq_expression(q("workflow_activations", "output_id"), DatabaseValue::Null)
            .limit(1),
    )?;
    Ok(!rows.is_empty())
}

pub fn validate_reconciliation_targets(
    connection: &dyn Database,
    request: &bcode_workflow::WorkflowRunGraphEditBatch,
) -> Result<(), WorkflowStoreError> {
    use bcode_workflow::WorkflowRunGraphReconciliation as Reconciliation;
    for disposition in &request.reconciliation {
        let (Reconciliation::Retain { activation_id }
        | Reconciliation::RetainWithBindings { activation_id, .. }
        | Reconciliation::Cancel { activation_id }) = disposition;
        let rows = reconciliation_activation_rows(connection, &request.run_id, activation_id)?;
        let valid = if let Some(row) = rows.first() {
            let status = super::database::text(row, "status")?;
            let output = match row.get("output_id") {
                Some(DatabaseValue::Null) => None,
                Some(DatabaseValue::String(value)) if value.is_empty() => {
                    let absent = !super::database::rows(
                        connection,
                        &connection
                            .select("workflow_activations")
                            .columns(&["activation_id"])
                            .where_eq("run_id", request.run_id.as_str())
                            .where_eq("activation_id", activation_id.as_str())
                            .where_eq("output_id", DatabaseValue::Null)
                            .limit(1),
                    )?
                    .is_empty();
                    if absent {
                        None
                    } else {
                        return Err(WorkflowStoreError::InvalidData(
                            "invalid activation output identity".to_string(),
                        ));
                    }
                }
                Some(DatabaseValue::String(value)) => Some(value),
                _ => {
                    return Err(WorkflowStoreError::InvalidData(
                        "invalid activation output identity".to_string(),
                    ));
                }
            };
            if status == "completed"
                && output.is_some()
                && matches!(
                    disposition,
                    Reconciliation::Retain { .. } | Reconciliation::RetainWithBindings { .. }
                )
            {
                let node_id = super::database::text(row, "node_id")?;
                if matches!(disposition, Reconciliation::Retain { .. })
                    && request.edits.iter().any(|edit| match edit {
                        bcode_workflow::WorkflowRunGraphEdit::ReplaceNode { node, .. } => {
                            node.id == node_id
                        }
                        bcode_workflow::WorkflowRunGraphEdit::RemoveNode { node_id: removed } => {
                            removed == &node_id
                        }
                        _ => false,
                    })
                {
                    return Err(WorkflowStoreError::InvalidData(
                        "completed retention cannot rewrite its node".into(),
                    ));
                }
                super::activation_output_value_by_identity(
                    connection,
                    &request.run_id,
                    &node_id,
                    activation_id,
                )?;
                true
            } else if status == "cancelled"
                && output.is_none()
                && matches!(disposition, Reconciliation::Cancel { .. })
            {
                accepted_cancellation_settled(connection, request, activation_id)?
            } else {
                output.is_none()
                    && matches!(
                        status.as_str(),
                        "pending"
                            | "running"
                            | "waiting_input"
                            | "waiting_approval"
                            | "waiting_mutation_approval"
                    )
            }
        } else {
            false
        };
        if !valid || rows.len() != 1 {
            return Err(WorkflowStoreError::InvalidData(
                "graph reconciliation requires an unambiguous active activation or explicitly bound completed result in this run"
                    .to_string(),
            ));
        }
    }
    Ok(())
}

fn reconciliation_activation_rows(
    connection: &dyn Database,
    run_id: &str,
    activation_id: &str,
) -> Result<Vec<switchy_database::Row>, WorkflowStoreError> {
    super::database::rows(
        connection,
        &connection
            .select("workflow_activations")
            .project_as(
                switchy_database::query::bounded_text(
                    switchy_database::query::identifier("status"),
                    super::MAX_ID_BYTES as u64,
                    DatabaseValue::Null,
                ),
                "status",
            )
            .project_as(
                switchy_database::query::bounded_text(
                    switchy_database::query::identifier("output_id"),
                    super::MAX_ID_BYTES as u64,
                    DatabaseValue::String(String::new()),
                ),
                "output_id",
            )
            .project_as(
                switchy_database::query::bounded_text(
                    switchy_database::query::identifier("node_id"),
                    super::MAX_ID_BYTES as u64,
                    DatabaseValue::Null,
                ),
                "node_id",
            )
            .where_eq("run_id", run_id)
            .where_eq("activation_id", activation_id)
            .limit(2),
    )
}

// Caller owns authorization, candidate validation, and the transaction commit.
pub fn persist_graph_publication(
    transaction: &dyn Database,
    request: &bcode_workflow::WorkflowRunGraphEditBatch,
    retentions: &[(String, String)],
    created_at_ms: u64,
) -> Result<u64, WorkflowStoreError> {
    let run_id = &request.run_id;
    let mutation_id = &request.mutation_id;
    let revision = request.expected_revision.checked_add(1).ok_or_else(|| {
        WorkflowStoreError::InvalidData("workflow graph revision overflow".to_string())
    })?;
    persist_leaf_retentions(transaction, run_id, revision, retentions)?;
    publish_candidate_edges(transaction, run_id, mutation_id, revision)?;
    let stored_revision = i64::try_from(revision)
        .map_err(|_| WorkflowStoreError::InvalidData("invalid graph revision".to_string()))?;
    publish_candidate_nodes(transaction, run_id, mutation_id, revision)?;
    let changed = crate::database::block_on(
        transaction
            .update("workflow_run_graphs")
            .value("revision", DatabaseValue::Int64(stored_revision))
            .where_eq("run_id", run_id.as_str())
            .where_eq(
                "revision",
                DatabaseValue::Int64(i64::try_from(request.expected_revision).map_err(|_| {
                    WorkflowStoreError::InvalidData("invalid graph revision".to_string())
                })?),
            )
            .execute(transaction),
    )?;
    if changed.len() != 1 {
        return Err(WorkflowStoreError::InvalidData(
            "workflow graph revision conflict".to_string(),
        ));
    }
    crate::database::block_on(
        transaction
            .insert("workflow_graph_edit_publications")
            .value("run_id", run_id.as_str())
            .value("mutation_id", mutation_id.as_str())
            .value("revision", DatabaseValue::Int64(stored_revision))
            .execute(transaction),
    )?;
    persist_retained_edge_bindings(transaction, request, revision)?;
    admit_added_leaf_entries(transaction, request, revision, created_at_ms)?;
    admit_completed_result_successors(transaction, request, revision, created_at_ms)?;
    super::append_event_in(
        transaction,
        run_id,
        "graph_edit_published",
        &serde_json::json!({"mutation_id": mutation_id, "revision": revision}).to_string(),
        created_at_ms,
    )?;
    Ok(revision)
}

fn publish_candidate_nodes(
    transaction: &dyn Database,
    run_id: &str,
    mutation_id: &str,
    revision: u64,
) -> Result<(), WorkflowStoreError> {
    let stored_revision = i64::try_from(revision)
        .map_err(|_| WorkflowStoreError::InvalidData("invalid graph revision".into()))?;
    let mut cursor: Option<String> = None;
    loop {
        let mut query = transaction
            .select("workflow_graph_edit_nodes")
            .project_as(
                switchy_database::query::bounded_text(
                    switchy_database::query::identifier("node_id"),
                    super::MAX_ID_BYTES as u64,
                    DatabaseValue::Null,
                ),
                "node_id",
            )
            .project_as(
                switchy_database::query::bounded_text(
                    switchy_database::query::identifier("node_json"),
                    super::MAX_INLINE_JSON_BYTES as u64,
                    DatabaseValue::Null,
                ),
                "node_json",
            )
            .project(switchy_database::query::identifier("is_entry"))
            .project(switchy_database::query::identifier("is_exit"))
            .where_eq("run_id", run_id)
            .where_eq("mutation_id", mutation_id)
            .sort("node_id", SortDirection::Asc)
            .limit(GRAPH_PAGE_LIMIT);
        if let Some(cursor) = cursor.as_deref() {
            query = query.where_gt("node_id", cursor);
        }
        let rows = super::database::rows(transaction, &query)?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            let node_id = super::database::text(&row, "node_id")?;
            let payload = match row.get("node_json") {
                Some(DatabaseValue::String(payload)) => Some(payload),
                Some(DatabaseValue::Null) => {
                    let deletion = super::database::rows(
                        transaction,
                        &transaction
                            .select("workflow_graph_edit_nodes")
                            .columns(&["node_id"])
                            .where_eq("run_id", run_id)
                            .where_eq("mutation_id", mutation_id)
                            .where_eq("node_id", node_id.as_str())
                            .where_eq("node_json", DatabaseValue::Null)
                            .limit(1),
                    )?;
                    if deletion.is_empty() {
                        return Err(WorkflowStoreError::InvalidData(
                            "candidate node payload is invalid or oversized".to_string(),
                        ));
                    }
                    None
                }
                _ => {
                    return Err(WorkflowStoreError::InvalidData(
                        "candidate node payload is invalid".to_string(),
                    ));
                }
            };
            crate::database::block_on(
                transaction
                    .update("workflow_run_graph_nodes")
                    .value("retired_at_revision", DatabaseValue::Int64(stored_revision))
                    .where_eq("run_id", run_id)
                    .where_eq("node_id", node_id.as_str())
                    .where_eq("retired_at_revision", DatabaseValue::Null)
                    .execute(transaction),
            )?;
            if let Some(payload) = payload {
                let entry = graph_boolean(&row, "is_entry")?;
                let exit = graph_boolean(&row, "is_exit")?;
                crate::database::block_on(
                    transaction
                        .insert("workflow_run_graph_nodes")
                        .value("run_id", run_id)
                        .value("node_id", node_id.as_str())
                        .value("revision", DatabaseValue::Int64(stored_revision))
                        .value("node_json", payload)
                        .value("is_entry", DatabaseValue::Int64(i64::from(entry)))
                        .value("is_exit", DatabaseValue::Int64(i64::from(exit)))
                        .execute(transaction),
                )?;
            }
            cursor = Some(node_id);
        }
    }
    Ok(())
}

fn publish_candidate_edges(
    transaction: &dyn Database,
    run_id: &str,
    mutation_id: &str,
    revision: u64,
) -> Result<(), WorkflowStoreError> {
    let revision = i64::try_from(revision)
        .map_err(|_| WorkflowStoreError::InvalidData("invalid graph revision".to_string()))?;
    let mut cursor = None;
    loop {
        let mut query = transaction
            .select("workflow_graph_edit_edges")
            .project(switchy_database::query::identifier("edge_id"))
            .project_as(
                switchy_database::query::bounded_text(
                    switchy_database::query::identifier("edge_json"),
                    super::MAX_INLINE_JSON_BYTES as u64,
                    DatabaseValue::Null,
                ),
                "edge_json",
            )
            .where_eq("run_id", run_id)
            .where_eq("mutation_id", mutation_id)
            .sort("edge_id", SortDirection::Asc)
            .limit(GRAPH_PAGE_LIMIT);
        if let Some(cursor) = cursor {
            query = query.where_gt("edge_id", DatabaseValue::Int64(cursor));
        }
        let rows = super::database::rows(transaction, &query)?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            let edge_id =
                i64::try_from(super::database::unsigned(&row, "edge_id")?).map_err(|_| {
                    WorkflowStoreError::InvalidData("invalid edge identity".to_string())
                })?;
            crate::database::block_on(
                transaction
                    .update("workflow_run_graph_edges")
                    .value("retired_at_revision", DatabaseValue::Int64(revision))
                    .where_eq("run_id", run_id)
                    .where_eq("edge_id", DatabaseValue::Int64(edge_id))
                    .where_eq("retired_at_revision", DatabaseValue::Null)
                    .execute(transaction),
            )?;
            let payload = match row.get("edge_json") {
                Some(DatabaseValue::String(payload)) => Some(payload),
                Some(DatabaseValue::Null) => {
                    let deletion = super::database::rows(
                        transaction,
                        &transaction
                            .select("workflow_graph_edit_edges")
                            .columns(&["edge_id"])
                            .where_eq("run_id", run_id)
                            .where_eq("mutation_id", mutation_id)
                            .where_eq("edge_id", DatabaseValue::Int64(edge_id))
                            .where_eq("edge_json", DatabaseValue::Null)
                            .limit(1),
                    )?;
                    if deletion.is_empty() {
                        return Err(WorkflowStoreError::InvalidData(
                            "candidate edge payload is invalid or oversized".to_string(),
                        ));
                    }
                    None
                }
                _ => {
                    return Err(WorkflowStoreError::InvalidData(
                        "candidate edge payload is invalid".to_string(),
                    ));
                }
            };
            if let Some(payload) = payload {
                let edge: EdgeDefinition = serde_json::from_str(&payload)?;
                crate::database::block_on(
                    transaction
                        .insert("workflow_run_graph_edges")
                        .value("run_id", run_id)
                        .value("edge_id", DatabaseValue::Int64(edge_id))
                        .value("revision", DatabaseValue::Int64(revision))
                        .value("source_node_id", edge.from)
                        .value("target_node_id", edge.to)
                        .value("edge_json", payload)
                        .execute(transaction),
                )?;
            }
            cursor = Some(edge_id);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn validate_retained_bindings(
    connection: &dyn Database,
    request: &bcode_workflow::WorkflowRunGraphEditBatch,
    graph: &WorkflowDefinition,
    edges: &std::collections::BTreeMap<u64, EdgeDefinition>,
) -> Result<(), WorkflowStoreError> {
    let mut claimed_edges = BTreeSet::new();
    for disposition in &request.reconciliation {
        let (activation_id, edge_ids) = match disposition {
            bcode_workflow::WorkflowRunGraphReconciliation::Cancel { .. } => continue,
            bcode_workflow::WorkflowRunGraphReconciliation::Retain { activation_id } => {
                (activation_id, &[][..])
            }
            bcode_workflow::WorkflowRunGraphReconciliation::RetainWithBindings {
                activation_id,
                edge_ids,
            } => (activation_id, edge_ids.as_slice()),
        };
        for edge_id in edge_ids {
            if !claimed_edges.insert(*edge_id) {
                return Err(WorkflowStoreError::InvalidData(
                    "retained edge requires one unambiguous source activation".to_string(),
                ));
            }
        }
        let rows = super::database::rows(
            connection,
            &connection
                .select("workflow_activations")
                .project_as(
                    switchy_database::query::bounded_text(
                        switchy_database::query::identifier("node_id"),
                        super::MAX_ID_BYTES as u64,
                        DatabaseValue::Null,
                    ),
                    "node_id",
                )
                .project_as(
                    switchy_database::query::bounded_text(
                        switchy_database::query::identifier("status"),
                        super::MAX_ID_BYTES as u64,
                        DatabaseValue::Null,
                    ),
                    "status",
                )
                .where_eq("run_id", request.run_id.as_str())
                .where_eq("activation_id", activation_id.as_str())
                .limit(1),
        )?;
        let row = rows.first().ok_or_else(|| {
            WorkflowStoreError::InvalidData("binding activation is missing".to_string())
        })?;
        let source_id = super::database::text(row, "node_id")?;
        let completed = super::database::text(row, "status")? == "completed";
        // Active retained work settles against the new topology, so every outgoing edge
        // needs an explicit source binding. Completed results may deliberately bind a subset.
        if !completed
            && edges
                .iter()
                .any(|(id, edge)| edge.from == source_id && !edge_ids.contains(id))
        {
            return Err(WorkflowStoreError::InvalidData(
                "active retained source requires bindings for every candidate outgoing edge"
                    .to_string(),
            ));
        }
        let completed_value = completed
            .then(|| {
                super::activation_output_value_by_identity(
                    connection,
                    &request.run_id,
                    &source_id,
                    activation_id,
                )
            })
            .transpose()?;
        let source = bound_activation_node(connection, &request.run_id, &source_id, activation_id)?
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData("binding source is missing".to_string())
            })?;
        for edge_id in edge_ids {
            let edge = edges.get(edge_id).ok_or_else(|| {
                WorkflowStoreError::InvalidData(
                    "binding edge is missing from candidate".to_string(),
                )
            })?;
            let target = graph.nodes.get(&edge.to).ok_or_else(|| {
                WorkflowStoreError::InvalidData(
                    "binding target is missing from candidate".to_string(),
                )
            })?;
            // A constant plan does not reinterpret the retained result. Its declared
            // target and literal value can be checked independently of source data.
            if edge.from == source_id
                && target.kind != bcode_workflow::NodeKind::Parallel
                && let Some(transform) = &edge.transform
                && transform.version == bcode_workflow::WORKFLOW_TRANSFORM_VERSION
                && transform.output == target.input
                && let bcode_workflow::WorkflowTransformExpression::Constant { value } =
                    &transform.expression
            {
                target
                    .input
                    .validate_value("retained constant binding", value)
                    .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
                continue;
            }
            // Exact schema equality is a conservative proof. Transform-aware compatibility
            // requires a separate proof and must not be inferred from a node identity.
            let target_schema = if target.kind == bcode_workflow::NodeKind::Parallel {
                let members = bcode_workflow::parallel_join_member_ids(target)
                    .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
                let position = members
                    .iter()
                    .position(|member| *member == source_id)
                    .ok_or_else(|| {
                        WorkflowStoreError::InvalidData(
                            "retained source is not a parallel member".into(),
                        )
                    })?;
                target
                    .input
                    .schema
                    .get("prefixItems")
                    .and_then(serde_json::Value::as_array)
                    .and_then(|items| items.get(position))
                    .ok_or_else(|| {
                        WorkflowStoreError::InvalidData(
                            "parallel tuple member schema missing".into(),
                        )
                    })?
            } else {
                &target.input.schema
            };
            let compatible = if target.kind == bcode_workflow::NodeKind::Parallel {
                bcode_workflow::parallel_member_schema_matches(&source.output.schema, target_schema)
            } else {
                source.output == target.input
            };
            if edge.from != source_id || edge.transform.is_some() || !compatible {
                return Err(WorkflowStoreError::InvalidData(
                    "retained-result binding lacks compatible source and target schemas"
                        .to_string(),
                ));
            }
            if let Some(value) = &completed_value {
                super::validate_json_schema("retained-result target input", target_schema, value)
                    .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
            }
        }
    }
    Ok(())
}

// Retain only host-installed metadata on an otherwise exact authored candidate node.
fn retained_candidate_node(
    transaction: &dyn Database,
    request: &bcode_workflow::WorkflowRunGraphEditBatch,
    node: &NodeDefinition,
) -> Result<NodeDefinition, WorkflowStoreError> {
    use switchy_database::query::FilterableQuery as _;
    let query = switchy_database::query::select("workflow_graph_edit_nodes")
        .columns(&["node_json"])
        .where_eq("run_id", request.run_id.as_str())
        .where_eq("mutation_id", request.mutation_id.as_str())
        .where_eq("node_id", node.id.as_str())
        .limit(1);
    let rows = super::database::rows(transaction, &query)
        .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
    let Some(switchy_database::DatabaseValue::String(payload)) =
        rows.first().and_then(|row| row.get("node_json"))
    else {
        return Ok(node.clone());
    };
    if payload.len() > super::MAX_INLINE_JSON_BYTES {
        return Err(WorkflowStoreError::InvalidData(
            "candidate node exceeds bounds".into(),
        ));
    }
    let mut retained: NodeDefinition = serde_json::from_str(&payload)?;
    let Some(parent) = retained
        .configuration
        .as_object_mut()
        .and_then(|object| object.remove("execution_parent"))
    else {
        return Ok(node.clone());
    };
    let binding: bcode_workflow::WorkflowExecutionParentBinding = serde_json::from_value(parent)?;
    if node.kind != bcode_workflow::NodeKind::Agent
        || binding.version != 1
        || binding.session_id.trim().is_empty()
        || binding.session_id.len() > 128
    {
        return Err(WorkflowStoreError::InvalidData(
            "invalid retained execution parent".into(),
        ));
    }
    if node.configuration.get("execution_parent").is_some() {
        if node != &serde_json::from_str::<NodeDefinition>(&payload)? {
            return Err(WorkflowStoreError::InvalidData(
                "conflicting authenticated execution parent".into(),
            ));
        }
        return Ok(node.clone());
    }
    if retained != *node {
        return Err(WorkflowStoreError::InvalidData(
            "retained execution parent candidate mismatch".into(),
        ));
    }
    let mut admitted = node.clone();
    admitted
        .configuration
        .as_object_mut()
        .ok_or_else(|| {
            WorkflowStoreError::InvalidData("prompt configuration must be object".into())
        })?
        .insert("execution_parent".into(), serde_json::to_value(binding)?);
    Ok(admitted)
}

fn persist_candidate_delta(
    transaction: &dyn Database,
    request: &bcode_workflow::WorkflowRunGraphEditBatch,
    graph: &WorkflowDefinition,
    edges: &std::collections::BTreeMap<u64, EdgeDefinition>,
) -> Result<(), WorkflowStoreError> {
    use bcode_workflow::WorkflowRunGraphEdit as Edit;
    let mut nodes = BTreeSet::new();
    let mut edge_ids = BTreeSet::new();
    for edit in &request.edits {
        match edit {
            Edit::AddNode { node, .. } | Edit::ReplaceNode { node, .. } => {
                nodes.insert(node.id.as_str());
            }
            Edit::RemoveNode { node_id } => {
                nodes.insert(node_id.as_str());
            }
            Edit::AddEdge { edge_id, .. }
            | Edit::ReplaceEdge { edge_id, .. }
            | Edit::RemoveEdge { edge_id } => {
                edge_ids.insert(*edge_id);
            }
        }
    }
    for node_id in nodes {
        let payload = graph
            .nodes
            .get(node_id)
            .map(|node| retained_candidate_node(transaction, request, node))
            .transpose()?
            .as_ref()
            .map(|node| super::bounded_json("validated graph node", node))
            .transpose()?;
        let entry = DatabaseValue::Int64(i64::from(graph.entries.iter().any(|id| id == node_id)));
        let exit = DatabaseValue::Int64(i64::from(graph.exits.iter().any(|id| id == node_id)));
        let payload = payload.map_or(DatabaseValue::Null, DatabaseValue::String);
        let updated = crate::database::block_on(
            transaction
                .update("workflow_graph_edit_nodes")
                .value("node_json", payload.clone())
                .value("is_entry", entry.clone())
                .value("is_exit", exit.clone())
                .where_eq("run_id", request.run_id.as_str())
                .where_eq("mutation_id", request.mutation_id.as_str())
                .where_eq("node_id", node_id)
                .execute(transaction),
        )?;
        if updated.is_empty() {
            crate::database::block_on(
                transaction
                    .insert("workflow_graph_edit_nodes")
                    .value("run_id", request.run_id.as_str())
                    .value("mutation_id", request.mutation_id.as_str())
                    .value("node_id", node_id)
                    .value("node_json", payload)
                    .value("is_entry", entry)
                    .value("is_exit", exit)
                    .execute(transaction),
            )?;
        }
    }
    for edge_id in edge_ids {
        let payload = edges
            .get(&edge_id)
            .map(|edge| super::bounded_json("validated graph edge", edge))
            .transpose()?;
        let payload = payload.map_or(DatabaseValue::Null, DatabaseValue::String);
        let edge_id =
            DatabaseValue::Int64(i64::try_from(edge_id).map_err(|_| {
                WorkflowStoreError::InvalidData("invalid edge identity".to_string())
            })?);
        let updated = crate::database::block_on(
            transaction
                .update("workflow_graph_edit_edges")
                .value("edge_json", payload.clone())
                .where_eq("run_id", request.run_id.as_str())
                .where_eq("mutation_id", request.mutation_id.as_str())
                .where_eq("edge_id", edge_id.clone())
                .execute(transaction),
        )?;
        if updated.is_empty() {
            crate::database::block_on(
                transaction
                    .insert("workflow_graph_edit_edges")
                    .value("run_id", request.run_id.as_str())
                    .value("mutation_id", request.mutation_id.as_str())
                    .value("edge_id", edge_id)
                    .value("edge_json", payload)
                    .execute(transaction),
            )?;
        }
    }
    Ok(())
}

fn apply_candidate_edit(
    graph: &mut WorkflowDefinition,
    edges: &mut std::collections::BTreeMap<u64, EdgeDefinition>,
    edit: &bcode_workflow::WorkflowRunGraphEdit,
) -> Result<(), WorkflowStoreError> {
    use bcode_workflow::WorkflowRunGraphEdit as Edit;
    let invalid =
        || WorkflowStoreError::InvalidData("graph edit identity precondition failed".to_string());
    match edit {
        Edit::AddNode { node, entry, exit } | Edit::ReplaceNode { node, entry, exit } => {
            if graph.nodes.contains_key(&node.id) != matches!(edit, Edit::ReplaceNode { .. }) {
                return Err(invalid());
            }
            graph.entries.retain(|id| id != &node.id);
            graph.exits.retain(|id| id != &node.id);
            if *entry {
                graph.entries.push(node.id.clone());
            }
            if *exit {
                graph.exits.push(node.id.clone());
            }
            graph.nodes.insert(node.id.clone(), node.clone());
        }
        Edit::RemoveNode { node_id } => {
            graph.nodes.remove(node_id).ok_or_else(invalid)?;
            graph.entries.retain(|id| id != node_id);
            graph.exits.retain(|id| id != node_id);
        }
        Edit::AddEdge { edge_id, edge } | Edit::ReplaceEdge { edge_id, edge } => {
            if edges.contains_key(edge_id) != matches!(edit, Edit::ReplaceEdge { .. }) {
                return Err(invalid());
            }
            edges.insert(*edge_id, edge.clone());
        }
        Edit::RemoveEdge { edge_id } => {
            edges.remove(edge_id).ok_or_else(invalid)?;
        }
    }
    Ok(())
}

pub fn initialize(connection: &dyn Database) -> Result<(), WorkflowStoreError> {
    initialize_workflow_run_graphs(connection)?;
    initialize_workflow_run_graph_nodes(connection)?;
    initialize_workflow_run_graph_edges(connection)?;
    initialize_workflow_run_graph_edges_target(connection)?;
    initialize_source_index(connection)?;
    initialize_retirement(connection)
}

fn initialize_workflow_run_graphs(connection: &dyn Database) -> Result<(), WorkflowStoreError> {
    use switchy_database::schema::{
        Column, DataType, SchemaComparison as C, SchemaExpression as E, create_table,
    };
    crate::database::block_on(
        create_table("workflow_run_graphs")
            .columns(vec![
                Column {
                    name: "run_id".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
                Column {
                    name: "revision".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::BigInt,
                    default: None,
                },
            ])
            .primary_key_columns(["run_id"])
            .foreign_key_column("run_id", "workflow_runs", "run_id")
            .check(E::column("revision").compare(C::Greater, E::Integer(0)))
            .execute(connection),
    )?;
    Ok(())
}

fn initialize_workflow_run_graph_nodes(
    connection: &dyn Database,
) -> Result<(), WorkflowStoreError> {
    use switchy_database::schema::{
        Column, DataType, SchemaComparison as C, SchemaExpression as E, create_table,
    };
    crate::database::block_on(
        create_table("workflow_run_graph_nodes")
            .columns(vec![
                Column {
                    name: "run_id".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
                Column {
                    name: "node_id".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
                Column {
                    name: "revision".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::BigInt,
                    default: None,
                },
                Column {
                    name: "node_json".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
                Column {
                    name: "is_entry".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::BigInt,
                    default: None,
                },
                Column {
                    name: "is_exit".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::BigInt,
                    default: None,
                },
            ])
            .primary_key_columns(["run_id", "node_id", "revision"])
            .foreign_key_column("run_id", "workflow_run_graphs", "run_id")
            .check(E::column("revision").compare(C::Greater, E::Integer(0)))
            .check(E::Or(vec![
                E::column("is_entry").compare(C::Equal, E::Integer(0)),
                E::column("is_entry").compare(C::Equal, E::Integer(1)),
            ]))
            .check(E::Or(vec![
                E::column("is_exit").compare(C::Equal, E::Integer(0)),
                E::column("is_exit").compare(C::Equal, E::Integer(1)),
            ]))
            .execute(connection),
    )?;
    Ok(())
}

fn initialize_workflow_run_graph_edges(
    connection: &dyn Database,
) -> Result<(), WorkflowStoreError> {
    use switchy_database::schema::{
        Column, DataType, SchemaComparison as C, SchemaExpression as E, create_table,
    };
    crate::database::block_on(
        create_table("workflow_run_graph_edges")
            .columns(vec![
                Column {
                    name: "run_id".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
                Column {
                    name: "edge_id".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::BigInt,
                    default: None,
                },
                Column {
                    name: "revision".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::BigInt,
                    default: None,
                },
                Column {
                    name: "source_node_id".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
                Column {
                    name: "target_node_id".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
                Column {
                    name: "edge_json".to_string(),
                    nullable: false,
                    auto_increment: false,
                    data_type: DataType::Text,
                    default: None,
                },
            ])
            .primary_key_columns(["run_id", "edge_id", "revision"])
            .foreign_key_column("run_id", "workflow_run_graphs", "run_id")
            .check(E::column("edge_id").compare(C::GreaterEqual, E::Integer(0)))
            .check(E::column("revision").compare(C::Greater, E::Integer(0)))
            .execute(connection),
    )?;
    Ok(())
}

fn initialize_workflow_run_graph_edges_target(
    connection: &dyn Database,
) -> Result<(), WorkflowStoreError> {
    use switchy_database::schema::create_index;
    crate::database::block_on(
        create_index("workflow_run_graph_edges_target")
            .table("workflow_run_graph_edges")
            .columns(vec!["run_id", "target_node_id", "edge_id"])
            .execute(connection),
    )?;
    Ok(())
}

pub fn initialize_source_index(connection: &dyn Database) -> Result<(), WorkflowStoreError> {
    crate::database::block_on(
        switchy_database::schema::create_index("workflow_run_graph_edges_source")
            .table("workflow_run_graph_edges")
            .columns(vec!["run_id", "source_node_id", "edge_id"])
            .execute(connection),
    )?;
    Ok(())
}

pub fn materialize(
    transaction: &dyn Database,
    run_id: &str,
    definition: &WorkflowDefinition,
) -> Result<(), WorkflowStoreError> {
    if definition.schema_version != bcode_workflow::WORKFLOW_DEFINITION_SCHEMA_VERSION {
        return Err(WorkflowStoreError::InvalidData(
            "unsupported workflow definition schema version during graph materialization"
                .to_string(),
        ));
    }
    super::validate_id("run_id", run_id)?;
    for (id, node) in &definition.nodes {
        super::validate_id("node_id", id)?;
        if id != &node.id {
            return Err(WorkflowStoreError::InvalidData(
                "workflow graph node identity mismatch".to_string(),
            ));
        }
    }
    let entries: BTreeSet<_> = definition.entries.iter().collect();
    let exits: BTreeSet<_> = definition.exits.iter().collect();
    for id in entries.union(&exits) {
        if !definition.nodes.contains_key(*id) {
            return Err(WorkflowStoreError::InvalidData(
                "workflow graph boundary references a missing node".to_string(),
            ));
        }
    }
    for edge in &definition.edges {
        if !definition.nodes.contains_key(&edge.from) || !definition.nodes.contains_key(&edge.to) {
            return Err(WorkflowStoreError::InvalidData(
                "workflow graph edge references a missing node".to_string(),
            ));
        }
    }
    crate::database::block_on(
        transaction
            .insert("workflow_run_graphs")
            .value("run_id", run_id)
            .value("revision", DatabaseValue::Int64(1))
            .execute(transaction),
    )?;
    for (id, node) in &definition.nodes {
        crate::database::block_on(
            transaction
                .insert("workflow_run_graph_nodes")
                .value("run_id", run_id)
                .value("node_id", id.as_str())
                .value("revision", DatabaseValue::Int64(1))
                .value("node_json", super::bounded_json("graph node", node)?)
                .value(
                    "is_entry",
                    DatabaseValue::Int64(i64::from(entries.contains(id))),
                )
                .value(
                    "is_exit",
                    DatabaseValue::Int64(i64::from(exits.contains(id))),
                )
                .execute(transaction),
        )?;
    }
    for (index, edge) in definition.edges.iter().enumerate() {
        let edge_id = i64::try_from(index).map_err(|_| {
            WorkflowStoreError::InvalidData("edge identity exceeds storage range".to_string())
        })?;
        crate::database::block_on(
            transaction
                .insert("workflow_run_graph_edges")
                .value("run_id", run_id)
                .value("edge_id", DatabaseValue::Int64(edge_id))
                .value("revision", DatabaseValue::Int64(1))
                .value("source_node_id", edge.from.as_str())
                .value("target_node_id", edge.to.as_str())
                .value("edge_json", super::bounded_json("graph edge", edge)?)
                .execute(transaction),
        )?;
    }
    Ok(())
}

pub fn migrate(transaction: &dyn Database) -> Result<(), WorkflowStoreError> {
    initialize(transaction)?;
    // Explicit offline migration may scan all runs, retaining one bounded definition at a time.
    let mut cursor: Option<String> = None;
    loop {
        let mut query = transaction
            .select("workflow_runs")
            .project_as(
                switchy_database::query::bounded_text(
                    switchy_database::query::identifier("run_id"),
                    super::MAX_ID_BYTES as u64,
                    DatabaseValue::Null,
                ),
                "run_id",
            )
            .project_as(
                switchy_database::query::bounded_text(
                    switchy_database::query::identifier("definition_id"),
                    super::MAX_ID_BYTES as u64,
                    DatabaseValue::Null,
                ),
                "definition_id",
            )
            .project(switchy_database::query::identifier("definition_version"))
            .sort("run_id", SortDirection::Asc)
            .limit(1);
        if let Some(cursor) = cursor.as_deref() {
            query = query.where_gt("run_id", cursor);
        }
        let rows = super::database::rows(transaction, &query)?;
        let Some(row) = rows.first() else {
            break;
        };
        let run_id = super::database::text(row, "run_id")?;
        let definition_id = super::database::text(row, "definition_id")?;
        let version = super::database::unsigned(row, "definition_version")?;
        let rows = super::database::rows(
            transaction,
            &transaction
                .select("workflow_definitions")
                .project_as(
                    switchy_database::query::bounded_text(
                        switchy_database::query::identifier("definition_json"),
                        super::MAX_INLINE_JSON_BYTES as u64,
                        DatabaseValue::Null,
                    ),
                    "definition_json",
                )
                .project_as(
                    switchy_database::query::bounded_text(
                        switchy_database::query::identifier("checksum_sha256"),
                        64,
                        DatabaseValue::Null,
                    ),
                    "checksum_sha256",
                )
                .where_eq("definition_id", definition_id)
                .where_eq(
                    "version",
                    DatabaseValue::Int64(i64::try_from(version).map_err(|_| {
                        WorkflowStoreError::InvalidData("invalid definition version".to_string())
                    })?),
                )
                .limit(1),
        )?;
        let row = rows.first().ok_or_else(|| {
            WorkflowStoreError::InvalidData(
                "foreign-key verification failed: workflow run definition is missing".to_string(),
            )
        })?;
        let payload = super::database::text(row, "definition_json").map_err(|_| {
            WorkflowStoreError::InvalidData(
                "workflow definition payload is invalid or oversized during graph migration".into(),
            )
        })?;
        let checksum = super::database::text(row, "checksum_sha256").map_err(|_| {
            WorkflowStoreError::InvalidData(
                "workflow definition checksum mismatch during graph migration".into(),
            )
        })?;
        if checksum.len() != 64 || checksum != super::sha256_hex(payload.as_bytes()) {
            return Err(WorkflowStoreError::InvalidData(
                "workflow definition checksum mismatch during graph migration".to_string(),
            ));
        }
        let definition: WorkflowDefinition = serde_json::from_str(&payload)?;
        definition.validate().map_err(|error| {
            WorkflowStoreError::InvalidData(format!(
                "invalid workflow definition during graph migration: {error}"
            ))
        })?;
        materialize(transaction, &run_id, &definition)?;
        cursor = Some(run_id);
    }
    Ok(())
}

pub fn graph_revision(
    connection: &dyn Database,
    run_id: &str,
) -> Result<Option<u64>, WorkflowStoreError> {
    super::validate_id("run_id", run_id)?;
    if super::database::rows(
        connection,
        &connection
            .select("workflow_runs")
            .columns(&["run_id"])
            .where_eq("run_id", run_id)
            .limit(1),
    )?
    .is_empty()
    {
        return Ok(None);
    }
    let rows = super::database::rows(
        connection,
        &connection
            .select("workflow_run_graphs")
            .columns(&["revision"])
            .where_eq("run_id", run_id)
            .limit(1),
    )
    .map_err(|error| match error {
        WorkflowStoreError::StructuredDatabase(switchy_database::DatabaseError::Rusqlite(
            switchy_database::rusqlite::RusqliteDatabaseError::UnsupportedBlob,
        )) => WorkflowStoreError::InvalidData("workflow run graph revision is invalid".into()),
        other => other,
    })?;
    let revision = rows
        .first()
        .map(|row| super::database::unsigned(row, "revision"))
        .transpose()?
        .filter(|revision| *revision > 0)
        .ok_or_else(|| {
            WorkflowStoreError::InvalidData("workflow run graph is missing or invalid".to_string())
        })?;
    let stored_revision = i64::try_from(revision).map_err(|_| {
        WorkflowStoreError::InvalidData(
            "workflow run graph revision exceeds storage range".to_string(),
        )
    })?;
    for table in ["workflow_run_graph_nodes", "workflow_run_graph_edges"] {
        if !super::database::rows(
            connection,
            &connection
                .select(table)
                .columns(&["revision"])
                .where_eq("run_id", run_id)
                .where_gt("retired_at_revision", DatabaseValue::Int64(stored_revision))
                .limit(1),
        )?
        .is_empty()
        {
            return Err(WorkflowStoreError::InvalidData(
                "retirement exceeds committed graph revision".to_string(),
            ));
        }
    }
    Ok(Some(revision))
}

pub fn initial_node(
    connection: &dyn Database,
    run_id: &str,
    node_id: &str,
) -> Result<Option<NodeDefinition>, WorkflowStoreError> {
    initial_node_record_in_snapshot(connection, run_id, node_id)
        .map(|record| record.map(|record| record.node))
}

pub fn initial_activation_node(
    connection: &dyn Database,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
) -> Result<Option<NodeDefinition>, WorkflowStoreError> {
    validate_activation_node_request(run_id, node_id, activation_id)?;
    let node = initial_activation_node_in_snapshot(connection, run_id, node_id, activation_id)?;
    Ok(node)
}

fn initial_activation_node_in_snapshot(
    connection: &dyn Database,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
) -> Result<Option<NodeDefinition>, WorkflowStoreError> {
    super::validate_id("activation_id", activation_id)?;
    let Some(record) = initial_node_record_in_snapshot(connection, run_id, node_id)? else {
        return Ok(None);
    };
    let rows = super::database::rows(
        connection,
        &connection
            .select("workflow_activation_graph_bindings")
            .columns(&["graph_revision"])
            .where_eq("run_id", run_id)
            .where_eq("node_id", node_id)
            .where_eq("activation_id", activation_id)
            .limit(1),
    )?;
    let admitted_revision = rows
        .first()
        .map(|row| super::database::unsigned(row, "graph_revision"))
        .transpose()?;
    if admitted_revision.is_some_and(|revision| revision != 1) {
        return Err(WorkflowStoreError::InvalidData(
            "activation admission binding does not match initial graph".to_string(),
        ));
    }
    let node = bound_activation_node(connection, run_id, node_id, activation_id)?;
    if node.as_ref() != Some(&record.node) {
        return Err(WorkflowStoreError::InvalidData(
            "activation executable binding does not match initial graph".to_string(),
        ));
    }
    Ok(node)
}

pub fn bound_activation_node(
    connection: &dyn Database,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
) -> Result<Option<NodeDefinition>, WorkflowStoreError> {
    activation_node_record(connection, run_id, node_id, activation_id)?
        .map(|record| Some(record.node))
        .ok_or_else(|| {
            WorkflowStoreError::InvalidData("activation executable binding is missing".to_string())
        })
}

fn validate_activation_node_request(
    run_id: &str,
    node_id: &str,
    activation_id: &str,
) -> Result<(), WorkflowStoreError> {
    super::validate_id("activation_id", activation_id)?;
    super::validate_id("node_id", node_id)?;
    super::validate_id("run_id", run_id)
}

fn activation_node_record(
    connection: &dyn Database,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
) -> Result<Option<RunGraphNode>, WorkflowStoreError> {
    validate_activation_node_request(run_id, node_id, activation_id)?;
    let node = activation_node_record_in_snapshot(connection, run_id, node_id, activation_id)?;
    Ok(node)
}

fn activation_node_record_in_snapshot(
    connection: &dyn Database,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
) -> Result<Option<RunGraphNode>, WorkflowStoreError> {
    validate_activation_node_request(run_id, node_id, activation_id)?;
    let rows = super::database::rows(
        connection,
        &connection
            .select("workflow_activations")
            .columns(&["node_revision"])
            .where_eq("run_id", run_id)
            .where_eq("node_id", node_id)
            .where_eq("activation_id", activation_id)
            .limit(1),
    )?;
    let revision = rows
        .first()
        .map(|row| super::database::unsigned(row, "node_revision"))
        .transpose()?;
    let Some(revision) = revision else {
        return Ok(None);
    };
    WorkflowStore::node_revision(connection, run_id, node_id, revision)?
        .ok_or_else(|| {
            WorkflowStoreError::InvalidData("activation executable revision is missing".to_string())
        })
        .map(Some)
}

#[cfg(test)]
pub fn revised_leaf_exit(
    connection: &dyn Database,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
) -> Result<bool, WorkflowStoreError> {
    let exit = reconciled_activation_exit(connection, run_id, node_id, activation_id)?;
    let has_edges = !super::database::rows(
        connection,
        &connection
            .select("workflow_run_graph_edges")
            .columns(&["edge_id"])
            .where_eq("run_id", run_id)
            .where_eq("source_node_id", node_id)
            .where_eq("retired_at_revision", DatabaseValue::Null)
            .limit(1),
    )?
    .is_empty();
    if has_edges {
        return Err(WorkflowStoreError::InvalidData(
            "revised successor settlement requires execution reconciliation".to_string(),
        ));
    }
    Ok(exit)
}

pub fn reconciled_activation_exit(
    connection: &dyn Database,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
) -> Result<bool, WorkflowStoreError> {
    let current = graph_revision(connection, run_id)?.ok_or_else(|| {
        WorkflowStoreError::InvalidData("settlement graph is missing".to_string())
    })?;
    let rows = super::database::rows(
        connection,
        &connection
            .select("workflow_activation_graph_bindings")
            .columns(&["graph_revision"])
            .where_eq("run_id", run_id)
            .where_eq("node_id", node_id)
            .where_eq("activation_id", activation_id)
            .limit(1),
    )?;
    let admitted = rows
        .first()
        .map(|row| super::database::unsigned(row, "graph_revision"))
        .transpose()?;
    if admitted != Some(current) {
        let retained = !super::database::rows(
            connection,
            &connection
                .select("workflow_leaf_retentions")
                .columns(&["revision"])
                .where_eq("run_id", run_id)
                .where_eq("node_id", node_id)
                .where_eq("activation_id", activation_id)
                .where_eq(
                    "revision",
                    DatabaseValue::Int64(i64::try_from(current).map_err(|_| {
                        WorkflowStoreError::InvalidData("invalid graph revision".to_string())
                    })?),
                )
                .limit(1),
        )?
        .is_empty();
        if admitted.is_none_or(|revision| revision == 0 || revision > current) || !retained {
            return Err(WorkflowStoreError::InvalidData(
                "settlement requires reconciliation with the current graph".to_string(),
            ));
        }
    }
    let node =
        activation_node_record(connection, run_id, node_id, activation_id)?.ok_or_else(|| {
            WorkflowStoreError::InvalidData("settlement binding is missing".to_string())
        })?;
    let rows = super::database::rows(
        connection,
        &connection
            .select("workflow_run_graph_nodes")
            .columns(&["revision", "retired_at_revision"])
            .where_eq("run_id", run_id)
            .where_eq("node_id", node_id)
            .sort("revision", SortDirection::Desc)
            .limit(1),
    )?;
    let row = rows
        .first()
        .ok_or_else(|| WorkflowStoreError::InvalidData("settlement node is missing".to_string()))?;
    let latest = (
        super::database::unsigned(row, "revision")?,
        optional_graph_unsigned(row, "retired_at_revision")?,
    );
    if latest != (node.revision, None) {
        return Err(WorkflowStoreError::InvalidData(
            "revised successor settlement requires execution reconciliation".to_string(),
        ));
    }
    Ok(node.exit)
}

#[cfg(test)]
pub fn initial_exit(
    connection: &dyn Database,
    run_id: &str,
    node_id: &str,
) -> Result<bool, WorkflowStoreError> {
    initial_node_record_in_snapshot(connection, run_id, node_id)?
        .map(|record| record.exit)
        .ok_or_else(|| {
            WorkflowStoreError::InvalidData(
                "completed workflow node is missing from the run graph".to_string(),
            )
        })
}

#[cfg(test)]
pub fn initial_edge_between(
    transaction: &dyn Database,
    run_id: &str,
    source_node_id: &str,
    target_node_id: &str,
) -> Result<Option<EdgeDefinition>, WorkflowStoreError> {
    super::validate_id("target_node_id", target_node_id)?;
    let mut cursor = None;
    let mut selected = None;
    loop {
        let edges = initial_outgoing_edges(transaction, run_id, source_node_id, cursor)?;
        if edges.is_empty() {
            return Ok(selected);
        }
        cursor = edges.last().map(|edge| edge.edge_id);
        let final_page = edges.len() < GRAPH_PAGE_LIMIT;
        for record in edges
            .into_iter()
            .filter(|record| record.edge.to == target_node_id)
        {
            if selected.replace(record.edge).is_some() {
                return Err(WorkflowStoreError::InvalidData(
                    "successor input requires an unambiguous edge binding".to_string(),
                ));
            }
        }
        if final_page {
            return Ok(selected);
        }
    }
}

#[cfg(test)]
pub fn initial_incoming_edges(
    connection: &dyn Database,
    run_id: &str,
    target_node_id: &str,
    after_edge_id: Option<u64>,
) -> Result<Vec<RunGraphEdge>, WorkflowStoreError> {
    super::validate_id("target_node_id", target_node_id)?;
    WorkflowStore::graph_edge_page(
        connection,
        run_id,
        Some(EdgeEndpoint::Target(target_node_id)),
        after_edge_id,
        None,
        GRAPH_PAGE_LIMIT,
    )
}

#[cfg(test)]
pub fn initial_outgoing_edges(
    connection: &dyn Database,
    run_id: &str,
    source_node_id: &str,
    after_edge_id: Option<u64>,
) -> Result<Vec<RunGraphEdge>, WorkflowStoreError> {
    super::validate_id("source_node_id", source_node_id)?;
    WorkflowStore::graph_edge_page(
        connection,
        run_id,
        Some(EdgeEndpoint::Source(source_node_id)),
        after_edge_id,
        None,
        GRAPH_PAGE_LIMIT,
    )
}

fn validate_initial_edge_relationships(
    connection: &dyn Database,
    run_id: &str,
    edge_id: u64,
) -> Result<(), WorkflowStoreError> {
    let mut query = connection
        .select("workflow_run_graph_edges")
        .where_eq("run_id", run_id)
        .where_eq("edge_id", super::database::integer_value(edge_id)?)
        .where_eq("revision", DatabaseValue::Int64(1))
        .limit(1);
    for column in ["source_node_id", "target_node_id"] {
        query = query.project_as(
            switchy_database::query::bounded_text(
                switchy_database::query::identifier(column),
                super::MAX_ID_BYTES as u64,
                DatabaseValue::Null,
            ),
            column,
        );
    }
    let rows = super::database::rows(connection, &query)?;
    if let Some(row) = rows.first() {
        for column in ["source_node_id", "target_node_id"] {
            let id = graph_endpoint_identity(row, column)?;
            initial_node_record_in_snapshot(connection, run_id, &id)
                .map_err(|_| {
                    WorkflowStoreError::InvalidData(
                        "workflow graph edge relationships are inconsistent".into(),
                    )
                })?
                .ok_or_else(|| {
                    WorkflowStoreError::InvalidData(
                        "workflow graph edge relationships are inconsistent".into(),
                    )
                })?;
        }
    }
    Ok(())
}

fn initial_node_record_in_snapshot(
    connection: &dyn Database,
    run_id: &str,
    node_id: &str,
) -> Result<Option<RunGraphNode>, WorkflowStoreError> {
    super::validate_id("node_id", node_id)?;
    match graph_revision(connection, run_id)? {
        None => return Ok(None),
        Some(1) => {}
        _ => {
            return Err(WorkflowStoreError::InvalidData(
                "initial graph lookup requires an intact initial graph revision".to_string(),
            ));
        }
    }
    if !super::database::rows(
        connection,
        &connection
            .select("workflow_run_graph_nodes")
            .columns(&["revision"])
            .where_eq("run_id", run_id)
            .where_eq("node_id", node_id)
            .where_not_eq("revision", DatabaseValue::Int64(1))
            .limit(1),
    )?
    .is_empty()
    {
        return Err(WorkflowStoreError::InvalidData(
            "workflow graph node revision or boundary flags are invalid".into(),
        ));
    }
    let rows = super::database::rows(
        connection,
        &connection
            .select("workflow_run_graph_nodes")
            .columns(&["revision"])
            .where_eq("run_id", run_id)
            .where_eq("node_id", node_id)
            .sort("revision", SortDirection::Desc)
            .limit(1),
    )?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    if super::database::unsigned(row, "revision")? != 1 {
        return Err(WorkflowStoreError::InvalidData(
            "workflow graph node payload is invalid or oversized".to_string(),
        ));
    }
    WorkflowStore::node_revision(connection, run_id, node_id, 1)
}

fn edge_cursor(after_edge_id: Option<u64>) -> Result<i64, WorkflowStoreError> {
    after_edge_id
        .map(i64::try_from)
        .transpose()
        .map(|cursor| cursor.unwrap_or(i64::MIN))
        .map_err(|_| {
            WorkflowStoreError::InvalidData("edge cursor exceeds storage range".to_string())
        })
}

/// One consistent, bounded page of a run-owned graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunGraphPage {
    /// Committed graph revision shared by every field.
    pub revision: u64,
    /// First unused edge identity across current and retired rows, or exhausted.
    #[serde(default)]
    pub next_edge_id: Option<u64>,
    /// Current node representations in identity order.
    pub nodes: Vec<RunGraphNode>,
    /// Current edges in identity order.
    pub edges: Vec<RunGraphEdge>,
    /// No nodes remain after this page.
    pub nodes_complete: bool,
    /// No edges remain after this page.
    pub edges_complete: bool,
}

impl WorkflowStore {
    fn validate_graph_page_request(
        run_id: &str,
        expected_revision: Option<u64>,
        after_node_id: Option<&str>,
        after_edge_id: Option<u64>,
        limit: usize,
    ) -> Result<(), WorkflowStoreError> {
        super::validate_id("run_id", run_id)?;
        if limit == 0
            || expected_revision
                .is_some_and(|revision| revision == 0 || i64::try_from(revision).is_err())
        {
            return Err(WorkflowStoreError::InvalidData(
                "invalid graph page limit or revision".to_string(),
            ));
        }
        if let Some(cursor) = after_node_id {
            super::validate_id("node cursor", cursor)?;
        }
        edge_cursor(after_edge_id)?;
        if expected_revision.is_none() && (after_node_id.is_some() || after_edge_id.is_some()) {
            return Err(WorkflowStoreError::InvalidData(
                "graph continuation requires an expected revision".to_string(),
            ));
        }
        Ok(())
    }

    fn next_run_graph_edge_id(
        connection: &dyn Database,
        run_id: &str,
    ) -> Result<Option<u64>, WorkflowStoreError> {
        // The primary key starts with (run_id, edge_id), including retired identities.
        let rows = super::database::rows(
            connection,
            &connection
                .select("workflow_run_graph_edges")
                .columns(&["edge_id"])
                .where_eq("run_id", run_id)
                .sort("edge_id", SortDirection::Desc)
                .limit(1),
        )?;
        let last = rows
            .first()
            .map(|row| super::database::unsigned(row, "edge_id"))
            .transpose()?;
        Ok(last.map_or(Some(0), |last| {
            i64::try_from(last)
                .ok()
                .and_then(|last| last.checked_add(1))
                .and_then(|next| u64::try_from(next).ok())
        }))
    }

    /// Read nodes, edges, and continuation status from one read snapshot.
    ///
    /// First-page discovery may omit the expected revision. Continuation cursors
    /// require the revision returned by the preceding page to avoid mixing plans.
    ///
    /// # Errors
    /// Returns an error for missing graphs, revision conflicts, invalid cursors or
    /// limits, damaged graph data, or database failures.
    pub fn current_run_graph_page(
        &self,
        run_id: &str,
        expected_revision: Option<u64>,
        after_node_id: Option<&str>,
        after_edge_id: Option<u64>,
        limit: usize,
    ) -> Result<RunGraphPage, WorkflowStoreError> {
        let transaction = super::database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let page = Self::current_run_graph_page_in(
            transaction.as_ref(),
            run_id,
            expected_revision,
            after_node_id,
            after_edge_id,
            limit,
        )?;
        super::database::commit(transaction)?;
        Ok(page)
    }

    /// Read one initial node using the caller's already leased snapshot.
    pub(super) fn run_graph_node_in(
        connection: &dyn Database,
        run_id: &str,
        node_id: &str,
    ) -> Result<Option<NodeDefinition>, WorkflowStoreError> {
        super::validate_id("run_id", run_id)?;
        super::validate_id("node_id", node_id)?;
        initial_node_record_in_snapshot(connection, run_id, node_id)
            .map(|node| node.map(|node| node.node))
    }

    /// Read one initial edge using the caller's already leased snapshot.
    pub(super) fn run_graph_edge_in(
        connection: &dyn Database,
        run_id: &str,
        edge_id: u64,
    ) -> Result<Option<RunGraphEdge>, WorkflowStoreError> {
        super::validate_id("run_id", run_id)?;
        match graph_revision(connection, run_id)? {
            None => return Ok(None),
            Some(1) => {}
            _ => {
                return Err(WorkflowStoreError::InvalidData(
                    "initial graph lookup requires an intact initial graph revision".to_string(),
                ));
            }
        }
        let id = i64::try_from(edge_id).map_err(|_| {
            WorkflowStoreError::InvalidData("edge identity exceeds storage range".to_string())
        })?;
        let rows = super::database::rows(
            connection,
            &connection
                .select("workflow_run_graph_edges")
                .columns(&["revision"])
                .where_eq("run_id", run_id)
                .where_eq("edge_id", DatabaseValue::Int64(id))
                .sort("revision", SortDirection::Desc)
                .limit(1),
        )?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        if super::database::unsigned(row, "revision")? != 1 {
            return Err(WorkflowStoreError::InvalidData(
                "invalid initial graph edge revision".to_string(),
            ));
        }
        validate_initial_edge_relationships(connection, run_id, edge_id)?;
        Self::run_graph_edge_revision_in_snapshot(connection, run_id, edge_id, 1)
    }

    /// Read with public validation using the caller-owned database lease.
    pub(super) fn activation_graph_node_in(
        db: &dyn Database,
        run_id: &str,
        node_id: &str,
        activation_id: &str,
    ) -> Result<Option<RunGraphNode>, WorkflowStoreError> {
        activation_node_record_in_snapshot(db, run_id, node_id, activation_id)
    }

    /// Read with public validation using the caller-owned database lease.
    pub(super) fn run_graph_node_revision_in(
        db: &dyn Database,
        run_id: &str,
        node_id: &str,
        revision: u64,
    ) -> Result<Option<RunGraphNode>, WorkflowStoreError> {
        Self::validate_node_revision_request(run_id, node_id, revision)?;

        Self::node_revision(db, run_id, node_id, revision)
    }

    /// Read with public validation using the caller-owned database lease.
    pub(super) fn run_graph_edge_revision_in(
        db: &dyn Database,
        run_id: &str,
        edge_id: u64,
        revision: u64,
    ) -> Result<Option<RunGraphEdge>, WorkflowStoreError> {
        Self::validate_edge_revision_request(run_id, edge_id, revision)?;

        Self::run_graph_edge_revision_in_snapshot(db, run_id, edge_id, revision)
    }

    /// Read with public validation using the caller-owned database lease.
    pub(super) fn current_run_graph_edge_in(
        db: &dyn Database,
        run_id: &str,
        edge_id: u64,
    ) -> Result<Option<RunGraphEdge>, WorkflowStoreError> {
        super::validate_id("run_id", run_id)?;
        i64::try_from(edge_id).map_err(|_| {
            WorkflowStoreError::InvalidData("edge identity exceeds storage range".to_string())
        })?;

        Self::current_run_graph_edge_in_snapshot(db, run_id, edge_id, &mut BTreeSet::new())
    }

    /// Read with public validation using the caller-owned database lease.
    pub(super) fn run_graph_node_record_in(
        db: &dyn Database,
        run_id: &str,
        node_id: &str,
    ) -> Result<Option<RunGraphNode>, WorkflowStoreError> {
        super::validate_id("run_id", run_id)?;
        super::validate_id("node_id", node_id)?;
        initial_node_record_in_snapshot(db, run_id, node_id)
    }

    /// Read with public validation using the caller-owned database lease.
    pub(super) fn run_graph_nodes_in(
        db: &dyn Database,
        run_id: &str,
        after_node_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RunGraphNode>, WorkflowStoreError> {
        Self::validate_graph_page_request(run_id, Some(1), after_node_id, None, limit)?;
        Self::initial_graph_nodes_in_snapshot(db, run_id, after_node_id, limit)
    }

    /// Read with public validation using the caller-owned database lease.
    pub(super) fn run_graph_edges_in(
        db: &dyn Database,
        run_id: &str,
        after_edge_id: Option<u64>,
        limit: usize,
    ) -> Result<Vec<RunGraphEdge>, WorkflowStoreError> {
        Self::graph_edge_page(db, run_id, None, after_edge_id, None, limit)
    }

    /// Read with public validation using the caller-owned database lease.
    pub(super) fn run_graph_incoming_edges_in(
        db: &dyn Database,
        run_id: &str,
        target_node_id: &str,
        after_edge_id: Option<u64>,
        limit: usize,
    ) -> Result<Vec<RunGraphEdge>, WorkflowStoreError> {
        super::validate_id("target_node_id", target_node_id)?;
        Self::graph_edge_page(
            db,
            run_id,
            Some(EdgeEndpoint::Target(target_node_id)),
            after_edge_id,
            None,
            limit,
        )
    }

    /// Read with public validation using the caller-owned database lease.
    pub(super) fn run_graph_outgoing_edges_in(
        db: &dyn Database,
        run_id: &str,
        source_node_id: &str,
        after_edge_id: Option<u64>,
        limit: usize,
    ) -> Result<Vec<RunGraphEdge>, WorkflowStoreError> {
        super::validate_id("source_node_id", source_node_id)?;
        Self::graph_edge_page(
            db,
            run_id,
            Some(EdgeEndpoint::Source(source_node_id)),
            after_edge_id,
            None,
            limit,
        )
    }

    /// Read with public validation using the caller-owned database lease.
    pub(super) fn current_run_graph_nodes_in(
        db: &dyn Database,
        run_id: &str,
        expected_revision: u64,
        after_node_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RunGraphNode>, WorkflowStoreError> {
        Self::validate_graph_page_request(
            run_id,
            Some(expected_revision),
            after_node_id,
            None,
            limit,
        )?;

        Self::current_run_graph_nodes_in_snapshot(
            db,
            run_id,
            expected_revision,
            after_node_id,
            limit,
        )
    }

    /// Read with public validation using the caller-owned database lease.
    pub(super) fn current_run_graph_node_in(
        db: &dyn Database,
        run_id: &str,
        node_id: &str,
    ) -> Result<Option<RunGraphNode>, WorkflowStoreError> {
        super::validate_id("run_id", run_id)?;
        super::validate_id("node_id", node_id)?;

        Self::current_run_graph_node_in_snapshot(db, run_id, node_id)
    }

    /// Read with public validation using the caller-owned database lease.
    pub(super) fn current_run_graph_edges_in(
        db: &dyn Database,
        run_id: &str,
        expected_revision: u64,
        after_edge_id: Option<u64>,
        limit: usize,
    ) -> Result<Vec<RunGraphEdge>, WorkflowStoreError> {
        Self::validate_graph_page_request(
            run_id,
            Some(expected_revision),
            None,
            after_edge_id,
            limit,
        )?;

        Self::current_run_graph_edges_in_snapshot(
            db,
            run_id,
            expected_revision,
            after_edge_id,
            limit,
            None,
        )
    }

    /// Read with public validation using the caller-owned database lease.
    pub(super) fn current_run_graph_incident_edges_in(
        db: &dyn Database,
        run_id: &str,
        expected_revision: u64,
        node_id: &str,
        outgoing: bool,
        after_edge_id: Option<u64>,
        limit: usize,
    ) -> Result<Vec<RunGraphEdge>, WorkflowStoreError> {
        super::validate_id("node_id", node_id)?;
        Self::validate_graph_page_request(
            run_id,
            Some(expected_revision),
            None,
            after_edge_id,
            limit,
        )?;

        let endpoint = if outgoing {
            EdgeEndpoint::Source(node_id)
        } else {
            EdgeEndpoint::Target(node_id)
        };
        Self::current_run_graph_edges_in_snapshot(
            db,
            run_id,
            expected_revision,
            after_edge_id,
            limit,
            Some(endpoint),
        )
    }

    pub(super) fn current_run_graph_page_in(
        connection: &dyn Database,
        run_id: &str,
        expected_revision: Option<u64>,
        after_node_id: Option<&str>,
        after_edge_id: Option<u64>,
        limit: usize,
    ) -> Result<RunGraphPage, WorkflowStoreError> {
        Self::validate_graph_page_request(
            run_id,
            expected_revision,
            after_node_id,
            after_edge_id,
            limit,
        )?;
        let revision = graph_revision(connection, run_id)?.ok_or_else(|| {
            WorkflowStoreError::InvalidData("workflow run graph not found".to_string())
        })?;
        if expected_revision.is_some_and(|expected| expected != revision) {
            return Err(WorkflowStoreError::InvalidData(
                "workflow graph revision conflict".to_string(),
            ));
        }
        let nodes = Self::current_run_graph_nodes_in_snapshot(
            connection,
            run_id,
            revision,
            after_node_id,
            limit,
        )?;
        let edges = Self::current_run_graph_edges_in_snapshot(
            connection,
            run_id,
            revision,
            after_edge_id,
            limit,
            None,
        )?;
        let nodes_complete = match nodes.last() {
            Some(last) => Self::current_run_graph_nodes_in_snapshot(
                connection,
                run_id,
                revision,
                Some(&last.node.id),
                1,
            )?
            .is_empty(),
            None => true,
        };
        let edges_complete = match edges.last() {
            Some(last) => Self::current_run_graph_edges_in_snapshot(
                connection,
                run_id,
                revision,
                Some(last.edge_id),
                1,
                None,
            )?
            .is_empty(),
            None => true,
        };
        let next_edge_id = Self::next_run_graph_edge_id(connection, run_id)?;
        Ok(RunGraphPage {
            revision,
            next_edge_id,
            nodes,
            edges,
            nodes_complete,
            edges_complete,
        })
    }

    /// Read the immutable executable node selected when an activation was admitted.
    ///
    /// Binding and executable data are read from one snapshot, reusing an existing
    /// transaction when the caller already owns one.
    ///
    /// # Errors
    /// Returns an error for invalid identities, missing or malformed revision bindings,
    /// missing executable data, or database failures. An absent activation returns `None`.
    pub fn activation_graph_node(
        &self,
        run_id: &str,
        node_id: &str,
        activation_id: &str,
    ) -> Result<Option<RunGraphNode>, WorkflowStoreError> {
        let transaction = super::database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let result =
            Self::activation_graph_node_in(transaction.as_ref(), run_id, node_id, activation_id)?;
        super::database::commit(transaction)?;
        Ok(result)
    }

    /// Read an exact immutable node revision rather than the current graph topology.
    ///
    /// This does not authorize execution or resolve an activation's binding. Missing
    /// runs or node revisions return `None`; an uncommitted revision is rejected.
    /// Graph metadata and executable data are read from one snapshot. An existing
    /// caller-owned transaction is reused and is neither committed nor rolled back
    /// by this method, including when the read fails.
    ///
    /// # Errors
    /// Returns an error for invalid identities/revisions, damaged graph metadata,
    /// malformed or oversized node payloads, or database failures.
    pub fn run_graph_node_revision(
        &self,
        run_id: &str,
        node_id: &str,
        revision: u64,
    ) -> Result<Option<RunGraphNode>, WorkflowStoreError> {
        Self::validate_node_revision_request(run_id, node_id, revision)?;
        let transaction = super::database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let result =
            Self::run_graph_node_revision_in(transaction.as_ref(), run_id, node_id, revision)?;
        super::database::commit(transaction)?;
        Ok(result)
    }

    fn validate_node_revision_request(
        run_id: &str,
        node_id: &str,
        revision: u64,
    ) -> Result<(), WorkflowStoreError> {
        super::validate_id("run_id", run_id)?;
        super::validate_id("node_id", node_id)?;
        if revision == 0 || i64::try_from(revision).is_err() {
            return Err(WorkflowStoreError::InvalidData(
                "node revision must be a positive storage integer".to_string(),
            ));
        }
        Ok(())
    }

    fn node_revision(
        connection: &dyn Database,
        run_id: &str,
        node_id: &str,
        revision: u64,
    ) -> Result<Option<RunGraphNode>, WorkflowStoreError> {
        Self::validate_node_revision_request(run_id, node_id, revision)?;
        let Some(current) = graph_revision(connection, run_id)? else {
            return Ok(None);
        };
        if revision > current {
            return Err(WorkflowStoreError::InvalidData(
                "node revision exceeds committed graph revision".to_string(),
            ));
        }
        let rows = super::database::rows(
            connection,
            &connection
                .select("workflow_run_graph_nodes")
                .project_as(
                    switchy_database::query::bounded_text(
                        switchy_database::query::identifier("node_json"),
                        super::MAX_INLINE_JSON_BYTES as u64,
                        DatabaseValue::Null,
                    ),
                    "node_json",
                )
                .project(switchy_database::query::identifier("is_entry"))
                .project(switchy_database::query::identifier("is_exit"))
                .where_eq("run_id", run_id)
                .where_eq("node_id", node_id)
                .where_eq(
                    "revision",
                    DatabaseValue::Int64(i64::try_from(revision).map_err(|_| {
                        WorkflowStoreError::InvalidData("invalid node revision".to_string())
                    })?),
                )
                .limit(1),
        )?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let entry = graph_boolean(row, "is_entry")?;
        let exit = graph_boolean(row, "is_exit")?;
        let payload = super::database::text(row, "node_json").map_err(|_| {
            WorkflowStoreError::InvalidData(
                "workflow graph node payload is invalid or oversized".to_string(),
            )
        })?;
        let node: NodeDefinition = serde_json::from_str(&payload)?;
        if node.id != node_id {
            return Err(WorkflowStoreError::InvalidData(
                "workflow graph node identity mismatch".to_string(),
            ));
        }
        Ok(Some(RunGraphNode {
            revision,
            node,
            entry,
            exit,
        }))
    }

    /// Read the latest committed representation of a run-owned node.
    ///
    /// Unlike an activation binding, this lookup follows node revisions. It does not
    /// authorize dispatch. Missing runs or nodes return `None`.
    /// Revision selection and executable data are read from one snapshot. An
    /// existing caller-owned transaction is reused; its completion remains the
    /// caller's responsibility on both success and failure.
    ///
    /// # Errors
    /// Returns an error for invalid identities, uncommitted revisions, damaged graph
    /// metadata, malformed or oversized executable data, or database failures.
    pub fn current_run_graph_node(
        &self,
        run_id: &str,
        node_id: &str,
    ) -> Result<Option<RunGraphNode>, WorkflowStoreError> {
        super::validate_id("run_id", run_id)?;
        super::validate_id("node_id", node_id)?;
        let transaction = super::database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let result = Self::current_run_graph_node_in(transaction.as_ref(), run_id, node_id)?;
        super::database::commit(transaction)?;
        Ok(result)
    }

    pub(super) fn current_run_graph_node_in_snapshot(
        connection: &dyn Database,
        run_id: &str,
        node_id: &str,
    ) -> Result<Option<RunGraphNode>, WorkflowStoreError> {
        super::validate_id("node_id", node_id)?;
        let Some(current) = graph_revision(connection, run_id)? else {
            return Ok(None);
        };
        let rows = super::database::rows(
            connection,
            &connection
                .select("workflow_run_graph_nodes")
                .columns(&["revision", "retired_at_revision"])
                .where_eq("run_id", run_id)
                .where_eq("node_id", node_id)
                .sort("revision", SortDirection::Desc)
                .limit(1),
        )?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let revision = super::database::unsigned(row, "revision")?;
        let retired = optional_graph_unsigned(row, "retired_at_revision")?;
        if revision > current || retired.is_some_and(|end| end > current || end <= revision) {
            return Err(WorkflowStoreError::InvalidData(
                "node revision exceeds committed graph revision".to_string(),
            ));
        }
        if retired.is_some() {
            return Ok(None);
        }
        Self::node_revision(connection, run_id, node_id, revision)
    }

    /// Read a bounded page of current nodes at an expected graph revision.
    ///
    /// Resume with the last returned node ID and the same expected revision. A
    /// concurrent graph edit invalidates that cursor rather than mixing plans.
    /// Each page contains at most 100 nodes and uses one read snapshot.
    ///
    /// # Errors
    /// Returns an error for invalid cursors or limits, missing or changed graph
    /// state, invalid executable data, or database failures.
    pub fn current_run_graph_nodes(
        &self,
        run_id: &str,
        expected_revision: u64,
        after_node_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RunGraphNode>, WorkflowStoreError> {
        Self::validate_graph_page_request(
            run_id,
            Some(expected_revision),
            after_node_id,
            None,
            limit,
        )?;
        let transaction = super::database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let result = Self::current_run_graph_nodes_in(
            transaction.as_ref(),
            run_id,
            expected_revision,
            after_node_id,
            limit,
        )?;
        super::database::commit(transaction)?;
        Ok(result)
    }

    fn current_run_graph_nodes_in_snapshot(
        transaction: &dyn Database,
        run_id: &str,
        expected_revision: u64,
        after_node_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RunGraphNode>, WorkflowStoreError> {
        if limit == 0 || expected_revision == 0 {
            return Err(WorkflowStoreError::InvalidData(
                "graph page limit and expected revision must be positive".to_string(),
            ));
        }
        if let Some(cursor) = after_node_id {
            super::validate_id("node cursor", cursor)?;
        }
        if graph_revision(transaction, run_id)? != Some(expected_revision) {
            return Err(WorkflowStoreError::InvalidData(
                "graph page expected revision does not match committed graph".to_string(),
            ));
        }
        let mut nodes = Vec::new();
        let mut cursor = after_node_id.map(str::to_string);
        for _ in 0..limit.min(GRAPH_PAGE_LIMIT) {
            let mut query = transaction
                .select("workflow_run_graph_nodes")
                .project_as(
                    switchy_database::query::bounded_text(
                        switchy_database::query::identifier("node_id"),
                        super::MAX_ID_BYTES as u64,
                        DatabaseValue::Null,
                    ),
                    "node_id",
                )
                .where_eq("run_id", run_id)
                .where_eq("retired_at_revision", DatabaseValue::Null)
                .sorts(vec![switchy_database::query::sort(
                    switchy_database::query::qualified_column(
                        "workflow_run_graph_nodes",
                        "node_id",
                    ),
                    SortDirection::Asc,
                )])
                .limit(1);
            query = if let Some(cursor) = cursor.as_deref() {
                query.where_gt("node_id", cursor)
            } else {
                query.where_gte("node_id", "")
            };
            let rows = super::database::rows(transaction, &query)?;
            let id = rows
                .first()
                .map(|row| graph_stored_identity(row, "node_id"))
                .transpose()?;
            let Some(id) = id else {
                break;
            };
            nodes.push(
                Self::current_run_graph_node_in_snapshot(transaction, run_id, &id)?.ok_or_else(
                    || WorkflowStoreError::InvalidData("graph page node is missing".to_string()),
                )?,
            );
            cursor = Some(id);
        }
        Ok(nodes)
    }

    /// Read one initial node revision with its admitted entry and exit roles.
    ///
    /// Missing runs or nodes return `None`. This bounded lookup never repairs state.
    ///
    /// # Errors
    /// Returns an error for invalid identities, missing or unsupported graph state,
    /// corrupt node data, oversized payloads, or database failures.
    pub fn run_graph_node_record(
        &self,
        run_id: &str,
        node_id: &str,
    ) -> Result<Option<RunGraphNode>, WorkflowStoreError> {
        let transaction = super::database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let result = Self::run_graph_node_record_in(transaction.as_ref(), run_id, node_id)?;
        super::database::commit(transaction)?;
        Ok(result)
    }

    /// Read one exact node from the initial admitted run graph.
    ///
    /// # Errors
    /// Returns an error for missing or unsupported graph state, inconsistent node identity,
    /// invalid/oversized payloads, or database failures. A missing run or node returns `None`.
    pub fn run_graph_node(
        &self,
        run_id: &str,
        node_id: &str,
    ) -> Result<Option<NodeDefinition>, WorkflowStoreError> {
        let transaction = super::database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let node = Self::run_graph_node_in(transaction.as_ref(), run_id, node_id)?;
        super::database::commit(transaction)?;
        Ok(node)
    }

    /// Read a bounded page of initial edges without loading the full graph.
    ///
    /// # Errors
    /// Returns an error for unsupported revisions, invalid graph relationships or payloads,
    /// invalid identities, zero limits, or database failures. Missing runs return an empty page.
    pub fn run_graph_edges(
        &self,
        run_id: &str,
        after_edge_id: Option<u64>,
        limit: usize,
    ) -> Result<Vec<RunGraphEdge>, WorkflowStoreError> {
        let transaction = super::database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let result = Self::run_graph_edges_in(transaction.as_ref(), run_id, after_edge_id, limit)?;
        super::database::commit(transaction)?;
        Ok(result)
    }

    /// Read one exact edge from the initial run graph by its stable identity.
    ///
    /// Missing runs or edges return `None`.
    /// # Errors
    /// Returns an error for invalid identities, unsupported revisions, damaged edge
    /// relationships or payloads, or database failures.
    pub fn run_graph_edge(
        &self,
        run_id: &str,
        edge_id: u64,
    ) -> Result<Option<RunGraphEdge>, WorkflowStoreError> {
        let edge_id = i64::try_from(edge_id).map_err(|_| {
            WorkflowStoreError::InvalidData("edge identity exceeds storage range".to_string())
        })?;
        let transaction = super::database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let mut edges =
            Self::graph_edge_page(transaction.as_ref(), run_id, None, None, Some(edge_id), 1)?;
        super::database::commit(transaction)?;
        Ok(edges.pop())
    }

    /// Read an immutable edge revision from a committed run graph.
    ///
    /// This inspection does not authorize dispatch. Missing runs or revisions
    /// return `None`; endpoint identities must exist in the same read snapshot.
    /// An existing caller-owned transaction is reused and remains owned by the
    /// caller on both success and failure.
    ///
    /// # Errors
    /// Returns an error for invalid identities or revisions, uncommitted data,
    /// malformed or oversized payloads, inconsistent endpoints, or database failures.
    pub fn run_graph_edge_revision(
        &self,
        run_id: &str,
        edge_id: u64,
        revision: u64,
    ) -> Result<Option<RunGraphEdge>, WorkflowStoreError> {
        Self::validate_edge_revision_request(run_id, edge_id, revision)?;
        let transaction = super::database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let result =
            Self::run_graph_edge_revision_in(transaction.as_ref(), run_id, edge_id, revision)?;
        super::database::commit(transaction)?;
        Ok(result)
    }

    fn validate_edge_revision_request(
        run_id: &str,
        edge_id: u64,
        revision: u64,
    ) -> Result<i64, WorkflowStoreError> {
        super::validate_id("run_id", run_id)?;
        let edge_id = i64::try_from(edge_id).map_err(|_| {
            WorkflowStoreError::InvalidData("edge identity exceeds storage range".to_string())
        })?;
        if revision == 0 || i64::try_from(revision).is_err() {
            return Err(WorkflowStoreError::InvalidData(
                "invalid edge revision".to_string(),
            ));
        }
        Ok(edge_id)
    }

    fn run_graph_edge_revision_in_snapshot(
        connection: &dyn Database,
        run_id: &str,
        edge_id: u64,
        revision: u64,
    ) -> Result<Option<RunGraphEdge>, WorkflowStoreError> {
        let edge_id = Self::validate_edge_revision_request(run_id, edge_id, revision)?;
        let transaction = connection;
        let Some(current) = graph_revision(transaction, run_id)? else {
            return Ok(None);
        };
        if revision > current {
            return Err(WorkflowStoreError::InvalidData(
                "edge revision exceeds committed graph revision".to_string(),
            ));
        }
        let rows = super::database::rows(
            transaction,
            &transaction
                .select("workflow_run_graph_edges")
                .project_as(
                    switchy_database::query::bounded_text(
                        switchy_database::query::identifier("edge_json"),
                        super::MAX_INLINE_JSON_BYTES as u64,
                        DatabaseValue::Null,
                    ),
                    "edge_json",
                )
                .project_as(
                    switchy_database::query::bounded_text(
                        switchy_database::query::identifier("source_node_id"),
                        super::MAX_ID_BYTES as u64,
                        DatabaseValue::Null,
                    ),
                    "source_node_id",
                )
                .project_as(
                    switchy_database::query::bounded_text(
                        switchy_database::query::identifier("target_node_id"),
                        super::MAX_ID_BYTES as u64,
                        DatabaseValue::Null,
                    ),
                    "target_node_id",
                )
                .where_eq("run_id", run_id)
                .where_eq("edge_id", DatabaseValue::Int64(edge_id))
                .where_eq(
                    "revision",
                    DatabaseValue::Int64(i64::try_from(revision).map_err(|_| {
                        WorkflowStoreError::InvalidData("invalid graph revision".to_string())
                    })?),
                )
                .limit(1),
        )?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let payload = super::database::text(row, "edge_json").map_err(|_| {
            WorkflowStoreError::InvalidData(
                "workflow graph edge payload is invalid or oversized".into(),
            )
        })?;
        let source = graph_endpoint_identity(row, "source_node_id")?;
        let target = graph_endpoint_identity(row, "target_node_id")?;

        let edge: EdgeDefinition = serde_json::from_str(&payload)?;
        if edge.from != source || edge.to != target {
            return Err(WorkflowStoreError::InvalidData(
                "edge endpoint identity mismatch".to_string(),
            ));
        }
        Self::validate_edge_endpoints_in_snapshot(transaction, run_id, revision, &source, &target)?;
        Ok(Some(RunGraphEdge {
            revision,
            edge_id: u64::try_from(edge_id).map_err(|_| {
                WorkflowStoreError::InvalidData("invalid edge identity".to_string())
            })?,
            edge,
        }))
    }

    fn validate_edge_endpoints_in_snapshot(
        transaction: &dyn Database,
        run_id: &str,
        revision: u64,
        source: &str,
        target: &str,
    ) -> Result<(), WorkflowStoreError> {
        // A self-loop has one endpoint identity; validate it once in this snapshot.
        let endpoints = [source, target];
        for id in &endpoints[..if source == target { 1 } else { 2 }] {
            super::validate_id("edge endpoint", id)?;
            let rows = super::database::rows(
                transaction,
                &transaction
                    .select("workflow_run_graph_nodes")
                    .columns(&["revision", "retired_at_revision"])
                    .where_eq("run_id", run_id)
                    .where_eq("node_id", *id)
                    .where_lte(
                        "revision",
                        DatabaseValue::Int64(i64::try_from(revision).map_err(|_| {
                            WorkflowStoreError::InvalidData("invalid graph revision".to_string())
                        })?),
                    )
                    .sort("revision", SortDirection::Desc)
                    .limit(1),
            )?;
            let row = rows.first().ok_or_else(|| {
                WorkflowStoreError::InvalidData("edge endpoint is missing at revision".to_string())
            })?;
            let endpoint_revision = super::database::unsigned(row, "revision")?;
            let retired = optional_graph_unsigned(row, "retired_at_revision")?;
            if retired.is_some_and(|end| end <= revision) {
                return Err(WorkflowStoreError::InvalidData(
                    "edge endpoint was retired at edge revision".to_string(),
                ));
            }
            Self::node_revision(transaction, run_id, id, endpoint_revision)?.ok_or_else(|| {
                WorkflowStoreError::InvalidData("edge endpoint is missing".to_string())
            })?;
        }
        Ok(())
    }

    /// Read the latest committed edge and validate its current endpoints.
    ///
    /// Missing runs or edges return `None`. This bounded read does not authorize
    /// dispatch. Caller-owned transactions are reused and left to the caller.
    ///
    /// # Errors
    /// Returns an error for invalid identities, uncommitted revisions, damaged
    /// graph metadata, malformed payloads, invalid endpoints, or database failures.
    pub fn current_run_graph_edge(
        &self,
        run_id: &str,
        edge_id: u64,
    ) -> Result<Option<RunGraphEdge>, WorkflowStoreError> {
        super::validate_id("run_id", run_id)?;
        i64::try_from(edge_id).map_err(|_| {
            WorkflowStoreError::InvalidData("edge identity exceeds storage range".to_string())
        })?;
        let transaction = super::database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let result = Self::current_run_graph_edge_in(transaction.as_ref(), run_id, edge_id)?;
        super::database::commit(transaction)?;
        Ok(result)
    }

    fn current_run_graph_edge_in_snapshot(
        connection: &dyn Database,
        run_id: &str,
        edge_id: u64,
        validated_endpoints: &mut BTreeSet<String>,
    ) -> Result<Option<RunGraphEdge>, WorkflowStoreError> {
        let id = i64::try_from(edge_id).map_err(|_| {
            WorkflowStoreError::InvalidData("edge identity exceeds storage range".to_string())
        })?;
        let Some(current) = graph_revision(connection, run_id)? else {
            return Ok(None);
        };
        let rows = super::database::rows(
            connection,
            &connection
                .select("workflow_run_graph_edges")
                .columns(&["revision", "retired_at_revision"])
                .where_eq("run_id", run_id)
                .where_eq("edge_id", DatabaseValue::Int64(id))
                .sort("revision", SortDirection::Desc)
                .limit(1),
        )?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let revision = super::database::unsigned(row, "revision")?;
        let retired = optional_graph_unsigned(row, "retired_at_revision")?;
        if revision > current || retired.is_some_and(|end| end > current || end <= revision) {
            return Err(WorkflowStoreError::InvalidData(
                "edge revision exceeds committed graph revision".to_string(),
            ));
        }
        if retired.is_some() {
            return Ok(None);
        }
        let edge =
            Self::run_graph_edge_revision_in_snapshot(connection, run_id, edge_id, revision)?
                .ok_or_else(|| {
                    WorkflowStoreError::InvalidData("graph edge is missing".to_string())
                })?;
        for endpoint in [&edge.edge.from, &edge.edge.to] {
            if validated_endpoints.contains(endpoint) {
                continue;
            }
            Self::current_run_graph_node_in_snapshot(connection, run_id, endpoint)?.ok_or_else(
                || {
                    WorkflowStoreError::InvalidData(
                        "current graph edge endpoint is missing".to_string(),
                    )
                },
            )?;
            validated_endpoints.insert(endpoint.clone());
        }
        Ok(Some(edge))
    }

    /// Read current edges in bounded identity order at an expected graph revision.
    ///
    /// # Errors
    /// Returns an error for invalid limits/cursors, changed or missing graph state,
    /// uncommitted revisions, invalid edge data, or database failures.
    pub fn current_run_graph_edges(
        &self,
        run_id: &str,
        expected_revision: u64,
        after_edge_id: Option<u64>,
        limit: usize,
    ) -> Result<Vec<RunGraphEdge>, WorkflowStoreError> {
        Self::validate_graph_page_request(
            run_id,
            Some(expected_revision),
            None,
            after_edge_id,
            limit,
        )?;
        let transaction = super::database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let result = Self::current_run_graph_edges_in(
            transaction.as_ref(),
            run_id,
            expected_revision,
            after_edge_id,
            limit,
        )?;
        super::database::commit(transaction)?;
        Ok(result)
    }

    /// Read a bounded page of current edges incident to one source or target.
    ///
    /// `outgoing` selects source edges; otherwise target edges are returned.
    /// This query does not authorize execution or resolve activation bindings.
    ///
    /// # Errors
    /// Returns an error for invalid identities, limits, revision conflicts,
    /// inconsistent endpoints, malformed edge revisions, or database failures.
    pub fn current_run_graph_incident_edges(
        &self,
        run_id: &str,
        expected_revision: u64,
        node_id: &str,
        outgoing: bool,
        after_edge_id: Option<u64>,
        limit: usize,
    ) -> Result<Vec<RunGraphEdge>, WorkflowStoreError> {
        super::validate_id("node_id", node_id)?;
        Self::validate_graph_page_request(
            run_id,
            Some(expected_revision),
            None,
            after_edge_id,
            limit,
        )?;
        let transaction = super::database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let result = Self::current_run_graph_incident_edges_in(
            transaction.as_ref(),
            run_id,
            expected_revision,
            node_id,
            outgoing,
            after_edge_id,
            limit,
        )?;
        super::database::commit(transaction)?;
        Ok(result)
    }

    pub(super) fn current_run_graph_edges_in_snapshot(
        connection: &dyn Database,
        run_id: &str,
        expected_revision: u64,
        after_edge_id: Option<u64>,
        limit: usize,
        endpoint: Option<EdgeEndpoint<'_>>,
    ) -> Result<Vec<RunGraphEdge>, WorkflowStoreError> {
        let mut cursor = edge_cursor(after_edge_id)?;
        if limit == 0 || expected_revision == 0 {
            return Err(WorkflowStoreError::InvalidData(
                "invalid graph page limit or revision".to_string(),
            ));
        }
        let transaction = connection;
        if graph_revision(transaction, run_id)? != Some(expected_revision) {
            return Err(WorkflowStoreError::InvalidData(
                "graph page revision conflict".to_string(),
            ));
        }
        let mut edges = Vec::new();
        // Current endpoint validity is shared only within this bounded read snapshot.
        let mut validated_endpoints = BTreeSet::new();
        for _ in 0..limit.min(GRAPH_PAGE_LIMIT) {
            let inclusive = after_edge_id.is_none() && edges.is_empty();
            let mut query = transaction
                .select("workflow_run_graph_edges")
                .columns(&["edge_id"])
                .where_eq("run_id", run_id)
                .where_eq("retired_at_revision", DatabaseValue::Null)
                .sort("edge_id", SortDirection::Asc)
                .limit(1);
            query = if inclusive {
                query.where_gte("edge_id", DatabaseValue::Int64(cursor))
            } else {
                query.where_gt("edge_id", DatabaseValue::Int64(cursor))
            };
            query = match endpoint {
                Some(EdgeEndpoint::Source(id)) => query.where_eq("source_node_id", id),
                Some(EdgeEndpoint::Target(id)) => query.where_eq("target_node_id", id),
                None => query,
            };
            let rows = super::database::rows(transaction, &query)?;
            let row = rows
                .first()
                .map(|row| super::database::unsigned(row, "edge_id"))
                .transpose()?;
            let Some(id) = row else {
                break;
            };
            let edge = Self::current_run_graph_edge_in_snapshot(
                connection,
                run_id,
                id,
                &mut validated_endpoints,
            )?
            .ok_or_else(|| WorkflowStoreError::InvalidData("graph edge is missing".to_string()))?;
            edges.push(edge);
            cursor = i64::try_from(id).map_err(|_| {
                WorkflowStoreError::InvalidData("invalid edge identity".to_string())
            })?;
        }
        Ok(edges)
    }

    /// Read a bounded page of initial edges targeting one node, ordered by edge identity.
    ///
    /// Missing runs or targets with no incoming edges return an empty page.
    /// # Errors
    /// Returns an error for invalid identities or cursors, zero limits, unsupported graph revisions,
    /// damaged edge relationships or payloads, or database failures.
    pub fn run_graph_incoming_edges(
        &self,
        run_id: &str,
        target_node_id: &str,
        after_edge_id: Option<u64>,
        limit: usize,
    ) -> Result<Vec<RunGraphEdge>, WorkflowStoreError> {
        super::validate_id("target_node_id", target_node_id)?;
        let transaction = super::database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let result = Self::run_graph_incoming_edges_in(
            transaction.as_ref(),
            run_id,
            target_node_id,
            after_edge_id,
            limit,
        )?;
        super::database::commit(transaction)?;
        Ok(result)
    }

    /// Read a bounded page of initial edges leaving a node, ordered by edge identity.
    ///
    /// Missing runs or sources with no outgoing edges return an empty page.
    /// # Errors
    /// Returns an error for invalid identities or cursors, zero limits, unsupported graph revisions,
    /// damaged edge relationships or payloads, or database failures.
    pub fn run_graph_outgoing_edges(
        &self,
        run_id: &str,
        source_node_id: &str,
        after_edge_id: Option<u64>,
        limit: usize,
    ) -> Result<Vec<RunGraphEdge>, WorkflowStoreError> {
        super::validate_id("source_node_id", source_node_id)?;
        let transaction = super::database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let result = Self::run_graph_outgoing_edges_in(
            transaction.as_ref(),
            run_id,
            source_node_id,
            after_edge_id,
            limit,
        )?;
        super::database::commit(transaction)?;
        Ok(result)
    }

    fn graph_edge_page(
        connection: &dyn Database,
        run_id: &str,
        endpoint: Option<EdgeEndpoint<'_>>,
        after_edge_id: Option<u64>,
        exact_edge_id: Option<i64>,
        limit: usize,
    ) -> Result<Vec<RunGraphEdge>, WorkflowStoreError> {
        Self::validate_graph_page_request(run_id, Some(1), None, after_edge_id, limit)?;
        let edges = Self::graph_edge_page_in_snapshot(
            connection,
            run_id,
            endpoint,
            after_edge_id,
            exact_edge_id,
            limit,
        )?;
        Ok(edges)
    }

    fn graph_edge_page_in_snapshot(
        connection: &dyn Database,
        run_id: &str,
        endpoint: Option<EdgeEndpoint<'_>>,
        after_edge_id: Option<u64>,
        exact_edge_id: Option<i64>,
        limit: usize,
    ) -> Result<Vec<RunGraphEdge>, WorkflowStoreError> {
        Self::validate_graph_page_request(run_id, Some(1), None, after_edge_id, limit)?;
        match graph_revision(connection, run_id)? {
            None => return Ok(Vec::new()),
            Some(1) => {}
            _ => {
                return Err(WorkflowStoreError::InvalidData(
                    "initial graph inspection does not support revised graphs".to_string(),
                ));
            }
        }
        let mut query = connection
            .select("workflow_run_graph_edges")
            .columns(&["edge_id", "revision"])
            .where_eq("run_id", run_id)
            .sort("edge_id", SortDirection::Asc)
            .sort("revision", SortDirection::Asc)
            .limit(limit.min(GRAPH_PAGE_LIMIT));
        let cursor = edge_cursor(after_edge_id)?;
        query = if after_edge_id.is_some() {
            query.where_gt("edge_id", DatabaseValue::Int64(cursor))
        } else {
            query.where_gte("edge_id", DatabaseValue::Int64(cursor))
        };
        if let Some(id) = exact_edge_id {
            query = query.where_eq("edge_id", DatabaseValue::Int64(id));
        }
        query = match endpoint {
            Some(EdgeEndpoint::Source(id)) => query
                .indexed_by("workflow_run_graph_edges_source")
                .where_eq("source_node_id", id),
            Some(EdgeEndpoint::Target(id)) => query
                .indexed_by("workflow_run_graph_edges_target")
                .where_eq("target_node_id", id),
            None => query,
        };
        let rows = super::database::rows(connection, &query)?;
        let mut edges = Vec::new();
        let mut validated_endpoints = BTreeSet::new();
        for row in rows {
            if super::database::unsigned(&row, "revision")? != 1 {
                return Err(WorkflowStoreError::InvalidData(
                    "workflow graph edge revision is invalid".to_string(),
                ));
            }
            let id = super::database::unsigned(&row, "edge_id")?;
            let edge = Self::run_graph_edge_in(connection, run_id, id)?.ok_or_else(|| {
                WorkflowStoreError::InvalidData("workflow graph edge is missing".to_string())
            })?;
            Self::validate_initial_edge_endpoints(
                connection,
                run_id,
                &edge.edge,
                &mut validated_endpoints,
            )?;
            edges.push(edge);
        }
        Ok(edges)
    }

    fn validate_initial_edge_endpoints(
        connection: &dyn Database,
        run_id: &str,
        edge: &EdgeDefinition,
        validated: &mut BTreeSet<String>,
    ) -> Result<(), WorkflowStoreError> {
        for endpoint in [&edge.from, &edge.to] {
            if validated.contains(endpoint) {
                continue;
            }
            initial_node_record_in_snapshot(connection, run_id, endpoint)
                .map_err(|_| {
                    WorkflowStoreError::InvalidData(
                        "workflow graph edge relationships are inconsistent".into(),
                    )
                })?
                .ok_or_else(|| {
                    WorkflowStoreError::InvalidData(
                        "workflow graph edge relationships are inconsistent".to_string(),
                    )
                })?;
            validated.insert(endpoint.clone());
        }
        Ok(())
    }

    /// Read the admitted graph revision without replay or repair.
    ///
    /// # Errors
    /// Returns an error for an invalid run identity, missing graph for an existing run,
    /// corrupt revision, or database failure.
    pub fn run_graph_revision(&self, run_id: &str) -> Result<Option<u64>, WorkflowStoreError> {
        graph_revision(&self.connection, run_id)
    }

    /// Read a bounded page of initial graph nodes ordered by stable node identity.
    ///
    /// This initial materialization API does not authorize graph edits or change scheduling.
    /// # Errors
    /// Returns an error for invalid identities, zero limits, missing/corrupt graph data, or database failure.
    pub fn run_graph_nodes(
        &self,
        run_id: &str,
        after_node_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RunGraphNode>, WorkflowStoreError> {
        Self::validate_graph_page_request(run_id, Some(1), after_node_id, None, limit)?;
        let transaction = super::database::begin(
            &self.connection,
            switchy_database::TransactionMode::Deferred,
        )?;
        let result = Self::run_graph_nodes_in(transaction.as_ref(), run_id, after_node_id, limit)?;
        super::database::commit(transaction)?;
        Ok(result)
    }

    fn initial_graph_nodes_in_snapshot(
        connection: &dyn Database,
        run_id: &str,
        after_node_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RunGraphNode>, WorkflowStoreError> {
        Self::validate_graph_page_request(run_id, Some(1), after_node_id, None, limit)?;
        match graph_revision(connection, run_id)? {
            None => return Ok(Vec::new()),
            Some(1) => {}
            _ => {
                return Err(WorkflowStoreError::InvalidData(
                    "initial graph inspection does not support revised graphs".to_string(),
                ));
            }
        }
        let mut query = connection
            .select("workflow_run_graph_nodes")
            .project_as(
                switchy_database::query::bounded_text(
                    switchy_database::query::identifier("node_id"),
                    super::MAX_ID_BYTES as u64,
                    DatabaseValue::Null,
                ),
                "node_id",
            )
            .where_eq("run_id", run_id)
            .sorts(vec![switchy_database::query::sort(
                switchy_database::query::qualified_column("workflow_run_graph_nodes", "node_id"),
                SortDirection::Asc,
            )])
            .sort("revision", SortDirection::Asc)
            .limit(limit.min(GRAPH_PAGE_LIMIT));
        query = if let Some(cursor) = after_node_id {
            query.where_gt("node_id", cursor)
        } else {
            query.where_gte("node_id", "")
        };
        let rows = super::database::rows(connection, &query)?;
        rows.iter()
            .map(|row| {
                let id = graph_stored_identity(row, "node_id")?;
                initial_node_record_in_snapshot(connection, run_id, &id)?.ok_or_else(|| {
                    WorkflowStoreError::InvalidData("graph node is missing".to_string())
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod publication_tests {
    use super::*;

    fn edge_publication_fixture() -> switchy_database::rusqlite::RusqliteDatabase {
        let connection = graph_test_database();
        graph_test_table(
            &connection,
            "workflow_run_graph_edges",
            &[
                ("run_id", false),
                ("edge_id", true),
                ("revision", true),
                ("source_node_id", false),
                ("target_node_id", false),
                ("edge_json", false),
                ("retired_at_revision", true),
            ],
        );
        graph_test_table(
            &connection,
            "workflow_graph_edit_edges",
            &[
                ("run_id", false),
                ("mutation_id", false),
                ("edge_id", true),
                ("edge_json", false),
            ],
        );
        for (id, source, target) in [(1, "a", "b"), (2, "b", "c")] {
            crate::database::block_on(
                connection
                    .insert("workflow_run_graph_edges")
                    .value("run_id", "run")
                    .value("edge_id", DatabaseValue::Int64(id))
                    .value("revision", DatabaseValue::Int64(1))
                    .value("source_node_id", source)
                    .value("target_node_id", target)
                    .value("edge_json", "{}")
                    .value("retired_at_revision", DatabaseValue::Null)
                    .execute(&connection),
            )
            .unwrap();
        }
        connection
    }

    #[test]
    fn edge_publication_replaces_adds_removes_and_rolls_back() {
        let connection = edge_publication_fixture();
        for (id, endpoints) in [(1, Some(("a", "c"))), (2, None), (3, Some(("c", "d")))] {
            let payload = endpoints
                .map(|(source, target)| {
                    serde_json::to_string(&EdgeDefinition {
                        from: source.to_string(),
                        to: target.to_string(),
                        kind: bcode_workflow::EdgeKind::Direct,
                        transform: None,
                    })
                    .unwrap()
                })
                .map_or(DatabaseValue::Null, DatabaseValue::String);
            crate::database::block_on(
                connection
                    .insert("workflow_graph_edit_edges")
                    .value("run_id", "run")
                    .value("mutation_id", "edit")
                    .value("edge_id", DatabaseValue::Int64(id))
                    .value("edge_json", payload)
                    .execute(&connection),
            )
            .unwrap();
        }
        for commit in [false, true] {
            let transaction = super::super::database::begin(
                &connection,
                switchy_database::TransactionMode::Immediate,
            )
            .unwrap();
            publish_candidate_edges(transaction.as_ref(), "run", "edit", 2).expect("publish edges");
            let rows = super::super::database::rows(
                transaction.as_ref(),
                &transaction
                    .select("workflow_run_graph_edges")
                    .columns(&["edge_id", "revision", "source_node_id", "target_node_id"])
                    .where_eq("retired_at_revision", DatabaseValue::Null)
                    .sort("edge_id", SortDirection::Asc),
            )
            .unwrap();
            let edges = rows
                .iter()
                .map(|row| {
                    Ok((
                        super::super::database::unsigned(row, "edge_id")?,
                        super::super::database::unsigned(row, "revision")?,
                        super::super::database::text(row, "source_node_id")?,
                        super::super::database::text(row, "target_node_id")?,
                    ))
                })
                .collect::<Result<Vec<_>, WorkflowStoreError>>()
                .unwrap();
            assert_eq!(
                edges,
                vec![
                    (1, 2, "a".into(), "c".into()),
                    (3, 2, "c".into(), "d".into())
                ]
            );
            if commit {
                super::super::database::commit(transaction).unwrap();
            } else {
                crate::database::block_on(transaction.rollback()).unwrap();
            }
        }
        let rows = super::super::database::rows(
            &connection,
            &connection
                .select("workflow_run_graph_edges")
                .project_as(switchy_database::query::count_all(), "count")
                .where_eq("revision", DatabaseValue::Int64(1))
                .where_eq("retired_at_revision", DatabaseValue::Int64(2)),
        )
        .unwrap();
        assert_eq!(
            super::super::database::unsigned(&rows[0], "count").unwrap(),
            2
        );
    }
}
