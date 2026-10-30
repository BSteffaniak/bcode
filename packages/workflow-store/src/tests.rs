//! Workflow persistence behavioral and compatibility acceptance.
use super::output::{
    TransformRunField, current_dependency_output_value, materialize_direct_successors,
    parallel_join_members, retained_activation_ready, run_has_unfinished_activations,
    skip_branch_nodes, successor_input_source_identity,
};
use super::*;
use crate::database::block_on;
use bcode_workflow::{Step, WorkflowBuilder};
use std::collections::BTreeMap;
use switchy_database::query::{
    Expression, FilterableQuery, cast_blob, cast_text, delete, insert, select, update, zero_blob,
};
use switchy_database::schema::{SchemaComparison as C, SchemaExpression as E, create_trigger};
use switchy_database::{Database, DatabaseValue, TransactionMode};

#[derive(Clone)]
enum FixtureStorageValue {
    Null,
    Text(String),
    BlobText(String),
}

impl FixtureStorageValue {
    fn expression(&self) -> Box<dyn Expression> {
        match self {
            Self::Null => DatabaseValue::Null.into(),
            Self::Text(text) => text.clone().into(),
            Self::BlobText(text) => cast_blob(text.clone()).into(),
        }
    }
}

fn fixture_link_row(
    ids: [&str; 5],
    attempt: u32,
    version: u32,
    target: &str,
    depth: u32,
) -> switchy_database::Row {
    switchy_database::Row {
        columns: vec![
            ("root_run_id".into(), ids[0].into()),
            ("parent_run_id".into(), ids[1].into()),
            ("parent_node_id".into(), ids[2].into()),
            ("parent_activation_id".into(), ids[3].into()),
            (
                "parent_attempt".into(),
                DatabaseValue::Int64(i64::from(attempt)),
            ),
            ("child_run_id".into(), ids[4].into()),
            ("version".into(), DatabaseValue::Int64(i64::from(version))),
            ("target_json".into(), target.into()),
            ("depth".into(), DatabaseValue::Int64(i64::from(depth))),
            ("created_at_ms".into(), DatabaseValue::Int64(0)),
        ],
    }
}

fn fixture_attempt_summary(
    column: &str,
    value: Box<dyn Expression>,
) -> Result<RawAttemptSummary, WorkflowStoreError> {
    let temp = tempfile::tempdir().expect("attempt decoder fixture directory");
    let db = fixture_open(&temp.path().join("attempt.db"), false);
    let text_columns = [
        "run_id",
        "node_id",
        "activation_id",
        "dispatch_identity",
        "side_effect",
        "status",
    ];
    let mut columns: Vec<_> = text_columns
        .into_iter()
        .map(|name| switchy_database::schema::Column {
            name: name.into(),
            nullable: true,
            auto_increment: false,
            data_type: switchy_database::schema::DataType::Text,
            default: None,
        })
        .collect();
    columns.extend(
        [
            "attempt",
            "has_receipt",
            "prepared_at_ms",
            "admitted_at_ms",
            "terminal_at_ms",
        ]
        .into_iter()
        .map(|name| switchy_database::schema::Column {
            name: name.into(),
            nullable: true,
            auto_increment: false,
            data_type: switchy_database::schema::DataType::BigInt,
            default: None,
        }),
    );
    block_on(
        switchy_database::schema::create_table("attempt_fixture")
            .columns(columns)
            .execute(&db),
    )
    .expect("decoder fixture table");
    block_on(
        insert("attempt_fixture")
            .value("run_id", "run")
            .value("node_id", "node")
            .value("activation_id", "activation")
            .value("attempt", 1)
            .value("dispatch_identity", "dispatch")
            .value("side_effect", "read_only")
            .value("status", "prepared")
            .value("has_receipt", 0)
            .value("prepared_at_ms", 1)
            .value("admitted_at_ms", DatabaseValue::Null)
            .value("terminal_at_ms", DatabaseValue::Null)
            .execute(&db),
    )
    .expect("decoder fixture row");
    block_on(db.exec_update_count(&(update("attempt_fixture").value(column, value))))
        .expect("decoder fixture corruption");
    let before = fixture_changes(&db);
    let mut query = select("attempt_fixture").columns(&[
        "attempt",
        "has_receipt",
        "prepared_at_ms",
        "admitted_at_ms",
        "terminal_at_ms",
    ]);
    for name in text_columns {
        query = query.project_as(
            switchy_database::query::bounded_text(
                switchy_database::query::identifier(name),
                MAX_ID_BYTES as u64,
                DatabaseValue::Null,
            ),
            name,
        );
    }
    // Typed projections replace the column list; retain numeric fields explicitly.
    for name in [
        "attempt",
        "has_receipt",
        "prepared_at_ms",
        "admitted_at_ms",
        "terminal_at_ms",
    ] {
        query = query.project(switchy_database::query::identifier(name));
    }
    let rows = database::rows(&db, &query)?;
    let result = attempt_summary_from_row(rows.first().expect("attempt decoder fixture row"));
    assert_eq!(fixture_changes(&db), before);
    result
}

fn fixture_copy_graph_nodes(db: &dyn Database, run_id: &str, revision: i64) {
    fixture_copy_graph_nodes_scoped(db, run_id, None, revision);
}

fn fixture_copy_graph_nodes_scoped(
    db: &dyn Database,
    run_id: &str,
    node_id: Option<&str>,
    revision: i64,
) {
    let query = select("workflow_run_graph_nodes")
        .columns(&["run_id", "node_id", "node_json", "is_entry", "is_exit"])
        .where_eq("run_id", run_id);
    let query = if let Some(node_id) = node_id {
        query.where_eq("node_id", node_id)
    } else {
        query
    };
    let rows = fixture_rows(db, &query);
    for row in rows {
        block_on(
            insert("workflow_run_graph_nodes")
                .value("run_id", fixture_text(&row, "run_id"))
                .value("node_id", fixture_text(&row, "node_id"))
                .value("revision", revision)
                .value("node_json", fixture_text(&row, "node_json"))
                .value("is_entry", fixture_unsigned(&row, "is_entry"))
                .value("is_exit", fixture_unsigned(&row, "is_exit"))
                .execute(db),
        )
        .expect("copied graph node fixture");
    }
}

fn fixture_open(path: &std::path::Path, foreign_keys: bool) -> impl Database + use<> {
    database::open(
        path,
        switchy_database_connection::SqliteAccess::ReadWriteCreate,
        std::time::Duration::from_secs(5),
        foreign_keys,
    )
    .expect("fixture database")
}

// Recreate the pre-graph activation shape together with its historical constraints.
// Foreign-key enforcement is disabled only around the owned fixture transaction;
// dependent rows remain intact and the caller's declared setting is restored.
fn fixture_historical_activations(
    db: &dyn Database,
    foreign_keys: bool,
) -> Result<(), WorkflowStoreError> {
    block_on(db.sqlite_foreign_keys(false))?;
    let result = fixture_recreate_historical_activations(db);
    let restored = block_on(db.sqlite_foreign_keys(foreign_keys));
    result?;
    restored?;
    Ok(())
}

fn fixture_recreate_historical_activations(db: &dyn Database) -> Result<(), WorkflowStoreError> {
    use switchy_database::schema::{Column, DataType, create_index, create_table, drop_table};
    let transaction = database::begin(db, TransactionMode::Immediate)?;
    let db: &dyn Database = transaction.as_ref();
    let columns = [
        "run_id",
        "node_id",
        "activation_id",
        "dependency_generation",
        "input_json",
        "status",
        "output_id",
        "created_at_ms",
    ];
    let rows = database::rows(db, &select("workflow_activations").columns(&columns))?;
    block_on(drop_table("workflow_activations").execute(db))?;
    let specifications = [
        ("run_id", DataType::Text, false),
        ("node_id", DataType::Text, false),
        ("activation_id", DataType::Text, false),
        ("dependency_generation", DataType::Int, false),
        ("input_json", DataType::Text, true),
        ("status", DataType::Text, false),
        ("output_id", DataType::Text, true),
        ("created_at_ms", DataType::Int, false),
    ];
    block_on(
        create_table("workflow_activations")
            .columns(
                specifications
                    .into_iter()
                    .map(|(name, data_type, nullable)| Column {
                        name: name.into(),
                        data_type,
                        nullable,
                        auto_increment: false,
                        default: None,
                    })
                    .collect(),
            )
            .primary_key_columns(["run_id", "node_id", "activation_id"])
            .composite_foreign_key(["run_id"], "workflow_runs", ["run_id"])
            .execute(db),
    )?;
    for row in rows {
        let mut statement = insert("workflow_activations");
        for column in columns {
            statement = statement.value(
                column,
                row.get(column).ok_or_else(|| {
                    WorkflowStoreError::InvalidData(format!("historical fixture missing {column}"))
                })?,
            );
        }
        block_on(statement.execute(db))?;
    }
    block_on(
        create_index("workflow_activations_running_node")
            .table("workflow_activations")
            .columns(vec!["run_id", "node_id", "activation_id"])
            .predicate(E::column("status").compare(C::Equal, E::Text("running".into())))
            .execute(db),
    )?;
    block_on(
        create_index("workflow_activations_identity")
            .table("workflow_activations")
            .columns(vec!["run_id", "activation_id"])
            .execute(db),
    )?;
    database::commit(transaction)
}

fn fixture_rows(
    db: &dyn Database,
    query: &switchy_database::query::SelectQuery<'_>,
) -> Vec<switchy_database::Row> {
    database::rows(db, query).expect("fixture rows")
}

fn fixture_text(row: &switchy_database::Row, column: &str) -> String {
    database::text(row, column).expect("fixture text")
}

fn fixture_unsigned(row: &switchy_database::Row, column: &str) -> u64 {
    database::unsigned(row, column).expect("fixture unsigned integer")
}

fn fixture_scalar_text(
    db: &dyn Database,
    query: &switchy_database::query::SelectQuery<'_>,
    column: &str,
) -> Result<String, WorkflowStoreError> {
    let rows = database::rows(db, query)?;
    database::text(
        rows.first()
            .ok_or_else(|| WorkflowStoreError::InvalidData("fixture scalar row missing".into()))?,
        column,
    )
}

fn fixture_scalar_unsigned(
    db: &dyn Database,
    query: &switchy_database::query::SelectQuery<'_>,
    column: &str,
) -> Result<u64, WorkflowStoreError> {
    let rows = database::rows(db, query)?;
    database::unsigned(
        rows.first()
            .ok_or_else(|| WorkflowStoreError::InvalidData("fixture scalar row missing".into()))?,
        column,
    )
}

fn fixture_changes(db: &dyn Database) -> u64 {
    block_on(db.sqlite_change_observation())
        .expect("fixture change observation")
        .total_changes
}

#[test]
fn identifier_validation_enforces_byte_bound_and_non_whitespace_content() {
    for invalid in [
        String::new(),
        " ".repeat(MAX_ID_BYTES + 1),
        "\u{2003}".repeat(MAX_ID_BYTES),
        "x".repeat(MAX_ID_BYTES + 1),
    ] {
        assert!(validate_id("test_id", &invalid).is_err());
    }
    assert!(validate_id("test_id", &"x".repeat(MAX_ID_BYTES)).is_ok());
    assert!(validate_id("test_id", " x ").is_ok());
    assert!(validate_id("test_id", "\u{2003}").is_err());
}

struct RejectPackageMutationBoundary(WorkflowPackageMutationBoundary);

impl WorkflowPackageMutationFault for RejectPackageMutationBoundary {
    fn after_boundary(
        &self,
        boundary: WorkflowPackageMutationBoundary,
        _member_id: Option<&str>,
    ) -> Result<(), WorkflowStoreError> {
        if boundary == self.0 {
            Err(WorkflowStoreError::InvalidData(
                "injected package mutation fault".to_string(),
            ))
        } else {
            Ok(())
        }
    }
}

#[allow(clippy::too_many_lines)]
fn authored_store_fixture() -> (
    AuthoredWorkflow,
    WorkflowDraft,
    bcode_workflow::WorkflowAuthoringCatalogSnapshot,
) {
    let schema = bcode_workflow::ValueSchema {
        type_name: "example.value/v1".to_string(),
        schema: serde_json::json!({
            "$schema": bcode_workflow::WORKFLOW_AUTHORING_JSON_SCHEMA_DIALECT,
            "type": "object",
            "additionalProperties": false,
            "required": ["message"],
            "properties": {"message": {"type": "string"}}
        }),
    };
    let document = WorkflowAuthoringDocument {
            schema_version: bcode_workflow::WORKFLOW_AUTHORING_DOCUMENT_VERSION,
            workflow_id: "authored/example".to_string(),
            metadata: bcode_workflow::WorkflowAuthoringMetadata {
                title: "Authored example".to_string(),
                description: Some("durable fixture".to_string()),
                labels: BTreeMap::new(),
            },
            configuration_schema: schema.clone(),
            configuration_defaults: Some(serde_json::json!({"message": "review"})),
            plugin_input_defaults: BTreeMap::new(),
            definition: WorkflowDefinition {
                schema_version: bcode_workflow::WORKFLOW_DEFINITION_SCHEMA_VERSION,
                name: "authored-example".to_string(),
                input: schema.clone(),
                output: schema.clone(),
                nodes: BTreeMap::from([(
                    "agent".to_string(),
                    bcode_workflow::NodeDefinition {
                        id: "agent".to_string(),
                        name: "Agent".to_string(),
                        kind: bcode_workflow::NodeKind::Agent,
                        dataflow: bcode_workflow::WorkflowNodeDataflowPolicy::Direct,
                        input: schema.clone(),
                        output: schema.clone(),
                        resources: Vec::new(),
                        configuration: serde_json::to_value(
                            bcode_workflow::WorkflowPromptConfiguration {
                                version: bcode_workflow::WORKFLOW_PROMPT_CONFIGURATION_VERSION,
                                worktree_directory: None,
                                activity_producer: None,
                                execution_target:
                                    bcode_workflow::PromptContextTarget::FreshIsolated,
                                agent_profile: "review".to_string(),
                                execution_parent: None,
                                provider: None,
                                model: None,
                                output: bcode_workflow::WorkflowPromptOutputPolicy::Structured {
                                    result: bcode_workflow::PromptStructuredOutputPolicy {
                                        schema,
                                        strict: true,
                                        correction: bcode_workflow::WorkflowStructuredResultCorrectionPolicy::default(),
                                    },
                                },
                                read_only: true,
                                tool_capability: bcode_workflow::WorkflowToolCapability::ReadOnly,
                                allow_user_questions: true,
                                tool_allowlist: Vec::new(),
                                timeout_ms: 30_000,
                                prompt_mode: "json_input".to_string(),
                                system_prompt: "Review input".to_string(),
                            },
                        )
                        .expect("agent configuration"),
                    },
                )]),
                entries: vec!["agent".to_string()],
                exits: vec!["agent".to_string()],
                edges: Vec::new(),
            },
            bindings: Vec::new(),
            requirements: bcode_workflow::WorkflowRequirementSummary {
                capabilities: std::collections::BTreeSet::from([
                    "workflow-production/v1".to_string()
                ]),
                plugins: std::collections::BTreeSet::new(),
                blocks: std::collections::BTreeSet::new(),
                agents: std::collections::BTreeSet::from(["review".to_string()]),
            },
            run_limits: WorkflowRunLimitPolicy::default(),
            producer: WorkflowProducerProvenance {
                kind: bcode_workflow::WorkflowProducerKind::Human,
                producer_id: Some("test".to_string()),
                source_revision: None,
            },
            presentation: None,
        };
    let checksum = document.source_digest_sha256().expect("checksum");
    let workflow = AuthoredWorkflow {
        workflow_id: document.workflow_id.clone(),
        title: document.metadata.title.clone(),
        description: document.metadata.description.clone(),
        archived: false,
        active_revision: None,
        created_at_ms: 1,
        updated_at_ms: 1,
    };
    let draft = WorkflowDraft {
        workflow_id: document.workflow_id.clone(),
        draft_id: "draft-1".to_string(),
        base_revision: None,
        generation: 1,
        checksum_sha256: checksum,
        producer: document.producer.clone(),
        document,
        created_at_ms: 2,
        updated_at_ms: 2,
    };
    let catalog = bcode_workflow::WorkflowAuthoringCatalogSnapshot {
        version: bcode_workflow::WORKFLOW_AUTHORING_CATALOG_VERSION,
        capabilities: bcode_workflow::WorkflowAuthoringCapabilitySummary::from(
            &bcode_workflow::WorkflowProductionCapabilities::current(),
        ),
        plugins: std::collections::BTreeSet::new(),
        blocks: BTreeMap::new(),
        node_configuration_schemas: bcode_workflow::workflow_node_configuration_schemas(),
        workflow_definitions: BTreeMap::new(),
        agent_profiles: std::collections::BTreeSet::from(["review".to_string()]),
        authoring_actions: BTreeMap::new(),
    };
    (workflow, draft, catalog)
}

fn definition(name: &str) -> WorkflowDefinition {
    WorkflowBuilder::new(
        name,
        Step::task(
            "review",
            |value: u32, _context| async move { Ok(value + 1) },
        ),
    )
    .build()
    .expect("workflow")
    .definition()
    .clone()
}

fn sequential_definition() -> WorkflowDefinition {
    WorkflowBuilder::new(
        "sequential",
        Step::task("first", |value: u32, _context| async move { Ok(value + 1) })
            .then(Step::task("second", |value: u32, _context| async move {
                Ok(value + 1)
            })),
    )
    .build()
    .expect("workflow")
    .definition()
    .clone()
}

fn fan_out_definition() -> WorkflowDefinition {
    let member_schema = bcode_workflow::ValueSchema::of::<u32>();
    let member_node = bcode_workflow::NodeDefinition {
        id: "member".to_string(),
        name: "member".to_string(),
        kind: bcode_workflow::NodeKind::Approval,
        dataflow: bcode_workflow::WorkflowNodeDataflowPolicy::Direct,
        input: member_schema.clone(),
        output: member_schema.clone(),
        resources: Vec::new(),
        configuration: serde_json::json!({"gate_version": 1}),
    };
    let node = bcode_workflow::NodeDefinition {
        id: "fan-out".to_string(),
        name: "fan-out".to_string(),
        kind: bcode_workflow::NodeKind::FanOut,
        dataflow: bcode_workflow::WorkflowNodeDataflowPolicy::Direct,
        input: bcode_workflow::ValueSchema {
            type_name: "values/v1".to_string(),
            schema: serde_json::json!({"type": "array", "items": &member_schema.schema}),
        },
        output: bcode_workflow::ValueSchema {
            type_name: format!("workflow.fan-out-result/v1<{}>", member_schema.type_name),
            schema: serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["version", "members"],
                "properties": {
                    "version": {"type": "integer", "const": bcode_workflow::WORKFLOW_FAN_OUT_RESULT_VERSION},
                    "members": {"type": "array", "items": {
                        "type": "object", "additionalProperties": false,
                        "required": ["index", "value"],
                        "properties": {"index": {"type": "integer", "minimum": 0}, "value": &member_schema.schema}
                    }}
                }
            }),
        },
        resources: Vec::new(),
        configuration: serde_json::to_value(bcode_workflow::WorkflowFanOutConfiguration {
            version: bcode_workflow::WORKFLOW_FAN_OUT_CONFIGURATION_VERSION,
            member_node: Box::new(member_node),
            max_members: 4,
            max_concurrency: 2,
            failure_policy: bcode_workflow::ParallelFailurePolicy::WaitAll,
        })
        .expect("configuration"),
    };
    let definition = WorkflowDefinition {
        schema_version: bcode_workflow::WORKFLOW_DEFINITION_SCHEMA_VERSION,
        name: "fan-out".to_string(),
        input: node.input.clone(),
        output: node.output.clone(),
        nodes: BTreeMap::from([("fan-out".to_string(), node)]),
        entries: vec!["fan-out".to_string()],
        exits: vec!["fan-out".to_string()],
        edges: Vec::new(),
    };
    definition.validate().expect("fan-out definition");
    definition
}

fn repeat_definition() -> WorkflowDefinition {
    WorkflowBuilder::new(
        "repeat",
        Step::map("body", |mut value: serde_json::Value| {
            value["condition_met"] = serde_json::json!(false);
            Ok(value)
        })
        .repeat_while(
            "repeat-control",
            bcode_workflow::field::<serde_json::Value>("condition_met").eq(false),
            2,
        ),
    )
    .build()
    .expect("workflow")
    .definition()
    .clone()
}

fn repeat_definition_with_outcome() -> WorkflowDefinition {
    let mut definition = repeat_definition();
    let repeat = definition
        .nodes
        .get_mut("repeat-control")
        .expect("repeat control");
    repeat.configuration["exhaustion_policy"] = serde_json::json!("emit_outcome");
    repeat.configuration["repeat_outcome_version"] =
        serde_json::json!(bcode_workflow::WORKFLOW_REPEAT_OUTCOME_VERSION);
    repeat.output = bcode_workflow::ValueSchema::of::<
        bcode_workflow::WorkflowRepeatOutcome<serde_json::Value>,
    >();
    definition.output = repeat.output.clone();
    definition.validate().expect("outcome repeat definition");
    definition
}

fn workflow_call_definition(
    target: bcode_workflow::WorkflowDefinitionIdentity,
) -> WorkflowDefinition {
    let schema = bcode_workflow::ValueSchema::of::<u32>();
    let node = bcode_workflow::NodeDefinition {
        id: "call".to_string(),
        name: "call".to_string(),
        kind: bcode_workflow::NodeKind::WorkflowCall,
        dataflow: bcode_workflow::WorkflowNodeDataflowPolicy::Direct,
        input: schema.clone(),
        output: schema.clone(),
        resources: Vec::new(),
        configuration: serde_json::to_value(bcode_workflow::WorkflowCallConfiguration {
            version: bcode_workflow::WORKFLOW_CALL_VERSION,
            target: bcode_workflow::WorkflowCallTarget::Definition { identity: target },
            input: None,
            output: None,
        })
        .expect("call"),
    };
    let definition = WorkflowDefinition {
        schema_version: bcode_workflow::WORKFLOW_DEFINITION_SCHEMA_VERSION,
        name: "parent-call".to_string(),
        input: schema.clone(),
        output: schema,
        nodes: BTreeMap::from([("call".to_string(), node)]),
        entries: vec!["call".to_string()],
        exits: vec!["call".to_string()],
        edges: Vec::new(),
    };
    definition.validate().expect("call definition");
    definition
}

fn parallel_join_definition() -> WorkflowDefinition {
    let left = Step::task("left", |value: u32, _context| async move { Ok(value + 1) });
    let right = Step::task("right", |value: u32, _context| async move { Ok(value + 2) });
    let workflow = WorkflowBuilder::new(
        "parallel",
        bcode_workflow::parallel_named("join", left, right),
    )
    .build()
    .expect("workflow");
    assert_eq!(
        workflow.definition().nodes["join"].input,
        bcode_workflow::ValueSchema::of::<(u32, u32)>()
    );
    workflow.definition().clone()
}

fn parallel_join_definition_with_policy(
    policy: bcode_workflow::ParallelFailurePolicy,
) -> WorkflowDefinition {
    let mut definition = parallel_join_definition();
    definition
        .nodes
        .get_mut("join")
        .expect("join")
        .configuration["failure_policy"] = serde_json::to_value(policy).expect("policy");
    definition
}

fn parallel_join_transform_definition() -> WorkflowDefinition {
    let mut definition = parallel_join_definition();
    let tuple_schema = bcode_workflow::ValueSchema::of::<(u32, u32)>();
    for edge in &mut definition.edges {
        if edge.to == "join" {
            edge.transform = Some(bcode_workflow::WorkflowTransform {
                version: bcode_workflow::WORKFLOW_TRANSFORM_VERSION,
                expression: bcode_workflow::WorkflowTransformExpression::Array {
                    items: vec![
                        bcode_workflow::WorkflowTransformExpression::Input {
                            source: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_JOIN_LEFT.to_string(),
                            path: String::new(),
                        },
                        bcode_workflow::WorkflowTransformExpression::Input {
                            source: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_JOIN_RIGHT
                                .to_string(),
                            path: String::new(),
                        },
                    ],
                },
                output: tuple_schema.clone(),
            });
        }
    }
    definition
}

fn conditional_definition() -> WorkflowDefinition {
    let inspect = Step::task("inspect", |value: u32, _context| async move { Ok(value) });
    let selected = Step::task("selected", |value: u32, _context| async move { Ok(value) });
    let other = Step::task("other", |value: u32, _context| async move { Ok(value) });
    WorkflowBuilder::new(
        "conditional",
        inspect.branch(
            "choose",
            bcode_workflow::field::<u32>("").eq(1_u32),
            selected,
            other,
        ),
    )
    .build()
    .expect("workflow")
    .definition()
    .clone()
}

fn conditional_nested_definition() -> WorkflowDefinition {
    let inspect = Step::task("inspect", |value: serde_json::Value, _context| async move {
        Ok(value)
    });
    let selected = Step::task(
        "selected",
        |value: serde_json::Value, _context| async move { Ok(value) },
    );
    let other = Step::task("other", |value: serde_json::Value, _context| async move {
        Ok(value)
    });
    WorkflowBuilder::new(
        "conditional-nested",
        inspect.branch(
            "choose",
            bcode_workflow::field::<serde_json::Value>("review.result.passed").eq(true),
            selected,
            other,
        ),
    )
    .build()
    .expect("workflow")
    .definition()
    .clone()
}

fn mutation_approval_definition() -> WorkflowDefinition {
    let schema = bcode_workflow::ValueSchema::of::<serde_json::Value>();
    WorkflowDefinition {
        schema_version: bcode_workflow::WORKFLOW_DEFINITION_SCHEMA_VERSION,
        name: "mutation-approval".to_string(),
        input: schema.clone(),
        output: schema.clone(),
        nodes: BTreeMap::from([(
            "git.commit".to_string(),
            bcode_workflow::NodeDefinition {
                id: "git.commit".to_string(),
                name: "commit".to_string(),
                kind: bcode_workflow::NodeKind::PluginBlock,
                dataflow: bcode_workflow::WorkflowNodeDataflowPolicy::Direct,
                input: schema.clone(),
                output: schema,
                resources: Vec::new(),
                configuration: serde_json::json!({}),
            },
        )]),
        entries: vec!["git.commit".to_string()],
        exits: vec!["git.commit".to_string()],
        edges: Vec::new(),
    }
}

fn mutation_approval(input: &serde_json::Value) -> WorkflowMutationApproval {
    let input_json = serde_json::to_string(input).expect("input");
    WorkflowMutationApproval {
        approval_id: "approval-1".to_string(),
        run_id: "mutation-run".to_string(),
        node_id: "git.commit".to_string(),
        activation_id: activation_identity("mutation-run", "git.commit", 0),
        scope: bcode_workflow::WorkflowMutationGrantScope {
            version: bcode_workflow::WORKFLOW_MUTATION_GRANT_SCOPE_VERSION,
            definition_id: "mutation-definition".to_string(),
            definition_version: 1,
            run_id: "mutation-run".to_string(),
            node_id: "git.commit".to_string(),
            activation_id: activation_identity("mutation-run", "git.commit", 0),
            workspace_snapshot: "snapshot-1".to_string(),
            plugin_id: "bcode.git".to_string(),
            block_id: "git.commit".to_string(),
            block_version: 1,
            operation: "git.commit".to_string(),
            input_checksum_sha256: sha256_hex(input_json.as_bytes()),
            operation_facts: None,
            preparation_descriptor_sha256: None,
            input_summary: input.clone(),
            resource_claims: vec![bcode_workflow::ResourceClaim {
                resource: "repository".to_string(),
                access: bcode_workflow::ResourceAccess::Write,
            }],
            reconciliation: bcode_workflow::WorkflowBlockReconciliation::RepairRequired,
            capability: bcode_workflow::WorkflowToolCapability::Mutating,
        },
        requested_at_ms: 20,
        expires_at_ms: Some(100),
    }
}

#[test]
fn prepared_mutation_approval_facts_are_persisted_and_survive_restart() {
    let temp = tempfile::tempdir().expect("temp");
    let input = serde_json::json!({"expected_head": "abc"});
    let mut approval = mutation_approval(&input);
    approval.scope.operation_facts = Some(serde_json::json!({
        "version": 1,
        "program": "git",
        "arguments": ["commit"]
    }));
    approval.scope.preparation_descriptor_sha256 = Some("a".repeat(64));
    {
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("mutation-definition", 1, &mutation_approval_definition())
            .expect("definition");
        store
            .create_run(&NewWorkflowRun {
                run_id: "mutation-run".to_string(),
                definition_id: "mutation-definition".to_string(),
                definition_version: 1,
                workspace_snapshot: "snapshot-1".to_string(),
                parent_session_id: None,
                parent_session_generation: None,
                binding: None,
                authored_provenance: None,
                input: Some(input),
                execution_authority: None,
                created_at_ms: 10,
                authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                    version: 1,
                    provider_id: "test-policy".to_string(),
                    profile_id: "build".to_string(),
                    policy_digest_sha256: "a".repeat(64),
                },
                authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
                limits: WorkflowRunLimits::default(),
            })
            .expect("run");
        store
            .request_mutation_approval(&approval)
            .expect("approval");
        store
            .request_mutation_approval(&approval)
            .expect("duplicate approval");
        assert_eq!(
            store
                .pending_mutation_approvals("mutation-run", 10)
                .expect("pending duplicate")
                .len(),
            1
        );
        let resolution = store
            .resolve_mutation_approval(
                &approval.approval_id,
                WorkflowMutationApprovalDecision::Approve,
                25,
            )
            .expect("approve prepared facts");
        assert!(resolution.grant_id.is_some());
        let grant = store
            .grants_for_run("mutation-run", 10)
            .expect("grant")
            .pop()
            .expect("prepared grant");
        assert_eq!(
            grant.scope,
            serde_json::to_value(&approval.scope).expect("scope")
        );
    }
    let store = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
    assert!(
        store
            .pending_mutation_approvals("mutation-run", 10)
            .expect("approvals")
            .is_empty()
    );
    let restored: bcode_workflow::WorkflowMutationGrantScope = serde_json::from_value(
        store
            .grants_for_run("mutation-run", 10)
            .expect("grants")
            .pop()
            .expect("grant")
            .scope,
    )
    .expect("prepared scope");
    assert_eq!(restored.operation_facts, approval.scope.operation_facts);
    assert_eq!(
        restored.preparation_descriptor_sha256,
        approval.scope.preparation_descriptor_sha256
    );
}

#[test]
fn replacement_retains_artifact_after_terminal_cancellation_until_withdrawal() {
    let (temp, mut store) = initialized_store();
    block_on(
        update("workflow_runs")
            .value("owner_plugin_id", "test")
            .value("workflow_kind", "replacement")
            .value("scope_key", "scope")
            .value("single_active", 1)
            .value("target_artifact_id", "reserved-artifact")
            .value("coordinator_daemon_instance_id", "daemon")
            .value("coordinator_generation", 1)
            .value("coordinator_fencing_token", "fence")
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("owner fixture");
    let authority = store
        .execution_authority("run-1")
        .expect("authority")
        .expect("owner");
    let successor = NewWorkflowRun {
        run_id: "successor".into(),
        execution_authority: Some(authority.clone()),
        binding: Some(WorkflowRunBinding {
            owner_plugin_id: "test".into(),
            workflow_kind: "replacement".into(),
            scope_key: "scope".into(),
            display_label: None,
            single_active: true,
        }),
        ..new_run()
    };
    store
        .request_replacement_owned("run-1", &authority, &successor, 20)
        .expect("intent");
    assert_eq!(
        store
            .run_summary("run-1")
            .expect("summary")
            .expect("run")
            .status,
        RunStatus::Cancelled
    );
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
    assert_eq!(
        store.active_target_artifact_ids(10).expect("retained"),
        ["reserved-artifact"]
    );
    store
        .withdraw_replacement_owned("run-1", "successor", &authority, 21)
        .expect("withdraw");
    assert!(
        store
            .active_target_artifact_ids(10)
            .expect("released")
            .is_empty()
    );
}

#[test]
fn active_target_artifact_ids_reports_artifacts_owning_resumable_runs() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("mutation-definition", 1, &mutation_approval_definition())
        .expect("definition");
    store
        .create_run(&NewWorkflowRun {
            run_id: "fenced-run".to_string(),
            definition_id: "mutation-definition".to_string(),
            definition_version: 1,
            workspace_snapshot: "snapshot-1".to_string(),
            parent_session_id: Some("fenced-parent-session".to_string()),
            parent_session_generation: None,
            binding: None,
            authored_provenance: None,
            input: Some(serde_json::json!({"expected_head": "abc"})),
            execution_authority: Some(WorkflowExecutionAuthority {
                target_artifact_id: "bcode-fenced-artifact".to_string(),
                daemon_instance_id: "instance-1".to_string(),
                generation: 1,
                fencing_token: "token-1".to_string(),
            }),
            created_at_ms: 10,
            authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                version: 1,
                provider_id: "test-policy".to_string(),
                profile_id: "build".to_string(),
                policy_digest_sha256: "a".repeat(64),
            },
            authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
            limits: WorkflowRunLimits::default(),
        })
        .expect("run");

    // A running run keeps its artifact retained so recovery stays possible.
    assert_eq!(
        store.active_target_artifact_ids(100).expect("artifacts"),
        vec!["bcode-fenced-artifact".to_string()]
    );

    // Terminal runs release their artifact, allowing image collection.
    assert!(
        store
            .request_cancellation("fenced-run", 30)
            .expect("cancel")
    );
    assert_eq!(
        store
            .run_summary("fenced-run")
            .expect("run")
            .expect("run")
            .status,
        RunStatus::Cancelled
    );
    assert!(
        store
            .active_target_artifact_ids(100)
            .expect("artifacts")
            .is_empty()
    );
}

#[test]
fn mutation_approval_cancellation_settles_wait_without_grant_or_attempt() {
    let temp = tempfile::tempdir().expect("temp");
    let input = serde_json::json!({"expected_head": "abc"});
    let approval = mutation_approval(&input);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("mutation-definition", 1, &mutation_approval_definition())
        .expect("definition");
    store
        .create_run(&NewWorkflowRun {
            run_id: "mutation-run".to_string(),
            definition_id: "mutation-definition".to_string(),
            definition_version: 1,
            workspace_snapshot: "snapshot-1".to_string(),
            parent_session_id: None,
            parent_session_generation: None,
            binding: None,
            authored_provenance: None,
            input: Some(input),
            execution_authority: None,
            created_at_ms: 10,
            authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                version: 1,
                provider_id: "test-policy".to_string(),
                profile_id: "build".to_string(),
                policy_digest_sha256: "a".repeat(64),
            },
            authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
            limits: WorkflowRunLimits::default(),
        })
        .expect("run");
    store.request_mutation_approval(&approval).expect("request");
    assert!(
        store
            .request_cancellation("mutation-run", 30)
            .expect("cancel")
    );
    assert!(
        store
            .pending_mutation_approvals("mutation-run", 10)
            .expect("approvals")
            .is_empty()
    );
    assert!(
        store
            .grants_for_run("mutation-run", 10)
            .expect("grants")
            .is_empty()
    );
    assert!(
        store
            .attempt_history("mutation-run", None, 10)
            .expect("attempts")
            .is_empty()
    );
    assert_eq!(
        store
            .run_summary("mutation-run")
            .expect("run")
            .expect("run")
            .status,
        RunStatus::Cancelled
    );
    assert!(
        store
            .resolve_mutation_approval(
                &approval.approval_id,
                WorkflowMutationApprovalDecision::Approve,
                31,
            )
            .is_err()
    );
}

struct MutationDispatchOwner(std::sync::atomic::AtomicUsize);

impl ActivationDispatchOwner for MutationDispatchOwner {
    fn plan<'a>(
        &'a self,
        _activation: &'a PendingActivation,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<ActivationDispatchPlan>, WorkflowStoreError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            Ok(Some(ActivationDispatchPlan {
                side_effect: DispatchSideEffect::Mutating,
                intent: serde_json::json!({"operation": "git.commit"}),
            }))
        })
    }

    fn dispatch<'a>(
        &'a self,
        request: &'a PreparedActivationDispatch,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, WorkflowStoreError>> + Send + 'a>>
    {
        Box::pin(async move {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(serde_json::json!({"dispatch_identity": request.dispatch_identity}))
        })
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn mutation_approval_approve_persists_grant_before_restart_dispatchability() {
    let temp = tempfile::tempdir().expect("temp");
    let input = serde_json::json!({"expected_head": "abc"});
    let approval = mutation_approval(&input);
    {
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("mutation-definition", 1, &mutation_approval_definition())
            .expect("definition");
        store
            .create_run(&NewWorkflowRun {
                run_id: "mutation-run".to_string(),
                definition_id: "mutation-definition".to_string(),
                definition_version: 1,
                workspace_snapshot: "snapshot-1".to_string(),
                parent_session_id: None,
                parent_session_generation: None,
                binding: None,
                authored_provenance: None,
                input: Some(input),
                execution_authority: None,
                created_at_ms: 10,
                authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                    version: 1,
                    provider_id: "test-policy".to_string(),
                    profile_id: "build".to_string(),
                    policy_digest_sha256: "a".repeat(64),
                },
                authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
                limits: WorkflowRunLimits::default(),
            })
            .expect("run");
        store.request_mutation_approval(&approval).expect("request");
        let resolved = store
            .resolve_mutation_approval(
                &approval.approval_id,
                WorkflowMutationApprovalDecision::Approve,
                30,
            )
            .expect("approve");
        assert_eq!(resolved.status, "approved");
        assert_eq!(resolved.activation_status, "pending");
        let grant_id = resolved.grant_id.expect("grant");
        let grant = store
            .grant(&grant_id)
            .expect("grant lookup")
            .expect("grant");
        assert_eq!(
            grant.scope,
            serde_json::to_value(&approval.scope).expect("scope")
        );
        assert_eq!(store.pending_activations(10).expect("pending").len(), 1);
        assert_eq!(
            store
                .resolve_mutation_approval(
                    &approval.approval_id,
                    WorkflowMutationApprovalDecision::Approve,
                    31,
                )
                .expect("duplicate approve")
                .grant_id
                .as_deref(),
            Some(grant_id.as_str())
        );
        assert!(
            store
                .resolve_mutation_approval(
                    &approval.approval_id,
                    WorkflowMutationApprovalDecision::Deny,
                    32,
                )
                .is_err()
        );
    }
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    assert_eq!(store.pending_activations(10).expect("pending").len(), 1);
    assert!(
        store
            .attempt_history("mutation-run", None, 10)
            .expect("attempts")
            .is_empty()
    );
    let owner = MutationDispatchOwner(std::sync::atomic::AtomicUsize::new(0));
    let first = store
        .dispatch_pending_activations(&owner, 10, 40)
        .await
        .expect("dispatch after restart");
    assert_eq!(first.admitted.len(), 1);
    let dispatch_identity = first.admitted[0].clone();
    assert_eq!(owner.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(
        store
            .dispatch_pending_activations(&owner, 10, 41)
            .await
            .expect("second scan")
            .admitted
            .is_empty()
    );
    assert_eq!(owner.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    let attempts = store
        .attempt_history("mutation-run", None, 10)
        .expect("attempts");
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].dispatch_identity, dispatch_identity);
}

#[test]
fn mutation_approval_clock_waits_for_write_lock() {
    let temp = tempfile::tempdir().expect("temp");
    let input = serde_json::json!({"expected_head": "abc"});
    let approval = mutation_approval(&input);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("mutation-definition", 1, &mutation_approval_definition())
        .expect("definition");
    store
        .create_run(&NewWorkflowRun {
            run_id: "mutation-run".to_owned(),
            definition_id: "mutation-definition".to_owned(),
            definition_version: 1,
            workspace_snapshot: "snapshot-1".to_owned(),
            parent_session_id: None,
            parent_session_generation: None,
            binding: None,
            authored_provenance: None,
            input: Some(input),
            execution_authority: None,
            created_at_ms: 10,
            authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                version: 1,
                provider_id: "test-policy".to_owned(),
                profile_id: "build".to_owned(),
                policy_digest_sha256: "a".repeat(64),
            },
            authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
            limits: WorkflowRunLimits::default(),
        })
        .expect("run");
    store
        .request_mutation_approval(&approval)
        .expect("approval");
    let blocker = WorkflowStore::open_in_state_dir(temp.path()).expect("blocker");
    let transaction =
        database::begin(&blocker.connection, TransactionMode::Immediate).expect("write lock");
    let sampled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let now = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(30));
    let worker_sampled = std::sync::Arc::clone(&sampled);
    let worker_now = std::sync::Arc::clone(&now);
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        started_tx.send(()).expect("started");
        let result = store
            .resolve_mutation_approval_with_clock(
                &approval.approval_id,
                WorkflowMutationApprovalDecision::Approve,
                None,
                || {
                    worker_sampled.store(true, std::sync::atomic::Ordering::SeqCst);
                    worker_now.load(std::sync::atomic::Ordering::SeqCst)
                },
            )
            .expect("resolution");
        drop(store);
        result
    });
    started_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("worker started");
    std::thread::sleep(std::time::Duration::from_millis(50));
    let sampled_while_locked = sampled.load(std::sync::atomic::Ordering::SeqCst);
    now.store(100, std::sync::atomic::Ordering::SeqCst);
    database::commit(transaction).expect("release lock");
    drop(blocker);
    let result = worker.join().expect("worker");
    assert!(!sampled_while_locked);
    assert!(sampled.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(result.status, "expired");
    assert!(result.grant_id.is_none());
}

#[test]
fn mutation_approval_deny_and_expiry_never_dispatch() {
    for (decision, resolved_at_ms, expected) in [
        (WorkflowMutationApprovalDecision::Deny, 30, "denied"),
        (WorkflowMutationApprovalDecision::Approve, 100, "expired"),
    ] {
        let temp = tempfile::tempdir().expect("temp");
        let input = serde_json::json!({"expected_head": "abc"});
        let approval = mutation_approval(&input);
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("mutation-definition", 1, &mutation_approval_definition())
            .expect("definition");
        store
            .create_run(&NewWorkflowRun {
                run_id: "mutation-run".to_string(),
                definition_id: "mutation-definition".to_string(),
                definition_version: 1,
                workspace_snapshot: "snapshot-1".to_string(),
                parent_session_id: None,
                parent_session_generation: None,
                binding: None,
                authored_provenance: None,
                input: Some(input),
                execution_authority: None,
                created_at_ms: 10,
                authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                    version: 1,
                    provider_id: "test-policy".to_string(),
                    profile_id: "build".to_string(),
                    policy_digest_sha256: "a".repeat(64),
                },
                authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
                limits: WorkflowRunLimits::default(),
            })
            .expect("run");
        store.request_mutation_approval(&approval).expect("request");
        let resolved = store
            .resolve_mutation_approval(&approval.approval_id, decision, resolved_at_ms)
            .expect("resolution");
        assert_eq!(resolved.status, expected);
        assert!(resolved.grant_id.is_none());
        assert!(store.pending_activations(10).expect("pending").is_empty());
        assert!(
            store
                .attempt_history("mutation-run", None, 10)
                .expect("attempts")
                .is_empty()
        );
        assert_eq!(
            store
                .run_summary("mutation-run")
                .expect("run")
                .expect("run")
                .status,
            RunStatus::Failed
        );
    }
}

#[test]
fn mutation_approval_wait_is_exact_persisted_and_restart_safe() {
    let temp = tempfile::tempdir().expect("temp");
    let input = serde_json::json!({"expected_head": "abc", "paths": ["src/lib.rs"]});
    {
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("mutation-definition", 1, &mutation_approval_definition())
            .expect("definition");
        store
            .create_run(&NewWorkflowRun {
                run_id: "mutation-run".to_string(),
                definition_id: "mutation-definition".to_string(),
                definition_version: 1,
                workspace_snapshot: "snapshot-1".to_string(),
                parent_session_id: None,
                parent_session_generation: None,
                binding: None,
                authored_provenance: None,
                input: Some(input.clone()),
                execution_authority: None,
                created_at_ms: 10,
                authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                    version: 1,
                    provider_id: "test-policy".to_string(),
                    profile_id: "build".to_string(),
                    policy_digest_sha256: "a".repeat(64),
                },
                authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
                limits: WorkflowRunLimits::default(),
            })
            .expect("run");
        let approval = mutation_approval(&input);
        store
            .request_mutation_approval(&approval)
            .expect("request approval");
        store
            .request_mutation_approval(&approval)
            .expect("idempotent request");
        assert!(store.pending_activations(10).expect("pending").is_empty());
        assert_eq!(
            store
                .pending_mutation_approvals("mutation-run", 10)
                .expect("approvals"),
            vec![approval]
        );
        assert!(
            store
                .attempt_history("mutation-run", None, 10)
                .expect("attempts")
                .is_empty()
        );
    }
    let store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    assert_eq!(
        store
            .pending_mutation_approvals("mutation-run", 10)
            .expect("reopened approvals")
            .len(),
        1
    );
    assert!(store.pending_activations(10).expect("pending").is_empty());
}

#[test]
fn mutation_approval_rejects_stale_or_wrong_identity_without_waiting() {
    let temp = tempfile::tempdir().expect("temp");
    let input = serde_json::json!({"expected_head": "abc"});
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("mutation-definition", 1, &mutation_approval_definition())
        .expect("definition");
    store
        .create_run(&NewWorkflowRun {
            run_id: "mutation-run".to_string(),
            definition_id: "mutation-definition".to_string(),
            definition_version: 1,
            workspace_snapshot: "snapshot-1".to_string(),
            parent_session_id: None,
            parent_session_generation: None,
            binding: None,
            authored_provenance: None,
            input: Some(input.clone()),
            execution_authority: None,
            created_at_ms: 10,
            authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                version: 1,
                provider_id: "test-policy".to_string(),
                profile_id: "build".to_string(),
                policy_digest_sha256: "a".repeat(64),
            },
            authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
            limits: WorkflowRunLimits::default(),
        })
        .expect("run");
    let mut stale = mutation_approval(&input);
    stale.scope.input_checksum_sha256 = "0".repeat(64);
    assert!(store.request_mutation_approval(&stale).is_err());
    let mut wrong_workspace = mutation_approval(&input);
    wrong_workspace.scope.workspace_snapshot = "other".to_string();
    assert!(store.request_mutation_approval(&wrong_workspace).is_err());
    let mut wrong_run = mutation_approval(&input);
    wrong_run.run_id = "other-run".to_string();
    wrong_run.scope.run_id = "other-run".to_string();
    assert!(store.request_mutation_approval(&wrong_run).is_err());
    let mut wrong_node = mutation_approval(&input);
    wrong_node.node_id = "other-node".to_string();
    wrong_node.scope.node_id = "other-node".to_string();
    assert!(store.request_mutation_approval(&wrong_node).is_err());
    let mut wrong_activation = mutation_approval(&input);
    wrong_activation.activation_id = "other-activation".to_string();
    wrong_activation.scope.activation_id = "other-activation".to_string();
    assert!(store.request_mutation_approval(&wrong_activation).is_err());
    assert_eq!(store.pending_activations(10).expect("pending").len(), 1);
    assert!(
        store
            .pending_mutation_approvals("mutation-run", 10)
            .expect("approvals")
            .is_empty()
    );
}

fn new_run() -> NewWorkflowRun {
    NewWorkflowRun {
        run_id: "run-1".to_string(),
        definition_id: "example".to_string(),
        definition_version: 1,
        workspace_snapshot: "snapshot-1".to_string(),
        parent_session_id: Some("session-1".to_string()),
        parent_session_generation: None,
        binding: None,
        authored_provenance: None,
        input: Some(serde_json::json!(1)),
        execution_authority: None,
        created_at_ms: 10,
        authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
            version: 1,
            provider_id: "test-policy".to_string(),
            profile_id: "build".to_string(),
            policy_digest_sha256: "a".repeat(64),
        },
        authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
        limits: WorkflowRunLimits::default(),
    }
}

fn authored_run_provenance() -> AuthoredWorkflowRunProvenance {
    AuthoredWorkflowRunProvenance::new(
        "authored/example".to_string(),
        3,
        bcode_workflow::WorkflowDefinitionIdentity {
            kind: "authored/example".to_string(),
            definition_id: "example".to_string(),
            definition_version: 1,
        },
        Some("review".to_string()),
        Some(4),
        serde_json::json!({"message": "review"}),
    )
}

fn persist_authored_run_provenance_fixture(store: &WorkflowStore) {
    block_on(
        insert("authored_workflows")
            .value("workflow_id", "authored/example")
            .value("title", "Example")
            .value("archived", 0)
            .value("active_revision", DatabaseValue::Null)
            .value("created_at_ms", 1)
            .value("updated_at_ms", 1)
            .execute(&store.connection),
    )
    .expect("authored workflow");
    block_on(
        insert("workflow_revisions")
            .value("workflow_id", "authored/example")
            .value("revision", 3)
            .value("source_checksum_sha256", "source")
            .value("executable_source_checksum_sha256", "executable")
            .value("definition_id", "example")
            .value("definition_version", 1)
            .value("document_json", "{}")
            .value("producer_json", "{}")
            .value("published_at_ms", 2)
            .execute(&store.connection),
    )
    .expect("authored revision");
    block_on(
        insert("workflow_presets")
            .value("workflow_id", "authored/example")
            .value("preset_id", "review")
            .value("revision", 3)
            .value("name", "Review")
            .value("generation", 4)
            .value("configuration_json", "{}")
            .value("run_limits_json", DatabaseValue::Null)
            .value("producer_json", "{}")
            .value("created_at_ms", 3)
            .value("updated_at_ms", 3)
            .execute(&store.connection),
    )
    .expect("authored preset");
}

#[test]
fn replacement_binding_reservation_cannot_be_bypassed_by_multi_active_request() {
    let (_temp, mut store, _, authority, _) = connected_publication_fixture();
    store
        .persist_definition("example", 1, &definition("example"))
        .expect("definition");
    let old = NewWorkflowRun {
        run_id: "old".into(),
        execution_authority: Some(authority.clone()),
        binding: Some(WorkflowRunBinding {
            owner_plugin_id: "test".into(),
            workflow_kind: "replacement".into(),
            scope_key: "scope".into(),
            display_label: None,
            single_active: true,
        }),
        ..new_run()
    };
    store.create_run(&old).expect("old");
    let successor = NewWorkflowRun {
        run_id: "successor".into(),
        ..old
    };
    store
        .request_replacement_owned("old", &authority, &successor, 20)
        .expect("intent");
    let mut competitor = NewWorkflowRun {
        run_id: "competitor".into(),
        ..successor.clone()
    };
    competitor.binding.as_mut().expect("binding").single_active = false;
    assert!(store.create_run(&competitor).is_err());
    block_on(
        update("workflow_runs")
            .value("status", "cancelled")
            .where_eq("run_id", "old")
            .execute(&store.connection),
    )
    .expect("settled fixture");
    assert!(store.create_run(&competitor).is_err());
    assert!(store.run_summary("competitor").expect("absent").is_none());
    assert_eq!(
        store.pending_replacement("old").expect("preserved"),
        Some(successor)
    );
    store
        .withdraw_replacement_owned("old", "successor", &authority, 21)
        .expect("withdraw");
    store.create_run(&competitor).expect("reservation released");
}

#[test]
fn replacement_terminal_parent_does_not_prove_effect_or_descendant_quiescence() {
    let (_temp, mut store) = initialized_store();
    let authority = WorkflowExecutionAuthority {
        target_artifact_id: "artifact".into(),
        daemon_instance_id: "daemon".into(),
        generation: 1,
        fencing_token: "fence".into(),
    };
    prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::Mutating);
    block_on(
        update("workflow_runs")
            .value("owner_plugin_id", "test")
            .value("workflow_kind", "replacement")
            .value("scope_key", "scope")
            .value("single_active", 1)
            .value("target_artifact_id", "artifact")
            .value("coordinator_daemon_instance_id", "daemon")
            .value("coordinator_generation", 1)
            .value("coordinator_fencing_token", "fence")
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("fixture");
    let successor = NewWorkflowRun {
        run_id: "successor".into(),
        execution_authority: Some(authority.clone()),
        binding: Some(WorkflowRunBinding {
            owner_plugin_id: "test".into(),
            workflow_kind: "replacement".into(),
            scope_key: "scope".into(),
            display_label: None,
            single_active: true,
        }),
        ..new_run()
    };
    store
        .request_replacement_owned("run-1", &authority, &successor, 20)
        .expect("intent");
    block_on(
        update("workflow_runs")
            .value("status", "cancelled")
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("terminal fixture");
    assert_eq!(
        store.replacement_readiness("run-1").expect("effects"),
        ReplacementReadiness::WaitingForEffects
    );
    assert!(
        store
            .complete_leaf_replacement_owned("run-1", &authority, &successor, 21)
            .is_err()
    );
    block_on(
        update("workflow_attempts")
            .value("status", "cancelled")
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("settled fixture");
    let child = NewWorkflowRun {
        run_id: "child".into(),
        ..new_run()
    };
    store.create_run(&child).expect("child");
    fixture_link_replacement_child(&store.connection);
    for status in ["running", "cancelled"] {
        block_on(
            update("workflow_runs")
                .value("status", status)
                .where_eq("run_id", "child")
                .execute(&store.connection),
        )
        .expect("child status");
        let before = fixture_changes(&store.connection);
        assert_eq!(
            store.replacement_readiness("run-1").expect("descendants"),
            ReplacementReadiness::DescendantProofRequired
        );
        assert!(
            store
                .complete_leaf_replacement_owned("run-1", &authority, &successor, 22)
                .is_err()
        );
        assert_eq!(fixture_changes(&store.connection), before);
    }
    assert!(
        store
            .pending_replacement("run-1")
            .expect("preserved")
            .is_some()
    );
    assert!(store.run_summary("successor").expect("uncreated").is_none());
}

fn fixture_link_replacement_child(db: &dyn Database) {
    block_on(
        insert("workflow_run_links")
            .value("root_run_id", "run-1")
            .value("parent_run_id", "run-1")
            .value("parent_node_id", "review")
            .value("parent_activation_id", "activation")
            .value("parent_attempt", 1)
            .value("child_run_id", "child")
            .value("version", 1)
            .value("target_json", "{}")
            .value("depth", 2)
            .value("created_at_ms", 0)
            .execute(db),
    )
    .expect("child fixture");
}

#[test]
fn execution_fence_blocks_transfer_until_external_commit_finishes() {
    let (_temp, mut store, _, authority, _) = connected_publication_fixture();
    let fence = store.execution_fence("run-1", &authority).expect("fence");
    fence.validate().expect("held authority");
    let next = WorkflowExecutionAuthority {
        daemon_instance_id: "next-owner".into(),
        generation: authority.generation + 1,
        fencing_token: "next-owner-fence".into(),
        ..authority.clone()
    };
    assert!(
        store
            .transfer_execution_authority("run-1", &authority, &next, 21)
            .is_err()
    );
    fence
        .validate()
        .expect("transfer did not alter held authority");
    drop(fence);
    store
        .transfer_execution_authority("run-1", &authority, &next, 22)
        .expect("transfer after external commit");
    assert!(store.execution_fence("run-1", &authority).is_err());
    store
        .execution_fence("run-1", &next)
        .expect("new owner fence")
        .validate()
        .expect("new owner");
}

#[test]
fn replacement_terminal_owner_can_transfer_only_while_intent_is_pending() {
    let (temp, mut store, _, authority, _) = connected_publication_fixture();
    store
        .persist_definition("example", 1, &definition("example"))
        .expect("definition");
    let old = NewWorkflowRun {
        run_id: "old".into(),
        execution_authority: Some(authority.clone()),
        binding: Some(WorkflowRunBinding {
            owner_plugin_id: "test".into(),
            workflow_kind: "replacement".into(),
            scope_key: "scope".into(),
            display_label: None,
            single_active: true,
        }),
        ..new_run()
    };
    store.create_run(&old).expect("old");
    let successor = NewWorkflowRun {
        run_id: "successor".into(),
        ..old
    };
    store
        .request_replacement_owned("old", &authority, &successor, 20)
        .expect("intent");
    assert_eq!(
        store
            .run_summary("old")
            .expect("summary")
            .expect("old")
            .status,
        RunStatus::Cancelled
    );
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
    let next = WorkflowExecutionAuthority {
        daemon_instance_id: "next".into(),
        generation: authority.generation + 1,
        fencing_token: "next-fence".into(),
        ..authority.clone()
    };
    store
        .transfer_execution_authority("old", &authority, &next, 21)
        .expect("recover terminal reservation");
    assert!(
        store
            .withdraw_replacement_owned("old", "successor", &authority, 22)
            .is_err()
    );
    store
        .withdraw_replacement_owned("old", "successor", &next, 23)
        .expect("withdraw");
    let later = WorkflowExecutionAuthority {
        daemon_instance_id: "later".into(),
        generation: next.generation + 1,
        fencing_token: "later-fence".into(),
        ..next.clone()
    };
    assert!(
        store
            .transfer_execution_authority("old", &next, &later, 24)
            .is_err()
    );
    assert_eq!(
        store
            .run_summary("old")
            .expect("summary")
            .expect("old")
            .status,
        RunStatus::Cancelled
    );
}

#[test]
fn replacement_requires_explicit_repair_before_new_intent() {
    let (_temp, mut store, _, authority, _) = connected_publication_fixture();
    store
        .persist_definition("example", 1, &definition("example"))
        .expect("definition");
    let old = NewWorkflowRun {
        run_id: "old".into(),
        execution_authority: Some(authority.clone()),
        binding: Some(WorkflowRunBinding {
            owner_plugin_id: "test".into(),
            workflow_kind: "replacement".into(),
            scope_key: "scope".into(),
            display_label: None,
            single_active: true,
        }),
        ..new_run()
    };
    store.create_run(&old).expect("old");
    let successor = NewWorkflowRun {
        run_id: "successor".into(),
        ..old
    };
    block_on(
        update("workflow_runs")
            .value("status", "repair_required")
            .where_eq("run_id", "old")
            .execute(&store.connection),
    )
    .expect("repair fixture");
    let before = fixture_changes(&store.connection);
    assert!(
        store
            .request_replacement_owned("old", &authority, &successor, 20)
            .is_err()
    );
    assert_eq!(fixture_changes(&store.connection), before);
    assert!(
        store
            .pending_replacement("old")
            .expect("no intent")
            .is_none()
    );
    let old = store.run_summary("old").expect("summary").expect("old");
    assert_eq!(old.status, RunStatus::RepairRequired);
    assert_eq!(old.cancellation_requested_at_ms, None);
}

fn verify_package_replacement(
    store: &mut WorkflowStore,
    run: &NewWorkflowRun,
    receipt: &bcode_workflow::WorkflowPackagePublicationReceipt,
) {
    let owner = WorkflowExecutionAuthority {
        target_artifact_id: "artifact".into(),
        daemon_instance_id: "daemon".into(),
        generation: 1,
        fencing_token: "fence".into(),
    };
    block_on(
        update("workflow_runs")
            .value("owner_plugin_id", "test")
            .value("workflow_kind", "package")
            .value("scope_key", "scope")
            .value("single_active", 1)
            .value("target_artifact_id", "artifact")
            .value("coordinator_daemon_instance_id", "daemon")
            .value("coordinator_generation", 1)
            .value("coordinator_fencing_token", "fence")
            .where_eq("run_id", run.run_id.as_str())
            .execute(&store.connection),
    )
    .expect("owner fixture");
    let successor = NewWorkflowRun {
        run_id: "package-successor".into(),
        execution_authority: Some(owner.clone()),
        binding: Some(WorkflowRunBinding {
            owner_plugin_id: "test".into(),
            workflow_kind: "package".into(),
            scope_key: "scope".into(),
            display_label: None,
            single_active: true,
        }),
        ..run.clone()
    };
    let selection = bcode_workflow::WorkflowPackageExportIdentity {
        package_id: receipt.package_id.clone(),
        package_lock_digest_sha256: Some(receipt.package_lock_digest_sha256.clone()),
        export: receipt.exports[0].export.clone(),
    };
    assert!(
        store
            .resolve_run_package_export(&run.run_id, &selection)
            .is_ok()
    );
    let mut wrong = selection.clone();
    wrong.package_lock_digest_sha256 = Some("0".repeat(64));
    assert!(
        store
            .resolve_run_package_export(&run.run_id, &wrong)
            .is_err()
    );
    wrong = selection;
    wrong.export = "missing".into();
    assert!(
        store
            .resolve_run_package_export(&run.run_id, &wrong)
            .is_err()
    );
    store
        .request_replacement_owned(&run.run_id, &owner, &successor, 30)
        .expect("intent");
    for phase in ["before_commit", "after_commit"] {
        let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "tests::authoring_packages::package_replacement_crash_child",
            ])
            .env("BCODE_TEST_PACKAGE_REPLACEMENT_DB", store.path())
            .env("BCODE_TEST_PACKAGE_REPLACEMENT_RUN", &run.run_id)
            .env("BCODE_TEST_REPLACEMENT_CRASH_PHASE", phase)
            .status()
            .expect("child");
        let committed = phase == "after_commit";
        assert_eq!(status.code(), Some(if committed { 74 } else { 73 }));
        assert_eq!(
            store
                .run_summary(&successor.run_id)
                .expect("successor")
                .is_some(),
            committed
        );
        assert_eq!(
            store
                .pending_replacement(&run.run_id)
                .expect("intent")
                .is_none(),
            committed
        );
    }
    assert!(
        !store
            .complete_leaf_replacement_owned(&run.run_id, &owner, &successor, 32)
            .expect("idempotent retry")
    );
    assert_eq!(
        store
            .resolve_run_package_member(&successor.run_id, &receipt.exports[0].member_id)
            .expect("inherited binding"),
        receipt.exports[0].definition_identity
    );
}

#[test]
fn multi_active_replacement_preserves_siblings_and_exact_cardinality() {
    let (_temp, mut store) = initialized_store();
    let owner = WorkflowExecutionAuthority {
        target_artifact_id: "a".into(),
        daemon_instance_id: "d".into(),
        generation: 1,
        fencing_token: "f".into(),
    };
    let old = NewWorkflowRun {
        run_id: "old".into(),
        execution_authority: Some(owner.clone()),
        binding: Some(WorkflowRunBinding {
            owner_plugin_id: "authored".into(),
            workflow_kind: "workflow".into(),
            scope_key: "revision/1".into(),
            display_label: None,
            single_active: false,
        }),
        ..new_run()
    };
    store.create_run(&old).expect("old");
    let sibling = NewWorkflowRun {
        run_id: "sibling".into(),
        ..old.clone()
    };
    store.create_run(&sibling).expect("sibling");
    let mut successor = NewWorkflowRun {
        run_id: "successor".into(),
        ..old
    };
    successor.binding.as_mut().expect("binding").single_active = true;
    assert!(
        store
            .request_replacement_owned("old", &owner, &successor, 20)
            .is_err()
    );
    successor.binding.as_mut().expect("binding").single_active = false;
    store
        .request_replacement_owned("old", &owner, &successor, 21)
        .expect("intent");
    store
        .create_run(&NewWorkflowRun {
            run_id: "another".into(),
            ..sibling.clone()
        })
        .expect("unrelated sibling remains admissible");
    assert!(
        store
            .complete_leaf_replacement_owned("old", &owner, &successor, 22)
            .expect("handoff")
    );
    let sibling = store
        .run_summary("sibling")
        .expect("summary")
        .expect("sibling");
    assert_eq!(sibling.status, RunStatus::Running);
    assert_eq!(sibling.cancellation_requested_at_ms, None);
    assert_eq!(
        store
            .run_summary("old")
            .expect("summary")
            .expect("old")
            .status,
        RunStatus::Cancelled
    );
}

#[test]
fn replacement_readiness_is_read_only_and_never_grants_execution() {
    let (_temp, mut store, _, authority, _) = connected_publication_fixture();
    store
        .persist_definition("example", 1, &definition("example"))
        .expect("definition");
    let old = NewWorkflowRun {
        run_id: "old".into(),
        execution_authority: Some(authority.clone()),
        binding: Some(WorkflowRunBinding {
            owner_plugin_id: "test".into(),
            workflow_kind: "replacement".into(),
            scope_key: "scope".into(),
            display_label: None,
            single_active: true,
        }),
        ..new_run()
    };
    store.create_run(&old).expect("old");
    assert_eq!(
        store.replacement_readiness("old").expect("absent"),
        ReplacementReadiness::Absent
    );
    let successor = NewWorkflowRun {
        run_id: "successor".into(),
        ..old
    };
    store
        .request_replacement_owned("old", &authority, &successor, 20)
        .expect("intent");
    let before = fixture_changes(&store.connection);
    assert_eq!(
        store.replacement_readiness("old").expect("waiting"),
        ReplacementReadiness::ReadyForAuthorization
    );
    assert_eq!(fixture_changes(&store.connection), before);
    block_on(
        update("workflow_runs")
            .value("status", "cancelled")
            .where_eq("run_id", "old")
            .execute(&store.connection),
    )
    .expect("settled fixture");
    assert_eq!(
        store.replacement_readiness("old").expect("ready"),
        ReplacementReadiness::ReadyForAuthorization
    );
    assert!(
        store
            .run_summary("successor")
            .expect("not admitted")
            .is_none()
    );
    block_on(
        update("workflow_runs")
            .value("target_artifact_id", "foreign")
            .where_eq("run_id", "old")
            .execute(&store.connection),
    )
    .expect("foreign fixture");
    assert_eq!(
        store.replacement_readiness("old").expect("incompatible"),
        ReplacementReadiness::ExecutionCompatibilityRequired
    );
}

#[test]
fn replacement_reserves_successor_identity_across_unrelated_bindings() {
    let (temp, mut store, _, authority, _) = connected_publication_fixture();
    store
        .persist_definition("example", 1, &definition("example"))
        .expect("definition");
    let old = NewWorkflowRun {
        run_id: "old".into(),
        execution_authority: Some(authority.clone()),
        binding: Some(WorkflowRunBinding {
            owner_plugin_id: "test".into(),
            workflow_kind: "replacement".into(),
            scope_key: "scope".into(),
            display_label: None,
            single_active: true,
        }),
        ..new_run()
    };
    store.create_run(&old).expect("old");
    let successor = NewWorkflowRun {
        run_id: "successor".into(),
        ..old
    };
    store
        .request_replacement_owned("old", &authority, &successor, 20)
        .expect("intent");
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
    let unrelated = NewWorkflowRun {
        binding: None,
        ..successor.clone()
    };
    assert!(store.create_run(&unrelated).is_err());
    assert!(store.run_summary("successor").expect("uncreated").is_none());
    assert_eq!(
        store.pending_replacement("old").expect("preserved"),
        Some(successor)
    );
    store
        .withdraw_replacement_owned("old", "successor", &authority, 21)
        .expect("withdraw");
    store.create_run(&unrelated).expect("released identity");
}

#[test]
fn replacement_invalid_definition_does_not_cancel_old_run() {
    let (_temp, mut store, _, authority, _) = connected_publication_fixture();
    store
        .persist_definition("example", 1, &definition("example"))
        .expect("definition");
    let old = NewWorkflowRun {
        run_id: "old".into(),
        execution_authority: Some(authority.clone()),
        binding: Some(WorkflowRunBinding {
            owner_plugin_id: "test".into(),
            workflow_kind: "replacement".into(),
            scope_key: "scope".into(),
            display_label: None,
            single_active: true,
        }),
        ..new_run()
    };
    store.create_run(&old).expect("old");
    let mut successor = NewWorkflowRun {
        run_id: "successor".into(),
        definition_id: "missing".into(),
        ..old
    };
    assert!(
        store
            .request_replacement_owned("old", &authority, &successor, 20)
            .is_err()
    );
    successor.definition_id = "example".into();
    successor.input = Some(serde_json::json!({"invalid": true}));
    assert!(
        store
            .request_replacement_owned("old", &authority, &successor, 21)
            .is_err()
    );
    assert!(
        store
            .pending_replacement("old")
            .expect("no intent")
            .is_none()
    );
    assert_eq!(
        store
            .run_summary("old")
            .expect("summary")
            .expect("old")
            .cancellation_requested_at_ms,
        None
    );
    assert!(
        !store
            .event_history("old", None, 100)
            .expect("history")
            .iter()
            .any(|event| event.event_type == "replacement_requested")
    );
}

#[test]
fn replacement_withdrawal_is_fenced_and_preserves_cancellation() {
    let (temp, mut store, _, authority, _) = connected_publication_fixture();
    store
        .persist_definition("example", 1, &definition("example"))
        .expect("definition");
    let old = NewWorkflowRun {
        run_id: "old".into(),
        execution_authority: Some(authority.clone()),
        binding: Some(WorkflowRunBinding {
            owner_plugin_id: "test".into(),
            workflow_kind: "replacement".into(),
            scope_key: "scope".into(),
            display_label: None,
            single_active: true,
        }),
        ..new_run()
    };
    store.create_run(&old).expect("old");
    let successor = NewWorkflowRun {
        run_id: "successor".into(),
        ..old
    };
    store
        .request_replacement_owned("old", &authority, &successor, 20)
        .expect("intent");
    let stale = WorkflowExecutionAuthority {
        fencing_token: "stale".into(),
        ..authority.clone()
    };
    assert!(
        store
            .withdraw_replacement_owned("old", "successor", &stale, 21)
            .is_err()
    );
    assert!(
        store
            .withdraw_replacement_owned("old", "different", &authority, 21)
            .is_err()
    );
    assert!(
        store
            .withdraw_replacement_owned("old", "successor", &authority, 22)
            .expect("withdraw")
    );
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
    assert!(
        !store
            .withdraw_replacement_owned("old", "successor", &authority, 23)
            .expect("retry")
    );
    assert!(store.pending_replacement("old").expect("absent").is_none());
    assert_eq!(
        store
            .run_summary("old")
            .expect("summary")
            .expect("old")
            .cancellation_requested_at_ms,
        Some(20)
    );
    assert_eq!(
        store
            .run_summary("old")
            .expect("summary")
            .expect("old")
            .status,
        RunStatus::Cancelled
    );
    store
        .create_run(&successor)
        .expect("released reservation after settlement");
    assert_eq!(
        store
            .event_history("old", None, 100)
            .expect("events")
            .iter()
            .filter(|event| event.event_type == "replacement_withdrawn")
            .count(),
        1
    );
}

#[test]
fn replacement_discovery_pages_terminal_owners_without_mutation() {
    let (temp, mut store, _, authority, _) = connected_publication_fixture();
    store
        .persist_definition("example", 1, &definition("example"))
        .expect("definition");
    for id in ["a", "b", "c"] {
        let old = NewWorkflowRun {
            run_id: id.into(),
            execution_authority: Some(authority.clone()),
            binding: Some(WorkflowRunBinding {
                owner_plugin_id: "test".into(),
                workflow_kind: "replacement".into(),
                scope_key: id.into(),
                display_label: None,
                single_active: true,
            }),
            ..new_run()
        };
        store.create_run(&old).expect("old");
        let successor = NewWorkflowRun {
            run_id: format!("next-{id}"),
            ..old
        };
        store
            .request_replacement_owned(id, &authority, &successor, 20)
            .expect("intent");
    }
    block_on(
        update("workflow_runs")
            .value("status", "cancelled")
            .where_eq("run_id", "b")
            .execute(&store.connection),
    )
    .expect("terminal fixture");
    drop(store);
    let store = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
    let before = fixture_changes(&store.connection);
    assert_eq!(
        store
            .pending_replacement_run_ids_after("", 1)
            .expect("first"),
        ["a"]
    );
    assert_eq!(
        store
            .pending_replacement_run_ids_after("a", 1)
            .expect("terminal"),
        ["b"]
    );
    assert_eq!(
        store
            .pending_replacement_run_ids_after("b", 1)
            .expect("last"),
        ["c"]
    );
    assert!(
        store
            .pending_replacement_run_ids_after("c", 1)
            .expect("end")
            .is_empty()
    );
    assert!(store.pending_replacement_run_ids_after("", 0).is_err());
    assert!(
        store
            .pending_replacement_run_ids_after(&"x".repeat(MAX_ID_BYTES + 1), 1)
            .is_err()
    );
    assert_eq!(fixture_changes(&store.connection), before);
    assert!(store.run_summary("next-b").expect("no admission").is_none());
}

type ReplacementDamage = (
    &'static str,
    &'static str,
    &'static str,
    Box<dyn Expression>,
    bool,
);

#[test]
fn replacement_damaged_identity_and_payload_fail_closed_without_writes() {
    let (_temp, mut store, _, authority, _) = connected_publication_fixture();
    store
        .persist_definition("example", 1, &definition("example"))
        .expect("definition");
    let old = NewWorkflowRun {
        run_id: "old".into(),
        execution_authority: Some(authority.clone()),
        binding: Some(WorkflowRunBinding {
            owner_plugin_id: "test".into(),
            workflow_kind: "replacement".into(),
            scope_key: "scope".into(),
            display_label: None,
            single_active: true,
        }),
        ..new_run()
    };
    store.create_run(&old).expect("old");
    let successor = NewWorkflowRun {
        run_id: "successor".into(),
        ..old
    };
    store
        .request_replacement_owned("old", &authority, &successor, 20)
        .expect("intent");
    let damages: Vec<ReplacementDamage> = vec![
        (
            "different successor",
            "workflow_replacement_intents",
            "successor_run_id",
            DatabaseValue::String("different".into()).into(),
            false,
        ),
        (
            "malformed successor payload",
            "workflow_replacement_intents",
            "successor_json",
            DatabaseValue::String("{}".into()).into(),
            false,
        ),
        (
            "oversized blob successor payload",
            "workflow_replacement_intents",
            "successor_json",
            zero_blob(2_000_000).into(),
            false,
        ),
        (
            "changed scope",
            "workflow_runs",
            "scope_key",
            DatabaseValue::String("different".into()).into(),
            true,
        ),
        (
            "cleared cancellation",
            "workflow_runs",
            "cancellation_requested_at_ms",
            DatabaseValue::Null.into(),
            true,
        ),
    ];
    for (damage, table, column, value, scoped) in damages {
        let payload = serde_json::to_string(&successor).expect("payload");
        let mutation = update(table).value(column, value);
        let mutation = if scoped {
            mutation.where_eq("run_id", "old")
        } else {
            mutation
        };
        block_on(store.connection.exec_update_count(&mutation)).expect("damage");
        let before = fixture_changes(&store.connection);
        assert!(store.pending_replacement("old").is_err(), "{damage}");
        assert!(
            store
                .request_replacement_owned("old", &authority, &successor, 21)
                .is_err(),
            "retry must reject {damage}"
        );
        assert!(
            store
                .complete_leaf_replacement_owned("old", &authority, &successor, 21)
                .is_err(),
            "{damage}"
        );
        assert_eq!(fixture_changes(&store.connection), before);
        fixture_restore_replacement_intent(&store.connection, &payload);
        fixture_restore_replacement_run(&store.connection);
    }
    assert_eq!(
        store.pending_replacement("old").expect("intact"),
        Some(successor)
    );
}

fn fixture_restore_replacement_intent(db: &dyn Database, payload: &str) {
    block_on(
        update("workflow_replacement_intents")
            .value("successor_run_id", "successor")
            .value("successor_json", payload)
            .execute(db),
    )
    .expect("restore intent");
}

fn fixture_restore_replacement_run(db: &dyn Database) {
    block_on(
        update("workflow_runs")
            .value("scope_key", "scope")
            .value("cancellation_requested_at_ms", 20)
            .where_eq("run_id", "old")
            .execute(db),
    )
    .expect("restore run");
}

#[test]
fn replacement_process_loss_child() {
    let Ok(root) = std::env::var("BCODE_TEST_REPLACEMENT_CRASH_ROOT") else {
        return;
    };
    let mut store = WorkflowStore::open_in_state_dir(Path::new(&root)).expect("store");
    let authority = store
        .execution_authority("old")
        .expect("authority")
        .expect("owner");
    let successor = store
        .pending_replacement("old")
        .expect("pending")
        .expect("intent");
    if std::env::var("BCODE_TEST_REPLACEMENT_CRASH_PHASE").as_deref() != Ok("before_handoff") {
        assert!(
            store
                .complete_leaf_replacement_owned("old", &authority, &successor, 22)
                .expect("handoff")
        );
    }
    std::process::exit(73);
}

#[test]
fn replacement_process_loss_preserves_atomic_handoff() {
    for phase in ["before_handoff", "before_commit", "after_commit"] {
        let commit = phase == "after_commit";
        let (temp, mut store, _, authority, _) = connected_publication_fixture();
        store
            .persist_definition("example", 1, &definition("example"))
            .expect("definition");
        let old = NewWorkflowRun {
            run_id: "old".into(),
            execution_authority: Some(authority.clone()),
            binding: Some(WorkflowRunBinding {
                owner_plugin_id: "test".into(),
                workflow_kind: "replacement".into(),
                scope_key: "scope".into(),
                display_label: None,
                single_active: true,
            }),
            ..new_run()
        };
        store.create_run(&old).expect("old");
        let successor = NewWorkflowRun {
            run_id: "successor".into(),
            ..old
        };
        store
            .request_replacement_owned("old", &authority, &successor, 20)
            .expect("intent");
        block_on(
            update("workflow_runs")
                .value("status", "cancelled")
                .where_eq("run_id", "old")
                .execute(&store.connection),
        )
        .expect("settled fixture");
        drop(store);
        let status = std::process::Command::new(std::env::current_exe().expect("executable"))
            .args(["--exact", "tests::replacement_process_loss_child"])
            .env("BCODE_TEST_REPLACEMENT_CRASH_ROOT", temp.path())
            .env("BCODE_TEST_REPLACEMENT_CRASH_PHASE", phase)
            .status()
            .expect("child");
        assert_eq!(status.code(), Some(73));
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
        assert_eq!(
            store.pending_replacement("old").expect("intent").is_none(),
            commit
        );
        assert_eq!(
            store.run_summary("successor").expect("successor").is_some(),
            commit
        );
        assert_eq!(
            store
                .complete_leaf_replacement_owned("old", &authority, &successor, 23)
                .expect("retry"),
            !commit
        );
        let events = store.event_history("old", None, 100).expect("events");
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "replacement_completed")
                .count(),
            1
        );
        assert!(
            store
                .attempt_history("successor", None, 10)
                .expect("no implicit dispatch")
                .is_empty()
        );
        assert_eq!(
            store.run_summary("old").expect("old").expect("run").status,
            RunStatus::Cancelled
        );
    }
}

#[test]
fn replacement_admission_failure_rolls_back_reservation_release() {
    let (temp, mut store, _, authority, _) = connected_publication_fixture();
    store
        .persist_definition("example", 1, &definition("example"))
        .expect("definition");
    let old = NewWorkflowRun {
        run_id: "old".into(),
        execution_authority: Some(authority.clone()),
        binding: Some(WorkflowRunBinding {
            owner_plugin_id: "test".into(),
            workflow_kind: "replacement".into(),
            scope_key: "scope".into(),
            display_label: None,
            single_active: true,
        }),
        ..new_run()
    };
    store.create_run(&old).expect("old");
    let successor = NewWorkflowRun {
        run_id: "successor".into(),
        ..old.clone()
    };
    store
        .request_replacement_owned("old", &authority, &successor, 20)
        .expect("intent");
    block_on(
        update("workflow_runs")
            .value("status", "cancelled")
            .where_eq("run_id", "old")
            .execute(&store.connection),
    )
    .expect("fault fixture status");
    block_on(
        create_trigger("reject_successor", "workflow_runs")
            .before_insert()
            .when(E::new_column("run_id").compare(C::Equal, E::Text("successor".into())))
            .raise_abort("injected admission failure")
            .execute(&store.connection),
    )
    .expect("fault fixture");
    assert!(
        store
            .complete_leaf_replacement_owned("old", &authority, &successor, 21)
            .is_err()
    );
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
    assert_eq!(
        store.pending_replacement("old").expect("pending"),
        Some(successor.clone())
    );
    assert!(
        store
            .run_summary("successor")
            .expect("no partial run")
            .is_none()
    );
    let competitor = NewWorkflowRun {
        run_id: "competitor".into(),
        ..successor.clone()
    };
    assert!(store.create_run(&competitor).is_err());
    let events = store.event_history("old", None, 100).expect("history");
    assert!(
        !events
            .iter()
            .any(|event| event.event_type == "replacement_completed")
    );
    block_on(store.connection.drop_trigger("reject_successor", false)).expect("clear fault");
    assert!(
        store
            .complete_leaf_replacement_owned("old", &authority, &successor, 22)
            .expect("retry")
    );
    assert!(
        store
            .pending_replacement("old")
            .expect("consumed")
            .is_none()
    );
    let events = store.event_history("old", None, 100).expect("history");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "replacement_completed")
            .count(),
        1
    );
    assert!(store.create_run(&competitor).is_err());
}

#[test]
fn replacement_rebinds_only_coordinator_after_restart() {
    let (temp, mut store, run, authority, _) = connected_publication_fixture();
    let mut old = new_run();
    old.run_id = "replacement-old".into();
    old.execution_authority = Some(authority.clone());
    old.binding = Some(WorkflowRunBinding {
        owner_plugin_id: "test".into(),
        workflow_kind: "replace".into(),
        scope_key: "scope".into(),
        display_label: None,
        single_active: true,
    });
    store
        .persist_definition("example", 1, &definition("example"))
        .expect("definition");
    store.create_run(&old).expect("old");
    store
        .prepare_attempt(&PreparedAttempt {
            run_id: old.run_id.clone(),
            node_id: "review".into(),
            activation_id: activation_identity(&old.run_id, "review", 0),
            attempt: 1,
            side_effect: DispatchSideEffect::Mutating,
            intent: serde_json::json!({}),
            prepared_at_ms: 12,
        })
        .expect("unsettled work");
    let successor = NewWorkflowRun {
        run_id: "replacement-new".into(),
        ..old.clone()
    };
    store
        .request_replacement_owned(&old.run_id, &authority, &successor, 20)
        .expect("intent");
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
    let replacement = WorkflowExecutionAuthority {
        daemon_instance_id: "replacement-daemon".into(),
        generation: authority.generation + 1,
        fencing_token: "replacement-fence".into(),
        ..authority.clone()
    };
    store
        .transfer_execution_authority(&old.run_id, &authority, &replacement, 21)
        .expect("transfer");
    let mut pending = store
        .pending_replacement(&old.run_id)
        .expect("read")
        .expect("intent");
    assert_eq!(pending, successor);
    block_on(
        update("workflow_attempts")
            .value("status", "cancelled")
            .where_eq("run_id", old.run_id.as_str())
            .execute(&store.connection),
    )
    .expect("settled effects fixture");
    pending.execution_authority = Some(replacement.clone());
    block_on(
        update("workflow_runs")
            .value("status", "cancelled")
            .where_eq("run_id", old.run_id.as_str())
            .execute(&store.connection),
    )
    .expect("settled fixture");
    assert!(
        store
            .complete_leaf_replacement_owned(&old.run_id, &authority, &successor, 22)
            .is_err()
    );
    let mut changed = pending.clone();
    changed.workspace_snapshot = "different-workspace".into();
    assert!(
        store
            .complete_leaf_replacement_owned(&old.run_id, &replacement, &changed, 23)
            .is_err()
    );
    assert!(
        store
            .pending_replacement(&old.run_id)
            .expect("preserved")
            .is_some()
    );
    assert!(
        store
            .complete_leaf_replacement_owned(&old.run_id, &replacement, &pending, 24)
            .expect("handoff")
    );
    assert_eq!(
        store
            .execution_authority(&pending.run_id)
            .expect("successor owner"),
        Some(replacement)
    );
    assert!(
        store
            .pending_replacement(&old.run_id)
            .expect("consumed")
            .is_none()
    );
    assert!(store.run_summary(&run.run_id).expect("unrelated").is_some());
}

#[test]
fn replacement_intent_survives_restart_and_reserves_terminal_binding() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("example", 1, &definition("example"))
        .expect("definition");
    let authority = WorkflowExecutionAuthority {
        target_artifact_id: "artifact".into(),
        daemon_instance_id: "daemon".into(),
        generation: 1,
        fencing_token: "fence".into(),
    };
    let old = NewWorkflowRun {
        binding: Some(WorkflowRunBinding {
            owner_plugin_id: "test".into(),
            workflow_kind: "test".into(),
            scope_key: "session".into(),
            display_label: None,
            single_active: true,
        }),
        execution_authority: Some(authority.clone()),
        ..new_run()
    };
    store.create_run(&old).expect("old");
    let successor = NewWorkflowRun {
        run_id: "successor".into(),
        ..old.clone()
    };
    assert!(
        store
            .request_replacement_owned(&old.run_id, &authority, &successor, 20)
            .expect("intent")
    );
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
    assert!(
        !store
            .request_replacement_owned(&old.run_id, &authority, &successor, 21)
            .expect("retry")
    );
    assert_eq!(
        store
            .run_summary(&old.run_id)
            .expect("summary")
            .expect("old")
            .cancellation_requested_at_ms,
        Some(20)
    );
    assert_eq!(
        store.replacement_readiness(&old.run_id).expect("quiescent"),
        ReplacementReadiness::ReadyForAuthorization
    );
    let conflicting = NewWorkflowRun {
        run_id: "conflict".into(),
        ..successor.clone()
    };
    assert!(
        store
            .request_replacement_owned(&old.run_id, &authority, &conflicting, 22)
            .is_err()
    );
    block_on(
        update("workflow_runs")
            .value("status", "cancelled")
            .where_eq("run_id", old.run_id.as_str())
            .execute(&store.connection),
    )
    .expect("settled fixture");
    assert!(store.create_run(&successor).is_err());
    assert!(store.create_run(&conflicting).is_err());
    assert!(
        store
            .run_summary("successor")
            .expect("not admitted")
            .is_none()
    );
    assert!(
        store
            .complete_leaf_replacement_owned(&old.run_id, &authority, &conflicting, 23)
            .is_err()
    );
    assert!(
        store
            .complete_leaf_replacement_owned(&old.run_id, &authority, &successor, 24)
            .expect("handoff")
    );
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("restart handoff");
    assert!(
        !store
            .complete_leaf_replacement_owned(&old.run_id, &authority, &successor, 25)
            .expect("idempotent")
    );
    assert_replacement_running(&store);
    assert!(store.create_run(&conflicting).is_err());
}

fn assert_replacement_running(store: &WorkflowStore) {
    assert_eq!(
        store
            .run_summary("successor")
            .expect("summary")
            .expect("successor")
            .status,
        RunStatus::Running
    );
}

#[test]
fn detach_preserves_repair_state_and_releases_binding() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("example", 1, &definition("example"))
        .expect("definition");
    let authority = WorkflowExecutionAuthority {
        target_artifact_id: "artifact".into(),
        daemon_instance_id: "daemon".into(),
        generation: 1,
        fencing_token: "fence".into(),
    };
    let binding = WorkflowRunBinding {
        owner_plugin_id: "bcode.test".into(),
        workflow_kind: "test".into(),
        scope_key: "session-1".into(),
        display_label: None,
        single_active: true,
    };
    let run = NewWorkflowRun {
        binding: Some(binding.clone()),
        execution_authority: Some(authority.clone()),
        ..new_run()
    };
    store.create_run(&run).expect("run");
    assert!(store.detach_run(&run.run_id, 11).is_err());
    block_on(
        update("workflow_runs")
            .value("status", "repair_required")
            .where_eq("run_id", run.run_id.as_str())
            .execute(&store.connection),
    )
    .expect("repair fixture");
    assert!(store.detach_run(&run.run_id, 13).expect("detach"));
    assert!(!store.detach_run(&run.run_id, 14).expect("duplicate"));
    assert_eq!(
        store.execution_authority(&run.run_id).expect("authority"),
        Some(authority)
    );
    let key = WorkflowRunBindingKey {
        owner_plugin_id: binding.owner_plugin_id,
        workflow_kind: binding.workflow_kind,
        scope_key: binding.scope_key,
    };
    assert!(store.associated_run(&key).expect("lookup").is_none());
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    let detached = store.run_summary(&run.run_id).unwrap().unwrap();
    assert_eq!(detached.status, RunStatus::RepairRequired);
    assert_eq!(detached.parent_session_id, run.parent_session_id);
    assert!(detached.binding.is_none());
    store
        .create_run(&NewWorkflowRun {
            run_id: "fresh".into(),
            created_at_ms: 15,
            ..run
        })
        .expect("fresh run");
    assert_eq!(store.associated_run(&key).unwrap().unwrap().run_id, "fresh");
}

#[test]
fn associated_run_lookup_is_indexed_and_single_active_is_atomic() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("example", 1, &definition("example"))
        .expect("definition");
    let run = NewWorkflowRun {
        binding: Some(WorkflowRunBinding {
            owner_plugin_id: "bcode.test".to_string(),
            workflow_kind: "test.workflow".to_string(),
            scope_key: "session-1".to_string(),
            display_label: Some("Test workflow".to_string()),
            single_active: true,
        }),
        ..new_run()
    };
    store.create_run(&run).expect("run");
    let key = WorkflowRunBindingKey {
        owner_plugin_id: "bcode.test".to_string(),
        workflow_kind: "test.workflow".to_string(),
        scope_key: "session-1".to_string(),
    };
    let associated = store
        .associated_run(&key)
        .expect("lookup")
        .expect("associated run");
    assert_eq!(associated.run_id, run.run_id);
    assert_eq!(associated.binding, run.binding);

    let conflict = NewWorkflowRun {
        run_id: "run-2".to_string(),
        execution_authority: None,
        created_at_ms: 15,
        ..run.clone()
    };
    let error = store.create_run(&conflict).expect_err("single active");
    assert!(matches!(
        error,
        WorkflowStoreError::ActiveBindingConflict { .. }
    ));

    assert!(store.pause_run(&run.run_id, 12).expect("pause"));
    assert!(store.resume_run(&run.run_id, 13).expect("resume"));
    assert!(store.request_cancellation(&run.run_id, 14).expect("cancel"));
    assert_eq!(
        store
            .run_summary(&run.run_id)
            .expect("summary")
            .expect("run")
            .status,
        RunStatus::Cancelled
    );
    store
        .create_run(&conflict)
        .expect("replacement after terminal");
    assert_eq!(
        store
            .associated_run(&key)
            .expect("lookup")
            .expect("latest")
            .run_id,
        "run-2"
    );
}

#[test]
fn terminal_descendant_proof_takeover_is_fenced_recovery_only_and_durable() {
    let (temp, mut store) = initialized_store();
    let old = WorkflowExecutionAuthority {
        target_artifact_id: "old".into(),
        daemon_instance_id: "ended".into(),
        generation: 1,
        fencing_token: "old-fence".into(),
    };
    store
        .create_run(&NewWorkflowRun {
            run_id: "child".into(),
            execution_authority: Some(old.clone()),
            ..new_run()
        })
        .expect("child");
    let identity = bcode_workflow::WorkflowDefinitionIdentity::for_definition(
        "example",
        &definition("example"),
    )
    .expect("identity");
    let target =
        serde_json::to_string(&bcode_workflow::WorkflowCallTarget::Definition { identity })
            .expect("target");
    block_on(
        insert("workflow_run_links")
            .value("root_run_id", "run-1")
            .value("parent_run_id", "run-1")
            .value("parent_node_id", "review")
            .value("parent_activation_id", "activation")
            .value("parent_attempt", 1)
            .value("child_run_id", "child")
            .value("version", WORKFLOW_RUN_LINK_VERSION)
            .value("target_json", target)
            .value("depth", 2)
            .value("created_at_ms", 0)
            .execute(&store.connection),
    )
    .expect("link");
    store
        .request_cancellation_owned("child", 20, &old)
        .expect("terminal");
    let next = WorkflowExecutionAuthority {
        target_artifact_id: "new".into(),
        daemon_instance_id: "next".into(),
        generation: 2,
        fencing_token: "new-fence".into(),
    };
    let mut evidence = EndedOwnerEvidence {
        ended_daemon_instance_id: old.daemon_instance_id.clone(),
        ended_target_artifact_id: old.target_artifact_id.clone(),
        liveness: EndedOwnerLiveness::NoLiveTrace,
        artifact_image_available: false,
    };
    assert!(
        store
            .take_descendant_proof_authority("child", &old, &next, &evidence, 21)
            .is_err()
    );
    evidence.liveness = EndedOwnerLiveness::ObservedEnded;
    store
        .take_descendant_proof_authority("child", &old, &next, &evidence, 22)
        .expect("takeover");
    assert!(
        store
            .advance_subtree_quiescence_owned("child", &old, 1)
            .is_err()
    );
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
    assert!(store.is_recovery_only("child").expect("barrier"));
    assert_eq!(
        store
            .run_summary("child")
            .expect("summary")
            .expect("child")
            .status,
        RunStatus::Cancelled
    );
    assert!(
        !store
            .advance_subtree_quiescence_owned("child", &next, 1)
            .expect("page")
    );
    assert!(
        store
            .advance_subtree_quiescence_owned("child", &next, 1)
            .expect("proof")
    );
    assert!(recovery::require_execution(&store.connection, "child").is_err());
}

#[test]
fn subtree_quiescence_is_incremental_restart_safe_and_freezes_verified_work() {
    let (temp, mut store) = initialized_store();
    let owner = WorkflowExecutionAuthority {
        target_artifact_id: "a".into(),
        daemon_instance_id: "d".into(),
        generation: 1,
        fencing_token: "f".into(),
    };
    block_on(
        update("workflow_runs")
            .value("target_artifact_id", "a")
            .value("coordinator_daemon_instance_id", "d")
            .value("coordinator_generation", 1)
            .value("coordinator_fencing_token", "f")
            .value("status", "cancelled")
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("parent fixture");
    let child = NewWorkflowRun {
        run_id: "child".into(),
        execution_authority: Some(owner.clone()),
        ..new_run()
    };
    store.create_run(&child).expect("child");
    fixture_link_quiescence_child(&store.connection);
    assert!(
        !store
            .advance_subtree_quiescence_owned("run-1", &owner, 1)
            .expect("attempt page")
    );
    assert!(
        !store
            .advance_subtree_quiescence_owned("run-1", &owner, 1)
            .expect("child blocks")
    );
    assert!(
        !store
            .advance_subtree_quiescence_owned("child", &owner, 1)
            .expect("active child")
    );
    store
        .request_cancellation_owned("child", 20, &owner)
        .expect("cancel child");
    assert!(
        !store
            .advance_subtree_quiescence_owned("child", &owner, 1)
            .expect("child attempts")
    );
    assert!(
        store
            .advance_subtree_quiescence_owned("child", &owner, 1)
            .expect("child proven")
    );
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
    assert!(
        !store
            .advance_subtree_quiescence_owned("run-1", &owner, 1)
            .expect("child page")
    );
    assert!(
        store
            .advance_subtree_quiescence_owned("run-1", &owner, 1)
            .expect("parent proven")
    );
    assert!(
        block_on(
            update("workflow_runs")
                .value("status", "running")
                .where_eq("run_id", "child")
                .execute(&store.connection)
        )
        .is_err()
    );
    assert!(
        block_on(
            delete("workflow_run_links")
                .where_eq("parent_run_id", "run-1")
                .execute(&store.connection)
        )
        .is_err()
    );
    assert!(
        store
            .advance_subtree_quiescence_owned(
                "run-1",
                &WorkflowExecutionAuthority {
                    fencing_token: "stale".into(),
                    ..owner
                },
                1
            )
            .is_err()
    );
}

fn fixture_link_quiescence_child(db: &dyn Database) {
    block_on(
        insert("workflow_run_links")
            .value("root_run_id", "run-1")
            .value("parent_run_id", "run-1")
            .value("parent_node_id", "review")
            .value("parent_activation_id", "activation")
            .value("parent_attempt", 1)
            .value("child_run_id", "child")
            .value("version", 1)
            .value("target_json", "{}")
            .value("depth", 2)
            .value("created_at_ms", 0)
            .execute(db),
    )
    .expect("link fixture");
}

#[test]
fn descendant_summaries_reject_invalid_links_without_mutation() {
    let (_temp, mut store) = initialized_store();
    let mut child = new_run();
    child.run_id = "child-run".to_string();
    store.create_run(&child).expect("child");
    let identity = bcode_workflow::WorkflowDefinitionIdentity::for_definition(
        "example",
        &definition("example"),
    )
    .expect("identity");
    let target =
        serde_json::to_string(&bcode_workflow::WorkflowCallTarget::Definition { identity })
            .expect("target");
    block_on(
        insert("workflow_run_links")
            .value("root_run_id", "run-1")
            .value("parent_run_id", "run-1")
            .value("parent_node_id", "review")
            .value("parent_activation_id", "activation")
            .value("parent_attempt", 1)
            .value("child_run_id", "child-run")
            .value("version", WORKFLOW_RUN_LINK_VERSION)
            .value("target_json", target.as_str())
            .value("depth", 2)
            .value("created_at_ms", 0)
            .execute(&store.connection),
    )
    .expect("link fixture");
    assert_eq!(
        store
            .descendant_run_summaries("run-1", 10)
            .expect("valid link")
            .len(),
        1
    );
    let mutations: Vec<(&str, Box<dyn Expression>)> = vec![
        ("version", (WORKFLOW_RUN_LINK_VERSION + 1).into()),
        ("parent_node_id", "".into()),
        ("target_json", "not-json".into()),
    ];
    for (column, value) in mutations {
        block_on(
            update("workflow_run_links")
                .value(column, value)
                .execute(&store.connection),
        )
        .expect("damage fixture");
        let before = fixture_changes(&store.connection);
        assert!(store.descendant_run_summaries("run-1", 10).is_err());
        assert!(store.child_run_links("run-1", 10).is_err());
        assert_eq!(fixture_changes(&store.connection), before);
        block_on(
            update("workflow_run_links")
                .value("version", WORKFLOW_RUN_LINK_VERSION)
                .value("parent_node_id", "review")
                .value("target_json", target.as_str())
                .execute(&store.connection),
        )
        .expect("restore fixture");
    }
    block_on(store.connection.sqlite_foreign_keys(false)).expect("disable fixture foreign keys");
    block_on(
        update("workflow_run_links")
            .value("child_run_id", "missing-child")
            .execute(&store.connection),
    )
    .expect("missing child fixture");
    block_on(store.connection.sqlite_foreign_keys(true)).expect("restore fixture foreign keys");
    let before = fixture_changes(&store.connection);
    assert_eq!(
        store
            .child_run_links("run-1", 10)
            .expect("link remains")
            .len(),
        1
    );
    let error = store
        .descendant_run_summaries("run-1", 10)
        .expect_err("missing child");
    assert!(
        error
            .to_string()
            .contains("workflow run link references a missing child run")
    );
    assert_eq!(fixture_changes(&store.connection), before);
}

#[test]
fn descendant_count_saturates_at_limit_and_is_root_scoped() {
    let temp = tempfile::tempdir().expect("fixture directory");
    let connection = fixture_open(&temp.path().join("descendants.db"), false);
    block_on(
        switchy_database::schema::create_table("workflow_run_links")
            .column(switchy_database::schema::Column {
                name: "root_run_id".into(),
                nullable: false,
                auto_increment: false,
                data_type: switchy_database::schema::DataType::Text,
                default: None,
            })
            .execute(&connection),
    )
    .expect("count fixture table");
    for root in ["root", "root", "root", "other"] {
        block_on(
            insert("workflow_run_links")
                .value("root_run_id", root)
                .execute(&connection),
        )
        .expect("count fixture");
    }
    let before = fixture_changes(&connection);
    for (root, limit, expected) in [
        ("root", 0, 0),
        ("root", 1, 1),
        ("root", 2, 2),
        ("root", 3, 3),
        ("root", 4, 3),
        ("other", 4, 1),
        ("missing", 4, 0),
    ] {
        assert_eq!(
            bounded_descendant_count(&connection, root, limit).expect("count"),
            expected
        );
    }
    assert_eq!(fixture_changes(&connection), before);
}

#[test]
fn run_link_decoder_rejects_child_aliasing_its_ancestors() {
    let (_temp, store) = initialized_store();
    let before = fixture_changes(&store.connection);
    for child in ["root", "parent"] {
        let result = decode_run_link(&fixture_link_row(
            ["root", "parent", "call", "activation", child],
            1,
            WORKFLOW_RUN_LINK_VERSION,
            "not-json",
            2,
        ));
        assert!(
            matches!(result, Err(WorkflowStoreError::InvalidData(message)) if message.contains("aliases its root or parent"))
        );
    }
    assert_eq!(fixture_changes(&store.connection), before);
}

#[test]
fn run_link_decoder_validates_attempt_and_depth_without_a_recursion_ceiling() {
    let (_temp, store) = initialized_store();
    let identity =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("child", &definition("child"))
            .expect("identity");
    let target =
        serde_json::to_string(&bcode_workflow::WorkflowCallTarget::Definition { identity })
            .expect("target");
    let before = fixture_changes(&store.connection);
    for (attempt, depth, valid) in [
        (0, 2, false),
        (1, 0, false),
        (1, 1, false),
        (1, 2, true),
        (u32::MAX, u32::MAX, true),
    ] {
        let result = decode_run_link(&fixture_link_row(
            ["root", "parent", "call", "activation", "child"],
            attempt,
            WORKFLOW_RUN_LINK_VERSION,
            &target,
            depth,
        ));
        assert_eq!(result.is_ok(), valid, "attempt {attempt}, depth {depth}");
    }
    assert_eq!(fixture_changes(&store.connection), before);
}

#[test]
fn run_link_decoder_rejects_invalid_ids_before_target_decoding() {
    let (_temp, store) = initialized_store();
    let before = fixture_changes(&store.connection);
    for column in [0, 1, 2, 3, 5] {
        let mut ids = ["root", "parent", "call", "activation", "child"];
        let index = if column == 5 { 4 } else { column };
        ids[index] = "";
        let result = decode_run_link(&fixture_link_row(
            ids,
            1,
            WORKFLOW_RUN_LINK_VERSION,
            "not-json",
            2,
        ));
        let column_name = [
            "root_run_id",
            "parent_run_id",
            "parent_node_id",
            "parent_activation_id",
            "child_run_id",
        ][index];
        assert!(
            matches!(result, Err(WorkflowStoreError::InvalidData(message)) if message.contains(column_name))
        );
    }
    assert_eq!(fixture_changes(&store.connection), before);
}

#[test]
fn run_link_decoder_rejects_invalid_target_identity() {
    let (_temp, store) = initialized_store();
    let mut identity =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("child", &definition("child"))
            .expect("identity");
    identity.definition_version = 0;
    let target =
        serde_json::to_string(&bcode_workflow::WorkflowCallTarget::Definition { identity })
            .expect("serializable invalid target");
    let before = fixture_changes(&store.connection);
    let result = decode_run_link(&fixture_link_row(
        ["root", "parent", "call", "activation", "child"],
        1,
        WORKFLOW_RUN_LINK_VERSION,
        &target,
        2,
    ));
    assert!(matches!(result, Err(WorkflowStoreError::InvalidData(_))));
    assert_eq!(fixture_changes(&store.connection), before);
}

#[test]
fn run_link_decoder_rejects_oversized_target_before_deserialization() {
    let (_temp, store) = initialized_store();
    let target = " ".repeat(MAX_INLINE_JSON_BYTES + 1);
    let before = fixture_changes(&store.connection);
    let result = decode_run_link(&fixture_link_row(
        ["root", "parent", "call", "activation", "child"],
        1,
        WORKFLOW_RUN_LINK_VERSION,
        &target,
        2,
    ));
    let error = result.expect_err("oversized target");
    assert!(error.to_string().contains("inline byte limit"));
    assert_eq!(fixture_changes(&store.connection), before);
}

#[test]
fn run_link_decoder_rejects_future_version_before_target_decoding() {
    let (_temp, store) = initialized_store();
    let before = fixture_changes(&store.connection);
    let result = decode_run_link(&fixture_link_row(
        ["root", "parent", "call", "activation", "child"],
        1,
        WORKFLOW_RUN_LINK_VERSION + 1,
        "not-json",
        2,
    ));
    assert!(
        matches!(result, Err(WorkflowStoreError::InvalidData(message)) if message.contains("unsupported workflow run link version"))
    );
    assert_eq!(fixture_changes(&store.connection), before);
}

#[test]
fn duplicate_run_verification_never_creates_missing_run() {
    let (_temp, mut store) = initialized_store();
    let mut request = new_run();
    request.run_id = "missing-linked-child".to_string();
    let before = fixture_changes(&store.connection);
    let transaction = database::begin(&store.connection, TransactionMode::Immediate)
        .expect("verification transaction");
    assert!(matches!(
        WorkflowStore::create_or_verify_run(transaction.as_ref(), &request, false),
        Err(WorkflowStoreError::InvalidData(_))
    ));
    database::commit(transaction).expect("commit verification");
    assert_eq!(fixture_changes(&store.connection), before);
    assert!(
        store
            .run_summary(&request.run_id)
            .expect("missing run")
            .is_none()
    );
    assert!(
        store
            .create_run_idempotent(&request)
            .expect("explicit creation still works")
    );
}

#[test]
fn concurrent_identical_run_creation_has_one_creator_and_one_duplicate() {
    let (temp, mut first) = initialized_store();
    let mut second = WorkflowStore::open_in_state_dir(temp.path()).expect("second store");
    let barrier = std::sync::Barrier::new(2);
    let mut request = new_run();
    request.run_id = "concurrent-start".to_string();
    let mut outcomes = std::thread::scope(|scope| {
        let left = scope.spawn(|| {
            barrier.wait();
            first.create_run_idempotent(&request).expect("first start")
        });
        let right = scope.spawn(|| {
            barrier.wait();
            second
                .create_run_idempotent(&request)
                .expect("second start")
        });
        [
            left.join().expect("first caller"),
            right.join().expect("second caller"),
        ]
    });
    outcomes.sort_unstable();
    assert_eq!(outcomes, [false, true]);
    assert!(
        !first
            .create_run_idempotent(&request)
            .expect("subsequent duplicate")
    );
}

#[test]
fn concurrent_conflicting_run_creation_preserves_winner_after_reopen() {
    let (temp, mut first) = initialized_store();
    let mut second = WorkflowStore::open_in_state_dir(temp.path()).expect("second store");
    let barrier = std::sync::Barrier::new(2);
    let mut left_request = new_run();
    left_request.run_id = "conflicting-start".to_string();
    let mut right_request = left_request.clone();
    right_request.input = Some(serde_json::json!(2));
    let (left, right) = std::thread::scope(|scope| {
        let left = scope.spawn(|| {
            barrier.wait();
            first.create_run_idempotent(&left_request)
        });
        let right = scope.spawn(|| {
            barrier.wait();
            second.create_run_idempotent(&right_request)
        });
        (
            left.join().expect("left caller"),
            right.join().expect("right caller"),
        )
    });
    let (winner, loser) = match (left, right) {
        (Ok(true), Err(WorkflowStoreError::InvalidData(_))) => (&left_request, &right_request),
        (Err(WorkflowStoreError::InvalidData(_)), Ok(true)) => (&right_request, &left_request),
        other => panic!("expected one creation and one identity conflict: {other:?}"),
    };
    drop(first);
    drop(second);
    let mut reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    let before = fixture_changes(&reopened.connection);
    assert!(
        !reopened
            .create_run_idempotent(winner)
            .expect("durable winner")
    );
    assert!(matches!(
        reopened.create_run_idempotent(loser),
        Err(WorkflowStoreError::InvalidData(_))
    ));
    assert_eq!(fixture_changes(&reopened.connection), before);
}

#[test]
fn stable_run_creation_is_idempotent_and_checks_all_immutable_context() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("example", 1, &definition("example"))
        .expect("definition");
    let run = new_run();
    assert!(store.create_run_idempotent(&run).expect("create"));
    assert!(!store.create_run_idempotent(&run).expect("idempotent"));
    assert_eq!(store.list_runs(10).expect("runs").len(), 1);
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    let restored = store
        .run_summary(&run.run_id)
        .expect("restored run")
        .expect("run");
    assert_eq!(restored.authorization_ceiling, run.authorization_ceiling);
    assert!(
        !store
            .create_run_idempotent(&run)
            .expect("restart idempotent")
    );

    for conflict in [
        NewWorkflowRun {
            authorization_ceiling: bcode_workflow::WorkflowToolCapability::ReadOnly,
            ..run.clone()
        },
        NewWorkflowRun {
            authored_provenance: None,
            input: Some(serde_json::json!(2)),
            ..run.clone()
        },
        NewWorkflowRun {
            authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                version: 1,
                provider_id: "test-policy".to_string(),
                profile_id: "build".to_string(),
                policy_digest_sha256: "a".repeat(64),
            },
            authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
            limits: WorkflowRunLimits {
                retry_cap: run.limits.retry_cap + 1,
                ..run.limits.clone()
            },
            ..run.clone()
        },
        NewWorkflowRun {
            workspace_snapshot: "snapshot-2".to_string(),
            ..run.clone()
        },
    ] {
        assert!(store.create_run_idempotent(&conflict).is_err());
    }
    assert_eq!(store.list_runs(10).expect("runs").len(), 1);
}

fn activation_id() -> String {
    activation_identity("run-1", "review", 0)
}

fn new_activation() -> NewActivation {
    NewActivation {
        run_id: "run-1".to_string(),
        node_id: "review".to_string(),
        activation_id: activation_id(),
        dependency_generation: 0,
        input: Some(serde_json::json!(1)),
        created_at_ms: 11,
    }
}

#[test]
fn retained_handle_preserves_fence_after_origin_drop_and_rejects_missing_database() {
    let (temp, store) = initialized_store();
    let retained = store.retained_handle().expect("retain");
    let path = store.path().to_path_buf();
    let fence = store.publication_read_fence().expect("fence");
    assert!(!std::sync::Arc::ptr_eq(
        &fence.identity,
        &retained.read_identity
    ));
    drop(store);
    assert!(retained.run_summary("run-1").unwrap().is_some());
    let lock = open_ownership_file(path.parent().unwrap()).unwrap();
    assert!(lock.try_lock().is_err());
    drop(retained);
    lock.try_lock()
        .expect("last retained connection releases ownership");
    drop(lock);
    let store = WorkflowStore::open_in_state_dir(temp.path()).unwrap();
    let moved = path.with_extension("retention-test");
    std::fs::rename(&path, &moved).unwrap();
    assert!(store.retained_handle().is_err());
    assert!(
        !path.exists(),
        "retention must never create a replacement database"
    );
    std::fs::rename(moved, path).unwrap();
    assert!(
        WorkflowStore::unavailable_placeholder()
            .unwrap()
            .retained_handle()
            .is_err()
    );
}

#[test]
fn baseline_uncapped_admission_is_typed_and_non_mutating() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::create_coexistence_baseline(temp.path()).expect("baseline");
    store
        .persist_definition("example", 1, &definition("example"))
        .expect("definition");
    let mut run = new_run();
    assert_eq!(run.limits.concurrency_cap, None);
    let before = fixture_changes(&store.connection);
    let columns = block_on(store.connection.get_table_columns("workflow_runs")).unwrap();
    assert!(
        columns
            .iter()
            .any(|column| column.name == "concurrency_cap" && !column.nullable)
    );
    assert_eq!(detected_typed_store_schema(&store.connection), Some(42));
    assert!(matches!(
        store.create_run(&run),
        Err(WorkflowStoreError::UnsupportedUncappedConcurrency)
    ));
    assert_eq!(fixture_changes(&store.connection), before);
    assert!(matches!(
        store.create_run_idempotent(&run),
        Err(WorkflowStoreError::UnsupportedUncappedConcurrency)
    ));
    assert_eq!(fixture_changes(&store.connection), before);
    assert!(store.run_limits(&run.run_id).unwrap().is_none());
    assert_eq!(
        block_on(store.connection.get_table_columns("workflow_runs")).unwrap(),
        columns
    );
    assert_eq!(detected_typed_store_schema(&store.connection), Some(42));

    // A compatible request still succeeds while an independent handle retains ownership.
    let observer = WorkflowStore::open_in_state_dir(temp.path()).expect("observer");
    run.limits.concurrency_cap = Some(7);
    store.create_run(&run).expect("finite baseline admission");
    assert_eq!(
        observer.run_limits(&run.run_id).unwrap().unwrap(),
        run.limits
    );
    assert_eq!(detected_typed_store_schema(&observer.connection), Some(42));
}

#[test]
fn current_schema_accepts_uncapped_default_limits_without_rewriting_policy() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("current store");
    store
        .persist_definition("example", 1, &definition("example"))
        .expect("definition");
    assert_eq!(detected_typed_store_schema(&store.connection), Some(43));
    let run = new_run();
    assert_eq!(run.limits.concurrency_cap, None);
    store.create_run(&run).expect("uncapped admission");
    let before_duplicate = fixture_changes(&store.connection);
    assert!(!store.create_run_idempotent(&run).expect("exact duplicate"));
    assert_eq!(fixture_changes(&store.connection), before_duplicate);
    drop(store);
    let reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    assert_eq!(
        reopened.run_limits(&run.run_id).unwrap().unwrap(),
        run.limits
    );
    assert_eq!(
        reopened
            .concurrency_snapshot(&run.run_id)
            .unwrap()
            .concurrency_cap,
        None
    );
    assert_eq!(detected_typed_store_schema(&reopened.connection), Some(43));
}

fn initialized_store() -> (tempfile::TempDir, WorkflowStore) {
    let temp = tempfile::tempdir().expect("temp");
    let store = initialized_store_at(temp.path());
    (temp, store)
}

fn initialized_store_at(state_dir: &Path) -> WorkflowStore {
    let mut store = WorkflowStore::open_in_state_dir(state_dir).expect("store");
    store
        .persist_definition("example", 1, &definition("example"))
        .expect("definition");
    store.create_run(&new_run()).expect("run");
    store
}

fn prepare_receipt_backed_attempt(
    store: &mut WorkflowStore,
    side_effect: DispatchSideEffect,
) -> String {
    let attempt = PreparedAttempt {
        run_id: "run-1".to_string(),
        node_id: "review".to_string(),
        activation_id: activation_id(),
        attempt: 1,
        side_effect,
        intent: serde_json::json!({"operation": "review"}),
        prepared_at_ms: 12,
    };
    let identity = store.prepare_attempt(&attempt).expect("prepare");
    store
        .persist_dispatch_receipt(&DispatchReceipt {
            run_id: attempt.run_id,
            node_id: attempt.node_id,
            activation_id: attempt.activation_id,
            attempt: 1,
            dispatch_identity: identity.clone(),
            receipt: serde_json::json!({"turn_id": "turn-1"}),
            admitted_at_ms: 13,
        })
        .expect("receipt");
    identity
}

#[test]
fn activation_summaries_inline_small_inputs_and_omit_large_values() {
    let (_temp, store) = initialized_store();
    let large_input =
        serde_json::Value::String("x".repeat(WORKFLOW_ACTIVATION_INPUT_SUMMARY_MAX_BYTES + 1));
    block_on(
        update("workflow_activations")
            .value(
                "input_json",
                serde_json::to_string(&large_input).expect("large JSON"),
            )
            .where_eq("run_id", "run-1")
            .where_eq("node_id", "review")
            .where_eq("activation_id", activation_id())
            .execute(&store.connection),
    )
    .expect("replace canonical input fixture");

    let activations = store.activations_for_run("run-1", 10).expect("summaries");
    assert_eq!(activations.len(), 1);
    assert!(matches!(
        activations[0].input_summary,
        WorkflowActivationInputSummary::Omitted { byte_count }
            if byte_count == serde_json::to_string(&large_input).expect("large JSON").len()
    ));

    block_on(
        update("workflow_activations")
            .value("input_json", "1")
            .where_eq("run_id", "run-1")
            .where_eq("node_id", "review")
            .where_eq("activation_id", activation_id())
            .execute(&store.connection),
    )
    .expect("replace small canonical input fixture");
    let activations = store
        .activations_for_run("run-1", 10)
        .expect("small summary");
    assert_eq!(
        activations[0].input_summary,
        WorkflowActivationInputSummary::Inline {
            value: serde_json::json!(1)
        }
    );
}

#[tokio::test]
async fn run_scoped_dispatch_ignores_unrelated_pending_activation() {
    use std::sync::Mutex;

    struct Owner(Mutex<Vec<String>>);
    impl ActivationDispatchOwner for Owner {
        fn plan<'a>(
            &'a self,
            _activation: &'a PendingActivation,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<Option<ActivationDispatchPlan>, WorkflowStoreError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                Ok(Some(ActivationDispatchPlan {
                    side_effect: DispatchSideEffect::ReadOnly,
                    intent: serde_json::json!({"operation": "review"}),
                }))
            })
        }

        fn dispatch<'a>(
            &'a self,
            request: &'a PreparedActivationDispatch,
        ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, WorkflowStoreError>> + Send + 'a>>
        {
            Box::pin(async move {
                self.0
                    .lock()
                    .expect("observed runs")
                    .push(request.activation.run_id.clone());
                Ok(serde_json::json!({"accepted": true}))
            })
        }
    }

    let (_temp, mut store) = initialized_store();
    store
        .create_run(&NewWorkflowRun {
            run_id: "run-2".to_string(),
            execution_authority: None,
            created_at_ms: 20,
            ..new_run()
        })
        .expect("second run");
    let owner = Owner(Mutex::new(Vec::new()));

    let summary = store
        .dispatch_pending_activations_for_run(&owner, "run-2", 10, 21)
        .await
        .expect("run-scoped dispatch");

    assert_eq!(summary.admitted.len(), 1);
    assert_eq!(*owner.0.lock().expect("observed runs"), ["run-2"]);
    assert_eq!(
        store
            .pending_activations_for_run("run-1", 10)
            .expect("first pending")
            .len(),
        1
    );
    assert!(
        store
            .pending_activations_for_run("run-2", 10)
            .expect("second pending")
            .is_empty()
    );
}

#[tokio::test]
async fn owner_dispatch_persists_intent_before_call_and_receipt_after_acceptance() {
    use std::sync::Mutex;

    struct Owner {
        store_path: PathBuf,
        observed_status: Mutex<Option<String>>,
    }
    impl ActivationDispatchOwner for Owner {
        fn plan<'a>(
            &'a self,
            activation: &'a PendingActivation,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<Option<ActivationDispatchPlan>, WorkflowStoreError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                Ok(
                    (activation.node.kind == bcode_workflow::NodeKind::Task).then(|| {
                        ActivationDispatchPlan {
                            side_effect: DispatchSideEffect::ReadOnly,
                            intent: serde_json::json!({"operation": "review"}),
                        }
                    }),
                )
            })
        }

        fn dispatch<'a>(
            &'a self,
            request: &'a PreparedActivationDispatch,
        ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, WorkflowStoreError>> + Send + 'a>>
        {
            Box::pin(async move {
                let connection = fixture_open(&self.store_path, true);
                let rows = database::rows(
                    &connection,
                    &select("workflow_attempts")
                        .columns(&["status"])
                        .where_eq("dispatch_identity", request.dispatch_identity.as_str()),
                )?;
                let status = database::text(
                    rows.first().ok_or_else(|| {
                        WorkflowStoreError::InvalidData("prepared attempt missing".into())
                    })?,
                    "status",
                )?;
                *self.observed_status.lock().expect("observed status") = Some(status);
                Ok(serde_json::json!({
                    "work_id": request.dispatch_identity,
                    "owner": "test"
                }))
            })
        }
    }

    let (temp, mut store) = initialized_store();
    let owner = Owner {
        store_path: workflow_database_path(temp.path()),
        observed_status: Mutex::new(None),
    };
    let summary = store
        .dispatch_pending_activations(&owner, 10, 20)
        .await
        .expect("dispatch");
    assert_eq!(summary.admitted.len(), 1);
    assert!(summary.unsupported.is_empty());
    assert_eq!(
        owner.observed_status.lock().expect("status").as_deref(),
        Some("prepared")
    );
    let attempt = store
        .attempt_history("run-1", None, 10)
        .expect("attempt")
        .pop()
        .expect("attempt row");
    assert_eq!(attempt.status, "admitted");
    assert!(attempt.has_receipt);
}

#[tokio::test]
async fn dispatch_failure_stays_prepared_and_is_not_redispatched() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Owner(AtomicUsize);
    impl ActivationDispatchOwner for Owner {
        fn plan<'a>(
            &'a self,
            _activation: &'a PendingActivation,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<Option<ActivationDispatchPlan>, WorkflowStoreError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                Ok(Some(ActivationDispatchPlan {
                    side_effect: DispatchSideEffect::Mutating,
                    intent: serde_json::json!({"operation": "apply"}),
                }))
            })
        }

        fn dispatch<'a>(
            &'a self,
            _request: &'a PreparedActivationDispatch,
        ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, WorkflowStoreError>> + Send + 'a>>
        {
            Box::pin(async move {
                self.0.fetch_add(1, Ordering::SeqCst);
                Err(WorkflowStoreError::InvalidData(
                    "owner acceptance unknown".to_string(),
                ))
            })
        }
    }

    let (_temp, mut store) = initialized_store();
    let owner = Owner(AtomicUsize::new(0));
    assert!(
        store
            .dispatch_pending_activations(&owner, 10, 20)
            .await
            .is_err()
    );
    assert_eq!(owner.0.load(Ordering::SeqCst), 1);
    assert!(store.pending_activations(10).expect("pending").is_empty());
    assert!(
        store
            .dispatch_pending_activations(&owner, 10, 21)
            .await
            .expect("retry scan")
            .admitted
            .is_empty()
    );
    assert_eq!(owner.0.load(Ordering::SeqCst), 1);
    let attempt = store
        .attempt_history("run-1", None, 10)
        .expect("attempt")
        .pop()
        .expect("attempt row");
    assert_eq!(attempt.status, "prepared");
    assert!(!attempt.has_receipt);
}

#[test]
fn retained_handle_refuses_tree_cancellation_after_contract_change() {
    let (_temp, mut store) = initialized_store();
    let foreign = fixture_open(store.path(), true);
    block_on(
        update("workflow_storage_compatibility")
            .value("compatibility", WORKFLOW_STORE_COMPATIBILITY + 1)
            .where_eq("contract_id", 1)
            .execute(&foreign),
    )
    .expect("incompatible contract");
    let error = store
        .request_cancellation_tree("run-1", 12)
        .expect_err("retained handle must refuse tree cancellation");
    assert!(matches!(error, WorkflowStoreError::UnsupportedStore { .. }));
    let intent_rows = fixture_rows(
        &foreign,
        &select("workflow_runs")
            .columns(&["cancellation_requested_at_ms"])
            .where_eq("run_id", "run-1"),
    );
    let intent =
        database::optional_unsigned(intent_rows.first().unwrap(), "cancellation_requested_at_ms")
            .expect("cancellation intent");
    assert_eq!(intent, None);
}

#[test]
fn retained_handle_refuses_attempt_observation_after_contract_change() {
    for owned in [false, true] {
        let (_temp, mut store) = initialized_store();
        let id = activation_id();
        let prepared = store
            .prepare_pending_activation(
                "run-1",
                "review",
                &id,
                DispatchSideEffect::ReadOnly,
                serde_json::json!({}),
                12,
            )
            .expect("prepare")
            .expect("pending");
        let authority = WorkflowExecutionAuthority {
            target_artifact_id: "artifact-a".into(),
            daemon_instance_id: "daemon-a".into(),
            generation: 1,
            fencing_token: "token-a".into(),
        };
        block_on(
            update("workflow_runs")
                .value("target_artifact_id", "artifact-a")
                .value("coordinator_daemon_instance_id", "daemon-a")
                .value("coordinator_generation", 1)
                .value("coordinator_fencing_token", "token-a")
                .where_eq("run_id", "run-1")
                .execute(&store.connection),
        )
        .expect("authority");
        let receipt = DispatchReceipt {
            run_id: "run-1".into(),
            node_id: "review".into(),
            activation_id: id,
            attempt: prepared.attempt,
            dispatch_identity: prepared.dispatch_identity.clone(),
            receipt: serde_json::json!({"accepted": true}),
            admitted_at_ms: 13,
        };
        store
            .persist_owned_dispatch_receipt(&receipt, &authority)
            .expect("receipt");
        let foreign = fixture_open(store.path(), true);
        block_on(
            update("workflow_storage_compatibility")
                .value("compatibility", WORKFLOW_STORE_COMPATIBILITY + 1)
                .where_eq("contract_id", 1)
                .execute(&foreign),
        )
        .expect("incompatible contract");
        let error = if owned {
            store.apply_owned_attempt_observation(
                &receipt.dispatch_identity,
                &authority,
                AttemptObservation::Unknown,
                14,
            )
        } else {
            store.apply_attempt_observation(
                &receipt.dispatch_identity,
                AttemptObservation::Unknown,
                14,
            )
        }
        .expect_err("retained handle must refuse observation");
        assert!(matches!(error, WorkflowStoreError::UnsupportedStore { .. }));
        let status: String = fixture_scalar_text(
            &foreign,
            &select("workflow_attempts")
                .columns(&["status"])
                .where_eq("dispatch_identity", receipt.dispatch_identity.as_str()),
            "status",
        )
        .expect("attempt unchanged");
        assert_eq!(status, "admitted");
    }
}

#[test]
fn pending_activation_admission_is_atomic_and_single_winner() {
    let (temp, mut first_store) = initialized_store();
    let mut second_store = WorkflowStore::open_in_state_dir(temp.path()).expect("second store");
    let activation = activation_id();
    let prepared = first_store
        .prepare_pending_activation(
            "run-1",
            "review",
            &activation,
            DispatchSideEffect::ReadOnly,
            serde_json::json!({"operation": "review"}),
            12,
        )
        .expect("admission")
        .expect("prepared");
    assert_eq!(prepared.activation.node_id, "review");
    assert_eq!(prepared.activation.input, Some(serde_json::json!(1)));
    assert_eq!(prepared.attempt, 1);
    assert_eq!(
        prepared.dispatch_identity,
        dispatch_identity("run-1", "review", &activation, 1)
    );
    assert!(
        second_store
            .prepare_pending_activation(
                "run-1",
                "review",
                &activation,
                DispatchSideEffect::ReadOnly,
                serde_json::json!({"operation": "review"}),
                13,
            )
            .expect("second admission")
            .is_none()
    );
    assert!(
        first_store
            .pending_activations(10)
            .expect("pending")
            .is_empty()
    );
    let attempts = first_store
        .attempt_history("run-1", None, 10)
        .expect("attempts");
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].dispatch_identity, prepared.dispatch_identity);
    let status: String = fixture_scalar_text(
        &first_store.connection,
        &select("workflow_activations")
            .columns(&["status"])
            .where_eq("run_id", "run-1")
            .where_eq("node_id", "review"),
        "status",
    )
    .expect("activation status");
    assert_eq!(status, "running");
}

#[test]
fn terminal_runs_returns_only_bounded_parent_owned_terminal_runs() {
    let (_temp, store) = initialized_store();
    assert!(store.terminal_runs(10).expect("terminal runs").is_empty());
    block_on(
        update("workflow_runs")
            .value("status", "failed")
            .value("updated_at_ms", 20)
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("terminal run");
    let terminal = store.terminal_runs(10).expect("terminal runs");
    assert_eq!(terminal.len(), 1);
    assert_eq!(terminal[0].run_id, "run-1");
    assert_eq!(terminal[0].status, RunStatus::Failed);
    assert!(store.terminal_runs(0).is_err());
}

#[test]
fn preparation_rejects_binding_changed_after_pending_selection() {
    let (_temp, mut store) = initialized_store();
    let pending = store.pending_activations(1).expect("selection").remove(0);
    block_on(
        update("workflow_activations")
            .value("node_revision", 2)
            .execute(&store.connection),
    )
    .expect("changed binding");
    let before = fixture_changes(&store.connection);
    assert!(
        store
            .prepare_pending_activation(
                &pending.run_id,
                &pending.node_id,
                &pending.activation_id,
                DispatchSideEffect::ReadOnly,
                serde_json::json!({}),
                20,
            )
            .is_err()
    );
    assert_eq!(fixture_changes(&store.connection), before);
}

#[test]
fn pending_activation_admission_rolls_back_when_intent_is_oversized() {
    let (_temp, mut store) = initialized_store();
    let error = store
        .prepare_pending_activation(
            "run-1",
            "review",
            &activation_id(),
            DispatchSideEffect::ReadOnly,
            serde_json::Value::String("x".repeat(MAX_INLINE_JSON_BYTES)),
            12,
        )
        .expect_err("oversized intent");
    assert!(error.to_string().contains("exceeds"));
    assert_eq!(store.pending_activations(10).expect("pending").len(), 1);
    assert!(
        store
            .attempt_history("run-1", None, 10)
            .expect("attempts")
            .is_empty()
    );
}

#[test]
fn prepared_recovery_rejects_malformed_json_without_writes() {
    for column in ["input_json", "intent_json"] {
        let (_temp, mut store) = initialized_store();
        store
            .prepare_pending_activation(
                "run-1",
                "review",
                &activation_id(),
                DispatchSideEffect::ReadOnly,
                serde_json::json!({"operation": "review"}),
                12,
            )
            .expect("prepare")
            .expect("activation");
        let table = if column == "input_json" {
            "workflow_activations"
        } else {
            "workflow_attempts"
        };
        block_on(
            update(table)
                .value(column, "{")
                .where_eq("run_id", "run-1")
                .execute(&store.connection),
        )
        .expect("malformed JSON fixture");
        let before = fixture_changes(&store.connection);
        assert!(
            prepared_read_only_dispatches_for_run(&store.connection, "unrelated-run", 10)
                .expect("unrelated scope must not decode damaged rows")
                .is_empty()
        );
        assert!(prepared_read_only_dispatches(&store.connection, 10).is_err());
        assert!(prepared_read_only_dispatches_for_run(&store.connection, "run-1", 10).is_err());
        assert_eq!(fixture_changes(&store.connection), before);
    }
}

#[test]
fn recovered_admission_observation_rolls_back_on_invalid_output() {
    let (_temp, mut store) = initialized_store();
    let prepared = store
        .prepare_pending_activation(
            "run-1",
            "review",
            &activation_id(),
            DispatchSideEffect::ReadOnly,
            serde_json::json!({"operation": "review"}),
            12,
        )
        .expect("prepare")
        .expect("activation");
    let authority = WorkflowExecutionAuthority {
        target_artifact_id: "artifact-a".into(),
        daemon_instance_id: "daemon-a".into(),
        generation: 1,
        fencing_token: "token-a".into(),
    };
    block_on(
        update("workflow_runs")
            .value("target_artifact_id", "artifact-a")
            .value("coordinator_daemon_instance_id", "daemon-a")
            .value("coordinator_generation", 1)
            .value("coordinator_fencing_token", "token-a")
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("authority");
    let receipt = DispatchReceipt {
        run_id: "run-1".into(),
        node_id: "review".into(),
        activation_id: activation_id(),
        attempt: prepared.attempt,
        dispatch_identity: prepared.dispatch_identity,
        receipt: serde_json::json!({"owner": "test"}),
        admitted_at_ms: 13,
    };
    let output = ValidatedOutput {
        output_id: "recovered-output".into(),
        run_id: "run-1".into(),
        node_id: "review".into(),
        activation_id: activation_id(),
        schema_id: "u32".into(),
        schema_version: 1,
        value: serde_json::json!("invalid"),
        artifact_reference: None,
        created_at_ms: 14,
    };
    assert!(
        store
            .recover_owned_dispatch_observation(
                &receipt,
                &authority,
                AttemptObservation::Succeeded {
                    output: output.clone()
                },
                14
            )
            .is_err()
    );
    let attempts = store.attempt_history("run-1", None, 10).expect("attempts");
    assert_eq!(attempts[0].status, "prepared");
    assert!(!attempts[0].has_receipt);
    let mut stale = authority.clone();
    stale.generation += 1;
    assert!(
        store
            .recover_owned_dispatch_observation(&receipt, &stale, AttemptObservation::Unknown, 14)
            .is_err()
    );
    let summary = store
        .recover_owned_dispatch_observation(
            &receipt,
            &authority,
            AttemptObservation::Succeeded {
                output: ValidatedOutput {
                    value: serde_json::json!(7),
                    ..output
                },
            },
            14,
        )
        .expect("recover atomically");
    assert_eq!(summary.succeeded, vec![receipt.dispatch_identity]);
    let attempts = store.attempt_history("run-1", None, 10).expect("attempts");
    assert_eq!(attempts[0].status, "succeeded");
    assert!(attempts[0].has_receipt);
}

#[test]
fn cancellation_admission_recovery_never_enables_redispatch() {
    let (_temp, mut store) = initialized_store();
    store
        .prepare_pending_activation(
            "run-1",
            "review",
            &activation_id(),
            DispatchSideEffect::ReadOnly,
            serde_json::json!({"operation": "review"}),
            12,
        )
        .expect("prepare")
        .expect("activation");
    for (status, activation_status) in [
        ("cancelling", "running"),
        ("sibling_cancelling", "running"),
        ("cancelling", "cancelled"),
        ("sibling_cancelling", "repair_required"),
        ("prepared", "cancelled"),
    ] {
        block_on(
            update("workflow_activations")
                .value("status", activation_status)
                .where_eq("run_id", "run-1")
                .execute(&store.connection),
        )
        .expect("activation state");
        block_on(
            update("workflow_attempts")
                .value("status", status)
                .where_eq("run_id", "run-1")
                .execute(&store.connection),
        )
        .expect("cancellation fixture");
        let before = fixture_changes(&store.connection);
        let evidence =
            prepared_dispatches_scoped(&store.connection, Some("run-1"), 10, false, None)
                .expect("evidence");
        assert_eq!(evidence.len(), 1);
        assert!(
            prepared_read_only_dispatches_for_run(&store.connection, "run-1", 10)
                .expect("redispatch")
                .is_empty()
        );
        assert_eq!(fixture_changes(&store.connection), before);
    }
}

#[test]
fn prepared_recovery_filters_identities_before_limit() {
    let (_temp, mut store) = initialized_store();
    store
        .prepare_pending_activation(
            "run-1",
            "review",
            &activation_id(),
            DispatchSideEffect::ReadOnly,
            serde_json::json!({"operation": "review"}),
            12,
        )
        .expect("prepare")
        .expect("activation");
    // Two durable attempts make the selected identity fall beyond a one-row prefix.
    let templates = fixture_rows(
        &store.connection,
        &select("workflow_attempts").where_eq("run_id", "run-1"),
    );
    for row in &templates {
        let mut mutation = insert("workflow_attempts");
        for name in [
            "run_id",
            "node_id",
            "activation_id",
            "status",
            "side_effect",
            "intent_json",
            "receipt_json",
            "admitted_at_ms",
            "terminal_at_ms",
            "intent_checksum",
        ] {
            mutation = mutation.value(name, row.get(name).expect("template column"));
        }
        block_on(
            mutation
                .value("attempt", fixture_unsigned(row, "attempt") + 1)
                .value("dispatch_identity", "later-identity")
                .value(
                    "prepared_at_ms",
                    fixture_unsigned(row, "prepared_at_ms") + 1,
                )
                .execute(&store.connection),
        )
        .expect("second attempt");
    }
    let selected = vec!["later-identity".to_owned()];
    let recovered =
        prepared_dispatches_scoped(&store.connection, Some("run-1"), 1, true, Some(&selected))
            .expect("selected recovery");
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].dispatch_identity, "later-identity");
    assert!(
        prepared_dispatches_scoped(&store.connection, Some("run-1"), 1, true, Some(&[]))
            .expect("empty selection")
            .is_empty()
    );
}

#[test]
fn prepared_recovery_preserves_absent_and_null_inputs() {
    let (_temp, mut store) = initialized_store();
    store
        .prepare_pending_activation(
            "run-1",
            "review",
            &activation_id(),
            DispatchSideEffect::ReadOnly,
            serde_json::json!({"operation": "review"}),
            12,
        )
        .expect("prepare")
        .expect("activation");
    for (input, expected) in [(None, None), (Some("null"), Some(serde_json::Value::Null))] {
        block_on(
            update("workflow_activations")
                .value("input_json", input)
                .where_eq("run_id", "run-1")
                .execute(&store.connection),
        )
        .expect("input fixture");
        let before = fixture_changes(&store.connection);
        for recovered in [
            prepared_read_only_dispatches(&store.connection, 10).expect("global recovery"),
            prepared_read_only_dispatches_for_run(&store.connection, "run-1", 10)
                .expect("run recovery"),
        ] {
            assert_eq!(recovered.len(), 1);
            assert_eq!(recovered[0].activation.input, expected);
        }
        assert_eq!(fixture_changes(&store.connection), before);
    }
}

#[test]
fn prepared_recovery_rejects_oversized_input_without_writes() {
    let (_temp, mut store) = initialized_store();
    store
        .prepare_pending_activation(
            "run-1",
            "review",
            &activation_id(),
            DispatchSideEffect::ReadOnly,
            serde_json::json!({"operation": "review"}),
            12,
        )
        .expect("prepare")
        .expect("activation");
    let inputs: Vec<Box<dyn Expression>> = vec![
        serde_json::to_string(&"x".repeat(MAX_INLINE_JSON_BYTES))
            .expect("json")
            .into(),
        cast_blob("null").into(),
    ];
    for input in inputs {
        block_on(
            store.connection.exec_update_count(
                &(update("workflow_activations")
                    .value("input_json", input)
                    .where_eq("run_id", "run-1")),
            ),
        )
        .expect("damaged input fixture");
        let before = fixture_changes(&store.connection);
        assert!(matches!(
            prepared_read_only_dispatches(&store.connection, 10),
            Err(WorkflowStoreError::InvalidData(_))
        ));
        assert!(prepared_read_only_dispatches_for_run(&store.connection, "run-1", 10).is_err());
        assert_eq!(fixture_changes(&store.connection), before);
    }
}

#[test]
fn prepared_recovery_rejects_oversized_intent_without_writes() {
    let (_temp, mut store) = initialized_store();
    store
        .prepare_pending_activation(
            "run-1",
            "review",
            &activation_id(),
            DispatchSideEffect::ReadOnly,
            serde_json::json!({"operation": "review"}),
            12,
        )
        .expect("prepare")
        .expect("activation");
    let intents: Vec<Box<dyn Expression>> = vec![
        serde_json::to_string(&"x".repeat(MAX_INLINE_JSON_BYTES))
            .expect("json")
            .into(),
        cast_blob("{}").into(),
    ];
    for intent in intents {
        block_on(
            store.connection.exec_update_count(
                &(update("workflow_attempts")
                    .value("intent_json", intent)
                    .where_eq("run_id", "run-1")),
            ),
        )
        .expect("damaged intent fixture");
        let before = fixture_changes(&store.connection);
        assert!(
            matches!(prepared_read_only_dispatches(&store.connection, 10),
            Err(WorkflowStoreError::InvalidData(message)) if message.contains("attempt intent is oversized or not text"))
        );
        assert!(prepared_read_only_dispatches_for_run(&store.connection, "run-1", 10).is_err());
        assert_eq!(fixture_changes(&store.connection), before);
    }
}

#[test]
fn prepared_recovery_requires_running_activations() {
    let (_temp, mut store) = initialized_store();
    store
        .prepare_pending_activation(
            "run-1",
            "review",
            &activation_id(),
            DispatchSideEffect::ReadOnly,
            serde_json::json!({"operation": "review"}),
            12,
        )
        .expect("prepare")
        .expect("activation");
    assert_eq!(
        prepared_read_only_dispatches(&store.connection, 10)
            .expect("recoverable")
            .len(),
        1
    );
    for status in [
        "pending",
        "waiting_input",
        "waiting_approval",
        "waiting_mutation_approval",
        "completed",
        "cancelled",
        "failed",
        "skipped",
    ] {
        block_on(
            update("workflow_activations")
                .value("status", status)
                .where_eq("run_id", "run-1")
                .execute(&store.connection),
        )
        .expect("non-running activation");
        let before = fixture_changes(&store.connection);
        assert!(
            prepared_read_only_dispatches(&store.connection, 10)
                .expect("global recovery")
                .is_empty()
        );
        assert!(
            prepared_read_only_dispatches_for_run(&store.connection, "run-1", 10)
                .expect("run recovery")
                .is_empty()
        );
        assert_eq!(fixture_changes(&store.connection), before);
    }
}

#[test]
fn indexed_attempt_lookup_is_bounded_and_rejects_corrupt_coordinates() {
    let (_temp, mut store) = initialized_store();
    let attempt = PreparedAttempt {
        run_id: "run-1".to_owned(),
        node_id: "review".to_owned(),
        activation_id: activation_id(),
        attempt: 1,
        side_effect: DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({}),
        prepared_at_ms: 12,
    };
    let identity = store.prepare_attempt(&attempt).expect("prepare");
    let before = fixture_changes(&store.connection);
    let observed = store
        .attempt_by_dispatch_identity(&identity)
        .expect("lookup")
        .expect("attempt");
    assert_eq!(observed.run_id, attempt.run_id);
    assert_eq!(observed.node_id, attempt.node_id);
    assert_eq!(observed.activation_id, attempt.activation_id);
    assert_eq!(observed.attempt, attempt.attempt);
    assert!(
        store
            .attempt_by_dispatch_identity(&"f".repeat(64))
            .expect("missing lookup")
            .is_none()
    );
    assert_eq!(fixture_changes(&store.connection), before);
    block_on(
        update("workflow_attempts")
            .value("attempt", 3)
            .where_eq("dispatch_identity", identity.as_str())
            .execute(&store.connection),
    )
    .expect("corrupt fixture");
    let before = fixture_changes(&store.connection);
    assert!(store.attempt_by_dispatch_identity(&identity).is_err());
    assert_eq!(fixture_changes(&store.connection), before);
}

#[test]
fn prepared_attempt_identity_is_stable_and_conflicting_intent_fails_closed() {
    let (_temp, mut store) = initialized_store();
    let attempt = PreparedAttempt {
        run_id: "run-1".to_string(),
        node_id: "review".to_string(),
        activation_id: activation_id(),
        attempt: 1,
        side_effect: DispatchSideEffect::Mutating,
        intent: serde_json::json!({"operation": "apply"}),
        prepared_at_ms: 12,
    };
    let first = store.prepare_attempt(&attempt).expect("prepare");
    block_on(
        update("workflow_runs")
            .value("node_execution_cap", 1)
            .value("concurrency_cap", 1)
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("exhaust capacity with existing attempt");
    let before = fixture_changes(&store.connection);
    let second = store
        .prepare_attempt(&attempt)
        .expect("idempotent at capacity");
    assert_eq!(fixture_changes(&store.connection), before);
    assert_eq!(first, second);
    assert_eq!(
        first,
        dispatch_identity("run-1", "review", activation_id().as_str(), 1)
    );
    let error = store
        .prepare_attempt(&PreparedAttempt {
            side_effect: DispatchSideEffect::ReadOnly,
            ..attempt.clone()
        })
        .expect_err("conflicting side-effect classification");
    assert!(error.to_string().contains("identity conflict"));
    assert_eq!(fixture_changes(&store.connection), before);
    let error = store
        .prepare_attempt(&PreparedAttempt {
            intent: serde_json::json!({"operation": "different"}),
            ..attempt
        })
        .expect_err("conflicting intent");
    assert!(error.to_string().contains("identity conflict"));
}

#[test]
fn dispatch_receipt_requires_exact_prepared_identity() {
    let (_temp, mut store) = initialized_store();
    let attempt = PreparedAttempt {
        run_id: "run-1".to_string(),
        node_id: "review".to_string(),
        activation_id: activation_id(),
        attempt: 1,
        side_effect: DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({"operation": "review"}),
        prepared_at_ms: 12,
    };
    let identity = store.prepare_attempt(&attempt).expect("prepare");
    store
        .persist_dispatch_receipt(&DispatchReceipt {
            run_id: attempt.run_id,
            node_id: attempt.node_id,
            activation_id: attempt.activation_id,
            attempt: 1,
            dispatch_identity: identity,
            receipt: serde_json::json!({"turn_id": "turn-1"}),
            admitted_at_ms: 13,
        })
        .expect("receipt");
    let status: String = fixture_scalar_text(
        &store.connection,
        &select("workflow_attempts")
            .columns(&["status"])
            .where_eq("run_id", "run-1"),
        "status",
    )
    .expect("status");
    assert_eq!(status, "admitted");
}

#[test]
fn restart_reconciliation_never_retries_ambiguous_mutation() {
    let (_temp, mut store) = initialized_store();
    let mutating = PreparedAttempt {
        run_id: "run-1".to_string(),
        node_id: "review".to_string(),
        activation_id: activation_id(),
        attempt: 1,
        side_effect: DispatchSideEffect::Mutating,
        intent: serde_json::json!({"operation": "apply"}),
        prepared_at_ms: 12,
    };
    let identity = store.prepare_attempt(&mutating).expect("prepare");

    let summary = store
        .reconcile_prepared_attempts(10, 20)
        .expect("reconcile");

    assert_eq!(summary.repair_required, [identity]);
    assert!(summary.safe_prepared.is_empty());
    let run_status: String = fixture_scalar_text(
        &store.connection,
        &select("workflow_runs")
            .columns(&["status"])
            .where_eq("run_id", "run-1"),
        "status",
    )
    .expect("run status");
    assert_eq!(run_status, "repair_required");
}

#[test]
fn normal_status_and_history_reads_are_bounded_and_projection_backed() {
    let (_temp, mut store) = initialized_store();
    let attempt = PreparedAttempt {
        run_id: "run-1".to_string(),
        node_id: "review".to_string(),
        activation_id: activation_id(),
        attempt: 1,
        side_effect: DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({"operation": "review"}),
        prepared_at_ms: 12,
    };
    let identity = store.prepare_attempt(&attempt).expect("prepare");

    let summary = store.run_summary("run-1").expect("summary").expect("run");
    assert_eq!(summary.status, RunStatus::Running);
    assert_eq!(store.list_runs(10).expect("runs"), [summary]);
    let attempts = store.attempt_history("run-1", None, 10).expect("attempts");
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].dispatch_identity, identity);
    assert!(!attempts[0].has_receipt);
    let events = store.event_history("run-1", None, 10).expect("events");
    assert_eq!(
        events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        ["run_created", "activation_created", "attempt_prepared"]
    );
    let page = store
        .event_history("run-1", Some(events[0].event_seq), 1)
        .expect("paged events");
    assert_eq!(page[0].event_type, "activation_created");
    assert_eq!(
        store
            .latest_global_event_sequence()
            .expect("latest sequence"),
        events.last().expect("last event").event_seq
    );
    let positions = store
        .event_positions_after(events[0].event_seq, 2)
        .expect("event positions");
    assert_eq!(
        positions
            .iter()
            .map(|(sequence, run_id, _)| (*sequence, run_id.as_str()))
            .collect::<Vec<_>>(),
        vec![
            (events[1].event_seq, "run-1"),
            (events[2].event_seq, "run-1")
        ]
    );
    assert!(store.event_positions_after(0, 0).is_err());
    assert!(store.list_runs(0).is_err());
    assert!(store.event_history("run-1", None, 1_001).is_err());
}

#[test]
fn durable_repeat_honors_requested_limits_above_old_defaults_after_reopen() {
    for iterations in [200_u32, 1_001] {
        let temp = tempfile::tempdir().expect("temp");
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        let mut definition = repeat_definition();
        definition
            .nodes
            .get_mut("repeat-control")
            .unwrap()
            .configuration["max_iterations"] = serde_json::json!(iterations);
        store
            .persist_definition("example", 1, &definition)
            .expect("definition");
        let mut run = new_run();
        run.input = Some(serde_json::json!({"condition_met": false, "iteration": 1}));
        run.limits.cycle_cap = iterations;
        // Exercise a budget beyond u32 through persistence, reopen and idempotency.
        run.limits.node_execution_cap = u64::from(u32::MAX) * 8;
        store.create_run_idempotent(&run).expect("run");
        // Repeat workflows must remain executable after upgrading the pre-graph store.
        for table in [
            "workflow_run_graph_edges",
            "workflow_run_graph_nodes",
            "workflow_run_graphs",
        ] {
            block_on(switchy_database::schema::drop_table(table).execute(&store.connection))
                .expect("historical repeat table");
        }
        fixture_historical_activations(&store.connection, true).expect("historical repeat column");
        block_on(
            update("workflow_store_contract")
                .value("schema_version", 15)
                .execute(&store.connection),
        )
        .expect("historical repeat fixture");
        drop(store);
        let mut store =
            WorkflowStore::initialize_in_state_dir(temp.path(), 907).expect("upgrade and reopen");
        assert!(!store.create_run_idempotent(&run).expect("idempotent"));
        assert_eq!(
            store.run_limits(&run.run_id).expect("limits").unwrap(),
            run.limits
        );
        for iteration in 1..=iterations {
            let body = store
                .pending_activations(10)
                .expect("body")
                .pop()
                .expect("activation");
            assert_eq!(body.dependency_generation, u64::from(iteration - 1));
            store
                .persist_validated_output(&ValidatedOutput {
                    output_id: format!("output-{iteration}"),
                    run_id: run.run_id.clone(),
                    node_id: "body".to_string(),
                    activation_id: body.activation_id,
                    schema_id: definition.nodes["body"].output.type_name.clone(),
                    schema_version: 1,
                    value: serde_json::json!({"condition_met": false, "iteration": iteration}),
                    artifact_reference: None,
                    created_at_ms: 20 + u64::from(iteration) * 2,
                })
                .expect("output");
            let settled = store
                .settle_pending_control_nodes(&run.run_id, 10, 21 + u64::from(iteration) * 2)
                .expect("settle");
            assert_eq!(settled.activated.len(), usize::from(iteration < iterations));
        }
        assert_eq!(
            store
                .run_summary(&run.run_id)
                .expect("summary")
                .unwrap()
                .status,
            RunStatus::Failed
        );
    }
}

#[test]
fn durable_repeat_control_settlement_advances_generation_then_fails_at_bound() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    let definition = repeat_definition();
    store
        .persist_definition("repeat", 1, &definition)
        .expect("definition");
    store
        .create_run(&NewWorkflowRun {
            run_id: "repeat-run".to_string(),
            definition_id: "repeat".to_string(),
            definition_version: 1,
            workspace_snapshot: "snapshot".to_string(),
            parent_session_id: None,
            parent_session_generation: None,
            binding: None,
            authored_provenance: None,
            input: Some(serde_json::json!({"condition_met": false, "iteration": 1})),
            execution_authority: None,
            created_at_ms: 1,
            authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                version: 1,
                provider_id: "test-policy".to_string(),
                profile_id: "build".to_string(),
                policy_digest_sha256: "a".repeat(64),
            },
            authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
            limits: WorkflowRunLimits::default(),
        })
        .expect("run");
    let body = store
        .pending_activations(10)
        .expect("body")
        .pop()
        .expect("body");
    store
        .persist_validated_output(&ValidatedOutput {
            output_id: "body-0-output".to_string(),
            run_id: "repeat-run".to_string(),
            node_id: "body".to_string(),
            activation_id: body.activation_id,
            schema_id: definition.nodes["body"].output.type_name.clone(),
            schema_version: 1,
            value: serde_json::json!({"condition_met": false, "iteration": 1}),
            artifact_reference: None,
            created_at_ms: 2,
        })
        .expect("body output");
    let settled = store
        .settle_pending_control_nodes("repeat-run", 10, 3)
        .expect("repeat");
    assert_eq!(settled.activated.len(), 1);
    assert_eq!(settled.activated[0].dependency_generation, 1);
    assert_eq!(settled.activated[0].input.as_ref().unwrap()["iteration"], 2);
    let body = store
        .pending_activations(10)
        .expect("body")
        .pop()
        .expect("body");
    store
        .persist_validated_output(&ValidatedOutput {
            output_id: "body-1-output".to_string(),
            run_id: "repeat-run".to_string(),
            node_id: "body".to_string(),
            activation_id: body.activation_id,
            schema_id: definition.nodes["body"].output.type_name.clone(),
            schema_version: 1,
            value: serde_json::json!({"condition_met": false, "iteration": 2}),
            artifact_reference: None,
            created_at_ms: 4,
        })
        .expect("body output");
    let settled = store
        .settle_pending_control_nodes("repeat-run", 10, 5)
        .expect("complete");
    assert!(settled.activated.is_empty());
    assert_eq!(
        store
            .run_summary("repeat-run")
            .expect("run")
            .expect("run")
            .status,
        RunStatus::Failed
    );
    assert!(
        store
            .event_history("repeat-run", None, 20)
            .expect("events")
            .iter()
            .any(|event| event.event_type == "run_failed"
                && event.payload["reason"] == "repeat_iteration_limit_exhausted")
    );
}

#[test]
fn repeat_restart_cannot_duplicate_or_skip_generation() {
    let temp = tempfile::tempdir().expect("temp");
    let definition = repeat_definition();
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("repeat-restart", 1, &definition)
        .expect("definition");
    store
        .create_run(&NewWorkflowRun {
            run_id: "repeat-restart-run".to_string(),
            definition_id: "repeat-restart".to_string(),
            definition_version: 1,
            workspace_snapshot: "snapshot".to_string(),
            parent_session_id: None,
            parent_session_generation: None,
            binding: None,
            authored_provenance: None,
            input: Some(serde_json::json!({"condition_met": false, "iteration": 1})),
            execution_authority: None,
            created_at_ms: 1,
            authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                version: 1,
                provider_id: "test-policy".to_string(),
                profile_id: "build".to_string(),
                policy_digest_sha256: "a".repeat(64),
            },
            authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
            limits: WorkflowRunLimits::default(),
        })
        .expect("run");
    let body = store
        .pending_activations(10)
        .expect("body")
        .pop()
        .expect("body");
    store
        .persist_validated_output(&ValidatedOutput {
            output_id: "restart-body-0".to_string(),
            run_id: "repeat-restart-run".to_string(),
            node_id: "body".to_string(),
            activation_id: body.activation_id,
            schema_id: definition.nodes["body"].output.type_name.clone(),
            schema_version: 1,
            value: serde_json::json!({"condition_met": false, "iteration": 1}),
            artifact_reference: None,
            created_at_ms: 2,
        })
        .expect("output");
    drop(store);

    let mut reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    let first = reopened
        .settle_pending_control_nodes("repeat-restart-run", 10, 3)
        .expect("settle after restart");
    assert_eq!(first.activated.len(), 1);
    assert_eq!(first.activated[0].dependency_generation, 1);
    let second = reopened
        .settle_pending_control_nodes("repeat-restart-run", 10, 4)
        .expect("idempotent settle");
    assert!(second.activated.is_empty());
    drop(reopened);

    let reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("second reopen");
    let rows: Vec<_> = fixture_rows(
        &reopened.connection,
        &select("workflow_activations")
            .columns(&["dependency_generation", "status"])
            .where_eq("run_id", "repeat-restart-run")
            .where_eq("node_id", "body")
            .sort(
                "dependency_generation",
                switchy_database::query::SortDirection::Asc,
            ),
    )
    .iter()
    .map(|row| {
        (
            fixture_unsigned(row, "dependency_generation"),
            fixture_text(row, "status"),
        )
    })
    .collect();
    assert_eq!(
        rows,
        vec![(0, "completed".to_string()), (1, "pending".to_string())]
    );
    let repeat_rows = fixture_scalar_unsigned(
        &reopened.connection,
        &select("workflow_activations")
            .project_as(switchy_database::query::count_all(), "count")
            .where_eq("run_id", "repeat-restart-run")
            .where_eq("node_id", "repeat-control"),
        "count",
    )
    .expect("repeat count");
    assert_eq!(repeat_rows, 1);
}

#[test]
fn repeat_emit_outcome_completes_at_bound_and_is_boundedly_inspectable() {
    let temp = tempfile::tempdir().expect("temp");
    let definition = repeat_definition_with_outcome();
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("repeat-outcome", 1, &definition)
        .expect("definition");
    store
        .create_run(&NewWorkflowRun {
            run_id: "repeat-outcome-run".to_string(),
            definition_id: "repeat-outcome".to_string(),
            definition_version: 1,
            workspace_snapshot: "snapshot".to_string(),
            parent_session_id: None,
            parent_session_generation: None,
            binding: None,
            authored_provenance: None,
            input: Some(serde_json::json!({"condition_met": false, "iteration": 1})),
            execution_authority: None,
            created_at_ms: 1,
            authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                version: 1,
                provider_id: "test-policy".to_string(),
                profile_id: "build".to_string(),
                policy_digest_sha256: "a".repeat(64),
            },
            authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
            limits: WorkflowRunLimits {
                cycle_cap: 1,
                ..WorkflowRunLimits::default()
            },
        })
        .expect("run");
    let body = store
        .pending_activations(10)
        .expect("body")
        .pop()
        .expect("body");
    store
        .persist_validated_output(&ValidatedOutput {
            output_id: "outcome-body".to_string(),
            run_id: "repeat-outcome-run".to_string(),
            node_id: "body".to_string(),
            activation_id: body.activation_id,
            schema_id: definition.nodes["body"].output.type_name.clone(),
            schema_version: 1,
            value: serde_json::json!({"condition_met": false, "iteration": 1}),
            artifact_reference: None,
            created_at_ms: 2,
        })
        .expect("body output");
    store
        .settle_pending_control_nodes("repeat-outcome-run", 10, 3)
        .expect("settle");
    assert_eq!(
        store
            .run_summary("repeat-outcome-run")
            .expect("run")
            .expect("run")
            .status,
        RunStatus::Completed
    );
    let outcomes = store
        .repeat_outcomes("repeat-outcome-run", 10)
        .expect("outcomes");
    assert_eq!(outcomes.len(), 1);
    assert_eq!(
        outcomes[0].outcome,
        bcode_workflow::WorkflowRepeatOutcomeKind::IterationLimitReached
    );
    assert_eq!(outcomes[0].iterations_completed, 1);
    assert_eq!(outcomes[0].max_iterations, 2);
    assert_eq!(outcomes[0].cycle_cap, 1);
    assert_eq!(outcomes[0].effective_iteration_bound, 1);
    let settled = store
        .event_history("repeat-outcome-run", None, 20)
        .expect("events")
        .into_iter()
        .find(|event| event.event_type == "control_node_settled")
        .expect("settled");
    assert_eq!(settled.payload["iterations_completed"], 1);
    assert_eq!(settled.payload["outcome"], "iteration_limit_reached");
    verify_repeat_outcome_graph_authority(store, temp.path(), &definition, &outcomes);
}

fn verify_repeat_outcome_graph_authority(
    store: WorkflowStore,
    state_dir: &Path,
    definition: &WorkflowDefinition,
    outcomes: &[WorkflowRepeatOutcomeSummary],
) {
    let mut source = definition.clone();
    source.nodes.clear();
    let json = serde_json::to_string(&source).expect("source");
    block_on(
        update("workflow_definitions")
            .value("definition_json", json.as_str())
            .value("checksum_sha256", sha256_hex(json.as_bytes()))
            .where_eq("definition_id", "repeat-outcome")
            .execute(&store.connection),
    )
    .expect("source fixture");
    drop(store);
    let store = WorkflowStore::open_in_state_dir(state_dir).expect("reopen");
    assert_eq!(
        store
            .repeat_outcomes("repeat-outcome-run", 10)
            .expect("admitted outcomes"),
        outcomes
    );
    for damage in 0..3 {
        match damage {
            0 => {
                block_on(
                    update("workflow_run_graph_nodes")
                        .value("node_json", "{}")
                        .where_eq("node_id", "repeat-control")
                        .execute(&store.connection),
                )
                .expect("damage fixture");
            }
            1 => {
                block_on(
                    update("workflow_run_graphs")
                        .value("revision", 2)
                        .where_eq("run_id", "repeat-outcome-run")
                        .execute(&store.connection),
                )
                .expect("damage fixture");
            }
            _ => {
                block_on(
                    delete("workflow_run_graph_nodes")
                        .where_eq("node_id", "repeat-control")
                        .execute(&store.connection),
                )
                .expect("damage fixture");
            }
        }
        let before = fixture_changes(&store.connection);
        assert!(store.repeat_outcomes("repeat-outcome-run", 10).is_err());
        assert_eq!(fixture_changes(&store.connection), before);
        let tx = database::begin(&store.connection, TransactionMode::Deferred)
            .expect("restore fixture transaction");
        block_on(
            delete("workflow_run_graph_nodes")
                .where_eq("run_id", "repeat-outcome-run")
                .where_eq("node_id", "repeat-control")
                .where_eq("revision", 1)
                .execute(tx.as_ref()),
        )
        .expect("replace damaged node");
        block_on(
            insert("workflow_run_graph_nodes")
                .value("run_id", "repeat-outcome-run")
                .value("node_id", "repeat-control")
                .value("revision", 1)
                .value(
                    "node_json",
                    serde_json::to_string(&definition.nodes["repeat-control"]).expect("node"),
                )
                .value("is_entry", 0)
                .value("is_exit", 1)
                .execute(tx.as_ref()),
        )
        .expect("restore node fixture");
        database::commit(tx).expect("restore fixture commit");
        block_on(async {
            update("workflow_run_graphs")
                .value("revision", 1)
                .where_eq("run_id", "repeat-outcome-run")
                .execute(&store.connection)
                .await?;
            Ok::<(), switchy_database::DatabaseError>(())
        })
        .expect("restore revision");
    }
}

#[test]
fn repeat_predicate_clearing_at_bound_completes_and_records_accounting() {
    let temp = tempfile::tempdir().expect("temp");
    let definition = repeat_definition();
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("repeat-clear", 1, &definition)
        .expect("definition");
    store
        .create_run(&NewWorkflowRun {
            run_id: "repeat-clear-run".to_string(),
            definition_id: "repeat-clear".to_string(),
            definition_version: 1,
            workspace_snapshot: "snapshot".to_string(),
            parent_session_id: None,
            parent_session_generation: None,
            binding: None,
            authored_provenance: None,
            input: Some(serde_json::json!({"condition_met": false, "iteration": 1})),
            execution_authority: None,
            created_at_ms: 1,
            authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                version: 1,
                provider_id: "test-policy".to_string(),
                profile_id: "build".to_string(),
                policy_digest_sha256: "a".repeat(64),
            },
            authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
            limits: WorkflowRunLimits {
                cycle_cap: 2,
                ..WorkflowRunLimits::default()
            },
        })
        .expect("run");
    let body = store
        .pending_activations(10)
        .expect("body")
        .pop()
        .expect("body");
    store
        .persist_validated_output(&ValidatedOutput {
            output_id: "clear-body-0".to_string(),
            run_id: "repeat-clear-run".to_string(),
            node_id: "body".to_string(),
            activation_id: body.activation_id,
            schema_id: definition.nodes["body"].output.type_name.clone(),
            schema_version: 1,
            value: serde_json::json!({"condition_met": false, "iteration": 1}),
            artifact_reference: None,
            created_at_ms: 2,
        })
        .expect("output");
    store
        .settle_pending_control_nodes("repeat-clear-run", 10, 3)
        .expect("advance");
    let body = store
        .pending_activations(10)
        .expect("body")
        .pop()
        .expect("body");
    store
        .persist_validated_output(&ValidatedOutput {
            output_id: "clear-body-1".to_string(),
            run_id: "repeat-clear-run".to_string(),
            node_id: "body".to_string(),
            activation_id: body.activation_id,
            schema_id: definition.nodes["body"].output.type_name.clone(),
            schema_version: 1,
            value: serde_json::json!({"condition_met": true, "iteration": 2}),
            artifact_reference: None,
            created_at_ms: 4,
        })
        .expect("output");
    store
        .settle_pending_control_nodes("repeat-clear-run", 10, 5)
        .expect("complete");
    assert_eq!(
        store
            .run_summary("repeat-clear-run")
            .expect("run")
            .expect("run")
            .status,
        RunStatus::Completed
    );
    let settled = store
        .event_history("repeat-clear-run", None, 30)
        .expect("events")
        .into_iter()
        .filter(|event| event.event_type == "control_node_settled")
        .collect::<Vec<_>>();
    assert_eq!(settled.len(), 2);
    assert_eq!(settled[0].payload["generation"], 0);
    assert_eq!(settled[0].payload["next_generation"], 1);
    assert_eq!(settled[1].payload["generation"], 1);
    assert_eq!(settled[1].payload["repeat"], false);
    assert_eq!(settled[1].payload["effective_iteration_bound"], 2);
}

fn assert_invalid_repeat_caps_roll_back(store: &mut WorkflowStore) {
    for invalid_cap in [-1_i64, 0, i64::from(u32::MAX) + 1] {
        block_on(
            update("workflow_runs")
                .value("cycle_cap", invalid_cap)
                .where_eq("run_id", "repeat-cap-run")
                .execute(&store.connection),
        )
        .expect("invalid cap fixture");
        let events_before = store
            .event_history("repeat-cap-run", None, 20)
            .expect("events");
        assert!(
            store
                .settle_pending_control_nodes("repeat-cap-run", 10, 3)
                .is_err()
        );
        assert!(
            block_on(store.connection.sqlite_is_autocommit()).expect("owned snapshot released")
        );
        assert_eq!(
            store
                .event_history("repeat-cap-run", None, 20)
                .expect("events"),
            events_before
        );
        assert_eq!(
            store
                .run_summary("repeat-cap-run")
                .expect("run")
                .expect("run")
                .status,
            RunStatus::Running
        );
    }
}

#[test]
fn repeat_cycle_cap_is_enforced_with_definition_bound() {
    let temp = tempfile::tempdir().expect("temp");
    let definition = repeat_definition();
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("repeat-cap", 1, &definition)
        .expect("definition");
    store
        .create_run(&NewWorkflowRun {
            run_id: "repeat-cap-run".to_string(),
            definition_id: "repeat-cap".to_string(),
            definition_version: 1,
            workspace_snapshot: "snapshot".to_string(),
            parent_session_id: None,
            parent_session_generation: None,
            binding: None,
            authored_provenance: None,
            input: Some(serde_json::json!({"condition_met": false, "iteration": 1})),
            execution_authority: None,
            created_at_ms: 1,
            authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                version: 1,
                provider_id: "test-policy".to_string(),
                profile_id: "build".to_string(),
                policy_digest_sha256: "a".repeat(64),
            },
            authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
            limits: WorkflowRunLimits {
                cycle_cap: 1,
                ..WorkflowRunLimits::default()
            },
        })
        .expect("run");
    let body = store
        .pending_activations(10)
        .expect("body")
        .pop()
        .expect("body");
    store
        .persist_validated_output(&ValidatedOutput {
            output_id: "cap-body".to_string(),
            run_id: "repeat-cap-run".to_string(),
            node_id: "body".to_string(),
            activation_id: body.activation_id,
            schema_id: definition.nodes["body"].output.type_name.clone(),
            schema_version: 1,
            value: serde_json::json!({"condition_met": false, "iteration": 1}),
            artifact_reference: None,
            created_at_ms: 2,
        })
        .expect("output");
    assert_invalid_repeat_caps_roll_back(&mut store);
    block_on(
        update("workflow_runs")
            .value("cycle_cap", 1)
            .where_eq("run_id", "repeat-cap-run")
            .execute(&store.connection),
    )
    .expect("restore cap");
    store
        .settle_pending_control_nodes("repeat-cap-run", 10, 3)
        .expect("settle");
    assert_eq!(
        store
            .run_summary("repeat-cap-run")
            .expect("run")
            .expect("run")
            .status,
        RunStatus::Failed
    );
    let failure = store
        .event_history("repeat-cap-run", None, 20)
        .expect("events")
        .into_iter()
        .find(|event| event.event_type == "run_failed")
        .expect("failure");
    assert_eq!(failure.payload["max_iterations"], 2);
    assert_eq!(failure.payload["cycle_cap"], 1);
    assert_eq!(failure.payload["effective_iteration_bound"], 1);
}

#[test]
fn explicit_failed_node_retry_is_bounded_exact_and_reuses_activation() {
    struct FailedObserver;
    impl AttemptStatusObserver for FailedObserver {
        fn observe(
            &self,
            _request: &AttemptReconciliationRequest,
        ) -> Result<AttemptObservation, WorkflowStoreError> {
            Ok(AttemptObservation::Failed {
                message: "review failed".to_string(),
            })
        }
    }

    let (_temp, mut store) = initialized_store();
    let prepared = store
        .prepare_pending_activation(
            "run-1",
            "review",
            &activation_id(),
            DispatchSideEffect::ReadOnly,
            serde_json::json!({"operation": "review"}),
            12,
        )
        .expect("prepare")
        .expect("prepared");
    store
        .persist_dispatch_receipt(&DispatchReceipt {
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: activation_id(),
            attempt: prepared.attempt,
            dispatch_identity: prepared.dispatch_identity,
            receipt: serde_json::json!({"turn_id": "turn-1"}),
            admitted_at_ms: 13,
        })
        .expect("receipt");
    store
        .reconcile_receipt_backed_attempts(&FailedObserver, 10, 20)
        .expect("failed reconciliation");
    assert_eq!(
        store
            .run_summary("run-1")
            .expect("run")
            .expect("run")
            .status,
        RunStatus::Failed
    );
    let result = store
        .retry_failed_node("run-1", "review", &activation_id(), 1, 21)
        .expect("retry");
    assert_eq!(result.next_attempt, 2);
    assert_eq!(
        store
            .run_summary("run-1")
            .expect("run")
            .expect("run")
            .status,
        RunStatus::Running
    );
    let prepared = store
        .prepare_pending_activation(
            "run-1",
            "review",
            &activation_id(),
            DispatchSideEffect::ReadOnly,
            serde_json::json!({"operation": "review"}),
            22,
        )
        .expect("prepare")
        .expect("prepared");
    assert_eq!(prepared.attempt, 2);
    assert!(
        store
            .retry_failed_node("run-1", "review", &activation_id(), 1, 23)
            .is_err()
    );
}

fn assert_retry_with_historical_target(
    store: &WorkflowStore,
    run: &NewWorkflowRun,
    activation: &str,
) {
    // A repeated goal may have a settled evaluator from a prior generation.
    // Neither a current/future target nor an unresolved older target is safe.
    block_on(
        update("workflow_activations")
            .value("dependency_generation", 1)
            .where_eq("run_id", run.run_id.as_str())
            .where_eq("node_id", "first")
            .execute(&store.connection),
    )
    .unwrap();
    for (generation, status) in [
        (0, "pending"),
        (0, "running"),
        (0, "waiting_approval"),
        (0, "repair_required"),
        (1, "completed"),
        (2, "completed"),
    ] {
        block_on(
            update("workflow_activations")
                .value("dependency_generation", generation)
                .value("status", status)
                .where_eq("run_id", run.run_id.as_str())
                .where_eq("node_id", "second")
                .execute(&store.connection),
        )
        .unwrap();
        assert!(
            store
                .retry_failed_node(&run.run_id, "first", activation, 1, 22)
                .is_err(),
            "unsafe downstream generation {generation}, status {status}"
        );
        assert_eq!(
            store.run_summary(&run.run_id).unwrap().unwrap().status,
            RunStatus::Failed
        );
    }
    block_on(
        update("workflow_activations")
            .value("dependency_generation", 0)
            .value("status", "completed")
            .where_eq("run_id", run.run_id.as_str())
            .where_eq("node_id", "second")
            .execute(&store.connection),
    )
    .unwrap();
    let retried = store
        .retry_failed_node(&run.run_id, "first", activation, 1, 23)
        .unwrap();
    assert_eq!(retried.next_attempt, 2);
    assert_eq!(retried.activation_id, activation);
    let downstream_status: String = fixture_scalar_text(
        &store.connection,
        &select("workflow_activations")
            .columns(&["status"])
            .where_eq("run_id", run.run_id.as_str())
            .where_eq("node_id", "second"),
        "status",
    )
    .unwrap();
    assert_eq!(downstream_status, "completed");
}

fn assert_context_pagination(
    store: &WorkflowStore,
    link: &WorkflowExecutionSessionLink,
    authority: &WorkflowExecutionAuthority,
    request: &bcode_workflow::WorkflowExecutionContextRequest,
) {
    for expected_revision in [None, Some(999)] {
        let continuation = bcode_workflow::WorkflowExecutionContextRequest {
            expected_revision,
            after_node_id: Some("review".to_owned()),
            ..request.clone()
        };
        assert!(
            store
                .execution_context_graph_page(link, authority, &continuation)
                .is_err()
        );
    }
}

fn assert_source_local_context(
    store: &WorkflowStore,
    link: &WorkflowExecutionSessionLink,
    authority: &WorkflowExecutionAuthority,
    request: &bcode_workflow::WorkflowExecutionContextRequest,
    page: &run_graph::RunGraphPage,
) {
    let before = fixture_changes(&store.connection);
    let source_request = bcode_workflow::WorkflowExecutionContextRequest {
        source_local: true,
        expected_revision: Some(page.revision),
        ..request.clone()
    };
    let (source_page, _, _) = store
        .execution_context_graph_page(link, authority, &source_request)
        .expect("source context");
    assert_eq!(source_page.nodes.len(), 1);
    assert_eq!(source_page.nodes[0].node.id, link.node_id);
    assert!(source_page.nodes_complete);
    assert_eq!(source_page.edges.len(), 1);
    assert!(source_page.edges_complete);
    assert!(
        source_page
            .edges
            .iter()
            .all(|edge| edge.edge.from == link.node_id)
    );
    assert!(!page.edges_complete);
    assert_ne!(page.nodes[0].node.id, link.node_id);
    assert_eq!(source_page.next_edge_id, Some(120));
    assert_eq!(source_page.next_edge_id, page.next_edge_id);
    let continuation = bcode_workflow::WorkflowExecutionContextRequest {
        after_edge_id: Some(source_page.edges[0].edge_id),
        ..source_request.clone()
    };
    let (last, _, _) = store
        .execution_context_graph_page(link, authority, &continuation)
        .expect("source continuation");
    assert!(last.edges.is_empty());
    assert!(last.edges_complete);
    let stale = bcode_workflow::WorkflowExecutionContextRequest {
        expected_revision: Some(999),
        ..source_request.clone()
    };
    assert!(
        store
            .execution_context_graph_page(link, authority, &stale)
            .is_err()
    );
    assert_context_pagination(store, link, authority, &source_request);
    assert_eq!(fixture_changes(&store.connection), before);
}

fn large_context_store(path: &Path) -> WorkflowStore {
    let mut store = WorkflowStore::open_in_state_dir(path).expect("store");
    let mut steps = Step::task("review", |value: u32, _| async move { Ok(value + 1) });
    for index in 0..120 {
        steps = steps.then(Step::task(
            format!("earlier-{index:03}"),
            |value: u32, _| async move { Ok(value + 1) },
        ));
    }
    let large = WorkflowBuilder::new("example", steps)
        .build()
        .expect("large graph");
    store
        .persist_definition("example", 1, large.definition())
        .expect("definition");
    store.create_run(&new_run()).expect("run");
    store
}

#[test]
fn run_edit_staging_checks_exact_active_execution() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = large_context_store(temp.path());
    block_on(async {
        update("workflow_runs")
            .value("target_artifact_id", "artifact-a")
            .value("coordinator_daemon_instance_id", "daemon-a")
            .value("coordinator_generation", 1)
            .value("coordinator_fencing_token", "token-a")
            .where_eq("run_id", "run-1")
            .execute(&store.connection)
            .await?;
        Ok::<(), switchy_database::DatabaseError>(())
    })
    .expect("owner fixture");
    let prepared = store
        .prepare_pending_activation(
            "run-1",
            "review",
            &activation_id(),
            DispatchSideEffect::ReadOnly,
            serde_json::json!({"operation": "review"}),
            12,
        )
        .expect("prepare")
        .expect("prepared");
    let link = WorkflowExecutionSessionLink::new(
        "run-1".to_string(),
        "review".to_string(),
        activation_id(),
        prepared.attempt,
        "session-1".to_string(),
        "snapshot-1".to_string(),
        13,
    );
    store.link_execution_session(&link).expect("link");
    let authority = store
        .execution_authority("run-1")
        .expect("authority")
        .expect("owned");
    let request = bcode_workflow::WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: "run-1".to_string(),
        expected_revision: 1,
        mutation_id: "agent-edit".to_string(),
        edits: vec![bcode_workflow::WorkflowRunGraphEdit::RemoveEdge { edge_id: 0 }],
        reconciliation: vec![],
    };
    let mut wrong = link.clone();
    wrong.session_id = "other-session".to_string();
    let context_request = bcode_workflow::WorkflowExecutionContextRequest {
        source_local: false,
        output_id: None,
        after_output_id: None,
        expected_revision: None,
        after_node_id: None,
        after_edge_id: None,
        limit: 1,
    };
    let before_read = fixture_changes(&store.connection);
    let (page, output, _) = store
        .execution_context_graph_page(&link, &authority, &context_request)
        .expect("active execution context");
    assert!(output.is_none());
    assert_eq!(page.revision, 1);
    assert!(page.nodes.len() <= 1);
    assert_eq!(fixture_changes(&store.connection), before_read);
    assert!(
        store
            .execution_context_graph_page(&wrong, &authority, &context_request)
            .is_err()
    );
    assert_source_local_context(&store, &link, &authority, &context_request, &page);
    assert_context_pagination(&store, &link, &authority, &context_request);
    assert!(
        store
            .stage_run_graph_edit_from_execution(&request, &authority, &wrong, 14)
            .is_err()
    );
    assert!(
        store
            .stage_run_graph_edit_from_execution(&request, &authority, &link, 14)
            .expect("stage")
    );
    fixture_cancel_staging_activation(&store.connection);
    assert!(
        store
            .execution_context_graph_page(&link, &authority, &context_request)
            .is_err()
    );
    let before = fixture_changes(&store.connection);
    assert!(
        store
            .stage_run_graph_edit_from_execution(&request, &authority, &link, 15)
            .is_err()
    );
    assert_eq!(fixture_changes(&store.connection), before);
}

fn fixture_cancel_staging_activation(db: &dyn Database) {
    block_on(
        update("workflow_activations")
            .value("status", "cancelled")
            .where_eq("run_id", "run-1")
            .execute(db),
    )
    .expect("settle fixture");
}

#[test]
fn damaged_execution_session_link_fails_closed_on_read() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = initialized_store_at(temp.path());
    let prepared = store
        .prepare_pending_activation(
            "run-1",
            "review",
            &activation_id(),
            DispatchSideEffect::ReadOnly,
            serde_json::json!({"operation": "review"}),
            12,
        )
        .expect("prepare")
        .expect("prepared");
    let link = WorkflowExecutionSessionLink::new(
        "run-1".to_string(),
        "review".to_string(),
        activation_id(),
        prepared.attempt,
        "session-1".to_string(),
        "snapshot-1".to_string(),
        13,
    );
    store.link_execution_session(&link).expect("link");
    block_on(
        update("workflow_execution_sessions")
            .value("workspace_snapshot", "")
            .where_eq("session_id", "session-1")
            .execute(&store.connection),
    )
    .expect("damage link");
    assert!(
        store
            .execution_session_link("run-1", "review", &activation_id(), 1)
            .is_err()
    );
}

#[test]
fn execution_session_link_survives_retry_and_cancellation_terminalization() {
    struct FailedObserver;
    impl AttemptStatusObserver for FailedObserver {
        fn observe(
            &self,
            _request: &AttemptReconciliationRequest,
        ) -> Result<AttemptObservation, WorkflowStoreError> {
            Ok(AttemptObservation::Failed {
                message: "retryable".to_string(),
            })
        }
    }

    let temp = tempfile::tempdir().expect("temp");
    let mut store = initialized_store_at(temp.path());
    let prepared = store
        .prepare_pending_activation(
            "run-1",
            "review",
            &activation_id(),
            DispatchSideEffect::ReadOnly,
            serde_json::json!({"operation": "review"}),
            12,
        )
        .expect("prepare")
        .expect("prepared");
    let link = WorkflowExecutionSessionLink::new(
        "run-1".to_string(),
        "review".to_string(),
        activation_id(),
        prepared.attempt,
        "session-1".to_string(),
        "snapshot-1".to_string(),
        13,
    );
    store.link_execution_session(&link).expect("link");
    store
        .persist_dispatch_receipt(&DispatchReceipt {
            run_id: link.run_id.clone(),
            node_id: link.node_id.clone(),
            activation_id: link.activation_id.clone(),
            attempt: link.attempt,
            dispatch_identity: prepared.dispatch_identity,
            receipt: serde_json::json!({"turn_id": "turn-1"}),
            admitted_at_ms: 14,
        })
        .expect("receipt");
    store
        .reconcile_receipt_backed_attempts(&FailedObserver, 10, 20)
        .expect("failed reconciliation");
    store
        .retry_failed_node("run-1", "review", &activation_id(), 1, 21)
        .expect("retry");
    let second = store
        .prepare_pending_activation(
            "run-1",
            "review",
            &activation_id(),
            DispatchSideEffect::ReadOnly,
            serde_json::json!({"operation": "review"}),
            22,
        )
        .expect("prepare")
        .expect("second attempt");
    assert_eq!(second.attempt, 2);
    assert_eq!(
        store
            .execution_session_link("run-1", "review", &activation_id(), 1)
            .expect("original link"),
        Some(link)
    );
    assert!(
        store
            .execution_session_link("run-1", "review", &activation_id(), 2)
            .expect("new attempt link")
            .is_none()
    );
    assert!(store.request_cancellation("run-1", 23).expect("cancel"));
    drop(store);
    let reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    assert!(
        reopened
            .execution_session_link("run-1", "review", &activation_id(), 1)
            .expect("link after restart")
            .is_some()
    );
}

#[test]
fn cancelled_ambiguous_attempt_cannot_be_abandoned_for_retry() {
    let (_temp, mut store) = initialized_store();
    let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::ReadOnly);
    store
        .apply_attempt_observation(&identity, AttemptObservation::Unknown, 20)
        .expect("ambiguous");
    store.request_cancellation("run-1", 21).expect("cancel");
    let before = store.attempt_history("run-1", None, 10).expect("before");
    assert!(
        store
            .repair_attempt(
                &identity,
                &RepairResolution::AbandonForExplicitRetry {
                    reason: "attempt to retry cancelled work".into(),
                },
                22
            )
            .is_err()
    );
    let after = store.attempt_history("run-1", None, 10).expect("after");
    assert_eq!(after[0].status, before[0].status);
    assert_eq!(after[0].attempt, before[0].attempt);
    assert!(store.pending_activations(10).expect("pending").is_empty());
}

#[test]
fn explicit_operator_retry_remains_distinct_from_ambiguity_repair() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = initialized_store_at(temp.path());
    let identity = store
        .prepare_attempt(&PreparedAttempt {
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: activation_id(),
            attempt: 1,
            side_effect: DispatchSideEffect::Mutating,
            intent: serde_json::json!({"operation": "mutate"}),
            prepared_at_ms: 12,
        })
        .expect("prepare");
    store
        .reconcile_prepared_attempts(10, 20)
        .expect("repair required");
    assert!(
        store
            .retry_failed_node("run-1", "review", &activation_id(), 1, 21)
            .is_err()
    );
    drop(store);

    let mut reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    let repaired = reopened
        .repair_attempt(
            &identity,
            &RepairResolution::AbandonForExplicitRetry {
                reason: "operator verified no side effect".to_string(),
            },
            22,
        )
        .expect("repair");
    assert_eq!(repaired.attempt_status, "abandoned");
    assert_eq!(repaired.run_status, RunStatus::Running);
    assert!(
        reopened
            .retry_failed_node("run-1", "review", &activation_id(), 1, 23)
            .is_err()
    );
    let prepared = reopened
        .prepare_pending_activation(
            "run-1",
            "review",
            &activation_id(),
            DispatchSideEffect::Mutating,
            serde_json::json!({"operation": "mutate"}),
            24,
        )
        .expect("prepare")
        .expect("next attempt");
    assert_eq!(prepared.attempt, 2);
}

#[test]
fn receipt_discovery_bounds_exact_and_batch_payloads_without_writes() {
    let (_temp, mut store) = initialized_store();
    let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::Mutating);
    assert!(
        receipt_backed_attempt(&store.connection, &identity)
            .expect("valid")
            .is_some()
    );
    assert_eq!(
        receipt_backed_attempts(&store.connection, Some("run-1"), 1)
            .expect("valid")
            .len(),
        1
    );
    block_on(
        update("workflow_attempts")
            .value(
                "receipt_json",
                serde_json::json!("x".repeat(MAX_INLINE_JSON_BYTES)).to_string(),
            )
            .where_eq("dispatch_identity", identity.as_str())
            .execute(&store.connection),
    )
    .expect("oversized valid JSON");
    let before = fixture_changes(&store.connection);
    assert!(receipt_backed_attempt(&store.connection, &identity).is_err());
    assert!(receipt_backed_attempts(&store.connection, Some("run-1"), 1).is_err());
    assert!(receipt_backed_attempts(&store.connection, None, 1).is_err());
    assert_eq!(fixture_changes(&store.connection), before);
}

#[test]
fn cancellation_discovery_rejects_oversized_receipts_without_writes() {
    let (_temp, mut store) = initialized_store();
    let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::Mutating);
    store.request_cancellation("run-1", 20).expect("intent");
    assert_eq!(
        store
            .active_attempt_cancellations("run-1", 10)
            .expect("valid")
            .len(),
        1
    );
    for receipt in [
        serde_json::json!("x".repeat(MAX_INLINE_JSON_BYTES)).to_string(),
        "null".into(),
    ] {
        block_on(
            update("workflow_attempts")
                .value("receipt_json", receipt.as_str())
                .where_eq("dispatch_identity", identity.as_str())
                .execute(&store.connection),
        )
        .expect("damage");
        if receipt == "null" {
            // Small valid JSON remains readable; the operation owner decides its meaning.
            assert!(store.active_attempt_cancellations("run-1", 10).is_ok());
        } else {
            let before = fixture_changes(&store.connection);
            assert!(store.active_attempt_cancellations("run-1", 10).is_err());
            assert_eq!(fixture_changes(&store.connection), before);
        }
    }
}

#[tokio::test]
async fn receipt_page_advances_past_deferred_work() {
    struct Observer;
    impl AsyncAttemptStatusObserver for Observer {
        fn observe_async<'a>(
            &'a self,
            _: &'a AttemptReconciliationRequest,
        ) -> Pin<Box<dyn Future<Output = Result<AttemptObservation, WorkflowStoreError>> + Send + 'a>>
        {
            Box::pin(async {
                Ok(AttemptObservation::Deferred {
                    reason: "still active".into(),
                })
            })
        }
    }
    let (temp, mut store) = initialized_store();
    let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::Mutating);
    block_on(
        store.connection.exec_update_count(
            &(update("workflow_runs")
                .value("target_artifact_id", "a")
                .value("coordinator_daemon_instance_id", "d")
                .value("coordinator_generation", 1)
                .value("coordinator_fencing_token", "f")
                .where_eq("run_id", "run-1")),
        ),
    )
    .expect("owner");
    let authority = store.execution_authority("run-1").unwrap().unwrap();
    for invalid_cursor in ["short".to_string(), "g".repeat(64), "a".repeat(65)] {
        let before = fixture_changes(&store.connection);
        assert!(
            store
                .reconcile_owned_receipt_page(
                    &Observer,
                    "run-1",
                    &authority,
                    &invalid_cursor,
                    1,
                    19,
                )
                .await
                .is_err()
        );
        assert_eq!(fixture_changes(&store.connection), before);
    }
    let (summary, cursor) = store
        .reconcile_owned_receipt_page(&Observer, "run-1", &authority, "", 1, 20)
        .await
        .expect("first");
    assert_eq!(summary.deferred, *std::slice::from_ref(&identity));
    assert_eq!(cursor.as_deref(), Some(identity.as_str()));
    let (summary, cursor) = store
        .reconcile_owned_receipt_page(&Observer, "run-1", &authority, &identity, 1, 21)
        .await
        .expect("next");
    assert!(summary.deferred.is_empty());
    assert!(cursor.is_none());
    let first = store
        .advance_receipt_recovery(&Observer, "run-1", &authority, 1, 22)
        .await
        .expect("durable first");
    assert_eq!(first.deferred, *std::slice::from_ref(&identity));
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
    let next = store
        .advance_receipt_recovery(&Observer, "run-1", &authority, 1, 23)
        .await
        .expect("durable next");
    assert!(next.deferred.is_empty());
    let revisited = store
        .advance_receipt_recovery(&Observer, "run-1", &authority, 1, 24)
        .await
        .expect("next sweep");
    assert_eq!(revisited.deferred, *std::slice::from_ref(&identity));
    block_on(
        store.connection.exec_update_count(
            &(update("workflow_attempts")
                .value(
                    "receipt_json",
                    zero_blob(u32::try_from(MAX_INLINE_JSON_BYTES + 1).unwrap()),
                )
                .where_eq("dispatch_identity", identity.as_str())),
        ),
    )
    .expect("oversized receipt");
    let before = fixture_changes(&store.connection);
    assert!(
        store
            .reconcile_owned_receipt_page(&Observer, "run-1", &authority, "", 1, 25)
            .await
            .is_err()
    );
    assert_eq!(fixture_changes(&store.connection), before);
}

#[tokio::test]
async fn concurrent_receipt_cursor_progress_is_not_overwritten() {
    struct Observer(std::path::PathBuf);
    impl AsyncAttemptStatusObserver for Observer {
        fn observe_async<'a>(
            &'a self,
            _: &'a AttemptReconciliationRequest,
        ) -> Pin<Box<dyn Future<Output = Result<AttemptObservation, WorkflowStoreError>> + Send + 'a>>
        {
            Box::pin(async move {
                let store = WorkflowStore::open_at_path(&self.0)?;
                insert("workflow_receipt_cursors")
                    .value("run_id", "run-1")
                    .value("after_dispatch_identity", "later-progress")
                    .execute(&store.connection)
                    .await?;
                Ok(AttemptObservation::Running)
            })
        }
    }
    let (_temp, mut store) = initialized_store();
    prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::Mutating);
    block_on(
        store.connection.exec_update_count(
            &(update("workflow_runs")
                .value("target_artifact_id", "a")
                .value("coordinator_daemon_instance_id", "d")
                .value("coordinator_generation", 1)
                .value("coordinator_fencing_token", "f")
                .where_eq("run_id", "run-1")),
        ),
    )
    .expect("owner");
    let authority = store.execution_authority("run-1").unwrap().unwrap();
    let observer = Observer(store.path().to_path_buf());
    store
        .advance_receipt_recovery(&observer, "run-1", &authority, 1, 20)
        .await
        .expect("observe");
    let cursor = fixture_scalar_text(
        &store.connection,
        &select("workflow_receipt_cursors")
            .columns(&["after_dispatch_identity"])
            .where_eq("run_id", "run-1"),
        "after_dispatch_identity",
    )
    .expect("cursor");
    assert_eq!(cursor, "later-progress");
    let damages: Vec<Box<dyn Expression>> = vec![zero_blob(1_000_000).into(), 42.into()];
    for damage in damages {
        block_on(store.connection.exec_update_count(
            &(update("workflow_receipt_cursors").value("after_dispatch_identity", damage)),
        ))
        .expect("damaged cursor");
        let before = fixture_changes(&store.connection);
        assert!(
            store
                .advance_receipt_recovery(&observer, "run-1", &authority, 1, 21)
                .await
                .is_err()
        );
        assert_eq!(fixture_changes(&store.connection), before);
    }
    let before = fixture_changes(&store.connection);
    assert!(
        store
            .advance_receipt_recovery(&observer, "run-1", &authority, 1, 21)
            .await
            .is_err()
    );
    assert_eq!(fixture_changes(&store.connection), before);
}

#[tokio::test]
async fn receipt_observation_stops_between_requests_when_authority_changes() {
    verify_observation_stops_between_requests(true).await;
}

#[tokio::test]
async fn receipt_observation_stops_between_requests_when_graph_changes() {
    verify_observation_stops_between_requests(false).await;
}

async fn verify_observation_stops_between_requests(authority_change: bool) {
    struct Observer(std::path::PathBuf, std::sync::atomic::AtomicUsize, bool);
    impl AsyncAttemptStatusObserver for Observer {
        fn observe_async<'a>(
            &'a self,
            _: &'a AttemptReconciliationRequest,
        ) -> Pin<Box<dyn Future<Output = Result<AttemptObservation, WorkflowStoreError>> + Send + 'a>>
        {
            Box::pin(async move {
                self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let store = WorkflowStore::open_at_path(&self.0)?;
                if self.2 {
                    update("workflow_runs")
                        .value("coordinator_generation", 2)
                        .where_eq("run_id", "run-1")
                        .execute(&store.connection)
                        .await?;
                } else {
                    update("workflow_run_graphs")
                        .value("revision", 2)
                        .where_eq("run_id", "run-1")
                        .execute(&store.connection)
                        .await?;
                }
                Ok(AttemptObservation::Running)
            })
        }
    }
    let (_temp, mut store) = initialized_store();
    prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::Mutating);
    block_on(
        update("workflow_runs")
            .value("target_artifact_id", "a")
            .value("coordinator_daemon_instance_id", "d")
            .value("coordinator_generation", 1)
            .value("coordinator_fencing_token", "f")
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("owner");
    let authority = store.execution_authority("run-1").unwrap().unwrap();
    let revision = store.run_graph_revision("run-1").unwrap().unwrap();
    let request = receipt_backed_attempts(&store.connection, Some("run-1"), 1)
        .expect("requests")
        .pop()
        .expect("request");
    let observer = Observer(
        store.path().to_path_buf(),
        std::sync::atomic::AtomicUsize::new(0),
        authority_change,
    );
    let before = fixture_changes(&store.connection);
    assert!(
        store
            .reconcile_attempt_requests_with_authority_async(
                &observer,
                vec![request.clone(), request],
                20,
                Some(("run-1", &authority, revision)),
            )
            .await
            .is_err()
    );
    assert_eq!(observer.1.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(fixture_changes(&store.connection), before);
}

#[tokio::test]
async fn receipt_pages_advance_past_unresolved_attempts() {
    struct Observer;
    impl AsyncAttemptStatusObserver for Observer {
        fn observe_async<'a>(
            &'a self,
            _: &'a AttemptReconciliationRequest,
        ) -> Pin<Box<dyn Future<Output = Result<AttemptObservation, WorkflowStoreError>> + Send + 'a>>
        {
            Box::pin(async { Ok(AttemptObservation::Running) })
        }
    }
    let (_temp, mut store) = initialized_store();
    let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::Mutating);
    block_on(
        update("workflow_runs")
            .value("target_artifact_id", "artifact-a")
            .value("coordinator_daemon_instance_id", "daemon-a")
            .value("coordinator_generation", 1)
            .value("coordinator_fencing_token", "token-a")
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("authority");
    let authority = store
        .execution_authority("run-1")
        .expect("authority")
        .expect("owner");
    let (_, cursor) = store
        .reconcile_owned_receipts_page_async(&Observer, "run-1", &authority, (None, 1), 30)
        .await
        .expect("first page");
    assert_eq!(cursor.as_deref(), Some(identity.as_str()));
    let mut reopened = WorkflowStore::open_at_path(store.path()).expect("reopen");
    let (summary, next) = reopened
        .reconcile_owned_receipts_page_async(
            &Observer,
            "run-1",
            &authority,
            (cursor.as_deref(), 1),
            31,
        )
        .await
        .expect("next page");
    assert!(next.is_none());
    assert!(summary.succeeded.is_empty());
    assert_eq!(
        reopened
            .attempt_history("run-1", None, 10)
            .expect("history")[0]
            .status,
        "running"
    );
    block_on(
        update("workflow_runs")
            .value("coordinator_generation", 2)
            .where_eq("run_id", "run-1")
            .execute(&reopened.connection),
    )
    .expect("transfer");
    assert!(
        reopened
            .reconcile_owned_receipts_page_async(
                &Observer,
                "run-1",
                &authority,
                (cursor.as_deref(), 1),
                32
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn receipt_settlement_rechecks_authority_after_observation() {
    verify_receipt_observation_fence(true, true).await;
}

#[tokio::test]
async fn receipt_settlement_rechecks_graph_after_observation() {
    verify_receipt_observation_fence(false, false).await;
}

async fn verify_receipt_observation_fence(authority_change: bool, rejects: bool) {
    struct Observer(std::path::PathBuf, bool);
    impl AsyncAttemptStatusObserver for Observer {
        fn observe_async<'a>(
            &'a self,
            _: &'a AttemptReconciliationRequest,
        ) -> Pin<Box<dyn Future<Output = Result<AttemptObservation, WorkflowStoreError>> + Send + 'a>>
        {
            Box::pin(async move {
                let store = WorkflowStore::open_at_path(&self.0)?;
                if self.1 {
                    update("workflow_runs")
                        .value("coordinator_generation", 2)
                        .where_eq("run_id", "run-1")
                        .execute(&store.connection)
                        .await?;
                } else {
                    update("workflow_run_graphs")
                        .value("revision", 2)
                        .where_eq("run_id", "run-1")
                        .execute(&store.connection)
                        .await?;
                }
                Ok(AttemptObservation::Running)
            })
        }
    }
    let (_temp, mut store) = initialized_store();
    prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::Mutating);
    block_on(
        update("workflow_runs")
            .value("target_artifact_id", "artifact-a")
            .value("coordinator_daemon_instance_id", "daemon-a")
            .value("coordinator_generation", 1)
            .value("coordinator_fencing_token", "token-a")
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("authority");
    let authority = store
        .execution_authority("run-1")
        .expect("authority")
        .expect("owner");
    let observer = Observer(store.path().to_path_buf(), authority_change);
    let before = fixture_changes(&store.connection);
    assert_eq!(
        store
            .advance_receipt_recovery(&observer, "run-1", &authority, 10, 30)
            .await
            .is_err(),
        rejects,
    );
    let cursor_count: u64 = fixture_scalar_unsigned(
        &store.connection,
        &select("workflow_receipt_cursors")
            .project_as(switchy_database::query::count_all(), "count")
            .where_eq("run_id", "run-1"),
        "count",
    )
    .expect("cursor unchanged");
    assert_eq!(cursor_count, 0);
    assert_eq!(before, fixture_changes(&store.connection));
    assert_eq!(
        store.attempt_history("run-1", None, 10).expect("history")[0].status,
        "admitted"
    );
}

#[tokio::test]
async fn run_scoped_async_reconciliation_ignores_unrelated_owner_failure() {
    struct Observer;
    impl AsyncAttemptStatusObserver for Observer {
        fn observe_async<'a>(
            &'a self,
            request: &'a AttemptReconciliationRequest,
        ) -> Pin<Box<dyn Future<Output = Result<AttemptObservation, WorkflowStoreError>> + Send + 'a>>
        {
            Box::pin(async move {
                if request.run_id == "run-1" {
                    return Err(WorkflowStoreError::InvalidData(
                        "stale unrelated workflow output is invalid".to_string(),
                    ));
                }
                Ok(AttemptObservation::Running)
            })
        }
    }

    let (_temp, mut store) = initialized_store();
    let stale_identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::Mutating);
    store
        .create_run(&NewWorkflowRun {
            run_id: "run-2".to_string(),
            execution_authority: None,
            created_at_ms: 20,
            ..new_run()
        })
        .expect("second run");
    let second_activation = activation_identity("run-2", "review", 0);
    let attempt = PreparedAttempt {
        run_id: "run-2".to_string(),
        node_id: "review".to_string(),
        activation_id: second_activation.clone(),
        attempt: 1,
        side_effect: DispatchSideEffect::Mutating,
        intent: serde_json::json!({"operation": "review"}),
        prepared_at_ms: 21,
    };
    let second_identity = store.prepare_attempt(&attempt).expect("prepare second");
    store
        .persist_dispatch_receipt(&DispatchReceipt {
            run_id: attempt.run_id,
            node_id: attempt.node_id,
            activation_id: attempt.activation_id,
            attempt: 1,
            dispatch_identity: second_identity.clone(),
            receipt: serde_json::json!({"turn_id": "turn-2"}),
            admitted_at_ms: 22,
        })
        .expect("second receipt");

    let summary = store
        .reconcile_receipt_backed_attempts_for_run_async(&Observer, "run-2", 10, 23)
        .await
        .expect("unrelated failure must not fail run-scoped reconciliation");

    assert!(summary.succeeded.is_empty());
    assert_eq!(
        store
            .attempt_history("run-1", None, 10)
            .expect("stale attempt")
            .pop()
            .expect("stale attempt")
            .status,
        "admitted"
    );
    assert_eq!(
        store
            .attempt_history("run-2", None, 10)
            .expect("second attempt")
            .pop()
            .expect("second attempt")
            .status,
        "running"
    );
    assert_eq!(
        store
            .active_attempts_for_run("run-1", 10)
            .expect("stale active attempt")[0]
            .dispatch_identity,
        stale_identity
    );
    assert_eq!(
        store
            .active_attempts_for_run("run-2", 10)
            .expect("second active attempt")[0]
            .dispatch_identity,
        second_identity
    );
}

#[tokio::test]
async fn unknown_cancellation_outcome_requires_repair_not_success() {
    struct Observer;
    impl AsyncAttemptStatusObserver for Observer {
        fn observe_async<'a>(
            &'a self,
            _: &'a AttemptReconciliationRequest,
        ) -> Pin<Box<dyn Future<Output = Result<AttemptObservation, WorkflowStoreError>> + Send + 'a>>
        {
            Box::pin(async { Ok(AttemptObservation::Unknown) })
        }
    }
    let (temp, mut store) = initialized_store();
    let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::Mutating);
    store.request_cancellation("run-1", 20).expect("intent");
    store
        .mark_cancellation_signalled(&identity, 21)
        .expect("signal");
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
    let summary = store
        .reconcile_receipt_backed_attempts_async(&Observer, 10, 22)
        .await
        .expect("observe");
    assert!(summary.cancelled.is_empty());
    assert_eq!(summary.repair_required, vec![identity]);
    assert_eq!(
        store.attempt_history("run-1", None, 10).expect("history")[0].status,
        "repair_required"
    );
    assert_eq!(
        store
            .run_summary("run-1")
            .expect("summary")
            .expect("run")
            .status,
        RunStatus::RepairRequired
    );
}

#[tokio::test]
async fn cancellation_recovery_requires_terminal_owner_evidence_after_restart() {
    struct Observer(AttemptObservation);
    impl AsyncAttemptStatusObserver for Observer {
        fn observe_async<'a>(
            &'a self,
            _request: &'a AttemptReconciliationRequest,
        ) -> Pin<Box<dyn Future<Output = Result<AttemptObservation, WorkflowStoreError>> + Send + 'a>>
        {
            Box::pin(async { Ok(self.0.clone()) })
        }
    }

    let (temp, mut store) = initialized_store();
    let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::Mutating);
    store.request_cancellation("run-1", 20).expect("intent");
    store
        .mark_cancellation_signalled(&identity, 21)
        .expect("signal");
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");

    for observation in [
        AttemptObservation::Running,
        AttemptObservation::Deferred {
            reason: "owner temporarily unavailable".to_string(),
        },
    ] {
        let summary = store
            .reconcile_receipt_backed_attempts_async(&Observer(observation), 10, 22)
            .await
            .expect("observe cancellation progress");
        assert!(summary.cancelled.is_empty());
        assert_ne!(
            store
                .run_summary("run-1")
                .expect("summary")
                .expect("run")
                .status,
            RunStatus::Cancelled
        );
        let attempts = store.attempt_history("run-1", None, 10).expect("attempts");
        assert_eq!(attempts[0].status, "cancelling");
        assert!(attempts[0].terminal_at_ms.is_none());
    }

    let summary = store
        .reconcile_receipt_backed_attempts_async(&Observer(AttemptObservation::Cancelled), 10, 23)
        .await
        .expect("confirmed cancellation");
    assert_eq!(summary.cancelled, [identity]);
    assert_eq!(
        store
            .run_summary("run-1")
            .expect("summary")
            .expect("run")
            .status,
        RunStatus::Cancelled
    );
}

#[test]
fn receipt_backed_restart_reconciliation_persists_terminal_success() {
    struct Observer;
    impl AttemptStatusObserver for Observer {
        fn observe(
            &self,
            request: &AttemptReconciliationRequest,
        ) -> Result<AttemptObservation, WorkflowStoreError> {
            Ok(AttemptObservation::Succeeded {
                output: ValidatedOutput {
                    output_id: "output-1".to_string(),
                    run_id: request.run_id.clone(),
                    node_id: request.node_id.clone(),
                    activation_id: request.activation_id.clone(),
                    schema_id: "u32".to_string(),
                    schema_version: 1,
                    value: serde_json::json!(1),
                    artifact_reference: None,
                    created_at_ms: 20,
                },
            })
        }
    }

    let (_temp, mut store) = initialized_store();
    let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::ReadOnly);
    let summary = store
        .reconcile_receipt_backed_attempts(&Observer, 10, 20)
        .expect("reconcile");
    assert_eq!(summary.succeeded, [identity]);
    let attempt = store
        .attempt_history("run-1", None, 10)
        .expect("attempt")
        .pop()
        .expect("row");
    assert_eq!(attempt.status, "succeeded");
    let activation_status: String = fixture_scalar_text(
        &store.connection,
        &select("workflow_activations")
            .columns(&["status"])
            .where_eq("run_id", "run-1"),
        "status",
    )
    .expect("activation");
    assert_eq!(activation_status, "completed");
    let receipt = DispatchReceipt {
        run_id: "run-1".into(),
        node_id: "review".into(),
        activation_id: activation_id(),
        attempt: 1,
        dispatch_identity: attempt.dispatch_identity,
        receipt: serde_json::json!({"turn_id": "turn-1"}),
        admitted_at_ms: 13,
    };
    let before = fixture_changes(&store.connection);
    store
        .persist_dispatch_receipt(&receipt)
        .expect("terminal exact receipt redelivery");
    assert_eq!(fixture_changes(&store.connection), before);
    let mut conflicting = receipt;
    conflicting.receipt = serde_json::json!({"turn_id": "different"});
    assert!(store.persist_dispatch_receipt(&conflicting).is_err());
    assert_eq!(fixture_changes(&store.connection), before);
}

#[test]
fn pending_work_rejects_inconsistent_executable_binding_without_writes() {
    let (_temp, store) = initialized_store();
    assert!(
        !store
            .pending_activations_for_run("run-1", 10)
            .expect("pending work")
            .is_empty()
    );
    block_on(
        update("workflow_activations")
            .value("node_revision", 2)
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("invalid binding fixture");
    let before = fixture_changes(&store.connection);
    assert!(store.pending_activations_for_run("run-1", 10).is_err());
    assert_eq!(fixture_changes(&store.connection), before);
}

#[test]
fn fan_out_work_rejects_inconsistent_controller_binding_without_writes() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("fan-out", 1, &fan_out_definition())
        .expect("definition");
    let mut run = new_run();
    run.definition_id = "fan-out".to_string();
    run.input = Some(serde_json::json!([3, 1, 2]));
    store.create_run(&run).expect("run");
    store
        .settle_pending_control_nodes("run-1", 10, 20)
        .expect("materialize");
    let members = store
        .pending_activations_for_run("run-1", 10)
        .expect("members");
    assert_eq!(members.len(), 2);
    block_on(
        update("workflow_activations")
            .value("node_revision", 2)
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("invalid binding");
    let before = fixture_changes(&store.connection);
    assert!(store.pending_activations_for_run("run-1", 10).is_err());
    for member in members {
        assert!(
            store
                .prepare_pending_activation(
                    &member.run_id,
                    &member.node_id,
                    &member.activation_id,
                    DispatchSideEffect::ReadOnly,
                    serde_json::json!({}),
                    21,
                )
                .is_err()
        );
    }
    assert_eq!(fixture_changes(&store.connection), before);
}

#[test]
fn pending_fan_out_requires_run_owned_controller_and_does_not_repair() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    let plan = fan_out_definition();
    let controller_id = plan.entries[0].clone();
    store
        .persist_definition("fan-out", 1, &plan)
        .expect("definition");
    let mut run = new_run();
    run.definition_id = "fan-out".to_string();
    run.input = Some(serde_json::json!([3, 1, 2]));
    store.create_run(&run).expect("run");
    store
        .settle_pending_control_nodes("run-1", 10, 20)
        .expect("materialize");
    assert_eq!(
        store
            .pending_activations_for_run("run-1", 10)
            .expect("members")
            .len(),
        2
    );
    let member = store
        .pending_activations_for_run("run-1", 1)
        .expect("member")
        .remove(0);
    block_on(
        delete("workflow_run_graph_nodes")
            .where_eq("run_id", "run-1")
            .where_eq("node_id", controller_id.as_str())
            .execute(&store.connection),
    )
    .expect("damage controller");
    let before = fixture_changes(&store.connection);
    let error = store
        .pending_activations_for_run("run-1", 10)
        .expect_err("missing controller");
    assert!(
        error
            .to_string()
            .contains("run-graph controller node is missing")
    );
    assert_eq!(fixture_changes(&store.connection), before);
    assert!(
        store
            .prepare_pending_activation(
                "run-1",
                &member.node_id,
                &member.activation_id,
                DispatchSideEffect::ReadOnly,
                serde_json::json!({}),
                30,
            )
            .expect_err("must not prepare from authored definition")
            .to_string()
            .contains("run-graph controller node is missing")
    );
    let attempts: u64 = fixture_scalar_unsigned(
        &store.connection,
        &select("workflow_attempts")
            .project_as(switchy_database::query::count_all(), "count")
            .where_eq("run_id", "run-1"),
        "count",
    )
    .expect("attempt count");
    assert_eq!(attempts, 0);
    drop(store);
    let reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    assert!(reopened.pending_activations_for_run("run-1", 10).is_err());
}

#[test]
fn fan_out_preparation_is_atomic_and_not_repeated_after_reopen() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("fan-out", 1, &fan_out_definition())
        .expect("definition");
    let mut run = new_run();
    run.definition_id = "fan-out".to_string();
    run.input = Some(serde_json::json!([3, 1, 2]));
    store.create_run(&run).expect("run");
    store
        .settle_pending_control_nodes("run-1", 10, 20)
        .expect("materialize");
    let member = store
        .pending_activations_for_run("run-1", 1)
        .expect("pending")
        .remove(0);
    let prepared = store
        .prepare_pending_activation(
            "run-1",
            &member.node_id,
            &member.activation_id,
            DispatchSideEffect::ReadOnly,
            serde_json::json!({"operation": "test"}),
            30,
        )
        .expect("prepare")
        .expect("admitted");
    assert_eq!(prepared.activation, member);
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    assert!(
        store
            .prepare_pending_activation(
                "run-1",
                &member.node_id,
                &member.activation_id,
                DispatchSideEffect::ReadOnly,
                serde_json::json!({"operation": "test"}),
                31,
            )
            .expect("duplicate")
            .is_none()
    );
    assert_eq!(
        fixture_scalar_unsigned(
            &store.connection,
            &select("workflow_attempts")
                .project_as(switchy_database::query::count_all(), "count")
                .where_eq("run_id", "run-1"),
            "count"
        )
        .expect("attempts"),
        1
    );
    assert!(
        store
            .pending_activations_for_run("run-1", 10)
            .expect("remaining")
            .iter()
            .all(|pending| pending.activation_id != member.activation_id)
    );
}

#[test]
fn fan_out_receipt_backed_success_aggregates_across_restarts() {
    verify_fan_out_receipt_backed_completion(false);
}

#[test]
fn fan_out_receipt_backed_wait_all_failure_settles_across_restarts() {
    verify_fan_out_receipt_backed_completion(true);
}

fn verify_fan_out_receipt_backed_completion(fail_first: bool) {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("fan-out", 1, &fan_out_definition())
        .expect("definition");
    let mut run = new_run();
    run.definition_id = "fan-out".to_string();
    run.input = Some(serde_json::json!([3, 1, 2]));
    store.create_run(&run).expect("run");
    store
        .settle_pending_control_nodes("run-1", 10, 20)
        .expect("materialize");
    for index in 0..3 {
        let member = store
            .pending_activations_for_run("run-1", 1)
            .expect("pending")
            .remove(0);
        let prepared = store
            .prepare_pending_activation(
                "run-1",
                &member.node_id,
                &member.activation_id,
                DispatchSideEffect::ReadOnly,
                serde_json::json!({"operation": "test"}),
                30,
            )
            .expect("prepare")
            .expect("admitted");
        store
            .persist_dispatch_receipt(&DispatchReceipt {
                run_id: member.run_id.clone(),
                node_id: member.node_id.clone(),
                activation_id: member.activation_id.clone(),
                attempt: prepared.attempt,
                dispatch_identity: prepared.dispatch_identity.clone(),
                receipt: serde_json::json!({"owner": "test"}),
                admitted_at_ms: 31,
            })
            .expect("receipt");
        drop(store);
        store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen receipt");
        let output = ValidatedOutput {
            output_id: format!("{}:output", member.activation_id),
            run_id: member.run_id,
            node_id: member.node_id,
            activation_id: member.activation_id,
            schema_id: member.node.output.type_name,
            schema_version: 1,
            value: member.input.expect("member input"),
            artifact_reference: None,
            created_at_ms: 40,
        };
        let failed = fail_first && index == 0;
        let observation = if failed {
            AttemptObservation::Failed {
                message: "member failed".to_string(),
            }
        } else {
            AttemptObservation::Succeeded { output }
        };
        verify_fan_out_activation_damage_rejects_observation(
            &mut store,
            &prepared.dispatch_identity,
            &observation,
        );
        let summary = store
            .apply_attempt_observation(&prepared.dispatch_identity, observation, 40)
            .expect("observe outcome");
        if failed {
            assert_eq!(summary.failed, [prepared.dispatch_identity]);
        } else {
            assert_eq!(summary.succeeded, [prepared.dispatch_identity]);
        }
    }
    drop(store);
    let store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen terminal");
    verify_fan_out_terminal_result(&store, fail_first);
}

fn verify_fan_out_activation_damage_rejects_observation(
    store: &mut WorkflowStore,
    identity: &str,
    observation: &AttemptObservation,
) {
    let activation = fixture_scalar_text(
        &store.connection,
        &select("workflow_attempts")
            .columns(&["activation_id"])
            .where_eq("dispatch_identity", identity),
        "activation_id",
    )
    .expect("attempt activation");
    block_on(
        update("workflow_activations")
            .value("status", "cancelled")
            .where_eq("activation_id", activation.as_str())
            .execute(&store.connection),
    )
    .expect("damage activation");
    assert!(
        store
            .apply_attempt_observation(identity, observation.clone(), 39)
            .expect_err("inconsistent activation")
            .to_string()
            .contains("activation is not running")
    );
    assert_eq!(
        {
            let member = fixture_scalar_text(
                &store.connection,
                &select("workflow_attempts")
                    .columns(&["activation_id"])
                    .where_eq("dispatch_identity", identity),
                "activation_id",
            )
            .expect("attempt member identity");
            fixture_scalar_text(
                &store.connection,
                &select("workflow_fan_out_members")
                    .columns(&["status"])
                    .where_eq("member_activation_id", member.as_str()),
                "status",
            )
            .expect("member rollback")
        },
        "running"
    );
    assert_eq!(
        fixture_scalar_text(
            &store.connection,
            &select("workflow_attempts")
                .columns(&["status"])
                .where_eq("dispatch_identity", identity),
            "status"
        )
        .expect("attempt unchanged"),
        "admitted"
    );
    let activation = fixture_scalar_text(
        &store.connection,
        &select("workflow_attempts")
            .columns(&["activation_id"])
            .where_eq("dispatch_identity", identity),
        "activation_id",
    )
    .expect("attempt activation");
    block_on(
        update("workflow_activations")
            .value("status", "running")
            .where_eq("activation_id", activation.as_str())
            .execute(&store.connection),
    )
    .expect("restore fixture");
}

fn verify_fan_out_terminal_result(store: &WorkflowStore, failed: bool) {
    let output = store.canonical_terminal_output("run-1").expect("output");
    if failed {
        assert!(output.is_none());
    } else {
        assert_eq!(
            output.expect("completed").value,
            serde_json::json!({"version": 1, "members": [
                {"index": 0, "value": 3}, {"index": 1, "value": 1}, {"index": 2, "value": 2}
            ]})
        );
    }
    let status = if failed { "failed" } else { "completed" };
    assert_eq!(
        fixture_scalar_text(
            &store.connection,
            &select("workflow_runs")
                .columns(&["status"])
                .where_eq("run_id", "run-1"),
            "status"
        )
        .expect("run status"),
        status
    );
    assert_eq!(
        fixture_scalar_text(
            &store.connection,
            &select("workflow_activations")
                .columns(&["status"])
                .where_eq("run_id", "run-1")
                .where_eq("node_id", "fan-out"),
            "status"
        )
        .expect("controller status"),
        status
    );
    let attempts = store.attempt_history("run-1", None, 10).expect("attempts");
    assert_eq!(attempts.len(), 3);
    assert_eq!(
        attempts
            .iter()
            .filter(|attempt| attempt.status == "failed")
            .count(),
        usize::from(failed)
    );
    assert_eq!(
        attempts
            .iter()
            .filter(|attempt| attempt.status == "succeeded")
            .count(),
        3 - usize::from(failed)
    );
    assert!(
        store
            .pending_activations_for_run("run-1", 10)
            .expect("pending")
            .is_empty()
    );
}

fn verify_failed_member_and_finish(store: WorkflowStore, request: &AttemptReconciliationRequest) {
    assert_eq!(
        store
            .pending_activations_for_run("run-1", 10)
            .expect("next wave")
            .len(),
        2
    );
    assert_eq!(
        fixture_scalar_text(
            &store.connection,
            &select("workflow_fan_out_members")
                .columns(&["status"])
                .where_eq("member_activation_id", request.activation_id.as_str()),
            "status"
        )
        .expect("member status"),
        "failed"
    );
    assert_eq!(
        fixture_scalar_text(
            &store.connection,
            &select("workflow_activations")
                .columns(&["status"])
                .where_eq("run_id", request.run_id.as_str())
                .where_eq("activation_id", request.activation_id.as_str()),
            "status"
        )
        .expect("activation status"),
        "failed"
    );
    assert_eq!(
        fixture_scalar_text(
            &store.connection,
            &select("workflow_runs")
                .columns(&["status"])
                .where_eq("run_id", request.run_id.as_str()),
            "status"
        )
        .expect("run status"),
        "running"
    );
    finish_wait_all_members_and_verify_failure(store);
}

fn finish_wait_all_members_and_verify_failure(mut store: WorkflowStore) {
    let members = store
        .pending_activations_for_run("run-1", 10)
        .expect("members");
    for member in members {
        store
            .prepare_pending_activation(
                "run-1",
                &member.node_id,
                &member.activation_id,
                DispatchSideEffect::ReadOnly,
                serde_json::json!({"operation": "test"}),
                50,
            )
            .expect("prepare")
            .expect("admitted");
        let request = AttemptReconciliationRequest {
            run_id: member.run_id.clone(),
            node_id: member.node_id.clone(),
            activation_id: member.activation_id.clone(),
            attempt: 1,
            dispatch_identity: "test".to_string(),
            side_effect: DispatchSideEffect::ReadOnly,
            receipt: serde_json::json!({}),
        };
        let output = ValidatedOutput {
            output_id: format!("{}:output", member.activation_id),
            run_id: member.run_id,
            node_id: member.node_id,
            activation_id: member.activation_id,
            schema_id: member.node.output.type_name,
            schema_version: 1,
            value: serde_json::json!(1),
            artifact_reference: None,
            created_at_ms: 60,
        };
        let transaction =
            database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
        settle_fan_out_member_success(transaction.as_ref(), &request, &output, 60)
            .expect("success");
        database::commit(transaction).expect("commit");
    }
    assert_eq!(
        fixture_scalar_text(
            &store.connection,
            &select("workflow_runs")
                .columns(&["status"])
                .where_eq("run_id", "run-1"),
            "status"
        )
        .expect("run status"),
        "failed"
    );
    assert_eq!(
        fixture_scalar_text(
            &store.connection,
            &select("workflow_activations")
                .columns(&["status"])
                .where_eq("run_id", "run-1")
                .where_eq("node_id", "fan-out"),
            "status"
        )
        .expect("controller status"),
        "failed"
    );
    assert!(
        store
            .canonical_terminal_output("run-1")
            .expect("terminal output")
            .is_none()
    );
}

#[test]
fn fan_out_member_output_is_validated_before_releasing_capacity() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("fan-out", 1, &fan_out_definition())
        .expect("definition");
    let mut run = new_run();
    run.definition_id = "fan-out".to_string();
    run.input = Some(serde_json::json!([3, 1, 2]));
    store.create_run(&run).expect("run");
    store
        .settle_pending_control_nodes("run-1", 10, 20)
        .expect("materialize");
    let member = store
        .pending_activations_for_run("run-1", 1)
        .expect("pending")
        .remove(0);
    store
        .prepare_pending_activation(
            "run-1",
            &member.node_id,
            &member.activation_id,
            DispatchSideEffect::ReadOnly,
            serde_json::json!({"operation": "test"}),
            30,
        )
        .expect("prepare")
        .expect("admitted");
    let request = AttemptReconciliationRequest {
        run_id: member.run_id.clone(),
        node_id: member.node_id.clone(),
        activation_id: member.activation_id.clone(),
        attempt: 1,
        dispatch_identity: "test".to_string(),
        side_effect: DispatchSideEffect::ReadOnly,
        receipt: serde_json::json!({}),
    };
    let mut output = ValidatedOutput {
        output_id: "member-output".to_string(),
        run_id: member.run_id,
        node_id: member.node_id,
        activation_id: member.activation_id,
        schema_id: member.node.output.type_name,
        schema_version: 1,
        value: serde_json::json!("invalid"),
        artifact_reference: None,
        created_at_ms: 40,
    };
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    let db: &dyn Database = transaction.as_ref();
    let before = fixture_changes(db);
    assert!(
        settle_fan_out_member_success(db, &request, &output, 40)
            .expect_err("invalid member value")
            .to_string()
            .contains("does not match")
    );
    assert_eq!(fixture_changes(db), before);
    output.value = serde_json::json!(3);
    let schema_id = output.schema_id.clone();
    output.schema_id = "wrong-schema".to_string();
    assert!(
        settle_fan_out_member_success(db, &request, &output, 40)
            .expect_err("invalid schema identity")
            .to_string()
            .contains("schema identity mismatch")
    );
    assert_eq!(fixture_changes(db), before);
    output.schema_id = schema_id;
    fixture_damage_fan_out_controller_binding(db);
    let before = fixture_changes(db);
    assert!(settle_fan_out_member_success(db, &request, &output, 40).is_err());
    assert!(settle_fan_out_member_failure(db, &request, "failure", 40).is_err());
    assert_eq!(fixture_changes(db), before);
    fixture_restore_fan_out_controller_binding(db);
    settle_fan_out_member_success(db, &request, &output, 40).expect("valid member");
    database::commit(transaction).expect("commit");
    drop(store);
    let store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    assert_eq!(
        store
            .pending_activations_for_run("run-1", 10)
            .expect("next wave")
            .len(),
        2
    );
    assert_eq!(
        fixture_scalar_text(
            &store.connection,
            &select("workflow_fan_out_members")
                .columns(&["output_json"])
                .where_eq("member_activation_id", request.activation_id.as_str()),
            "output_json"
        )
        .expect("persisted member"),
        "3"
    );
}

fn fixture_restore_fan_out_controller_binding(db: &dyn Database) {
    block_on(
        update("workflow_activations")
            .value("node_revision", 1)
            .execute(db),
    )
    .expect("restore bindings");
}

fn fixture_damage_fan_out_controller_binding(db: &dyn Database) {
    block_on(
        update("workflow_activations")
            .value("node_revision", 2)
            .execute(db),
    )
    .expect("invalid controller binding");
}

fn fan_out_retirement_fixture() -> (tempfile::TempDir, WorkflowStore, NewWorkflowRun) {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("fan-out", 1, &fan_out_definition())
        .expect("definition");
    let mut run = new_run();
    run.definition_id = "fan-out".into();
    run.input = Some(serde_json::json!([3, 1, 2]));
    store.create_run(&run).expect("run");
    (temp, store, run)
}

fn exercise_fan_out_retirement(succeeds: Option<bool>) {
    let (temp, mut store, run) = fan_out_retirement_fixture();
    fixture_assign_fan_out_owner(&store.connection);
    let authority = store
        .execution_authority(&run.run_id)
        .expect("authority")
        .expect("owner");
    let summary = store
        .settle_pending_control_nodes(&run.run_id, 10, 20)
        .expect("materialize");
    let member = &summary.activated[0];
    let prepared = store
        .prepare_pending_activation(
            &run.run_id,
            &member.node_id,
            &member.activation_id,
            DispatchSideEffect::Mutating,
            serde_json::json!({}),
            20,
        )
        .expect("prepare child")
        .expect("child");
    let mut request = bcode_workflow::WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: run.run_id.clone(),
        mutation_id: "cancel-fan-out".into(),
        expected_revision: 1,
        edits: vec![bcode_workflow::WorkflowRunGraphEdit::RemoveNode {
            node_id: "fan-out".into(),
        }],
        reconciliation: vec![bcode_workflow::WorkflowRunGraphReconciliation::Cancel {
            activation_id: activation_identity(&run.run_id, "fan-out", 0),
        }],
    };
    let mut replacement = sequential_definition().nodes["first"].clone();
    replacement.id = "replacement".into();
    replacement.input = fan_out_definition().nodes["fan-out"].input.clone();
    request
        .edits
        .push(bcode_workflow::WorkflowRunGraphEdit::AddNode {
            node: replacement,
            entry: true,
            exit: true,
        });
    request
        .reconciliation
        .extend(summary.activated.iter().map(|member| {
            bcode_workflow::WorkflowRunGraphReconciliation::Cancel {
                activation_id: member.activation_id.clone(),
            }
        }));
    let mut uncovered = request.clone();
    uncovered.mutation_id = "uncovered-fan-out".into();
    uncovered.reconciliation.pop();
    store
        .stage_run_graph_edit(&uncovered, &authority, 21)
        .expect("stage uncovered");
    assert!(
        store
            .publish_retained_leaf_run_graph_edit(
                &run.run_id,
                &uncovered.mutation_id,
                &authority,
                22
            )
            .is_err()
    );
    assert_eq!(
        store.run_graph_revision(&run.run_id).expect("revision"),
        Some(1)
    );
    store
        .stage_run_graph_edit(&request, &authority, 21)
        .expect("stage");
    complete_fan_out_publication_cancellation(
        &mut store, &run, &request, &authority, &prepared, succeeds,
    );
    if succeeds.is_some() {
        return;
    }
    let receipt = DispatchReceipt {
        run_id: run.run_id.clone(),
        node_id: member.node_id.clone(),
        activation_id: member.activation_id.clone(),
        attempt: prepared.attempt,
        dispatch_identity: prepared.dispatch_identity,
        receipt: serde_json::json!({"accepted":true}),
        admitted_at_ms: 23,
    };
    let before = fixture_changes(&store.connection);
    store
        .persist_dispatch_receipt(&receipt)
        .expect("exact cancelled receipt is idempotent");
    assert_eq!(fixture_changes(&store.connection), before);
    let mut conflicting = receipt;
    conflicting.receipt = serde_json::json!({"accepted":"conflict"});
    assert!(store.persist_dispatch_receipt(&conflicting).is_err());
    assert_eq!(fixture_changes(&store.connection), before);
    drop(store);
    assert_retired_fan_out_after_reopen(temp.path(), &run.run_id);
}

fn fixture_assign_fan_out_owner(db: &dyn Database) {
    block_on(
        update("workflow_runs")
            .value("target_artifact_id", "artifact-a")
            .value("coordinator_daemon_instance_id", "daemon-a")
            .value("coordinator_generation", 1)
            .value("coordinator_fencing_token", "token-a")
            .execute(db),
    )
    .expect("owner");
}

struct CancelledFanOut;
impl AttemptStatusObserver for CancelledFanOut {
    fn observe(
        &self,
        _: &AttemptReconciliationRequest,
    ) -> Result<AttemptObservation, WorkflowStoreError> {
        Ok(AttemptObservation::Cancelled)
    }
}

fn complete_fan_out_publication_cancellation(
    store: &mut WorkflowStore,
    run: &NewWorkflowRun,
    request: &bcode_workflow::WorkflowRunGraphEditBatch,
    authority: &WorkflowExecutionAuthority,
    prepared: &PreparedActivationDispatch,
    succeeds: Option<bool>,
) {
    store
        .persist_dispatch_receipt(&DispatchReceipt {
            run_id: run.run_id.clone(),
            node_id: prepared.activation.node_id.clone(),
            activation_id: prepared.activation.activation_id.clone(),
            attempt: prepared.attempt,
            dispatch_identity: prepared.dispatch_identity.clone(),
            receipt: serde_json::json!({"accepted":true}),
            admitted_at_ms: 22,
        })
        .expect("receipt");
    let sibling = store
        .pending_activations_for_run(&run.run_id, 10)
        .expect("sibling")
        .remove(0);
    let sibling = store
        .prepare_pending_activation(
            &run.run_id,
            &sibling.node_id,
            &sibling.activation_id,
            DispatchSideEffect::Mutating,
            serde_json::json!({}),
            22,
        )
        .expect("prepare sibling")
        .expect("prepared");
    store
        .accept_pending_run_graph_publication(&run.run_id, &request.mutation_id, authority, 23)
        .expect("accept");
    assert!(
        store
            .record_dispatch_handoff(&sibling, Some(authority))
            .is_err()
    );
    let handed_off = fixture_scalar_unsigned(
        &store.connection,
        &select("workflow_dispatch_handoffs")
            .columns(&["handed_off"])
            .where_eq("dispatch_identity", sibling.dispatch_identity.as_str()),
        "handed_off",
    )
    .expect("handoff")
        != 0;
    assert!(!handed_off);
    assert_eq!(
        store
            .pending_publication_cancellations(&run.run_id, authority, 10)
            .expect("intents")
            .len(),
        1
    );
    let pending = store
        .pending_activations_for_run(&run.run_id, 10)
        .expect("pending sibling");
    for sibling in pending {
        assert!(
            store
                .prepare_pending_activation(
                    &run.run_id,
                    &sibling.node_id,
                    &sibling.activation_id,
                    DispatchSideEffect::Mutating,
                    serde_json::json!({}),
                    24
                )
                .expect("fenced sibling")
                .is_none()
        );
    }
    store
        .mark_publication_cancellation_signalled(
            &run.run_id,
            &prepared.dispatch_identity,
            authority,
            24,
        )
        .expect("signal");
    if let Some(failed) = succeeds {
        assert_competing_fan_out_success(store, run, request, authority, failed);
        return;
    }
    store
        .reconcile_receipt_backed_attempts(&CancelledFanOut, 10, 25)
        .expect("owner cancelled");
    assert_eq!(
        store
            .finalize_pending_run_graph_publication(
                &run.run_id,
                &request.mutation_id,
                authority,
                26
            )
            .expect("finalize"),
        bcode_workflow::WorkflowRunGraphPublicationStatus::Committed { revision: 2 }
    );
}

fn assert_competing_fan_out_success(
    store: &mut WorkflowStore,
    run: &NewWorkflowRun,
    request: &bcode_workflow::WorkflowRunGraphEditBatch,
    authority: &WorkflowExecutionAuthority,
    failed: bool,
) {
    struct Succeeded(bool);
    impl AttemptStatusObserver for Succeeded {
        fn observe(
            &self,
            request: &AttemptReconciliationRequest,
        ) -> Result<AttemptObservation, WorkflowStoreError> {
            if self.0 {
                return Ok(AttemptObservation::Failed {
                    message: "competing failure".into(),
                });
            }
            Ok(AttemptObservation::Succeeded {
                output: ValidatedOutput {
                    output_id: "competing-success".into(),
                    run_id: request.run_id.clone(),
                    node_id: request.node_id.clone(),
                    activation_id: request.activation_id.clone(),
                    schema_id: "u32".into(),
                    schema_version: 1,
                    value: serde_json::json!(7),
                    artifact_reference: None,
                    created_at_ms: 25,
                },
            })
        }
    }
    store
        .reconcile_receipt_backed_attempts(&Succeeded(failed), 10, 25)
        .expect("success");
    let state_rows = fixture_rows(
        &store.connection,
        &select("workflow_fan_out_members")
            .columns(&["status", "output_json"])
            .where_eq("run_id", run.run_id.as_str())
            .where_eq("member_index", 0),
    );
    let row = state_rows.first().expect("member result");
    let state = (
        fixture_text(row, "status"),
        database::optional_text(row, "output_json").expect("member output"),
    );
    let expected = if failed { "failed" } else { "completed" };
    assert_eq!(state, (expected.into(), (!failed).then(|| "7".into())));
    let statuses: Vec<String> = fixture_rows(
        &store.connection,
        &select("workflow_fan_out_members")
            .columns(&["status"])
            .where_eq("run_id", run.run_id.as_str())
            .sort("member_index", switchy_database::query::SortDirection::Asc),
    )
    .iter()
    .map(|row| fixture_text(row, "status"))
    .collect();
    assert_eq!(statuses, [expected, "running", "waiting"]);
    assert_eq!(
        store
            .finalize_pending_run_graph_publication(
                &run.run_id,
                &request.mutation_id,
                authority,
                26
            )
            .expect("terminal conflict"),
        bcode_workflow::WorkflowRunGraphPublicationStatus::Conflicted {
            expected_revision: 1,
            current_revision: 1
        }
    );
    assert_eq!(
        store.run_graph_revision(&run.run_id).expect("revision"),
        Some(1)
    );
    recover_conflicted_fan_out(store, run, request, authority, expected);
}

fn recover_conflicted_fan_out(
    store: &mut WorkflowStore,
    run: &NewWorkflowRun,
    request: &bcode_workflow::WorkflowRunGraphEditBatch,
    authority: &WorkflowExecutionAuthority,
    expected: &str,
) {
    let terminal = fixture_scalar_text(
        &store.connection,
        &select("workflow_fan_out_members")
            .columns(&["member_activation_id"])
            .where_eq("run_id", run.run_id.as_str())
            .where_eq("member_index", 0),
        "member_activation_id",
    )
    .expect("terminal");
    let mut revised = request.clone();
    revised.mutation_id = "reconcile-terminal-member".into();
    revised.reconciliation.retain(|item| !matches!(item, bcode_workflow::WorkflowRunGraphReconciliation::Cancel { activation_id } if *activation_id == terminal));
    store
        .stage_run_graph_edit(&revised, authority, 27)
        .expect("revised dispositions");
    store
        .publish_retained_leaf_run_graph_edit(&run.run_id, &revised.mutation_id, authority, 28)
        .expect("recover retirement");
    let statuses: Vec<String> = fixture_rows(
        &store.connection,
        &select("workflow_fan_out_members")
            .columns(&["status"])
            .where_eq("run_id", run.run_id.as_str())
            .sort("member_index", switchy_database::query::SortDirection::Asc),
    )
    .iter()
    .map(|row| fixture_text(row, "status"))
    .collect();
    assert_eq!(statuses, [expected, "cancelled", "cancelled"]);
    assert_eq!(
        store.run_graph_revision(&run.run_id).expect("revision"),
        Some(2)
    );
}

fn assert_retired_fan_out_after_reopen(path: &std::path::Path, run_id: &str) {
    let mut store = WorkflowStore::open_in_state_dir(path).expect("reopen");
    assert!(
        store
            .pending_activations_for_run(run_id, 10)
            .expect("pending")
            .iter()
            .all(|activation| activation.node_id == "replacement")
    );
    assert!(
        store
            .settle_pending_control_nodes(run_id, 10, 23)
            .expect("settle")
            .activated
            .is_empty()
    );
    let count: u64 = fixture_scalar_unsigned(
        &store.connection,
        &select("workflow_fan_out_members")
            .project_as(switchy_database::query::count_all(), "count")
            .where_eq("run_id", run_id)
            .where_eq("status", "cancelled"),
        "count",
    )
    .expect("members");
    assert_eq!(count, 3);
}

#[test]
fn durable_fan_out_materialization_is_bounded_ordered_and_restart_safe() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("fan-out", 1, &fan_out_definition())
        .expect("definition");
    let mut run = new_run();
    run.definition_id = "fan-out".to_string();
    run.input = Some(serde_json::json!([3, 1, 2]));
    store.create_run(&run).expect("run");

    let summary = store
        .settle_pending_control_nodes("run-1", 10, 20)
        .expect("materialize");
    assert_eq!(summary.settled.len(), 1);
    assert_eq!(summary.activated.len(), 2);
    assert_eq!(
        summary
            .activated
            .iter()
            .map(|member| member.dependency_generation)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
    drop(store);

    let mut reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    assert!(
        reopened
            .settle_pending_control_nodes("run-1", 10, 21)
            .expect("idempotent")
            .settled
            .is_empty(),
        "a materialized controller must not duplicate members after restart"
    );
    let pending = reopened
        .pending_activations_for_run("run-1", 10)
        .expect("pending members");
    assert_eq!(pending.len(), 2);
    assert_eq!(pending[0].input, Some(serde_json::json!(3)));
    assert_eq!(pending[1].input, Some(serde_json::json!(1)));
    assert_ne!(pending[0].activation_id, pending[1].activation_id);
}

#[test]
#[allow(clippy::too_many_lines)]
fn automatic_retry_policy_from_owner_observation_is_atomic_and_restart_safe() {
    struct RetryableFailureObserver;
    impl AttemptStatusObserver for RetryableFailureObserver {
        fn observe(
            &self,
            _request: &AttemptReconciliationRequest,
        ) -> Result<AttemptObservation, WorkflowStoreError> {
            Ok(AttemptObservation::RetryableFailure {
                message: "retryable".to_string(),
            })
        }
    }

    let temp = tempfile::tempdir().expect("temp");
    let mut definition = definition("retry-policy");
    let node = definition.nodes.get_mut("review").expect("node");
    node.kind = bcode_workflow::NodeKind::PluginBlock;
    node.configuration = serde_json::to_value(bcode_workflow::WorkflowBlockDefinition {
        block_id: "example.retryable".to_string(),
        block_version: 1,
        plugin_id: "bcode.example".to_string(),
        operation: "retryable".to_string(),
        input: node.input.clone(),
        output: node.output.clone(),
        effect: bcode_workflow::WorkflowBlockEffect::ReadOnly,
        resources: Vec::new(),
        authorization: bcode_workflow::WorkflowBlockAuthorization {
            capability: bcode_workflow::WorkflowToolCapability::ReadOnly,
            explicit_grant_required: false,
        },
        timeout_ms: 1_000,
        cancellation_supported: true,
        reconciliation: bcode_workflow::WorkflowBlockReconciliation::ReceiptStatus,
        automatic_retry: Some(bcode_workflow::WorkflowAutomaticRetryPolicy {
            version: bcode_workflow::WORKFLOW_AUTOMATIC_RETRY_POLICY_VERSION,
            max_attempts: 3,
            eligible_failures: vec![
                bcode_workflow::AutomaticRetryFailureKind::OwnerReportedRetryable,
            ],
            initial_backoff_ms: 100,
            backoff_multiplier: 2,
            maximum_backoff_ms: 1_000,
        }),
        preparation_required: false,
    })
    .expect("block");
    definition.validate().expect("definition");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("example", 1, &definition)
        .expect("definition");
    store.create_run(&new_run()).expect("run");
    let prepared = store
        .prepare_pending_activation(
            "run-1",
            "review",
            &activation_id(),
            DispatchSideEffect::ReadOnly,
            serde_json::json!({"operation": "retryable"}),
            12,
        )
        .expect("prepare")
        .expect("pending activation");
    let identity = prepared.dispatch_identity;
    store
        .persist_dispatch_receipt(&DispatchReceipt {
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: activation_id(),
            attempt: 1,
            dispatch_identity: identity.clone(),
            receipt: serde_json::json!({"receipt": "retryable"}),
            admitted_at_ms: 13,
        })
        .expect("receipt");

    // Source configuration is not the admitted execution's retry authority.
    definition
        .nodes
        .get_mut("review")
        .expect("node")
        .configuration["automatic_retry"] = serde_json::Value::Null;
    let source_json = serde_json::to_string(&definition).expect("source");
    block_on(
        update("workflow_definitions")
            .value("definition_json", source_json.as_str())
            .value("checksum_sha256", sha256_hex(source_json.as_bytes()))
            .execute(&store.connection),
    )
    .expect("source fixture");
    drop(store);
    let mut store =
        WorkflowStore::open_in_state_dir(temp.path()).expect("reopen before observation");
    let summary = store
        .reconcile_receipt_backed_attempts(&RetryableFailureObserver, 10, 20)
        .expect("reconcile");
    assert_eq!(summary.failed, [identity]);
    assert_eq!(summary.retries_scheduled.len(), 1);
    assert_eq!(summary.retries_scheduled[0].next_attempt, 2);
    assert_eq!(summary.retries_scheduled[0].next_attempt_at_ms, 120);
    drop(store);

    let mut reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    assert_eq!(
        reopened
            .automatic_retry_schedule("run-1", "review", &activation_id())
            .expect("schedule"),
        Some(summary.retries_scheduled[0].clone())
    );
    assert_eq!(
        reopened.resumable_pending_run_ids(10).expect("resumable"),
        vec!["run-1".to_string()]
    );
    assert!(
        reopened
            .consume_next_due_automatic_retry_for_run("run-1", 119)
            .expect("not due")
            .is_none()
    );
    let retry = reopened
        .consume_next_due_automatic_retry_for_run("run-1", 120)
        .expect("consume")
        .expect("retry");
    assert_eq!(retry.next_attempt, 2);
    assert!(
        reopened
            .consume_next_due_automatic_retry_for_run("run-1", 120)
            .expect("duplicate")
            .is_none()
    );
}

#[test]
fn due_automatic_retry_consumption_is_atomic_and_cancellation_safe() {
    struct FailedObserver;
    impl AttemptStatusObserver for FailedObserver {
        fn observe(
            &self,
            _request: &AttemptReconciliationRequest,
        ) -> Result<AttemptObservation, WorkflowStoreError> {
            Ok(AttemptObservation::Failed {
                message: "retryable".to_string(),
            })
        }
    }

    let (_temp, mut store) = initialized_store();
    let prepared = store
        .prepare_pending_activation(
            "run-1",
            "review",
            &activation_id(),
            DispatchSideEffect::ReadOnly,
            serde_json::json!({"operation": "review"}),
            10,
        )
        .expect("prepare")
        .expect("attempt");
    store
        .persist_dispatch_receipt(&DispatchReceipt {
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: activation_id(),
            attempt: 1,
            dispatch_identity: prepared.dispatch_identity,
            receipt: serde_json::json!({"receipt": "failed"}),
            admitted_at_ms: 11,
        })
        .expect("receipt");
    store
        .reconcile_receipt_backed_attempts(&FailedObserver, 10, 20)
        .expect("failed");
    let request = AttemptReconciliationRequest {
        run_id: "run-1".to_string(),
        node_id: "review".to_string(),
        activation_id: activation_id(),
        attempt: 1,
        dispatch_identity: dispatch_identity("run-1", "review", &activation_id(), 1),
        side_effect: DispatchSideEffect::ReadOnly,
        receipt: serde_json::json!({"receipt": "failed"}),
    };
    store
        .schedule_automatic_retry_from_observation(
            &request,
            &AttemptObservation::RetryableFailure {
                message: "retryable".to_string(),
            },
            bcode_workflow::WorkflowBlockEffect::ReadOnly,
            bcode_workflow::WorkflowBlockReconciliation::ReceiptStatus,
            3,
            100,
            20,
        )
        .expect("schedule from owner outcome")
        .expect("retry schedule");
    assert!(
        store
            .schedule_automatic_retry_from_observation(
                &request,
                &AttemptObservation::Unknown,
                bcode_workflow::WorkflowBlockEffect::Mutating,
                bcode_workflow::WorkflowBlockReconciliation::RepairRequired,
                3,
                100,
                20,
            )
            .is_err(),
        "ambiguous owner outcomes must never schedule mutation retries"
    );
    assert!(
        store
            .consume_next_due_automatic_retry_for_run("run-1", 119)
            .expect("nothing due")
            .is_none()
    );
    let consumed = store
        .consume_next_due_automatic_retry_for_run("run-1", 120)
        .expect("due retry")
        .expect("schedule");
    assert_eq!(consumed.next_attempt, 2);
    assert!(
        store
            .automatic_retry_schedule("run-1", "review", &activation_id())
            .expect("schedule")
            .is_none()
    );
    assert!(
        store
            .consume_next_due_automatic_retry_for_run("run-1", 121)
            .expect("consumed schedule")
            .is_none(),
        "consumed schedules must not duplicate requeue"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn automatic_retry_backoff_schedule_survives_restart_without_requeueing() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = initialized_store_at(temp.path());
    let eligibility = bcode_workflow::AutomaticRetryEligibility {
        version: bcode_workflow::WORKFLOW_AUTOMATIC_RETRY_POLICY_VERSION,
        effect: bcode_workflow::WorkflowBlockEffect::ReadOnly,
        reconciliation: bcode_workflow::WorkflowBlockReconciliation::ReceiptStatus,
        failure: bcode_workflow::AutomaticRetryFailureKind::OwnerReportedRetryable,
        attempts_completed: 1,
        max_attempts: 3,
        retry_cap: 2,
    };
    let schedule = store
        .schedule_automatic_retry(
            eligibility,
            "run-1",
            "review",
            &activation_id(),
            bcode_workflow::AutomaticRetryFailureKind::OwnerReportedRetryable,
            5_000,
            100,
        )
        .expect("schedule");
    assert_eq!(schedule.next_attempt, 2);
    assert_eq!(schedule.next_attempt_at_ms, 5_100);
    drop(store);

    let mut reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    assert_eq!(
        reopened
            .automatic_retry_schedule("run-1", "review", &activation_id())
            .expect("load"),
        Some(schedule.clone())
    );
    assert_eq!(
        reopened
            .attempt_history("run-1", None, 10)
            .expect("attempts")
            .len(),
        0
    );
    assert_eq!(
        reopened
            .schedule_automatic_retry(
                eligibility,
                "run-1",
                "review",
                &activation_id(),
                bcode_workflow::AutomaticRetryFailureKind::OwnerReportedRetryable,
                5_000,
                100,
            )
            .expect("idempotent"),
        schedule
    );
    assert!(
        reopened
            .schedule_automatic_retry(
                eligibility,
                "run-1",
                "review",
                &activation_id(),
                bcode_workflow::AutomaticRetryFailureKind::OwnerReportedRetryable,
                6_000,
                100,
            )
            .is_err()
    );
    assert_eq!(
        reopened
            .event_history("run-1", None, 20)
            .expect("events")
            .iter()
            .filter(|event| event.event_type == "automatic_retry_scheduled")
            .count(),
        1
    );

    let ambiguous = bcode_workflow::AutomaticRetryEligibility {
        effect: bcode_workflow::WorkflowBlockEffect::Mutating,
        reconciliation: bcode_workflow::WorkflowBlockReconciliation::RepairRequired,
        failure: bcode_workflow::AutomaticRetryFailureKind::AmbiguousMutation,
        ..eligibility
    };
    assert!(
        reopened
            .schedule_automatic_retry(
                ambiguous,
                "run-1",
                "review",
                &activation_id(),
                bcode_workflow::AutomaticRetryFailureKind::AmbiguousMutation,
                5_000,
                100,
            )
            .is_err(),
        "ambiguous mutations must never acquire an automatic retry schedule"
    );
    assert_eq!(
        reopened
            .automatic_retry_schedule("run-1", "review", &activation_id())
            .expect("existing schedule"),
        Some(schedule),
        "rejected conflicting facts must not alter the durable schedule"
    );
}

#[test]
fn retry_boundaries_survive_restart_without_automatic_scheduling() {
    struct FailedObserver;
    impl AttemptStatusObserver for FailedObserver {
        fn observe(
            &self,
            _request: &AttemptReconciliationRequest,
        ) -> Result<AttemptObservation, WorkflowStoreError> {
            Ok(AttemptObservation::Failed {
                message: "terminal failure".to_string(),
            })
        }
    }

    let temp = tempfile::tempdir().expect("temp");
    let mut store = initialized_store_at(temp.path());
    let prepared = store
        .prepare_pending_activation(
            "run-1",
            "review",
            &activation_id(),
            DispatchSideEffect::ReadOnly,
            serde_json::json!({"operation": "review"}),
            12,
        )
        .expect("prepare")
        .expect("attempt");
    drop(store);

    let mut reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("prepared restart");
    let prepared_rows = reopened
        .attempt_history("run-1", None, 10)
        .expect("history");
    assert_eq!(prepared_rows[0].status, "prepared");
    assert_eq!(prepared_rows[0].attempt, 1);
    reopened
        .persist_dispatch_receipt(&DispatchReceipt {
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: activation_id(),
            attempt: prepared.attempt,
            dispatch_identity: prepared.dispatch_identity,
            receipt: serde_json::json!({"turn_id": "turn-1"}),
            admitted_at_ms: 13,
        })
        .expect("receipt");
    drop(reopened);

    let mut reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("receipt restart");
    assert_eq!(
        reopened
            .attempt_history("run-1", None, 10)
            .expect("history")[0]
            .status,
        "admitted"
    );
    reopened
        .reconcile_receipt_backed_attempts(&FailedObserver, 10, 20)
        .expect("terminal failure");
    drop(reopened);

    let reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("terminal restart");
    assert!(
        reopened
            .pending_activations(10)
            .expect("pending")
            .is_empty()
    );
    assert_eq!(
        reopened
            .attempt_history("run-1", None, 10)
            .expect("history")[0]
            .status,
        "failed"
    );
    let retry = reopened
        .retry_failed_node("run-1", "review", &activation_id(), 1, 21)
        .expect("operator retry");
    assert_eq!(retry.next_attempt, 2);
    drop(reopened);

    let reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("retry restart");
    assert_eq!(
        reopened
            .pending_activations(10)
            .expect("pending")
            .pop()
            .expect("activation")
            .activation_id,
        activation_id()
    );
    assert_eq!(
        reopened
            .attempt_history("run-1", None, 10)
            .expect("history")
            .len(),
        1
    );
}

#[test]
fn cancelled_receipt_backed_mutation_is_terminal_not_repair_required() {
    struct Observer;
    impl AttemptStatusObserver for Observer {
        fn observe(
            &self,
            _request: &AttemptReconciliationRequest,
        ) -> Result<AttemptObservation, WorkflowStoreError> {
            Ok(AttemptObservation::Cancelled)
        }
    }

    let (temp, mut store) = initialized_store();
    let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::Mutating);
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
    store.request_cancellation("run-1", 19).expect("cancel");
    let summary = store
        .reconcile_receipt_backed_attempts(&Observer, 10, 20)
        .expect("reconcile");
    assert_eq!(summary.cancelled, [identity]);
    assert!(summary.repair_required.is_empty());
    assert_eq!(
        store
            .run_summary("run-1")
            .expect("summary")
            .expect("run")
            .status,
        RunStatus::Cancelled
    );
}

#[test]
fn failure_history_returns_newest_bounded_canonical_diagnostics_in_order() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    let definition = definition("example");
    store
        .persist_definition("example", 1, &definition)
        .expect("definition");
    store.create_run(&new_run()).expect("run");
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    append_event_in(
        transaction.as_ref(),
        "run-1",
        "attempt_failed",
        &serde_json::json!({"message": "first"}).to_string(),
        2,
    )
    .expect("first failure");
    append_event_in(transaction.as_ref(), "run-1", "run_paused", "{}", 3).expect("non-failure");
    append_event_in(
        transaction.as_ref(),
        "run-1",
        "run_failed",
        &serde_json::json!({"reason": "second"}).to_string(),
        4,
    )
    .expect("second failure");
    database::commit(transaction).expect("commit");

    let events = store.failure_history("run-1", 2).expect("failure history");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event_type, "attempt_failed");
    assert_eq!(events[1].event_type, "run_failed");
}

#[test]
fn run_creation_rejects_malformed_definition_with_matching_checksum() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("example", 1, &definition("example"))
        .expect("definition");
    let mut invalid = definition("example");
    invalid.entries.clear();
    let payload = serde_json::to_string(&invalid).expect("payload");
    block_on(
        update("workflow_definitions")
            .value("definition_json", payload.as_str())
            .value("checksum_sha256", sha256_hex(payload.as_bytes()))
            .execute(&store.connection),
    )
    .expect("malformed fixture");
    let before = fixture_changes(&store.connection);
    assert!(
        store
            .create_run(&new_run())
            .expect_err("invalid definition")
            .to_string()
            .contains("invalid stored workflow definition")
    );
    assert_eq!(fixture_changes(&store.connection), before);
    assert!(store.run_summary("run-1").expect("summary").is_none());
    assert_eq!(store.run_graph_revision("run-1").expect("graph"), None);
}

#[test]
fn run_creation_rejects_damaged_definitions_before_writes() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("example", 1, &definition("example"))
        .expect("definition");
    let mut changed = definition("example");
    changed.name = "modified".to_string();
    let payload = serde_json::to_string(&changed).expect("payload");
    for (payload, expected) in [
        (payload, "checksum mismatch"),
        (
            "x".repeat(MAX_INLINE_JSON_BYTES + 1),
            "invalid or oversized",
        ),
    ] {
        block_on(
            update("workflow_definitions")
                .value("definition_json", payload.as_str())
                .execute(&store.connection),
        )
        .expect("damage");
        let before = fixture_changes(&store.connection);
        assert!(
            store
                .create_run(&new_run())
                .expect_err("damaged definition")
                .to_string()
                .contains(expected)
        );
        assert_eq!(fixture_changes(&store.connection), before);
        assert!(store.run_summary("run-1").expect("summary").is_none());
        assert_eq!(store.run_graph_revision("run-1").expect("graph"), None);
    }
}

fn historical_startup_fixture() -> tempfile::TempDir {
    let (temp, store) = initialized_store();
    for table in [
        "workflow_run_graph_edges",
        "workflow_run_graph_nodes",
        "workflow_run_graphs",
    ] {
        block_on(switchy_database::schema::drop_table(table).execute(&store.connection))
            .expect("schema 15 fixture table");
    }
    fixture_historical_activations(&store.connection, true).expect("schema 15 fixture activation");
    block_on(
        update("workflow_store_contract")
            .value("schema_version", 15)
            .execute(&store.connection),
    )
    .expect("schema 15 fixture");
    drop(store);
    temp
}

fn remove_retirement_fixture(store: &WorkflowStore) {
    for (index, table) in [
        (
            "workflow_run_graph_nodes_retirement",
            "workflow_run_graph_nodes",
        ),
        (
            "workflow_run_graph_edges_retirement",
            "workflow_run_graph_edges",
        ),
        ("workflow_run_graph_nodes_live", "workflow_run_graph_nodes"),
        ("workflow_run_graph_edges_live", "workflow_run_graph_edges"),
    ] {
        block_on(switchy_database::schema::drop_index(index, table).execute(&store.connection))
            .expect("pre-retirement index");
    }
    for table in ["workflow_run_graph_nodes", "workflow_run_graph_edges"] {
        block_on(
            switchy_database::schema::alter_table(table)
                .drop_column("retired_at_revision".into())
                .execute(&store.connection),
        )
        .expect("pre-retirement schema");
    }
}

#[test]
fn missing_recovery_barrier_is_not_recreated_on_open() {
    let (temp, store) = initialized_store();
    block_on(
        store
            .connection
            .drop_trigger("recovery_blocks_handoff", false),
    )
    .expect("damage");
    drop(store);
    assert!(WorkflowStore::open_in_state_dir(temp.path()).is_err());
    assert!(WorkflowStore::initialize_in_state_dir(temp.path(), 994).is_err());
    let connection = fixture_open(&workflow_database_path(temp.path()), true);
    assert!(
        fixture_rows(
            &connection,
            &select("sqlite_schema")
                .columns(&["name"])
                .where_eq("name", "recovery_blocks_handoff")
        )
        .is_empty()
    );
}

/// Initializes and upgrades workflow storage for tests before a process-level interruption.

#[test]
fn cancellation_process_loss_child() {
    let Ok(root) = std::env::var("BCODE_TEST_CANCELLATION_CRASH_ROOT") else {
        return;
    };
    let mut store = initialized_store_at(Path::new(&root));
    prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::Mutating);
    block_on(
        update("workflow_runs")
            .value("target_artifact_id", "old-artifact")
            .value("coordinator_daemon_instance_id", "old-daemon")
            .value("coordinator_generation", 1)
            .value("coordinator_fencing_token", "old-fence")
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("fixture authority");
    let authority = store
        .execution_authority("run-1")
        .expect("authority")
        .expect("owner");
    store
        .request_cancellation_owned("run-1", 20, &authority)
        .expect("durable intent");
    // Intentionally skip destructors, receipt observation, and cursor advancement.
    std::process::exit(73);
}

#[tokio::test]
async fn cancellation_process_loss_preserves_intent_and_fences_takeover() {
    struct Cancelled;
    impl AsyncAttemptStatusObserver for Cancelled {
        fn observe_async<'a>(
            &'a self,
            _: &'a AttemptReconciliationRequest,
        ) -> Pin<Box<dyn Future<Output = Result<AttemptObservation, WorkflowStoreError>> + Send + 'a>>
        {
            Box::pin(async { Ok(AttemptObservation::Cancelled) })
        }
    }
    let temp = tempfile::tempdir().expect("temp");
    let status = std::process::Command::new(std::env::current_exe().expect("executable"))
        .args(["--exact", "tests::cancellation_process_loss_child"])
        .env("BCODE_TEST_CANCELLATION_CRASH_ROOT", temp.path())
        .status()
        .expect("child");
    assert_eq!(status.code(), Some(73));
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
    let old = store
        .execution_authority("run-1")
        .expect("authority")
        .expect("owner");
    assert_eq!(
        store
            .run_summary("run-1")
            .expect("summary")
            .expect("run")
            .cancellation_requested_at_ms,
        Some(20)
    );
    let next = WorkflowExecutionAuthority {
        target_artifact_id: "new-artifact".into(),
        daemon_instance_id: "new-daemon".into(),
        generation: old.generation + 1,
        fencing_token: "new-fence".into(),
    };
    store
        .take_recovery_authority(
            "run-1",
            &old,
            &next,
            &EndedOwnerEvidence {
                ended_daemon_instance_id: old.daemon_instance_id.clone(),
                ended_target_artifact_id: old.target_artifact_id.clone(),
                liveness: EndedOwnerLiveness::ObservedEnded,
                artifact_image_available: false,
            },
            21,
        )
        .expect("takeover after observed child exit");
    assert!(store.request_cancellation_owned("run-1", 22, &old).is_err());
    assert!(recovery::require_execution(&store.connection, "run-1").is_err());
    let result = store
        .advance_receipt_recovery(&Cancelled, "run-1", &next, 1, 23)
        .await
        .expect("settle");
    assert_eq!(result.cancelled.len(), 1);
    assert_eq!(
        store
            .run_summary("run-1")
            .expect("summary")
            .expect("run")
            .status,
        RunStatus::Cancelled
    );
    drop(store);
    let store = WorkflowStore::open_in_state_dir(temp.path()).expect("second restart");
    assert_eq!(
        store
            .attempt_history("run-1", None, 10)
            .expect("attempts")
            .len(),
        1
    );
    assert_eq!(
        store
            .run_summary("run-1")
            .expect("summary")
            .expect("run")
            .status,
        RunStatus::Cancelled
    );
}

/// Exercise an independent writer while the parent retains its store handle.
#[test]
fn coexistence_writer_child() {
    let Ok(root) = std::env::var("BCODE_TEST_WORKFLOW_COEXISTENCE_ROOT") else {
        return;
    };
    let mut store = WorkflowStore::open_in_state_dir(Path::new(&root)).expect("child store");
    let mut run = new_run();
    run.run_id = "child-run".into();
    store.create_run(&run).expect("independent admission");
    assert!(store.pause_run("child-run", 20).expect("child pause"));
    assert_eq!(
        store
            .run_summary("run-1")
            .expect("parent summary")
            .expect("parent run")
            .status,
        RunStatus::Running
    );
}

#[test]
fn independent_process_writer_preserves_open_store_usability() {
    let (temp, mut store) = initialized_store();
    let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", "tests::coexistence_writer_child"])
        .env("BCODE_TEST_WORKFLOW_COEXISTENCE_ROOT", temp.path())
        .status()
        .expect("child");
    assert!(status.success());
    assert!(store.pause_run("run-1", 21).expect("parent still writable"));
    assert_eq!(
        store
            .run_summary("child-run")
            .expect("child summary")
            .expect("child run")
            .status,
        RunStatus::Paused
    );
    let reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    for id in ["run-1", "child-run"] {
        assert_eq!(
            reopened
                .run_summary(id)
                .expect("summary")
                .expect("run")
                .status,
            RunStatus::Paused
        );
    }
}

#[test]
fn auxiliary_open_refuses_retained_exclusive_ownership_without_mutation() {
    let (temp, store) = initialized_store();
    let path = store.path().to_path_buf();
    drop(store);
    let owner = open_ownership_file(path.parent().expect("root")).expect("owner");
    owner.lock().expect("exclusive owner");
    let before = std::fs::read(&path).expect("database before");
    assert!(matches!(
        WorkflowStore::open_at_path(&path),
        Err(WorkflowStoreError::UpgradeOwnershipUnavailable)
    ));
    assert_eq!(std::fs::read(&path).expect("database after"), before);
    drop(owner);
    let store = WorkflowStore::open_in_state_dir(temp.path()).expect("open after release");
    assert_eq!(
        store
            .run_summary("run-1")
            .expect("summary")
            .expect("run")
            .status,
        RunStatus::Running
    );
}

#[test]
fn independent_run_child() {
    let Ok(root) = std::env::var("BCODE_TEST_INDEPENDENT_RUN_ROOT") else {
        return;
    };
    let mut store = WorkflowStore::initialize_in_state_dir(Path::new(&root), 915)
        .expect("open while another process owns a different run");
    let mut run = new_run();
    run.run_id = "independent-child".into();
    store.create_run(&run).expect("admit independent run");
    assert!(store.pause_run(&run.run_id, 22).expect("advance child run"));
}

#[test]
fn independent_runs_admit_across_live_processes() {
    let (temp, mut parent) = initialized_store();
    let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args(["--exact", "tests::independent_run_child"])
        .env("BCODE_TEST_INDEPENDENT_RUN_ROOT", temp.path())
        .status()
        .expect("child process");
    assert!(status.success());
    let mut parent_run = new_run();
    parent_run.run_id = "independent-parent".into();
    parent.create_run(&parent_run).expect("admit parent run");
    assert!(
        parent
            .pause_run(&parent_run.run_id, 23)
            .expect("advance parent run")
    );
    for id in ["independent-parent", "independent-child"] {
        assert_eq!(
            parent
                .run_summary(id)
                .expect("summary")
                .expect("run")
                .status,
            RunStatus::Paused
        );
    }
}

#[test]
fn additive_revision_child() {
    let Ok(root) = std::env::var("BCODE_TEST_ADDITIVE_REVISION_ROOT") else {
        return;
    };
    let mut store = WorkflowStore::open_in_state_dir(Path::new(&root)).expect("child open");
    let tx =
        database::begin(&store.connection, TransactionMode::Immediate).expect("write reservation");
    verify_store_schema(tx.as_ref()).expect("recheck contract under reservation");
    block_on(
        switchy_database::schema::create_table("additive_extension")
            .column(switchy_database::schema::Column {
                name: "id".into(),
                nullable: true,
                auto_increment: false,
                data_type: switchy_database::schema::DataType::BigInt,
                default: None,
            })
            .primary_key("id")
            .execute(tx.as_ref()),
    )
    .expect("additive DDL");
    block_on(
        update("workflow_store_contract")
            .value("schema_version", WORKFLOW_STORE_SCHEMA_VERSION + 1)
            .execute(tx.as_ref()),
    )
    .expect("revision");
    if std::env::var_os("BCODE_TEST_ADDITIVE_REVISION_INTERRUPT").is_some() {
        // Exit without Rust destructors to exercise SQLite's interruption recovery.
        std::process::exit(73);
    }
    database::commit(tx).expect("atomic revision");
    let mut run = new_run();
    run.run_id = "additive-child".into();
    store
        .create_run(&run)
        .expect("child admission after upgrade");
    assert!(store.pause_run(&run.run_id, 20).expect("child progress"));
}

#[test]
fn additive_revision_process_preserves_retained_writer_after_commit_or_interruption() {
    for interrupted in [false, true] {
        let (temp, mut store) = initialized_store();
        let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"));
        child
            .args(["--exact", "tests::additive_revision_child"])
            .env("BCODE_TEST_ADDITIVE_REVISION_ROOT", temp.path());
        if interrupted {
            child.env("BCODE_TEST_ADDITIVE_REVISION_INTERRUPT", "1");
        } else {
            child.env_remove("BCODE_TEST_ADDITIVE_REVISION_INTERRUPT");
        }
        let status = child.status().expect("child process");
        assert_eq!(status.code(), Some(if interrupted { 73 } else { 0 }));
        assert!(store.pause_run("run-1", 21).expect("retained writer"));
        let expected = WORKFLOW_STORE_SCHEMA_VERSION + u32::from(!interrupted);
        assert_eq!(detected_store_schema(&store.connection), Some(expected));
        let extension_exists = !fixture_rows(
            &store.connection,
            &select("sqlite_master")
                .columns(&["name"])
                .where_eq("type", "table")
                .where_eq("name", "additive_extension"),
        )
        .is_empty();
        assert_eq!(extension_exists, !interrupted);
        let reopened = WorkflowStore::initialize_in_state_dir(temp.path(), 914)
            .expect("reopen while retained");
        assert_eq!(
            reopened
                .run_summary("run-1")
                .expect("summary")
                .expect("run")
                .status,
            RunStatus::Paused
        );
        assert_eq!(
            reopened
                .run_summary("additive-child")
                .expect("child summary")
                .is_some(),
            !interrupted
        );
    }
}

#[test]
fn initialization_retry_classification_is_limited_to_contention() {
    assert!(WorkflowStoreError::UpgradeOwnershipUnavailable.is_transient_initialization_failure());
    for error in [
        switchy_database::DatabaseError::Busy,
        switchy_database::DatabaseError::Locked,
    ] {
        assert!(
            WorkflowStoreError::StructuredDatabase(error).is_transient_initialization_failure()
        );
    }
    let temp = tempfile::tempdir().expect("corrupt initialization fixture");
    let path = temp.path().join("corrupt.db");
    std::fs::write(&path, b"not a SQLite database").expect("corrupt fixture");
    let corrupt = match database::open(
        &path,
        switchy_database_connection::SqliteAccess::ReadWrite,
        std::time::Duration::ZERO,
        true,
    ) {
        Ok(db) => block_on(select("sqlite_schema").execute(&db)).expect_err("corrupt database"),
        Err(WorkflowStoreError::StructuredDatabase(error)) => error,
        Err(error) => panic!("unexpected corruption error: {error}"),
    };
    assert_eq!(
        corrupt.classification(),
        Some(switchy_database::DatabaseErrorKind::Corrupt)
    );
    for error in [
        WorkflowStoreError::UnsupportedStore {
            actual: Some(999),
            expected: WORKFLOW_STORE_SCHEMA_VERSION,
        },
        WorkflowStoreError::InvalidData("damaged state".into()),
        WorkflowStoreError::StructuredDatabase(corrupt),
        WorkflowStoreError::StructuredDatabase(switchy_database::DatabaseError::NoRow),
    ] {
        assert!(!error.is_transient_initialization_failure());
    }
}

fn verify_graph_migration_preserves_invalid_definition(invalid: &WorkflowDefinition) {
    verify_graph_migration_rejects_definition(invalid, false);
}

fn verify_graph_migration_rejects_definition(invalid: &WorkflowDefinition, stale_checksum: bool) {
    verify_graph_migration_rejects_definition_checksum(invalid, stale_checksum, None);
}

fn verify_graph_migration_rejects_definition_checksum(
    invalid: &WorkflowDefinition,
    stale_checksum: bool,
    checksum_override: Option<&str>,
) {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("example", 1, &definition("example"))
        .expect("definition");
    store.create_run(&new_run()).expect("run");
    store
        .persist_definition("valid-earlier", 1, &definition("valid-earlier"))
        .expect("valid definition");
    let mut earlier = new_run();
    earlier.run_id = "run-0".to_string();
    earlier.definition_id = "valid-earlier".to_string();
    store.create_run(&earlier).expect("earlier valid run");
    let path = store.path().to_path_buf();
    drop(store);
    let connection = fixture_open(&path, true);
    let payload = serde_json::to_string(invalid).expect("invalid payload");
    block_on(
        update("workflow_definitions")
            .value("definition_json", payload.as_str())
            .where_eq("definition_id", "example")
            .execute(&connection),
    )
    .expect("future definition");
    if let Some(checksum) = checksum_override {
        block_on(
            update("workflow_definitions")
                .value("checksum_sha256", checksum)
                .where_eq("definition_id", "example")
                .execute(&connection),
        )
        .expect("checksum damage fixture");
    } else if !stale_checksum {
        block_on(
            update("workflow_definitions")
                .value("checksum_sha256", sha256_hex(payload.as_bytes()))
                .where_eq("definition_id", "example")
                .execute(&connection),
        )
        .expect("fixture checksum");
    }
    for table in [
        "workflow_run_graph_edges",
        "workflow_run_graph_nodes",
        "workflow_run_graphs",
    ] {
        block_on(switchy_database::schema::drop_table(table).execute(&connection))
            .expect("historical fixture table");
    }
    fixture_historical_activations(&connection, true).expect("historical fixture column");
    block_on(
        update("workflow_store_contract")
            .value("schema_version", 15)
            .where_eq("contract_id", 1)
            .execute(&connection),
    )
    .expect("historical fixture");
    let expected_checksum = fixture_scalar_text(
        &connection,
        &select("workflow_definitions")
            .columns(&["checksum_sha256"])
            .where_eq("definition_id", "example")
            .where_eq("version", 1),
        "checksum_sha256",
    )
    .expect("fixture checksum before migration");
    drop(connection);
    assert!(
        WorkflowStore::migrate_to_current_in_state_dir(temp.path(), 43)
            .expect_err("unsupported definition")
            .to_string()
            .contains(if payload.len() > MAX_INLINE_JSON_BYTES {
                "workflow definition payload is invalid or oversized during graph migration"
            } else if stale_checksum {
                "workflow definition checksum mismatch during graph migration"
            } else {
                "invalid workflow definition during graph migration"
            })
    );
    assert_rejected_graph_migration_preserves_definitions(&path, &payload, &expected_checksum);
}

fn assert_rejected_graph_migration_preserves_definitions(
    path: &std::path::Path,
    payload: &str,
    expected_checksum: &str,
) {
    let connection = fixture_open(path, true);
    assert_eq!(detected_store_schema(&connection), Some(15));
    assert!(
        block_on(
            select("workflow_run_graphs")
                .columns(&["run_id"])
                .execute(&connection)
        )
        .is_err()
    );
    assert_eq!(
        fixture_scalar_text(
            &connection,
            &select("workflow_definitions")
                .columns(&["definition_json"])
                .where_eq("definition_id", "example"),
            "definition_json"
        )
        .expect("preserved payload"),
        payload
    );
    let preserved_checksum = fixture_scalar_text(
        &connection,
        &select("workflow_definitions")
            .columns(&["checksum_sha256"])
            .where_eq("definition_id", "example")
            .where_eq("version", 1),
        "checksum_sha256",
    )
    .expect("preserved checksum");
    assert_eq!(preserved_checksum, expected_checksum);
    let preserved_valid = fixture_scalar_text(
        &connection,
        &select("workflow_definitions")
            .columns(&["definition_json"])
            .where_eq("definition_id", "valid-earlier")
            .where_eq("version", 1),
        "definition_json",
    )
    .expect("preserved earlier definition");
    assert_eq!(
        serde_json::from_str::<WorkflowDefinition>(&preserved_valid).expect("valid payload"),
        definition("valid-earlier")
    );
    assert_rollback_run_bindings(&connection);
}

fn assert_rollback_run_bindings(connection: &dyn Database) {
    let rows = fixture_rows(
        connection,
        &select("workflow_runs")
            .columns(&["run_id", "definition_id", "definition_version"])
            .sort("run_id", switchy_database::query::SortDirection::Asc),
    );
    let bindings: Vec<_> = rows
        .iter()
        .map(|row| {
            (
                fixture_text(row, "run_id"),
                fixture_text(row, "definition_id"),
                fixture_unsigned(row, "definition_version"),
            )
        })
        .collect();
    assert_eq!(
        bindings,
        vec![
            ("run-0".to_string(), "valid-earlier".to_string(), 1),
            ("run-1".to_string(), "example".to_string(), 1),
        ]
    );
}

#[test]
fn published_call_requires_exact_available_target_interface() {
    let (_temp, mut store) = initialized_store();
    block_on(async {
        update("workflow_runs")
            .value("target_artifact_id", "artifact-a")
            .value("coordinator_daemon_instance_id", "daemon-a")
            .value("coordinator_generation", 1)
            .value("coordinator_fencing_token", "token-a")
            .where_eq("run_id", "run-1")
            .execute(&store.connection)
            .await?;
        Ok::<(), switchy_database::DatabaseError>(())
    })
    .expect("owner");
    let authority = store
        .execution_authority("run-1")
        .expect("authority")
        .expect("owner");
    let target = definition("child");
    let mut node = workflow_call_definition(bcode_workflow::WorkflowDefinitionIdentity {
        kind: "child".to_owned(),
        definition_id: "child".to_owned(),
        definition_version: 1,
    })
    .nodes
    .remove("call")
    .expect("call");
    node.input = target.input.clone();
    node.output = target.output.clone();
    let request = bcode_workflow::WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: "run-1".to_owned(),
        expected_revision: 1,
        mutation_id: "add-call".to_owned(),
        edits: vec![bcode_workflow::WorkflowRunGraphEdit::AddNode {
            node,
            entry: true,
            exit: true,
        }],
        reconciliation: vec![bcode_workflow::WorkflowRunGraphReconciliation::Retain {
            activation_id: activation_id(),
        }],
    };
    store
        .stage_run_graph_edit(&request, &authority, 20)
        .expect("stage");
    assert!(
        store
            .publish_retained_leaf_run_graph_edit("run-1", "add-call", &authority, 21)
            .is_err()
    );
    store
        .persist_definition("child", 1, &target)
        .expect("target");
    assert_eq!(
        store
            .publish_retained_leaf_run_graph_edit("run-1", "add-call", &authority, 22)
            .expect("publish"),
        2
    );
    assert_eq!(
        store
            .current_run_graph_node("run-1", "call")
            .expect("node")
            .expect("call")
            .node
            .kind,
        bcode_workflow::NodeKind::WorkflowCall
    );
}

fn verify_edge_removal_publication(retain_active: bool) {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("sequential", 1, &sequential_definition())
        .expect("definition");
    let mut run = new_run();
    run.definition_id = "sequential".to_string();
    store.create_run(&run).expect("run");
    fixture_assign_edge_removal_owner(&store.connection);
    let authority = store
        .execution_authority("run-1")
        .expect("authority")
        .expect("owner");
    let edges = store.run_graph_edges("run-1", None, 100).expect("edges");
    assert!(!edges.is_empty());
    assert_incident_edges(&store, &edges);
    let request = bcode_workflow::WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: "run-1".to_string(),
        mutation_id: "disconnect".to_string(),
        expected_revision: 1,
        edits: edges
            .iter()
            .map(|edge| bcode_workflow::WorkflowRunGraphEdit::RemoveEdge {
                edge_id: edge.edge_id,
            })
            .collect(),
        reconciliation: if retain_active {
            vec![bcode_workflow::WorkflowRunGraphReconciliation::Retain {
                activation_id: activation_identity("run-1", "first", 0),
            }]
        } else {
            vec![]
        },
    };
    store
        .stage_run_graph_edit(&request, &authority, 20)
        .expect("stage");
    assert!(
        store
            .publish_quiescent_run_graph_edit("run-1", "disconnect", &authority, 21)
            .is_err()
    );
    assert_eq!(
        store.run_graph_revision("run-1").expect("revision"),
        Some(1)
    );
    if !retain_active {
        fixture_quiesce_edge_removal(&store.connection);
    }
    assert_eq!(
        store
            .publish_retained_leaf_run_graph_edit("run-1", "disconnect", &authority, 22)
            .expect("publish"),
        2
    );
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    let live = fixture_scalar_unsigned(
        &store.connection,
        &select("workflow_run_graph_edges")
            .project_as(switchy_database::query::count_all(), "count")
            .where_eq("run_id", "run-1")
            .where_eq("retired_at_revision", DatabaseValue::Null),
        "count",
    )
    .expect("live edges");
    assert_eq!(live, 0);
    assert!(
        store
            .current_run_graph_incident_edges("run-1", 2, "first", true, None, 1)
            .expect("retired outgoing")
            .is_empty()
    );
    assert!(
        store
            .current_run_graph_incident_edges("run-1", 1, "first", true, None, 1)
            .is_err()
    );
    for edge in edges {
        assert_eq!(
            store
                .run_graph_edge_revision("run-1", edge.edge_id, 1)
                .expect("historical edge"),
            Some(edge)
        );
    }
    assert_eq!(
        store
            .publish_quiescent_run_graph_edit("run-1", "disconnect", &authority, 23)
            .expect("duplicate"),
        2
    );
    settle_disconnected_source(&mut store, &authority, retain_active);
}

fn fixture_assign_edge_removal_owner(db: &dyn Database) {
    block_on(async {
        update("workflow_runs")
            .value("target_artifact_id", "artifact-a")
            .value("coordinator_daemon_instance_id", "daemon-a")
            .value("coordinator_generation", 1)
            .value("coordinator_fencing_token", "token-a")
            .where_eq("run_id", "run-1")
            .execute(db)
            .await?;
        Ok::<(), switchy_database::DatabaseError>(())
    })
    .expect("owner");
}

fn fixture_quiesce_edge_removal(db: &dyn Database) {
    block_on(
        update("workflow_activations")
            .value("status", "cancelled")
            .where_eq("run_id", "run-1")
            .execute(db),
    )
    .expect("quiesce");
}

fn assert_incident_edges(store: &WorkflowStore, edges: &[super::RunGraphEdge]) {
    for edge in edges {
        for (node, outgoing) in [(&edge.edge.from, true), (&edge.edge.to, false)] {
            assert_eq!(
                store
                    .current_run_graph_incident_edges("run-1", 1, node, outgoing, None, 100)
                    .expect("incident edges"),
                edges
            );
        }
    }
}

fn assert_revised_settlement_rollback(
    store: &mut WorkflowStore,
    output: &ValidatedOutput,
    authority: &WorkflowExecutionAuthority,
) {
    struct Fault;
    impl WorkflowOutputFault for Fault {
        fn after_boundary(
            &self,
            boundary: WorkflowOutputBoundary,
            _: &ValidatedOutput,
        ) -> Result<(), WorkflowStoreError> {
            if boundary == WorkflowOutputBoundary::SuccessorsMaterialized {
                return Err(WorkflowStoreError::InvalidData(
                    "injected revised settlement failure".to_string(),
                ));
            }
            Ok(())
        }
    }
    let error = store
        .persist_validated_output_at_graph_revision_with_fault(output, 2, authority, &Fault)
        .expect_err("rollback");
    assert!(
        error
            .to_string()
            .contains("injected revised settlement failure")
    );
    assert!(
        store
            .output_summaries(&output.run_id, 10)
            .expect("outputs")
            .is_empty()
    );
    let status = fixture_scalar_text(
        &store.connection,
        &select("workflow_activations")
            .columns(&["status"])
            .where_eq("run_id", output.run_id.as_str())
            .where_eq("activation_id", output.activation_id.as_str()),
        "status",
    )
    .expect("activation status");
    assert_eq!(status, "pending");
    assert_eq!(
        store
            .run_graph_revision(&output.run_id)
            .expect("published revision"),
        Some(2)
    );
}

fn settle_disconnected_source(
    store: &mut WorkflowStore,
    authority: &WorkflowExecutionAuthority,
    retain_active: bool,
) {
    let activation = NewActivation {
        run_id: "run-1".to_string(),
        node_id: "first".to_string(),
        activation_id: activation_identity("run-1", "first", 1),
        dependency_generation: 1,
        input: Some(serde_json::json!(1)),
        created_at_ms: 24,
    };
    if !retain_active {
        store
            .create_activation_at_graph_revision(&activation, 2, authority)
            .expect("revised admission");
    }
    let output = ValidatedOutput {
        output_id: "disconnected-output".to_string(),
        run_id: activation.run_id,
        node_id: activation.node_id,
        activation_id: if retain_active {
            activation_identity("run-1", "first", 0)
        } else {
            activation.activation_id
        },
        schema_id: "u32".to_string(),
        schema_version: 1,
        value: serde_json::json!(1),
        artifact_reference: None,
        created_at_ms: 25,
    };
    assert_revised_settlement_rollback(store, &output, authority);
    let outcome = store
        .persist_validated_output_at_graph_revision(&output, 2, authority)
        .expect("retired edge does not block settlement");
    assert!(outcome.activated.is_empty());
}

fn assert_published_leaf_admission(store: &WorkflowStore) {
    let activation_id = activation_identity("run-1", "replacement-leaf", 0);
    assert_eq!(
        store
            .activation_admitted_graph_revision("run-1", "replacement-leaf", &activation_id)
            .expect("admission"),
        Some(2)
    );
    let status: String = fixture_scalar_text(
        &store.connection,
        &select("workflow_activations")
            .columns(&["status"])
            .where_eq("run_id", "run-1")
            .where_eq("activation_id", activation_id),
        "status",
    )
    .expect("published entry");
    assert_eq!(status, "pending");
}

#[test]
fn candidate_requires_complete_active_source_bindings() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    let graph = sequential_definition();
    store
        .persist_definition("sequential", 1, &graph)
        .expect("definition");
    let mut run = new_run();
    run.definition_id = "sequential".into();
    store.create_run(&run).expect("run");
    let mut node = graph.nodes["first"].clone();
    node.id = "added".into();
    let request = bcode_workflow::WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: run.run_id.clone(),
        mutation_id: "missing-bindings".into(),
        expected_revision: 1,
        edits: vec![bcode_workflow::WorkflowRunGraphEdit::AddNode {
            node,
            entry: true,
            exit: true,
        }],
        reconciliation: vec![bcode_workflow::WorkflowRunGraphReconciliation::Retain {
            activation_id: activation_identity(&run.run_id, "first", 0),
        }],
    };
    block_on(async {
        update("workflow_runs")
            .value("target_artifact_id", "artifact-a")
            .value("coordinator_daemon_instance_id", "daemon-a")
            .value("coordinator_generation", 1)
            .value("coordinator_fencing_token", "token-a")
            .execute(&store.connection)
            .await?;
        Ok::<(), switchy_database::DatabaseError>(())
    })
    .expect("owner");
    let authority = store
        .execution_authority(&run.run_id)
        .expect("authority")
        .expect("owner");
    store
        .stage_run_graph_edit(&request, &authority, 20)
        .expect("stage");
    let error = store
        .publish_retained_leaf_run_graph_edit(&run.run_id, &request.mutation_id, &authority, 21)
        .expect_err("incomplete reconciliation");
    assert!(error.to_string().contains("every candidate outgoing edge"));
    assert_eq!(
        run_graph::graph_revision(&store.connection, &run.run_id).expect("revision"),
        Some(1)
    );
}

fn verify_persisted_candidate_validation(
    store: &WorkflowStore,
    authority: &WorkflowExecutionAuthority,
) {
    store
        .validate_staged_run_graph_edit("run-1", "valid-replacement", authority)
        .expect("duplicate validation");
    let records: Vec<(String, u64)> = fixture_rows(
        &store.connection,
        &select("workflow_graph_edit_validations").columns(&["mutation_id", "expected_revision"]),
    )
    .iter()
    .map(|row| {
        (
            fixture_text(row, "mutation_id"),
            fixture_unsigned(row, "expected_revision"),
        )
    })
    .collect();
    assert_eq!(records, vec![("valid-replacement".to_string(), 1)]);
    let delta_rows = fixture_rows(
        &store.connection,
        &select("workflow_graph_edit_nodes").columns(&[
            "mutation_id",
            "node_id",
            "node_json",
            "is_entry",
        ]),
    );
    let row = delta_rows.first().expect("validated replacement delta");
    let delta = (
        fixture_text(row, "mutation_id"),
        fixture_text(row, "node_id"),
        !matches!(row.get("node_json"), Some(DatabaseValue::Null)),
        fixture_unsigned(row, "is_entry") != 0,
    );
    assert_eq!(
        delta,
        (
            "valid-replacement".to_string(),
            "review".to_string(),
            true,
            true
        )
    );
    assert_eq!(
        store
            .run_graph_revision("run-1")
            .expect("unpublished graph"),
        Some(1)
    );
}

fn verify_running_candidate_requires_disposition(
    store: &mut WorkflowStore,
    replacement: &bcode_workflow::WorkflowRunGraphEditBatch,
    authority: &WorkflowExecutionAuthority,
) {
    let mut candidate = replacement.clone();
    candidate.mutation_id = "missing-disposition".to_string();
    candidate.reconciliation.clear();
    store
        .stage_run_graph_edit(&candidate, authority, 24)
        .expect("stage");
    block_on(
        update("workflow_activations")
            .value("status", "running")
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("running target");
    let before = fixture_changes(&store.connection);
    let error = store
        .validate_staged_run_graph_edit("run-1", "missing-disposition", authority)
        .expect_err("explicit disposition required");
    assert!(
        error
            .to_string()
            .contains("requires explicit graph reconciliation")
    );
    assert_eq!(before, fixture_changes(&store.connection));
}

fn verify_candidate_read_and_damage(
    store: &mut WorkflowStore,
    request: &bcode_workflow::WorkflowRunGraphEditBatch,
    authority: &WorkflowExecutionAuthority,
) {
    assert_eq!(
        store
            .staged_run_graph_edit("run-1", "mutation-1", authority)
            .expect("read candidate"),
        Some(request.clone())
    );
    assert!(
        store
            .validate_staged_run_graph_edit("run-1", "mutation-1", authority)
            .is_err()
    );
    let node = store
        .current_run_graph_node("run-1", "review")
        .expect("current node")
        .expect("node");
    let mut replacement = request.clone();
    replacement.mutation_id = "valid-replacement".to_string();
    replacement.edits = vec![bcode_workflow::WorkflowRunGraphEdit::ReplaceNode {
        node: node.node,
        entry: node.entry,
        exit: node.exit,
    }];
    verify_running_candidate_requires_disposition(store, &replacement, authority);
    replacement.reconciliation = vec![bcode_workflow::WorkflowRunGraphReconciliation::Retain {
        activation_id: activation_id(),
    }];
    store
        .stage_run_graph_edit(&replacement, authority, 24)
        .expect("stage replacement");
    assert_eq!(
        store
            .validate_staged_run_graph_edit("run-1", "valid-replacement", authority)
            .expect("validate"),
        RunGraphCandidateValidation::Validated
    );
    verify_persisted_candidate_validation(store, authority);
    block_on(
        update("workflow_activations")
            .value("status", "completed")
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("settled reconciliation target");
    let before = fixture_changes(&store.connection);
    assert!(
        store
            .validate_staged_run_graph_edit("run-1", "valid-replacement", authority)
            .expect_err("terminal reconciliation target")
            .to_string()
            .contains("unambiguous active activation")
    );
    assert_eq!(before, fixture_changes(&store.connection));
    block_on(
        update("workflow_activations")
            .value("status", "pending")
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("restore target");
    for (status, cancellation) in [("completed", None), ("running", Some(25))] {
        block_on(
            update("workflow_runs")
                .value("status", status)
                .value(
                    "cancellation_requested_at_ms",
                    cancellation.map_or(DatabaseValue::Null, DatabaseValue::Int32),
                )
                .where_eq("run_id", "run-1")
                .execute(&store.connection),
        )
        .expect("close graph edit admission");
        let changes = fixture_changes(&store.connection);
        assert!(
            store
                .validate_staged_run_graph_edit("run-1", "valid-replacement", authority)
                .is_err()
        );
        assert_eq!(fixture_changes(&store.connection), changes);
    }
    block_on(
        update("workflow_runs")
            .value("status", "running")
            .value("cancellation_requested_at_ms", DatabaseValue::Null)
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("restore run admission");
    block_on(
        update("workflow_graph_edit_candidates")
            .value("expected_revision", 2)
            .execute(&store.connection),
    )
    .expect("damage candidate revision");
    assert!(
        store
            .staged_run_graph_edit("run-1", "mutation-1", authority)
            .is_err()
    );
    assert!(store.stage_run_graph_edit(request, authority, 24).is_err());
    assert!(block_on(store.connection.sqlite_is_autocommit()).expect("owned snapshot released"));
}

#[test]
fn revision_aware_admission_fences_and_preserves_bindings() {
    let (temp, mut store) = initialized_store();
    let original = store
        .activation_graph_node("run-1", "review", &activation_id())
        .expect("original binding")
        .expect("original node");
    block_on(
        update("workflow_runs")
            .value("target_artifact_id", "artifact-a")
            .value("coordinator_daemon_instance_id", "daemon-a")
            .value("coordinator_generation", 1)
            .value("coordinator_fencing_token", "token-a")
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("fixture owner");
    fixture_copy_graph_nodes(&store.connection, "run-1", 2);
    block_on(
        update("workflow_run_graphs")
            .value("revision", 2)
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("admitted revision fixture");
    let authority = store
        .execution_authority("run-1")
        .expect("authority")
        .expect("owner");
    let activation = NewActivation {
        run_id: "run-1".to_string(),
        node_id: "review".to_string(),
        activation_id: activation_identity("run-1", "review", 1),
        dependency_generation: 1,
        input: Some(serde_json::json!(42)),
        created_at_ms: 20,
    };
    let mut stale = authority.clone();
    stale.generation += 1;
    assert!(
        store
            .create_activation_at_graph_revision(&activation, 2, &stale)
            .is_err()
    );
    assert!(
        store
            .create_activation_at_graph_revision(&activation, 1, &authority)
            .is_err()
    );
    assert!(
        store
            .activation_graph_node("run-1", "review", &activation.activation_id)
            .expect("no failed admission")
            .is_none()
    );
    let before = fixture_changes(&store.connection);
    assert!(store.create_activation(&activation).is_err());
    assert_eq!(before, fixture_changes(&store.connection));
    store
        .create_activation_at_graph_revision(&activation, 2, &authority)
        .expect("admit revised activation");
    assert!(
        store
            .create_activation_at_graph_revision(&activation, 2, &authority)
            .is_err()
    );
    drop(store);
    let store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    assert_eq!(
        store
            .activation_graph_node("run-1", "review", &activation_id())
            .expect("old binding"),
        Some(original.clone())
    );
    let mut revised = original;
    revised.revision = 2;
    assert_eq!(
        store
            .activation_graph_node("run-1", "review", &activation.activation_id)
            .expect("new binding"),
        Some(revised)
    );
}

#[test]
fn current_node_revision_does_not_rebind_existing_activation_after_reopen() {
    let (temp, store) = initialized_store();
    let original = store
        .activation_graph_node("run-1", "review", &activation_id())
        .expect("binding")
        .expect("node");
    let mut revised = original.clone();
    revised.revision = 2;
    revised.entry = false;
    revised.exit = false;
    revised.node.configuration = serde_json::json!({"revision_fixture": "replacement"});
    assert_ne!(revised.node.configuration, original.node.configuration);
    let transaction = database::begin(&store.connection, TransactionMode::Deferred)
        .expect("revision transaction");
    block_on(
        insert("workflow_run_graph_nodes")
            .value("run_id", "run-1")
            .value("node_id", "review")
            .value("revision", 2)
            .value(
                "node_json",
                serde_json::to_string(&revised.node).expect("revised executable"),
            )
            .value("is_entry", 0)
            .value("is_exit", 0)
            .execute(transaction.as_ref()),
    )
    .expect("revised node fixture");
    block_on(
        update("workflow_run_graphs")
            .value("revision", 2)
            .where_eq("run_id", "run-1")
            .execute(transaction.as_ref()),
    )
    .expect("revised graph fixture");
    database::commit(transaction).expect("commit revision fixture");
    assert_eq!(
        store
            .current_run_graph_node("run-1", "review")
            .expect("current"),
        Some(revised.clone())
    );
    assert_eq!(
        store
            .activation_graph_node("run-1", "review", &activation_id())
            .expect("unchanged binding"),
        Some(original.clone())
    );
    drop(store);
    let reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    let before = fixture_changes(&reopened.connection);
    assert_eq!(
        reopened
            .current_run_graph_node("run-1", "review")
            .expect("reopened current"),
        Some(revised)
    );
    assert_eq!(
        reopened
            .activation_graph_node("run-1", "review", &activation_id())
            .expect("reopened binding"),
        Some(original.clone())
    );
    assert_eq!(fixture_changes(&reopened.connection), before);
    assert!(block_on(reopened.connection.sqlite_is_autocommit()).expect("owned snapshot released"));
    block_on(
        update("workflow_run_graph_nodes")
            .value("node_json", "invalid-json")
            .where_eq("run_id", "run-1")
            .where_eq("node_id", "review")
            .where_eq("revision", 2)
            .execute(&reopened.connection),
    )
    .expect("damaged latest revision fixture");
    let before = fixture_changes(&reopened.connection);
    assert!(reopened.current_run_graph_node("run-1", "review").is_err());
    assert_eq!(
        reopened
            .activation_graph_node("run-1", "review", &activation_id())
            .expect("intact historical binding"),
        Some(original)
    );
    assert_eq!(fixture_changes(&reopened.connection), before);
    assert!(block_on(reopened.connection.sqlite_is_autocommit()).expect("owned snapshot released"));
}

#[test]
fn activation_binding_never_substitutes_valid_latest_for_damaged_bound_revision() {
    let (_temp, store) = initialized_store();
    let original = store
        .activation_graph_node("run-1", "review", &activation_id())
        .expect("binding")
        .expect("node");
    {
        fixture_copy_graph_nodes_scoped(&store.connection, "run-1", Some("review"), 2);
        block_on(async {
            update("workflow_run_graphs")
                .value("revision", 2)
                .where_eq("run_id", "run-1")
                .execute(&store.connection)
                .await?;
            update("workflow_run_graph_nodes")
                .value("node_json", "invalid-json")
                .where_eq("run_id", "run-1")
                .where_eq("node_id", "review")
                .where_eq("revision", 1)
                .execute(&store.connection)
                .await?;
            Ok::<(), switchy_database::DatabaseError>(())
        })
    }
    .expect("damaged bound revision fixture");
    let before = fixture_changes(&store.connection);
    let mut latest = original;
    latest.revision = 2;
    assert_eq!(
        store
            .current_run_graph_node("run-1", "review")
            .expect("valid latest"),
        Some(latest)
    );
    assert!(
        store
            .activation_graph_node("run-1", "review", &activation_id())
            .is_err()
    );
    assert!(
        run_graph::bound_activation_node(&store.connection, "run-1", "review", &activation_id())
            .is_err()
    );
    assert_eq!(fixture_changes(&store.connection), before);
    assert!(block_on(store.connection.sqlite_is_autocommit()).expect("owned snapshot released"));
}

#[test]
fn activation_binding_does_not_fall_forward_when_bound_revision_is_missing() {
    let (_temp, store) = initialized_store();
    let mut latest = store
        .activation_graph_node("run-1", "review", &activation_id())
        .expect("binding")
        .expect("node");
    {
        fixture_copy_graph_nodes_scoped(&store.connection, "run-1", Some("review"), 2);
        block_on(async {
            update("workflow_run_graphs")
                .value("revision", 2)
                .where_eq("run_id", "run-1")
                .execute(&store.connection)
                .await?;
            delete("workflow_run_graph_nodes")
                .where_eq("run_id", "run-1")
                .where_eq("node_id", "review")
                .where_eq("revision", 1)
                .execute(&store.connection)
                .await?;
            Ok::<(), switchy_database::DatabaseError>(())
        })
    }
    .expect("missing bound revision fixture");
    latest.revision = 2;
    let before = fixture_changes(&store.connection);
    assert_eq!(
        store
            .current_run_graph_node("run-1", "review")
            .expect("latest node"),
        Some(latest)
    );
    assert!(
        store
            .run_graph_node_revision("run-1", "review", 1)
            .expect("missing revision")
            .is_none()
    );
    assert!(
        store
            .activation_graph_node("run-1", "review", &activation_id())
            .is_err()
    );
    assert!(
        run_graph::bound_activation_node(&store.connection, "run-1", "review", &activation_id())
            .is_err()
    );
    assert_eq!(fixture_changes(&store.connection), before);
    assert!(block_on(store.connection.sqlite_is_autocommit()).expect("owned snapshot released"));
}

#[test]
fn activation_binding_snapshot_survives_another_connections_commit() {
    let (temp, store) = initialized_store();
    block_on(
        store
            .connection
            .sqlite_journal_mode(switchy_database::SqliteJournalMode::Wal),
    )
    .expect("WAL");
    let original = store
        .activation_graph_node("run-1", "review", &activation_id())
        .expect("original binding")
        .expect("node");
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("reader snapshot");
    let db: &dyn Database = transaction.as_ref();
    assert_eq!(
        WorkflowStore::activation_graph_node_in(db, "run-1", "review", &activation_id())
            .expect("pin snapshot"),
        Some(original.clone())
    );
    let writer = fixture_open(&workflow_database_path(temp.path()), true);
    let write = database::begin(&writer, TransactionMode::Deferred).expect("writer transaction");
    fixture_copy_graph_nodes_scoped(write.as_ref(), "run-1", Some("review"), 2);
    block_on(
        update("workflow_run_graphs")
            .value("revision", 2)
            .where_eq("run_id", "run-1")
            .execute(write.as_ref()),
    )
    .expect("revision fixture");
    block_on(
        update("workflow_activations")
            .value("node_revision", 2)
            .where_eq("run_id", "run-1")
            .where_eq("node_id", "review")
            .execute(write.as_ref()),
    )
    .expect("binding fixture");
    database::commit(write).expect("concurrent commit");
    assert_eq!(
        WorkflowStore::activation_graph_node_in(db, "run-1", "review", &activation_id())
            .expect("retained snapshot"),
        Some(original.clone())
    );
    assert_eq!(
        run_graph::initial_activation_node(db, "run-1", "review", &activation_id())
            .expect("initial binding in retained snapshot"),
        Some(original.node.clone())
    );
    assert!(!block_on(db.sqlite_is_autocommit()).expect("reader lease active"));
    database::commit(transaction).expect("release reader snapshot");
    assert!(
        run_graph::initial_activation_node(&store.connection, "run-1", "review", &activation_id())
            .is_err()
    );
    assert!(block_on(store.connection.sqlite_is_autocommit()).expect("reader released"));
    let mut revised = original;
    revised.revision = 2;
    assert_eq!(
        store
            .activation_graph_node("run-1", "review", &activation_id())
            .expect("fresh snapshot"),
        Some(revised)
    );
    assert!(block_on(store.connection.sqlite_is_autocommit()).expect("owned read released"));
}

#[test]
fn direct_bound_node_read_releases_owned_snapshot_after_failure() {
    let (_temp, store) = initialized_store();
    let expected = store
        .activation_graph_node("run-1", "review", &activation_id())
        .expect("binding")
        .expect("node");
    assert_eq!(
        run_graph::bound_activation_node(&store.connection, "run-1", "review", &activation_id())
            .expect("direct binding"),
        Some(expected.node)
    );
    assert!(block_on(store.connection.sqlite_is_autocommit()).expect("owner snapshot released"));
    let before = fixture_changes(&store.connection);
    assert!(
        run_graph::bound_activation_node(
            &store.connection,
            "run-1",
            "review",
            "missing-activation"
        )
        .is_err()
    );
    assert!(block_on(store.connection.sqlite_is_autocommit()).expect("owner snapshot released"));
    assert_eq!(fixture_changes(&store.connection), before);
}

#[test]
fn direct_bound_node_read_releases_snapshot_on_invalid_revision() {
    let (_temp, store) = initialized_store();
    block_on(
        update("workflow_activations")
            .value("node_revision", 2)
            .where_eq("run_id", "run-1")
            .where_eq("node_id", "review")
            .execute(&store.connection),
    )
    .expect("invalid binding fixture");
    let before = fixture_changes(&store.connection);
    assert!(
        run_graph::bound_activation_node(&store.connection, "run-1", "review", &activation_id(),)
            .is_err()
    );
    assert!(block_on(store.connection.sqlite_is_autocommit()).expect("owner snapshot released"));
    assert_eq!(fixture_changes(&store.connection), before);
    let revision = fixture_scalar_unsigned(
        &store.connection,
        &select("workflow_activations")
            .columns(&["node_revision"])
            .where_eq("activation_id", activation_id()),
        "node_revision",
    )
    .expect("fixture preserved");
    assert_eq!(revision, 2);
    let transaction = database::begin(&store.connection, TransactionMode::Deferred)
        .expect("new caller transaction");
    assert!(
        run_graph::bound_activation_node(transaction.as_ref(), "run-1", "review", &activation_id(),)
            .is_err()
    );
    assert!(!block_on(transaction.sqlite_is_autocommit()).expect("caller lease active"));
    block_on(transaction.rollback()).expect("caller retains ownership");
    assert_eq!(fixture_changes(&store.connection), before);
}

#[test]
fn initial_activation_lookup_validates_activation_before_missing_node() {
    let (_temp, store) = initialized_store();
    let before = fixture_changes(&store.connection);
    for (run_id, node_id) in [
        ("run-1", "review"),
        ("run-1", "missing"),
        ("missing", "review"),
    ] {
        assert!(
            run_graph::initial_activation_node(&store.connection, run_id, node_id, "").is_err()
        );
        assert!(
            block_on(store.connection.sqlite_is_autocommit()).expect("owner snapshot released")
        );
    }
    assert!(
        run_graph::initial_activation_node(
            &store.connection,
            "run-1",
            "missing",
            "valid-activation"
        )
        .expect("missing node")
        .is_none()
    );
    assert_eq!(fixture_changes(&store.connection), before);
}

#[test]
fn direct_bound_node_read_rejects_missing_committed_executable() {
    let (temp, store) = initialized_store();
    block_on(async {
        update("workflow_run_graphs")
            .value("revision", 2)
            .where_eq("run_id", "run-1")
            .execute(&store.connection)
            .await?;
        update("workflow_activations")
            .value("node_revision", 2)
            .where_eq("run_id", "run-1")
            .where_eq("node_id", "review")
            .execute(&store.connection)
            .await?;
        Ok::<(), switchy_database::DatabaseError>(())
    })
    .expect("missing executable fixture");
    let before = fixture_changes(&store.connection);
    assert!(
        store
            .run_graph_node_revision("run-1", "review", 2)
            .expect("missing exact revision")
            .is_none()
    );
    assert!(
        run_graph::bound_activation_node(&store.connection, "run-1", "review", &activation_id())
            .is_err()
    );
    assert!(
        store
            .activation_graph_node("run-1", "review", &activation_id())
            .is_err()
    );
    assert!(block_on(store.connection.sqlite_is_autocommit()).expect("owned snapshot released"));
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("caller snapshot");
    assert!(
        run_graph::bound_activation_node(transaction.as_ref(), "run-1", "review", &activation_id())
            .is_err()
    );
    assert!(!block_on(transaction.sqlite_is_autocommit()).expect("caller lease active"));
    block_on(transaction.rollback()).expect("caller retains ownership");
    assert_eq!(fixture_changes(&store.connection), before);
    assert!(
        store
            .run_graph_node_revision("run-1", "review", 2)
            .expect("still missing")
            .is_none()
    );
    drop(store);
    let reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen damaged binding");
    let before = fixture_changes(&reopened.connection);
    assert!(
        reopened
            .activation_graph_node("run-1", "review", &activation_id())
            .is_err()
    );
    assert!(
        reopened
            .run_graph_node_revision("run-1", "review", 2)
            .expect("preserved missing executable")
            .is_none()
    );
    let binding = fixture_scalar_unsigned(
        &reopened.connection,
        &select("workflow_activations")
            .columns(&["node_revision"])
            .where_eq("activation_id", activation_id()),
        "node_revision",
    )
    .expect("preserved binding");
    assert_eq!(binding, 2);
    assert_eq!(fixture_changes(&reopened.connection), before);
    assert!(block_on(reopened.connection.sqlite_is_autocommit()).expect("owned snapshot released"));
}

#[test]
fn activation_binding_read_preserves_callers_transaction() {
    let (_temp, store) = initialized_store();
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    let db: &dyn Database = transaction.as_ref();
    let original = WorkflowStore::activation_graph_node_in(db, "run-1", "review", &activation_id())
        .expect("binding in transaction")
        .expect("node");
    block_on(
        delete("workflow_run_graph_nodes")
            .where_eq("run_id", "run-1")
            .where_eq("node_id", "review")
            .execute(db),
    )
    .expect("transactional removal");
    assert!(
        WorkflowStore::activation_graph_node_in(db, "run-1", "review", &activation_id()).is_err()
    );
    assert!(!block_on(db.sqlite_is_autocommit()).expect("caller transaction active"));
    block_on(transaction.rollback()).expect("rollback remains caller-owned");
    assert_eq!(
        store
            .activation_graph_node("run-1", "review", &activation_id())
            .expect("restored executable"),
        Some(original)
    );
    assert!(
        block_on(store.connection.sqlite_is_autocommit()).expect("caller transaction released")
    );
}

#[test]
fn activation_binding_reads_reject_missing_executable_without_writes() {
    let (_temp, store) = initialized_store();
    block_on(
        delete("workflow_run_graph_nodes")
            .where_eq("run_id", "run-1")
            .where_eq("node_id", "review")
            .execute(&store.connection),
    )
    .expect("missing executable fixture");
    let before = fixture_changes(&store.connection);
    assert!(
        store
            .activation_graph_node("run-1", "review", &activation_id())
            .is_err()
    );
    assert!(
        run_graph::bound_activation_node(&store.connection, "run-1", "review", &activation_id())
            .is_err()
    );
    assert_eq!(fixture_changes(&store.connection), before);
}

#[test]
fn exact_node_revision_preserves_historical_executable_after_reopen() {
    let (temp, store) = initialized_store();
    let original = store
        .run_graph_node_revision("run-1", "review", 1)
        .expect("lookup")
        .expect("initial node");
    // Model two committed revisions; production graph mutation remains unavailable.
    block_on(
        insert("workflow_run_graph_nodes")
            .value("run_id", "run-1")
            .value("node_id", "review")
            .value("revision", 2)
            .value(
                "node_json",
                serde_json::to_string(&original.node).expect("node"),
            )
            .value("is_entry", 0)
            .value("is_exit", 0)
            .execute(&store.connection),
    )
    .expect("revision node fixture");
    block_on(
        update("workflow_run_graphs")
            .value("revision", 2)
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("revision fixture");
    drop(store);
    let store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    let changes = fixture_changes(&store.connection);
    assert_eq!(
        store
            .run_graph_node_revision("run-1", "review", 1)
            .expect("historical node"),
        Some(original)
    );
    let current = store
        .run_graph_node_revision("run-1", "review", 2)
        .expect("exact node")
        .expect("second revision");
    assert_eq!(current.revision, 2);
    let page = store
        .current_run_graph_nodes("run-1", 2, None, 1)
        .expect("current page");
    assert_eq!(page, vec![current.clone()]);
    assert!(
        store
            .current_run_graph_nodes("run-1", 2, Some("review"), 1)
            .expect("end")
            .is_empty()
    );
    assert!(store.current_run_graph_nodes("run-1", 1, None, 1).is_err());
    assert!(store.current_run_graph_nodes("run-1", 2, None, 0).is_err());
    assert!(!current.entry && !current.exit);
    assert_eq!(
        store
            .current_run_graph_node("run-1", "review")
            .expect("current node"),
        Some(current)
    );
    assert!(
        store
            .current_run_graph_node("run-1", "absent")
            .expect("absent")
            .is_none()
    );
    assert!(store.run_graph_node_revision("run-1", "review", 3).is_err());
    assert!(store.run_graph_node_revision("run-1", "review", 0).is_err());
    assert!(
        store
            .run_graph_node_revision("run-1", "absent", 1)
            .expect("absent node")
            .is_none()
    );
    assert!(store.run_graph_node("run-1", "review").is_err());
    assert_eq!(fixture_changes(&store.connection), changes);
}

#[test]
fn edge_endpoint_retirement_is_evaluated_at_edge_revision() {
    let (temp, store) = initialized_store();
    let edge = bcode_workflow::EdgeDefinition {
        from: "review".to_string(),
        to: "review".to_string(),
        kind: bcode_workflow::EdgeKind::default(),
        transform: None,
    };
    for revision in [1, 2, 3] {
        block_on(
            insert("workflow_run_graph_edges")
                .value("run_id", "run-1")
                .value("edge_id", 0)
                .value("revision", revision)
                .value("source_node_id", "review")
                .value("target_node_id", "review")
                .value("edge_json", serde_json::to_string(&edge).expect("json"))
                .execute(&store.connection),
        )
        .expect("edge fixture");
    }
    block_on(
        update("workflow_run_graph_nodes")
            .value("retired_at_revision", 2)
            .execute(&store.connection),
    )
    .expect("retirement fixture");
    let rows = fixture_rows(
        &store.connection,
        &select("workflow_run_graph_nodes").where_eq("revision", 1),
    );
    for row in rows {
        block_on(
            insert("workflow_run_graph_nodes")
                .value("run_id", fixture_text(&row, "run_id"))
                .value("node_id", fixture_text(&row, "node_id"))
                .value("revision", 3)
                .value("node_json", fixture_text(&row, "node_json"))
                .value("is_entry", fixture_unsigned(&row, "is_entry"))
                .value("is_exit", fixture_unsigned(&row, "is_exit"))
                .execute(&store.connection),
        )
        .expect("reintroduction fixture");
    }
    block_on(
        update("workflow_run_graphs")
            .value("revision", 3)
            .execute(&store.connection),
    )
    .expect("retirement and reintroduction");
    drop(store);
    let store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    let before = fixture_changes(&store.connection);
    assert!(
        store
            .run_graph_edge_revision("run-1", 0, 1)
            .expect("before retirement")
            .is_some()
    );
    let error = store
        .run_graph_edge_revision("run-1", 0, 2)
        .expect_err("retired endpoint");
    assert!(error.to_string().contains("endpoint was retired"));
    assert!(
        store
            .run_graph_edge_revision("run-1", 0, 3)
            .expect("reintroduced endpoint")
            .is_some()
    );
    assert_eq!(fixture_changes(&store.connection), before);
}

#[test]
fn exact_edge_read_rejects_invalid_endpoint_storage_without_writes() {
    let (_temp, store) = initialized_store();
    let oversized = "x".repeat(MAX_ID_BYTES + 1);
    for (source, target) in [
        ("", "review"),
        ("review", ""),
        (" ", "review"),
        ("review", " "),
        (oversized.as_str(), "review"),
        ("review", oversized.as_str()),
    ] {
        let tx = database::begin(&store.connection, TransactionMode::Deferred)
            .expect("edge fixture transaction");
        block_on(
            delete("workflow_run_graph_edges")
                .where_eq("run_id", "run-1")
                .where_eq("edge_id", 0)
                .where_eq("revision", 1)
                .execute(tx.as_ref()),
        )
        .expect("replace edge fixture");
        block_on(
            insert("workflow_run_graph_edges")
                .value("run_id", "run-1")
                .value("edge_id", 0)
                .value("revision", 1)
                .value("source_node_id", source)
                .value("target_node_id", target)
                .value("edge_json", "not-json")
                .execute(tx.as_ref()),
        )
        .expect("damaged edge fixture");
        database::commit(tx).expect("edge fixture commit");
        let before = fixture_changes(&store.connection);
        let error = store
            .run_graph_edge_revision("run-1", 0, 1)
            .expect_err("invalid endpoint");
        assert!(error.to_string().contains("edge endpoint"), "{error}");
        assert_eq!(fixture_changes(&store.connection), before);
    }
}

#[test]
fn successor_input_rejects_ambiguous_edge_binding() {
    let (_temp, store) = initialized_store();
    let edge = bcode_workflow::EdgeDefinition {
        from: "review".to_owned(),
        to: "review".to_owned(),
        kind: bcode_workflow::EdgeKind::default(),
        transform: None,
    };
    for id in [0, 1] {
        block_on(
            insert("workflow_run_graph_edges")
                .value("run_id", "run-1")
                .value("edge_id", id)
                .value("revision", 1)
                .value("source_node_id", "review")
                .value("target_node_id", "review")
                .value("edge_json", serde_json::to_string(&edge).expect("json"))
                .execute(&store.connection),
        )
        .expect("edge");
    }
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    let db: &dyn Database = transaction.as_ref();
    let before = fixture_changes(db);
    let error = run_graph::initial_edge_between(db, "run-1", "review", "review")
        .expect_err("ambiguous binding");
    assert!(error.to_string().contains("unambiguous edge binding"));
    assert_eq!(fixture_changes(db), before);
}

#[test]
fn exact_edge_revision_preserves_history_after_reopen() {
    let (temp, store) = initialized_store();
    let edge = bcode_workflow::EdgeDefinition {
        from: "review".to_string(),
        to: "review".to_string(),
        kind: bcode_workflow::EdgeKind::default(),
        transform: None,
    };
    block_on(
        insert("workflow_run_graph_edges")
            .value("run_id", "run-1")
            .value("edge_id", 0)
            .value("revision", 1)
            .value("source_node_id", "review")
            .value("target_node_id", "review")
            .value("edge_json", serde_json::to_string(&edge).expect("json"))
            .execute(&store.connection),
    )
    .expect("edge fixture");
    block_on(
        update("workflow_run_graphs")
            .value("revision", 2)
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("revision fixture");
    drop(store);
    let store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    let before = fixture_changes(&store.connection);
    let page = store
        .current_run_graph_edges("run-1", 2, None, 1)
        .expect("page");
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].edge, edge);
    assert!(
        store
            .current_run_graph_edges("run-1", 2, Some(0), 1)
            .expect("end")
            .is_empty()
    );
    assert!(store.current_run_graph_edges("run-1", 1, None, 1).is_err());
    assert_eq!(
        store
            .run_graph_edge_revision("run-1", 0, 1)
            .expect("historical")
            .expect("edge")
            .edge,
        edge
    );
    assert!(
        store
            .run_graph_edge_revision("run-1", 0, 2)
            .expect("absent")
            .is_none()
    );
    assert!(store.run_graph_edge_revision("run-1", 0, 3).is_err());
    assert_eq!(fixture_changes(&store.connection), before);
}

#[test]
fn current_edges_reject_damaged_current_endpoint_but_preserve_history() {
    let (_temp, store) = initialized_store();
    let edge = bcode_workflow::EdgeDefinition {
        from: "review".to_string(),
        to: "review".to_string(),
        kind: bcode_workflow::EdgeKind::default(),
        transform: None,
    };
    block_on(
        insert("workflow_run_graph_edges")
            .value("run_id", "run-1")
            .value("edge_id", 0)
            .value("revision", 1)
            .value("source_node_id", "review")
            .value("target_node_id", "review")
            .value("edge_json", serde_json::to_string(&edge).expect("json"))
            .execute(&store.connection),
    )
    .expect("edge");
    block_on(
        insert("workflow_run_graph_nodes")
            .value("run_id", "run-1")
            .value("node_id", "review")
            .value("revision", 2)
            .value("node_json", "{}")
            .value("is_entry", 0)
            .value("is_exit", 0)
            .execute(&store.connection),
    )
    .expect("damaged current node");
    block_on(
        update("workflow_run_graphs")
            .value("revision", 2)
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("damaged current endpoint");
    let before = fixture_changes(&store.connection);
    assert!(store.current_run_graph_edges("run-1", 2, None, 1).is_err());
    assert_eq!(
        store
            .run_graph_edge_revision("run-1", 0, 1)
            .expect("history")
            .expect("edge")
            .edge,
        edge
    );
    assert_eq!(fixture_changes(&store.connection), before);
}

#[test]
fn current_node_rejects_uncommitted_revision_without_writes() {
    let (_temp, store) = initialized_store();
    {
        fixture_copy_graph_nodes_scoped(&store.connection, "run-1", Some("review"), 2);
        block_on(async { Ok::<(), switchy_database::DatabaseError>(()) })
    }
    .expect("future node");
    let before = fixture_changes(&store.connection);
    assert!(store.current_run_graph_node("run-1", "review").is_err());
    assert_eq!(fixture_changes(&store.connection), before);
}

fn assert_backup_nodes(backup: &dyn Database, expected: &[RunGraphNode]) {
    let rows = fixture_rows(
        backup,
        &select("workflow_run_graph_nodes")
            .columns(&["node_id", "revision", "node_json", "is_entry", "is_exit"])
            .where_eq("run_id", "run-1")
            .sort("node_id", switchy_database::query::SortDirection::Asc),
    );
    let actual: Vec<_> = rows
        .iter()
        .map(|row| {
            (
                fixture_text(row, "node_id"),
                fixture_unsigned(row, "revision"),
                fixture_text(row, "node_json"),
                fixture_unsigned(row, "is_entry") != 0,
                fixture_unsigned(row, "is_exit") != 0,
            )
        })
        .collect();
    assert_eq!(actual.len(), expected.len());
    for ((id, revision, payload, entry, exit), node) in actual.iter().zip(expected) {
        assert_eq!(id, &node.node.id);
        assert_eq!(*revision, node.revision);
        assert_eq!((*entry, *exit), (node.entry, node.exit));
        assert_eq!(
            serde_json::from_str::<bcode_workflow::NodeDefinition>(payload).expect("node payload"),
            node.node
        );
    }
}

fn assert_schema_16_backup_edge(backup: &dyn Database, edge: &bcode_workflow::EdgeDefinition) {
    assert_eq!(detected_store_schema(backup), Some(16));
    let revision = fixture_scalar_unsigned(
        backup,
        &select("workflow_run_graphs")
            .columns(&["revision"])
            .where_eq("run_id", "run-1"),
        "revision",
    )
    .expect("backed up graph revision");
    assert_eq!(revision, 1);
    let rows = fixture_rows(
        backup,
        &select("workflow_run_graph_edges")
            .columns(&[
                "edge_id",
                "revision",
                "source_node_id",
                "target_node_id",
                "edge_json",
            ])
            .where_eq("run_id", "run-1")
            .where_eq("edge_id", 42),
    );
    let row = rows.first().expect("backed up edge");
    let (edge_id, revision, source, target, payload) = (
        fixture_unsigned(row, "edge_id"),
        fixture_unsigned(row, "revision"),
        fixture_text(row, "source_node_id"),
        fixture_text(row, "target_node_id"),
        fixture_text(row, "edge_json"),
    );
    assert_eq!((edge_id, revision), (42, 1));
    assert_eq!((&source, &target), (&edge.from, &edge.to));
    assert_eq!(
        &serde_json::from_str::<bcode_workflow::EdgeDefinition>(&payload).expect("edge payload"),
        edge
    );
}

fn add_migration_exit_node(
    store: &WorkflowStore,
    source: &bcode_workflow::NodeDefinition,
) -> bcode_workflow::NodeDefinition {
    let mut target = source.clone();
    target.id = "migration-target".to_owned();
    block_on(
        update("workflow_run_graph_nodes")
            .value("is_entry", 1)
            .value("is_exit", 0)
            .where_eq("run_id", "run-1")
            .where_eq("node_id", source.id.as_str())
            .execute(&store.connection),
    )
    .expect("entry-only source");
    block_on(
        insert("workflow_run_graph_nodes")
            .value("run_id", "run-1")
            .value("node_id", target.id.as_str())
            .value("revision", 1)
            .value(
                "node_json",
                serde_json::to_string(&target).expect("serialize target"),
            )
            .value("is_entry", 0)
            .value("is_exit", 1)
            .execute(&store.connection),
    )
    .expect("persist target");
    target
}

#[test]
fn endpoint_index_damage_is_isolated_by_direction() {
    for source in [true, false] {
        let (temp, store) = initialized_store();
        let node = store
            .run_graph_nodes("run-1", None, 1)
            .expect("node")
            .remove(0)
            .node;
        let edge = bcode_workflow::EdgeDefinition {
            from: node.id.clone(),
            to: node.id.clone(),
            kind: bcode_workflow::EdgeKind::Direct,
            transform: None,
        };
        block_on(
            insert("workflow_run_graph_edges")
                .value("run_id", "run-1")
                .value("edge_id", 42)
                .value("revision", 1)
                .value("source_node_id", node.id.as_str())
                .value("target_node_id", node.id.as_str())
                .value(
                    "edge_json",
                    serde_json::to_string(&edge).expect("serialize"),
                )
                .execute(&store.connection),
        )
        .expect("persist edge");
        let index = if source {
            "workflow_run_graph_edges_source"
        } else {
            "workflow_run_graph_edges_target"
        };
        block_on(
            switchy_database::schema::drop_index(index, "workflow_run_graph_edges")
                .execute(&store.connection),
        )
        .expect("damage one index");
        let before = fixture_changes(&store.connection);
        let outgoing = store.run_graph_outgoing_edges("run-1", &node.id, None, 1);
        let incoming = store.run_graph_incoming_edges("run-1", &node.id, None, 1);
        let (damaged, intact) = if source {
            (outgoing, incoming)
        } else {
            (incoming, outgoing)
        };
        assert!(damaged.is_err());
        let intact = intact.expect("other direction remains available");
        assert_eq!(intact.len(), 1);
        assert_eq!(intact[0].edge_id, 42);
        assert_eq!(intact[0].edge, edge);
        assert_eq!(fixture_changes(&store.connection), before);
        drop(store);
        let reopened =
            WorkflowStore::open_in_state_dir(temp.path()).expect("reopen without repair");
        let before = fixture_changes(&reopened.connection);
        let outgoing = reopened.run_graph_outgoing_edges("run-1", &node.id, None, 1);
        let incoming = reopened.run_graph_incoming_edges("run-1", &node.id, None, 1);
        let (damaged, available) = if source {
            (outgoing, incoming)
        } else {
            (incoming, outgoing)
        };
        assert!(damaged.is_err());
        assert_eq!(available.expect("intact direction after reopen"), intact);
        assert_eq!(fixture_changes(&reopened.connection), before);
    }
}

#[test]
fn endpoint_reads_reject_missing_indexes_without_repair() {
    let (_temp, store) = initialized_store();
    for index in [
        "workflow_run_graph_edges_source",
        "workflow_run_graph_edges_target",
    ] {
        block_on(
            switchy_database::schema::drop_index(index, "workflow_run_graph_edges")
                .execute(&store.connection),
        )
        .expect("damage indexes");
    }
    let before = fixture_changes(&store.connection);
    assert!(
        store
            .run_graph_outgoing_edges("run-1", "review", None, 1)
            .is_err()
    );
    assert!(
        store
            .run_graph_incoming_edges("run-1", "review", None, 1)
            .is_err()
    );
    assert!(
        store
            .run_graph_edges("run-1", None, 1)
            .expect("unfiltered read remains available")
            .is_empty()
    );
    assert_eq!(fixture_changes(&store.connection), before);
    let count = fixture_scalar_unsigned(
        &store.connection,
        &select("sqlite_master")
            .project_as(switchy_database::query::count_all(), "count")
            .where_in(
                "name",
                vec![
                    "workflow_run_graph_edges_source",
                    "workflow_run_graph_edges_target",
                ],
            ),
        "count",
    )
    .expect("indexes remain absent");
    assert_eq!(count, 0);
}

#[test]
fn exact_node_reads_surface_invalid_row_revisions_without_repair() {
    let (temp, store) = initialized_store();
    let node = store
        .run_graph_nodes("run-1", None, 1)
        .expect("node")
        .remove(0)
        .node;
    block_on(store.connection.sqlite_ignore_check_constraints(true)).expect("fixture constraints");
    for revision in [-1, 0] {
        block_on(
            update("workflow_run_graph_nodes")
                .value("revision", revision)
                .where_eq("run_id", "run-1")
                .execute(&store.connection),
        )
        .expect("corrupt revision");
        let before = fixture_changes(&store.connection);
        assert!(store.run_graph_node("run-1", &node.id).is_err());
        assert!(store.run_graph_nodes("run-1", None, 10).is_err());
        assert_eq!(fixture_changes(&store.connection), before);
        let rows = fixture_rows(
            &store.connection,
            &select("workflow_run_graph_nodes")
                .where_eq("run_id", "run-1")
                .where_eq("revision", revision),
        );
        for row in rows {
            block_on(
                insert("workflow_run_graph_nodes")
                    .value("run_id", fixture_text(&row, "run_id"))
                    .value("node_id", fixture_text(&row, "node_id"))
                    .value("revision", 1)
                    .value("node_json", fixture_text(&row, "node_json"))
                    .value("is_entry", fixture_unsigned(&row, "is_entry"))
                    .value("is_exit", fixture_unsigned(&row, "is_exit"))
                    .execute(&store.connection),
            )
            .expect("coexisting valid revision");
        }
        let before = fixture_changes(&store.connection);
        assert!(store.run_graph_node("run-1", &node.id).is_err());
        assert!(store.run_graph_nodes("run-1", None, 1).is_err());
        assert_eq!(fixture_changes(&store.connection), before);
        block_on(
            delete("workflow_run_graph_nodes")
                .where_eq("run_id", "run-1")
                .where_eq("revision", 1)
                .execute(&store.connection),
        )
        .expect("remove valid fixture row");
        assert_eq!(
            {
                let rows = fixture_rows(
                    &store.connection,
                    &select("workflow_run_graph_nodes")
                        .columns(&["revision"])
                        .where_eq("run_id", "run-1"),
                );
                match rows.first().expect("preserved revision").get("revision") {
                    Some(DatabaseValue::Int64(value)) => value,
                    value => panic!("unexpected revision storage: {value:?}"),
                }
            },
            revision
        );
    }
    drop(store);
    let reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen damaged graph");
    let before = fixture_changes(&reopened.connection);
    assert!(reopened.run_graph_node("run-1", &node.id).is_err());
    assert!(reopened.run_graph_nodes("run-1", None, 1).is_err());
    assert_eq!(fixture_changes(&reopened.connection), before);
    assert_eq!(
        {
            let rows = fixture_rows(
                &reopened.connection,
                &select("workflow_run_graph_nodes")
                    .columns(&["revision"])
                    .where_eq("run_id", "run-1"),
            );
            match rows.first().expect("persisted damage").get("revision") {
                Some(DatabaseValue::Int64(value)) => value,
                value => panic!("unexpected revision storage: {value:?}"),
            }
        },
        0
    );
}

#[test]
fn edge_reads_surface_invalid_row_revisions_without_repair() {
    let (temp, store) = initialized_store();
    let node = store
        .run_graph_nodes("run-1", None, 1)
        .expect("node")
        .remove(0)
        .node;
    let edge = bcode_workflow::EdgeDefinition {
        from: node.id.clone(),
        to: node.id.clone(),
        kind: bcode_workflow::EdgeKind::Direct,
        transform: None,
    };
    block_on(store.connection.sqlite_ignore_check_constraints(true)).expect("fixture constraints");
    for revision in [-1, 0] {
        block_on(
            delete("workflow_run_graph_edges")
                .where_eq("run_id", "run-1")
                .execute(&store.connection),
        )
        .expect("reset fixture");
        for row_revision in [revision, 1] {
            block_on(
                insert("workflow_run_graph_edges")
                    .value("run_id", "run-1")
                    .value("edge_id", 0)
                    .value("revision", row_revision)
                    .value("source_node_id", node.id.as_str())
                    .value("target_node_id", node.id.as_str())
                    .value("edge_json", serde_json::to_string(&edge).expect("payload"))
                    .execute(&store.connection),
            )
            .expect("coexisting corrupt and valid edge revision");
        }
        let before = fixture_changes(&store.connection);
        assert!(store.run_graph_edge("run-1", 0).is_err());
        assert!(store.run_graph_edges("run-1", None, 1).is_err());
        for limit in [1, 10] {
            assert!(
                store
                    .run_graph_outgoing_edges("run-1", &node.id, None, limit)
                    .is_err()
            );
        }
        assert!(
            store
                .run_graph_incoming_edges("run-1", &node.id, None, 1)
                .is_err()
        );
        assert!(store.run_graph_edges("run-1", None, 10).is_err());
        assert!(
            store
                .run_graph_incoming_edges("run-1", &node.id, None, 10)
                .is_err()
        );
        assert_eq!(fixture_changes(&store.connection), before);
    }
    drop(store);
    let reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen damaged edges");
    let before = fixture_changes(&reopened.connection);
    assert!(
        reopened
            .run_graph_outgoing_edges("run-1", &node.id, None, 1)
            .is_err()
    );
    assert!(reopened.run_graph_edge("run-1", 0).is_err());
    assert!(reopened.run_graph_edges("run-1", None, 1).is_err());
    assert!(
        reopened
            .run_graph_incoming_edges("run-1", &node.id, None, 1)
            .is_err()
    );
    assert_eq!(fixture_changes(&reopened.connection), before);
    let revisions: Vec<_> = fixture_rows(
        &reopened.connection,
        &select("workflow_run_graph_edges")
            .columns(&["revision"])
            .where_eq("run_id", "run-1")
            .sort("revision", switchy_database::query::SortDirection::Asc),
    )
    .iter()
    .map(|row| match row.get("revision") {
        Some(DatabaseValue::Int64(value)) => value,
        value => panic!("unexpected revision storage: {value:?}"),
    })
    .collect();
    assert_eq!(revisions, vec![0, 1]);
}

#[test]
fn edge_first_pages_surface_negative_identities_without_repair() {
    let (_temp, store) = initialized_store();
    let node = store
        .run_graph_nodes("run-1", None, 1)
        .expect("node")
        .remove(0)
        .node;
    let edge = bcode_workflow::EdgeDefinition {
        from: node.id.clone(),
        to: node.id.clone(),
        kind: bcode_workflow::EdgeKind::Direct,
        transform: None,
    };
    block_on(store.connection.sqlite_ignore_check_constraints(true)).expect("fixture constraints");
    for id in [i64::MIN, -1] {
        block_on(
            delete("workflow_run_graph_edges")
                .where_eq("run_id", "run-1")
                .execute(&store.connection),
        )
        .expect("reset fixture");
        block_on(
            insert("workflow_run_graph_edges")
                .value("run_id", "run-1")
                .value("edge_id", id)
                .value("revision", 1)
                .value("source_node_id", node.id.as_str())
                .value("target_node_id", node.id.as_str())
                .value("edge_json", serde_json::to_string(&edge).expect("payload"))
                .execute(&store.connection),
        )
        .expect("corrupt edge identity");
        let before = fixture_changes(&store.connection);
        assert!(store.run_graph_edges("run-1", None, 10).is_err());
        assert!(
            store
                .run_graph_incoming_edges("run-1", &node.id, None, 10)
                .is_err()
        );
        assert_eq!(fixture_changes(&store.connection), before);
    }
}

#[test]
fn exact_edge_revision_reuses_caller_snapshot_without_committing() {
    let (_temp, store) = initialized_store();
    let edge = bcode_workflow::EdgeDefinition {
        from: "review".to_string(),
        to: "review".to_string(),
        kind: bcode_workflow::EdgeKind::Direct,
        transform: None,
    };
    block_on(
        insert("workflow_run_graph_edges")
            .value("run_id", "run-1")
            .value("edge_id", 0)
            .value("revision", 1)
            .value("source_node_id", "review")
            .value("target_node_id", "review")
            .value(
                "edge_json",
                serde_json::to_string(&edge).expect("edge payload"),
            )
            .execute(&store.connection),
    )
    .expect("edge fixture");
    let before = fixture_changes(&store.connection);
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("snapshot");
    let db: &dyn Database = transaction.as_ref();
    assert_eq!(
        WorkflowStore::run_graph_edge_revision_in(db, "run-1", 0, 1)
            .expect("read")
            .expect("edge")
            .edge,
        edge
    );
    assert!(!block_on(db.sqlite_is_autocommit()).expect("lease remains active"));
    assert!(WorkflowStore::run_graph_edge_revision_in(db, "run-1", 0, 2).is_err());
    assert!(!block_on(db.sqlite_is_autocommit()).expect("lease remains active"));
    block_on(transaction.rollback()).expect("caller retains ownership");
    assert!(store.run_graph_edge_revision("run-1", 0, 2).is_err());
    assert!(block_on(store.connection.sqlite_is_autocommit()).expect("owner released"));
    assert_eq!(fixture_changes(&store.connection), before);
}

#[test]
fn exact_node_read_preserves_snapshot_across_another_connection_write() {
    let (temp, store) = initialized_store();
    let writer = WorkflowStore::open_in_state_dir(temp.path()).expect("second connection");
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("snapshot");
    let db: &dyn Database = transaction.as_ref();
    let original = WorkflowStore::run_graph_node_revision_in(db, "run-1", "review", 1)
        .expect("initial read")
        .expect("node");
    block_on(
        writer
            .connection
            .sqlite_set_busy_timeout(std::time::Duration::ZERO),
    )
    .expect("nonblocking writer");
    let error = block_on(
        update("workflow_run_graph_nodes")
            .value("node_json", "not-json")
            .where_eq("run_id", "run-1")
            .where_eq("node_id", "review")
            .execute(&writer.connection),
    )
    .expect_err("snapshot fences writer");
    assert_eq!(
        error.classification(),
        Some(switchy_database::DatabaseErrorKind::Busy)
    );
    let before = fixture_changes(db);
    assert_eq!(
        WorkflowStore::run_graph_node_revision_in(db, "run-1", "review", 1)
            .expect("snapshot read")
            .expect("node"),
        original
    );
    assert!(!block_on(db.sqlite_is_autocommit()).expect("snapshot active"));
    block_on(transaction.rollback()).expect("release snapshot");
    block_on(
        update("workflow_run_graph_nodes")
            .value("node_json", "not-json")
            .where_eq("run_id", "run-1")
            .where_eq("node_id", "review")
            .execute(&writer.connection),
    )
    .expect("write after release");
    assert!(store.run_graph_node_revision("run-1", "review", 1).is_err());
    assert!(block_on(store.connection.sqlite_is_autocommit()).expect("owner released"));
    assert_eq!(fixture_changes(&store.connection), before);
}

#[test]
fn exact_node_revision_reuses_caller_snapshot_without_committing() {
    let (_temp, store) = initialized_store();
    let before = fixture_changes(&store.connection);
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("snapshot");
    let db: &dyn Database = transaction.as_ref();
    let node = WorkflowStore::run_graph_node_revision_in(db, "run-1", "review", 1)
        .expect("node read")
        .expect("node");
    assert_eq!(node.node.id, "review");
    assert_eq!(
        WorkflowStore::current_run_graph_node_in(db, "run-1", "review")
            .expect("current read")
            .expect("node"),
        node
    );
    assert!(WorkflowStore::current_run_graph_node_in(db, "run-1", "").is_err());
    assert!(!block_on(db.sqlite_is_autocommit()).expect("lease active"));
    assert!(WorkflowStore::run_graph_node_revision_in(db, "run-1", "review", 2).is_err());
    assert!(!block_on(db.sqlite_is_autocommit()).expect("lease active"));
    block_on(transaction.rollback()).expect("caller retains transaction");
    assert!(store.run_graph_node_revision("run-1", "review", 2).is_err());
    assert!(block_on(store.connection.sqlite_is_autocommit()).expect("owner released"));
    assert_eq!(fixture_changes(&store.connection), before);
}

#[test]
fn current_edges_reject_damaged_current_endpoint_but_exact_edges_remain_readable() {
    let (_temp, store) = initialized_store();
    let edge = bcode_workflow::EdgeDefinition {
        from: "review".to_string(),
        to: "review".to_string(),
        kind: bcode_workflow::EdgeKind::Direct,
        transform: None,
    };
    block_on(
        insert("workflow_run_graph_edges")
            .value("run_id", "run-1")
            .value("edge_id", 0)
            .value("revision", 1)
            .value("source_node_id", "review")
            .value("target_node_id", "review")
            .value("edge_json", serde_json::to_string(&edge).expect("edge"))
            .execute(&store.connection),
    )
    .expect("edge fixture");
    block_on(
        insert("workflow_run_graph_nodes")
            .value("run_id", "run-1")
            .value("node_id", "review")
            .value("revision", 2)
            .value("node_json", "invalid-json")
            .value("is_entry", 0)
            .value("is_exit", 0)
            .execute(&store.connection),
    )
    .expect("damaged current node");
    block_on(
        update("workflow_run_graphs")
            .value("revision", 2)
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("damaged current endpoint");
    let before = fixture_changes(&store.connection);
    assert_eq!(
        store
            .run_graph_edge_revision("run-1", 0, 1)
            .expect("historical edge")
            .expect("edge")
            .edge,
        edge
    );
    assert!(store.current_run_graph_edges("run-1", 2, None, 10).is_err());
    assert_eq!(fixture_changes(&store.connection), before);
    assert!(block_on(store.connection.sqlite_is_autocommit()).expect("owned snapshot released"));
}

#[test]
fn initial_edge_reads_reject_damaged_endpoint_payload_without_mutation() {
    let (_temp, store) = initialized_store();
    let edge = bcode_workflow::EdgeDefinition {
        from: "review".to_string(),
        to: "review".to_string(),
        kind: bcode_workflow::EdgeKind::Direct,
        transform: None,
    };
    block_on(
        insert("workflow_run_graph_edges")
            .value("run_id", "run-1")
            .value("edge_id", 0)
            .value("revision", 1)
            .value("source_node_id", "review")
            .value("target_node_id", "review")
            .value("edge_json", serde_json::to_string(&edge).expect("edge"))
            .execute(&store.connection),
    )
    .expect("edge fixture");
    assert!(
        store
            .run_graph_edge("run-1", 0)
            .expect("intact edge")
            .is_some()
    );
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("caller snapshot");
    let db: &dyn Database = transaction.as_ref();
    block_on(
        update("workflow_run_graph_nodes")
            .value("node_json", "{")
            .where_eq("run_id", "run-1")
            .where_eq("node_id", "review")
            .execute(db),
    )
    .expect("damaged endpoint");
    let before = fixture_changes(db);
    assert!(WorkflowStore::run_graph_edge_in(db, "run-1", 0).is_err());
    assert!(WorkflowStore::run_graph_edges_in(db, "run-1", None, 10).is_err());
    assert!(WorkflowStore::run_graph_incoming_edges_in(db, "run-1", "review", None, 10).is_err());
    assert!(WorkflowStore::run_graph_outgoing_edges_in(db, "run-1", "review", None, 10).is_err());
    assert_eq!(fixture_changes(db), before);
    assert!(!block_on(db.sqlite_is_autocommit()).expect("caller lease active"));
    block_on(transaction.rollback()).expect("caller rollback");
    assert!(
        store
            .run_graph_edge("run-1", 0)
            .expect("restored edge")
            .is_some()
    );
}

#[test]
fn initial_edges_validate_each_distinct_endpoint_payload() {
    let (_temp, store) = initialized_store();
    let mut target = store
        .run_graph_node("run-1", "review")
        .expect("node")
        .expect("review");
    target.id = "target".to_string();
    block_on(
        insert("workflow_run_graph_nodes")
            .value("run_id", "run-1")
            .value("node_id", "target")
            .value("revision", 1)
            .value("node_json", serde_json::to_string(&target).expect("target"))
            .value("is_entry", 0)
            .value("is_exit", 1)
            .execute(&store.connection),
    )
    .expect("target fixture");
    let edge = bcode_workflow::EdgeDefinition {
        from: "review".to_string(),
        to: "target".to_string(),
        kind: bcode_workflow::EdgeKind::Direct,
        transform: None,
    };
    block_on(
        insert("workflow_run_graph_edges")
            .value("run_id", "run-1")
            .value("edge_id", 0)
            .value("revision", 1)
            .value("source_node_id", "review")
            .value("target_node_id", "target")
            .value("edge_json", serde_json::to_string(&edge).expect("edge"))
            .execute(&store.connection),
    )
    .expect("edge fixture");
    target.id = "wrong-identity".to_string();
    let damaged_payloads = [
        "{".to_string(),
        " ".repeat(MAX_INLINE_JSON_BYTES + 1),
        serde_json::to_string(&target).expect("mismatched identity"),
    ];
    for endpoint in ["review", "target"] {
        for payload in &damaged_payloads {
            let transaction =
                database::begin(&store.connection, TransactionMode::Deferred).expect("snapshot");
            let db: &dyn Database = transaction.as_ref();
            block_on(
                update("workflow_run_graph_nodes")
                    .value("node_json", payload)
                    .where_eq("run_id", "run-1")
                    .where_eq("node_id", endpoint)
                    .execute(db),
            )
            .expect("damage endpoint");
            let before = fixture_changes(db);
            assert!(
                WorkflowStore::run_graph_edge_in(db, "run-1", 0).is_err(),
                "{endpoint}"
            );
            assert!(
                WorkflowStore::run_graph_edges_in(db, "run-1", None, 10).is_err(),
                "{endpoint}"
            );
            assert!(
                WorkflowStore::run_graph_incoming_edges_in(db, "run-1", "target", None, 10)
                    .is_err(),
                "{endpoint}"
            );
            assert!(
                WorkflowStore::run_graph_outgoing_edges_in(db, "run-1", "review", None, 10)
                    .is_err(),
                "{endpoint}"
            );
            assert_eq!(fixture_changes(db), before);
            assert!(!block_on(db.sqlite_is_autocommit()).expect("caller snapshot active"));
            block_on(transaction.rollback()).expect("rollback damage");
            assert_eq!(
                store
                    .run_graph_edge("run-1", 0)
                    .expect("restored")
                    .expect("edge")
                    .edge,
                edge
            );
        }
    }
}

#[test]
fn initial_edge_endpoint_validation_is_not_reused_across_pages() {
    let (_temp, store) = initialized_store();
    let edge = bcode_workflow::EdgeDefinition {
        from: "review".to_string(),
        to: "review".to_string(),
        kind: bcode_workflow::EdgeKind::Direct,
        transform: None,
    };
    let json = serde_json::to_string(&edge).expect("edge");
    for id in 0..3 {
        block_on(
            insert("workflow_run_graph_edges")
                .value("run_id", "run-1")
                .value("edge_id", id)
                .value("revision", 1)
                .value("source_node_id", "review")
                .value("target_node_id", "review")
                .value("edge_json", json.as_str())
                .execute(&store.connection),
        )
        .expect("shared endpoint edge");
    }
    let page = store.run_graph_edges("run-1", None, 2).expect("first page");
    assert_eq!(
        page.iter().map(|edge| edge.edge_id).collect::<Vec<_>>(),
        vec![0, 1]
    );
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("snapshot");
    let db: &dyn Database = transaction.as_ref();
    block_on(
        update("workflow_run_graph_nodes")
            .value("node_json", "{")
            .where_eq("run_id", "run-1")
            .where_eq("node_id", "review")
            .execute(db),
    )
    .expect("damage shared endpoint");
    let before = fixture_changes(db);
    assert!(WorkflowStore::run_graph_edges_in(db, "run-1", Some(1), 2).is_err());
    assert_eq!(fixture_changes(db), before);
    block_on(transaction.rollback()).expect("rollback");
    let page = store
        .run_graph_edges("run-1", Some(1), 2)
        .expect("restored page");
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].edge_id, 2);
}

#[test]
fn initial_node_reads_validate_before_storage_reads() {
    let (_temp, store) = initialized_store();
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("snapshot");
    block_on(transaction.sqlite_defer_foreign_keys(true)).expect("defer fixture foreign keys");
    let db: &dyn Database = transaction.as_ref();
    block_on(switchy_database::schema::drop_table("workflow_run_graphs").execute(db))
        .expect("unavailable graph fixture");
    let before = fixture_changes(db);
    for (run_id, node_id) in [("", "review"), ("run-1", "")] {
        assert!(matches!(
            WorkflowStore::run_graph_node_record_in(db, run_id, node_id),
            Err(WorkflowStoreError::InvalidData(_))
        ));
        assert!(matches!(
            WorkflowStore::run_graph_node_in(db, run_id, node_id),
            Err(WorkflowStoreError::InvalidData(_))
        ));
        assert!(matches!(
            super::run_graph::initial_exit(db, run_id, node_id),
            Err(WorkflowStoreError::InvalidData(_))
        ));
    }
    assert!(matches!(
        WorkflowStore::run_graph_node_record_in(db, "run-1", "review"),
        Err(WorkflowStoreError::StructuredDatabase(_))
    ));
    assert!(matches!(
        WorkflowStore::run_graph_node_in(db, "run-1", "review"),
        Err(WorkflowStoreError::StructuredDatabase(_))
    ));
    assert!(matches!(
        super::run_graph::initial_exit(db, "run-1", "review"),
        Err(WorkflowStoreError::StructuredDatabase(_))
    ));
    assert!(!block_on(db.sqlite_is_autocommit()).expect("caller lease active"));
    assert_eq!(fixture_changes(db), before);
    block_on(transaction.rollback()).expect("restore graph");
    let record = store
        .run_graph_node_record("run-1", "review")
        .expect("restored graph")
        .expect("node");
    assert_eq!(
        store.run_graph_node("run-1", "review").expect("node read"),
        Some(record.node)
    );
    assert_eq!(
        super::run_graph::initial_exit(&store.connection, "run-1", "review").expect("exit read"),
        record.exit
    );
}

fn assert_activation_keeps_node_in(db: &dyn Database, original: &super::RunGraphNode) {
    assert_eq!(
        WorkflowStore::activation_graph_node_in(db, "run-1", "review", &activation_id())
            .expect("activation binding"),
        Some(original.clone())
    );
}

fn assert_single_node_graph_page_in(db: &dyn Database, node: &super::RunGraphNode) {
    for expected in [None, Some(node.revision)] {
        let page = WorkflowStore::current_run_graph_page_in(db, "run-1", expected, None, None, 1)
            .expect("combined page");
        assert_eq!(page.revision, node.revision);
        assert_eq!(page.nodes, vec![node.clone()]);
        assert!(page.nodes_complete && page.edges_complete);
        assert!(page.edges.is_empty());
    }
}

fn assert_activation_keeps_node(store: &WorkflowStore, original: &super::RunGraphNode) {
    assert_eq!(
        store
            .activation_graph_node("run-1", "review", &activation_id())
            .expect("activation binding"),
        Some(original.clone())
    );
}

fn assert_single_node_graph_page(store: &WorkflowStore, node: &super::RunGraphNode) {
    for expected in [None, Some(node.revision)] {
        let page = store
            .current_run_graph_page("run-1", expected, None, None, 1)
            .expect("combined page");
        assert_eq!(page.revision, node.revision);
        assert_eq!(page.nodes, vec![node.clone()]);
        assert!(page.nodes_complete && page.edges_complete);
        assert!(page.edges.is_empty());
    }
}

#[test]
fn current_node_read_preserves_snapshot_across_concurrent_revision() {
    let (_temp, store) = initialized_store();
    // WAL permits the fixture writer to commit while the reader retains its snapshot.
    block_on(
        store
            .connection
            .sqlite_journal_mode(switchy_database::SqliteJournalMode::Wal),
    )
    .expect("enable concurrent fixture writes");
    let original = store
        .current_run_graph_node("run-1", "review")
        .expect("read")
        .expect("node");
    let writer = fixture_open(store.path(), true);
    let snapshot = database::begin(&store.connection, TransactionMode::Deferred).expect("snapshot");
    let db: &dyn Database = snapshot.as_ref();
    assert_eq!(
        WorkflowStore::current_run_graph_node_in(db, "run-1", "review").expect("pin snapshot"),
        Some(original.clone())
    );
    let mut revised_node = original.node.clone();
    revised_node.name = "Revised review".to_string();
    let edit = database::begin(&writer, TransactionMode::Deferred).expect("edit transaction");
    fixture_insert_concurrent_node_revision(edit.as_ref(), &revised_node, &original);
    block_on(
        update("workflow_run_graphs")
            .value("revision", 2)
            .where_eq("run_id", "run-1")
            .execute(edit.as_ref()),
    )
    .expect("publish revision");
    database::commit(edit).expect("concurrent commit");
    assert_activation_keeps_node_in(db, &original);
    assert_single_node_graph_page_in(db, &original);
    assert!(matches!(
        WorkflowStore::current_run_graph_page_in(db, "run-1", Some(2), None, None, 1),
        Err(WorkflowStoreError::InvalidData(_))
    ));
    assert_eq!(
        WorkflowStore::current_run_graph_nodes_in(db, "run-1", 1, None, 1).expect("snapshot page"),
        vec![original.clone()]
    );
    assert!(matches!(
        WorkflowStore::current_run_graph_nodes_in(db, "run-1", 2, None, 1),
        Err(WorkflowStoreError::InvalidData(_))
    ));
    assert_eq!(
        WorkflowStore::current_run_graph_node_in(db, "run-1", "review").expect("snapshot read"),
        Some(original.clone())
    );
    assert_eq!(
        run_graph::graph_revision(db, "run-1").expect("snapshot revision"),
        Some(1)
    );
    assert!(!block_on(db.sqlite_is_autocommit()).expect("snapshot remains active"));
    block_on(snapshot.rollback()).expect("release snapshot");
    assert!(matches!(
        store.current_run_graph_page("run-1", Some(1), None, None, 1),
        Err(WorkflowStoreError::InvalidData(_))
    ));
    assert!(matches!(
        store.current_run_graph_nodes("run-1", 1, None, 1),
        Err(WorkflowStoreError::InvalidData(_))
    ));
    assert!(block_on(store.connection.sqlite_is_autocommit()).expect("snapshot released"));
    let current = store
        .current_run_graph_node("run-1", "review")
        .expect("fresh read")
        .expect("node");
    assert_eq!(current.revision, 2);
    assert_single_node_graph_page(&store, &current);
    assert_eq!(
        store
            .current_run_graph_nodes("run-1", 2, None, 1)
            .expect("fresh revision page"),
        vec![current.clone()]
    );
    assert_eq!(current.node, revised_node);
    assert_eq!(
        (current.entry, current.exit),
        (!original.entry, !original.exit)
    );
    assert_ne!(current.node, original.node);
    assert_activation_keeps_node(&store, &original);
    assert_eq!(
        store
            .run_graph_node_revision("run-1", "review", 1)
            .expect("historical read"),
        Some(original)
    );
}

fn fixture_insert_concurrent_node_revision(
    db: &dyn Database,
    revised_node: &bcode_workflow::NodeDefinition,
    original: &RunGraphNode,
) {
    block_on(
        insert("workflow_run_graph_nodes")
            .value("run_id", "run-1")
            .value("node_id", "review")
            .value("revision", 2)
            .value(
                "node_json",
                serde_json::to_string(revised_node).expect("revised node payload"),
            )
            .value("is_entry", !original.entry)
            .value("is_exit", !original.exit)
            .execute(db),
    )
    .expect("new revision");
}

fn assert_single_edge_graph_page_in(db: &dyn Database, edge: &super::RunGraphEdge) {
    for expected in [None, Some(edge.revision)] {
        let page = WorkflowStore::current_run_graph_page_in(db, "run-1", expected, None, None, 1)
            .expect("combined edge page");
        assert_eq!(page.revision, edge.revision);
        assert_eq!(page.edges, vec![edge.clone()]);
        assert!(page.edges_complete && page.nodes_complete);
        assert_eq!(page.nodes.len(), 1);
        assert_eq!(page.nodes[0].node.id, "review");
        assert_eq!(page.nodes[0].revision, 1);
    }
}

fn assert_single_edge_graph_page(store: &WorkflowStore, edge: &super::RunGraphEdge) {
    for expected in [None, Some(edge.revision)] {
        let page = store
            .current_run_graph_page("run-1", expected, None, None, 1)
            .expect("combined edge page");
        assert_eq!(page.revision, edge.revision);
        assert_eq!(page.edges, vec![edge.clone()]);
        assert!(page.edges_complete && page.nodes_complete);
        assert_eq!(page.nodes.len(), 1);
        assert_eq!(page.nodes[0].node.id, "review");
        assert_eq!(page.nodes[0].revision, 1);
    }
}

#[test]
fn current_edge_reads_preserve_snapshot_across_concurrent_revision() {
    let (_temp, store) = initialized_store();
    block_on(
        store
            .connection
            .sqlite_journal_mode(switchy_database::SqliteJournalMode::Wal),
    )
    .expect("concurrent fixture");
    let edge = bcode_workflow::EdgeDefinition {
        from: "review".to_string(),
        to: "review".to_string(),
        kind: bcode_workflow::EdgeKind::Direct,
        transform: None,
    };
    block_on(
        insert("workflow_run_graph_edges")
            .value("run_id", "run-1")
            .value("edge_id", 0)
            .value("revision", 1)
            .value("source_node_id", "review")
            .value("target_node_id", "review")
            .value("edge_json", serde_json::to_string(&edge).expect("payload"))
            .execute(&store.connection),
    )
    .expect("initial edge");
    let writer = fixture_open(store.path(), true);
    let snapshot = database::begin(&store.connection, TransactionMode::Deferred).expect("snapshot");
    let db: &dyn Database = snapshot.as_ref();
    let original = WorkflowStore::current_run_graph_edge_in(db, "run-1", 0)
        .expect("pin snapshot")
        .expect("edge");
    let revised_edge = bcode_workflow::EdgeDefinition {
        kind: bcode_workflow::EdgeKind::Retry { max_attempts: 2 },
        ..edge
    };
    let edit = database::begin(&writer, TransactionMode::Deferred).expect("edit");
    block_on(
        insert("workflow_run_graph_edges")
            .value("run_id", "run-1")
            .value("edge_id", 0)
            .value("revision", 2)
            .value("source_node_id", "review")
            .value("target_node_id", "review")
            .value(
                "edge_json",
                serde_json::to_string(&revised_edge).expect("revised payload"),
            )
            .execute(edit.as_ref()),
    )
    .expect("new edge revision");
    block_on(
        update("workflow_run_graphs")
            .value("revision", 2)
            .where_eq("run_id", "run-1")
            .execute(edit.as_ref()),
    )
    .expect("publish");
    database::commit(edit).expect("concurrent commit");
    assert_single_edge_graph_page_in(db, &original);
    assert_eq!(
        WorkflowStore::current_run_graph_edge_in(db, "run-1", 0).expect("snapshot edge"),
        Some(original.clone())
    );
    assert_eq!(
        WorkflowStore::current_run_graph_edges_in(db, "run-1", 1, None, 1).expect("snapshot page"),
        vec![original.clone()]
    );
    assert!(matches!(
        WorkflowStore::current_run_graph_edges_in(db, "run-1", 2, None, 1),
        Err(WorkflowStoreError::InvalidData(_))
    ));
    assert!(!block_on(db.sqlite_is_autocommit()).expect("snapshot active"));
    block_on(snapshot.rollback()).expect("release");
    assert!(matches!(
        store.current_run_graph_edges("run-1", 1, None, 1),
        Err(WorkflowStoreError::InvalidData(_))
    ));
    assert!(block_on(store.connection.sqlite_is_autocommit()).expect("snapshot released"));
    let current = store
        .current_run_graph_edge("run-1", 0)
        .expect("fresh edge")
        .expect("edge");
    assert_eq!(current.revision, 2);
    assert_eq!(current.edge, revised_edge);
    assert_ne!(current.edge, original.edge);
    assert_single_edge_graph_page(&store, &current);
    assert_eq!(
        store
            .current_run_graph_edges("run-1", 2, None, 1)
            .expect("fresh page"),
        vec![current]
    );
    assert_eq!(
        store
            .run_graph_edge_revision("run-1", 0, 1)
            .expect("historical edge"),
        Some(original)
    );
}

#[test]
fn edge_reads_reject_unusable_endpoint_identities() {
    let (_temp, store) = initialized_store();
    let template = store
        .run_graph_nodes("run-1", None, 1)
        .expect("node")
        .remove(0)
        .node;
    for id in [String::new(), " ".to_string(), "x".repeat(MAX_ID_BYTES + 1)] {
        let mut node = template.clone();
        node.id.clone_from(&id);
        block_on(
            update("workflow_run_graph_nodes")
                .value("node_id", id.as_str())
                .value(
                    "node_json",
                    serde_json::to_string(&node).expect("node payload"),
                )
                .where_eq("run_id", "run-1")
                .execute(&store.connection),
        )
        .expect("corrupt node identity");
        let edge = bcode_workflow::EdgeDefinition {
            from: id.clone(),
            to: id.clone(),
            kind: bcode_workflow::EdgeKind::Direct,
            transform: None,
        };
        let tx = database::begin(&store.connection, TransactionMode::Deferred)
            .expect("edge fixture transaction");
        block_on(
            delete("workflow_run_graph_edges")
                .where_eq("run_id", "run-1")
                .where_eq("edge_id", 0)
                .where_eq("revision", 1)
                .execute(tx.as_ref()),
        )
        .expect("replace edge fixture");
        block_on(
            insert("workflow_run_graph_edges")
                .value("run_id", "run-1")
                .value("edge_id", 0)
                .value("revision", 1)
                .value("source_node_id", id.as_str())
                .value("target_node_id", id.as_str())
                .value(
                    "edge_json",
                    serde_json::to_string(&edge).expect("edge payload"),
                )
                .execute(tx.as_ref()),
        )
        .expect("corrupt edge identity");
        database::commit(tx).expect("edge fixture commit");
        let before = fixture_changes(&store.connection);
        assert!(
            store
                .run_graph_edge("run-1", 0)
                .expect_err("unusable endpoint")
                .to_string()
                .contains("source_node_id must contain")
        );
        assert!(store.run_graph_edges("run-1", None, 10).is_err());
        assert_eq!(fixture_changes(&store.connection), before);
    }
}

#[test]
fn edge_reads_reject_corrupt_endpoint_boundary_flags() {
    let (_temp, store) = initialized_store();
    let node = store
        .run_graph_nodes("run-1", None, 1)
        .expect("node")
        .remove(0)
        .node;
    let mut target = node.clone();
    target.id = "target".to_string();
    block_on(
        insert("workflow_run_graph_nodes")
            .value("run_id", "run-1")
            .value("node_id", target.id.as_str())
            .value("revision", 1)
            .value(
                "node_json",
                serde_json::to_string(&target).expect("target payload"),
            )
            .value("is_entry", 0)
            .value("is_exit", 1)
            .execute(&store.connection),
    )
    .expect("target");
    let edge = bcode_workflow::EdgeDefinition {
        from: node.id.clone(),
        to: target.id.clone(),
        kind: bcode_workflow::EdgeKind::Direct,
        transform: None,
    };
    block_on(
        insert("workflow_run_graph_edges")
            .value("run_id", "run-1")
            .value("edge_id", 0)
            .value("revision", 1)
            .value("source_node_id", node.id.as_str())
            .value("target_node_id", target.id.as_str())
            .value(
                "edge_json",
                serde_json::to_string(&edge).expect("serialize"),
            )
            .execute(&store.connection),
    )
    .expect("edge");
    block_on(store.connection.sqlite_ignore_check_constraints(true)).expect("fixture constraints");
    for endpoint in [&node.id, &target.id] {
        for flag in ["is_entry", "is_exit"] {
            assert!(
                store
                    .run_graph_edge("run-1", 0)
                    .expect("intact endpoints")
                    .is_some()
            );
            block_on(
                update("workflow_run_graph_nodes")
                    .value(flag, 2)
                    .where_eq("run_id", "run-1")
                    .where_eq("node_id", endpoint.as_str())
                    .execute(&store.connection),
            )
            .expect("damage one flag");
            let before = fixture_changes(&store.connection);
            assert!(store.run_graph_edge("run-1", 0).is_err());
            assert!(store.run_graph_edges("run-1", None, 10).is_err());
            assert!(
                store
                    .run_graph_incoming_edges("run-1", &target.id, None, 10)
                    .is_err()
            );
            assert_eq!(fixture_changes(&store.connection), before);
            block_on(
                update("workflow_run_graph_nodes")
                    .value(flag, 0)
                    .where_eq("run_id", "run-1")
                    .where_eq("node_id", endpoint.as_str())
                    .execute(&store.connection),
            )
            .expect("restore fixture flag");
        }
    }
}

#[test]
fn transform_run_json_bounds_payloads_and_preserves_absence() {
    let (_temp, store) = initialized_store();
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    let db: &dyn Database = transaction.as_ref();
    for field in [
        TransformRunField::Input,
        TransformRunField::AuthoredProvenance,
    ] {
        let column = match field {
            TransformRunField::Input => "input_json",
            TransformRunField::AuthoredProvenance => "authored_provenance_json",
        };
        for (value, valid) in [
            (FixtureStorageValue::Null, true),
            (FixtureStorageValue::Text("null".into()), true),
            (FixtureStorageValue::BlobText("null".into()), false),
            (
                FixtureStorageValue::Text("x".repeat(MAX_INLINE_JSON_BYTES)),
                true,
            ),
            (
                FixtureStorageValue::Text("é".repeat(MAX_INLINE_JSON_BYTES / 2)),
                true,
            ),
            (
                FixtureStorageValue::Text("é".repeat(MAX_INLINE_JSON_BYTES / 2 + 1)),
                false,
            ),
            (
                FixtureStorageValue::Text("x".repeat(MAX_INLINE_JSON_BYTES + 1)),
                false,
            ),
        ] {
            block_on(
                db.exec_update_count(
                    &(update("workflow_runs")
                        .value(column, value.expression())
                        .where_eq("run_id", "run-1")),
                ),
            )
            .expect("payload fixture");
            let changes = fixture_changes(db);
            let result = transform_run_json(db, "run-1", field);
            assert_eq!(result.is_ok(), valid);
            if valid {
                assert_eq!(
                    result.expect("valid payload"),
                    match value {
                        FixtureStorageValue::Text(text) => Some(text),
                        _ => None,
                    }
                );
            }
            assert_eq!(fixture_changes(db), changes);
        }
    }
}

#[test]
fn branch_skip_rejects_identity_collision_and_deduplicates_nodes() {
    let (_temp, store) = initialized_store();
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    let db: &dyn Database = transaction.as_ref();
    let nodes = ["review".to_string(), "review".to_string()];
    skip_branch_nodes(db, "run-1", 1, &nodes, 20).expect("duplicate node IDs");
    assert_eq!(
        activation_status_at_generation(db, "run-1", "review", 1)
            .expect("unique skipped activation")
            .as_deref(),
        Some("skipped")
    );
    block_on(
        update("workflow_activations")
            .value("dependency_generation", 2)
            .where_eq("run_id", "run-1")
            .where_eq("dependency_generation", 1)
            .execute(db),
    )
    .expect("identity collision fixture");
    let changes = fixture_changes(db);
    assert!(skip_branch_nodes(db, "run-1", 1, &nodes, 30).is_err());
    assert_eq!(fixture_changes(db), changes);
    assert!(
        activation_status_at_generation(db, "run-1", "review", 1)
            .expect("absent generation")
            .is_none()
    );
}

#[test]
fn branch_skip_preserves_existing_generation_with_distinct_identity() {
    let (_temp, store) = initialized_store();
    let fixture = database::begin(&store.connection, TransactionMode::Deferred)
        .expect("identity fixture transaction");
    block_on(fixture.sqlite_defer_foreign_keys(true)).expect("defer fixture foreign keys");
    block_on(
        update("workflow_activation_graph_bindings")
            .value("activation_id", "existing-identity")
            .where_eq("run_id", "run-1")
            .execute(fixture.as_ref()),
    )
    .expect("binding identity fixture");
    block_on(
        update("workflow_activations")
            .value("activation_id", "existing-identity")
            .where_eq("run_id", "run-1")
            .execute(fixture.as_ref()),
    )
    .expect("activation identity fixture");
    database::commit(fixture).expect("identity fixture");
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    let db: &dyn Database = transaction.as_ref();
    let changes = fixture_changes(db);
    skip_branch_nodes(db, "run-1", 0, &["review".to_string()], 20).expect("existing generation");
    assert_eq!(fixture_changes(db), changes);
    assert_eq!(
        activation_status_at_generation(db, "run-1", "review", 0)
            .expect("unambiguous generation")
            .as_deref(),
        Some("pending")
    );
}

#[test]
fn branch_skip_is_idempotent_and_rejects_ambiguous_generation() {
    let (_temp, store) = initialized_store();
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    let db: &dyn Database = transaction.as_ref();
    let nodes = ["review".to_string()];
    skip_branch_nodes(db, "run-1", 1, &nodes, 20).expect("first skip");
    let changes = fixture_changes(db);
    skip_branch_nodes(db, "run-1", 1, &nodes, 30).expect("duplicate skip");
    assert_eq!(fixture_changes(db), changes);
    let rows = fixture_rows(
        db,
        &select("workflow_activations")
            .columns(&["status", "created_at_ms"])
            .where_eq("run_id", "run-1")
            .where_eq("node_id", "review")
            .where_eq("dependency_generation", 1),
    );
    let row = rows.first().expect("skipped activation");
    let (status, created_at) = (
        fixture_text(row, "status"),
        fixture_unsigned(row, "created_at_ms"),
    );
    assert_eq!(status, "skipped");
    assert_eq!(created_at, 20);
    block_on(
        insert("workflow_activations")
            .value("run_id", "run-1")
            .value("node_id", "review")
            .value("activation_id", "duplicate-generation")
            .value("dependency_generation", 1)
            .value("status", "skipped")
            .value("created_at_ms", 20)
            .execute(db),
    )
    .expect("ambiguous generation fixture");
    let changes = fixture_changes(db);
    let error = skip_branch_nodes(db, "run-1", 1, &nodes, 40).expect_err("ambiguous generation");
    assert!(error.to_string().contains("ambiguous activations"));
    assert_eq!(fixture_changes(db), changes);
}

#[test]
fn branch_skip_rejects_unknown_existing_activation_without_mutation() {
    let (_temp, store) = initialized_store();
    block_on(
        update("workflow_activations")
            .value("status", "future_status")
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("future state fixture");
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    let db: &dyn Database = transaction.as_ref();
    let changes = fixture_changes(db);
    let error = skip_branch_nodes(db, "run-1", 0, &["review".to_string()], 20)
        .expect_err("unknown state must not be silently skipped");
    assert!(error.to_string().contains("unsupported activation status"));
    assert_eq!(fixture_changes(db), changes);
}

#[test]
fn unknown_activation_state_does_not_block_other_run_completion() {
    let (_temp, mut store) = initialized_store();
    let mut other = new_run();
    other.run_id = "run-2".to_string();
    store.create_run(&other).expect("second run");
    block_on(
        update("workflow_activations")
            .value("status", "future_status")
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("future state fixture");
    let original_events = store.event_history("run-1", None, 100).expect("events");
    let result = store
        .persist_validated_output(&ValidatedOutput {
            output_id: "other-exit-output".to_string(),
            run_id: "run-2".to_string(),
            node_id: "review".to_string(),
            activation_id: activation_identity("run-2", "review", 0),
            schema_id: "u32".to_string(),
            schema_version: 1,
            value: serde_json::json!(2),
            artifact_reference: None,
            created_at_ms: 20,
        })
        .expect("unaffected run completes");
    assert_eq!(result.run_status, RunStatus::Completed);
    assert_eq!(
        store
            .run_summary("run-2")
            .expect("summary")
            .expect("run")
            .status,
        RunStatus::Completed
    );
    assert_eq!(
        store.event_history("run-1", None, 100).expect("events"),
        original_events
    );
    let status: String = fixture_scalar_text(
        &store.connection,
        &select("workflow_activations")
            .columns(&["status"])
            .where_eq("run_id", "run-1"),
        "status",
    )
    .expect("preserved status");
    assert_eq!(status, "future_status");
}

#[test]
fn unfinished_activation_check_classifies_states_and_isolates_runs() {
    let (_temp, store) = initialized_store();
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    let db: &dyn Database = transaction.as_ref();
    for (status, unfinished) in [
        ("pending", true),
        ("running", true),
        ("waiting_input", true),
        ("waiting_approval", true),
        ("waiting_mutation_approval", true),
        ("completed", false),
        ("skipped", false),
        ("failed", false),
        ("cancelled", false),
    ] {
        block_on(
            update("workflow_activations")
                .value("status", status)
                .where_eq("run_id", "run-1")
                .execute(db),
        )
        .expect("status fixture");
        let changes = fixture_changes(db);
        assert_eq!(
            run_has_unfinished_activations(db, "run-1").expect("supported status"),
            unfinished,
            "{status}"
        );
        assert_eq!(fixture_changes(db), changes);
    }
    block_on(
        update("workflow_activations")
            .value("status", "future_status")
            .where_eq("run_id", "run-1")
            .execute(db),
    )
    .expect("future status fixture");
    let changes = fixture_changes(db);
    assert!(run_has_unfinished_activations(db, "run-1").is_err());
    assert!(
        !run_has_unfinished_activations(db, "unrelated-run")
            .expect("unrelated run remains readable")
    );
    assert_eq!(fixture_changes(db), changes);
}

#[test]
fn exit_output_rejects_unknown_activation_state_atomically() {
    let (_temp, mut store) = initialized_store();
    block_on(
        insert("workflow_activations")
            .value("run_id", "run-1")
            .value("node_id", "review")
            .value("activation_id", "future-activation")
            .value("dependency_generation", 1)
            .value("status", "future_status")
            .value("created_at_ms", 1)
            .execute(&store.connection),
    )
    .expect("future fixture");
    let events = store.event_history("run-1", None, 100).expect("events");
    let error = store
        .persist_validated_output(&ValidatedOutput {
            output_id: "exit-output".to_string(),
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: activation_identity("run-1", "review", 0),
            schema_id: "u32".to_string(),
            schema_version: 1,
            value: serde_json::json!(2),
            artifact_reference: None,
            created_at_ms: 20,
        })
        .expect_err("unsupported state");
    assert!(error.to_string().contains("unsupported activation status"));
    assert_eq!(
        store.event_history("run-1", None, 100).expect("events"),
        events
    );
    assert!(
        store
            .output_summaries("run-1", 100)
            .expect("outputs")
            .is_empty()
    );
    let status: String = fixture_scalar_text(
        &store.connection,
        &select("workflow_activations")
            .columns(&["status"])
            .where_eq("activation_id", "future-activation"),
        "status",
    )
    .expect("preserved future status");
    assert_eq!(status, "future_status");
}

#[test]
fn exit_output_does_not_complete_run_with_waiting_activation() {
    for status in [
        "waiting_input",
        "waiting_approval",
        "waiting_mutation_approval",
    ] {
        let (_temp, mut store) = initialized_store();
        block_on(
            insert("workflow_activations")
                .value("run_id", "run-1")
                .value("node_id", "review")
                .value("activation_id", "waiting-activation")
                .value("dependency_generation", 1)
                .value("status", status)
                .value("created_at_ms", 1)
                .execute(&store.connection),
        )
        .expect("waiting fixture");
        let result = store
            .persist_validated_output(&ValidatedOutput {
                output_id: "exit-output".to_string(),
                run_id: "run-1".to_string(),
                node_id: "review".to_string(),
                activation_id: activation_identity("run-1", "review", 0),
                schema_id: "u32".to_string(),
                schema_version: 1,
                value: serde_json::json!(2),
                artifact_reference: None,
                created_at_ms: 20,
            })
            .expect("exit output");
        assert_eq!(result.run_status, RunStatus::Running);
        assert_eq!(
            store
                .run_summary("run-1")
                .expect("summary")
                .expect("run")
                .status,
            RunStatus::Running
        );
        assert!(
            !store
                .event_history("run-1", None, 100)
                .expect("events")
                .iter()
                .any(|event| event.event_type == "run_completed")
        );
    }
}

#[test]
fn activation_status_reader_preserves_supported_states_and_generation_absence() {
    let (_temp, store) = initialized_store();
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    let db: &dyn Database = transaction.as_ref();
    for status in [
        "pending",
        "running",
        "waiting_input",
        "waiting_approval",
        "waiting_mutation_approval",
        "completed",
        "skipped",
        "failed",
        "cancelled",
    ] {
        block_on(
            update("workflow_activations")
                .value("status", status)
                .where_eq("run_id", "run-1")
                .where_eq("node_id", "review")
                .execute(db),
        )
        .expect("status fixture");
        let before = fixture_changes(db);
        assert_eq!(
            activation_status_at_generation(db, "run-1", "review", 0)
                .expect("supported state")
                .as_deref(),
            Some(status),
        );
        assert!(
            activation_status_at_generation(db, "run-1", "review", 1)
                .expect("absent generation")
                .is_none()
        );
        assert_eq!(fixture_changes(db), before);
    }
}

#[test]
fn successor_readiness_rejects_ambiguous_completed_dependency() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("readiness", 1, &sequential_definition())
        .expect("definition");
    let mut run = new_run();
    run.definition_id = "readiness".to_string();
    store.create_run(&run).expect("run");
    block_on(
        update("workflow_activations")
            .value("status", "completed")
            .where_eq("run_id", run.run_id.as_str())
            .execute(&store.connection),
    )
    .expect("completed dependency");
    let output = ValidatedOutput {
        output_id: "first-output".to_string(),
        run_id: run.run_id.clone(),
        node_id: "first".to_string(),
        activation_id: activation_identity(&run.run_id, "first", 0),
        schema_id: "u32".to_string(),
        schema_version: 1,
        value: serde_json::json!(1),
        artifact_reference: None,
        created_at_ms: 20,
    };
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    let db: &dyn Database = transaction.as_ref();
    assert!(successor_dependencies_ready(db, &output, "second", 0).expect("ready"));
    let rows = fixture_rows(
        db,
        &select("workflow_activations")
            .where_eq("run_id", run.run_id.as_str())
            .where_eq("node_id", "first"),
    );
    for row in rows {
        block_on(
            insert("workflow_activations")
                .value("run_id", fixture_text(&row, "run_id"))
                .value("node_id", fixture_text(&row, "node_id"))
                .value("activation_id", "duplicate")
                .value(
                    "dependency_generation",
                    fixture_unsigned(&row, "dependency_generation"),
                )
                .value("status", "pending")
                .value("created_at_ms", fixture_unsigned(&row, "created_at_ms"))
                .execute(db),
        )
        .expect("ambiguous fixture");
    }
    let before = fixture_changes(db);
    assert!(
        successor_dependencies_ready(db, &output, "second", 0)
            .expect_err("ambiguous dependency")
            .to_string()
            .contains("ambiguous activations")
    );
    assert_eq!(fixture_changes(db), before);
    block_on(
        delete("workflow_activations")
            .where_eq("run_id", run.run_id.as_str())
            .where_eq("activation_id", "duplicate")
            .execute(db),
    )
    .expect("remove duplicate fixture");
    block_on(
        update("workflow_activations")
            .value("status", "future_status")
            .where_eq("run_id", run.run_id.as_str())
            .where_eq("node_id", "first")
            .execute(db),
    )
    .expect("unsupported dependency fixture");
    let before = fixture_changes(db);
    assert!(
        successor_dependencies_ready(db, &output, "second", 0)
            .expect_err("unsupported dependency is not a normal wait")
            .to_string()
            .contains("unsupported activation status")
    );
    assert_eq!(fixture_changes(db), before);
    block_on(
        delete("workflow_run_graph_nodes")
            .where_eq("run_id", run.run_id.as_str())
            .where_eq("node_id", "first")
            .execute(db),
    )
    .expect("remove dependency node fixture");
    let before = fixture_changes(db);
    assert!(
        successor_dependencies_ready(db, &output, "second", 0)
            .expect_err("missing dependency node")
            .to_string()
            .contains("edge endpoint is missing at revision")
    );
    assert_eq!(fixture_changes(db), before);
}

#[test]
fn successor_readiness_checks_dependencies_beyond_first_page() {
    let (_temp, mut store) = initialized_store();
    let mut plan = definition("example");
    for id in ["pending", "target"] {
        let mut node = plan.nodes["review"].clone();
        node.id = id.to_string();
        plan.nodes.insert(id.to_string(), node);
    }
    let edge = bcode_workflow::EdgeDefinition {
        from: "review".to_string(),
        to: "target".to_string(),
        kind: bcode_workflow::EdgeKind::Direct,
        transform: None,
    };
    plan.edges = vec![edge.clone(); 100];
    let mut pending = edge.clone();
    pending.from = "pending".to_string();
    plan.edges.push(pending);
    let output = ValidatedOutput {
        output_id: "result".to_string(),
        run_id: "run-1".to_string(),
        node_id: "review".to_string(),
        activation_id: activation_id(),
        schema_id: plan.nodes["review"].output.type_name.clone(),
        schema_version: 1,
        value: serde_json::json!(2),
        artifact_reference: None,
        created_at_ms: 12,
    };
    store
        .persist_validated_output(&output)
        .expect("completed source");
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    let db: &dyn Database = transaction.as_ref();
    for table in [
        "workflow_run_graph_edges",
        "workflow_run_graph_nodes",
        "workflow_run_graphs",
    ] {
        block_on(delete(table).where_eq("run_id", "run-1").execute(db))
            .expect("replace fixture graph");
    }
    run_graph::materialize(db, "run-1", &plan).expect("materialize");
    let before = fixture_changes(db);
    assert!(!successor_dependencies_ready(db, &output, "target", 0).expect("pending dependency"));
    assert_eq!(fixture_changes(db), before);
    let mut conditional = edge;
    conditional.kind = bcode_workflow::EdgeKind::Conditional {
        predicate: bcode_workflow::PredicateExpression::Equals {
            version: bcode_workflow::WORKFLOW_PREDICATE_VERSION,
            path: "status".to_string(),
            value: serde_json::json!("ready"),
        },
        expected: true,
    };
    block_on(
        insert("workflow_run_graph_edges")
            .value("run_id", "run-1")
            .value("edge_id", 101)
            .value("revision", 1)
            .value("source_node_id", "review")
            .value("target_node_id", "target")
            .value(
                "edge_json",
                serde_json::to_string(&conditional).expect("edge"),
            )
            .execute(db),
    )
    .expect("conditional edge beyond page");
    let before = fixture_changes(db);
    assert!(
        successor_dependencies_ready(db, &output, "target", 0).expect("conditional dependency")
    );
    assert!(!successor_dependencies_ready(db, &output, "target", 1).expect("different generation"));
    assert_eq!(fixture_changes(db), before);
}

#[test]
fn edge_between_rejects_ambiguous_matches_beyond_a_page_without_mutation() {
    let (_temp, store) = initialized_store();
    let mut plan = definition("example");
    let mut target = plan.nodes["review"].clone();
    target.id = "target".to_string();
    plan.nodes.insert(target.id.clone(), target);
    let edge = bcode_workflow::EdgeDefinition {
        from: "review".to_string(),
        to: "review".to_string(),
        kind: bcode_workflow::EdgeKind::Direct,
        transform: None,
    };
    plan.edges = vec![edge.clone(); 100];
    let mut matched = edge;
    matched.to = "target".to_string();
    plan.edges.push(matched.clone());
    plan.edges.push(matched.clone());
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    let db: &dyn Database = transaction.as_ref();
    for table in [
        "workflow_run_graph_edges",
        "workflow_run_graph_nodes",
        "workflow_run_graphs",
    ] {
        block_on(delete(table).where_eq("run_id", "run-1").execute(db))
            .expect("replace fixture graph");
    }
    run_graph::materialize(db, "run-1", &plan).expect("materialize");
    // A different later duplicate makes edge ordering observable.
    let mut later = matched;
    later.kind = bcode_workflow::EdgeKind::Conditional {
        predicate: bcode_workflow::PredicateExpression::Equals {
            version: bcode_workflow::WORKFLOW_PREDICATE_VERSION,
            path: "status".to_string(),
            value: serde_json::json!("ready"),
        },
        expected: true,
    };
    block_on(
        update("workflow_run_graph_edges")
            .value("edge_json", serde_json::to_string(&later).expect("edge"))
            .where_eq("run_id", "run-1")
            .where_eq("edge_id", 101)
            .execute(db),
    )
    .expect("later duplicate");
    let before = fixture_changes(db);
    assert!(
        run_graph::initial_edge_between(db, "run-1", "review", "target")
            .expect_err("ambiguous second page matches")
            .to_string()
            .contains("unambiguous edge binding")
    );
    assert_eq!(
        run_graph::initial_edge_between(db, "run-1", "review", "absent").expect("no match"),
        None
    );
    assert_eq!(fixture_changes(db), before);

    block_on(
        update("workflow_run_graph_edges")
            .value("edge_json", "invalid")
            .where_eq("run_id", "run-1")
            .where_eq("edge_id", 101)
            .execute(transaction.as_ref()),
    )
    .expect("damage second page");
    let before = fixture_changes(db);
    assert!(run_graph::initial_edge_between(db, "run-1", "review", "absent").is_err());
    assert_eq!(fixture_changes(db), before);
}

#[test]
fn edge_between_rejects_unsupported_revision_without_mutation() {
    let (_temp, store) = initialized_store();
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    let db: &dyn Database = transaction.as_ref();
    block_on(
        update("workflow_run_graphs")
            .value("revision", 2)
            .where_eq("run_id", "run-1")
            .execute(db),
    )
    .expect("unsupported graph revision");
    let before = fixture_changes(db);
    for target in ["review", "absent"] {
        assert!(matches!(
            run_graph::initial_edge_between(db, "run-1", "review", target),
            Err(WorkflowStoreError::InvalidData(_))
        ));
    }
    assert_eq!(fixture_changes(db), before);
    let revision = fixture_scalar_unsigned(
        db,
        &select("workflow_run_graphs")
            .columns(&["revision"])
            .where_eq("run_id", "run-1"),
        "revision",
    )
    .expect("preserved revision");
    assert_eq!(revision, 2);
}

#[test]
fn edge_readers_reject_future_only_revision_without_mutation() {
    let (_temp, store) = initialized_store();
    let edge = bcode_workflow::EdgeDefinition {
        from: "review".to_string(),
        to: "review".to_string(),
        kind: bcode_workflow::EdgeKind::Direct,
        transform: None,
    };
    block_on(
        insert("workflow_run_graph_edges")
            .value("run_id", "run-1")
            .value("edge_id", 0)
            .value("revision", 2)
            .value("source_node_id", "review")
            .value("target_node_id", "review")
            .value(
                "edge_json",
                serde_json::to_string(&edge).expect("edge payload"),
            )
            .execute(&store.connection),
    )
    .expect("future-only edge");
    let before = fixture_changes(&store.connection);
    assert!(matches!(
        store.run_graph_edge("run-1", 0),
        Err(WorkflowStoreError::InvalidData(_))
    ));
    for result in [
        store.run_graph_edges("run-1", None, 1),
        store.run_graph_incoming_edges("run-1", "review", None, 1),
        store.run_graph_outgoing_edges("run-1", "review", None, 1),
    ] {
        assert!(matches!(result, Err(WorkflowStoreError::InvalidData(_))));
    }
    assert_eq!(fixture_changes(&store.connection), before);
}

#[test]
fn successor_materialization_rejects_inconsistent_source_binding() {
    let (_temp, store) = initialized_store();
    let activation = store.pending_activations(1).expect("pending").remove(0);
    let output = ValidatedOutput {
        output_id: "binding-output".to_string(),
        run_id: activation.run_id,
        node_id: activation.node_id,
        activation_id: activation.activation_id,
        schema_id: activation.node.output.type_name,
        schema_version: 1,
        value: serde_json::json!(1),
        artifact_reference: None,
        created_at_ms: 20,
    };
    block_on(
        update("workflow_activations")
            .value("node_revision", 2)
            .execute(&store.connection),
    )
    .expect("invalid binding");
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    let before = fixture_changes(transaction.as_ref());
    assert!(
        materialize_direct_successors(transaction.as_ref(), &output, &NoopWorkflowOutputFault)
            .is_err()
    );
    assert_eq!(fixture_changes(transaction.as_ref()), before);
}

fn assert_unsettled_reassignment_refused(
    store: &mut WorkflowStore,
    identity: &str,
    initial: &WorkflowExecutionAuthority,
    replacement: &WorkflowExecutionAuthority,
    evidence: &EndedOwnerEvidence,
) {
    for status in [
        "running",
        "cancelling",
        "sibling_cancelling",
        "repair_required",
        "future_attempt_state",
    ] {
        block_on(
            update("workflow_attempts")
                .value("status", status)
                .where_eq("dispatch_identity", identity)
                .execute(&store.connection),
        )
        .expect("persist unresolved operation state");
        assert!(store.validate_quiescent_reassignment("run-1").is_err());
        assert!(
            store
                .reassign_execution_authority_from_ended_owner(
                    "run-1",
                    initial,
                    replacement,
                    evidence,
                    21,
                )
                .is_err()
        );
    }
}

#[test]
fn cross_artifact_reassignment_requires_quiescent_run_and_is_audited() {
    let (_temp, mut store) = initialized_store();
    let initial = WorkflowExecutionAuthority {
        target_artifact_id: "artifact-old".to_string(),
        daemon_instance_id: "daemon-old".to_string(),
        generation: 1,
        fencing_token: "token-old".to_string(),
    };
    block_on(
        update("workflow_runs")
            .value("target_artifact_id", initial.target_artifact_id.as_str())
            .value(
                "coordinator_daemon_instance_id",
                initial.daemon_instance_id.as_str(),
            )
            .value("coordinator_generation", initial.generation)
            .value("coordinator_fencing_token", initial.fencing_token.as_str())
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("seed authority");
    let replacement = WorkflowExecutionAuthority {
        target_artifact_id: "artifact-new".to_string(),
        daemon_instance_id: "daemon-new".to_string(),
        generation: 2,
        fencing_token: "token-new".to_string(),
    };
    let evidence = EndedOwnerEvidence {
        ended_daemon_instance_id: "daemon-old".to_string(),
        ended_target_artifact_id: "artifact-old".to_string(),
        liveness: EndedOwnerLiveness::NoLiveTrace,
        artifact_image_available: false,
    };

    // Every unresolved state pins the run to its original artifact.
    let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::Mutating);
    assert_unsettled_reassignment_refused(&mut store, &identity, &initial, &replacement, &evidence);
    assert_eq!(
        store.execution_authority("run-1").expect("authority"),
        Some(initial.clone())
    );

    // Once the attempt is settled the run is quiescent and may be reassigned.
    block_on(
        update("workflow_attempts")
            .value("status", "failed")
            .value("terminal_at_ms", 22)
            .where_eq("dispatch_identity", identity)
            .execute(&store.connection),
    )
    .expect("settle attempt");
    store
        .reassign_execution_authority_from_ended_owner(
            "run-1",
            &initial,
            &replacement,
            &evidence,
            23,
        )
        .expect("reassign quiescent run");
    assert_eq!(
        store.execution_authority("run-1").expect("authority"),
        Some(replacement.clone())
    );
    store
        .verify_execution_authority("run-1", &replacement)
        .expect("new authority is current");
    assert!(store.verify_execution_authority("run-1", &initial).is_err());
    let audit = store
        .event_history("run-1", None, 100)
        .expect("events")
        .into_iter()
        .find(|event| event.event_type == "authority_reassigned")
        .expect("reassignment is audited");
    assert_eq!(audit.payload["evidence"]["liveness"], "no_live_trace");
    assert_eq!(audit.payload["from"]["target_artifact_id"], "artifact-old");
    assert_eq!(audit.payload["to"]["target_artifact_id"], "artifact-new");

    // Stale expected authority loses the compare-and-swap.
    assert!(
        store
            .reassign_execution_authority_from_ended_owner(
                "run-1",
                &initial,
                &replacement,
                &evidence,
                24,
            )
            .is_err()
    );
}

#[test]
fn deferred_receipt_backed_mutation_remains_running() {
    struct Observer;
    impl AttemptStatusObserver for Observer {
        fn observe(
            &self,
            _request: &AttemptReconciliationRequest,
        ) -> Result<AttemptObservation, WorkflowStoreError> {
            Ok(AttemptObservation::Deferred {
                reason: "another live daemon owns the attempt".to_string(),
            })
        }
    }

    let (temp, mut store) = initialized_store();
    let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::Mutating);
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
    let summary = store
        .reconcile_receipt_backed_attempts(&Observer, 10, 20)
        .expect("reconcile");
    assert_eq!(summary.deferred, [identity]);
    assert!(summary.repair_required.is_empty());
    assert_eq!(
        store
            .run_summary("run-1")
            .expect("summary")
            .expect("run")
            .status,
        RunStatus::Running
    );
    assert_eq!(
        store.attempt_history("run-1", None, 10).expect("attempts")[0].status,
        "admitted"
    );
}

#[test]
fn sibling_cancellation_receipt_is_observed_after_restart_before_signal_ack() {
    struct Observer;
    impl AttemptStatusObserver for Observer {
        fn observe(
            &self,
            _request: &AttemptReconciliationRequest,
        ) -> Result<AttemptObservation, WorkflowStoreError> {
            Ok(AttemptObservation::Cancelled)
        }
    }
    let (temp, mut store) = initialized_store();
    let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::Mutating);
    block_on(
        update("workflow_attempts")
            .value("status", "sibling_cancelling")
            .where_eq("dispatch_identity", identity.as_str())
            .execute(&store.connection),
    )
    .expect("interrupted sibling signal");
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
    let summary = store
        .reconcile_receipt_backed_attempts(&Observer, 10, 20)
        .expect("observe");
    assert_eq!(summary.cancelled, [identity]);
    assert_eq!(
        store.attempt_history("run-1", None, 10).expect("history")[0].status,
        "cancelled"
    );
}

#[test]
fn unknown_receipt_backed_mutation_becomes_repair_required() {
    struct Observer;
    impl AttemptStatusObserver for Observer {
        fn observe(
            &self,
            _request: &AttemptReconciliationRequest,
        ) -> Result<AttemptObservation, WorkflowStoreError> {
            Ok(AttemptObservation::Unknown)
        }
    }

    let (temp, mut store) = initialized_store();
    let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::Mutating);
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
    let summary = store
        .reconcile_receipt_backed_attempts(&Observer, 10, 20)
        .expect("reconcile");
    assert_eq!(summary.repair_required, [identity]);
    assert_eq!(
        store
            .run_summary("run-1")
            .expect("summary")
            .expect("run")
            .status,
        RunStatus::RepairRequired
    );
}

fn assert_root_attempt_budget_survives_reopen(store: &WorkflowStore, child_run_id: &str) {
    let reopened = WorkflowStore::open_at_path(store.path()).expect("reopen");
    let transaction =
        database::begin(&reopened.connection, TransactionMode::Deferred).expect("budget snapshot");
    block_on(
        update("workflow_runs")
            .value("node_execution_cap", 1)
            .where_eq("run_id", "parent-run")
            .execute(transaction.as_ref()),
    )
    .expect("budget");
    assert!(enforce_root_attempt_limit(transaction.as_ref(), child_run_id).is_err());
    assert!(enforce_root_attempt_limit(transaction.as_ref(), "parent-run").is_err());
    block_on(
        update("workflow_runs")
            .value("node_execution_cap", 2)
            .where_eq("run_id", "parent-run")
            .execute(transaction.as_ref()),
    )
    .expect("budget");
    enforce_root_attempt_limit(transaction.as_ref(), child_run_id)
        .expect("remaining attempt allowance");
    block_on(transaction.rollback()).expect("restore fixture limits");
}

async fn assert_root_exhaustion_defers_child_dispatch(path: &Path, child_run_id: &str) {
    let mut reopened = WorkflowStore::open_at_path(path).unwrap();
    let original_cap = fixture_scalar_unsigned(
        &reopened.connection,
        &select("workflow_runs")
            .columns(&["node_execution_cap"])
            .where_eq("run_id", "parent-run"),
        "node_execution_cap",
    )
    .unwrap();
    fixture_exhaust_root_attempt_budget(&reopened.connection);
    let owner = MutationDispatchOwner(std::sync::atomic::AtomicUsize::new(0));
    let before = fixture_changes(&reopened.connection);
    let allowance = reopened
        .execution_allowance_observation(child_run_id)
        .unwrap();
    assert_eq!(allowance.root_cap, 1);
    assert_eq!(allowance.root_consumed, Some(1));
    assert_eq!(allowance.run_consumed, Some(0));
    assert!(allowance.run_cap > 0);
    assert_eq!(fixture_changes(&reopened.connection), before);
    assert_child_allowance_does_not_grant_root(&mut reopened, child_run_id);
    let summary = reopened
        .dispatch_pending_activations_for_run(&owner, child_run_id, 10, 3)
        .await
        .unwrap();
    assert_eq!(summary.exhausted.len(), 1);
    assert!(summary.admitted.is_empty());
    assert!(summary.raced.is_empty());
    assert_eq!(owner.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(fixture_changes(&reopened.connection) > before);
    let event = fixture_scalar_text(
        &reopened.connection,
        &select("workflow_events")
            .columns(&["payload_json"])
            .where_eq("run_id", child_run_id)
            .where_eq("event_type", "execution_allowance_exhausted"),
        "payload_json",
    )
    .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&event).unwrap(),
        serde_json::json!({"version":1,"additional_work_admitted":false})
    );
    let before_duplicate = fixture_changes(&reopened.connection);
    let duplicate = reopened
        .dispatch_pending_activations_for_run(&owner, child_run_id, 10, 4)
        .await
        .unwrap();
    assert_eq!(duplicate.exhausted, summary.exhausted);
    assert_eq!(fixture_changes(&reopened.connection), before_duplicate);
    assert_eq!(owner.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    let pending_before_restart = reopened
        .pending_activations_for_run(child_run_id, 10)
        .unwrap();
    let run_before_restart = reopened.run_summary(child_run_id).unwrap();
    drop(reopened);
    let mut reopened = WorkflowStore::open_at_path(path).unwrap();
    assert_eq!(
        reopened.run_summary(child_run_id).unwrap(),
        run_before_restart
    );
    let before_restart_dispatch = fixture_changes(&reopened.connection);
    let after_restart = reopened
        .dispatch_pending_activations_for_run(&owner, child_run_id, 10, 5)
        .await
        .unwrap();
    assert_eq!(after_restart.exhausted, summary.exhausted);
    assert!(after_restart.admitted.is_empty());
    assert_eq!(
        fixture_changes(&reopened.connection),
        before_restart_dispatch
    );
    assert_eq!(owner.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(
        reopened
            .pending_activations_for_run(child_run_id, 10)
            .unwrap(),
        pending_before_restart
    );
    assert!(
        reopened
            .attempt_history(child_run_id, None, 10)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        reopened
            .attempt_history("parent-run", None, 10)
            .unwrap()
            .len(),
        1
    );
    fixture_restore_root_attempt_budget(&reopened.connection, original_cap);
}

fn fixture_restore_root_attempt_budget(db: &dyn Database, original_cap: u64) {
    block_on(
        update("workflow_runs")
            .value("node_execution_cap", original_cap)
            .where_eq("run_id", "parent-run")
            .execute(db),
    )
    .unwrap();
}

fn fixture_exhaust_root_attempt_budget(db: &dyn Database) {
    block_on(
        update("workflow_runs")
            .value("node_execution_cap", 1)
            .where_eq("run_id", "parent-run")
            .execute(db),
    )
    .unwrap();
}

fn assert_child_allowance_does_not_grant_root(store: &mut WorkflowStore, child_run_id: &str) {
    // The composition fixture predates owned execution; give this child a verified
    // fixture owner before exercising the same fenced operation used for renewal.
    block_on(
        update("workflow_runs")
            .value("target_artifact_id", "artifact-a")
            .value("coordinator_daemon_instance_id", "daemon-a")
            .value("coordinator_generation", 1)
            .value("coordinator_fencing_token", "token-a")
            .where_eq("run_id", child_run_id)
            .execute(&store.connection),
    )
    .unwrap();
    let authority = store.execution_authority(child_run_id).unwrap().unwrap();
    let before = store.execution_allowance_observation(child_run_id).unwrap();
    store
        .increase_execution_allowance(
            child_run_id,
            &authority,
            before.run_cap,
            before.run_cap + 1,
            3,
        )
        .unwrap();
    let after = store.execution_allowance_observation(child_run_id).unwrap();
    assert_eq!(after.run_cap, before.run_cap + 1);
    assert_eq!(after.root_cap, before.root_cap);
    assert_eq!(after.root_consumed, before.root_consumed);
    assert_eq!(after.run_consumed, before.run_consumed);
    assert_eq!(after.exhausted(), Some(true));
    // Leave the increased cap installed: retrying the original child admission
    // must compare against its original allowance, not the renewed execution cap.
    block_on(
        update("workflow_runs")
            .value("target_artifact_id", DatabaseValue::Null)
            .value("coordinator_daemon_instance_id", DatabaseValue::Null)
            .value("coordinator_generation", DatabaseValue::Null)
            .value("coordinator_fencing_token", DatabaseValue::Null)
            .where_eq("run_id", child_run_id)
            .execute(&store.connection),
    )
    .unwrap();
}

fn assert_inherited_child_cancellation(store: &mut WorkflowStore, child_run_id: &str) {
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("test snapshot");
    block_on(
        update("workflow_runs")
            .value("target_artifact_id", "artifact-a")
            .value("coordinator_daemon_instance_id", "daemon-a")
            .value("coordinator_generation", 1)
            .value("coordinator_fencing_token", "token-a")
            .where_eq("run_id", child_run_id)
            .execute(transaction.as_ref()),
    )
    .expect("owner");
    let authority = execution_authority_in(transaction.as_ref(), child_run_id)
        .expect("authority")
        .expect("owner");
    block_on(
        update("workflow_runs")
            .value("cancellation_requested_at_ms", 3)
            .where_eq("run_id", "parent-run")
            .execute(transaction.as_ref()),
    )
    .expect("parent intent");
    // Release the fixture transaction before the operation's own commit boundary.
    database::commit(transaction).expect("fixture commit");
    block_on(
        update("workflow_runs")
            .value("status", "paused")
            .where_eq("run_id", child_run_id)
            .execute(&store.connection),
    )
    .expect("paused child");
    assert!(
        store
            .needs_continuation(child_run_id)
            .expect("discover paused cancellation")
    );
    let mut restarted = WorkflowStore::open_at_path(store.path()).expect("reopen child store");
    let mut foreign_authority = authority.clone();
    foreign_authority.daemon_instance_id = "foreign-daemon".to_string();
    assert!(
        restarted
            .inherit_parent_cancellation_owned(child_run_id, &foreign_authority, 4)
            .is_err()
    );
    assert!(
        restarted
            .run_summary(child_run_id)
            .expect("summary")
            .expect("child")
            .cancellation_requested_at_ms
            .is_none()
    );
    assert!(
        restarted
            .inherit_parent_cancellation_owned(child_run_id, &authority, 4)
            .expect("inherit after reopen")
    );
    assert!(
        !store
            .inherit_parent_cancellation_owned(child_run_id, &authority, 4)
            .expect("duplicate")
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn exact_child_run_creation_is_atomic_idempotent_bounded_and_restart_safe() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    let child_definition = definition("child");
    let target_identity =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("child", &child_definition)
            .expect("identity");
    let parent_definition = workflow_call_definition(target_identity.clone());
    store
        .persist_definition("parent", 1, &parent_definition)
        .expect("parent definition");
    store
        .persist_definition(
            &target_identity.definition_id,
            target_identity.definition_version,
            &child_definition,
        )
        .expect("child definition");
    store
        .create_run(&NewWorkflowRun {
            run_id: "parent-run".to_string(),
            definition_id: "parent".to_string(),
            definition_version: 1,
            workspace_snapshot: "snapshot".to_string(),
            parent_session_id: None,
            parent_session_generation: None,
            binding: None,
            authored_provenance: None,
            input: Some(serde_json::json!(1)),
            execution_authority: None,
            created_at_ms: 1,
            authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                version: 1,
                provider_id: "test-policy".to_string(),
                profile_id: "build".to_string(),
                policy_digest_sha256: "a".repeat(64),
            },
            authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
            limits: WorkflowRunLimits::default(),
        })
        .expect("parent run");
    let activation = store
        .pending_activations(10)
        .expect("activation")
        .pop()
        .expect("activation");
    store
        .prepare_pending_activation(
            "parent-run",
            "call",
            &activation.activation_id,
            DispatchSideEffect::ReadOnly,
            serde_json::json!({"target": target_identity}),
            2,
        )
        .expect("prepared");
    let target = bcode_workflow::WorkflowCallTarget::Definition {
        identity: target_identity.clone(),
    };
    let child_run_id = workflow_child_run_id(
        "parent-run",
        "parent-run",
        &activation.activation_id,
        1,
        &target,
    );
    let request = NewChildWorkflowRun {
        link: WorkflowRunLink {
            version: WORKFLOW_RUN_LINK_VERSION,
            root_run_id: "parent-run".to_string(),
            parent_run_id: "parent-run".to_string(),
            parent_node_id: "call".to_string(),
            parent_activation_id: activation.activation_id,
            parent_attempt: 1,
            child_run_id: child_run_id.clone(),
            target,
            depth: 2,
            created_at_ms: 3,
        },
        run: NewWorkflowRun {
            run_id: child_run_id.clone(),
            definition_id: target_identity.definition_id,
            definition_version: target_identity.definition_version,
            workspace_snapshot: "snapshot".to_string(),
            parent_session_id: None,
            parent_session_generation: None,
            binding: None,
            authored_provenance: None,
            input: Some(serde_json::json!(1)),
            execution_authority: None,
            created_at_ms: 3,
            authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                version: 1,
                provider_id: "test-policy".to_string(),
                profile_id: "build".to_string(),
                policy_digest_sha256: "a".repeat(64),
            },
            authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
            limits: WorkflowRunLimits::default(),
        },
    };
    let mut foreign_owner = request.clone();
    foreign_owner.run.execution_authority = Some(WorkflowExecutionAuthority {
        target_artifact_id: "foreign-artifact".into(),
        daemon_instance_id: "foreign-daemon".into(),
        generation: 1,
        fencing_token: "foreign-token".into(),
    });
    assert!(store.create_child_run_idempotent(&foreign_owner).is_err());
    assert!(store.run_summary(&request.run.run_id).unwrap().is_none());
    {
        let transaction = database::begin(&store.connection, TransactionMode::Deferred)
            .expect("ancestry snapshot");
        validate_child_ancestry(transaction.as_ref(), &request.link)
            .expect("valid ownership chain");
        let mut invalid = request.link.clone();
        invalid.root_run_id = "unrelated-root".to_string();
        assert!(validate_child_ancestry(transaction.as_ref(), &invalid).is_err());
        invalid = request.link.clone();
        invalid.depth += 1;
        assert!(validate_child_ancestry(transaction.as_ref(), &invalid).is_err());
        invalid = request.link.clone();
        invalid.child_run_id.clone_from(&invalid.parent_run_id);
        assert!(validate_child_ancestry(transaction.as_ref(), &invalid).is_err());
        block_on(transaction.rollback()).expect("snapshot end");
    }
    let mut mismatched_profile = request.clone();
    mismatched_profile.run.run_id = workflow_child_run_id(
        "parent-run",
        "parent-run",
        &mismatched_profile.link.parent_activation_id,
        1,
        &mismatched_profile.link.target,
    );
    mismatched_profile
        .run
        .authorization_profile
        .policy_digest_sha256 = "b".repeat(64);
    assert!(
        store
            .create_child_run_idempotent(&mismatched_profile)
            .is_err()
    );
    assert!(
        store
            .create_child_run_idempotent(&request)
            .expect("created")
    );
    assert_root_attempt_budget_survives_reopen(&store, &child_run_id);
    assert_root_exhaustion_defers_child_dispatch(store.path(), &child_run_id).await;
    let attempts = store
        .active_attempts_for_run("parent-run", 10)
        .expect("attempts");
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].receipt["child_run_id"], child_run_id);
    store
        .persist_dispatch_receipt(&DispatchReceipt {
            run_id: "parent-run".to_string(),
            node_id: "call".to_string(),
            activation_id: request.link.parent_activation_id.clone(),
            attempt: 1,
            dispatch_identity: dispatch_identity(
                "parent-run",
                "call",
                &request.link.parent_activation_id,
                1,
            ),
            receipt: attempts[0].receipt.clone(),
            admitted_at_ms: 3,
        })
        .expect("scheduler receipt redelivery is idempotent");
    assert!(
        !store
            .create_child_run_idempotent(&request)
            .expect("duplicate")
    );
    assert_eq!(
        store.child_run_links("parent-run", 10).expect("links"),
        std::slice::from_ref(&request.link)
    );
    let receipt = store
        .active_attempts_for_run("parent-run", 10)
        .expect("attempt")
        .pop()
        .expect("attempt");
    let admitted_call = store
        .run_graph_node("parent-run", "call")
        .expect("graph")
        .expect("call");
    let before = fixture_changes(&store.connection);
    assert!(matches!(
        store
            .observe_child_attempt(&receipt, 4)
            .expect("running child"),
        AttemptObservation::Running
    ));
    assert_eq!(fixture_changes(&store.connection), before);
    for (table, column, value) in [
        (
            "workflow_run_graph_nodes",
            "node_json",
            DatabaseValue::String("{}".into()),
        ),
        ("workflow_run_graphs", "revision", DatabaseValue::Int64(2)),
        (
            "workflow_activations",
            "node_revision",
            DatabaseValue::Int64(2),
        ),
    ] {
        let query = update(table)
            .value(column, value)
            .where_eq("run_id", "parent-run");
        let query = if table == "workflow_run_graphs" {
            query
        } else {
            query.where_eq("node_id", "call")
        };
        block_on(query.execute(&store.connection)).expect("damage fixture");
        let before = fixture_changes(&store.connection);
        assert!(store.observe_child_attempt(&receipt, 4).is_err());
        assert_eq!(fixture_changes(&store.connection), before);
        block_on(
            update("workflow_run_graph_nodes")
                .value(
                    "node_json",
                    serde_json::to_string(&admitted_call).expect("node"),
                )
                .where_eq("run_id", "parent-run")
                .where_eq("node_id", "call")
                .execute(&store.connection),
        )
        .expect("restore fixture");
        block_on(async {
            update("workflow_run_graphs")
                .value("revision", 1)
                .where_eq("run_id", "parent-run")
                .execute(&store.connection)
                .await?;
            update("workflow_activations")
                .value("node_revision", 1)
                .where_eq("run_id", "parent-run")
                .where_eq("node_id", "call")
                .execute(&store.connection)
                .await?;
            Ok::<(), switchy_database::DatabaseError>(())
        })
        .expect("restore revision");
    }
    let mut wrong_kind = admitted_call.clone();
    wrong_kind.kind = bcode_workflow::NodeKind::Task;
    let mut wrong_target = admitted_call.clone();
    let mut configuration: bcode_workflow::WorkflowCallConfiguration =
        serde_json::from_value(wrong_target.configuration.clone()).expect("call");
    configuration.target = bcode_workflow::WorkflowCallTarget::Definition {
        identity: bcode_workflow::WorkflowDefinitionIdentity::for_definition(
            "different-child",
            &definition("different-child"),
        )
        .expect("identity"),
    };
    wrong_target.configuration = serde_json::to_value(configuration).expect("call");
    for (node, diagnostic) in [
        (wrong_kind, "not an admitted workflow call"),
        (wrong_target, "conflicts with admitted call target"),
    ] {
        block_on(
            update("workflow_run_graph_nodes")
                .value("node_json", serde_json::to_string(&node).expect("node"))
                .where_eq("run_id", "parent-run")
                .where_eq("node_id", "call")
                .execute(&store.connection),
        )
        .expect("inconsistent call fixture");
        let before = fixture_changes(&store.connection);
        assert!(
            store
                .observe_child_attempt(&receipt, 4)
                .expect_err("inconsistent call")
                .to_string()
                .contains(diagnostic)
        );
        assert_eq!(fixture_changes(&store.connection), before);
    }
    block_on(
        update("workflow_run_graph_nodes")
            .value(
                "node_json",
                serde_json::to_string(&admitted_call).expect("node"),
            )
            .where_eq("run_id", "parent-run")
            .where_eq("node_id", "call")
            .execute(&store.connection),
    )
    .expect("restore call fixture");
    let mut changed_source = parent_definition.clone();
    changed_source.nodes.clear();
    let source_json = serde_json::to_string(&changed_source).expect("source");
    block_on(
        update("workflow_definitions")
            .value("definition_json", source_json.as_str())
            .value("checksum_sha256", sha256_hex(source_json.as_bytes()))
            .where_eq("definition_id", "parent")
            .execute(&store.connection),
    )
    .expect("source fixture");
    drop(store);
    let mut store =
        WorkflowStore::open_in_state_dir(temp.path()).expect("reopen before observation");
    assert!(matches!(
        store
            .observe_child_attempt(&receipt, 4)
            .expect("admitted call"),
        AttemptObservation::Running
    ));
    // Restore source for the cancellation paths not yet moved to the graph.
    let source_json = serde_json::to_string(&parent_definition).expect("source");
    block_on(
        update("workflow_definitions")
            .value("definition_json", source_json.as_str())
            .value("checksum_sha256", sha256_hex(source_json.as_bytes()))
            .where_eq("definition_id", "parent")
            .execute(&store.connection),
    )
    .expect("restore source fixture");
    assert_inherited_child_cancellation(&mut store, &child_run_id);
    let (recorded, cancelled_run_ids) = store
        .request_cancellation_tree("parent-run", 4)
        .expect("cancel parent tree");
    assert!(!recorded, "parent and child intent already inherited");
    assert_eq!(cancelled_run_ids, ["parent-run"]);
    let running = store
        .observe_child_attempt(&receipt, 5)
        .expect("observe cancelled child");
    let summary = store
        .apply_attempt_observation(&receipt.dispatch_identity, running, 5)
        .expect("settle parent cancellation");
    assert_eq!(summary.cancelled, [receipt.dispatch_identity]);
    assert_eq!(
        store
            .run_summary("parent-run")
            .expect("parent")
            .expect("parent")
            .status,
        RunStatus::Cancelled
    );
    drop(store);
    let reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
    assert_eq!(
        reopened.child_run_links("parent-run", 10).expect("links"),
        std::slice::from_ref(&request.link)
    );
    assert_eq!(
        reopened
            .descendant_run_summaries("parent-run", 10)
            .expect("descendant status"),
        [WorkflowDescendantRunSummary {
            link: request.link,
            run: reopened
                .run_summary(&child_run_id)
                .expect("child")
                .expect("child"),
        }]
    );
    assert!(
        reopened
            .run_summary(&child_run_id)
            .expect("child")
            .is_some()
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn cancellation_tree_omits_already_terminal_descendants() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    let child_definition = definition("terminal-child");
    let target = bcode_workflow::WorkflowDefinitionIdentity::for_definition(
        "terminal-child",
        &child_definition,
    )
    .expect("identity");
    let parent_definition = workflow_call_definition(target.clone());
    store
        .persist_definition("terminal-parent", 1, &parent_definition)
        .expect("parent definition");
    store
        .persist_definition(
            &target.definition_id,
            target.definition_version,
            &child_definition,
        )
        .expect("child definition");
    let mut parent = new_run();
    parent.run_id = "terminal-parent-run".to_string();
    parent.definition_id = "terminal-parent".to_string();
    parent.input = Some(serde_json::json!(1));
    store.create_run(&parent).expect("parent run");
    let pending = store
        .pending_activations_for_run(&parent.run_id, 10)
        .expect("pending")
        .pop()
        .expect("parent activation");
    store
        .prepare_pending_activation(
            &parent.run_id,
            &pending.node_id,
            &pending.activation_id,
            DispatchSideEffect::ReadOnly,
            serde_json::json!({"target": target}),
            2,
        )
        .expect("prepare parent attempt");
    let child_run_id = workflow_child_run_id(
        &parent.run_id,
        &parent.run_id,
        &pending.activation_id,
        1,
        &bcode_workflow::WorkflowCallTarget::Definition {
            identity: target.clone(),
        },
    );
    let link = WorkflowRunLink {
        version: WORKFLOW_RUN_LINK_VERSION,
        root_run_id: parent.run_id.clone(),
        parent_run_id: parent.run_id.clone(),
        parent_node_id: pending.node.id.clone(),
        parent_activation_id: pending.activation_id.clone(),
        parent_attempt: 1,
        child_run_id: child_run_id.clone(),
        depth: 2,
        target: bcode_workflow::WorkflowCallTarget::Definition {
            identity: target.clone(),
        },
        created_at_ms: 2,
    };
    store
        .create_child_run_idempotent(&NewChildWorkflowRun {
            link,
            run: NewWorkflowRun {
                run_id: child_run_id.clone(),
                definition_id: target.definition_id.clone(),
                definition_version: target.definition_version,
                workspace_snapshot: parent.workspace_snapshot.clone(),
                parent_session_id: parent.parent_session_id.clone(),
                parent_session_generation: None,
                binding: None,
                authored_provenance: None,
                input: Some(serde_json::json!(1)),
                execution_authority: None,
                created_at_ms: 2,
                authorization_profile: parent.authorization_profile.clone(),
                authorization_ceiling: parent.authorization_ceiling,
                limits: parent.limits.clone(),
            },
        })
        .expect("child run");
    block_on(
        update("workflow_runs")
            .value("status", "completed")
            .where_eq("run_id", child_run_id.as_str())
            .execute(&store.connection),
    )
    .expect("complete child");

    let (recorded, cancelled_run_ids) = store
        .request_cancellation_tree(&parent.run_id, 3)
        .expect("cancel parent tree");
    assert!(recorded);
    assert_eq!(cancelled_run_ids, [parent.run_id]);
    assert_eq!(
        store
            .run_summary(&child_run_id)
            .expect("child")
            .expect("child")
            .status,
        RunStatus::Completed
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn flagship_restart_preserves_wait_approval_repeat_and_composed_status() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    let child_definition = definition("batch");
    let child_identity = bcode_workflow::WorkflowDefinitionIdentity::for_definition(
        "implementation-batch",
        &child_definition,
    )
    .expect("identity");
    let parent_definition = workflow_call_definition(child_identity.clone());
    store
        .persist_definition("delivery-tranche", 1, &parent_definition)
        .expect("parent definition");
    store
        .persist_definition(
            &child_identity.definition_id,
            child_identity.definition_version,
            &child_definition,
        )
        .expect("child definition");
    store
        .create_run(&NewWorkflowRun {
            run_id: "flagship-root".to_string(),
            definition_id: "delivery-tranche".to_string(),
            definition_version: 1,
            workspace_snapshot: "snapshot".to_string(),
            parent_session_id: None,
            parent_session_generation: None,
            binding: None,
            authored_provenance: None,
            input: Some(serde_json::json!(1)),
            execution_authority: None,
            created_at_ms: 1,
            authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                version: 1,
                provider_id: "test-policy".to_string(),
                profile_id: "build".to_string(),
                policy_digest_sha256: "a".repeat(64),
            },
            authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
            limits: WorkflowRunLimits::default(),
        })
        .expect("root");
    let activation = store
        .pending_activations(10)
        .expect("activation")
        .pop()
        .expect("activation");
    store
        .prepare_pending_activation(
            "flagship-root",
            "call",
            &activation.activation_id,
            DispatchSideEffect::ReadOnly,
            serde_json::json!({"target": child_identity}),
            2,
        )
        .expect("prepared");
    let target = bcode_workflow::WorkflowCallTarget::Definition {
        identity: child_identity,
    };
    let child_run_id = workflow_child_run_id(
        "flagship-root",
        "flagship-root",
        &activation.activation_id,
        1,
        &target,
    );
    let link = WorkflowRunLink {
        version: WORKFLOW_RUN_LINK_VERSION,
        root_run_id: "flagship-root".to_string(),
        parent_run_id: "flagship-root".to_string(),
        parent_node_id: activation.node_id,
        parent_activation_id: activation.activation_id,
        parent_attempt: 1,
        child_run_id: child_run_id.clone(),
        target,
        depth: 2,
        created_at_ms: 2,
    };
    store
        .create_child_run_idempotent(&NewChildWorkflowRun {
            link: link.clone(),
            run: NewWorkflowRun {
                run_id: child_run_id.clone(),
                definition_id: link
                    .target
                    .definition_identity()
                    .unwrap()
                    .definition_id
                    .clone(),
                definition_version: link
                    .target
                    .definition_identity()
                    .unwrap()
                    .definition_version,
                workspace_snapshot: "snapshot".to_string(),
                parent_session_id: None,
                parent_session_generation: None,
                binding: None,
                authored_provenance: None,
                input: Some(serde_json::json!(1)),
                execution_authority: None,
                created_at_ms: 2,
                authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                    version: 1,
                    provider_id: "test-policy".to_string(),
                    profile_id: "build".to_string(),
                    policy_digest_sha256: "a".repeat(64),
                },
                authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
                limits: WorkflowRunLimits::default(),
            },
        })
        .expect("child");
    drop(store);

    let reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
    let descendants = reopened
        .descendant_run_summaries("flagship-root", 64)
        .expect("bounded hierarchy");
    assert_eq!(descendants.len(), 1);
    assert_eq!(descendants[0].link, link);
    assert_eq!(descendants[0].run.run_id, child_run_id);
    assert_eq!(descendants[0].run.status, RunStatus::Running);
}

#[test]
#[allow(clippy::too_many_lines)]
fn child_run_failure_rolls_back_child_and_link() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    let child_definition = definition("child");
    let identity =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("child", &child_definition)
            .expect("identity");
    let parent_definition = workflow_call_definition(identity.clone());
    store
        .persist_definition("parent", 1, &parent_definition)
        .expect("parent");
    store
        .persist_definition(
            &identity.definition_id,
            identity.definition_version,
            &child_definition,
        )
        .expect("child");
    store
        .create_run(&NewWorkflowRun {
            run_id: "rollback-parent".to_string(),
            definition_id: "parent".to_string(),
            definition_version: 1,
            workspace_snapshot: "snapshot".to_string(),
            parent_session_id: None,
            parent_session_generation: None,
            binding: None,
            authored_provenance: None,
            input: Some(serde_json::json!(1)),
            execution_authority: None,
            created_at_ms: 1,
            authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                version: 1,
                provider_id: "test-policy".to_string(),
                profile_id: "build".to_string(),
                policy_digest_sha256: "a".repeat(64),
            },
            authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
            limits: WorkflowRunLimits::default(),
        })
        .expect("parent run");
    let activation = store
        .pending_activations(10)
        .expect("pending")
        .pop()
        .expect("call");
    store
        .prepare_pending_activation(
            "rollback-parent",
            "call",
            &activation.activation_id,
            DispatchSideEffect::ReadOnly,
            serde_json::json!({}),
            2,
        )
        .expect("prepare");
    let target = bcode_workflow::WorkflowCallTarget::Definition {
        identity: identity.clone(),
    };
    let child_id = workflow_child_run_id(
        "rollback-parent",
        "rollback-parent",
        &activation.activation_id,
        1,
        &target,
    );
    let mut request = NewChildWorkflowRun {
        link: WorkflowRunLink {
            version: WORKFLOW_RUN_LINK_VERSION,
            root_run_id: "rollback-parent".to_string(),
            parent_run_id: "rollback-parent".to_string(),
            parent_node_id: "call".to_string(),
            parent_activation_id: activation.activation_id,
            parent_attempt: 1,
            child_run_id: child_id.clone(),
            target,
            depth: 2,
            created_at_ms: 3,
        },
        run: NewWorkflowRun {
            run_id: child_id.clone(),
            definition_id: identity.definition_id,
            definition_version: identity.definition_version,
            workspace_snapshot: "wrong".to_string(),
            parent_session_id: None,
            parent_session_generation: None,
            binding: None,
            authored_provenance: None,
            input: Some(serde_json::json!(1)),
            execution_authority: None,
            created_at_ms: 3,
            authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                version: 1,
                provider_id: "test-policy".to_string(),
                profile_id: "build".to_string(),
                policy_digest_sha256: "a".repeat(64),
            },
            authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
            limits: WorkflowRunLimits::default(),
        },
    };
    assert!(store.create_child_run_idempotent(&request).is_err());
    assert!(store.run_summary(&child_id).expect("child").is_none());
    assert!(
        store
            .child_run_links("rollback-parent", 10)
            .expect("links")
            .is_empty()
    );
    request.run.workspace_snapshot = "snapshot".to_string();
    request.run.limits.node_execution_cap += 1;
    assert!(
        store.create_child_run_idempotent(&request).is_err(),
        "child limits must not exceed the inherited parent envelope"
    );
    request.run.limits = WorkflowRunLimits::default();
    block_on(
        update("workflow_activations")
            .value("node_revision", 2)
            .where_eq("run_id", "rollback-parent")
            .execute(&store.connection),
    )
    .expect("invalid parent binding");
    let before = fixture_changes(&store.connection);
    assert!(store.create_child_run_idempotent(&request).is_err());
    assert_eq!(fixture_changes(&store.connection), before);
    assert!(
        store
            .run_summary(&child_id)
            .expect("no child on binding error")
            .is_none()
    );
    block_on(
        update("workflow_activations")
            .value("node_revision", 1)
            .where_eq("run_id", "rollback-parent")
            .execute(&store.connection),
    )
    .expect("restore parent binding");
    request.link.depth = MAX_WORKFLOW_RUN_DEPTH + 1;
    assert!(store.create_child_run_idempotent(&request).is_err());
    assert!(store.run_summary(&child_id).expect("child").is_none());
    request.link.depth = 2;
    block_on(
        update("workflow_runs")
            .value("cancellation_requested_at_ms", 3)
            .where_eq("run_id", "rollback-parent")
            .execute(&store.connection),
    )
    .expect("cancel parent fixture");
    let before = fixture_changes(&store.connection);
    assert!(store.create_child_run_idempotent(&request).is_err());
    assert_eq!(fixture_changes(&store.connection), before);
    assert!(
        store
            .run_summary(&child_id)
            .expect("no child after cancellation")
            .is_none()
    );
    block_on(
        update("workflow_runs")
            .value("cancellation_requested_at_ms", DatabaseValue::Null)
            .where_eq("run_id", "rollback-parent")
            .execute(&store.connection),
    )
    .expect("restore parent fixture");
    let original_profile =
        serde_json::to_string(&request.run.authorization_profile).expect("authorization profile");
    block_on(
        update("workflow_runs")
            .value(
                "authorization_profile_json",
                format!("{original_profile}{}", " ".repeat(MAX_INLINE_JSON_BYTES)),
            )
            .where_eq("run_id", "rollback-parent")
            .execute(&store.connection),
    )
    .expect("oversized authorization fixture");
    let before = fixture_changes(&store.connection);
    assert!(matches!(
        store.create_child_run_idempotent(&request),
        Err(WorkflowStoreError::InvalidData(_))
    ));
    assert_eq!(fixture_changes(&store.connection), before);
    assert!(
        store
            .run_summary(&child_id)
            .expect("no child on oversized authorization")
            .is_none()
    );
    block_on(
        update("workflow_runs")
            .value("authorization_profile_json", original_profile)
            .where_eq("run_id", "rollback-parent")
            .execute(&store.connection),
    )
    .expect("restore authorization fixture");
    for definition_id in ["parent", request.run.definition_id.as_str()] {
        let original = fixture_scalar_text(
            &store.connection,
            &select("workflow_definitions")
                .columns(&["definition_json"])
                .where_eq("definition_id", definition_id),
            "definition_json",
        )
        .expect("original definition");
        let oversized = format!("{original}{}", " ".repeat(MAX_INLINE_JSON_BYTES));
        block_on(
            update("workflow_definitions")
                .value("definition_json", oversized.as_str())
                .where_eq("definition_id", definition_id)
                .execute(&store.connection),
        )
        .expect("oversized valid JSON fixture");
        let before = fixture_changes(&store.connection);
        assert!(
            matches!(
                store.create_child_run_idempotent(&request),
                Err(WorkflowStoreError::InvalidData(_))
            ),
            "oversized definition must be rejected at the query boundary"
        );
        assert_eq!(fixture_changes(&store.connection), before);
        assert!(
            store
                .run_summary(&child_id)
                .expect("no child on oversized definition")
                .is_none()
        );
        block_on(
            update("workflow_definitions")
                .value("definition_json", original)
                .where_eq("definition_id", definition_id)
                .execute(&store.connection),
        )
        .expect("restore definition fixture");
    }
    block_on(
        insert("workflow_recovery_barriers")
            .value("run_id", request.link.parent_run_id.as_str())
            .value("source_artifact_id", "original-artifact")
            .value("created_at_ms", 1)
            .execute(&store.connection),
    )
    .expect("recovery barrier");
    let before = fixture_changes(&store.connection);
    assert!(store.create_child_run_idempotent(&request).is_err());
    assert_eq!(fixture_changes(&store.connection), before);
    assert!(store.run_summary(&child_id).expect("lookup").is_none());
    block_on(
        delete("workflow_recovery_barriers")
            .where_eq("run_id", request.link.parent_run_id.as_str())
            .execute(&store.connection),
    )
    .expect("clear test barrier");
    assert!(
        store
            .create_child_run_idempotent(&request)
            .expect("valid binding admits child")
    );
    assert!(
        !store
            .create_child_run_idempotent(&request)
            .expect("exact duplicate")
    );
    request.run.input = Some(serde_json::json!(2));
    let before = fixture_changes(&store.connection);
    assert!(store.create_child_run_idempotent(&request).is_err());
    assert_eq!(fixture_changes(&store.connection), before);
}

#[test]
fn terminal_output_survives_restart_without_duplicate_materialization() {
    let (temp, mut store) = initialized_store();
    let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::ReadOnly);
    let output = ValidatedOutput {
        output_id: "restart-output".to_string(),
        run_id: "run-1".to_string(),
        node_id: "review".to_string(),
        activation_id: activation_id(),
        schema_id: "u32".to_string(),
        schema_version: 1,
        value: serde_json::json!(7),
        artifact_reference: None,
        created_at_ms: 20,
    };
    store
        .apply_attempt_observation(&identity, AttemptObservation::Succeeded { output }, 20)
        .expect("success");
    drop(store);
    let mut reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
    assert_eq!(
        reopened
            .output_summaries("run-1", 10)
            .expect("outputs")
            .len(),
        1
    );
    assert_eq!(
        reopened
            .run_summary("run-1")
            .expect("summary")
            .expect("run")
            .status,
        RunStatus::Completed
    );
    let terminal = reopened
        .run_summary("run-1")
        .expect("summary")
        .expect("run");
    assert_eq!(
        terminal.terminal_output_id.as_deref(),
        Some("restart-output")
    );
    let expected_terminal_checksum = sha256_hex(b"7");
    assert_eq!(
        terminal.terminal_output_checksum_sha256.as_deref(),
        Some(expected_terminal_checksum.as_str())
    );
    assert!(
        reopened
            .apply_attempt_observation(&identity, AttemptObservation::Unknown, 21,)
            .is_err()
    );
    assert_eq!(
        reopened
            .output_summaries("run-1", 10)
            .expect("outputs")
            .len(),
        1
    );
}

#[test]
fn terminal_run_rejects_resume_with_typed_transition_error() {
    let (_temp, mut store) = initialized_store();
    block_on(
        update("workflow_runs")
            .value("status", "completed")
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("complete run");

    let error = store
        .resume_run("run-1", 20)
        .expect_err("completed run must not resume");
    assert!(matches!(
        error,
        WorkflowStoreError::InvalidRunTransition {
            current: RunStatus::Completed,
            target: RunStatus::Running,
        }
    ));
}

#[test]
fn missing_run_control_uses_typed_not_found_error() {
    let (_temp, mut store) = initialized_store();
    let error = store
        .pause_run("missing-run", 20)
        .expect_err("missing run must fail");
    assert!(matches!(
        error,
        WorkflowStoreError::RunNotFound { run_id } if run_id == "missing-run"
    ));
}

#[test]
fn pause_and_resume_are_durable_idempotent_admission_gates() {
    let (_temp, mut store) = initialized_store();
    assert!(store.pause_run("run-1", 20).expect("pause"));
    assert!(!store.pause_run("run-1", 21).expect("idempotent pause"));
    assert_eq!(
        store
            .run_summary("run-1")
            .expect("summary")
            .expect("run")
            .status,
        RunStatus::Paused
    );
    assert!(
        store
            .create_activation(&NewActivation {
                node_id: "later".to_string(),
                activation_id: "activation-2".to_string(),
                created_at_ms: 22,
                ..new_activation()
            })
            .is_err()
    );
    assert!(
        store
            .prepare_attempt(&PreparedAttempt {
                run_id: "run-1".to_string(),
                node_id: "review".to_string(),
                activation_id: activation_id(),
                attempt: 1,
                side_effect: DispatchSideEffect::ReadOnly,
                intent: serde_json::json!({}),
                prepared_at_ms: 22,
            })
            .is_err()
    );
    assert!(store.resume_run("run-1", 23).expect("resume"));
    assert!(!store.resume_run("run-1", 24).expect("idempotent resume"));
    store
        .prepare_attempt(&PreparedAttempt {
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: activation_id(),
            attempt: 1,
            side_effect: DispatchSideEffect::ReadOnly,
            intent: serde_json::json!({}),
            prepared_at_ms: 25,
        })
        .expect("admission after resume");
    store.request_cancellation("run-1", 26).expect("cancel");
    assert!(store.pause_run("run-1", 27).is_err());
}

#[test]
fn cancellation_intent_is_persisted_before_further_admission() {
    let (_temp, mut store) = initialized_store();
    assert!(store.request_cancellation("run-1", 20).expect("first"));
    assert!(!store.request_cancellation("run-1", 21).expect("idempotent"));
    let summary = store.run_summary("run-1").expect("summary").expect("run");
    assert_eq!(summary.cancellation_requested_at_ms, Some(20));
    let error = store
        .prepare_attempt(&PreparedAttempt {
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: activation_id(),
            attempt: 1,
            side_effect: DispatchSideEffect::ReadOnly,
            intent: serde_json::json!({}),
            prepared_at_ms: 22,
        })
        .expect_err("cancelled run rejects admission");
    assert!(error.to_string().contains("cancelled"));
    assert_eq!(summary.status, RunStatus::Cancelled);
    assert_eq!(
        store
            .active_attempts_for_cancellation("run-1", 10)
            .expect("active children"),
        Vec::<String>::new()
    );
}

fn create_local_publication_target(
    store: &mut WorkflowStore,
    run_id: &str,
    authority: &WorkflowExecutionAuthority,
) {
    store
        .create_activation_at_graph_revision(
            &NewActivation {
                run_id: run_id.into(),
                node_id: "second".into(),
                activation_id: activation_identity(run_id, "second", 0),
                dependency_generation: 0,
                input: Some(serde_json::json!(1)),
                created_at_ms: 25,
            },
            2,
            authority,
        )
        .expect("local pending work");
}

fn exercise_pending_publication(conflict: bool) {
    let (_temp, mut store, run, authority, _) = connected_publication_fixture();
    let id = activation_identity(&run.run_id, "first", 0);
    let prepared = store
        .prepare_pending_activation(
            &run.run_id,
            "first",
            &id,
            DispatchSideEffect::Mutating,
            serde_json::json!({}),
            25,
        )
        .expect("prepare")
        .expect("pending");
    let local_id = activation_identity(&run.run_id, "second", 0);
    if !conflict {
        create_local_publication_target(&mut store, &run.run_id, &authority);
    }
    let mut request = bcode_workflow::WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: run.run_id.clone(),
        mutation_id: "pending-cancel".into(),
        expected_revision: 2,
        edits: vec![bcode_workflow::WorkflowRunGraphEdit::RemoveEdge { edge_id: 0 }],
        reconciliation: vec![
            bcode_workflow::WorkflowRunGraphReconciliation::Cancel {
                activation_id: id.clone(),
            },
            bcode_workflow::WorkflowRunGraphReconciliation::Cancel {
                activation_id: local_id,
            },
        ],
    };
    if conflict {
        request.reconciliation.pop();
    }
    store
        .stage_run_graph_edit(&request, &authority, 26)
        .expect("stage");
    assert!(
        store
            .accept_pending_run_graph_publication(&run.run_id, &request.mutation_id, &authority, 27)
            .is_err()
    );
    let count: u64 = fixture_scalar_unsigned(
        &store.connection,
        &select("workflow_pending_publications")
            .project_as(switchy_database::query::count_all(), "count"),
        "count",
    )
    .expect("count");
    assert_eq!(count, 0);
    store
        .persist_dispatch_receipt(&DispatchReceipt {
            run_id: run.run_id.clone(),
            node_id: "first".into(),
            activation_id: id,
            attempt: prepared.attempt,
            dispatch_identity: prepared.dispatch_identity.clone(),
            receipt: serde_json::json!({"accepted": true}),
            admitted_at_ms: 28,
        })
        .expect("receipt");
    assert!(
        store
            .accept_pending_run_graph_publication(&run.run_id, &request.mutation_id, &authority, 29)
            .expect("accept")
    );
    assert!(
        !store
            .accept_pending_run_graph_publication(&run.run_id, &request.mutation_id, &authority, 30)
            .expect("duplicate")
    );
    assert_eq!(
        store.run_graph_revision(&run.run_id).expect("revision"),
        Some(2)
    );
    assert_pending_publication_finalization(
        &mut store,
        &run,
        &request,
        &authority,
        prepared.dispatch_identity,
        conflict,
    );
}

fn assert_pending_publication_finalization(
    store: &mut WorkflowStore,
    run: &NewWorkflowRun,
    request: &bcode_workflow::WorkflowRunGraphEditBatch,
    authority: &WorkflowExecutionAuthority,
    identity: String,
    conflict: bool,
) {
    use bcode_workflow::WorkflowRunGraphPublicationStatus as Status;
    let page = store
        .accepted_run_graph_publication_page(&run.run_id, authority, None, 1)
        .expect("discover accepted publication");
    assert_eq!(page, std::slice::from_ref(&request.mutation_id));
    assert!(
        store
            .accepted_run_graph_publication_page(
                &run.run_id,
                authority,
                Some(&request.mutation_id),
                1
            )
            .expect("end of page")
            .is_empty()
    );
    let mut stale = authority.clone();
    stale.generation += 1;
    assert!(
        store
            .accepted_run_graph_publication_page(&run.run_id, &stale, None, 1)
            .is_err()
    );
    assert_eq!(
        store
            .finalize_pending_run_graph_publication(
                &run.run_id,
                &request.mutation_id,
                authority,
                30
            )
            .expect("not settled"),
        Status::Pending {
            expected_revision: 2
        }
    );
    if conflict {
        assert_publication_status_conflict(store, &run.run_id, authority);
    }
    assert_publication_cancellation_settlement(store, &run.run_id, authority, identity);
    let expected = if conflict {
        Status::Conflicted {
            expected_revision: 2,
            current_revision: 3,
        }
    } else {
        Status::Committed { revision: 3 }
    };
    for _ in 0..2 {
        assert_eq!(
            store
                .finalize_pending_run_graph_publication(
                    &run.run_id,
                    &request.mutation_id,
                    authority,
                    33
                )
                .expect("finalize"),
            expected
        );
    }
}

fn assert_publication_status_conflict(
    store: &mut WorkflowStore,
    run_id: &str,
    authority: &WorkflowExecutionAuthority,
) {
    use bcode_workflow::WorkflowRunGraphPublicationStatus as Status;
    assert_eq!(
        store
            .run_graph_publication_status(run_id, "unknown", authority)
            .expect("absent"),
        None
    );
    assert_eq!(
        store
            .run_graph_publication_status(run_id, "pending-cancel", authority)
            .expect("pending"),
        Some(Status::Pending {
            expected_revision: 2
        })
    );
    let mut stale = authority.clone();
    stale.generation += 1;
    assert!(
        store
            .run_graph_publication_status(run_id, "pending-cancel", &stale)
            .is_err()
    );
    let mut node = store
        .current_run_graph_node(run_id, "second")
        .expect("node")
        .expect("second")
        .node;
    node.name = "later revision".into();
    let edit = bcode_workflow::WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: run_id.into(),
        mutation_id: "later-edit".into(),
        expected_revision: 2,
        edits: vec![bcode_workflow::WorkflowRunGraphEdit::ReplaceNode {
            node,
            entry: false,
            exit: true,
        }],
        reconciliation: vec![
            bcode_workflow::WorkflowRunGraphReconciliation::RetainWithBindings {
                activation_id: activation_identity(run_id, "first", 0),
                edge_ids: vec![0],
            },
        ],
    };
    store
        .stage_run_graph_edit(&edit, authority, 30)
        .expect("stage later edit");
    store
        .publish_retained_leaf_run_graph_edit(run_id, &edit.mutation_id, authority, 30)
        .expect("publish later edit");
    assert_eq!(
        store
            .run_graph_publication_status(run_id, "pending-cancel", authority)
            .expect("conflict"),
        Some(Status::Conflicted {
            expected_revision: 2,
            current_revision: 3
        })
    );
    assert_eq!(
        store
            .run_graph_publication_status(run_id, "bound-connected", authority)
            .expect("stable commit"),
        Some(Status::Committed { revision: 2 })
    );
    assert_eq!(
        store
            .pending_publication_cancellations(run_id, authority, 10)
            .expect("durable cancellation")
            .len(),
        1
    );
}

fn assert_publication_cancellation_settlement(
    store: &mut WorkflowStore,
    run_id: &str,
    authority: &WorkflowExecutionAuthority,
    identity: String,
) {
    let mut stale = authority.clone();
    stale.generation += 1;
    assert!(
        store
            .pending_publication_cancellations(run_id, &stale, 10)
            .is_err()
    );
    store
        .enter_recovery_only(run_id, authority, 29)
        .expect("recovery retains accepted publication cancellation");
    let pending = store
        .pending_publication_cancellations(run_id, authority, 10)
        .expect("discover accepted intent");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].dispatch_identity, identity);
    assert!(
        store
            .has_publication_cancellation_intent(run_id, &identity)
            .expect("accepted intent")
    );
    assert!(
        !store
            .has_publication_cancellation_intent(run_id, "unrelated-dispatch")
            .expect("unrelated attempt")
    );
    assert!(
        store
            .pending_publication_cancellations_after(run_id, authority, Some(&identity), 1)
            .expect("end of cancellation sweep")
            .is_empty()
    );
    assert!(
        store
            .mark_publication_cancellation_signalled(run_id, &identity, &stale, 30)
            .is_err()
    );
    assert!(
        store
            .mark_publication_cancellation_signalled(run_id, &identity, authority, 30)
            .expect("signal")
    );
    assert!(
        !store
            .mark_publication_cancellation_signalled(run_id, &identity, authority, 30)
            .expect("duplicate signal")
    );
    let running = store
        .apply_attempt_observation(&identity, AttemptObservation::Running, 31)
        .expect("still running");
    assert!(running.cancelled.is_empty());
    assert_eq!(
        store
            .pending_publication_cancellations(run_id, authority, 10)
            .expect("still pending")
            .len(),
        1
    );
    let settled = store
        .apply_attempt_observation(&identity, AttemptObservation::Cancelled, 32)
        .expect("owner stopped");
    assert_eq!(settled.cancelled, [identity]);
    assert!(
        store
            .pending_publication_cancellations(run_id, authority, 10)
            .expect("settled")
            .is_empty()
    );
    assert!(
        !cancellation_requested_for_run(&store.connection, run_id).expect("not run cancellation")
    );
}

#[test]
fn cancellation_finalization_preserves_unresolved_attempts() {
    for status in ["repair_required", "future_attempt_state"] {
        let (_temp, mut store) = initialized_store();
        let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::Mutating);
        store.request_cancellation("run-1", 20).expect("intent");
        block_on(
            update("workflow_attempts")
                .value("status", status)
                .where_eq("dispatch_identity", identity)
                .execute(&store.connection),
        )
        .expect("unresolved fixture");
        let tx =
            database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
        assert!(!finalize_run_cancellation_if_settled(tx.as_ref(), "run-1", 21).expect("preserve"));
        database::commit(tx).expect("commit");
        assert_ne!(
            store
                .run_summary("run-1")
                .expect("summary")
                .expect("run")
                .status,
            RunStatus::Cancelled
        );
    }
}

#[test]
fn cancellation_requires_terminal_owner_evidence() {
    for observation in [
        AttemptObservation::Admitted,
        AttemptObservation::Running,
        AttemptObservation::Deferred {
            reason: "still waiting".into(),
        },
        AttemptObservation::Unknown,
    ] {
        let (_temp, mut store) = initialized_store();
        let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::Mutating);
        store.request_cancellation("run-1", 20).expect("intent");
        let unknown = matches!(observation, AttemptObservation::Unknown);
        let summary = store
            .apply_attempt_observation(&identity, observation, 21)
            .expect("nonterminal owner");
        assert!(summary.cancelled.is_empty());
        if unknown {
            assert_eq!(summary.repair_required, [identity]);
            assert_eq!(
                store
                    .run_summary("run-1")
                    .expect("summary")
                    .expect("run")
                    .status,
                RunStatus::RepairRequired
            );
            continue;
        }
        assert_eq!(
            store
                .run_summary("run-1")
                .expect("summary")
                .expect("run")
                .status,
            RunStatus::Running
        );
        assert!(
            store.attempt_history("run-1", None, 10).expect("attempts")[0]
                .terminal_at_ms
                .is_none()
        );
        let summary = store
            .apply_attempt_observation(&identity, AttemptObservation::Cancelled, 22)
            .expect("owner stopped");
        assert_eq!(summary.cancelled, [identity]);
        assert_eq!(
            store
                .run_summary("run-1")
                .expect("summary")
                .expect("run")
                .status,
            RunStatus::Cancelled
        );
    }
}

#[test]
fn cancellation_intent_wins_over_late_success_observation() {
    let (_temp, mut store) = initialized_store();
    let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::Mutating);
    store.request_cancellation("run-1", 20).expect("intent");
    assert!(
        store
            .mark_cancellation_signalled(&identity, 21)
            .expect("signal")
    );
    let output = ValidatedOutput {
        output_id: "late-output".to_string(),
        run_id: "run-1".to_string(),
        node_id: "review".to_string(),
        activation_id: activation_id(),
        schema_id: "u32".to_string(),
        schema_version: 1,
        value: serde_json::json!(1),
        artifact_reference: None,
        created_at_ms: 22,
    };

    let summary = store
        .apply_attempt_observation(&identity, AttemptObservation::Succeeded { output }, 22)
        .expect("cancelled late success");

    assert_eq!(summary.cancelled, [identity]);
    assert!(
        store
            .output_summaries("run-1", 10)
            .expect("outputs")
            .is_empty()
    );
    assert_eq!(
        store
            .run_summary("run-1")
            .expect("summary")
            .expect("run")
            .status,
        RunStatus::Cancelled
    );
}

#[test]
fn orphaned_read_only_attempt_cancellation_finalizes_run() {
    let (_temp, mut store) = initialized_store();
    let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::ReadOnly);
    store.request_cancellation("run-1", 20).expect("intent");

    let status = store
        .settle_orphaned_attempt_cancellation(&identity, 21)
        .expect("settle orphaned read-only attempt");

    assert_eq!(status, RunStatus::Cancelled);
    assert_eq!(
        store.attempt_history("run-1", None, 10).expect("attempts")[0].status,
        "cancelled"
    );
}

#[test]
fn terminal_orphan_cancellation_releases_single_active_binding() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("example", 1, &definition("example"))
        .expect("definition");
    let first = NewWorkflowRun {
        binding: Some(WorkflowRunBinding {
            owner_plugin_id: "bcode.test".to_string(),
            workflow_kind: "test.workflow".to_string(),
            scope_key: "session-1".to_string(),
            display_label: None,
            single_active: true,
        }),
        ..new_run()
    };
    store.create_run(&first).expect("first run");
    let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::ReadOnly);
    store.request_cancellation("run-1", 20).expect("cancel");
    assert_eq!(
        store
            .settle_orphaned_attempt_cancellation(&identity, 21)
            .expect("settle orphan"),
        RunStatus::Cancelled
    );

    store
        .create_run(&NewWorkflowRun {
            run_id: "run-2".to_string(),
            execution_authority: None,
            created_at_ms: 22,
            ..first
        })
        .expect("replacement after orphan cancellation");
}

#[test]
fn orphaned_receipt_less_mutation_requires_repair() {
    let (_temp, mut store) = initialized_store();
    let identity = store
        .prepare_attempt(&PreparedAttempt {
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: activation_id(),
            attempt: 1,
            side_effect: DispatchSideEffect::Mutating,
            intent: serde_json::json!({"operation": "mutate"}),
            prepared_at_ms: 12,
        })
        .expect("prepare");
    store.request_cancellation("run-1", 20).expect("intent");

    assert_eq!(
        store
            .settle_orphaned_attempt_cancellation(&identity, 21)
            .expect("classify orphaned mutation"),
        RunStatus::RepairRequired
    );
}

#[test]
fn orphaned_receipt_backed_mutation_requires_repair() {
    let (_temp, mut store) = initialized_store();
    let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::Mutating);
    store.request_cancellation("run-1", 20).expect("intent");

    let status = store
        .settle_orphaned_attempt_cancellation(&identity, 21)
        .expect("classify orphaned mutation");

    assert_eq!(status, RunStatus::RepairRequired);
    assert_eq!(
        store.attempt_history("run-1", None, 10).expect("attempts")[0].status,
        "repair_required"
    );
}

#[test]
fn cancelled_attempt_finalizes_run_after_owner_settles() {
    let (_temp, mut store) = initialized_store();
    let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::ReadOnly);
    store.request_cancellation("run-1", 20).expect("intent");
    assert!(
        store
            .mark_cancellation_signalled(&identity, 21)
            .expect("signal")
    );

    let summary = store
        .apply_attempt_observation(&identity, AttemptObservation::Cancelled, 22)
        .expect("cancelled observation");

    assert_eq!(
        summary.cancelled.as_slice(),
        std::slice::from_ref(&identity)
    );
    assert_eq!(
        store
            .run_summary("run-1")
            .expect("summary")
            .expect("run")
            .status,
        RunStatus::Cancelled
    );
    assert_eq!(
        store.attempt_history("run-1", None, 10).expect("attempts")[0].status,
        "cancelled"
    );
    assert!(
        store
            .apply_attempt_observation(&identity, AttemptObservation::Cancelled, 23)
            .is_err(),
        "terminal cancellation cannot settle a second time"
    );
    assert_eq!(
        store
            .event_history("run-1", None, 100)
            .expect("events")
            .iter()
            .filter(|event| event.event_type == "attempt_cancelled")
            .count(),
        1
    );
}

#[tokio::test]
async fn cancellation_propagates_to_receipt_backed_owner_after_durable_intent() {
    #[derive(Default)]
    struct Owner {
        requests: std::sync::Mutex<Vec<ActiveAttemptCancellation>>,
    }

    impl AttemptCancellationOwner for Owner {
        fn cancel_attempt<'a>(
            &'a self,
            request: &'a ActiveAttemptCancellation,
        ) -> Pin<Box<dyn Future<Output = Result<(), WorkflowStoreError>> + Send + 'a>> {
            Box::pin(async move {
                self.requests
                    .lock()
                    .expect("owner requests")
                    .push(request.clone());
                Ok(())
            })
        }
    }

    let (_temp, mut store) = initialized_store();
    let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::ReadOnly);
    let owner = Owner::default();
    assert!(
        store
            .propagate_cancellation("run-1", &owner, 10, 20)
            .await
            .is_err(),
        "owner signaling must be impossible before durable cancellation intent"
    );
    assert!(owner.requests.lock().expect("requests").is_empty());

    store.request_cancellation("run-1", 20).expect("intent");
    let summary = store
        .propagate_cancellation("run-1", &owner, 10, 21)
        .await
        .expect("propagate");
    assert_eq!(
        summary.signalled.as_slice(),
        std::slice::from_ref(&identity)
    );
    let requests = owner.requests.lock().expect("requests").clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].dispatch_identity, identity);
    assert_eq!(
        requests[0].receipt,
        Some(serde_json::json!({"turn_id": "turn-1"}))
    );
    let repeated = store
        .propagate_cancellation("run-1", &owner, 10, 22)
        .await
        .expect("already signalled");
    assert!(repeated.signalled.is_empty());
    assert_eq!(owner.requests.lock().expect("requests").len(), 1);
    assert_eq!(
        store.attempt_history("run-1", None, 10).expect("attempts")[0].status,
        "cancelling"
    );
}

#[tokio::test]
async fn cancellation_propagation_is_retryable_after_owner_failure() {
    struct Owner;
    impl AttemptCancellationOwner for Owner {
        fn cancel_attempt<'a>(
            &'a self,
            _request: &'a ActiveAttemptCancellation,
        ) -> Pin<Box<dyn Future<Output = Result<(), WorkflowStoreError>> + Send + 'a>> {
            Box::pin(async {
                Err(WorkflowStoreError::InvalidData(
                    "owner unavailable".to_string(),
                ))
            })
        }
    }

    let (_temp, mut store) = initialized_store();
    let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::ReadOnly);
    store.request_cancellation("run-1", 20).expect("intent");
    assert!(
        store
            .propagate_cancellation("run-1", &Owner, 10, 21)
            .await
            .is_err()
    );
    assert_eq!(
        store
            .active_attempts_for_cancellation("run-1", 10)
            .expect("retryable"),
        [identity]
    );
}

#[test]
fn active_children_are_exposed_only_after_cancellation_is_durable() {
    let (_temp, mut store) = initialized_store();
    let identity = store
        .prepare_attempt(&PreparedAttempt {
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: activation_id(),
            attempt: 1,
            side_effect: DispatchSideEffect::ReadOnly,
            intent: serde_json::json!({}),
            prepared_at_ms: 12,
        })
        .expect("attempt");
    assert!(store.active_attempts_for_cancellation("run-1", 10).is_err());
    store.request_cancellation("run-1", 20).expect("intent");
    assert_eq!(
        store
            .active_attempts_for_cancellation("run-1", 10)
            .expect("children"),
        [identity]
    );
}

#[test]
fn elapsed_deadline_fails_idle_run_once_without_replay() {
    let (_temp, mut store) = initialized_store();
    block_on(
        update("workflow_runs")
            .value("deadline_at_ms", 20)
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("deadline");
    assert!(!store.expire_run_deadline("run-1", 19).expect("early"));
    assert!(store.expire_run_deadline("run-1", 20).expect("expired"));
    assert!(!store.expire_run_deadline("run-1", 21).expect("stable"));
    assert_eq!(
        store
            .run_summary("run-1")
            .expect("summary")
            .expect("run")
            .status,
        RunStatus::Failed
    );
    assert_eq!(
        store
            .event_history("run-1", None, 20)
            .expect("events")
            .into_iter()
            .filter(|event| event.event_type == "run_deadline_elapsed")
            .count(),
        1
    );
}

#[test]
fn persisted_limits_fail_closed_at_activation_and_attempt_admission() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("example", 1, &definition("example"))
        .expect("definition");
    let mut run = new_run();
    run.limits = WorkflowRunLimits {
        deadline_at_ms: Some(20),
        node_execution_cap: 1,
        concurrency_cap: Some(1),
        cycle_cap: 1,
        retry_cap: 0,
        ..WorkflowRunLimits::default()
    };
    store.create_run(&run).expect("run");
    let attempt = PreparedAttempt {
        run_id: "run-1".to_string(),
        node_id: "review".to_string(),
        activation_id: activation_id(),
        attempt: 1,
        side_effect: DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({}),
        prepared_at_ms: 12,
    };
    store.prepare_attempt(&attempt).expect("first attempt");
    assert!(
        store
            .prepare_attempt(&PreparedAttempt {
                activation_id: "activation-2".to_string(),
                ..attempt.clone()
            })
            .is_err()
    );
    assert!(
        store
            .create_activation(&NewActivation {
                activation_id: "cycle-overflow".to_string(),
                dependency_generation: 1,
                ..new_activation()
            })
            .is_err()
    );
    let (_temp, mut deadline_store) = initialized_store();
    block_on(
        update("workflow_runs")
            .value("deadline_at_ms", 12)
            .where_eq("run_id", "run-1")
            .execute(&deadline_store.connection),
    )
    .expect("deadline");
    assert!(deadline_store.prepare_attempt(&attempt).is_err());
}

#[test]
fn activation_admission_supports_maximum_cycle_cap() {
    let (_temp, mut store) = initialized_store();
    block_on(
        update("workflow_runs")
            .value("cycle_cap", DatabaseValue::Int64(i64::from(u32::MAX)))
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("maximum cap fixture");
    assert_eq!(
        store
            .run_limits("run-1")
            .expect("limits")
            .expect("run")
            .cycle_cap,
        u32::MAX
    );
    store
        .create_activation(&NewActivation {
            activation_id: "maximum-cap-admitted".to_string(),
            dependency_generation: u64::from(u32::MAX) - 1,
            ..new_activation()
        })
        .expect("generation below maximum cap");
    let before = fixture_changes(&store.connection);
    assert!(
        store
            .create_activation(&NewActivation {
                activation_id: "maximum-cap-rejected".to_string(),
                dependency_generation: u64::from(u32::MAX),
                ..new_activation()
            })
            .is_err()
    );
    assert_eq!(fixture_changes(&store.connection), before);
    assert!(block_on(store.connection.sqlite_is_autocommit()).expect("owned snapshot released"));
}

#[test]
fn activation_admission_rejects_out_of_range_stored_cycle_cap() {
    let (_temp, mut store) = initialized_store();
    block_on(
        update("workflow_runs")
            .value("cycle_cap", u64::from(u32::MAX) + 1)
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("invalid cap fixture");
    let before = fixture_changes(&store.connection);
    assert!(store.run_limits("run-1").is_err());
    assert!(
        store
            .create_activation(&NewActivation {
                activation_id: "invalid-cycle-cap".to_string(),
                ..new_activation()
            })
            .is_err()
    );
    assert_eq!(fixture_changes(&store.connection), before);
    assert!(block_on(store.connection.sqlite_is_autocommit()).expect("owned snapshot released"));
}

#[test]
fn activation_deadline_is_exclusive_and_rejection_does_not_mutate() {
    let (_temp, mut store) = initialized_store();
    block_on(
        update("workflow_runs")
            .value("deadline_at_ms", 20)
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("deadline fixture");
    let activation = NewActivation {
        activation_id: "deadline-activation".to_string(),
        created_at_ms: 19,
        ..new_activation()
    };
    store
        .create_activation(&activation)
        .expect("before deadline");
    for created_at_ms in [20, 21] {
        let before = fixture_changes(&store.connection);
        let error = store
            .create_activation(&NewActivation {
                created_at_ms,
                ..activation.clone()
            })
            .expect_err("expired activation");
        assert!(
            error
                .to_string()
                .contains("workflow wall-clock deadline has elapsed")
        );
        assert_eq!(fixture_changes(&store.connection), before);
        assert!(
            block_on(store.connection.sqlite_is_autocommit()).expect("owned snapshot released")
        );
    }
    block_on(
        update("workflow_runs")
            .value("deadline_at_ms", DatabaseValue::Null)
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("remove deadline");
    store
        .create_activation(&NewActivation {
            activation_id: "no-deadline-activation".to_string(),
            created_at_ms: 21,
            ..activation
        })
        .expect("no deadline");
}

#[test]
fn activation_admission_preserves_deadline_and_cancellation_after_reopen() {
    let (temp, store) = initialized_store();
    block_on(
        update("workflow_runs")
            .value("deadline_at_ms", 20)
            .value("cancellation_requested_at_ms", 12)
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("persist admission state");
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    let activation = NewActivation {
        activation_id: "after-reopen".to_string(),
        created_at_ms: 19,
        ..new_activation()
    };
    let before = fixture_changes(&store.connection);
    let error = store
        .create_activation(&activation)
        .expect_err("durable cancellation");
    assert!(
        error
            .to_string()
            .contains("workflow cancellation has been requested")
    );
    assert_eq!(fixture_changes(&store.connection), before);
    // Isolate the persisted deadline from the cancellation fixture.
    block_on(
        update("workflow_runs")
            .value("cancellation_requested_at_ms", DatabaseValue::Null)
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("clear fixture cancellation");
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen deadline");
    let before = fixture_changes(&store.connection);
    let error = store
        .create_activation(&NewActivation {
            created_at_ms: 20,
            ..activation.clone()
        })
        .expect_err("durable deadline");
    assert!(
        error
            .to_string()
            .contains("workflow wall-clock deadline has elapsed")
    );
    assert_eq!(fixture_changes(&store.connection), before);
    store
        .create_activation(&activation)
        .expect("before persisted deadline");
}

#[test]
fn activation_creation_rejects_durable_cancellation_without_mutation() {
    let (_temp, mut store) = initialized_store();
    block_on(
        update("workflow_runs")
            .value("cancellation_requested_at_ms", 12)
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("cancellation fixture");
    let before = fixture_changes(&store.connection);
    let error = store
        .create_activation(&NewActivation {
            activation_id: "after-cancellation".to_string(),
            ..new_activation()
        })
        .expect_err("cancelled admission");
    assert!(
        error
            .to_string()
            .contains("workflow cancellation has been requested")
    );
    assert_eq!(fixture_changes(&store.connection), before);
    assert!(block_on(store.connection.sqlite_is_autocommit()).expect("owned snapshot released"));
}

#[test]
fn admission_rejects_unknown_run_status_without_exposing_it() {
    let (_temp, store) = initialized_store();
    block_on(
        update("workflow_runs")
            .value("status", "future-secret-status")
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("future status fixture");
    let before = fixture_changes(&store.connection);
    let attempt = PreparedAttempt {
        run_id: "run-1".to_string(),
        node_id: "review".to_string(),
        activation_id: activation_id(),
        attempt: 1,
        side_effect: DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({}),
        prepared_at_ms: 12,
    };
    for error in [
        enforce_attempt_limits(&store.connection, &attempt).expect_err("unknown status"),
        enforce_activation_limits(&store.connection, &new_activation())
            .expect_err("unknown status"),
    ] {
        assert!(error.to_string().contains("unknown workflow run status"));
        assert!(!error.to_string().contains("future-secret-status"));
    }
    assert_eq!(fixture_changes(&store.connection), before);
}

#[test]
fn attempt_admission_state_preserves_deadline_retry_and_cancellation_boundaries() {
    let mut attempt = PreparedAttempt {
        run_id: "run-1".to_string(),
        node_id: "review".to_string(),
        activation_id: activation_id(),
        attempt: 1,
        side_effect: DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({}),
        prepared_at_ms: 12,
    };
    validate_attempt_admission_state(RunStatus::Running, false, Some(13), 0, &attempt)
        .expect("first attempt before deadline");
    for status in [
        RunStatus::Paused,
        RunStatus::RepairRequired,
        RunStatus::Failed,
        RunStatus::Completed,
        RunStatus::Cancelled,
    ] {
        assert!(validate_attempt_admission_state(status, false, None, 0, &attempt).is_err());
    }
    assert!(validate_attempt_admission_state(RunStatus::Running, true, None, 0, &attempt).is_err());
    assert!(
        validate_attempt_admission_state(RunStatus::Running, false, Some(12), 0, &attempt).is_err()
    );
    attempt.attempt = 2;
    assert!(
        validate_attempt_admission_state(RunStatus::Running, false, None, 0, &attempt).is_err()
    );
    validate_attempt_admission_state(RunStatus::Running, false, None, 1, &attempt)
        .expect("one permitted retry");
    attempt.attempt = u32::MAX;
    validate_attempt_admission_state(RunStatus::Running, false, None, u32::MAX, &attempt)
        .expect("retry allowance addition saturates");
}

#[test]
fn attempt_admission_limits_are_run_scoped() {
    let (_temp, mut store) = initialized_store();
    let mut other = new_run();
    other.run_id = "other-run".to_string();
    store.create_run(&other).expect("other run");
    let attempt = PreparedAttempt {
        run_id: "other-run".to_string(),
        node_id: "review".to_string(),
        activation_id: activation_identity("other-run", "review", 0),
        attempt: 1,
        side_effect: DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({}),
        prepared_at_ms: 12,
    };
    store.prepare_attempt(&attempt).expect("other attempt");
    block_on(
        update("workflow_runs")
            .value("concurrency_cap", 1)
            .value("node_execution_cap", 1)
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("limits");
    let local = PreparedAttempt {
        run_id: "run-1".to_string(),
        activation_id: activation_id(),
        ..attempt
    };
    let before = fixture_changes(&store.connection);
    enforce_attempt_limits(&store.connection, &local)
        .expect("foreign attempt consumes neither allowance");
    assert_eq!(fixture_changes(&store.connection), before);
    store.prepare_attempt(&local).expect("local admission");
    let error =
        enforce_attempt_limits(&store.connection, &local).expect_err("local allowance exhausted");
    assert!(
        error
            .to_string()
            .contains("workflow node-execution cap exceeded")
    );
}

#[test]
fn concurrency_snapshot_tracks_uncapped_reservations_without_mutation() {
    let (_temp, mut store) = initialized_store();
    let empty = store.concurrency_snapshot("run-1").expect("snapshot");
    assert_eq!(empty.concurrency_cap, None);
    assert_eq!(empty.occupied_count, 0);
    store
        .prepare_attempt(&PreparedAttempt {
            run_id: "run-1".into(),
            node_id: "review".into(),
            activation_id: activation_id(),
            attempt: 1,
            side_effect: DispatchSideEffect::ReadOnly,
            intent: serde_json::json!({}),
            prepared_at_ms: 12,
        })
        .expect("reserve");
    let before = fixture_changes(&store.connection);
    let occupied = store.concurrency_snapshot("run-1").expect("snapshot");
    assert_eq!(occupied.occupied_count, 1);
    let scheduling = store
        .scheduling_snapshot("run-1", (None, 1))
        .expect("scheduling snapshot");
    assert_eq!(scheduling.capacity, occupied);
    assert_eq!(scheduling.activations.len(), 1);
    assert_eq!(scheduling.activations[0].node_id, "review");
    let reserved = scheduling.activations[0]
        .latest_attempt
        .as_ref()
        .expect("exact attempt");
    assert_eq!(reserved.activation_id, activation_id());
    assert_eq!(reserved.status, "prepared");
    assert!(!reserved.has_receipt);
    assert!(reserved.terminal_at_ms.is_none());
    assert_eq!(occupied.graph_revision, empty.graph_revision);
    assert_eq!(fixture_changes(&store.connection), before);
    assert!(block_on(store.connection.sqlite_is_autocommit()).expect("owned snapshot released"));
}

#[test]
fn concurrency_admission_counts_all_active_states_but_not_terminal_attempts() {
    let (_temp, mut store) = initialized_store();
    block_on(
        update("workflow_runs")
            .value("concurrency_cap", 1)
            .value("node_execution_cap", 100)
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("limits");
    let attempt = PreparedAttempt {
        run_id: "run-1".to_string(),
        node_id: "review".to_string(),
        activation_id: activation_id(),
        attempt: 1,
        side_effect: DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({}),
        prepared_at_ms: 12,
    };
    enforce_attempt_limits(&store.connection, &attempt).expect("empty capacity");
    store.prepare_attempt(&attempt).expect("first attempt");
    for status in [
        "prepared",
        "admitted",
        "running",
        "cancelling",
        "sibling_cancelling",
    ] {
        block_on(
            update("workflow_attempts")
                .value("status", status)
                .execute(&store.connection),
        )
        .expect("active fixture");
        let before = fixture_changes(&store.connection);
        let error =
            enforce_attempt_limits(&store.connection, &attempt).expect_err("capacity occupied");
        assert!(
            error
                .to_string()
                .contains("workflow concurrency cap reached"),
            "{status}: {error}"
        );
        assert_eq!(fixture_changes(&store.connection), before);
    }
    for status in ["succeeded", "failed", "cancelled", "abandoned"] {
        block_on(
            update("workflow_attempts")
                .value("status", status)
                .execute(&store.connection),
        )
        .expect("terminal fixture");
        let before = fixture_changes(&store.connection);
        enforce_attempt_limits(&store.connection, &attempt).expect("capacity released");
        assert_eq!(fixture_changes(&store.connection), before);
        block_on(
            update("workflow_runs")
                .value("node_execution_cap", 1)
                .where_eq("run_id", "run-1")
                .execute(&store.connection),
        )
        .expect("exhausted allowance");
        let before = fixture_changes(&store.connection);
        let error = enforce_attempt_limits(&store.connection, &attempt)
            .expect_err("terminal attempt consumes allowance");
        assert!(
            error
                .to_string()
                .contains("workflow node-execution cap exceeded")
        );
        assert_eq!(fixture_changes(&store.connection), before);
        block_on(
            update("workflow_runs")
                .value("node_execution_cap", 100)
                .where_eq("run_id", "run-1")
                .execute(&store.connection),
        )
        .expect("restore allowance");
    }
}

#[test]
fn oversized_definition_is_rejected_before_persistence() {
    let (_temp, mut store) = initialized_store();
    let mut definition = definition("oversized");
    definition
        .nodes
        .get_mut("review")
        .expect("node")
        .configuration = serde_json::json!({"padding": "x".repeat(MAX_INLINE_JSON_BYTES)});
    assert!(
        store
            .persist_definition("oversized-definition", 1, &definition)
            .is_err()
    );
    assert!(
        store
            .definition("oversized-definition", 1)
            .expect("lookup")
            .is_none()
    );
}

#[test]
fn doctor_is_bounded_non_mutating_and_reports_corruption() {
    let (_temp, mut store) = initialized_store();
    let identity = store
        .prepare_attempt(&PreparedAttempt {
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: activation_id(),
            attempt: 1,
            side_effect: DispatchSideEffect::Mutating,
            intent: serde_json::json!({"operation": "apply"}),
            prepared_at_ms: 12,
        })
        .expect("prepare");
    block_on(
        update("workflow_attempts")
            .value("dispatch_identity", "corrupt")
            .value("status", "repair_required")
            .where_eq("dispatch_identity", identity)
            .execute(&store.connection),
    )
    .expect("corrupt identity");
    block_on(
        update("workflow_activations")
            .value("status", "completed")
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("corrupt activation");
    block_on(
        insert("workflow_grants")
            .value("grant_id", "corrupt-grant")
            .value("run_id", "run-1")
            .value("node_id", "review")
            .value("scope_json", "{}")
            .value("granted_at_ms", 10)
            .value("expires_at_ms", DatabaseValue::Null)
            .execute(&store.connection),
    )
    .expect("corrupt grant");
    let event_count_before: u64 = fixture_scalar_unsigned(
        &store.connection,
        &select("workflow_events").project_as(switchy_database::query::count_all(), "count"),
        "count",
    )
    .expect("events");

    let first = store.doctor_run("run-1", 1).expect("doctor");
    assert!(first.truncated);
    assert_eq!(first.issues.len(), 1);
    let full = store.doctor_run("run-1", 10).expect("doctor");
    assert_eq!(full.issues.len(), 4);
    assert!(
        full.issues
            .iter()
            .any(|issue| matches!(issue, WorkflowDoctorIssue::RepairStatusMismatch { .. }))
    );
    assert!(
        full.issues
            .iter()
            .any(|issue| matches!(issue, WorkflowDoctorIssue::ActivationOutputMismatch { .. }))
    );
    assert!(
        full.issues
            .iter()
            .any(|issue| matches!(issue, WorkflowDoctorIssue::InvalidGrant { .. }))
    );
    assert!(
        full.issues
            .iter()
            .any(|issue| matches!(issue, WorkflowDoctorIssue::AttemptIdentityMismatch { .. }))
    );
    let event_count_after: u64 = fixture_scalar_unsigned(
        &store.connection,
        &select("workflow_events").project_as(switchy_database::query::count_all(), "count"),
        "count",
    )
    .expect("events");
    assert_eq!(event_count_before, event_count_after);
}

#[test]
fn explicit_repair_finishes_persisted_cancellation_after_restart() {
    let (temp, mut store) = initialized_store();
    let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::Mutating);
    store.request_cancellation("run-1", 20).expect("intent");
    store
        .settle_orphaned_attempt_cancellation(&identity, 21)
        .expect("ambiguous");
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
    let result = store
        .repair_attempt(
            &identity,
            &RepairResolution::ConfirmCancelled {
                message: "operator verified external work stopped".into(),
            },
            22,
        )
        .expect("repair");
    assert_eq!(result.run_status, RunStatus::Cancelled);
    assert_eq!(result.attempt_status, "cancelled");
    assert!(store.resume_run("run-1", 23).is_err());
    assert_eq!(
        store
            .run_summary("run-1")
            .expect("summary")
            .expect("run")
            .status,
        RunStatus::Cancelled
    );
}

#[test]
fn explicit_repair_requires_proof_and_never_dispatches() {
    let (_temp, mut store) = initialized_store();
    let identity = store
        .prepare_attempt(&PreparedAttempt {
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: activation_id(),
            attempt: 1,
            side_effect: DispatchSideEffect::Mutating,
            intent: serde_json::json!({"operation": "apply"}),
            prepared_at_ms: 12,
        })
        .expect("prepare");
    store
        .reconcile_prepared_attempts(10, 20)
        .expect("repair required");
    let error = store
        .repair_attempt(
            &identity,
            &RepairResolution::ConfirmSucceeded {
                output: ValidatedOutput {
                    output_id: "output-1".to_string(),
                    run_id: "wrong-run".to_string(),
                    node_id: "review".to_string(),
                    activation_id: activation_id(),
                    schema_id: "u32".to_string(),
                    schema_version: 1,
                    value: serde_json::json!(1),
                    artifact_reference: None,
                    created_at_ms: 21,
                },
            },
            21,
        )
        .expect_err("mismatched proof");
    assert!(error.to_string().contains("does not match"));

    let result = store
        .repair_attempt(
            &identity,
            &RepairResolution::ConfirmSucceeded {
                output: ValidatedOutput {
                    output_id: "output-1".to_string(),
                    run_id: "run-1".to_string(),
                    node_id: "review".to_string(),
                    activation_id: activation_id(),
                    schema_id: "u32".to_string(),
                    schema_version: 1,
                    value: serde_json::json!(1),
                    artifact_reference: None,
                    created_at_ms: 21,
                },
            },
            21,
        )
        .expect("repair");
    assert_eq!(result.attempt_status, "succeeded");
    assert_eq!(result.run_status, RunStatus::Paused);
    assert_eq!(
        store
            .run_summary("run-1")
            .expect("summary")
            .expect("run")
            .status,
        RunStatus::Paused
    );
    assert!(
        store
            .doctor_run("run-1", 10)
            .expect("doctor")
            .issues
            .is_empty()
    );
}

fn assert_attempt_coordinate_corruption_is_non_mutating(store: &WorkflowStore, identity: &str) {
    block_on(
        update("workflow_attempts")
            .value("attempt", 3)
            .where_eq("dispatch_identity", identity)
            .execute(&store.connection),
    )
    .unwrap();
    let before = fixture_changes(&store.connection);
    let error = store
        .attempt_history("run-1", None, 1)
        .expect_err("reject inconsistent identity");
    assert!(error.to_string().contains("identity conflicts"));
    assert_eq!(fixture_changes(&store.connection), before);
    block_on(
        update("workflow_attempts")
            .value("attempt", 2)
            .where_eq("dispatch_identity", identity)
            .execute(&store.connection),
    )
    .unwrap();
}

#[test]
fn explicit_abandonment_is_required_before_retrying_ambiguous_mutation() {
    let (_temp, mut store) = initialized_store();
    let attempt = PreparedAttempt {
        run_id: "run-1".to_string(),
        node_id: "review".to_string(),
        activation_id: activation_id(),
        attempt: 1,
        side_effect: DispatchSideEffect::Mutating,
        intent: serde_json::json!({"operation": "apply"}),
        prepared_at_ms: 12,
    };
    let identity = store.prepare_attempt(&attempt).expect("prepare");
    store
        .reconcile_prepared_attempts(10, 20)
        .expect("repair required");
    assert!(
        store
            .prepare_attempt(&PreparedAttempt {
                attempt: 2,
                prepared_at_ms: 21,
                ..attempt.clone()
            })
            .is_err()
    );
    let result = store
        .repair_attempt(
            &identity,
            &RepairResolution::AbandonForExplicitRetry {
                reason: "operator verified that no external mutation occurred".to_string(),
            },
            22,
        )
        .expect("abandon");
    assert_eq!(result.attempt_status, "abandoned");
    store
        .prepare_attempt(&PreparedAttempt {
            attempt: 2,
            prepared_at_ms: 23,
            ..attempt
        })
        .expect("explicit retry");
    let before = fixture_changes(&store.connection);
    let newest = store.attempt_history("run-1", None, 1).expect("retry page");
    assert_eq!(newest.len(), 1);
    assert_eq!(newest[0].attempt, 2);
    assert_eq!(newest[0].status, "prepared");
    let cursor = super::AttemptCursor {
        prepared_at_ms: newest[0].prepared_at_ms,
        dispatch_identity: newest[0].dispatch_identity.clone(),
    };
    let older = store
        .attempt_history("run-1", Some(&cursor), 1)
        .expect("abandoned page");
    assert_eq!(older.len(), 1);
    assert_eq!(older[0].dispatch_identity, identity);
    assert_eq!(older[0].attempt, 1);
    assert_eq!(older[0].status, "abandoned");
    assert_eq!(older[0].terminal_at_ms, Some(22));
    let end_cursor = super::AttemptCursor {
        prepared_at_ms: older[0].prepared_at_ms,
        dispatch_identity: older[0].dispatch_identity.clone(),
    };
    assert!(
        store
            .attempt_history("run-1", Some(&end_cursor), 1)
            .expect("end of attempt history")
            .is_empty()
    );
    assert_eq!(fixture_changes(&store.connection), before);
    assert_attempt_coordinate_corruption_is_non_mutating(&store, &newest[0].dispatch_identity);
    block_on(
        update("workflow_attempts")
            .value("status", "unsupported_future_status")
            .where_eq("dispatch_identity", newest[0].dispatch_identity.as_str())
            .execute(&store.connection),
    )
    .expect("damaged newest attempt");
    let before = fixture_changes(&store.connection);
    assert!(matches!(
        store.attempt_history("run-1", None, 1),
        Err(WorkflowStoreError::InvalidData(_))
    ));
    let preserved = store
        .attempt_history("run-1", Some(&cursor), 1)
        .expect("older page remains accessible");
    assert_eq!(preserved.len(), 1);
    assert_eq!(preserved[0].dispatch_identity, identity);
    assert_eq!(preserved[0].status, "abandoned");
    let stored_status = fixture_scalar_text(
        &store.connection,
        &select("workflow_attempts")
            .columns(&["status"])
            .where_eq("dispatch_identity", newest[0].dispatch_identity.as_str()),
        "status",
    )
    .expect("preserved unsupported status");
    assert_eq!(stored_status, "unsupported_future_status");
    assert_eq!(fixture_changes(&store.connection), before);
}

#[test]
#[allow(clippy::too_many_lines)]
fn durable_input_gate_waits_validates_and_activates_successor() {
    let (temp, mut store) = initialized_store();
    let schema = bcode_workflow::ValueSchema {
        type_name: "u32".to_string(),
        schema: serde_json::json!({"type": "integer", "minimum": 0}),
    };
    let definition = bcode_workflow::WorkflowDefinition {
        schema_version: bcode_workflow::WORKFLOW_DEFINITION_SCHEMA_VERSION,
        name: "input-gate".to_string(),
        input: schema.clone(),
        output: schema.clone(),
        nodes: std::collections::BTreeMap::from([
            (
                "input".to_string(),
                bcode_workflow::NodeDefinition {
                    id: "input".to_string(),
                    name: "input".to_string(),
                    kind: bcode_workflow::NodeKind::Input,
                    dataflow: bcode_workflow::WorkflowNodeDataflowPolicy::Direct,
                    input: schema.clone(),
                    output: schema.clone(),
                    resources: Vec::new(),
                    configuration: serde_json::json!({"gate_version": 1}),
                },
            ),
            (
                "next".to_string(),
                bcode_workflow::NodeDefinition {
                    id: "next".to_string(),
                    name: "next".to_string(),
                    kind: bcode_workflow::NodeKind::Task,
                    dataflow: bcode_workflow::WorkflowNodeDataflowPolicy::Direct,
                    input: schema.clone(),
                    output: schema,
                    resources: Vec::new(),
                    configuration: serde_json::Value::Null,
                },
            ),
        ]),
        entries: vec!["input".to_string()],
        exits: vec!["next".to_string()],
        edges: vec![bcode_workflow::EdgeDefinition {
            from: "input".to_string(),
            to: "next".to_string(),
            kind: bcode_workflow::EdgeKind::Direct,
            transform: None,
        }],
    };
    store
        .persist_definition("input-gate", 1, &definition)
        .expect("definition");
    let mut run = new_run();
    run.run_id = "input-run".to_string();
    run.definition_id = "input-gate".to_string();
    store.create_run(&run).expect("run");
    assert!(
        store
            .pending_activations(10)
            .expect("pending")
            .into_iter()
            .all(|activation| activation.run_id != "input-run")
    );
    let wait = store
        .waiting_activations("input-run", 10)
        .expect("waits")
        .pop()
        .expect("wait");
    assert_eq!(wait.kind, WorkflowWaitKind::Input);
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
    assert_eq!(
        store
            .waiting_activations("input-run", 10)
            .expect("reopened wait"),
        std::slice::from_ref(&wait)
    );
    assert!(
        store
            .provide_input(
                "input-run",
                "input",
                &wait.activation_id,
                serde_json::json!("wrong"),
                20,
            )
            .is_err()
    );
    block_on(
        update("workflow_activations")
            .value("node_revision", 2)
            .where_eq("run_id", "input-run")
            .execute(&store.connection),
    )
    .expect("invalid binding");
    let before = fixture_changes(&store.connection);
    assert!(
        store
            .provide_input(
                "input-run",
                "input",
                &wait.activation_id,
                serde_json::json!(7),
                20
            )
            .is_err()
    );
    assert_eq!(fixture_changes(&store.connection), before);
    block_on(
        update("workflow_activations")
            .value("node_revision", 1)
            .where_eq("run_id", "input-run")
            .execute(&store.connection),
    )
    .expect("restore binding");
    let result = store
        .provide_input(
            "input-run",
            "input",
            &wait.activation_id,
            serde_json::json!(7),
            21,
        )
        .expect("resolve");
    assert_eq!(result.outcome, "accepted");
    assert_eq!(result.activated.len(), 1);
    let next = store
        .pending_activations(10)
        .expect("pending")
        .into_iter()
        .find(|activation| activation.run_id == "input-run")
        .expect("next");
    assert_eq!(next.node_id, "next");
    assert_eq!(next.input, Some(serde_json::json!(7)));
    assert!(
        store
            .waiting_activations("input-run", 10)
            .expect("waits")
            .is_empty()
    );
}

#[test]
fn waiting_resolution_rejects_non_gate_nodes_without_writes() {
    for status in ["waiting_input", "waiting_approval"] {
        let (_temp, mut store) = initialized_store();
        block_on(
            update("workflow_activations")
                .value("status", status)
                .where_eq("run_id", "run-1")
                .where_eq("node_id", "review")
                .execute(&store.connection),
        )
        .expect("inconsistent wait fixture");
        let before = fixture_changes(&store.connection);
        if status == "waiting_input" {
            let error = store
                .provide_input(
                    "run-1",
                    "review",
                    &activation_id(),
                    serde_json::json!(3),
                    20,
                )
                .expect_err("not an input gate");
            assert!(error.to_string().contains("admitted node kind"));
        } else {
            for accepted in [false, true] {
                let error = store
                    .resolve_approval("run-1", "review", &activation_id(), accepted, 20)
                    .expect_err("not an approval gate");
                assert!(error.to_string().contains("admitted node kind"));
            }
        }
        assert_eq!(fixture_changes(&store.connection), before);
        assert_eq!(
            store
                .run_summary("run-1")
                .expect("summary")
                .expect("run")
                .status,
            RunStatus::Running
        );
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn durable_approval_denial_is_terminal_and_never_activates_downstream() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    let schema = bcode_workflow::ValueSchema {
        type_name: "u32".to_string(),
        schema: serde_json::json!({"type": "integer"}),
    };
    let definition = bcode_workflow::WorkflowDefinition {
        schema_version: bcode_workflow::WORKFLOW_DEFINITION_SCHEMA_VERSION,
        name: "approval".to_string(),
        input: schema.clone(),
        output: schema.clone(),
        nodes: std::collections::BTreeMap::from([
            (
                "approve".to_string(),
                bcode_workflow::NodeDefinition {
                    id: "approve".to_string(),
                    name: "approve".to_string(),
                    kind: bcode_workflow::NodeKind::Approval,
                    dataflow: bcode_workflow::WorkflowNodeDataflowPolicy::Direct,
                    input: schema.clone(),
                    output: schema.clone(),
                    resources: Vec::new(),
                    configuration: serde_json::json!({"gate_version": 1}),
                },
            ),
            (
                "mutate".to_string(),
                bcode_workflow::NodeDefinition {
                    id: "mutate".to_string(),
                    name: "mutate".to_string(),
                    kind: bcode_workflow::NodeKind::Task,
                    dataflow: bcode_workflow::WorkflowNodeDataflowPolicy::Direct,
                    input: schema.clone(),
                    output: schema,
                    resources: Vec::new(),
                    configuration: serde_json::Value::Null,
                },
            ),
        ]),
        entries: vec!["approve".to_string()],
        exits: vec!["mutate".to_string()],
        edges: vec![bcode_workflow::EdgeDefinition {
            from: "approve".to_string(),
            to: "mutate".to_string(),
            kind: bcode_workflow::EdgeKind::Direct,
            transform: None,
        }],
    };
    store
        .persist_definition("approval", 1, &definition)
        .expect("definition");
    let authority = WorkflowExecutionAuthority {
        target_artifact_id: "test-artifact".into(),
        daemon_instance_id: "test-owner".into(),
        generation: 2,
        fencing_token: "current-token".into(),
    };
    store
        .create_run(&NewWorkflowRun {
            run_id: "approval-run".to_string(),
            definition_id: "approval".to_string(),
            definition_version: 1,
            workspace_snapshot: "snapshot".to_string(),
            parent_session_id: Some("approval-session".into()),
            parent_session_generation: None,
            binding: None,
            authored_provenance: None,
            input: Some(serde_json::json!(3)),
            execution_authority: Some(authority.clone()),
            created_at_ms: 1,
            authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                version: 1,
                provider_id: "test-policy".to_string(),
                profile_id: "build".to_string(),
                policy_digest_sha256: "a".repeat(64),
            },
            authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
            limits: WorkflowRunLimits::default(),
        })
        .expect("run");
    let wait = store
        .waiting_activations("approval-run", 10)
        .expect("waits")
        .pop()
        .expect("wait");
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
    assert_eq!(
        store
            .waiting_activations("approval-run", 10)
            .expect("reopened wait"),
        std::slice::from_ref(&wait)
    );
    let foreign = WorkflowExecutionAuthority {
        target_artifact_id: "foreign".into(),
        daemon_instance_id: "foreign".into(),
        generation: 1,
        fencing_token: "foreign".into(),
    };
    let before = fixture_changes(&store.connection);
    assert!(
        store
            .resolve_approval_owned(
                "approval-run",
                "approve",
                &wait.activation_id,
                true,
                18,
                &foreign
            )
            .is_err()
    );
    assert_eq!(fixture_changes(&store.connection), before);
    assert_eq!(
        store.waiting_activations("approval-run", 10).unwrap(),
        std::slice::from_ref(&wait)
    );
    let mut stale = authority.clone();
    stale.generation = 1;
    assert!(
        store
            .resolve_approval_owned(
                "approval-run",
                "approve",
                &wait.activation_id,
                true,
                18,
                &stale
            )
            .is_err()
    );
    assert_eq!(fixture_changes(&store.connection), before);
    for (table, column, value) in [
        (
            "workflow_run_graph_nodes",
            "node_json",
            DatabaseValue::String("{}".into()),
        ),
        ("workflow_run_graphs", "revision", DatabaseValue::Int64(2)),
    ] {
        let query = update(table)
            .value(column, value)
            .where_eq("run_id", "approval-run");
        let query = if table == "workflow_run_graph_nodes" {
            query.where_eq("node_id", "approve")
        } else {
            query
        };
        block_on(query.execute(&store.connection)).expect("damage fixture");
        let before = fixture_changes(&store.connection);
        for accepted in [false, true] {
            assert!(
                store
                    .resolve_approval("approval-run", "approve", &wait.activation_id, accepted, 19)
                    .is_err()
            );
            assert_eq!(fixture_changes(&store.connection), before);
        }
        block_on(
            update("workflow_run_graph_nodes")
                .value(
                    "node_json",
                    serde_json::to_string(&definition.nodes["approve"]).expect("node"),
                )
                .where_eq("run_id", "approval-run")
                .where_eq("node_id", "approve")
                .execute(&store.connection),
        )
        .expect("restore node fixture");
        block_on(async {
            update("workflow_run_graphs")
                .value("revision", 1)
                .where_eq("run_id", "approval-run")
                .execute(&store.connection)
                .await?;
            Ok::<(), switchy_database::DatabaseError>(())
        })
        .expect("restore revision");
    }
    // Denial must use the admitted node even if the source fixture changes.
    let mut source = definition.clone();
    source.nodes.clear();
    let json = serde_json::to_string(&source).expect("source");
    block_on(
        update("workflow_definitions")
            .value("definition_json", json.as_str())
            .value("checksum_sha256", sha256_hex(json.as_bytes()))
            .where_eq("definition_id", "approval")
            .execute(&store.connection),
    )
    .expect("source fixture");
    let result = store
        .resolve_approval_owned(
            "approval-run",
            "approve",
            &wait.activation_id,
            false,
            20,
            &authority,
        )
        .expect("deny");
    assert_eq!(result.outcome, "denied");
    assert_eq!(result.run_status, RunStatus::Failed);
    assert!(store.pending_activations(10).expect("pending").is_empty());
    assert_eq!(
        store
            .run_summary("approval-run")
            .expect("summary")
            .expect("run")
            .status,
        RunStatus::Failed
    );
}

#[test]
fn output_faults_roll_back_every_atomic_boundary() {
    struct Fault(WorkflowOutputBoundary);
    impl WorkflowOutputFault for Fault {
        fn after_boundary(
            &self,
            boundary: WorkflowOutputBoundary,
            _output: &ValidatedOutput,
        ) -> Result<(), WorkflowStoreError> {
            if boundary == self.0 {
                return Err(WorkflowStoreError::InvalidData(format!(
                    "crash after {boundary:?}"
                )));
            }
            Ok(())
        }
    }

    for boundary in [
        WorkflowOutputBoundary::OutputInserted,
        WorkflowOutputBoundary::ActivationCompleted,
        WorkflowOutputBoundary::SuccessorsMaterialized,
    ] {
        let temp = tempfile::tempdir().expect("temp");
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("sequential", 1, &sequential_definition())
            .expect("definition");
        let mut run = new_run();
        run.run_id = "fault-run".to_string();
        run.definition_id = "sequential".to_string();
        store.create_run(&run).expect("run");
        let output = ValidatedOutput {
            output_id: "fault-output".to_string(),
            run_id: "fault-run".to_string(),
            node_id: "first".to_string(),
            activation_id: activation_identity("fault-run", "first", 0),
            schema_id: "u32".to_string(),
            schema_version: 1,
            value: serde_json::json!(2),
            artifact_reference: None,
            created_at_ms: 20,
        };
        assert!(
            store
                .persist_validated_output_with_fault(&output, &Fault(boundary))
                .is_err()
        );
        drop(store);
        let reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
        let output_count = fixture_scalar_unsigned(
            &reopened.connection,
            &select("workflow_outputs")
                .project_as(switchy_database::query::count_all(), "count")
                .where_eq("run_id", "fault-run"),
            "count",
        )
        .expect("outputs");
        let first_status = fixture_scalar_text(
            &reopened.connection,
            &select("workflow_activations")
                .columns(&["status"])
                .where_eq("run_id", "fault-run")
                .where_eq("node_id", "first"),
            "status",
        )
        .expect("first");
        let successor_count = fixture_scalar_unsigned(
            &reopened.connection,
            &select("workflow_activations")
                .project_as(switchy_database::query::count_all(), "count")
                .where_eq("run_id", "fault-run")
                .where_eq("node_id", "second"),
            "count",
        )
        .expect("successor");
        assert_eq!(output_count, 0);
        assert_eq!(first_status, "pending");
        assert_eq!(successor_count, 0);
    }
}

#[tokio::test]
async fn dispatch_faults_preserve_exact_boundary_state() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Owner(AtomicUsize);
    impl ActivationDispatchOwner for Owner {
        fn plan<'a>(
            &'a self,
            _activation: &'a PendingActivation,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<Option<ActivationDispatchPlan>, WorkflowStoreError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                Ok(Some(ActivationDispatchPlan {
                    side_effect: DispatchSideEffect::ReadOnly,
                    intent: serde_json::json!({"operation": "review"}),
                }))
            })
        }

        fn dispatch<'a>(
            &'a self,
            request: &'a PreparedActivationDispatch,
        ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, WorkflowStoreError>> + Send + 'a>>
        {
            Box::pin(async move {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(serde_json::json!({"identity": request.dispatch_identity}))
            })
        }
    }

    struct Fault(WorkflowDispatchBoundary);
    impl WorkflowDispatchFault for Fault {
        fn after_boundary(
            &self,
            boundary: WorkflowDispatchBoundary,
            _request: &PreparedActivationDispatch,
        ) -> Result<(), WorkflowStoreError> {
            if boundary == self.0 {
                return Err(WorkflowStoreError::InvalidData(format!(
                    "crash after {boundary:?}"
                )));
            }
            Ok(())
        }
    }

    for (boundary, expected_calls, expected_status, has_receipt) in [
        (
            WorkflowDispatchBoundary::IntentCommitted,
            0,
            "prepared",
            false,
        ),
        (
            WorkflowDispatchBoundary::OwnerAccepted,
            1,
            "prepared",
            false,
        ),
        (
            WorkflowDispatchBoundary::ReceiptCommitted,
            1,
            "admitted",
            true,
        ),
    ] {
        let (temp, mut store) = initialized_store();
        let owner = Owner(AtomicUsize::new(0));
        assert!(
            store
                .dispatch_pending_activations_with_fault(&owner, &Fault(boundary), 10, 20)
                .await
                .is_err()
        );
        assert_eq!(owner.0.load(Ordering::SeqCst), expected_calls);
        drop(store);
        let reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
        let attempt = reopened
            .attempt_history("run-1", None, 10)
            .expect("attempt")
            .pop()
            .expect("row");
        assert_eq!(attempt.status, expected_status);
        assert_eq!(attempt.has_receipt, has_receipt);
    }
}

#[tokio::test]
async fn prepared_read_only_redispatch_reuses_identity_and_persists_receipt() {
    use std::sync::Mutex;

    struct Owner(Mutex<Vec<(String, serde_json::Value)>>);
    impl ActivationDispatchOwner for Owner {
        fn plan<'a>(
            &'a self,
            _activation: &'a PendingActivation,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<Option<ActivationDispatchPlan>, WorkflowStoreError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move { unreachable!("redispatch uses the already committed intent") })
        }

        fn dispatch<'a>(
            &'a self,
            request: &'a PreparedActivationDispatch,
        ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, WorkflowStoreError>> + Send + 'a>>
        {
            Box::pin(async move {
                self.0
                    .lock()
                    .expect("calls")
                    .push((request.dispatch_identity.clone(), request.intent.clone()));
                Ok(serde_json::json!({"owner": "test"}))
            })
        }
    }

    let (temp, mut store) = initialized_store();
    let activation = activation_id();
    let intent = serde_json::json!({
        "operation": "review",
        "preparation": {
            "version": 1,
            "owner_id": "bcode.test",
            "input_sha256": "a".repeat(64),
            "operation_facts": {"subject": "repository"},
            "descriptor": {"nonce": "prepared-once"},
            "diagnostics": []
        }
    });
    let identity = store
        .prepare_pending_activation(
            "run-1",
            "review",
            &activation,
            DispatchSideEffect::ReadOnly,
            intent.clone(),
            12,
        )
        .expect("prepare")
        .expect("prepared")
        .dispatch_identity;
    drop(store);
    let mut reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
    let owner = Owner(Mutex::new(Vec::new()));
    let admitted = reopened
        .redispatch_prepared_read_only(&owner, 10, 20)
        .await
        .expect("redispatch");
    assert_eq!(admitted.as_slice(), std::slice::from_ref(&identity));
    let prepared = reopened
        .reconcile_prepared_attempts(10, 22)
        .expect("prepared attempts");
    assert!(
        prepared.safe_prepared.is_empty(),
        "receipt persistence closes redispatch scan"
    );
    let calls = owner.0.lock().expect("calls").clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, identity);
    assert_eq!(
        calls[0].1, intent,
        "restart redispatch must invoke the owner with the exact persisted preparation"
    );
    let attempt = reopened
        .attempt_history("run-1", None, 10)
        .expect("attempt")
        .pop()
        .expect("row");
    assert_eq!(attempt.status, "admitted");
    assert!(attempt.has_receipt);
    assert!(
        reopened
            .redispatch_prepared_read_only(&owner, 10, 21)
            .await
            .expect("idempotent scan")
            .is_empty()
    );
}

#[test]
fn crash_boundaries_reopen_without_blind_mutation_redispatch() {
    struct Observer;
    impl AttemptStatusObserver for Observer {
        fn observe(
            &self,
            request: &AttemptReconciliationRequest,
        ) -> Result<AttemptObservation, WorkflowStoreError> {
            Ok(AttemptObservation::Succeeded {
                output: ValidatedOutput {
                    output_id: "output-after-restart".to_string(),
                    run_id: request.run_id.clone(),
                    node_id: request.node_id.clone(),
                    activation_id: request.activation_id.clone(),
                    schema_id: "u32".to_string(),
                    schema_version: 1,
                    value: serde_json::json!(1),
                    artifact_reference: None,
                    created_at_ms: 30,
                },
            })
        }
    }

    let temp = tempfile::tempdir().expect("temp");
    let attempt = PreparedAttempt {
        run_id: "run-1".to_string(),
        node_id: "review".to_string(),
        activation_id: activation_id(),
        attempt: 1,
        side_effect: DispatchSideEffect::Mutating,
        intent: serde_json::json!({"operation": "apply"}),
        prepared_at_ms: 12,
    };
    let identity = {
        let mut store = initialized_store_at(temp.path());
        store.prepare_attempt(&attempt).expect("prepare")
    };
    {
        let mut reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
        let summary = reopened
            .reconcile_prepared_attempts(10, 20)
            .expect("reconcile missing receipt");
        assert_eq!(summary.repair_required, [identity]);
    }

    let temp = tempfile::tempdir().expect("temp");
    let identity = {
        let mut store = initialized_store_at(temp.path());
        let identity = store.prepare_attempt(&attempt).expect("prepare");
        store
            .persist_dispatch_receipt(&DispatchReceipt {
                run_id: attempt.run_id.clone(),
                node_id: attempt.node_id.clone(),
                activation_id: attempt.activation_id.clone(),
                attempt: attempt.attempt,
                dispatch_identity: identity.clone(),
                receipt: serde_json::json!({"turn_id": "turn-1"}),
                admitted_at_ms: 13,
            })
            .expect("receipt");
        identity
    };
    let mut reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    let summary = reopened
        .reconcile_receipt_backed_attempts(&Observer, 10, 30)
        .expect("receipt reconciliation");
    assert_eq!(summary.succeeded, [identity]);
    assert_eq!(
        reopened
            .attempt_history("run-1", None, 10)
            .expect("attempts")[0]
            .status,
        "succeeded"
    );
}

#[test]
fn parallel_and_output_crash_boundaries_reconcile_independently() {
    struct Observer;
    impl AttemptStatusObserver for Observer {
        fn observe(
            &self,
            request: &AttemptReconciliationRequest,
        ) -> Result<AttemptObservation, WorkflowStoreError> {
            Ok(AttemptObservation::Succeeded {
                output: ValidatedOutput {
                    output_id: format!("output-{}", request.activation_id),
                    run_id: request.run_id.clone(),
                    node_id: request.node_id.clone(),
                    activation_id: request.activation_id.clone(),
                    schema_id: "u32".to_string(),
                    schema_version: 1,
                    value: serde_json::json!(1),
                    artifact_reference: None,
                    created_at_ms: 30,
                },
            })
        }
    }

    let temp = tempfile::tempdir().expect("temp");
    {
        let mut store = initialized_store_at(temp.path());
        store
            .create_activation(&NewActivation {
                activation_id: "activation-2".to_string(),
                created_at_ms: 12,
                ..new_activation()
            })
            .expect("parallel activation");
        let first = PreparedAttempt {
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: activation_id(),
            attempt: 1,
            side_effect: DispatchSideEffect::ReadOnly,
            intent: serde_json::json!({"operation": "review"}),
            prepared_at_ms: 13,
        };
        let first_identity = store.prepare_attempt(&first).expect("first prepare");
        store
            .persist_dispatch_receipt(&DispatchReceipt {
                run_id: first.run_id.clone(),
                node_id: first.node_id.clone(),
                activation_id: first.activation_id.clone(),
                attempt: first.attempt,
                dispatch_identity: first_identity,
                receipt: serde_json::json!({"turn_id": "turn-1"}),
                admitted_at_ms: 14,
            })
            .expect("first receipt");
        store
            .prepare_attempt(&PreparedAttempt {
                activation_id: "activation-2".to_string(),
                prepared_at_ms: 15,
                ..first
            })
            .expect("second prepare");
    }
    let mut reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    let receipt_summary = reopened
        .reconcile_receipt_backed_attempts(&Observer, 10, 30)
        .expect("first branch");
    assert_eq!(receipt_summary.succeeded.len(), 1);
    let prepared_summary = reopened
        .reconcile_prepared_attempts(10, 31)
        .expect("second branch");
    assert_eq!(prepared_summary.safe_prepared.len(), 1);
    let attempts = reopened
        .attempt_history("run-1", None, 10)
        .expect("attempts");
    assert_eq!(attempts.len(), 2);
    assert!(attempts.iter().any(|attempt| attempt.status == "succeeded"));
    assert!(attempts.iter().any(|attempt| attempt.status == "prepared"));

    drop(reopened);
    let reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("second reopen");
    let outputs = fixture_scalar_unsigned(
        &reopened.connection,
        &select("workflow_outputs").project_as(switchy_database::query::count_all(), "count"),
        "count",
    )
    .expect("outputs");
    let completed = fixture_scalar_unsigned(
        &reopened.connection,
        &select("workflow_activations")
            .project_as(switchy_database::query::count_all(), "count")
            .where_eq("status", "completed"),
        "count",
    )
    .expect("completed activations");
    assert_eq!(outputs, 1);
    assert_eq!(completed, 1);
}

#[tokio::test]
async fn dispatch_admission_enforces_declared_resources_and_run_concurrency() {
    struct Owner;
    impl ActivationDispatchOwner for Owner {
        fn plan<'a>(
            &'a self,
            _activation: &'a PendingActivation,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<Option<ActivationDispatchPlan>, WorkflowStoreError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                Ok(Some(ActivationDispatchPlan {
                    side_effect: DispatchSideEffect::ReadOnly,
                    intent: serde_json::json!({"operation": "read"}),
                }))
            })
        }

        fn dispatch<'a>(
            &'a self,
            _request: &'a PreparedActivationDispatch,
        ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, WorkflowStoreError>> + Send + 'a>>
        {
            Box::pin(async { Ok(serde_json::json!({"accepted": true})) })
        }
    }

    let temp = tempfile::tempdir().expect("temp");
    let mut definition = parallel_join_definition();
    for node in ["left", "right"] {
        definition.nodes.get_mut(node).expect("worker").resources =
            vec![bcode_workflow::ResourceClaim::write("repository")];
    }
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("parallel-resource", 1, &definition)
        .expect("definition");
    store
        .create_run(&NewWorkflowRun {
            run_id: "parallel-resource-run".to_string(),
            definition_id: "parallel-resource".to_string(),
            definition_version: 1,
            workspace_snapshot: "snapshot".to_string(),
            parent_session_id: Some("test-session".into()),
            parent_session_generation: None,
            binding: None,
            authored_provenance: None,
            input: Some(serde_json::json!(1)),
            execution_authority: Some(resource_run_authority()),
            created_at_ms: 1,
            authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                version: 1,
                provider_id: "test-policy".to_string(),
                profile_id: "build".to_string(),
                policy_digest_sha256: "a".repeat(64),
            },
            authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
            limits: WorkflowRunLimits {
                concurrency_cap: Some(2),
                ..WorkflowRunLimits::default()
            },
        })
        .expect("run");
    let summary = store
        .dispatch_pending_activations(&Owner, 10, 2)
        .await
        .expect("dispatch");
    assert_eq!(summary.admitted.len(), 1);
    assert_eq!(summary.raced.len(), 1);
    let leases = store
        .resource_leases_for_run("parallel-resource-run", 10)
        .expect("leases");
    assert_eq!(leases.len(), 1);
    // With the resource lease released, concurrency alone must defer the second
    // worker without preparing another attempt or changing its pending state.
    fixture_release_parallel_resource(&store.connection);
    block_on(
        update("workflow_runs")
            .value("concurrency_cap", 1)
            .where_eq("run_id", "parallel-resource-run")
            .execute(&store.connection),
    )
    .expect("single execution slot");
    let summary = store
        .dispatch_pending_activations(&Owner, 10, 4)
        .await
        .expect("capacity defers");
    assert!(summary.admitted.is_empty());
    assert_eq!(summary.raced.len(), 1);
    assert_eq!(store.pending_activations(10).unwrap().len(), 1);
    let attempts = store
        .attempt_history("parallel-resource-run", None, 10)
        .unwrap();
    assert_eq!(attempts.len(), 1);
    let leases = store
        .resource_leases_for_run("parallel-resource-run", 10)
        .unwrap();
    assert!(leases.is_empty());
    let first_dispatch = &attempts[0].dispatch_identity;
    assert_deferred_worker_resumes(&mut store, &Owner, temp.path(), first_dispatch).await;
}

fn fixture_release_parallel_resource(db: &dyn Database) {
    block_on(
        update("workflow_resource_leases")
            .value("released_at_ms", 3)
            .where_eq("run_id", "parallel-resource-run")
            .execute(db),
    )
    .expect("release resource fixture");
}

fn resource_run_authority() -> WorkflowExecutionAuthority {
    WorkflowExecutionAuthority {
        target_artifact_id: "test-artifact".into(),
        daemon_instance_id: "test-daemon".into(),
        generation: 1,
        fencing_token: "test-fence".into(),
    }
}

async fn assert_deferred_worker_resumes(
    store: &mut WorkflowStore,
    owner: &impl ActivationDispatchOwner,
    path: &Path,
    first_dispatch: &str,
) {
    block_on(
        update("workflow_runs")
            .value("node_execution_cap", 1)
            .where_eq("run_id", "parallel-resource-run")
            .execute(&store.connection),
    )
    .unwrap();
    let exhausted = store
        .dispatch_pending_activations(owner, 10, 5)
        .await
        .unwrap();
    assert_eq!(exhausted.exhausted.len(), 1);
    assert!(exhausted.admitted.is_empty());
    store
        .apply_attempt_observation(first_dispatch, AttemptObservation::Cancelled, 5)
        .unwrap();
    let mut reopened = WorkflowStore::open_in_state_dir(path).unwrap();
    let exhausted = reopened
        .dispatch_pending_activations(owner, 10, 6)
        .await
        .unwrap();
    assert_eq!(exhausted.exhausted.len(), 1);
    assert!(exhausted.admitted.is_empty());
    let authority = reopened
        .execution_authority("parallel-resource-run")
        .unwrap()
        .unwrap();
    reopened
        .increase_execution_allowance("parallel-resource-run", &authority, 1, 1000, 6)
        .unwrap();
    let before_retry = fixture_changes(&reopened.connection);
    reopened
        .increase_execution_allowance("parallel-resource-run", &authority, 1, 1000, 6)
        .unwrap();
    assert_eq!(fixture_changes(&reopened.connection), before_retry);
    let summary = reopened
        .dispatch_pending_activations(owner, 10, 6)
        .await
        .unwrap();
    assert_eq!(summary.admitted.len(), 1);
    assert!(summary.raced.is_empty());
    let attempts = reopened
        .attempt_history("parallel-resource-run", None, 10)
        .unwrap();
    assert_eq!(attempts.len(), 2);
    assert_eq!(
        attempts
            .iter()
            .filter(|attempt| attempt.dispatch_identity == first_dispatch)
            .count(),
        1
    );
    assert!(reopened.pending_activations(10).unwrap().is_empty());
}

#[test]
#[allow(clippy::too_many_lines)]
fn decisions_grants_leases_and_checkpoints_are_durable_and_fail_closed() {
    let (temp, mut store) = initialized_store();
    let decision = WorkflowDecision {
        decision_id: "decision-1".to_string(),
        run_id: "run-1".to_string(),
        node_id: Some("review".to_string()),
        decision_type: "branch".to_string(),
        value: serde_json::json!({"selected": "approve"}),
        created_at_ms: 12,
    };
    store.persist_decision(&decision).expect("decision");
    store
        .persist_decision(&decision)
        .expect("idempotent decision");
    assert_eq!(
        store.decision("decision-1").expect("load decision"),
        Some(decision.clone())
    );
    assert!(
        store
            .persist_decision(&WorkflowDecision {
                value: serde_json::json!({"selected": "reject"}),
                ..decision.clone()
            })
            .is_err()
    );

    let grant = WorkflowGrant {
        grant_id: "grant-1".to_string(),
        run_id: "run-1".to_string(),
        node_id: "review".to_string(),
        scope: serde_json::json!({"workspace": "snapshot-1", "tools": ["git_commit"]}),
        granted_at_ms: 13,
        expires_at_ms: Some(100),
        max_uses: Some(2),
        uses_consumed: 0,
    };
    store.persist_grant(&grant).expect("grant");
    store.persist_grant(&grant).expect("idempotent grant");
    assert_eq!(
        store.grant("grant-1").expect("load grant"),
        Some(grant.clone())
    );
    assert_eq!(
        store
            .consume_grant("grant-1", 20)
            .expect("first consumption")
            .uses_consumed,
        1
    );
    assert_eq!(
        store
            .consume_grant("grant-1", 21)
            .expect("second consumption")
            .uses_consumed,
        2
    );
    assert!(store.consume_grant("grant-1", 22).is_err());

    let stale_grant = WorkflowGrant {
        grant_id: "grant-stale".to_string(),
        granted_at_ms: 13,
        expires_at_ms: Some(19),
        ..grant.clone()
    };
    store.persist_grant(&stale_grant).expect("stale grant");
    assert!(store.consume_grant("grant-stale", 20).is_err());

    let read = WorkflowResourceLease {
        lease_id: "lease-read-1".to_string(),
        run_id: "run-1".to_string(),
        node_id: "review".to_string(),
        activation_id: activation_id(),
        resource_key: "repository".to_string(),
        mode: ResourceLeaseMode::Read,
        acquired_at_ms: 14,
        expires_at_ms: None,
    };
    store.acquire_resource_lease(&read).expect("read lease");
    store
        .acquire_resource_lease(&read)
        .expect("idempotent lease");
    let second_read = WorkflowResourceLease {
        lease_id: "lease-read-2".to_string(),
        ..read.clone()
    };
    store
        .acquire_resource_lease(&second_read)
        .expect("compatible read lease");
    assert!(
        store
            .acquire_resource_lease(&WorkflowResourceLease {
                lease_id: "lease-write".to_string(),
                mode: ResourceLeaseMode::Write,
                ..read.clone()
            })
            .is_err()
    );
    store
        .release_resource_lease("lease-read-1", 15)
        .expect("release first");
    store
        .release_resource_lease("lease-read-2", 16)
        .expect("release second");
    assert!(store.acquire_resource_lease(&second_read).is_err());
    store
        .acquire_resource_lease(&WorkflowResourceLease {
            lease_id: "lease-write".to_string(),
            mode: ResourceLeaseMode::Write,
            acquired_at_ms: 17,
            ..read
        })
        .expect("write after readers");

    let event_tail = fixture_scalar_unsigned(
        &store.connection,
        &select("workflow_events").project_as(
            switchy_database::query::max(switchy_database::query::identifier("event_seq")),
            "tail",
        ),
        "tail",
    )
    .expect("tail");
    let checkpoint = WorkflowProjectionCheckpoint {
        projection_name: "run_summary".to_string(),
        projection_version: 1,
        last_event_seq: event_tail,
    };
    store
        .advance_projection_checkpoint(&checkpoint)
        .expect("checkpoint");
    assert_eq!(
        store
            .projection_checkpoint("run_summary")
            .expect("load checkpoint"),
        Some(checkpoint.clone())
    );
    assert!(
        store
            .advance_projection_checkpoint(&WorkflowProjectionCheckpoint {
                last_event_seq: event_tail.saturating_sub(1),
                ..checkpoint.clone()
            })
            .is_err()
    );
    drop(store);
    let reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
    assert_eq!(
        reopened.decision("decision-1").expect("restored decision"),
        Some(decision)
    );
    assert_eq!(
        reopened
            .grant("grant-1")
            .expect("restored grant")
            .expect("grant")
            .uses_consumed,
        2
    );
    assert_eq!(
        reopened
            .projection_checkpoint("run_summary")
            .expect("restored checkpoint"),
        Some(checkpoint)
    );
    assert!(
        !fixture_rows(
            &reopened.connection,
            &select("workflow_resource_leases")
                .columns(&["lease_id"])
                .where_eq("lease_id", "lease-write")
                .where_eq("released_at_ms", DatabaseValue::Null)
        )
        .is_empty()
    );
}

#[test]
fn target_input_mismatch_returns_actionable_typed_diagnostic_atomically() {
    let temp = tempfile::tempdir().expect("temp");
    let mut definition = sequential_definition();
    definition
        .nodes
        .get_mut("second")
        .expect("second")
        .input
        .schema = serde_json::json!({"type": "string"});
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("typed-mismatch", 1, &definition)
        .expect("definition");
    store
        .create_run(&NewWorkflowRun {
            run_id: "typed-mismatch-run".to_string(),
            definition_id: "typed-mismatch".to_string(),
            definition_version: 1,
            workspace_snapshot: "snapshot".to_string(),
            parent_session_id: None,
            parent_session_generation: None,
            binding: None,
            authored_provenance: None,
            input: Some(serde_json::json!(1)),
            execution_authority: None,
            created_at_ms: 1,
            authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                version: 1,
                provider_id: "test-policy".to_string(),
                profile_id: "build".to_string(),
                policy_digest_sha256: "a".repeat(64),
            },
            authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
            limits: WorkflowRunLimits::default(),
        })
        .expect("run");
    let error = store
        .persist_validated_output(&ValidatedOutput {
            output_id: "first-output".to_string(),
            run_id: "typed-mismatch-run".to_string(),
            node_id: "first".to_string(),
            activation_id: activation_identity("typed-mismatch-run", "first", 0),
            schema_id: definition.nodes["first"].output.type_name.clone(),
            schema_version: 1,
            value: serde_json::json!(2),
            artifact_reference: None,
            created_at_ms: 2,
        })
        .expect_err("mismatch");
    assert!(matches!(
        &error,
        WorkflowStoreError::TargetInputValidation(failure)
            if failure.run_id == "typed-mismatch-run"
                && failure.source_node_id == "first"
                && failure.source_activation_id
                    == activation_identity("typed-mismatch-run", "first", 0)
                && failure.target_node_id == "second"
                && failure.target_schema_id == definition.nodes["second"].input.type_name
                && failure.message.contains("does not match schema")
    ));
    let output_count: u64 = fixture_scalar_unsigned(
        &store.connection,
        &select("workflow_outputs")
            .project_as(switchy_database::query::count_all(), "count")
            .where_eq("run_id", "typed-mismatch-run"),
        "count",
    )
    .expect("outputs");
    assert_eq!(output_count, 0);
    assert!(
        store
            .pending_activations(10)
            .expect("pending")
            .iter()
            .all(|activation| activation.node_id != "second")
    );
}

#[test]
fn parallel_join_does_not_hide_later_invalid_or_duplicate_members() {
    let (_temp, mut store) = initialized_store();
    let mut target = definition("example").nodes["review"].clone();
    store
        .persist_validated_output(&ValidatedOutput {
            output_id: "result".to_string(),
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: activation_id(),
            schema_id: target.output.type_name.clone(),
            schema_version: 1,
            value: serde_json::json!(2),
            artifact_reference: None,
            created_at_ms: 12,
        })
        .expect("output");
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    let db: &dyn Database = transaction.as_ref();
    for (exits, diagnostic) in [
        (
            serde_json::json!(["review", "absent"]),
            "missing run-graph member",
        ),
        (
            serde_json::json!(["review", "review"]),
            "ambiguous completed",
        ),
    ] {
        target.configuration = serde_json::json!({"left_exits": exits, "right_exits": ["review"]});
        let before = fixture_changes(db);
        assert!(
            parallel_join_members(db, "run-1", &target, 0)
                .expect_err("invalid members")
                .to_string()
                .contains(diagnostic)
        );
        assert_eq!(fixture_changes(db), before);
    }
    target.configuration = serde_json::json!({"left_exits": ["review"], "right_exits": ["review"]});
    assert_eq!(
        parallel_join_members(db, "run-1", &target, 0).expect("members"),
        (serde_json::json!(2), serde_json::json!(2))
    );
}

#[test]
fn retained_binding_storage_round_trips_and_rolls_back() {
    let (temp, store) = initialized_store();
    let request = bcode_workflow::WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: "run-1".into(),
        mutation_id: "binding-fixture".into(),
        expected_revision: 1,
        edits: vec![],
        reconciliation: vec![
            bcode_workflow::WorkflowRunGraphReconciliation::RetainWithBindings {
                activation_id: activation_id(),
                edge_ids: vec![42],
            },
        ],
    };
    block_on(
        insert("workflow_graph_edit_candidates")
            .value("run_id", "run-1")
            .value("mutation_id", "binding-fixture")
            .value("expected_revision", 1)
            .value("request_json", "{}")
            .value("authority_json", "{}")
            .value("created_at_ms", 1)
            .execute(&store.connection),
    )
    .expect("candidate fixture");
    block_on(
        insert("workflow_graph_edit_publications")
            .value("run_id", "run-1")
            .value("mutation_id", "binding-fixture")
            .value("revision", 2)
            .execute(&store.connection),
    )
    .expect("publication fixture");
    block_on(
        insert("workflow_run_graph_edges")
            .value("run_id", "run-1")
            .value("edge_id", 42)
            .value("revision", 2)
            .value("source_node_id", "review")
            .value("target_node_id", "review")
            .value("edge_json", "{}")
            .execute(&store.connection),
    )
    .expect("edge fixture");
    for commit in [false, true] {
        let transaction =
            database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
        run_graph::persist_retained_edge_bindings(transaction.as_ref(), &request, 2)
            .expect("binding");
        let count = fixture_scalar_unsigned(
            transaction.as_ref(),
            &select("workflow_retained_edge_bindings")
                .project_as(switchy_database::query::count_all(), "count"),
            "count",
        )
        .expect("count");
        assert_eq!(count, 1);
        if commit {
            database::commit(transaction).expect("commit");
        } else {
            block_on(transaction.rollback()).expect("rollback");
        }
    }
    drop(store);
    let store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    let rows = fixture_rows(
        &store.connection,
        &select("workflow_retained_edge_bindings")
            .columns(&["edge_revision", "source_activation_id"])
            .where_eq("run_id", "run-1")
            .where_eq("graph_revision", 2)
            .where_eq("edge_id", 42),
    );
    let row = rows.first().expect("durable binding");
    let binding = (
        fixture_unsigned(row, "edge_revision"),
        fixture_text(row, "source_activation_id"),
    );
    assert_eq!(binding, (2, activation_id()));
}

#[test]
fn retained_readiness_rejects_unknown_status_without_mutation() {
    let (_temp, store) = initialized_store();
    assert!(
        !retained_activation_ready(&store.connection, "run-1", "review", &activation_id())
            .expect("unfinished")
    );
    block_on(
        update("workflow_activations")
            .value("status", "future_status")
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("unknown status");
    let before = fixture_changes(&store.connection);
    assert!(matches!(
        retained_activation_ready(&store.connection, "run-1", "review", &activation_id()),
        Err(WorkflowStoreError::InvalidData(_))
    ));
    assert_eq!(fixture_changes(&store.connection), before);
}

#[test]
fn completed_result_reconciliation_requires_explicit_binding_and_valid_output() {
    use bcode_workflow::WorkflowRunGraphReconciliation as Disposition;
    let (_temp, mut store) = initialized_store();
    store
        .persist_validated_output(&ValidatedOutput {
            output_id: "retained-result".to_string(),
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: activation_id(),
            schema_id: definition("example").nodes["review"]
                .output
                .type_name
                .clone(),
            schema_version: 1,
            value: serde_json::json!(2),
            artifact_reference: None,
            created_at_ms: 12,
        })
        .expect("completed output");
    let mut request = bcode_workflow::WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: "run-1".to_string(),
        mutation_id: "retain-result".to_string(),
        expected_revision: 1,
        edits: vec![],
        reconciliation: vec![Disposition::RetainWithBindings {
            activation_id: activation_id(),
            edge_ids: vec![1],
        }],
    };
    let before = fixture_changes(&store.connection);
    run_graph::validate_reconciliation_targets(&store.connection, &request)
        .expect("explicit completed-result binding");
    request.reconciliation = vec![Disposition::Retain {
        activation_id: activation_id(),
    }];
    run_graph::validate_reconciliation_targets(&store.connection, &request)
        .expect("unchanged completed node can be retained");
    let mut replacement = definition("example").nodes["review"].clone();
    replacement.name = "rewritten retained node".into();
    request.edits = vec![bcode_workflow::WorkflowRunGraphEdit::ReplaceNode {
        node: replacement,
        entry: true,
        exit: true,
    }];
    assert!(run_graph::validate_reconciliation_targets(&store.connection, &request).is_err());
    request.edits.clear();
    for disposition in [
        Disposition::Cancel {
            activation_id: activation_id(),
        },
        Disposition::RetainWithBindings {
            activation_id: "missing".into(),
            edge_ids: vec![1],
        },
    ] {
        request.reconciliation = vec![disposition];
        assert!(run_graph::validate_reconciliation_targets(&store.connection, &request).is_err());
    }
    assert_eq!(fixture_changes(&store.connection), before);
    request.reconciliation = vec![Disposition::RetainWithBindings {
        activation_id: activation_id(),
        edge_ids: vec![1],
    }];
    block_on(
        update("workflow_outputs")
            .value("checksum_sha256", "bad")
            .execute(&store.connection),
    )
    .expect("damage");
    assert!(run_graph::validate_reconciliation_targets(&store.connection, &request).is_err());
    request.reconciliation = vec![Disposition::Retain {
        activation_id: activation_id(),
    }];
    assert!(run_graph::validate_reconciliation_targets(&store.connection, &request).is_err());
}

#[test]
fn completed_member_output_is_scoped_to_node_and_generation() {
    let (_temp, mut store) = initialized_store();
    store
        .persist_validated_output(&ValidatedOutput {
            output_id: "result".to_string(),
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: activation_id(),
            schema_id: definition("example").nodes["review"]
                .output
                .type_name
                .clone(),
            schema_version: 1,
            value: serde_json::json!(2),
            artifact_reference: None,
            created_at_ms: 12,
        })
        .expect("output");
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    let db: &dyn Database = transaction.as_ref();
    // A later generation may exist without a result. It must neither conflict
    // with the completed generation nor borrow that generation's output.
    block_on(
        insert("workflow_activations")
            .value("run_id", "run-1")
            .value("node_id", "review")
            .value("activation_id", "later")
            .value("dependency_generation", 1)
            .value("input_json", "1")
            .value("status", "pending")
            .value("created_at_ms", 13)
            .execute(db),
    )
    .expect("later generation");
    let before = fixture_changes(db);
    assert_eq!(
        activation_output_value(db, "run-1", "review", 0).expect("completed generation"),
        serde_json::json!(2)
    );
    for (run_id, node_id, generation) in [
        ("run-1", "review", 1),
        ("run-1", "absent", 0),
        ("absent", "review", 0),
    ] {
        assert!(matches!(
            activation_output_value(db, run_id, node_id, generation),
            Err(WorkflowStoreError::StructuredDatabase(
                switchy_database::DatabaseError::NoRow
            ))
        ));
    }
    assert_eq!(fixture_changes(db), before);
}

#[test]
fn completed_member_output_damage_is_not_an_unfinished_member() {
    for damage in 0..6 {
        let (_temp, mut store) = initialized_store();
        store
            .persist_validated_output(&ValidatedOutput {
                output_id: "result".to_string(),
                run_id: "run-1".to_string(),
                node_id: "review".to_string(),
                activation_id: activation_id(),
                schema_id: definition("example").nodes["review"]
                    .output
                    .type_name
                    .clone(),
                schema_version: 1,
                value: serde_json::json!(2),
                artifact_reference: None,
                created_at_ms: 12,
            })
            .expect("output");
        let transaction =
            database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
        let db: &dyn Database = transaction.as_ref();
        assert_eq!(
            activation_output_value(db, "run-1", "review", 0).expect("value"),
            serde_json::json!(2)
        );
        block_on(transaction.sqlite_defer_foreign_keys(true)).expect("damage fixture constraints");
        match damage {
            0 => {
                block_on(
                    insert("workflow_activations")
                        .value("run_id", "run-1")
                        .value("node_id", "review")
                        .value("activation_id", "conflicting")
                        .value("dependency_generation", 0)
                        .value("input_json", "1")
                        .value("status", "completed")
                        .value("created_at_ms", 13)
                        .execute(db),
                )
                .expect("damage fixture");
            }
            1 => {
                block_on(
                    update("workflow_activations")
                        .value("output_id", DatabaseValue::Null)
                        .where_eq("node_id", "review")
                        .execute(db),
                )
                .expect("damage fixture");
            }
            2 => {
                block_on(
                    update("workflow_outputs")
                        .value("checksum_sha256", "wrong")
                        .execute(db),
                )
                .expect("damage fixture");
            }
            3 => {
                block_on(
                    update("workflow_outputs")
                        .value("checksum_sha256", cast_text(zero_blob(1_048_577)))
                        .execute(db),
                )
                .expect("damage fixture");
            }
            4 => {
                block_on(
                    update("workflow_outputs")
                        .value("activation_id", "different")
                        .execute(db),
                )
                .expect("damage fixture");
            }
            _ => {
                block_on(
                    update("workflow_outputs")
                        .value("value_json", "not-json")
                        .execute(db),
                )
                .expect("damage fixture");
            }
        }
        let before = fixture_changes(db);
        let error = activation_output_value(db, "run-1", "review", 0).expect_err("damaged output");
        assert!(matches!(error, WorkflowStoreError::InvalidData(_)));
        assert_eq!(fixture_changes(db), before);
    }
}

#[test]
fn parallel_join_rejects_missing_admitted_member_before_output_lookup() {
    let (_temp, store) = initialized_store();
    let mut target = definition("join").nodes["review"].clone();
    target.configuration = serde_json::json!({
        "left_exits": ["absent", "review"],
        "right_exits": ["review"],
    });
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    let db: &dyn Database = transaction.as_ref();
    let before = fixture_changes(db);
    let error = parallel_join_members(db, "run-1", &target, 0).expect_err("missing member");
    assert!(
        error
            .to_string()
            .contains("missing run-graph member: absent")
    );
    assert_eq!(fixture_changes(db), before);
    target.configuration["left_exits"] = serde_json::json!(["review"]);
    let error = parallel_join_members(db, "run-1", &target, 0).expect_err("not completed");
    assert!(error.to_string().contains("no completed left_exits"));
    block_on(
        update("workflow_run_graph_nodes")
            .value("node_json", "{}")
            .where_eq("node_id", "review")
            .execute(db),
    )
    .expect("damage fixture");
    let before = fixture_changes(db);
    let error = parallel_join_members(db, "run-1", &target, 0).expect_err("damaged member");
    assert!(!error.to_string().contains("no completed left_exits"));
    assert_eq!(fixture_changes(db), before);
}

#[test]
fn branch_skip_validates_all_admitted_nodes_before_writes() {
    let (_temp, store) = initialized_store();
    let nodes = vec!["review".to_string(), "missing".to_string()];
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    let db: &dyn Database = transaction.as_ref();
    let before = fixture_changes(db);
    assert!(skip_branch_nodes(db, "run-1", 1, &nodes, 20).is_err());
    assert_eq!(fixture_changes(db), before);
    skip_branch_nodes(db, "run-1", 1, &nodes[..1], 20).expect("admitted node");
    let status = fixture_scalar_text(
        db,
        &select("workflow_activations")
            .columns(&["status"])
            .where_eq("run_id", "run-1")
            .where_eq("node_id", "review")
            .where_eq("dependency_generation", 1),
        "status",
    )
    .expect("skip");
    assert_eq!(status, "skipped");
    let before = fixture_changes(db);
    skip_branch_nodes(db, "run-1", 1, &nodes[..1], 20).expect("duplicate skip");
    assert_eq!(fixture_changes(db), before);
    block_on(
        update("workflow_run_graph_nodes")
            .value("node_json", "{}")
            .where_eq("node_id", "review")
            .execute(db),
    )
    .expect("damage fixture");
    let before = fixture_changes(db);
    assert!(skip_branch_nodes(db, "run-1", 2, &nodes[..1], 21).is_err());
    assert_eq!(fixture_changes(db), before);
}

#[test]
fn branch_target_input_validation_rolls_back_selected_activation() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    let mut definition = conditional_definition();
    definition
        .nodes
        .get_mut("selected")
        .expect("selected")
        .input
        .schema = serde_json::json!({"type": "string"});
    store
        .persist_definition("invalid-branch", 1, &definition)
        .expect("definition");
    let mut run = new_run();
    run.run_id = "invalid-branch-run".to_string();
    run.definition_id = "invalid-branch".to_string();
    store.create_run(&run).expect("run");
    store
        .persist_validated_output(&ValidatedOutput {
            output_id: "inspect-output".to_string(),
            run_id: run.run_id.clone(),
            node_id: "inspect".to_string(),
            activation_id: activation_identity(&run.run_id, "inspect", 0),
            schema_id: definition.nodes["inspect"].output.type_name.clone(),
            schema_version: 1,
            value: serde_json::json!(1),
            artifact_reference: None,
            created_at_ms: 2,
        })
        .expect("inspect output");
    let branch = store
        .pending_activations(10)
        .expect("branch")
        .into_iter()
        .find(|activation| activation.node_id == "choose")
        .expect("branch activation");
    let error = store
        .settle_pending_control_nodes(&run.run_id, 10, 3)
        .expect_err("selected target mismatch");
    assert!(matches!(
        &error,
        WorkflowStoreError::TargetInputValidation(failure)
            if failure.run_id == "invalid-branch-run"
                && failure.source_node_id == "choose"
                && failure.target_node_id == "selected"
    ));
    assert!(
        store
            .pending_activations(10)
            .expect("pending")
            .iter()
            .any(|activation| activation.activation_id == branch.activation_id)
    );
    assert!(
        store
            .pending_activations(10)
            .expect("pending")
            .iter()
            .all(|activation| activation.node_id != "selected")
    );
}

#[test]
fn repeat_completion_does_not_terminalize_unfinished_work() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("example", 1, &repeat_definition())
        .expect("definition");
    let mut run = new_run();
    run.input = Some(serde_json::json!({"condition_met": true}));
    store.create_run(&run).expect("run");
    let body = store
        .pending_activations(10)
        .expect("pending")
        .pop()
        .expect("body");
    store
        .persist_validated_output(&ValidatedOutput {
            output_id: "body-output".into(),
            run_id: run.run_id.clone(),
            node_id: body.node_id,
            activation_id: body.activation_id,
            schema_id: body.node.output.type_name,
            schema_version: 1,
            value: run.input.clone().expect("input"),
            artifact_reference: None,
            created_at_ms: 2,
        })
        .expect("body output");
    store
        .create_activation(&NewActivation {
            run_id: run.run_id.clone(),
            node_id: "body".into(),
            activation_id: "other-body".into(),
            dependency_generation: 9,
            input: run.input.clone(),
            created_at_ms: 3,
        })
        .expect("other work");
    let result = store
        .settle_pending_control_nodes(&run.run_id, 10, 4)
        .expect("settle repeat");
    assert!(result.activated.is_empty());
    assert_eq!(
        store
            .run_summary(&run.run_id)
            .expect("summary")
            .expect("run")
            .status,
        RunStatus::Running
    );
    drop(store);
    let store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    assert_eq!(
        store
            .run_summary(&run.run_id)
            .expect("summary")
            .expect("run")
            .status,
        RunStatus::Running
    );
}

#[test]
fn repeat_target_damage_rolls_back_control_settlement() {
    for (table, column, value, filter, identity) in repeat_target_damage_cases() {
        let temp = tempfile::tempdir().expect("temp");
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("example", 1, &repeat_definition())
            .expect("definition");
        let mut run = new_run();
        run.input = Some(serde_json::json!({"condition_met": false, "iteration": 1}));
        store.create_run(&run).expect("run");
        let body = store
            .pending_activations(10)
            .expect("pending")
            .pop()
            .expect("body");
        store
            .persist_validated_output(&ValidatedOutput {
                output_id: "body-output".to_string(),
                run_id: run.run_id.clone(),
                node_id: body.node_id,
                activation_id: body.activation_id,
                schema_id: body.node.output.type_name,
                schema_version: 1,
                value: run.input.clone().expect("input"),
                artifact_reference: None,
                created_at_ms: 2,
            })
            .expect("body output");
        block_on(store.connection.sqlite_ignore_check_constraints(true)).expect("fixture");
        if let Some(column) = column {
            block_on(
                update(table)
                    .value(column, value)
                    .where_eq(filter, identity)
                    .execute(&store.connection),
            )
            .expect("damage fixture");
        } else {
            block_on(
                delete(table)
                    .where_eq(filter, identity)
                    .execute(&store.connection),
            )
            .expect("damage fixture");
        }
        let events = store.event_history(&run.run_id, None, 100).expect("events");
        let outputs = fixture_scalar_unsigned(
            &store.connection,
            &select("workflow_outputs").project_as(switchy_database::query::count_all(), "count"),
            "count",
        )
        .expect("outputs");
        assert!(
            store
                .settle_pending_control_nodes(&run.run_id, 10, 3)
                .is_err()
        );
        assert_eq!(
            store.event_history(&run.run_id, None, 100).expect("events"),
            events
        );
        assert_eq!(
            fixture_scalar_unsigned(
                &store.connection,
                &select("workflow_outputs")
                    .project_as(switchy_database::query::count_all(), "count"),
                "count"
            )
            .expect("outputs"),
            outputs
        );
        let pending = store.pending_activations(10).expect("pending");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].node_id, "repeat-control");
    }
}

type RepeatTargetDamage = (
    &'static str,
    Option<&'static str>,
    DatabaseValue,
    &'static str,
    &'static str,
);

fn repeat_target_damage_cases() -> [RepeatTargetDamage; 6] {
    [
        (
            "workflow_run_graph_edges",
            Some("edge_json"),
            DatabaseValue::String("{}".into()),
            "source_node_id",
            "repeat-control",
        ),
        (
            "workflow_run_graph_edges",
            Some("target_node_id"),
            DatabaseValue::String("absent".into()),
            "source_node_id",
            "repeat-control",
        ),
        (
            "workflow_run_graph_edges",
            Some("revision"),
            DatabaseValue::Int64(2),
            "source_node_id",
            "repeat-control",
        ),
        (
            "workflow_run_graph_nodes",
            None,
            DatabaseValue::Null,
            "node_id",
            "body",
        ),
        (
            "workflow_run_graph_nodes",
            Some("node_json"),
            DatabaseValue::String("{}".into()),
            "node_id",
            "body",
        ),
        (
            "workflow_run_graph_nodes",
            Some("is_exit"),
            DatabaseValue::Int64(2),
            "node_id",
            "body",
        ),
    ]
}

#[test]
fn repeat_back_input_validation_rolls_back_generation() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    let mut definition = repeat_definition();
    definition.nodes.get_mut("body").expect("body").input.schema = serde_json::json!({
        "type": "object",
        "required": ["condition_met", "iteration"],
        "properties": {
            "condition_met": {"type": "boolean"},
            "iteration": {"const": 1}
        }
    });
    let back_edge = definition
        .edges
        .iter_mut()
        .find(|edge| matches!(edge.kind, bcode_workflow::EdgeKind::Back { .. }))
        .expect("back edge");
    back_edge
        .transform
        .as_mut()
        .expect("repeat transform")
        .output = definition.nodes["body"].input.clone();
    store
        .persist_definition("invalid-repeat", 1, &definition)
        .expect("definition");
    let mut run = new_run();
    run.run_id = "invalid-repeat-run".to_string();
    run.definition_id = "invalid-repeat".to_string();
    run.input = Some(serde_json::json!({"condition_met": false, "iteration": 1}));
    store.create_run(&run).expect("run");
    let body = store
        .pending_activations(10)
        .expect("body")
        .pop()
        .expect("body");
    store
        .persist_validated_output(&ValidatedOutput {
            output_id: "body-output".to_string(),
            run_id: run.run_id.clone(),
            node_id: "body".to_string(),
            activation_id: body.activation_id,
            schema_id: definition.nodes["body"].output.type_name.clone(),
            schema_version: 1,
            value: serde_json::json!({"condition_met": false, "iteration": 1}),
            artifact_reference: None,
            created_at_ms: 2,
        })
        .expect("body output");
    let repeat = store
        .pending_activations(10)
        .expect("repeat")
        .into_iter()
        .find(|activation| activation.node_id == "repeat-control")
        .expect("repeat activation");
    let error = store
        .settle_pending_control_nodes(&run.run_id, 10, 3)
        .expect_err("repeat target mismatch");
    assert!(error.to_string().contains("transform output"));
    assert!(
        store
            .pending_activations(10)
            .expect("pending")
            .iter()
            .any(|activation| activation.activation_id == repeat.activation_id)
    );
    assert_eq!(
        store
            .pending_activations(10)
            .expect("pending")
            .iter()
            .filter(|activation| activation.node_id == "body")
            .count(),
        0
    );
}

#[test]
fn idempotent_start_rejects_oversized_stored_json_without_mutation() {
    let (_temp, mut store) = initialized_store();
    for column in [
        "input_json",
        "authored_provenance_json",
        "authorization_profile_json",
    ] {
        let rows = fixture_rows(
            &store.connection,
            &select("workflow_runs")
                .columns(&[column])
                .where_eq("run_id", "run-1"),
        );
        let original = database::optional_text(rows.first().expect("original row"), column)
            .expect("original value");
        block_on(
            store.connection.exec_update_count(
                &(update("workflow_runs")
                    .value(
                        column,
                        cast_text(zero_blob(
                            u32::try_from(MAX_INLINE_JSON_BYTES + 1).expect("fixture byte limit"),
                        )),
                    )
                    .where_eq("run_id", "run-1")),
            ),
        )
        .expect("oversized value");
        let before = fixture_changes(&store.connection);
        assert!(
            matches!(
                store.create_run_idempotent(&new_run()),
                Err(WorkflowStoreError::InvalidData(_))
            ),
            "accepted or misclassified {column}"
        );
        assert_eq!(fixture_changes(&store.connection), before);
        block_on(
            store.connection.exec_update_count(
                &(update("workflow_runs")
                    .value(column, cast_blob("null"))
                    .where_eq("run_id", "run-1")),
            ),
        )
        .expect("blob JSON fixture");
        let before = fixture_changes(&store.connection);
        assert!(
            matches!(
                store.create_run_idempotent(&new_run()),
                Err(WorkflowStoreError::InvalidData(_))
            ),
            "accepted blob {column}"
        );
        assert_eq!(fixture_changes(&store.connection), before);
        block_on(
            store.connection.exec_update_count(
                &(update("workflow_runs")
                    .value(
                        column,
                        original.map_or(DatabaseValue::Null, DatabaseValue::String),
                    )
                    .where_eq("run_id", "run-1")),
            ),
        )
        .expect("restore fixture");
    }
    assert!(
        !store
            .create_run_idempotent(&new_run())
            .expect("valid duplicate")
    );
}

#[test]
fn attempt_history_rejects_unrepresentable_cursor_timestamp() {
    let (_temp, store) = initialized_store();
    let before = fixture_changes(&store.connection);
    for prepared_at_ms in [0, i64::MAX as u64, i64::MAX as u64 + 1, u64::MAX] {
        let cursor = super::AttemptCursor {
            prepared_at_ms,
            dispatch_identity: "dispatch".to_string(),
        };
        let result = store.attempt_history("run-1", Some(&cursor), 1);
        if i64::try_from(prepared_at_ms).is_ok() {
            assert!(result.expect("representable cursor").is_empty());
        } else {
            assert!(matches!(result, Err(WorkflowStoreError::InvalidData(_))));
        }
    }
    assert_eq!(fixture_changes(&store.connection), before);
}

#[test]
fn attempt_summary_validates_attempt_number_boundaries() {
    for number in [0, 1, i64::from(u32::MAX)] {
        let raw = fixture_attempt_summary("attempt", DatabaseValue::Int64(number).into())
            .expect("representable attempt fixture");
        let mut raw = raw;
        raw.4 = super::dispatch_identity(&raw.0, &raw.1, &raw.2, raw.3);
        let result = super::parse_attempt_summary(raw);
        if number == 0 {
            assert!(matches!(result, Err(WorkflowStoreError::InvalidData(_))));
        } else {
            assert_eq!(i64::from(result.expect("positive attempt").attempt), number);
        }
    }
    for number in [Some(-1), Some(i64::from(u32::MAX) + 1), None] {
        assert!(
            fixture_attempt_summary(
                "attempt",
                number
                    .map_or(DatabaseValue::Null, DatabaseValue::Int64)
                    .into()
            )
            .is_err()
        );
    }
}

#[test]
fn attempt_summary_preserves_supported_statuses() {
    for status in [
        "prepared",
        "admitted",
        "running",
        "cancelling",
        "sibling_cancelling",
        "succeeded",
        "failed",
        "cancelled",
        "repair_required",
        "paused",
        "abandoned",
    ] {
        let raw =
            fixture_attempt_summary("status", status.into()).expect("supported status fixture");
        let mut raw = raw;
        raw.4 = super::dispatch_identity(&raw.0, &raw.1, &raw.2, raw.3);
        let summary = super::parse_attempt_summary(raw).expect("supported status");
        assert_eq!(summary.status, status);
    }
}

#[test]
fn attempt_summary_rejects_unknown_status_without_exposing_payload() {
    for value in [
        FixtureStorageValue::Text("secret-bearing-future-status".to_string()),
        FixtureStorageValue::Text("x".repeat(MAX_ID_BYTES + 1)),
        FixtureStorageValue::BlobText("prepared".into()),
        FixtureStorageValue::Null,
    ] {
        let raw =
            fixture_attempt_summary("status", value.expression()).expect("invalid storage fixture");
        let error = super::parse_attempt_summary(raw).expect_err("reject status");
        assert!(matches!(error, WorkflowStoreError::InvalidData(_)));
        assert!(!error.to_string().contains("secret-bearing"));
    }
}

#[test]
fn attempt_summary_validates_stored_identities() {
    for column in [0, 1, 2, 4] {
        for value in [
            FixtureStorageValue::Text(String::new()),
            FixtureStorageValue::Text("x".repeat(MAX_ID_BYTES + 1)),
            FixtureStorageValue::BlobText("identity".into()),
            FixtureStorageValue::Null,
            FixtureStorageValue::Text("x".repeat(MAX_ID_BYTES)),
        ] {
            let valid =
                matches!(&value, FixtureStorageValue::Text(text) if text.len() == MAX_ID_BYTES);
            let name = match column {
                0 => "run_id",
                1 => "node_id",
                2 => "activation_id",
                4 => "dispatch_identity",
                _ => unreachable!(),
            };
            let raw = fixture_attempt_summary(name, value.expression()).expect("identity fixture");
            let mut raw = raw;
            if column != 4 {
                raw.4 = super::dispatch_identity(&raw.0, &raw.1, &raw.2, raw.3);
            }
            let result = super::parse_attempt_summary(raw);
            if valid && column != 4 {
                assert!(result.is_ok());
            } else {
                assert!(matches!(result, Err(WorkflowStoreError::InvalidData(_))));
            }
        }
    }
}

#[test]
fn attempt_summary_rejects_invalid_side_effect_storage() {
    for value in [
        FixtureStorageValue::Text("x".repeat(MAX_ID_BYTES + 1)),
        FixtureStorageValue::BlobText("read_only".into()),
        FixtureStorageValue::Null,
    ] {
        let raw = fixture_attempt_summary("side_effect", value.expression())
            .expect("invalid storage fixture");
        assert!(matches!(
            super::parse_attempt_summary(raw),
            Err(WorkflowStoreError::InvalidData(_))
        ));
    }
}

#[test]
fn side_effect_parser_rejects_unknown_values_without_exposing_payload() {
    let payload = "secret-bearing-untrusted-side-effect";
    let error = super::parse_side_effect(payload).expect_err("reject unknown side effect");
    assert!(matches!(error, WorkflowStoreError::InvalidData(_)));
    assert!(!error.to_string().contains(payload));
    assert!(matches!(
        super::parse_side_effect("read_only"),
        Ok(DispatchSideEffect::ReadOnly)
    ));
    assert!(matches!(
        super::parse_side_effect("mutating"),
        Ok(DispatchSideEffect::Mutating)
    ));
}

#[test]
fn run_summary_rejects_unknown_enums_without_exposing_payload() {
    for column in ["status", "authorization_ceiling"] {
        let (_temp, store) = initialized_store();
        let payload = "secret-bearing-untrusted-enum";
        block_on(
            update("workflow_runs")
                .value(column, payload)
                .where_eq("run_id", "run-1")
                .execute(&store.connection),
        )
        .expect("unknown enum fixture");
        let before = fixture_changes(&store.connection);
        let error = store.run_summary("run-1").expect_err("reject unknown enum");
        assert!(matches!(error, WorkflowStoreError::InvalidData(_)));
        assert!(!error.to_string().contains(payload));
        assert_eq!(fixture_changes(&store.connection), before);
    }
}

#[test]
fn run_summary_rejects_oversized_enums_without_mutation() {
    for column in ["status", "authorization_ceiling"] {
        let (_temp, store) = initialized_store();
        block_on(
            update("workflow_runs")
                .value(column, "x".repeat(MAX_ID_BYTES + 1))
                .where_eq("run_id", "run-1")
                .execute(&store.connection),
        )
        .expect("oversized enum fixture");
        let before = fixture_changes(&store.connection);
        assert!(matches!(
            store.run_summary("run-1"),
            Err(WorkflowStoreError::InvalidData(_))
        ));
        assert_eq!(fixture_changes(&store.connection), before);
    }
}

#[test]
fn run_summary_rejects_blob_enums_without_mutation() {
    for (column, value) in [
        ("status", "pending"),
        ("authorization_ceiling", "read_only"),
    ] {
        let (_temp, store) = initialized_store();
        block_on(
            store.connection.exec_update_count(
                &(update("workflow_runs")
                    .value(column, cast_blob(value))
                    .where_eq("run_id", "run-1")),
            ),
        )
        .expect("blob enum fixture");
        let before = fixture_changes(&store.connection);
        assert!(matches!(
            store.run_summary("run-1"),
            Err(WorkflowStoreError::InvalidData(_))
        ));
        assert_eq!(fixture_changes(&store.connection), before);
    }
}

#[test]
fn run_summary_rejects_blob_workspace_identity_without_mutation() {
    let (_temp, store) = initialized_store();
    block_on(
        store.connection.exec_update_count(
            &(update("workflow_runs")
                .value("workspace_snapshot", cast_blob("valid-looking-workspace"))
                .where_eq("run_id", "run-1")),
        ),
    )
    .expect("blob identity fixture");
    let before = fixture_changes(&store.connection);
    assert!(matches!(
        store.run_summary("run-1"),
        Err(WorkflowStoreError::InvalidData(_))
    ));
    assert_eq!(fixture_changes(&store.connection), before);
}

#[test]
fn run_summary_preserves_maximum_length_workspace_and_parent_ids() {
    let (temp, mut store) = initialized_store();
    let mut request = new_run();
    request.run_id = "maximum-identity-run".to_string();
    request.workspace_snapshot = "w".repeat(MAX_ID_BYTES);
    request.parent_session_id = Some("p".repeat(MAX_ID_BYTES));
    request.parent_session_generation = Some(1);
    store
        .create_run(&request)
        .expect("maximum-length identities");
    drop(store);
    let store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen identities");
    let before = fixture_changes(&store.connection);
    let summary = store
        .run_summary(&request.run_id)
        .expect("summary")
        .expect("stored run");
    assert_eq!(summary.workspace_snapshot, request.workspace_snapshot);
    assert_eq!(summary.parent_session_id, request.parent_session_id);
    assert_eq!(
        summary.parent_session_generation,
        request.parent_session_generation
    );
    assert_eq!(fixture_changes(&store.connection), before);
}

#[test]
fn run_summary_rejects_invalid_identity_without_mutation() {
    for (column, value) in [
        ("workspace_snapshot", String::new()),
        ("parent_session_id", String::new()),
        ("workspace_snapshot", " ".to_string()),
        ("parent_session_id", " ".to_string()),
        ("workspace_snapshot", "x".repeat(MAX_ID_BYTES + 1)),
        ("parent_session_id", "x".repeat(MAX_ID_BYTES + 1)),
    ] {
        let (_temp, store) = initialized_store();
        block_on(
            update("workflow_runs")
                .value(column, value)
                .where_eq("run_id", "run-1")
                .execute(&store.connection),
        )
        .expect("invalid identity fixture");
        let before = fixture_changes(&store.connection);
        assert!(matches!(
            store.run_summary("run-1"),
            Err(WorkflowStoreError::InvalidData(_))
        ));
        assert_eq!(fixture_changes(&store.connection), before);
    }
}

#[test]
fn run_summary_rejects_orphaned_parent_generation_without_mutation() {
    let (_temp, store) = initialized_store();
    block_on(
        update("workflow_runs")
            .value("parent_session_id", DatabaseValue::Null)
            .value("parent_session_generation", 1)
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("orphaned generation fixture");
    let before = fixture_changes(&store.connection);
    assert!(matches!(
        store.run_summary("run-1"),
        Err(WorkflowStoreError::InvalidData(_))
    ));
    assert_eq!(fixture_changes(&store.connection), before);
}

#[test]
fn run_summary_rejects_oversized_json_without_mutation() {
    for column in ["authored_provenance_json", "authorization_profile_json"] {
        let (_temp, store) = initialized_store();
        let payload = format!("\"{}\"", "x".repeat(MAX_INLINE_JSON_BYTES));
        block_on(
            update("workflow_runs")
                .value(column, payload)
                .where_eq("run_id", "run-1")
                .execute(&store.connection),
        )
        .expect("oversized JSON fixture");
        let before = fixture_changes(&store.connection);
        assert!(matches!(
            store.run_summary("run-1"),
            Err(WorkflowStoreError::InvalidData(_))
        ));
        assert_eq!(fixture_changes(&store.connection), before);
    }
}

#[test]
fn run_summary_rejects_malformed_json_without_exposing_payload() {
    for (column, message) in [
        (
            "authored_provenance_json",
            "stored workflow run authored provenance JSON is malformed",
        ),
        (
            "authorization_profile_json",
            "stored workflow run authorization profile JSON is malformed",
        ),
    ] {
        let (_temp, store) = initialized_store();
        let payload = "{private-fixture-value";
        block_on(
            update("workflow_runs")
                .value(column, payload)
                .where_eq("run_id", "run-1")
                .execute(&store.connection),
        )
        .expect("malformed JSON fixture");
        let before = fixture_changes(&store.connection);
        let error = store
            .run_summary("run-1")
            .expect_err("malformed stored JSON");
        assert!(matches!(error, WorkflowStoreError::InvalidData(actual) if actual == message));
        assert_eq!(fixture_changes(&store.connection), before);
        let preserved = fixture_scalar_text(
            &store.connection,
            &select("workflow_runs")
                .columns(&[column])
                .where_eq("run_id", "run-1"),
            column,
        )
        .expect("preserved JSON");
        assert_eq!(preserved, payload);
    }
}

#[test]
fn run_summary_rejects_invalid_complete_binding_without_mutation() {
    for (column, value) in [
        ("owner_plugin_id", String::new()),
        ("workflow_kind", String::new()),
        ("scope_key", String::new()),
        ("display_label", " ".to_string()),
        ("owner_plugin_id", "x".repeat(MAX_ID_BYTES + 1)),
        ("workflow_kind", "x".repeat(MAX_ID_BYTES + 1)),
        ("scope_key", "x".repeat(MAX_ID_BYTES + 1)),
        ("display_label", "x".repeat(MAX_DISPLAY_LABEL_BYTES + 1)),
    ] {
        let (_temp, mut store) = initialized_store();
        let mut request = new_run();
        request.run_id = "binding-validation".to_string();
        request.binding = Some(WorkflowRunBinding {
            owner_plugin_id: "test-plugin".to_string(),
            workflow_kind: "test-kind".to_string(),
            scope_key: "test-scope".to_string(),
            display_label: Some("Valid label".to_string()),
            single_active: false,
        });
        store.create_run(&request).expect("valid binding");
        assert_eq!(
            store
                .run_summary(&request.run_id)
                .expect("valid summary")
                .expect("stored run")
                .binding,
            request.binding
        );
        block_on(
            update("workflow_runs")
                .value(column, value)
                .where_eq("run_id", request.run_id.as_str())
                .execute(&store.connection),
        )
        .expect("invalid complete binding");
        let before = fixture_changes(&store.connection);
        assert!(matches!(
            store.run_summary(&request.run_id),
            Err(WorkflowStoreError::InvalidData(_))
        ));
        assert_eq!(fixture_changes(&store.connection), before);
    }
}

#[test]
fn run_summary_rejects_damaged_binding_without_mutation() {
    for (column, value) in [
        ("single_active", DatabaseValue::Int64(-1)),
        ("single_active", DatabaseValue::Int64(2)),
        ("single_active", DatabaseValue::Int64(1)),
        ("display_label", DatabaseValue::String("orphan".into())),
    ] {
        let (_temp, store) = initialized_store();
        block_on(
            update("workflow_runs")
                .value(column, value)
                .where_eq("run_id", "run-1")
                .execute(&store.connection),
        )
        .expect("damaged binding fixture");
        let before = fixture_changes(&store.connection);
        assert!(matches!(
            store.run_summary("run-1"),
            Err(WorkflowStoreError::InvalidData(_))
        ));
        assert_eq!(fixture_changes(&store.connection), before);
    }
}

#[test]
fn idempotent_start_rejects_invalid_single_active_values_without_mutation() {
    for value in [-1_i64, 2, i64::MAX] {
        let (_temp, mut store) = initialized_store();
        let mut request = new_run();
        request.run_id = "bound-flag-run".to_string();
        request.binding = Some(WorkflowRunBinding {
            owner_plugin_id: "test-plugin".to_string(),
            workflow_kind: "test-kind".to_string(),
            scope_key: "test-scope".to_string(),
            display_label: None,
            single_active: true,
        });
        assert!(
            store
                .create_run_idempotent(&request)
                .expect("create bound run")
        );
        assert!(
            !store
                .create_run_idempotent(&request)
                .expect("valid bound duplicate")
        );
        block_on(
            update("workflow_runs")
                .value("single_active", value)
                .where_eq("run_id", "bound-flag-run")
                .execute(&store.connection),
        )
        .expect("invalid flag fixture");
        let before = fixture_changes(&store.connection);
        let error = store
            .create_run_idempotent(&request)
            .expect_err("invalid flag");
        assert!(matches!(error, WorkflowStoreError::InvalidData(message)
                if message == "stored workflow run single-active flag is invalid"));
        assert_eq!(fixture_changes(&store.connection), before);
    }
}

#[test]
fn idempotent_start_rejects_orphaned_binding_metadata_without_mutation() {
    for (column, value) in [
        ("display_label", DatabaseValue::String("orphan".into())),
        ("single_active", DatabaseValue::Int64(1)),
    ] {
        let (temp, store) = initialized_store();
        block_on(
            update("workflow_runs")
                .value(column, value.clone())
                .where_eq("run_id", "run-1")
                .execute(&store.connection),
        )
        .expect("orphaned binding fixture");
        drop(store);
        let mut store =
            WorkflowStore::open_in_state_dir(temp.path()).expect("reopen damaged binding");
        let before = fixture_changes(&store.connection);
        assert!(matches!(
            store.create_run_idempotent(&new_run()),
            Err(WorkflowStoreError::InvalidData(_))
        ));
        assert_eq!(fixture_changes(&store.connection), before);
        let rows = fixture_rows(
            &store.connection,
            &select("workflow_runs")
                .columns(&[column])
                .where_eq("run_id", "run-1"),
        );
        assert_eq!(
            rows.first()
                .expect("preserved binding metadata")
                .get(column),
            Some(value)
        );
        block_on(
            update("workflow_runs")
                .value("display_label", DatabaseValue::Null)
                .value("single_active", 0)
                .where_eq("run_id", "run-1")
                .execute(&store.connection),
        )
        .expect("restore fixture");
        assert!(
            !store
                .create_run_idempotent(&new_run())
                .expect("restored duplicate")
        );
    }
}

#[test]
fn idempotent_start_rejects_malformed_json_text_without_mutation() {
    for column in ["input_json", "authored_provenance_json"] {
        let (_temp, mut store) = initialized_store();
        block_on(
            update("workflow_runs")
                .value(column, "{broken")
                .where_eq("run_id", "run-1")
                .execute(&store.connection),
        )
        .expect("malformed fixture");
        let before = fixture_changes(&store.connection);
        assert!(matches!(
            store.create_run_idempotent(&new_run()),
            Err(WorkflowStoreError::InvalidData(_))
        ));
        assert_eq!(fixture_changes(&store.connection), before);
    }
}

#[test]
fn conflicting_start_preserves_admitted_run_across_reopen() {
    let (temp, mut store) = initialized_store();
    let pending = store.pending_activations(100).expect("pending");
    let mut rejected = new_run();
    let admitted_nodes = store
        .run_graph_nodes(&rejected.run_id, None, 100)
        .expect("admitted graph");
    assert!(!admitted_nodes.is_empty());
    let admitted_summary = store.run_summary(&rejected.run_id).expect("admitted run");
    rejected.input = Some(serde_json::json!("conflicting duplicate"));
    assert!(store.create_run(&rejected).is_err());
    drop(store);
    let store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen duplicate");
    assert_eq!(store.pending_activations(100).expect("pending"), pending);
    assert_eq!(
        store.run_summary(&rejected.run_id).expect("admitted run"),
        admitted_summary
    );
    assert_eq!(
        store
            .run_graph_nodes(&rejected.run_id, None, 100)
            .expect("admitted graph"),
        admitted_nodes
    );
}

#[test]
fn invalid_successor_input_rolls_back_output_and_activation_atomically() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    let mut definition = sequential_definition();
    definition
        .nodes
        .get_mut("second")
        .expect("second")
        .input
        .schema = serde_json::json!({"type": "string"});
    store
        .persist_definition("invalid-successor", 1, &definition)
        .expect("definition");
    let mut run = new_run();
    run.run_id = "invalid-successor-run".to_string();
    run.definition_id = "invalid-successor".to_string();
    store.create_run(&run).expect("run");
    let error = store
        .persist_validated_output(&ValidatedOutput {
            output_id: "invalid-successor-output".to_string(),
            run_id: run.run_id.clone(),
            node_id: "first".to_string(),
            activation_id: activation_identity(&run.run_id, "first", 0),
            schema_id: definition.nodes["first"].output.type_name.clone(),
            schema_version: 1,
            value: serde_json::json!(2),
            artifact_reference: None,
            created_at_ms: 2,
        })
        .expect_err("target input mismatch");
    assert!(error.to_string().contains("workflow activation input"));
    assert_eq!(
        store
            .pending_activations(10)
            .expect("pending")
            .into_iter()
            .map(|activation| activation.node_id)
            .collect::<Vec<_>>(),
        ["first"]
    );
    assert!(
        store
            .output_summaries(&run.run_id, 10)
            .expect("outputs")
            .is_empty()
    );
}

#[test]
fn named_dependency_and_run_sources_materialize_deterministically() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    let mut definition = sequential_definition();
    let second = definition.nodes.get_mut("second").expect("second");
    second.input = bcode_workflow::ValueSchema {
        type_name: "example.composed/v1".to_string(),
        schema: serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["configuration", "root", "first"],
            "properties": {
                "configuration": {"type": "string"},
                "root": {"type": "integer"},
                "first": {"type": "integer"}
            }
        }),
    };
    definition.edges[0].transform = Some(bcode_workflow::WorkflowTransform {
        version: bcode_workflow::WORKFLOW_TRANSFORM_VERSION,
        expression: bcode_workflow::WorkflowTransformExpression::Object {
            fields: BTreeMap::from([
                (
                    "configuration".to_string(),
                    bcode_workflow::WorkflowTransformExpression::Input {
                        source: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_CONFIGURATION.to_string(),
                        path: "message".to_string(),
                    },
                ),
                (
                    "root".to_string(),
                    bcode_workflow::WorkflowTransformExpression::Input {
                        source: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_STATE.to_string(),
                        path: String::new(),
                    },
                ),
                (
                    "first".to_string(),
                    bcode_workflow::WorkflowTransformExpression::Input {
                        source: format!(
                            "{}first",
                            bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_DEPENDENCY_PREFIX
                        ),
                        path: String::new(),
                    },
                ),
            ]),
        },
        output: second.input.clone(),
    });
    store
        .persist_definition("example", 1, &definition)
        .expect("definition");
    persist_authored_run_provenance_fixture(&store);
    let mut run = new_run();
    run.run_id = "named-sources-run".to_string();
    run.authored_provenance = Some(AuthoredWorkflowRunProvenance::new(
        "authored/example".to_string(),
        3,
        bcode_workflow::WorkflowDefinitionIdentity {
            kind: "authored/example".to_string(),
            definition_id: "example".to_string(),
            definition_version: 1,
        },
        Some("review".to_string()),
        Some(4),
        serde_json::json!({"message": "review"}),
    ));
    store.create_run(&run).expect("run");
    store
        .persist_validated_output(&ValidatedOutput {
            output_id: "named-first-output".to_string(),
            run_id: run.run_id.clone(),
            node_id: "first".to_string(),
            activation_id: activation_identity(&run.run_id, "first", 0),
            schema_id: "u32".to_string(),
            schema_version: 1,
            value: serde_json::json!(2),
            artifact_reference: None,
            created_at_ms: 20,
        })
        .expect("first output");
    drop(store);

    let reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
    let activation = reopened
        .pending_activations(10)
        .expect("pending after restart")
        .into_iter()
        .find(|activation| activation.run_id == run.run_id && activation.node_id == "second")
        .expect("materialized named input");
    assert_eq!(
        activation.input,
        Some(serde_json::json!({
            "configuration": "review",
            "root": 1,
            "first": 2
        }))
    );
}

#[test]
fn ambiguous_successor_generation_rolls_back_predecessor_completion() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("example", 1, &sequential_definition())
        .expect("definition");
    store.create_run(&new_run()).expect("run");
    for identity in ["successor-a", "successor-b"] {
        block_on(
            insert("workflow_activations")
                .value("run_id", "run-1")
                .value("node_id", "second")
                .value("activation_id", identity)
                .value("dependency_generation", 0)
                .value("status", "pending")
                .value("created_at_ms", 10)
                .execute(&store.connection),
        )
        .expect("ambiguous successor");
    }
    let events_before: u64 = fixture_scalar_unsigned(
        &store.connection,
        &select("workflow_events").project_as(switchy_database::query::count_all(), "count"),
        "count",
    )
    .expect("event count before completion");
    let before = fixture_changes(&store.connection);
    let error = store
        .persist_validated_output(&ValidatedOutput {
            output_id: "first-output".to_string(),
            run_id: "run-1".to_string(),
            node_id: "first".to_string(),
            activation_id: activation_identity("run-1", "first", 0),
            schema_id: "u32".to_string(),
            schema_version: 1,
            value: serde_json::json!(2),
            artifact_reference: None,
            created_at_ms: 20,
        })
        .expect_err("ambiguous generation must fail closed");
    assert!(matches!(error, WorkflowStoreError::InvalidData(message)
            if message.contains("ambiguous activations: second")));
    // The failure is reached after writes, so verify rollback rather than a read-only rejection.
    assert!(fixture_changes(&store.connection) > before);
    drop(store);
    let store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen after rollback");
    let events_after: u64 = fixture_scalar_unsigned(
        &store.connection,
        &select("workflow_events").project_as(switchy_database::query::count_all(), "count"),
        "count",
    )
    .expect("event count after rollback");
    assert_eq!(events_after, events_before);
    let successors: u64 = fixture_scalar_unsigned(
        &store.connection,
        &select("workflow_activations")
            .project_as(switchy_database::query::count_all(), "count")
            .where_eq("run_id", "run-1")
            .where_eq("node_id", "second")
            .where_eq("status", "pending"),
        "count",
    )
    .expect("ambiguous state remains preserved");
    assert_eq!(successors, 2);
    let status: String = fixture_scalar_text(
        &store.connection,
        &select("workflow_activations")
            .columns(&["status"])
            .where_eq("run_id", "run-1")
            .where_eq("node_id", "first"),
        "status",
    )
    .expect("predecessor status");
    assert_eq!(status, "pending");
    let outputs: u64 = fixture_scalar_unsigned(
        &store.connection,
        &select("workflow_outputs")
            .project_as(switchy_database::query::count_all(), "count")
            .where_eq("run_id", "run-1"),
        "count",
    )
    .expect("output count");
    assert_eq!(outputs, 0);
}

#[test]
fn unknown_successor_status_rolls_back_completion() {
    for status in [
        FixtureStorageValue::Text("future_status".into()),
        FixtureStorageValue::Text(String::new()),
        FixtureStorageValue::Text("x".repeat(33)),
        FixtureStorageValue::BlobText("pending".into()),
    ] {
        let temp = tempfile::tempdir().expect("temp");
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("example", 1, &sequential_definition())
            .expect("definition");
        store.create_run(&new_run()).expect("run");
        block_on(
            insert("workflow_activations")
                .value("run_id", "run-1")
                .value("node_id", "second")
                .value("activation_id", "future-successor")
                .value("dependency_generation", 0)
                .value("status", "pending")
                .value("created_at_ms", 10)
                .execute(&store.connection),
        )
        .expect("future status");
        block_on(
            store.connection.exec_update_count(
                &(update("workflow_activations")
                    .value("status", status.expression())
                    .where_eq("run_id", "run-1")
                    .where_eq("activation_id", "future-successor")),
            ),
        )
        .expect("future status");
        let error = store
            .persist_validated_output(&ValidatedOutput {
                output_id: "first-output".to_string(),
                run_id: "run-1".to_string(),
                node_id: "first".to_string(),
                activation_id: activation_identity("run-1", "first", 0),
                schema_id: "u32".to_string(),
                schema_version: 1,
                value: serde_json::json!(2),
                artifact_reference: None,
                created_at_ms: 20,
            })
            .expect_err("unknown successor must fail closed");
        assert!(matches!(error, WorkflowStoreError::InvalidData(message)
            if message.contains("unsupported activation status: second")));
        let predecessor = fixture_scalar_text(
            &store.connection,
            &select("workflow_activations")
                .columns(&["status"])
                .where_eq("run_id", "run-1")
                .where_eq("node_id", "first"),
            "status",
        )
        .expect("preserved predecessor status");
        assert_eq!(predecessor, "pending");
        let retained = fixture_scalar_unsigned(
            &store.connection,
            &select("workflow_activations")
                .project_as(switchy_database::query::count_all(), "count")
                .where_eq("run_id", "run-1")
                .where_eq("node_id", "second")
                .where_eq_expression(
                    switchy_database::query::identifier("status"),
                    status.expression(),
                ),
            "count",
        )
        .expect("preserved successor storage/value");
        assert_eq!(retained, 1);
        let outputs: u64 = fixture_scalar_unsigned(
            &store.connection,
            &select("workflow_outputs")
                .project_as(switchy_database::query::count_all(), "count")
                .where_eq("run_id", "run-1"),
            "count",
        )
        .expect("output count");
        assert_eq!(outputs, 0);
    }
}

#[test]
fn ignored_successor_insert_rolls_back_completion() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("example", 1, &sequential_definition())
        .expect("definition");
    store.create_run(&new_run()).expect("run");
    fixture_ignore_successor_insert(&store.connection);
    let output = ValidatedOutput {
        output_id: "first-output".to_string(),
        run_id: "run-1".to_string(),
        node_id: "first".to_string(),
        activation_id: activation_identity("run-1", "first", 0),
        schema_id: "u32".to_string(),
        schema_version: 1,
        value: serde_json::json!(2),
        artifact_reference: None,
        created_at_ms: 20,
    };
    let error = store
        .persist_validated_output(&output)
        .expect_err("ignored insertion must fail");
    assert!(matches!(error, WorkflowStoreError::InvalidData(message)
            if message.contains("did not create exactly one activation: second")));
    let outputs: u64 = fixture_scalar_unsigned(
        &store.connection,
        &select("workflow_outputs")
            .project_as(switchy_database::query::count_all(), "count")
            .where_eq("run_id", "run-1"),
        "count",
    )
    .expect("output count");
    assert_eq!(outputs, 0);
    let status: String = fixture_scalar_text(
        &store.connection,
        &select("workflow_activations")
            .columns(&["status"])
            .where_eq("run_id", "run-1")
            .where_eq("node_id", "first"),
        "status",
    )
    .expect("predecessor status");
    assert_eq!(status, "pending");
    block_on(store.connection.drop_trigger("ignore_successor", false))
        .expect("remove injected fault");
    let result = store
        .persist_validated_output(&output)
        .expect("retry rolled-back completion");
    assert_eq!(result.activated.len(), 1);
    assert_eq!(
        result.activated[0].activation_id,
        activation_identity("run-1", "second", 0)
    );
    let successors: u64 = fixture_scalar_unsigned(
        &store.connection,
        &select("workflow_activations")
            .project_as(switchy_database::query::count_all(), "count")
            .where_eq("run_id", "run-1")
            .where_eq("node_id", "second"),
        "count",
    )
    .expect("successor count");
    assert_eq!(successors, 1);
    store
        .persist_validated_output(&ValidatedOutput {
            output_id: "second-output".to_string(),
            run_id: "run-1".to_string(),
            node_id: "second".to_string(),
            activation_id: activation_identity("run-1", "second", 0),
            schema_id: "u32".to_string(),
            schema_version: 1,
            value: serde_json::json!(3),
            artifact_reference: None,
            created_at_ms: 30,
        })
        .expect("finish successor after retry");
    drop(store);
    let store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen completed run");
    let outputs: u64 = fixture_scalar_unsigned(
        &store.connection,
        &select("workflow_outputs")
            .project_as(switchy_database::query::count_all(), "count")
            .where_eq("run_id", "run-1"),
        "count",
    )
    .expect("durable outputs");
    assert_eq!(outputs, 2);
    let run_status: String = fixture_scalar_text(
        &store.connection,
        &select("workflow_runs")
            .columns(&["status"])
            .where_eq("run_id", "run-1"),
        "status",
    )
    .expect("terminal run status");
    assert_eq!(run_status, "completed");
}

fn fixture_ignore_successor_insert(db: &dyn Database) {
    block_on(
        create_trigger("ignore_successor", "workflow_activations")
            .temporary(true)
            .before_insert()
            .when(E::new_column("node_id").compare(C::Equal, E::Text("second".into())))
            .raise_ignore()
            .execute(db),
    )
    .expect("inject ignored insertion");
}

#[test]
fn successor_identity_collision_rolls_back_completion() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("example", 1, &sequential_definition())
        .expect("definition");
    store.create_run(&new_run()).expect("run");
    block_on(
        insert("workflow_activations")
            .value("run_id", "run-1")
            .value("node_id", "second")
            .value("activation_id", activation_identity("run-1", "second", 0))
            .value("dependency_generation", 1)
            .value("status", "pending")
            .value("created_at_ms", 10)
            .execute(&store.connection),
    )
    .expect("conflicting identity");
    let error = store
        .persist_validated_output(&ValidatedOutput {
            output_id: "first-output".to_string(),
            run_id: "run-1".to_string(),
            node_id: "first".to_string(),
            activation_id: activation_identity("run-1", "first", 0),
            schema_id: "u32".to_string(),
            schema_version: 1,
            value: serde_json::json!(2),
            artifact_reference: None,
            created_at_ms: 20,
        })
        .expect_err("identity collision must not conceal missing successor");
    assert!(
        matches!(error, WorkflowStoreError::StructuredDatabase(ref error) if error.classification() == Some(switchy_database::DatabaseErrorKind::Constraint))
    );
    let outputs: u64 = fixture_scalar_unsigned(
        &store.connection,
        &select("workflow_outputs")
            .project_as(switchy_database::query::count_all(), "count")
            .where_eq("run_id", "run-1"),
        "count",
    )
    .expect("output count");
    assert_eq!(outputs, 0);
    let status: String = fixture_scalar_text(
        &store.connection,
        &select("workflow_activations")
            .columns(&["status"])
            .where_eq("run_id", "run-1")
            .where_eq("node_id", "first"),
        "status",
    )
    .expect("predecessor status");
    assert_eq!(status, "pending");
}

#[test]
fn successor_creation_is_not_suppressed_by_another_generation() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("example", 1, &sequential_definition())
        .expect("definition");
    store.create_run(&new_run()).expect("run");
    block_on(
        insert("workflow_activations")
            .value("run_id", "run-1")
            .value("node_id", "second")
            .value("activation_id", "other-generation")
            .value("dependency_generation", 1)
            .value("input_json", "7")
            .value("status", "completed")
            .value("created_at_ms", 10)
            .execute(&store.connection),
    )
    .expect("other generation");
    let result = store
        .persist_validated_output(&ValidatedOutput {
            output_id: "first-output".to_string(),
            run_id: "run-1".to_string(),
            node_id: "first".to_string(),
            activation_id: activation_identity("run-1", "first", 0),
            schema_id: "u32".to_string(),
            schema_version: 1,
            value: serde_json::json!(2),
            artifact_reference: None,
            created_at_ms: 20,
        })
        .expect("source completion");
    assert_eq!(result.activated.len(), 1);
    assert_eq!(result.activated[0].node_id, "second");
    assert_eq!(result.activated[0].dependency_generation, 0);
    assert_eq!(
        result.activated[0].activation_id,
        activation_identity("run-1", "second", 0)
    );
    let rows = fixture_rows(
        &store.connection,
        &select("workflow_activations")
            .columns(&["status", "input_json"])
            .where_eq("run_id", "run-1")
            .where_eq("activation_id", "other-generation"),
    );
    let row = rows.first().expect("other generation preserved");
    let (status, input) = (fixture_text(row, "status"), fixture_text(row, "input_json"));
    assert_eq!(status, "completed");
    assert_eq!(input, "7");
}

#[test]
fn successor_preserves_existing_generation_with_distinct_identity() {
    for status in [
        "pending",
        "running",
        "waiting_input",
        "waiting_approval",
        "waiting_mutation_approval",
        "completed",
        "skipped",
        "failed",
        "cancelled",
    ] {
        let temp = tempfile::tempdir().expect("temp");
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("example", 1, &sequential_definition())
            .expect("definition");
        store.create_run(&new_run()).expect("run");
        block_on(
            insert("workflow_activations")
                .value("run_id", "run-1")
                .value("node_id", "second")
                .value("activation_id", "existing-successor")
                .value("dependency_generation", 0)
                .value("input_json", "7")
                .value("status", status)
                .value("created_at_ms", 10)
                .execute(&store.connection),
        )
        .expect("existing successor");
        let transaction = database::begin(&store.connection, TransactionMode::Deferred)
            .expect("admission transaction");
        record_activation_graph_binding(
            transaction.as_ref(),
            "run-1",
            "second",
            "existing-successor",
            1,
        )
        .expect("existing successor admission");
        database::commit(transaction).expect("admission commit");
        let result = store
            .persist_validated_output(&ValidatedOutput {
                output_id: "first-output".to_string(),
                run_id: "run-1".to_string(),
                node_id: "first".to_string(),
                activation_id: activation_identity("run-1", "first", 0),
                schema_id: "u32".to_string(),
                schema_version: 1,
                value: serde_json::json!(2),
                artifact_reference: None,
                created_at_ms: 20,
            })
            .expect("source completion");
        assert!(result.activated.is_empty());
        let rows = fixture_rows(
            &store.connection,
            &select("workflow_activations")
                .columns(&["activation_id", "input_json"])
                .where_eq("run_id", "run-1")
                .where_eq("node_id", "second"),
        );
        let row = rows.first().expect("successor preserved");
        let (identity, input) = (
            fixture_text(row, "activation_id"),
            fixture_text(row, "input_json"),
        );
        assert_eq!(identity, "existing-successor");
        assert_eq!(input, "7");
        let transaction =
            database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
        assert_eq!(
            activation_status_at_generation(transaction.as_ref(), "run-1", "second", 0)
                .expect("unique generation")
                .as_deref(),
            Some(status)
        );
    }
}

#[test]
fn dependency_output_does_not_borrow_result_from_noncompleted_state() {
    let (_temp, mut store) = initialized_store();
    store
        .persist_validated_output(&ValidatedOutput {
            output_id: "review-output".to_string(),
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: activation_identity("run-1", "review", 0),
            schema_id: "u32".to_string(),
            schema_version: 1,
            value: serde_json::json!(2),
            artifact_reference: None,
            created_at_ms: 20,
        })
        .expect("completed output");
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    let db: &dyn Database = transaction.as_ref();
    for status in [
        "pending",
        "running",
        "waiting_input",
        "waiting_approval",
        "waiting_mutation_approval",
        "skipped",
        "failed",
        "cancelled",
    ] {
        block_on(
            update("workflow_activations")
                .value("status", status)
                .where_eq("run_id", "run-1")
                .execute(db),
        )
        .expect("status fixture");
        let changes = fixture_changes(db);
        assert!(
            matches!(
                activation_output_value(db, "run-1", "review", 0),
                Err(WorkflowStoreError::StructuredDatabase(
                    switchy_database::DatabaseError::NoRow
                ))
            ),
            "{status}"
        );
        assert_eq!(fixture_changes(db), changes);
    }
}

#[test]
fn dependency_output_unknown_status_is_not_an_absent_result() {
    let (_temp, store) = initialized_store();
    block_on(
        update("workflow_activations")
            .value("status", "future_status")
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("future state fixture");
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    let db: &dyn Database = transaction.as_ref();
    let changes = fixture_changes(db);
    let error = activation_output_value(db, "run-1", "review", 0).expect_err("unknown status");
    assert!(matches!(error, WorkflowStoreError::InvalidData(_)));
    assert!(error.to_string().contains("unsupported activation status"));
    assert_eq!(fixture_changes(db), changes);
    let status = fixture_scalar_text(
        db,
        &select("workflow_activations")
            .columns(&["status"])
            .where_eq("run_id", "run-1"),
        "status",
    )
    .expect("preserved status");
    assert_eq!(status, "future_status");
}

#[test]
fn completed_dependency_rejects_conflicting_pending_generation() {
    let (_temp, mut store) = initialized_store();
    store
        .persist_validated_output(&ValidatedOutput {
            output_id: "review-output".to_string(),
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: activation_identity("run-1", "review", 0),
            schema_id: "u32".to_string(),
            schema_version: 1,
            value: serde_json::json!(2),
            artifact_reference: None,
            created_at_ms: 20,
        })
        .expect("completed output");
    block_on(
        insert("workflow_activations")
            .value("run_id", "run-1")
            .value("node_id", "review")
            .value("activation_id", "conflicting-activation")
            .value("dependency_generation", 0)
            .value("status", "pending")
            .value("created_at_ms", 20)
            .execute(&store.connection),
    )
    .expect("ambiguous fixture");
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    let db: &dyn Database = transaction.as_ref();
    let changes = fixture_changes(db);
    let error = activation_output_value(db, "run-1", "review", 0)
        .expect_err("mixed statuses cannot conceal duplicate generation");
    assert!(error.to_string().contains("ambiguous activations"));
    assert_eq!(fixture_changes(db), changes);
}

#[test]
fn oversized_transform_context_rolls_back_output_and_successor() {
    for field in [
        TransformRunField::Input,
        TransformRunField::AuthoredProvenance,
    ] {
        let temp = tempfile::tempdir().expect("temp");
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        let mut definition = sequential_definition();
        definition.edges[0].transform = Some(bcode_workflow::WorkflowTransform {
            version: bcode_workflow::WORKFLOW_TRANSFORM_VERSION,
            expression: bcode_workflow::WorkflowTransformExpression::Input {
                source: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_CURRENT.to_string(),
                path: String::new(),
            },
            output: definition.nodes["second"].input.clone(),
        });
        store
            .persist_definition("example", 1, &definition)
            .expect("definition");
        store.create_run(&new_run()).expect("run");
        let column = match field {
            TransformRunField::Input => "input_json",
            TransformRunField::AuthoredProvenance => "authored_provenance_json",
        };
        block_on(
            update("workflow_runs")
                .value(column, "x".repeat(MAX_INLINE_JSON_BYTES + 1))
                .where_eq("run_id", "run-1")
                .execute(&store.connection),
        )
        .expect("oversized context fixture");
        let events = store.event_history("run-1", None, 100).expect("events");
        let error = store
            .persist_validated_output(&ValidatedOutput {
                output_id: "first-output".to_string(),
                run_id: "run-1".to_string(),
                node_id: "first".to_string(),
                activation_id: activation_identity("run-1", "first", 0),
                schema_id: "u32".to_string(),
                schema_version: 1,
                value: serde_json::json!(2),
                artifact_reference: None,
                created_at_ms: 20,
            })
            .expect_err("oversized context");
        assert!(error.to_string().contains("invalid or oversized"));
        assert!(
            store
                .output_summaries("run-1", 100)
                .expect("outputs")
                .is_empty()
        );
        assert_eq!(
            store.event_history("run-1", None, 100).expect("events"),
            events
        );
        let transaction =
            database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
        assert!(
            activation_status_at_generation(transaction.as_ref(), "run-1", "second", 0)
                .expect("successor status")
                .is_none()
        );
        assert_eq!(
            activation_status_at_generation(transaction.as_ref(), "run-1", "first", 0)
                .expect("source status")
                .as_deref(),
            Some("pending")
        );
    }
}

#[test]
fn unsupported_transform_is_rejected_before_run_context_reads() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    let definition = sequential_definition();
    store
        .persist_definition("example", 1, &definition)
        .expect("definition");
    store.create_run(&new_run()).expect("run");
    let mut edge = definition.edges[0].clone();
    let transform = bcode_workflow::WorkflowTransform {
        version: bcode_workflow::WORKFLOW_TRANSFORM_VERSION + 1,
        expression: bcode_workflow::WorkflowTransformExpression::Input {
            source: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_CURRENT.to_string(),
            path: String::new(),
        },
        output: definition.nodes["second"].input.clone(),
    };
    let expected = transform
        .validate()
        .expect_err("future version")
        .to_string();
    edge.transform = Some(transform);
    block_on(
        update("workflow_run_graph_edges")
            .value("edge_json", serde_json::to_string(&edge).expect("JSON"))
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("future transform fixture");
    block_on(
        update("workflow_runs")
            .value("input_json", "invalid JSON")
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("invalid context fixture");
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    let db: &dyn Database = transaction.as_ref();
    let changes = fixture_changes(db);
    let error = activation_input(
        db,
        &ValidatedOutput {
            output_id: "first-output".to_string(),
            run_id: "run-1".to_string(),
            node_id: "first".to_string(),
            activation_id: activation_identity("run-1", "first", 0),
            schema_id: "u32".to_string(),
            schema_version: 1,
            value: serde_json::json!(2),
            artifact_reference: None,
            created_at_ms: 20,
        },
        &definition.nodes["second"],
        0,
        &run_graph::initial_edge_between(db, "run-1", "first", "second")
            .expect("edge lookup")
            .expect("edge"),
    )
    .expect_err("future transform");
    assert!(error.to_string().contains(&expected));
    assert_eq!(fixture_changes(db), changes);
}

#[test]
fn transform_source_dependency_consumes_exact_settling_output() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    let definition = sequential_definition();
    store
        .persist_definition("example", 1, &definition)
        .expect("definition");
    store.create_run(&new_run()).expect("run");
    let mut edge = definition.edges[0].clone();
    edge.transform = Some(bcode_workflow::WorkflowTransform {
        version: bcode_workflow::WORKFLOW_TRANSFORM_VERSION,
        expression: bcode_workflow::WorkflowTransformExpression::Input {
            source: format!(
                "{}first",
                bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_DEPENDENCY_PREFIX
            ),
            path: String::new(),
        },
        output: definition.nodes["second"].input.clone(),
    });
    let output = ValidatedOutput {
        output_id: "exact-output".to_string(),
        run_id: "run-1".to_string(),
        node_id: "first".to_string(),
        activation_id: activation_identity("run-1", "first", 0),
        schema_id: "u32".to_string(),
        schema_version: 1,
        value: serde_json::json!(42),
        artifact_reference: None,
        created_at_ms: 20,
    };
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    assert_eq!(
        activation_input(
            transaction.as_ref(),
            &output,
            &definition.nodes["second"],
            0,
            &edge
        )
        .expect("exact source input"),
        output.value
    );
}

#[test]
fn transform_non_dependency_reference_rolls_back_output() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    let definition = sequential_definition();
    store
        .persist_definition("example", 1, &definition)
        .expect("definition");
    store.create_run(&new_run()).expect("run");
    let mut edge = definition.edges[0].clone();
    edge.transform = Some(bcode_workflow::WorkflowTransform {
        version: bcode_workflow::WORKFLOW_TRANSFORM_VERSION,
        expression: bcode_workflow::WorkflowTransformExpression::Input {
            source: format!(
                "{}second",
                bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_DEPENDENCY_PREFIX
            ),
            path: String::new(),
        },
        output: definition.nodes["second"].input.clone(),
    });
    block_on(
        update("workflow_run_graph_edges")
            .value(
                "edge_json",
                serde_json::to_string(&edge).expect("edge JSON"),
            )
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("invalid reference fixture");
    let events = store.event_history("run-1", None, 100).expect("events");
    let error = store
        .persist_validated_output(&ValidatedOutput {
            output_id: "first-output".to_string(),
            run_id: "run-1".to_string(),
            node_id: "first".to_string(),
            activation_id: activation_identity("run-1", "first", 0),
            schema_id: "u32".to_string(),
            schema_version: 1,
            value: serde_json::json!(2),
            artifact_reference: None,
            created_at_ms: 20,
        })
        .expect_err("non-dependency reference");
    assert!(error.to_string().contains("non-dependency node"));
    assert!(
        store
            .output_summaries("run-1", 100)
            .expect("outputs")
            .is_empty()
    );
    assert_eq!(
        store.event_history("run-1", None, 100).expect("events"),
        events
    );
}

#[test]
fn named_dependency_materialization_rejects_skipped_branch_values_atomically() {
    check_skipped_dependency_materialization(true);
}

#[test]
fn unreferenced_skipped_dependency_does_not_block_transform() {
    check_skipped_dependency_materialization(false);
}

fn check_skipped_dependency_materialization(reference_skipped: bool) {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    let schema = bcode_workflow::ValueSchema::of::<u32>();
    let mut definition = sequential_definition();
    definition.nodes.insert(
        "skipped".to_string(),
        bcode_workflow::NodeDefinition {
            id: "skipped".to_string(),
            name: "skipped".to_string(),
            kind: bcode_workflow::NodeKind::Task,
            dataflow: bcode_workflow::WorkflowNodeDataflowPolicy::Direct,
            input: schema.clone(),
            output: schema,
            resources: Vec::new(),
            configuration: serde_json::Value::Null,
        },
    );
    definition.entries.push("skipped".to_string());
    definition.edges[0].transform = Some(bcode_workflow::WorkflowTransform {
        version: bcode_workflow::WORKFLOW_TRANSFORM_VERSION,
        expression: bcode_workflow::WorkflowTransformExpression::Input {
            source: if reference_skipped {
                format!(
                    "{}skipped",
                    bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_DEPENDENCY_PREFIX
                )
            } else {
                bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_CURRENT.to_string()
            },
            path: String::new(),
        },
        output: definition.nodes["second"].input.clone(),
    });
    definition.edges.push(bcode_workflow::EdgeDefinition {
        from: "skipped".to_string(),
        to: "second".to_string(),
        kind: bcode_workflow::EdgeKind::Direct,
        transform: definition.edges[0].transform.clone(),
    });
    store
        .persist_definition("skipped-dependency", 1, &definition)
        .expect("definition");
    let mut run = new_run();
    run.definition_id = "skipped-dependency".to_string();
    run.run_id = "skipped-dependency-run".to_string();
    store.create_run(&run).expect("run");
    fixture_skip_dependency(&store.connection, &run.run_id);
    let result = store.persist_validated_output(&ValidatedOutput {
        output_id: "first-output".to_string(),
        run_id: run.run_id.clone(),
        node_id: "first".to_string(),
        activation_id: activation_identity(&run.run_id, "first", 0),
        schema_id: "u32".to_string(),
        schema_version: 1,
        value: serde_json::json!(2),
        artifact_reference: None,
        created_at_ms: 3,
    });
    if !reference_skipped {
        result.expect("unreferenced skipped dependency is not loaded");
        let transaction =
            database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
        let input = fixture_scalar_text(
            transaction.as_ref(),
            &select("workflow_activations")
                .columns(&["input_json"])
                .where_eq("run_id", run.run_id.as_str())
                .where_eq("node_id", "second"),
            "input_json",
        )
        .expect("successor input");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&input).expect("JSON"),
            serde_json::json!(2)
        );
        return;
    }
    let error = result.expect_err("skipped dependency has no value");
    assert!(!error.to_string().is_empty());
    assert!(
        store
            .output_summaries(&run.run_id, 10)
            .expect("outputs")
            .is_empty()
    );
    assert_eq!(
        store
            .pending_activations(10)
            .expect("pending")
            .iter()
            .filter(|activation| activation.run_id == run.run_id && activation.node_id == "second")
            .count(),
        0
    );
}

fn fixture_skip_dependency(db: &dyn Database, run_id: &str) {
    block_on(
        update("workflow_activations")
            .value("status", "skipped")
            .where_eq("run_id", run_id)
            .where_eq("node_id", "skipped")
            .execute(db),
    )
    .expect("skip dependency");
}

#[test]
fn revised_leaf_output_requires_current_admission_and_completes() {
    let (temp, mut store) = initialized_store();
    block_on(
        update("workflow_run_graphs")
            .value("revision", 2)
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("revision fixture");
    let output = ValidatedOutput {
        output_id: "revised-output".to_string(),
        run_id: "run-1".to_string(),
        node_id: "review".to_string(),
        activation_id: activation_id(),
        schema_id: "u32".to_string(),
        schema_version: 1,
        value: serde_json::json!(1),
        artifact_reference: None,
        created_at_ms: 20,
    };
    assert!(store.persist_validated_output(&output).is_err());
    block_on(
        update("workflow_activation_graph_bindings")
            .value("graph_revision", 2)
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .expect("current admission fixture");
    block_on(async {
        update("workflow_runs")
            .value("target_artifact_id", "artifact-a")
            .value("coordinator_daemon_instance_id", "daemon-a")
            .value("coordinator_generation", 1)
            .value("coordinator_fencing_token", "token-a")
            .where_eq("run_id", "run-1")
            .execute(&store.connection)
            .await?;
        Ok::<(), switchy_database::DatabaseError>(())
    })
    .expect("authority fixture");
    let authority = store
        .execution_authority("run-1")
        .expect("authority")
        .expect("owner");
    let mut stale = authority.clone();
    stale.generation += 1;
    let before = fixture_changes(&store.connection);
    assert!(
        store
            .persist_validated_output_at_graph_revision(&output, 2, &stale)
            .is_err()
    );
    assert!(
        store
            .persist_validated_output_at_graph_revision(&output, 1, &authority)
            .is_err()
    );
    assert_eq!(before, fixture_changes(&store.connection));
    let outcome = store
        .persist_validated_output_at_graph_revision(&output, 2, &authority)
        .expect("revised leaf settlement");
    assert_eq!(outcome.run_status, RunStatus::Completed);
    assert!(outcome.activated.is_empty());
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    assert!(store.persist_validated_output(&output).is_err());
    let status: String = fixture_scalar_text(
        &store.connection,
        &select("workflow_runs")
            .columns(&["status"])
            .where_eq("run_id", "run-1"),
        "status",
    )
    .expect("durable terminal status");
    assert_eq!(status, "completed");
}

#[test]
fn revised_readiness_rejects_unreconciled_completed_dependency() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("sequential", 1, &sequential_definition())
        .expect("definition");
    let mut run = new_run();
    run.definition_id = "sequential".into();
    store.create_run(&run).expect("run");
    block_on(async {
        update("workflow_run_graphs")
            .value("revision", 2)
            .execute(&store.connection)
            .await?;
        update("workflow_activations")
            .value("status", "completed")
            .execute(&store.connection)
            .await?;
        Ok::<(), switchy_database::DatabaseError>(())
    })
    .expect("revision fixture");
    let output = ValidatedOutput {
        output_id: "dependency".into(),
        run_id: run.run_id.clone(),
        node_id: "first".into(),
        activation_id: activation_identity(&run.run_id, "first", 0),
        schema_id: "u32".into(),
        schema_version: 1,
        value: serde_json::json!(2),
        artifact_reference: None,
        created_at_ms: 20,
    };
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    let error = successor_dependencies_ready(transaction.as_ref(), &output, "second", 0)
        .expect_err("stale admission");
    assert!(
        error
            .to_string()
            .contains("successor dependency requires graph reconciliation")
    );
    let input_error =
        current_dependency_output_value(transaction.as_ref(), &run.run_id, "first", 0, 2)
            .expect_err("stale input admission");
    assert!(
        input_error
            .to_string()
            .contains("dependency input requires graph reconciliation")
    );
    block_on(
        update("workflow_activation_graph_bindings")
            .value("graph_revision", 2)
            .execute(transaction.as_ref()),
    )
    .expect("current admission");
    assert!(
        successor_dependencies_ready(transaction.as_ref(), &output, "second", 0).expect("ready")
    );
}

#[test]
fn current_revision_direct_settlement_admits_successor_at_same_revision() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("sequential", 1, &sequential_definition())
        .expect("definition");
    let mut run = new_run();
    run.definition_id = "sequential".into();
    store.create_run(&run).expect("run");
    // Exercise current-revision settlement independently of still-guarded publication.
    block_on(async {
        update("workflow_run_graphs")
            .value("revision", 2)
            .execute(&store.connection)
            .await?;
        update("workflow_activation_graph_bindings")
            .value("graph_revision", 2)
            .execute(&store.connection)
            .await?;
        Ok::<(), switchy_database::DatabaseError>(())
    })
    .expect("revision fixture");
    let outcome = store
        .persist_validated_output(&ValidatedOutput {
            output_id: "revised-first".into(),
            run_id: "run-1".into(),
            node_id: "first".into(),
            activation_id: activation_identity("run-1", "first", 0),
            schema_id: "u32".into(),
            schema_version: 1,
            value: serde_json::json!(2),
            artifact_reference: None,
            created_at_ms: 20,
        })
        .expect("revised settlement");
    assert_eq!(outcome.activated.len(), 1);
    assert_eq!(outcome.activated[0].input, Some(serde_json::json!(2)));
    drop(store);
    let store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    assert_eq!(
        store
            .activation_admitted_graph_revision(
                "run-1",
                "second",
                &outcome.activated[0].activation_id
            )
            .expect("binding"),
        Some(2)
    );
}

fn assert_retained_successor_deadline(store: &mut WorkflowStore, output: &ValidatedOutput) {
    block_on(
        update("workflow_runs")
            .value("deadline_at_ms", 20)
            .where_eq("run_id", "sequential-run")
            .execute(&store.connection),
    )
    .expect("deadline");
    let error = store
        .persist_validated_output(output)
        .expect_err("expired successor admission");
    assert!(error.to_string().contains("deadline"));
    assert!(
        store
            .validated_outputs("sequential-run", 10)
            .expect("no expired output")
            .is_empty()
    );
    block_on(
        update("workflow_runs")
            .value("deadline_at_ms", DatabaseValue::Null)
            .where_eq("run_id", "sequential-run")
            .execute(&store.connection),
    )
    .expect("clear deadline");
}

struct FailRetainedSuccessors;
impl WorkflowOutputFault for FailRetainedSuccessors {
    fn after_boundary(
        &self,
        boundary: WorkflowOutputBoundary,
        _output: &ValidatedOutput,
    ) -> Result<(), WorkflowStoreError> {
        if boundary == WorkflowOutputBoundary::SuccessorsMaterialized {
            return Err(WorkflowStoreError::InvalidData(
                "injected successor failure".into(),
            ));
        }
        Ok(())
    }
}

#[test]
fn current_revision_input_source_requires_admission_proof() {
    let (_temp, mut store) = initialized_store();
    let output = ValidatedOutput {
        output_id: "current-output".into(),
        run_id: "run-1".into(),
        node_id: "review".into(),
        activation_id: activation_id(),
        schema_id: definition("example").nodes["review"]
            .output
            .type_name
            .clone(),
        schema_version: 1,
        value: serde_json::json!(2),
        artifact_reference: None,
        created_at_ms: 12,
    };
    store.persist_validated_output(&output).expect("output");
    let mut edge = sequential_definition().edges[0].clone();
    edge.from = "review".into();
    let edge = run_graph::RunGraphEdge {
        edge_id: 99,
        revision: 2,
        edge,
    };
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    let mut retry_edge = edge.clone();
    retry_edge.edge.kind = bcode_workflow::EdgeKind::Retry { max_attempts: 2 };
    assert!(
        successor_input_source_identity(transaction.as_ref(), "run-1", 2, 0, &retry_edge)
            .expect("retry is not an input")
            .is_none()
    );
    let error = successor_input_source_identity(transaction.as_ref(), "run-1", 2, 0, &edge)
        .expect_err("historical source");
    assert!(error.to_string().contains("graph reconciliation"));
    block_on(transaction.rollback()).expect("rollback");
}

#[test]
fn deferred_retained_source_checks_inputs_during_settlement() {
    for value in [2, 3] {
        assert_deferred_retained_source(value, false);
        assert_deferred_retained_source(value, true);
    }
}

fn readmit_fixture_source_at_current_revision(store: &mut WorkflowStore, identity: &str) {
    // Replace only the unstarted fixture activation, not a completed result.
    block_on(async {
        delete("workflow_retained_edge_bindings")
            .where_eq("source_node_id", "other")
            .execute(&store.connection)
            .await?;
        delete("workflow_leaf_retentions")
            .where_eq("node_id", "other")
            .execute(&store.connection)
            .await?;
        delete("workflow_activation_graph_bindings")
            .where_eq("node_id", "other")
            .execute(&store.connection)
            .await?;
        delete("workflow_activations")
            .where_eq("node_id", "other")
            .execute(&store.connection)
            .await?;
        update("workflow_runs")
            .value("target_artifact_id", "artifact-a")
            .value("coordinator_daemon_instance_id", "daemon-a")
            .value("coordinator_generation", 1)
            .value("coordinator_fencing_token", "token-a")
            .where_eq("run_id", "run-1")
            .execute(&store.connection)
            .await?;
        Ok::<(), switchy_database::DatabaseError>(())
    })
    .expect("current-source fixture");
    let authority = store
        .execution_authority("run-1")
        .expect("authority")
        .expect("owner");
    store
        .create_activation_at_graph_revision(
            &NewActivation {
                run_id: "run-1".into(),
                node_id: "other".into(),
                activation_id: identity.into(),
                dependency_generation: 0,
                input: Some(serde_json::json!(1)),
                created_at_ms: 26,
            },
            2,
            &authority,
        )
        .expect("ownership-checked admission");
}

fn assert_deferred_retained_source(value: u32, current_source: bool) {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    let mut graph = sequential_definition();
    let mut other = graph.nodes["first"].clone();
    other.id = "other".into();
    graph.nodes.insert(other.id.clone(), other);
    graph.entries.push("other".into());
    let mut edge = graph.edges[0].clone();
    edge.from = "other".into();
    graph.edges.push(edge);
    store
        .persist_definition("deferred", 1, &graph)
        .expect("definition");
    let mut run = new_run();
    run.definition_id = "deferred".into();
    store.create_run(&run).expect("run");
    let first_id = activation_identity(&run.run_id, "first", 0);
    let other_id = activation_identity(&run.run_id, "other", 0);
    let first = ValidatedOutput {
        output_id: "first-output".into(),
        run_id: run.run_id.clone(),
        node_id: "first".into(),
        activation_id: first_id.clone(),
        schema_id: "u32".into(),
        schema_version: 1,
        value: serde_json::json!(2),
        artifact_reference: None,
        created_at_ms: 20,
    };
    store
        .persist_validated_output(&first)
        .expect("first source");
    fixture_insert_deferred_candidate(&store.connection);
    let request = bcode_workflow::WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: run.run_id.clone(),
        mutation_id: "deferred".into(),
        expected_revision: 1,
        edits: vec![],
        reconciliation: vec![
            bcode_workflow::WorkflowRunGraphReconciliation::RetainWithBindings {
                activation_id: first_id,
                edge_ids: vec![0],
            },
            bcode_workflow::WorkflowRunGraphReconciliation::RetainWithBindings {
                activation_id: other_id.clone(),
                edge_ids: vec![1],
            },
        ],
    };
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    run_graph::persist_graph_publication(
        transaction.as_ref(),
        &request,
        &[("other".into(), other_id.clone())],
        25,
    )
    .expect("publication");
    database::commit(transaction).expect("commit");
    assert_eq!(
        store
            .activations_for_run(&run.run_id, 10)
            .expect("deferred")
            .len(),
        2
    );
    if current_source {
        readmit_fixture_source_at_current_revision(&mut store, &other_id);
    }
    let late = ValidatedOutput {
        output_id: "other-output".into(),
        node_id: "other".into(),
        activation_id: other_id,
        value: serde_json::json!(value),
        created_at_ms: 30,
        ..first.clone()
    };
    let result = store.persist_validated_output(&late);
    if value == 2 {
        assert_eq!(result.expect("matching input").activated.len(), 1);
    } else {
        assert!(
            result
                .expect_err("conflicting input")
                .to_string()
                .contains("consistent inputs")
        );
        assert_eq!(
            store.validated_outputs(&run.run_id, 10).expect("outputs"),
            vec![first]
        );
        assert_eq!(
            store
                .activations_for_run(&run.run_id, 10)
                .expect("rollback")
                .len(),
            2
        );
    }
}

fn fixture_insert_deferred_candidate(db: &dyn Database) {
    block_on(
        insert("workflow_graph_edit_candidates")
            .value("run_id", "run-1")
            .value("mutation_id", "deferred")
            .value("expected_revision", 1)
            .value("request_json", "{}")
            .value("authority_json", "{}")
            .value("created_at_ms", 1)
            .execute(db),
    )
    .expect("candidate fixture");
}

#[test]
fn completed_sources_converge_independently_of_reconciliation_order() {
    for reverse in [false, true] {
        for other_value in [2, 3] {
            assert_completed_source_convergence(reverse, other_value);
        }
    }
}

fn assert_completed_source_convergence(reverse: bool, other_value: u32) {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    let mut graph = sequential_definition();
    let mut other = graph.nodes["first"].clone();
    other.id = "other".into();
    graph.nodes.insert(other.id.clone(), other);
    graph.entries.push("other".into());
    let mut edge = graph.edges[0].clone();
    edge.from = "other".into();
    graph.edges.push(edge);
    store
        .persist_definition("converge", 1, &graph)
        .expect("definition");
    let mut run = new_run();
    run.definition_id = "converge".into();
    store.create_run(&run).expect("run");
    let mut reconciliation = Vec::new();
    for (node, value, edge_id) in [("first", 2, 0), ("other", other_value, 1)] {
        let identity = activation_identity(&run.run_id, node, 0);
        store
            .persist_validated_output(&ValidatedOutput {
                output_id: format!("output-{node}"),
                run_id: run.run_id.clone(),
                node_id: node.into(),
                activation_id: identity.clone(),
                schema_id: "u32".into(),
                schema_version: 1,
                value: serde_json::json!(value),
                artifact_reference: None,
                created_at_ms: 20,
            })
            .expect("source output");
        reconciliation.push(
            bcode_workflow::WorkflowRunGraphReconciliation::RetainWithBindings {
                activation_id: identity,
                edge_ids: vec![edge_id],
            },
        );
    }
    block_on(
        delete("workflow_activation_graph_bindings")
            .where_eq("node_id", "second")
            .execute(&store.connection),
    )
    .expect("remove successor binding");
    block_on(
        delete("workflow_activations")
            .where_eq("node_id", "second")
            .execute(&store.connection),
    )
    .expect("remove successor");
    fixture_insert_convergence_candidate(&store.connection);
    if reverse {
        reconciliation.reverse();
    }
    let request = bcode_workflow::WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: run.run_id.clone(),
        mutation_id: "converge".into(),
        expected_revision: 1,
        edits: vec![],
        reconciliation,
    };
    {
        let transaction =
            database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
        let result = run_graph::persist_graph_publication(transaction.as_ref(), &request, &[], 25);
        if other_value == 2 {
            assert_eq!(result.expect("agreeing sources"), 2);
            database::commit(transaction).expect("commit");
        } else {
            assert!(
                result
                    .expect_err("conflicting sources")
                    .to_string()
                    .contains("consistent inputs")
            );
        }
    }
    let count: u64 = fixture_scalar_unsigned(
        &store.connection,
        &select("workflow_activations")
            .project_as(switchy_database::query::count_all(), "count")
            .where_eq("node_id", "second"),
        "count",
    )
    .expect("count");
    assert_eq!(count, u64::from(other_value == 2));
    assert_eq!(
        run_graph::graph_revision(&store.connection, &run.run_id).expect("revision"),
        Some(if other_value == 2 { 2 } else { 1 })
    );
}

fn fixture_insert_convergence_candidate(db: &dyn Database) {
    block_on(
        insert("workflow_graph_edit_candidates")
            .value("run_id", "run-1")
            .value("mutation_id", "converge")
            .value("expected_revision", 1)
            .value("request_json", "{}")
            .value("authority_json", "{}")
            .value("created_at_ms", 1)
            .execute(db),
    )
    .expect("unpublished fixture");
}

fn assert_ambiguous_completed_admission(
    store: &WorkflowStore,
    request: &bcode_workflow::WorkflowRunGraphEditBatch,
) {
    let mut ambiguous = request.clone();
    ambiguous
        .reconciliation
        .extend(request.reconciliation.clone());
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    let error =
        run_graph::admit_completed_result_successors(transaction.as_ref(), &ambiguous, 2, 25)
            .expect_err("ambiguous input source");
    assert!(error.to_string().contains("unambiguous input source"));
    let successors = fixture_scalar_unsigned(
        transaction.as_ref(),
        &select("workflow_activations")
            .project_as(switchy_database::query::count_all(), "count")
            .where_eq("node_id", "second"),
        "count",
    )
    .expect("successors");
    assert_eq!(successors, 0);
    block_on(transaction.rollback()).expect("rollback");
}

fn assert_completed_admission_late_failure(
    store: &WorkflowStore,
    request: &bcode_workflow::WorkflowRunGraphEditBatch,
) {
    block_on(
        create_trigger(
            "fail_successor_binding",
            "workflow_activation_graph_bindings",
        )
        .temporary(true)
        .before_insert()
        .when(E::new_column("node_id").compare(C::Equal, E::Text("second".into())))
        .raise_abort("injected binding failure")
        .execute(&store.connection),
    )
    .expect("fault trigger");
    {
        let transaction =
            database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
        let error = run_graph::persist_graph_publication(transaction.as_ref(), request, &[], 25)
            .expect_err("failure after activation insertion");
        assert!(error.to_string().contains("injected binding failure"));
        let inserted = !fixture_rows(
            transaction.as_ref(),
            &select("workflow_activations")
                .columns(&["node_id"])
                .where_eq("node_id", "second"),
        )
        .is_empty();
        assert!(inserted, "fault must occur after successor insertion");
        // Match publisher error propagation: dropping the transaction rolls back all writes.
    }
    assert_eq!(
        store
            .activations_for_run(&request.run_id, 10)
            .expect("rollback")
            .len(),
        1
    );
    block_on(
        store
            .connection
            .drop_trigger("fail_successor_binding", false),
    )
    .expect("remove fault");
}

fn parallel_publication_edits(
    parallel: &WorkflowDefinition,
) -> Vec<bcode_workflow::WorkflowRunGraphEdit> {
    let mut edits = parallel
        .nodes
        .values()
        .map(|node| bcode_workflow::WorkflowRunGraphEdit::AddNode {
            node: node.clone(),
            entry: parallel.entries.contains(&node.id),
            exit: parallel.exits.contains(&node.id),
        })
        .collect::<Vec<_>>();
    edits.extend(parallel.edges.iter().enumerate().map(|(index, edge)| {
        bcode_workflow::WorkflowRunGraphEdit::AddEdge {
            edge_id: u64::try_from(index).expect("edge") + 1,
            edge: edge.clone(),
        }
    }));
    edits
}

fn assert_replaced_parallel_history(store: &WorkflowStore, run_id: &str) {
    let rows = fixture_rows(
        &store.connection,
        &select("workflow_activations")
            .columns(&["status", "input_json"])
            .where_eq("run_id", run_id)
            .where_eq("node_id", "join"),
    );
    let row = rows.first().expect("historical join");
    let historical = (fixture_text(row, "status"), fixture_text(row, "input_json"));
    assert_eq!(historical.0, "cancelled");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&historical.1).expect("input"),
        serde_json::json!([3, 2])
    );
    let outputs = store.validated_outputs(run_id, 10).expect("outputs");
    assert!(!outputs.iter().any(|output| output.node_id == "join"));
    assert!(
        outputs
            .iter()
            .any(|output| output.node_id == "replacement-join"
                && output.value == serde_json::json!([2, 3]))
    );
}

fn replace_admitted_parallel_join(
    store: &mut WorkflowStore,
    run: &NewWorkflowRun,
    authority: &WorkflowExecutionAuthority,
    parallel: &WorkflowDefinition,
) {
    use bcode_workflow::{
        WorkflowRunGraphEdit as Edit, WorkflowRunGraphReconciliation as Reconcile,
    };
    let mut join = parallel.nodes["join"].clone();
    join.id = "replacement-join".into();
    let request = bcode_workflow::WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: run.run_id.clone(),
        mutation_id: "replace-admitted-join".into(),
        expected_revision: 5,
        edits: vec![
            Edit::RemoveEdge { edge_id: 1 },
            Edit::RemoveEdge { edge_id: 2 },
            Edit::RemoveNode {
                node_id: "join".into(),
            },
            Edit::AddNode {
                node: join,
                entry: false,
                exit: true,
            },
        ],
        reconciliation: vec![
            Reconcile::Cancel {
                activation_id: activation_identity(&run.run_id, "join", 0),
            },
            Reconcile::Retain {
                activation_id: activation_identity(&run.run_id, "extra", 0),
            },
            Reconcile::RetainWithBindings {
                activation_id: activation_identity(&run.run_id, "first", 0),
                edge_ids: vec![0],
            },
            Reconcile::RetainWithBindings {
                activation_id: activation_identity(&run.run_id, "left", 0),
                edge_ids: vec![3],
            },
            Reconcile::RetainWithBindings {
                activation_id: activation_identity(&run.run_id, "right", 0),
                edge_ids: vec![4],
            },
        ],
    };
    let mut request = request;
    request
        .edits
        .extend(parallel.edges.iter().enumerate().map(|(index, edge)| {
            let mut edge = edge.clone();
            edge.to = "replacement-join".into();
            Edit::AddEdge {
                edge_id: u64::try_from(index).expect("edge") + 3,
                edge,
            }
        }));
    store
        .stage_run_graph_edit(&request, authority, 32)
        .expect("stage replacement");
    store
        .publish_retained_leaf_run_graph_edit(&run.run_id, &request.mutation_id, authority, 33)
        .expect("replace admitted join");
}

#[test]
fn published_direct_dependency_join_waits_for_both_sources() {
    let (_temp, mut store, run, authority, _) = connected_publication_fixture();
    let mut peer = store
        .current_run_graph_node(&run.run_id, "second")
        .expect("node")
        .expect("second")
        .node;
    peer.id = "peer".into();
    let request = bcode_workflow::WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: run.run_id.clone(),
        mutation_id: "join-peer".into(),
        expected_revision: 2,
        edits: vec![
            bcode_workflow::WorkflowRunGraphEdit::AddNode {
                node: peer,
                entry: true,
                exit: false,
            },
            bcode_workflow::WorkflowRunGraphEdit::AddEdge {
                edge_id: 1,
                edge: bcode_workflow::EdgeDefinition {
                    from: "peer".into(),
                    to: "second".into(),
                    kind: bcode_workflow::EdgeKind::Direct,
                    transform: None,
                },
            },
        ],
        reconciliation: vec![
            bcode_workflow::WorkflowRunGraphReconciliation::RetainWithBindings {
                activation_id: activation_identity(&run.run_id, "first", 0),
                edge_ids: vec![0],
            },
        ],
    };
    store
        .stage_run_graph_edit(&request, &authority, 25)
        .expect("stage join");
    store
        .publish_retained_leaf_run_graph_edit(&run.run_id, &request.mutation_id, &authority, 26)
        .expect("publish join");
    for node in ["first", "peer"] {
        let result = store
            .persist_validated_output(&ValidatedOutput {
                output_id: format!("{node}-joined"),
                run_id: run.run_id.clone(),
                node_id: node.into(),
                activation_id: activation_identity(&run.run_id, node, 0),
                schema_id: "u32".into(),
                schema_version: 1,
                value: serde_json::json!(2),
                artifact_reference: None,
                created_at_ms: 30,
            })
            .expect("settle source");
        assert_eq!(result.activated.len(), usize::from(node == "peer"));
        if node == "peer" {
            assert_eq!(result.activated[0].node_id, "second");
        }
    }
}

fn settle_inserted_repeat_successors(store: &mut WorkflowStore, run_id: &str) {
    for expected in ["additional", "evaluation"] {
        store
            .settle_pending_control_nodes(run_id, 10, 23)
            .expect("no early repeat");
        assert!(
            store
                .activation_admitted_graph_revision(
                    run_id,
                    "body",
                    &activation_identity(run_id, "body", 1)
                )
                .expect("next iteration absent")
                .is_none()
        );
        let pending = store
            .pending_activations(10)
            .expect("pending")
            .pop()
            .expect("successor");
        assert_eq!(pending.node_id, expected);
        store
            .persist_validated_output(&ValidatedOutput {
                output_id: format!("{expected}-output"),
                run_id: run_id.into(),
                node_id: pending.node_id,
                activation_id: pending.activation_id,
                schema_id: pending.node.output.type_name,
                schema_version: 1,
                value: pending.input.expect("successor input"),
                artifact_reference: None,
                created_at_ms: 24,
            })
            .expect("successor output");
    }
    store
        .settle_pending_control_nodes(run_id, 10, 25)
        .expect("repeat settlement");
    assert_eq!(
        store
            .activation_admitted_graph_revision(
                run_id,
                "body",
                &activation_identity(run_id, "body", 1)
            )
            .expect("next iteration"),
        Some(2)
    );
    assert_eq!(
        store
            .run_summary(run_id)
            .expect("summary")
            .expect("run")
            .status,
        RunStatus::Running
    );
}

fn connected_publication_fixture() -> (
    tempfile::TempDir,
    WorkflowStore,
    NewWorkflowRun,
    WorkflowExecutionAuthority,
    bcode_workflow::WorkflowRunGraphEditBatch,
) {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("sequential", 1, &sequential_definition())
        .expect("definition");
    let mut run = new_run();
    run.definition_id = "sequential".into();
    store.create_run(&run).expect("run");
    block_on(async {
        update("workflow_runs")
            .value("target_artifact_id", "artifact-a")
            .value("coordinator_daemon_instance_id", "daemon-a")
            .value("coordinator_generation", 1)
            .value("coordinator_fencing_token", "token-a")
            .where_eq("run_id", "run-1")
            .execute(&store.connection)
            .await?;
        Ok::<(), switchy_database::DatabaseError>(())
    })
    .expect("owner");
    let authority = store
        .execution_authority(&run.run_id)
        .expect("authority")
        .expect("owner");
    let first_id = activation_identity(&run.run_id, "first", 0);
    let mut second = store
        .current_run_graph_node(&run.run_id, "second")
        .expect("node")
        .expect("second")
        .node;
    second.name = "revised successor".into();
    let mut request = bcode_workflow::WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: run.run_id.clone(),
        mutation_id: "connected".into(),
        expected_revision: 1,
        edits: vec![bcode_workflow::WorkflowRunGraphEdit::ReplaceNode {
            node: second,
            entry: false,
            exit: true,
        }],
        reconciliation: vec![bcode_workflow::WorkflowRunGraphReconciliation::Retain {
            activation_id: first_id.clone(),
        }],
    };
    store
        .stage_run_graph_edit(&request, &authority, 20)
        .expect("stage missing binding");
    assert!(
        store
            .publish_retained_leaf_run_graph_edit(&run.run_id, &request.mutation_id, &authority, 21)
            .is_err()
    );
    assert_eq!(
        store.run_graph_revision(&run.run_id).expect("revision"),
        Some(1)
    );
    request.mutation_id = "bound-connected".into();
    request.reconciliation = vec![
        bcode_workflow::WorkflowRunGraphReconciliation::RetainWithBindings {
            activation_id: first_id,
            edge_ids: vec![0],
        },
    ];
    store
        .stage_run_graph_edit(&request, &authority, 22)
        .expect("stage");
    assert_eq!(
        store
            .publish_retained_leaf_run_graph_edit(&run.run_id, &request.mutation_id, &authority, 23)
            .expect("publish"),
        2
    );
    (temp, store, run, authority, request)
}

fn candidate_transfer_snapshot(
    store: &WorkflowStore,
    run_id: &str,
) -> Vec<(String, String, u64, u64)> {
    crate::database::rows(
        &store.connection,
        &store
            .connection
            .select("workflow_graph_edit_candidates")
            .project(switchy_database::query::identifier("mutation_id"))
            .project(switchy_database::query::identifier("request_json"))
            .project(switchy_database::query::identifier("expected_revision"))
            .project(switchy_database::query::identifier("created_at_ms"))
            .where_eq("run_id", run_id)
            .sort("mutation_id", switchy_database::query::SortDirection::Asc)
            .limit(256),
    )
    .unwrap()
    .iter()
    .map(|row| {
        (
            crate::database::text(row, "mutation_id").unwrap(),
            crate::database::text(row, "request_json").unwrap(),
            crate::database::unsigned(row, "expected_revision").unwrap(),
            crate::database::unsigned(row, "created_at_ms").unwrap(),
        )
    })
    .collect()
}

#[test]
fn same_artifact_transfer_preserves_exact_candidates_and_fences_old_owner() {
    let (temp, mut store, run, authority, request) = connected_publication_fixture();
    let before = candidate_transfer_snapshot(&store, &run.run_id);
    let replacement = WorkflowExecutionAuthority {
        daemon_instance_id: "candidate-replacement".into(),
        generation: authority.generation + 1,
        fencing_token: "candidate-replacement-fence".into(),
        ..authority.clone()
    };
    let mut foreign = replacement.clone();
    foreign.target_artifact_id = "foreign-artifact".into();
    assert!(
        store
            .transfer_execution_authority(&run.run_id, &authority, &foreign, 30)
            .is_err()
    );
    assert_eq!(candidate_transfer_snapshot(&store, &run.run_id), before);
    store
        .transfer_execution_authority(&run.run_id, &authority, &replacement, 31)
        .unwrap();
    drop(store);
    let store = WorkflowStore::open_in_state_dir(temp.path()).unwrap();
    assert_eq!(candidate_transfer_snapshot(&store, &run.run_id), before);
    assert_eq!(
        store
            .staged_run_graph_edit(&run.run_id, &request.mutation_id, &replacement)
            .unwrap(),
        Some(request.clone())
    );
    assert!(
        store
            .staged_run_graph_edit(&run.run_id, &request.mutation_id, &authority)
            .is_err()
    );
    assert_eq!(store.run_graph_revision(&run.run_id).unwrap(), Some(2));
}

#[test]
fn candidate_transfer_pages_preserve_every_intent_and_fence_old_owner() {
    let (temp, mut store, run, authority, mut request) = connected_publication_fixture();
    request.expected_revision = 2;
    for index in 0..129 {
        request.mutation_id = format!("paged-{index:03}");
        store
            .stage_run_graph_edit(&request, &authority, 30)
            .unwrap();
    }
    let before = candidate_transfer_snapshot(&store, &run.run_id);
    assert_eq!(before.len(), 131);
    let replacement = WorkflowExecutionAuthority {
        daemon_instance_id: "paged-replacement".into(),
        generation: authority.generation + 1,
        fencing_token: "paged-replacement-fence".into(),
        ..authority.clone()
    };
    store
        .transfer_execution_authority(&run.run_id, &authority, &replacement, 31)
        .unwrap();
    drop(store);
    let store = WorkflowStore::open_in_state_dir(temp.path()).unwrap();
    assert_eq!(candidate_transfer_snapshot(&store, &run.run_id), before);
    for (mutation_id, payload, _, _) in before {
        let expected: bcode_workflow::WorkflowRunGraphEditBatch =
            serde_json::from_str(&payload).unwrap();
        assert_eq!(
            store
                .staged_run_graph_edit(&run.run_id, &mutation_id, &replacement)
                .unwrap(),
            Some(expected)
        );
        assert!(
            store
                .staged_run_graph_edit(&run.run_id, &mutation_id, &authority)
                .is_err()
        );
    }
}

#[test]
fn unsupported_and_malformed_candidate_envelopes_roll_back_transfer() {
    for future in [false, true] {
        let (temp, mut store, run, authority, request) = connected_publication_fixture();
        let payload = if future {
            let mut value = serde_json::to_value(&request).unwrap();
            value["version"] = serde_json::json!(u32::MAX);
            value.to_string()
        } else {
            "{legacy-invalid-envelope".to_owned()
        };
        block_on(
            update("workflow_graph_edit_candidates")
                .value("request_json", payload.as_str())
                .where_eq("run_id", run.run_id.as_str())
                .where_eq("mutation_id", "connected")
                .execute(&store.connection),
        )
        .unwrap();
        let before = candidate_transfer_snapshot(&store, &run.run_id);
        let replacement = WorkflowExecutionAuthority {
            daemon_instance_id: "invalid-envelope-replacement".into(),
            generation: authority.generation + 1,
            fencing_token: "invalid-envelope-fence".into(),
            ..authority.clone()
        };
        assert!(
            store
                .transfer_execution_authority(&run.run_id, &authority, &replacement, 31)
                .is_err()
        );
        drop(store);
        let store = WorkflowStore::open_in_state_dir(temp.path()).unwrap();
        assert_eq!(candidate_transfer_snapshot(&store, &run.run_id), before);
        assert_eq!(
            store.execution_authority(&run.run_id).unwrap(),
            Some(authority.clone())
        );
        // bound-connected was processed first; its provenance must have rolled back too.
        assert_eq!(
            store
                .staged_run_graph_edit(&run.run_id, &request.mutation_id, &authority)
                .unwrap(),
            Some(request)
        );
        assert!(
            store
                .staged_run_graph_edit(&run.run_id, "connected", &authority)
                .is_err()
        );
    }
}

#[test]
fn interrupted_candidate_transfer_rolls_back_without_rewriting_intent() {
    let (temp, store, run, authority, request) = connected_publication_fixture();
    let before = candidate_transfer_snapshot(&store, &run.run_id);
    let replacement = WorkflowExecutionAuthority {
        daemon_instance_id: "candidate-replacement".into(),
        generation: authority.generation + 1,
        fencing_token: "candidate-replacement-fence".into(),
        ..authority.clone()
    };
    let transaction =
        crate::database::begin(&store.connection, TransactionMode::Immediate).unwrap();
    WorkflowStore::transfer_graph_edit_candidates_in(
        transaction.as_ref(),
        &run.run_id,
        &authority,
        &replacement,
    )
    .unwrap();
    // Simulate interruption before the encompassing ownership transaction commits.
    drop(transaction);
    drop(store);
    let store = WorkflowStore::open_in_state_dir(temp.path()).unwrap();
    assert_eq!(candidate_transfer_snapshot(&store, &run.run_id), before);
    assert_eq!(
        store.execution_authority(&run.run_id).unwrap(),
        Some(authority.clone())
    );
    assert_eq!(
        store
            .staged_run_graph_edit(&run.run_id, &request.mutation_id, &authority)
            .unwrap(),
        Some(request)
    );
}

#[test]
fn inconsistent_candidate_aborts_entire_authority_transfer() {
    let (_temp, mut store, run, authority, request) = connected_publication_fixture();
    let before = candidate_transfer_snapshot(&store, &run.run_id);
    // This sorts after bound-connected, exercising rollback of an earlier rebind too.
    block_on(
        update("workflow_graph_edit_candidates")
            .value("authority_json", "{}")
            .where_eq("run_id", run.run_id.as_str())
            .where_eq("mutation_id", "connected")
            .execute(&store.connection),
    )
    .unwrap();
    let replacement = WorkflowExecutionAuthority {
        daemon_instance_id: "candidate-replacement".into(),
        generation: authority.generation + 1,
        fencing_token: "candidate-replacement-fence".into(),
        ..authority.clone()
    };
    assert!(
        store
            .transfer_execution_authority(&run.run_id, &authority, &replacement, 31)
            .is_err()
    );
    assert_eq!(
        store.execution_authority(&run.run_id).unwrap(),
        Some(authority.clone())
    );
    assert_eq!(candidate_transfer_snapshot(&store, &run.run_id), before);
    assert_eq!(
        store
            .staged_run_graph_edit(&run.run_id, &request.mutation_id, &authority)
            .unwrap(),
        Some(request)
    );
}

#[test]
fn preparation_failure_rolls_back_resource_leases_and_events() {
    let (_temp, mut store, run, _, _) = connected_publication_fixture();
    let mut node = store
        .current_run_graph_node(&run.run_id, "first")
        .expect("node")
        .expect("first")
        .node;
    node.resources.push(bcode_workflow::ResourceClaim {
        resource: "workspace".into(),
        access: bcode_workflow::ResourceAccess::Write,
    });
    block_on(
        update("workflow_run_graph_nodes")
            .value(
                "node_json",
                serde_json::to_string(&node).expect("node JSON"),
            )
            .where_eq("run_id", run.run_id.as_str())
            .where_eq("node_id", "first")
            .execute(&store.connection),
    )
    .expect("resource fixture");
    // Fail after resource acquisition, at attempt insertion, to prove the shared
    // transaction rolls back leases as well as admission and its event history.
    block_on(
        create_trigger("reject_attempt", "workflow_attempts")
            .before_insert()
            .raise_abort("injected attempt failure")
            .execute(&store.connection),
    )
    .expect("fault");
    let before: u64 = fixture_scalar_unsigned(
        &store.connection,
        &select("workflow_events").project_as(switchy_database::query::count_all(), "count"),
        "count",
    )
    .expect("events");
    let id = activation_identity(&run.run_id, "first", 0);
    assert!(
        store
            .prepare_pending_activation(
                &run.run_id,
                "first",
                &id,
                DispatchSideEffect::Mutating,
                serde_json::json!({}),
                25
            )
            .is_err()
    );
    assert!(
        store
            .resource_leases_for_run(&run.run_id, 10)
            .expect("leases")
            .is_empty()
    );
    assert!(
        store
            .attempt_history(&run.run_id, None, 10)
            .expect("attempts")
            .is_empty()
    );
    let after: u64 = fixture_scalar_unsigned(
        &store.connection,
        &select("workflow_events").project_as(switchy_database::query::count_all(), "count"),
        "count",
    )
    .expect("events");
    assert_eq!(before, after);
    assert_eq!(
        store
            .pending_activations_for_run(&run.run_id, 10)
            .expect("pending")
            .len(),
        1
    );
}

#[tokio::test]
async fn dispatch_receipt_is_fenced_when_authority_transfers_after_owner_acceptance() {
    struct Transfer {
        path: PathBuf,
        prior: WorkflowExecutionAuthority,
        replacement: WorkflowExecutionAuthority,
    }
    impl WorkflowDispatchFault for Transfer {
        fn after_boundary(
            &self,
            boundary: WorkflowDispatchBoundary,
            request: &PreparedActivationDispatch,
        ) -> Result<(), WorkflowStoreError> {
            if boundary == WorkflowDispatchBoundary::OwnerAccepted {
                WorkflowStore::open_at_path(&self.path)?.transfer_execution_authority(
                    &request.activation.run_id,
                    &self.prior,
                    &self.replacement,
                    26,
                )?;
            }
            Ok(())
        }
    }
    let (_temp, mut store, run, authority, _) = connected_publication_fixture();
    let mut replacement = authority.clone();
    replacement.generation += 1;
    replacement.daemon_instance_id = "replacement".into();
    replacement.fencing_token = "replacement-token".into();
    let fault = Transfer {
        path: store.path().to_path_buf(),
        prior: authority.clone(),
        replacement: replacement.clone(),
    };
    let owner = MutationDispatchOwner(std::sync::atomic::AtomicUsize::new(0));
    let pending = store
        .pending_activations_for_run(&run.run_id, 1)
        .expect("pending");
    assert!(
        store
            .dispatch_activations_with_fault(&owner, &fault, pending, 25, Some(&authority))
            .await
            .is_err()
    );
    assert_eq!(owner.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    let attempts = store
        .attempt_history(&run.run_id, None, 10)
        .expect("attempts");
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].status, "prepared");
    assert!(!attempts[0].has_receipt);
    let recovered = store
        .reconcile_owned_prepared_attempts_for_run(&run.run_id, &replacement, 10, 27)
        .expect("replacement recovery");
    assert_eq!(recovered.repair_required.len(), 1);
    assert!(
        store
            .pending_activations_for_run(&run.run_id, 10)
            .expect("pending")
            .is_empty()
    );
}

#[test]
fn retained_handle_refuses_dispatch_handoff_and_receipts_after_contract_change() {
    for stage in 0..3 {
        let (_temp, mut store, run, authority, _) = connected_publication_fixture();
        let id = activation_identity(&run.run_id, "first", 0);
        let prepared = store
            .prepare_pending_activation(
                &run.run_id,
                "first",
                &id,
                DispatchSideEffect::ReadOnly,
                serde_json::json!({}),
                25,
            )
            .expect("prepare")
            .expect("pending");
        if stage != 0 {
            store
                .record_dispatch_handoff(&prepared, Some(&authority))
                .expect("handoff before incompatible contract");
        }
        let receipt = DispatchReceipt {
            run_id: run.run_id.clone(),
            node_id: "first".into(),
            activation_id: id,
            attempt: prepared.attempt,
            dispatch_identity: prepared.dispatch_identity.clone(),
            receipt: serde_json::json!({"accepted": true}),
            admitted_at_ms: 26,
        };
        let foreign = fixture_open(store.path(), true);
        block_on(
            update("workflow_storage_compatibility")
                .value("compatibility", WORKFLOW_STORE_COMPATIBILITY + 1)
                .where_eq("contract_id", 1)
                .execute(&foreign),
        )
        .expect("incompatible contract");
        let error = match stage {
            0 => store.record_dispatch_handoff(&prepared, Some(&authority)),
            1 => store.persist_owned_dispatch_receipt(&receipt, &authority),
            _ => store
                .recover_owned_dispatch_observation(
                    &receipt,
                    &authority,
                    AttemptObservation::Unknown,
                    27,
                )
                .map(|_| ()),
        }
        .expect_err("retained handle must refuse dispatch write");
        assert!(matches!(error, WorkflowStoreError::UnsupportedStore { .. }));
        let rows = fixture_rows(
            &foreign,
            &select("workflow_attempts")
                .columns(&["status", "receipt_json"])
                .where_eq("dispatch_identity", prepared.dispatch_identity.as_str()),
        );
        let row = rows.first().expect("attempt unchanged");
        let (status, has_receipt) = (
            fixture_text(row, "status"),
            database::optional_text(row, "receipt_json")
                .expect("receipt marker")
                .is_some(),
        );
        assert_eq!(status, "prepared");
        assert!(!has_receipt);
        if stage == 0 {
            let handed_off = fixture_scalar_unsigned(
                &foreign,
                &select("workflow_dispatch_handoffs")
                    .columns(&["handed_off"])
                    .where_eq("dispatch_identity", prepared.dispatch_identity.as_str()),
                "handed_off",
            )
            .expect("handoff unchanged")
                != 0;
            assert!(!handed_off);
        }
    }
}

#[test]
fn allowance_inspection_reports_unknown_beyond_read_budget() {
    let (_temp, mut store) = initialized_store();
    store
        .prepare_attempt(&PreparedAttempt {
            run_id: "run-1".into(),
            node_id: "review".into(),
            activation_id: activation_id(),
            attempt: 1,
            side_effect: DispatchSideEffect::ReadOnly,
            intent: serde_json::json!({}),
            prepared_at_ms: 1,
        })
        .unwrap();
    block_on(
        update("workflow_runs")
            .value("node_execution_cap", 10000)
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .unwrap();
    let templates = fixture_rows(
        &store.connection,
        &select("workflow_attempts")
            .columns(&[
                "run_id",
                "node_id",
                "activation_id",
                "side_effect",
                "status",
                "intent_json",
                "intent_checksum",
                "prepared_at_ms",
            ])
            .where_eq("attempt", 1),
    );
    for n in 2..=1001 {
        for row in &templates {
            block_on(
                insert("workflow_attempts")
                    .value("run_id", fixture_text(row, "run_id"))
                    .value("node_id", fixture_text(row, "node_id"))
                    .value("activation_id", fixture_text(row, "activation_id"))
                    .value("attempt", n)
                    .value("dispatch_identity", format!("fixture-{n}"))
                    .value("side_effect", fixture_text(row, "side_effect"))
                    .value("status", fixture_text(row, "status"))
                    .value("intent_json", fixture_text(row, "intent_json"))
                    .value("intent_checksum", fixture_text(row, "intent_checksum"))
                    .value("prepared_at_ms", fixture_unsigned(row, "prepared_at_ms"))
                    .execute(&store.connection),
            )
            .unwrap();
        }
    }
    let before = fixture_changes(&store.connection);
    let observation = store.execution_allowance_observation("run-1").unwrap();
    assert_eq!(observation.run_consumed, None);
    assert_eq!(observation.root_consumed, None);
    assert_eq!(observation.run_cap, 10000);
    assert_eq!(fixture_changes(&store.connection), before);
    // Terminal settlement does not refund attempts, and reaching the cap exactly
    // proves exhaustion even at the inspection probe boundary.
    block_on(
        update("workflow_attempts")
            .value("status", "cancelled")
            .where_eq("run_id", "run-1")
            .execute(&store.connection),
    )
    .unwrap();
    for (cap, expected) in [(1_000, Some(1_000)), (1_001, Some(1_001)), (1_002, None)] {
        block_on(
            update("workflow_runs")
                .value("node_execution_cap", cap)
                .where_eq("run_id", "run-1")
                .execute(&store.connection),
        )
        .unwrap();
        let before = fixture_changes(&store.connection);
        let observed = store.execution_allowance_observation("run-1").unwrap();
        assert_eq!(observed.run_consumed, expected);
        assert_eq!(observed.root_consumed, expected);
        assert_eq!(observed.exhausted(), expected.map(|_| true));
        assert_eq!(fixture_changes(&store.connection), before);
    }
}

#[test]
fn allowance_inspection_rejects_invalid_caps_without_mutation() {
    let (_temp, store) = initialized_store();
    for cap in [0_i64, -1] {
        block_on(
            update("workflow_runs")
                .value("node_execution_cap", cap)
                .where_eq("run_id", "run-1")
                .execute(&store.connection),
        )
        .unwrap();
        let before = fixture_changes(&store.connection);
        assert!(store.execution_allowance_observation("run-1").is_err());
        assert_eq!(fixture_changes(&store.connection), before);
    }
}

#[test]
fn execution_allowance_increase_is_fenced_and_compare_and_set() {
    let (_temp, mut store, run, authority, _) = connected_publication_fixture();
    let cap = store
        .execution_allowance_observation(&run.run_id)
        .unwrap()
        .run_cap;
    let pending = store.pending_activations_for_run(&run.run_id, 10).unwrap();
    let summary = store.run_summary(&run.run_id).unwrap();
    let mut stale = authority.clone();
    stale.generation += 1;
    let before = fixture_changes(&store.connection);
    assert!(cap > 1);
    assert!(
        store
            .increase_execution_allowance(&run.run_id, &authority, cap - 1, cap, 24)
            .is_err()
    );
    assert!(
        store
            .increase_execution_allowance(&run.run_id, &stale, cap, cap + 1, 25)
            .is_err()
    );
    assert!(
        store
            .increase_execution_allowance(&run.run_id, &authority, cap, cap, 25)
            .is_err()
    );
    assert_eq!(fixture_changes(&store.connection), before);
    store
        .increase_execution_allowance(&run.run_id, &authority, cap, cap + 1, 25)
        .unwrap();
    assert_eq!(
        store
            .execution_allowance_observation(&run.run_id)
            .unwrap()
            .run_cap,
        cap + 1
    );
    let after = fixture_changes(&store.connection);
    assert!(
        store
            .increase_execution_allowance(&run.run_id, &authority, cap - 1, cap + 1, 26)
            .is_err()
    );
    store
        .increase_execution_allowance(&run.run_id, &authority, cap, cap + 1, 26)
        .unwrap();
    assert!(
        store
            .increase_execution_allowance(&run.run_id, &authority, cap, cap + 2, 27)
            .is_err()
    );
    assert_eq!(fixture_changes(&store.connection), after);
    assert_eq!(store.run_summary(&run.run_id).unwrap(), summary);
    assert_eq!(
        store.pending_activations_for_run(&run.run_id, 10).unwrap(),
        pending
    );
    assert!(
        store
            .attempt_history(&run.run_id, None, 10)
            .unwrap()
            .is_empty()
    );
    for status in ["completed", "failed", "cancelled", "repair_required"] {
        block_on(
            update("workflow_runs")
                .value("status", status)
                .where_eq("run_id", run.run_id.as_str())
                .execute(&store.connection),
        )
        .unwrap();
        let before = fixture_changes(&store.connection);
        assert!(
            store
                .increase_execution_allowance(&run.run_id, &authority, cap + 1, cap + 2, 28)
                .is_err()
        );
        assert_eq!(fixture_changes(&store.connection), before);
    }
}

#[test]
fn execution_allowance_increase_survives_reopen_and_respects_barriers() {
    let (_temp, mut store, run, authority, _) = connected_publication_fixture();
    let cap = store
        .execution_allowance_observation(&run.run_id)
        .unwrap()
        .run_cap;
    store
        .increase_execution_allowance(&run.run_id, &authority, cap, cap + 1, 25)
        .unwrap();
    let path = store.path().to_path_buf();
    drop(store);
    let mut store = WorkflowStore::open_at_path(&path).unwrap();
    let before = fixture_changes(&store.connection);
    store
        .increase_execution_allowance(&run.run_id, &authority, cap, cap + 1, 26)
        .unwrap();
    assert_eq!(fixture_changes(&store.connection), before);
    let events: u64 = fixture_scalar_unsigned(
        &store.connection,
        &select("workflow_events")
            .project_as(switchy_database::query::count_all(), "count")
            .where_eq("run_id", run.run_id.as_str())
            .where_eq("event_type", "execution_allowance_increased"),
        "count",
    )
    .unwrap();
    assert_eq!(events, 1);
    for barrier in ["cancel", "recovery"] {
        if barrier == "cancel" {
            block_on(
                update("workflow_runs")
                    .value("cancellation_requested_at_ms", 27)
                    .where_eq("run_id", run.run_id.as_str())
                    .execute(&store.connection),
            )
            .unwrap();
        } else {
            block_on(
                update("workflow_runs")
                    .value("cancellation_requested_at_ms", DatabaseValue::Null)
                    .where_eq("run_id", run.run_id.as_str())
                    .execute(&store.connection),
            )
            .unwrap();
            block_on(
                insert("workflow_recovery_barriers")
                    .value("run_id", run.run_id.as_str())
                    .value("source_artifact_id", "original-artifact")
                    .value("created_at_ms", 1)
                    .execute(&store.connection),
            )
            .unwrap();
        }
        let before = fixture_changes(&store.connection);
        // Even a duplicate acknowledgement must not bypass current eligibility.
        for (expected, target) in [(cap, cap + 1), (cap + 1, cap + 2)] {
            assert!(
                store
                    .increase_execution_allowance(&run.run_id, &authority, expected, target, 28)
                    .is_err()
            );
        }
        assert_eq!(fixture_changes(&store.connection), before);
        assert_eq!(
            store
                .execution_allowance_observation(&run.run_id)
                .unwrap()
                .run_cap,
            cap + 1
        );
    }
}

#[test]
fn reassignment_rejects_empty_replacement_identity_without_writes() {
    let (_temp, mut store, run, authority, _) = connected_publication_fixture();
    let evidence = EndedOwnerEvidence {
        ended_daemon_instance_id: authority.daemon_instance_id.clone(),
        ended_target_artifact_id: authority.target_artifact_id.clone(),
        liveness: EndedOwnerLiveness::ObservedEnded,
        artifact_image_available: true,
    };
    for (artifact, daemon, fence) in [
        ("", "new-daemon", "new-fence"),
        ("new-artifact", "", "new-fence"),
        ("new-artifact", "new-daemon", ""),
    ] {
        let replacement = WorkflowExecutionAuthority {
            target_artifact_id: artifact.into(),
            daemon_instance_id: daemon.into(),
            fencing_token: fence.into(),
            generation: authority.generation + 1,
        };
        let before = fixture_changes(&store.connection);
        assert!(
            store
                .reassign_execution_authority_from_ended_owner(
                    &run.run_id,
                    &authority,
                    &replacement,
                    &evidence,
                    20,
                )
                .is_err()
        );
        assert_eq!(fixture_changes(&store.connection), before);
        assert_eq!(
            store.execution_authority(&run.run_id).expect("owner"),
            Some(authority.clone())
        );
    }
}

#[test]
fn pause_and_resume_reject_stale_owners_after_transfer() {
    let (_temp, mut store, run, authority, _) = connected_publication_fixture();
    let replacement = WorkflowExecutionAuthority {
        daemon_instance_id: "replacement".into(),
        generation: authority.generation + 1,
        fencing_token: "replacement-token".into(),
        ..authority.clone()
    };
    store
        .transfer_execution_authority(&run.run_id, &authority, &replacement, 20)
        .expect("transfer");
    assert!(store.pause_run_owned(&run.run_id, 21, &authority).is_err());
    assert!(
        store
            .pause_run_owned(&run.run_id, 22, &replacement)
            .expect("pause")
    );
    assert!(store.resume_run_owned(&run.run_id, 23, &authority).is_err());
    assert_eq!(
        store
            .run_summary(&run.run_id)
            .expect("summary")
            .expect("run")
            .status,
        RunStatus::Paused
    );
    assert!(
        store
            .resume_run_owned(&run.run_id, 24, &replacement)
            .expect("resume")
    );
    assert!(
        !store
            .resume_run_owned(&run.run_id, 25, &replacement)
            .expect("idempotent")
    );
}

#[test]
fn unresolved_repair_retains_owning_artifact_until_terminal_resolution() {
    let (_temp, mut store, run, authority, _) = connected_publication_fixture();
    let id = activation_identity(&run.run_id, "first", 0);
    let prepared = store
        .prepare_pending_activation(
            &run.run_id,
            "first",
            &id,
            DispatchSideEffect::Mutating,
            serde_json::json!({}),
            25,
        )
        .expect("prepare")
        .expect("pending");
    store
        .request_cancellation_owned(&run.run_id, 26, &authority)
        .expect("cancel");
    store
        .settle_orphaned_attempt_cancellation_owned(&prepared.dispatch_identity, 27, &authority)
        .expect("repair required");
    assert_eq!(
        store
            .run_summary(&run.run_id)
            .expect("summary")
            .expect("run")
            .status,
        RunStatus::RepairRequired
    );
    assert_eq!(
        store
            .active_target_artifact_ids(10)
            .expect("retained artifacts"),
        *std::slice::from_ref(&authority.target_artifact_id)
    );
    store
        .repair_attempt_owned(
            &prepared.dispatch_identity,
            &RepairResolution::ConfirmCancelled {
                message: "operator verified termination".into(),
            },
            28,
            &authority,
        )
        .expect("resolution");
    assert!(
        store
            .active_target_artifact_ids(10)
            .expect("terminal artifacts")
            .is_empty()
    );
}

#[test]
fn recovery_takeover_preserves_unsettled_attempt_and_blocks_execution_after_reopen() {
    let (temp, mut store, run, authority, _) = connected_publication_fixture();
    let id = activation_identity(&run.run_id, "first", 0);
    let prepared = store
        .prepare_pending_activation(
            &run.run_id,
            "first",
            &id,
            DispatchSideEffect::Mutating,
            serde_json::json!({}),
            10,
        )
        .expect("prepare")
        .expect("pending");
    let replacement = WorkflowExecutionAuthority {
        target_artifact_id: "new-artifact".into(),
        daemon_instance_id: "new-owner".into(),
        generation: authority.generation + 1,
        fencing_token: "new-fence".into(),
    };
    let evidence = EndedOwnerEvidence {
        ended_daemon_instance_id: authority.daemon_instance_id.clone(),
        ended_target_artifact_id: authority.target_artifact_id.clone(),
        liveness: EndedOwnerLiveness::ObservedEnded,
        artifact_image_available: true,
    };
    store
        .take_recovery_authority(&run.run_id, &authority, &replacement, &evidence, 20)
        .expect("takeover");
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    assert!(
        store
            .verify_execution_authority(&run.run_id, &authority)
            .is_err()
    );
    store
        .verify_execution_authority(&run.run_id, &replacement)
        .expect("new owner");
    assert_eq!(
        store
            .attempt_by_dispatch_identity(&prepared.dispatch_identity)
            .expect("lookup")
            .expect("attempt")
            .status,
        "prepared"
    );
    assert!(
        store
            .resume_run_owned(&run.run_id, 21, &replacement)
            .is_err()
    );
    assert!(
        store
            .finish_recovery_only(&run.run_id, &replacement, 22)
            .is_err()
    );
    assert!(
        store
            .record_dispatch_handoff(&prepared, Some(&replacement))
            .is_err()
    );
    let handed_off = fixture_scalar_unsigned(
        &store.connection,
        &select("workflow_dispatch_handoffs")
            .columns(&["handed_off"])
            .where_eq("dispatch_identity", prepared.dispatch_identity.as_str()),
        "handed_off",
    )
    .expect("handoff unchanged")
        != 0;
    assert!(!handed_off);
    let retained = store.active_target_artifact_ids(10).expect("retention");
    assert!(retained.contains(&authority.target_artifact_id));
    assert!(retained.contains(&replacement.target_artifact_id));
    store
        .request_cancellation_owned(&run.run_id, 23, &replacement)
        .expect("cancel intent allowed");
}

#[test]
fn recovery_takeover_settles_cancellation_from_owner_evidence_after_restart() {
    let (temp, mut store, run, authority, _) = connected_publication_fixture();
    let activation_id = activation_identity(&run.run_id, "first", 0);
    let prepared = store
        .prepare_pending_activation(
            &run.run_id,
            "first",
            &activation_id,
            DispatchSideEffect::Mutating,
            serde_json::json!({}),
            10,
        )
        .expect("prepare")
        .expect("pending");
    store
        .record_dispatch_handoff(&prepared, Some(&authority))
        .expect("handoff");
    store
        .persist_dispatch_receipt(&DispatchReceipt {
            run_id: run.run_id.clone(),
            node_id: "first".into(),
            activation_id,
            attempt: prepared.attempt,
            dispatch_identity: prepared.dispatch_identity.clone(),
            receipt: serde_json::json!({"owner": "test.operation/v1", "accepted": true}),
            admitted_at_ms: 11,
        })
        .expect("acceptance");
    let replacement = WorkflowExecutionAuthority {
        target_artifact_id: "new-artifact".into(),
        daemon_instance_id: "new-owner".into(),
        generation: authority.generation + 1,
        fencing_token: "new-fence".into(),
    };
    let evidence = EndedOwnerEvidence {
        ended_daemon_instance_id: authority.daemon_instance_id.clone(),
        ended_target_artifact_id: authority.target_artifact_id.clone(),
        liveness: EndedOwnerLiveness::ObservedEnded,
        artifact_image_available: true,
    };
    store
        .take_recovery_authority(&run.run_id, &authority, &replacement, &evidence, 20)
        .expect("takeover");
    store
        .request_cancellation_owned(&run.run_id, 21, &replacement)
        .expect("durable intent");
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("restart recovery");
    assert!(store.is_recovery_only(&run.run_id).expect("barrier"));
    assert!(
        store
            .request_cancellation_owned(&run.run_id, 22, &authority)
            .is_err()
    );
    let pending = store
        .active_attempt_cancellations(&run.run_id, 10)
        .expect("pending cancellation");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].dispatch_identity, prepared.dispatch_identity);
    assert!(pending[0].receipt.is_some());
    store
        .verify_execution_authority(&run.run_id, &replacement)
        .expect("recovery owner");
    store
        .apply_attempt_observation(
            &prepared.dispatch_identity,
            AttemptObservation::Cancelled,
            23,
        )
        .expect("operation owner terminal evidence");
    drop(store);
    let store = WorkflowStore::open_in_state_dir(temp.path()).expect("restart after settlement");
    assert_eq!(
        store
            .run_summary(&run.run_id)
            .expect("summary")
            .expect("run")
            .status,
        RunStatus::Cancelled
    );
    assert!(
        store
            .active_attempt_cancellations(&run.run_id, 10)
            .expect("settled cancellations")
            .is_empty()
    );
    assert!(
        store
            .active_target_artifact_ids(10)
            .expect("retention")
            .is_empty()
    );
}

#[test]
fn owned_sibling_cancellation_discovery_survives_restart_and_rejects_stale_owner() {
    let (temp, mut store, run, authority, _) = connected_publication_fixture();
    let id = activation_identity(&run.run_id, "first", 0);
    let prepared = store
        .prepare_pending_activation(
            &run.run_id,
            "first",
            &id,
            DispatchSideEffect::Mutating,
            serde_json::json!({}),
            10,
        )
        .expect("prepare")
        .expect("pending");
    block_on(
        update("workflow_attempts")
            .value("status", "sibling_cancelling")
            .where_eq("dispatch_identity", prepared.dispatch_identity.as_str())
            .execute(&store.connection),
    )
    .expect("durable sibling intent");
    drop(store);
    let store = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
    let pending = store
        .pending_owned_sibling_cancellations(&run.run_id, &authority, 1)
        .expect("pending");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].dispatch_identity, prepared.dispatch_identity);
    assert_eq!(
        store.pending_sibling_cancellations(1).expect("global"),
        pending
    );
    block_on(
        update("workflow_attempts")
            .value(
                "receipt_json",
                serde_json::json!("x".repeat(MAX_INLINE_JSON_BYTES)).to_string(),
            )
            .where_eq("dispatch_identity", prepared.dispatch_identity.as_str())
            .execute(&store.connection),
    )
    .expect("oversized receipt");
    let before = fixture_changes(&store.connection);
    assert!(store.pending_sibling_cancellations(1).is_err());
    assert!(
        store
            .pending_owned_sibling_cancellations(&run.run_id, &authority, 1)
            .is_err()
    );
    assert_eq!(fixture_changes(&store.connection), before);
    let stale = WorkflowExecutionAuthority {
        generation: authority.generation + 1,
        ..authority
    };
    assert!(
        store
            .pending_owned_sibling_cancellations(&run.run_id, &stale, 1)
            .is_err()
    );
}

#[test]
fn damaged_recovery_provenance_cannot_clear_barrier() {
    let (_temp, mut store, run, authority, _) = connected_publication_fixture();
    store
        .enter_recovery_only(&run.run_id, &authority, 20)
        .expect("barrier");
    block_on(
        store.connection.exec_update_count(
            &(update("workflow_recovery_barriers")
                .value("source_artifact_id", zero_blob(1_000_000))
                .where_eq("run_id", run.run_id.as_str())),
        ),
    )
    .expect("damaged provenance");
    let before = fixture_changes(&store.connection);
    assert!(store.recovery_source_artifact(&run.run_id).is_err());
    assert!(
        store
            .finish_recovery_only(&run.run_id, &authority, 21)
            .is_err()
    );
    assert!(
        store
            .resume_recovered_run(&run.run_id, &authority, 22)
            .is_err()
    );
    assert!(store.is_recovery_only(&run.run_id).expect("preserved"));
    assert_eq!(fixture_changes(&store.connection), before);
}

#[test]
fn recovery_resume_rolls_back_barrier_and_audit_when_resume_write_fails() {
    let (temp, mut store, run, authority, _) = connected_publication_fixture();
    store
        .enter_recovery_only(&run.run_id, &authority, 20)
        .expect("enter");
    let before = store.event_history(&run.run_id, None, 100).expect("events");
    block_on(
        create_trigger("fail_resume_write", "workflow_runs")
            .before_update_of(["status"])
            .when(E::new_column("status").compare(C::Equal, E::Text("running".into())))
            .raise_abort("injected resume failure")
            .execute(&store.connection),
    )
    .expect("fault");
    assert!(
        store
            .resume_recovered_run(&run.run_id, &authority, 21)
            .is_err()
    );
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    assert!(
        store
            .is_recovery_only(&run.run_id)
            .expect("barrier survived")
    );
    assert_eq!(
        store
            .run_summary(&run.run_id)
            .expect("summary")
            .expect("run")
            .status,
        RunStatus::Paused
    );
    assert_eq!(
        store.event_history(&run.run_id, None, 100).expect("events"),
        before
    );
    block_on(store.connection.drop_trigger("fail_resume_write", false)).expect("remove fault");
    assert!(
        store
            .resume_recovered_run(&run.run_id, &authority, 22)
            .expect("retry")
    );
    assert!(!store.is_recovery_only(&run.run_id).expect("completed"));
    assert_eq!(
        store
            .run_summary(&run.run_id)
            .expect("summary")
            .expect("run")
            .status,
        RunStatus::Running
    );
}

#[test]
fn recovery_repair_abandonment_does_not_grant_execution() {
    let (_temp, mut store, run, authority, _) = connected_publication_fixture();
    let id = activation_identity(&run.run_id, "first", 0);
    let prepared = store
        .prepare_pending_activation(
            &run.run_id,
            "first",
            &id,
            DispatchSideEffect::Mutating,
            serde_json::json!({}),
            10,
        )
        .expect("prepare")
        .expect("pending");
    store
        .reconcile_prepared_attempts(10, 11)
        .expect("ambiguity");
    store
        .enter_recovery_only(&run.run_id, &authority, 12)
        .expect("recovery");
    let result = store
        .repair_attempt_owned(
            &prepared.dispatch_identity,
            &RepairResolution::AbandonForExplicitRetry {
                reason: "operator accepts unresolved effects".into(),
            },
            13,
            &authority,
        )
        .expect("resolve without execution");
    assert_eq!(result.attempt_status, "abandoned");
    assert_eq!(result.run_status, RunStatus::Paused);
    assert!(store.is_recovery_only(&run.run_id).expect("barrier"));
    assert!(store.resume_run_owned(&run.run_id, 14, &authority).is_err());
}

#[test]
fn recovery_discovery_includes_fenced_repair_runs() {
    let (_temp, mut store, run, authority, _) = connected_publication_fixture();
    let id = activation_identity(&run.run_id, "first", 0);
    store
        .prepare_pending_activation(
            &run.run_id,
            "first",
            &id,
            DispatchSideEffect::Mutating,
            serde_json::json!({}),
            10,
        )
        .expect("prepare");
    store.reconcile_prepared_attempts(10, 11).expect("repair");
    assert!(
        store
            .recovery_candidate_run_ids(10)
            .expect("before")
            .is_empty()
    );
    store
        .enter_recovery_only(&run.run_id, &authority, 12)
        .expect("recovery");
    assert_eq!(
        store.recovery_candidate_run_ids(10).expect("after"),
        [run.run_id]
    );
}

#[test]
fn recovery_completion_preserves_barrier_for_active_child() {
    let (_temp, mut store, run, authority, _) = connected_publication_fixture();
    let mut child = run.clone();
    child.run_id = "recovery-child".into();
    store.create_run(&child).expect("child");
    block_on(
        insert("workflow_run_links")
            .value("root_run_id", run.run_id.as_str())
            .value("parent_run_id", run.run_id.as_str())
            .value("parent_node_id", "first")
            .value("parent_activation_id", "activation")
            .value("parent_attempt", 1)
            .value("child_run_id", child.run_id.as_str())
            .value("version", 1)
            .value("target_json", "{}")
            .value("depth", 2)
            .value("created_at_ms", 1)
            .execute(&store.connection),
    )
    .expect("child link fixture");
    store
        .enter_recovery_only(&run.run_id, &authority, 20)
        .expect("recovery");
    assert!(
        store
            .finish_recovery_only(&run.run_id, &authority, 21)
            .is_err()
    );
    assert!(
        store
            .is_recovery_only(&run.run_id)
            .expect("barrier retained")
    );
    store
        .request_cancellation(&child.run_id, 22)
        .expect("child termination");
    assert!(
        store
            .finish_recovery_only(&run.run_id, &authority, 23)
            .expect("settled child")
    );
}

#[test]
fn retained_handle_refuses_cancellation_signal_markers_after_contract_change() {
    for sibling in [false, true] {
        let (_temp, mut store, run, authority, _) = connected_publication_fixture();
        let id = activation_identity(&run.run_id, "first", 0);
        let prepared = store
            .prepare_pending_activation(
                &run.run_id,
                "first",
                &id,
                DispatchSideEffect::Mutating,
                serde_json::json!({}),
                10,
            )
            .expect("prepare")
            .expect("pending");
        store
            .request_cancellation_owned(&run.run_id, 12, &authority)
            .expect("intent");
        if sibling {
            block_on(
                update("workflow_attempts")
                    .value("status", "sibling_cancelling")
                    .where_eq("dispatch_identity", prepared.dispatch_identity.as_str())
                    .execute(&store.connection),
            )
            .expect("sibling cancellation intent");
        }
        let foreign = fixture_open(store.path(), true);
        block_on(
            update("workflow_storage_compatibility")
                .value("compatibility", WORKFLOW_STORE_COMPATIBILITY + 1)
                .where_eq("contract_id", 1)
                .execute(&foreign),
        )
        .expect("incompatible contract");
        let error = if sibling {
            store.mark_sibling_cancellation_signalled_owned(
                &prepared.dispatch_identity,
                13,
                &authority,
            )
        } else {
            store.mark_cancellation_signalled_owned(&prepared.dispatch_identity, 13, &authority)
        }
        .expect_err("retained handle must refuse signal marker");
        assert!(matches!(error, WorkflowStoreError::UnsupportedStore { .. }));
        let status: String = fixture_scalar_text(
            &foreign,
            &select("workflow_attempts")
                .columns(&["status"])
                .where_eq("dispatch_identity", prepared.dispatch_identity.as_str()),
            "status",
        )
        .expect("attempt unchanged");
        assert_eq!(
            status,
            if sibling {
                "sibling_cancelling"
            } else {
                "prepared"
            }
        );
    }
}

#[test]
fn retained_handle_refuses_never_dispatched_cancellation_after_contract_change() {
    let (_temp, mut store, run, authority, _) = connected_publication_fixture();
    let id = activation_identity(&run.run_id, "first", 0);
    let prepared = store
        .prepare_pending_activation(
            &run.run_id,
            "first",
            &id,
            DispatchSideEffect::Mutating,
            serde_json::json!({}),
            10,
        )
        .expect("prepare")
        .expect("pending");
    store
        .enter_recovery_only(&run.run_id, &authority, 11)
        .expect("recovery");
    store
        .request_cancellation_owned(&run.run_id, 12, &authority)
        .expect("intent");
    let foreign = fixture_open(store.path(), true);
    block_on(
        update("workflow_storage_compatibility")
            .value("compatibility", WORKFLOW_STORE_COMPATIBILITY + 1)
            .where_eq("contract_id", 1)
            .execute(&foreign),
    )
    .expect("incompatible contract");
    let error = store
        .cancel_never_dispatched_owned(&prepared.dispatch_identity, &authority, 13)
        .expect_err("retained handle must refuse cancellation");
    assert!(matches!(error, WorkflowStoreError::UnsupportedStore { .. }));
    let status = fixture_scalar_text(
        &foreign,
        &select("workflow_attempts")
            .columns(&["status"])
            .where_eq("dispatch_identity", prepared.dispatch_identity.as_str()),
        "status",
    )
    .expect("unchanged attempt");
    let handed_off = fixture_scalar_unsigned(
        &foreign,
        &select("workflow_dispatch_handoffs")
            .columns(&["handed_off"])
            .where_eq("dispatch_identity", prepared.dispatch_identity.as_str()),
        "handed_off",
    )
    .expect("unchanged handoff")
        != 0;
    assert_eq!(status, "prepared");
    assert!(!handed_off);
}

#[test]
fn retained_handle_refuses_orphaned_cancellation_settlement_after_contract_change() {
    let (_temp, mut store, run, authority, _) = connected_publication_fixture();
    let id = activation_identity(&run.run_id, "first", 0);
    let prepared = store
        .prepare_pending_activation(
            &run.run_id,
            "first",
            &id,
            DispatchSideEffect::Mutating,
            serde_json::json!({}),
            10,
        )
        .expect("prepare")
        .expect("pending");
    store
        .enter_recovery_only(&run.run_id, &authority, 11)
        .expect("recovery");
    store
        .request_cancellation_owned(&run.run_id, 12, &authority)
        .expect("intent");
    let foreign = fixture_open(store.path(), true);
    block_on(
        update("workflow_storage_compatibility")
            .value("compatibility", WORKFLOW_STORE_COMPATIBILITY + 1)
            .where_eq("contract_id", 1)
            .execute(&foreign),
    )
    .expect("incompatible contract");
    let error = store
        .settle_orphaned_attempt_cancellation_owned(&prepared.dispatch_identity, 13, &authority)
        .expect_err("retained handle must refuse settlement");
    assert!(matches!(error, WorkflowStoreError::UnsupportedStore { .. }));
    let status: String = fixture_scalar_text(
        &foreign,
        &select("workflow_attempts")
            .columns(&["status"])
            .where_eq("dispatch_identity", prepared.dispatch_identity.as_str()),
        "status",
    )
    .expect("attempt status");
    assert_eq!(status, "prepared");
    let run_status: String = fixture_scalar_text(
        &foreign,
        &select("workflow_runs")
            .columns(&["status"])
            .where_eq("run_id", run.run_id.as_str()),
        "status",
    )
    .expect("run status");
    assert_ne!(run_status, "repair_required");
}

#[test]
fn cancellation_uses_never_dispatched_proof_not_missing_receipts() {
    for handoff in [false, true] {
        let (_temp, mut store, run, authority, _) = connected_publication_fixture();
        let id = activation_identity(&run.run_id, "first", 0);
        let prepared = store
            .prepare_pending_activation(
                &run.run_id,
                "first",
                &id,
                DispatchSideEffect::Mutating,
                serde_json::json!({}),
                10,
            )
            .expect("prepare")
            .expect("pending");
        if handoff {
            store
                .record_dispatch_handoff(&prepared, Some(&authority))
                .expect("handoff");
        }
        store
            .enter_recovery_only(&run.run_id, &authority, 11)
            .expect("recovery");
        store
            .request_cancellation_owned(&run.run_id, 12, &authority)
            .expect("intent");
        assert_eq!(
            store
                .cancel_never_dispatched_owned(&prepared.dispatch_identity, &authority, 13)
                .expect("proof"),
            !handoff
        );
        assert_eq!(
            store
                .attempt_by_dispatch_identity(&prepared.dispatch_identity)
                .expect("lookup")
                .expect("attempt")
                .status,
            if handoff { "prepared" } else { "cancelled" }
        );
    }
}

#[test]
fn offline_continuation_preserves_run_and_rejects_stale_owner() {
    let temp = tempfile::tempdir().unwrap();
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).unwrap();
    let mut definition = sequential_definition();
    for node in definition.nodes.values_mut() {
        node.kind = bcode_workflow::NodeKind::Agent;
        node.configuration = serde_json::json!({
            "version": 3, "execution_target": "shared_parent_sequential", "agent_profile": "build",
            "provider": null, "model": null, "output": {"mode":"preserve_input"},
            "read_only": false, "tool_capability": "mutating", "tool_allowlist": [],
            "timeout_ms": 1000, "prompt_mode": "json_input", "system_prompt": "work"
        });
    }
    store
        .persist_definition("sequential", 1, &definition)
        .unwrap();
    let mut run = new_run();
    run.definition_id = "sequential".into();
    let old = WorkflowExecutionAuthority {
        target_artifact_id: "old-artifact".into(),
        daemon_instance_id: "missing-owner".into(),
        generation: 1,
        fencing_token: "old-fence".into(),
    };
    run.execution_authority = Some(old.clone());
    store.create_run(&run).unwrap();
    store.enter_recovery_only(&run.run_id, &old, 20).unwrap();
    let next = WorkflowExecutionAuthority {
        target_artifact_id: "current-artifact".into(),
        daemon_instance_id: "maintenance-owner".into(),
        generation: 2,
        fencing_token: "new-fence".into(),
    };
    let before = store.pending_activations(10).unwrap();
    // Stale authority must leave the barrier intact.
    let mut stale = old.clone();
    stale.fencing_token = "stale".into();
    assert!(
        store
            .recover_offline_continuation(&run.run_id, &stale, &next, 21)
            .is_err()
    );
    assert!(store.is_recovery_only(&run.run_id).unwrap());
    store
        .recover_offline_continuation(&run.run_id, &old, &next, 21)
        .unwrap();
    assert!(!store.is_recovery_only(&run.run_id).unwrap());
    assert_eq!(
        store.run_summary(&run.run_id).unwrap().unwrap().status,
        RunStatus::Paused
    );
    assert_eq!(store.pending_activations(10).unwrap(), before);
    assert!(
        store
            .recover_offline_continuation(&run.run_id, &old, &next, 22)
            .is_err()
    );
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).unwrap();
    assert_eq!(
        store.execution_authority(&run.run_id).unwrap(),
        Some(next.clone())
    );
    assert!(store.resume_run_owned(&run.run_id, 23, &next).unwrap());
    assert!(store.validate_offline_continuation(&run.run_id).is_err());
}

#[test]
fn recovery_completion_is_fenced_idempotent_and_does_not_resume() {
    let (_temp, mut store, run, authority, _) = connected_publication_fixture();
    store
        .enter_recovery_only(&run.run_id, &authority, 20)
        .expect("enter");
    let mut stale = authority.clone();
    stale.generation += 1;
    assert!(store.finish_recovery_only(&run.run_id, &stale, 21).is_err());
    assert!(store.resume_run(&run.run_id, 22).is_err());
    assert!(
        store
            .finish_recovery_only(&run.run_id, &authority, 23)
            .expect("finish")
    );
    assert!(
        !store
            .finish_recovery_only(&run.run_id, &authority, 24)
            .expect("duplicate")
    );
    assert_eq!(
        store
            .run_summary(&run.run_id)
            .expect("summary")
            .expect("run")
            .status,
        RunStatus::Paused
    );
    assert!(
        store
            .resume_run_owned(&run.run_id, 25, &authority)
            .expect("explicit resume")
    );
}

#[test]
fn recovery_only_survives_reopen_and_blocks_resume() {
    let (temp, mut store, run, authority, _) = connected_publication_fixture();
    store
        .enter_recovery_only(&run.run_id, &authority, 20)
        .expect("recovery");
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    assert!(store.resume_run_owned(&run.run_id, 21, &authority).is_err());
    assert_eq!(
        store
            .run_summary(&run.run_id)
            .expect("summary")
            .expect("run")
            .status,
        RunStatus::Paused
    );
    store
        .request_cancellation_owned(&run.run_id, 22, &authority)
        .expect("cancellation still allowed");
}

#[test]
fn missing_runtime_during_handoff_defers_cancellation_until_receipt() {
    for side_effect in [DispatchSideEffect::ReadOnly, DispatchSideEffect::Mutating] {
        let (_temp, mut store, run, authority, _) = connected_publication_fixture();
        let id = activation_identity(&run.run_id, "first", 0);
        let prepared = store
            .prepare_pending_activation(
                &run.run_id,
                "first",
                &id,
                side_effect,
                serde_json::json!({}),
                25,
            )
            .expect("prepare")
            .expect("pending");
        store
            .record_dispatch_handoff(&prepared, Some(&authority))
            .expect("handoff");
        store.request_cancellation(&run.run_id, 26).expect("cancel");
        assert_eq!(
            store
                .settle_orphaned_attempt_cancellation(&prepared.dispatch_identity, 27)
                .expect("defer"),
            RunStatus::Running
        );
        let pending = store
            .active_attempt_cancellations(&run.run_id, 10)
            .expect("retry discovery");
        assert_eq!(pending.len(), 1);
        assert!(pending[0].receipt.is_none());
        store
            .persist_dispatch_receipt(&DispatchReceipt {
                run_id: run.run_id.clone(),
                node_id: "first".into(),
                activation_id: id,
                attempt: prepared.attempt,
                dispatch_identity: prepared.dispatch_identity.clone(),
                receipt: serde_json::json!({"accepted": true}),
                admitted_at_ms: 28,
            })
            .expect("late owner acceptance");
        assert!(
            store
                .active_attempt_cancellations(&run.run_id, 10)
                .expect("retry")[0]
                .receipt
                .is_some()
        );
        store
            .mark_cancellation_signalled(&prepared.dispatch_identity, 29)
            .expect("signal");
        store
            .apply_attempt_observation(
                &prepared.dispatch_identity,
                AttemptObservation::Cancelled,
                30,
            )
            .expect("settle");
        assert_eq!(
            store
                .run_summary(&run.run_id)
                .expect("summary")
                .expect("run")
                .status,
            RunStatus::Cancelled
        );
    }
}

#[test]
fn cancellation_before_receipt_preserves_owner_receipt_without_reopening_attempt() {
    let (_temp, mut store, run, authority, _) = connected_publication_fixture();
    let id = activation_identity(&run.run_id, "first", 0);
    let prepared = store
        .prepare_pending_activation(
            &run.run_id,
            "first",
            &id,
            DispatchSideEffect::Mutating,
            serde_json::json!({}),
            25,
        )
        .expect("prepare")
        .expect("pending");
    store
        .record_dispatch_handoff(&prepared, Some(&authority))
        .expect("handoff");
    store.request_cancellation(&run.run_id, 26).expect("cancel");
    store
        .mark_cancellation_signalled(&prepared.dispatch_identity, 27)
        .expect("signal");
    let mut receipt = DispatchReceipt {
        run_id: run.run_id.clone(),
        node_id: "first".into(),
        activation_id: id,
        attempt: prepared.attempt,
        dispatch_identity: prepared.dispatch_identity.clone(),
        receipt: serde_json::json!({"owner": "accepted"}),
        admitted_at_ms: 28,
    };
    store
        .persist_dispatch_receipt(&receipt)
        .expect("late receipt");
    store
        .persist_dispatch_receipt(&receipt)
        .expect("duplicate receipt");
    let status: String = fixture_scalar_text(
        &store.connection,
        &select("workflow_attempts")
            .columns(&["status"])
            .where_eq("dispatch_identity", prepared.dispatch_identity.as_str()),
        "status",
    )
    .expect("status");
    assert_eq!(status, "cancelling");
    let recorded = fixture_scalar_text(
        &store.connection,
        &select("workflow_attempts")
            .columns(&["receipt_json"])
            .where_eq("dispatch_identity", prepared.dispatch_identity.as_str()),
        "receipt_json",
    )
    .expect("receipt");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&recorded).expect("JSON"),
        receipt.receipt
    );
    receipt.receipt = serde_json::json!({"owner": "conflicting"});
    assert!(store.persist_dispatch_receipt(&receipt).is_err());
    store
        .apply_attempt_observation(
            &prepared.dispatch_identity,
            AttemptObservation::Cancelled,
            29,
        )
        .expect("settled");
    assert!(store.persist_dispatch_receipt(&receipt).is_err());
    assert_eq!(
        store
            .run_summary(&run.run_id)
            .expect("summary")
            .expect("run")
            .status,
        RunStatus::Cancelled
    );
}

fn assert_graph_gate_cancellation(kind: bcode_workflow::NodeKind, answer_first: bool) {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    let mut definition = sequential_definition();
    definition.nodes.get_mut("first").expect("gate").kind = kind;
    store
        .persist_definition("sequential", 1, &definition)
        .expect("definition");
    let mut run = new_run();
    run.definition_id = "sequential".into();
    store.create_run(&run).expect("run");
    block_on(async {
        update("workflow_runs")
            .value("target_artifact_id", "artifact-a")
            .value("coordinator_daemon_instance_id", "daemon-a")
            .value("coordinator_generation", 1)
            .value("coordinator_fencing_token", "token-a")
            .where_eq("run_id", "run-1")
            .execute(&store.connection)
            .await?;
        Ok::<(), switchy_database::DatabaseError>(())
    })
    .expect("owner");
    let authority = store
        .execution_authority(&run.run_id)
        .expect("authority")
        .expect("owner");
    let activation_id = activation_identity(&run.run_id, "first", 0);
    let request = bcode_workflow::WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: run.run_id.clone(),
        mutation_id: "cancel-gate".into(),
        expected_revision: 1,
        edits: vec![bcode_workflow::WorkflowRunGraphEdit::RemoveEdge { edge_id: 0 }],
        reconciliation: vec![bcode_workflow::WorkflowRunGraphReconciliation::Cancel {
            activation_id: activation_id.clone(),
        }],
    };
    assert_eq!(
        store
            .waiting_activations(&run.run_id, 10)
            .expect("waiting")
            .len(),
        1
    );
    store
        .stage_run_graph_edit(&request, &authority, 20)
        .expect("stage");
    if answer_first {
        assert_answer_wins_graph_cancellation(
            &mut store,
            &request,
            &authority,
            kind,
            &activation_id,
        );
        return;
    }
    assert_eq!(
        store
            .publish_retained_leaf_run_graph_edit(&run.run_id, &request.mutation_id, &authority, 21)
            .expect("cancel gate"),
        2
    );
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    assert!(
        store
            .waiting_activations(&run.run_id, 10)
            .expect("waiting")
            .is_empty()
    );
    assert!(
        store
            .provide_input(
                &run.run_id,
                "first",
                &activation_id,
                serde_json::json!(2),
                22
            )
            .is_err()
    );
    assert!(
        store
            .resolve_approval(&run.run_id, "first", &activation_id, true, 22)
            .is_err()
    );
    assert!(
        store
            .validated_outputs(&run.run_id, 10)
            .expect("outputs")
            .is_empty()
    );
    assert!(store.pending_activations(10).expect("pending").is_empty());
}

fn assert_answer_wins_graph_cancellation(
    store: &mut WorkflowStore,
    request: &bcode_workflow::WorkflowRunGraphEditBatch,
    authority: &WorkflowExecutionAuthority,
    kind: bcode_workflow::NodeKind,
    activation_id: &str,
) {
    if kind == bcode_workflow::NodeKind::Input {
        store
            .provide_input(
                &request.run_id,
                "first",
                activation_id,
                serde_json::json!(2),
                21,
            )
            .expect("answer");
    } else {
        store
            .resolve_approval(&request.run_id, "first", activation_id, true, 21)
            .expect("approve");
    }
    assert!(
        store
            .publish_retained_leaf_run_graph_edit(
                &request.run_id,
                &request.mutation_id,
                authority,
                22
            )
            .is_err()
    );
    assert_eq!(
        store
            .run_graph_revision(&request.run_id)
            .expect("unchanged revision"),
        Some(1)
    );
    assert_eq!(
        store
            .validated_outputs(&request.run_id, 10)
            .expect("preserved output")
            .len(),
        1
    );
    assert_eq!(store.pending_activations(10).expect("successor").len(), 1);
}

#[test]
fn retained_connected_successor_requires_binding_and_commits_current_revision() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("sequential", 1, &sequential_definition())
        .expect("definition");
    let mut run = new_run();
    run.definition_id = "sequential".into();
    run.run_id = "sequential-run".into();
    store.create_run(&run).expect("run");
    let first_id = activation_identity("sequential-run", "first", 0);
    // Model a committed connected revision below the still-guarded public publisher.
    block_on(
        update("workflow_run_graphs")
            .value("revision", 2)
            .execute(&store.connection),
    )
    .expect("revision fixture");
    fixture_record_connected_leaf_retention(&store.connection, &first_id);
    let output = ValidatedOutput {
        output_id: "retained-first".into(),
        run_id: "sequential-run".into(),
        node_id: "first".into(),
        activation_id: first_id.clone(),
        schema_id: "u32".into(),
        schema_version: 1,
        value: serde_json::json!(2),
        artifact_reference: None,
        created_at_ms: 20,
    };
    assert!(store.persist_validated_output(&output).is_err());
    assert!(
        store
            .validated_outputs("sequential-run", 10)
            .expect("outputs")
            .is_empty()
    );
    assert_eq!(
        store
            .activations_for_run("sequential-run", 10)
            .expect("activations")
            .len(),
        1
    );
    block_on(
        insert("workflow_retained_edge_bindings")
            .value("run_id", "sequential-run")
            .value("mutation_id", "retained")
            .value("graph_revision", 2)
            .value("edge_id", 0)
            .value("edge_revision", 1)
            .value("source_node_id", "first")
            .value("source_activation_id", first_id.as_str())
            .execute(&store.connection),
    )
    .expect("bind edge");
    assert!(
        store
            .persist_validated_output_with_fault(&output, &FailRetainedSuccessors)
            .is_err()
    );
    assert!(
        store
            .validated_outputs("sequential-run", 10)
            .expect("rolled back outputs")
            .is_empty()
    );
    assert_eq!(
        store
            .activations_for_run("sequential-run", 10)
            .expect("rolled back successors")
            .len(),
        1
    );
    assert_retained_successor_deadline(&mut store, &output);
    let result = store
        .persist_validated_output(&output)
        .expect("retained settlement");
    assert_eq!(result.activated.len(), 1);
    assert_eq!(result.activated[0].input, Some(serde_json::json!(2)));
    let successor_id = &result.activated[0].activation_id;
    assert_eq!(
        store
            .activation_admitted_graph_revision("sequential-run", "second", successor_id)
            .expect("revision"),
        Some(2)
    );
    drop(store);
    assert_retained_successor_completes_after_reopen(
        temp.path(),
        &result.activated[0].activation_id,
    );
}

fn fixture_record_connected_leaf_retention(db: &dyn Database, first_id: &str) {
    block_on(
        insert("workflow_graph_edit_candidates")
            .value("run_id", "sequential-run")
            .value("mutation_id", "retained")
            .value("expected_revision", 1)
            .value("request_json", "{}")
            .value("authority_json", "{}")
            .value("created_at_ms", 1)
            .execute(db),
    )
    .expect("candidate fixture");
    block_on(
        insert("workflow_graph_edit_publications")
            .value("run_id", "sequential-run")
            .value("mutation_id", "retained")
            .value("revision", 2)
            .execute(db),
    )
    .expect("publication fixture");
    block_on(
        insert("workflow_leaf_retentions")
            .value("run_id", "sequential-run")
            .value("node_id", "first")
            .value("activation_id", first_id)
            .value("revision", 2)
            .execute(db),
    )
    .expect("retain");
}

fn assert_retained_successor_completes_after_reopen(path: &std::path::Path, activation_id: &str) {
    let mut store = WorkflowStore::open_in_state_dir(path).expect("reopen");
    let terminal = store
        .persist_validated_output(&ValidatedOutput {
            output_id: "retained-second".into(),
            run_id: "sequential-run".into(),
            node_id: "second".into(),
            activation_id: activation_id.to_owned(),
            schema_id: "u32".into(),
            schema_version: 1,
            value: serde_json::json!(3),
            artifact_reference: None,
            created_at_ms: 21,
        })
        .expect("complete after reopen");
    assert_eq!(terminal.run_status, RunStatus::Completed);
}

#[test]
fn validated_output_atomically_activates_direct_successor_and_completes_run() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("sequential", 1, &sequential_definition())
        .expect("definition");
    let mut run = new_run();
    run.definition_id = "sequential".to_string();
    run.run_id = "sequential-run".to_string();
    store.create_run(&run).expect("run");
    let first_id = activation_identity("sequential-run", "first", 0);
    assert_eq!(
        store
            .activation_admitted_graph_revision("sequential-run", "first", &first_id)
            .expect("entry binding"),
        Some(1)
    );
    let first = store
        .persist_validated_output(&ValidatedOutput {
            output_id: "first-output".to_string(),
            run_id: "sequential-run".to_string(),
            node_id: "first".to_string(),
            activation_id: first_id,
            schema_id: "u32".to_string(),
            schema_version: 1,
            value: serde_json::json!(2),
            artifact_reference: None,
            created_at_ms: 20,
        })
        .expect("first output");
    assert_eq!(first.run_status, RunStatus::Running);
    assert_eq!(first.activated.len(), 1);
    assert_eq!(first.activated[0].node_id, "second");
    assert_eq!(first.activated[0].input, Some(serde_json::json!(2)));
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen successor");
    let payload = fixture_scalar_text(
        &store.connection,
        &select("workflow_events")
            .columns(&["payload_json"])
            .where_eq("run_id", "sequential-run")
            .where_eq("event_type", "activation_created")
            .sort("event_seq", switchy_database::query::SortDirection::Desc)
            .limit(1),
        "payload_json",
    )
    .expect("successor event");
    let payload: serde_json::Value = serde_json::from_str(&payload).expect("event JSON");
    assert_eq!(
        payload["input_binding"],
        serde_json::json!({
            "version": 1, "graph_revision": 1, "edge_id": 0, "edge_revision": 1,
            "source_activation_id": activation_identity("sequential-run", "first", 0),
            "source_output_id": "first-output",
        })
    );
    assert_eq!(
        store
            .activation_admitted_graph_revision(
                "sequential-run",
                "second",
                &first.activated[0].activation_id
            )
            .expect("successor binding"),
        Some(1)
    );
    let second = store
        .persist_validated_output(&ValidatedOutput {
            output_id: "second-output".to_string(),
            run_id: "sequential-run".to_string(),
            node_id: "second".to_string(),
            activation_id: activation_identity("sequential-run", "second", 0),
            schema_id: "u32".to_string(),
            schema_version: 1,
            value: serde_json::json!(3),
            artifact_reference: None,
            created_at_ms: 21,
        })
        .expect("second output");
    assert_eq!(second.run_status, RunStatus::Completed);
    assert!(second.activated.is_empty());
    assert_eq!(
        store
            .run_summary("sequential-run")
            .expect("summary")
            .expect("run")
            .status,
        RunStatus::Completed
    );
}

#[test]
fn paused_completion_materializes_successor_without_admitting_execution() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("sequential", 1, &sequential_definition())
        .unwrap();
    let mut run = new_run();
    run.definition_id = "sequential".into();
    store.create_run(&run).unwrap();
    store.pause_run(&run.run_id, 15).unwrap();
    let result = store
        .persist_validated_output(&ValidatedOutput {
            output_id: "paused-first".into(),
            run_id: run.run_id.clone(),
            node_id: "first".into(),
            activation_id: activation_identity(&run.run_id, "first", 0),
            schema_id: "u32".into(),
            schema_version: 1,
            value: serde_json::json!(2),
            artifact_reference: None,
            created_at_ms: 20,
        })
        .unwrap();
    assert_eq!(result.run_status, RunStatus::Paused);
    assert_eq!(result.activated.len(), 1);
    assert!(store.pending_activations(10).unwrap().is_empty());
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).unwrap();
    assert_eq!(
        store.run_summary(&run.run_id).unwrap().unwrap().status,
        RunStatus::Paused
    );
    store.resume_run(&run.run_id, 21).unwrap();
    let pending = store.pending_activations(10).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].node_id, "second");
    store.pause_run(&run.run_id, 22).unwrap();
    let result = store
        .persist_validated_output(&ValidatedOutput {
            output_id: "paused-second".into(),
            run_id: run.run_id.clone(),
            node_id: "second".into(),
            activation_id: activation_identity(&run.run_id, "second", 0),
            schema_id: "u32".into(),
            schema_version: 1,
            value: serde_json::json!(3),
            artifact_reference: None,
            created_at_ms: 23,
        })
        .unwrap();
    assert_eq!(result.run_status, RunStatus::Completed);
    assert_eq!(
        store.run_summary(&run.run_id).unwrap().unwrap().status,
        RunStatus::Completed
    );
}

#[test]
fn join_activates_once_only_after_all_direct_dependencies_complete() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    let definition = parallel_join_definition();
    store
        .persist_definition("parallel", 1, &definition)
        .expect("definition");
    let mut run = new_run();
    run.run_id = "parallel-run".to_string();
    run.definition_id = "parallel".to_string();
    store.create_run(&run).expect("run");
    for (node_id, output_id, timestamp) in
        [("left", "left-output", 20), ("right", "right-output", 21)]
    {
        let result = store
            .persist_validated_output(&ValidatedOutput {
                output_id: output_id.to_string(),
                run_id: "parallel-run".to_string(),
                node_id: node_id.to_string(),
                activation_id: activation_identity("parallel-run", node_id, 0),
                schema_id: "u32".to_string(),
                schema_version: 1,
                value: serde_json::json!(2),
                artifact_reference: None,
                created_at_ms: timestamp,
            })
            .expect("branch output");
        if node_id == "left" {
            assert!(result.activated.is_empty());
        } else {
            assert_eq!(result.activated.len(), 1);
            assert_eq!(result.activated[0].node_id, "join");
            assert_eq!(result.activated[0].input, Some(serde_json::json!([2, 2])));
        }
    }
    let join_count: u64 = fixture_scalar_unsigned(
        &store.connection,
        &select("workflow_activations")
            .project_as(switchy_database::query::count_all(), "count")
            .where_eq("run_id", "parallel-run")
            .where_eq("node_id", "join"),
        "count",
    )
    .expect("join count");
    assert_eq!(join_count, 1);
}

fn outcome_collection_definition() -> bcode_workflow::WorkflowDefinition {
    let mut definition = parallel_join_definition_with_policy(
        bcode_workflow::ParallelFailurePolicy::CollectOutcomes,
    );
    let schemas = ["left", "right"]
        .into_iter()
        .map(|id| {
            (
                id.to_string(),
                bcode_workflow::workflow_member_outcome_schema(&definition.nodes[id].output)
                    .expect("outcome schema"),
            )
        })
        .collect();
    let schema = bcode_workflow::named_result_schema("outcomes".into(), &schemas).expect("schema");
    let join = definition.nodes.get_mut("join").expect("join");
    join.input = schema.clone();
    join.output = schema.clone();
    definition.output = schema;
    definition
}

#[test]
#[allow(clippy::too_many_lines)]
fn outcome_collection_retains_success_and_terminal_failure_across_restart() {
    struct TerminalObserver(bool);
    impl AttemptStatusObserver for TerminalObserver {
        fn observe(
            &self,
            _: &AttemptReconciliationRequest,
        ) -> Result<AttemptObservation, WorkflowStoreError> {
            Ok(if self.0 {
                AttemptObservation::Cancelled
            } else {
                AttemptObservation::Failed {
                    message: "worker failed".into(),
                }
            })
        }
    }
    for (failed_first, cancelled) in [(false, false), (true, false), (false, true), (true, true)] {
        let observer = TerminalObserver(cancelled);
        let temp = tempfile::tempdir().expect("temp");
        let definition = outcome_collection_definition();
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("outcomes", 1, &definition)
            .expect("definition");
        let mut run = new_run();
        run.definition_id = "outcomes".into();
        run.input = Some(serde_json::json!(1));
        store.create_run(&run).expect("run");
        let attempt = store
            .prepare_pending_activation(
                &run.run_id,
                "left",
                &activation_identity(&run.run_id, "left", 0),
                DispatchSideEffect::ReadOnly,
                serde_json::json!({}),
                2,
            )
            .expect("prepare")
            .expect("attempt");
        store
            .persist_dispatch_receipt(&DispatchReceipt {
                run_id: run.run_id.clone(),
                node_id: "left".into(),
                activation_id: attempt.activation.activation_id,
                attempt: attempt.attempt,
                dispatch_identity: attempt.dispatch_identity,
                receipt: serde_json::json!({}),
                admitted_at_ms: 3,
            })
            .expect("receipt");
        if failed_first {
            store
                .reconcile_receipt_backed_attempts(&observer, 10, 4)
                .expect("terminal failure");
        }
        store
            .persist_validated_output(&ValidatedOutput {
                output_id: "right-output".into(),
                run_id: run.run_id.clone(),
                node_id: "right".into(),
                activation_id: activation_identity(&run.run_id, "right", 0),
                schema_id: definition.nodes["right"].output.type_name.clone(),
                schema_version: 1,
                value: serde_json::json!(2),
                artifact_reference: None,
                created_at_ms: 5,
            })
            .expect("output");
        if !failed_first {
            store
                .reconcile_receipt_backed_attempts(&observer, 10, 6)
                .expect("terminal failure");
        }
        drop(store);
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
        assert_eq!(
            store
                .run_summary(&run.run_id)
                .expect("summary")
                .expect("run")
                .status,
            RunStatus::Running
        );
        let pending = store.pending_activations(10).expect("pending");
        let join = pending
            .iter()
            .find(|item| item.node_id == "join")
            .expect("outcome controller");
        assert_eq!(
            join.input,
            Some(
                serde_json::json!({"left":{"status":if cancelled { "cancelled" } else { "failed" }},"right":{"status":"completed","value":2}})
            )
        );
        store
            .settle_pending_control_nodes(&run.run_id, 10, 7)
            .expect("settle");
        let outputs = store.output_summaries(&run.run_id, 10).expect("outputs");
        assert_eq!(outputs.len(), 2);
        assert!(outputs.iter().all(|output| output.node_id != "left"));
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn wait_all_parallel_failure_persists_only_after_all_members_settle() {
    struct FailedObserver;
    impl AttemptStatusObserver for FailedObserver {
        fn observe(
            &self,
            _request: &AttemptReconciliationRequest,
        ) -> Result<AttemptObservation, WorkflowStoreError> {
            Ok(AttemptObservation::Failed {
                message: "branch failed".to_string(),
            })
        }
    }

    let temp = tempfile::tempdir().expect("temp");
    let definition = parallel_join_definition();
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("parallel-failure", 1, &definition)
        .expect("definition");
    store
        .create_run(&NewWorkflowRun {
            run_id: "parallel-failure-run".to_string(),
            definition_id: "parallel-failure".to_string(),
            definition_version: 1,
            workspace_snapshot: "snapshot".to_string(),
            parent_session_id: None,
            parent_session_generation: None,
            binding: None,
            authored_provenance: None,
            input: Some(serde_json::json!(1)),
            execution_authority: None,
            created_at_ms: 1,
            authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                version: 1,
                provider_id: "test-policy".to_string(),
                profile_id: "build".to_string(),
                policy_digest_sha256: "a".repeat(64),
            },
            authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
            limits: WorkflowRunLimits::default(),
        })
        .expect("run");
    let left = store
        .prepare_pending_activation(
            "parallel-failure-run",
            "left",
            &activation_identity("parallel-failure-run", "left", 0),
            DispatchSideEffect::ReadOnly,
            serde_json::json!({"branch": "left"}),
            2,
        )
        .expect("prepare")
        .expect("left");
    store
        .persist_dispatch_receipt(&DispatchReceipt {
            run_id: "parallel-failure-run".to_string(),
            node_id: "left".to_string(),
            activation_id: left.activation.activation_id,
            attempt: left.attempt,
            dispatch_identity: left.dispatch_identity,
            receipt: serde_json::json!({"accepted": true}),
            admitted_at_ms: 3,
        })
        .expect("receipt");
    store
        .reconcile_receipt_backed_attempts(&FailedObserver, 10, 4)
        .expect("left failure");
    assert_eq!(
        store
            .run_summary("parallel-failure-run")
            .expect("run")
            .expect("run")
            .status,
        RunStatus::Running
    );
    drop(store);
    let mut reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    assert!(
        reopened
            .decision("parallel-failure-run:join:0:parallel-wait-all")
            .expect("decision before settlement")
            .is_none()
    );
    assert_eq!(
        reopened
            .attempt_history("parallel-failure-run", None, 10)
            .expect("attempts")
            .len(),
        1
    );
    reopened
        .persist_validated_output(&ValidatedOutput {
            output_id: "right-output".to_string(),
            run_id: "parallel-failure-run".to_string(),
            node_id: "right".to_string(),
            activation_id: activation_identity("parallel-failure-run", "right", 0),
            schema_id: definition.nodes["right"].output.type_name.clone(),
            schema_version: 1,
            value: serde_json::json!(2),
            artifact_reference: None,
            created_at_ms: 5,
        })
        .expect("right output");
    let decision = reopened
        .decision("parallel-failure-run:join:0:parallel-wait-all")
        .expect("decision")
        .expect("settlement");
    assert_eq!(decision.value["policy"], "wait_all");
    assert_eq!(decision.value["outcome"], "failed");
    assert_eq!(
        reopened
            .run_summary("parallel-failure-run")
            .expect("run")
            .expect("run")
            .status,
        RunStatus::Failed
    );
}

struct CancelledParallelMemberObserver;
impl AttemptStatusObserver for CancelledParallelMemberObserver {
    fn observe(
        &self,
        _request: &AttemptReconciliationRequest,
    ) -> Result<AttemptObservation, WorkflowStoreError> {
        Ok(AttemptObservation::Cancelled)
    }
}

#[test]
fn cancelled_parallel_member_settles_wait_all_in_either_order() {
    for cancelled_first in [false, true] {
        let temp = tempfile::tempdir().expect("temp");
        let definition = parallel_join_definition();
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("cancelled-member", 1, &definition)
            .expect("definition");
        let mut run = new_run();
        run.definition_id = "cancelled-member".into();
        run.input = Some(serde_json::json!(1));
        store.create_run(&run).expect("run");
        let attempt = store
            .prepare_pending_activation(
                &run.run_id,
                "left",
                &activation_identity(&run.run_id, "left", 0),
                DispatchSideEffect::ReadOnly,
                serde_json::json!({}),
                2,
            )
            .expect("prepare")
            .expect("attempt");
        store
            .persist_dispatch_receipt(&DispatchReceipt {
                run_id: run.run_id.clone(),
                node_id: "left".into(),
                activation_id: attempt.activation.activation_id,
                attempt: attempt.attempt,
                dispatch_identity: attempt.dispatch_identity,
                receipt: serde_json::json!({"accepted":true}),
                admitted_at_ms: 3,
            })
            .expect("receipt");
        if cancelled_first {
            store
                .reconcile_receipt_backed_attempts(&CancelledParallelMemberObserver, 10, 4)
                .expect("cancel");
            assert_eq!(
                store
                    .run_summary(&run.run_id)
                    .expect("summary")
                    .expect("run")
                    .status,
                RunStatus::Running
            );
        }
        store
            .persist_validated_output(&ValidatedOutput {
                output_id: "right-output".into(),
                run_id: run.run_id.clone(),
                node_id: "right".into(),
                activation_id: activation_identity(&run.run_id, "right", 0),
                schema_id: definition.nodes["right"].output.type_name.clone(),
                schema_version: 1,
                value: serde_json::json!(2),
                artifact_reference: None,
                created_at_ms: 5,
            })
            .expect("sibling output");
        if !cancelled_first {
            store
                .reconcile_receipt_backed_attempts(&CancelledParallelMemberObserver, 10, 6)
                .expect("cancel");
        }
        drop(store);
        let reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
        assert_eq!(
            reopened
                .run_summary(&run.run_id)
                .expect("summary")
                .expect("run")
                .status,
            RunStatus::Failed
        );
        let join_count = fixture_scalar_unsigned(
            &reopened.connection,
            &select("workflow_activations")
                .project_as(switchy_database::query::count_all(), "count")
                .where_eq("run_id", run.run_id.as_str())
                .where_eq("node_id", "join"),
            "count",
        )
        .expect("join count");
        assert_eq!(join_count, 0);
        assert_eq!(
            reopened.output_summaries(&run.run_id, 10).expect("outputs")[0].output_id,
            "right-output"
        );
        let decision = reopened
            .decision(&format!("{}:join:0:parallel-wait-all", run.run_id))
            .expect("decision")
            .expect("settled");
        assert_eq!(decision.value["outcome"], "failed");
    }
}

#[test]
fn parallel_missing_result_preserves_successful_sibling_and_failure_after_reopen() {
    for policy in [
        bcode_workflow::ParallelFailurePolicy::WaitAll,
        bcode_workflow::ParallelFailurePolicy::FailFast,
    ] {
        let temp = tempfile::tempdir().expect("temp");
        let definition = parallel_join_definition_with_policy(policy);
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("missing-result", 1, &definition)
            .expect("definition");
        let mut run = new_run();
        run.definition_id = "missing-result".into();
        run.input = Some(serde_json::json!(1));
        store.create_run(&run).expect("run");
        block_on(
            update("workflow_activations")
                .value("status", "skipped")
                .where_eq("run_id", run.run_id.as_str())
                .where_eq("node_id", "left")
                .execute(&store.connection),
        )
        .expect("skip left without output");
        let result = store
            .persist_validated_output(&ValidatedOutput {
                output_id: "right-output".into(),
                run_id: run.run_id.clone(),
                node_id: "right".into(),
                activation_id: activation_identity(&run.run_id, "right", 0),
                schema_id: definition.nodes["right"].output.type_name.clone(),
                schema_version: 1,
                value: serde_json::json!(2),
                artifact_reference: None,
                created_at_ms: 5,
            })
            .expect("retain successful sibling despite missing result");
        assert_eq!(result.run_status, RunStatus::Failed);
        assert!(result.activated.is_empty());
        drop(store);
        let reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
        assert_eq!(
            reopened
                .run_summary(&run.run_id)
                .expect("summary")
                .expect("run")
                .status,
            RunStatus::Failed
        );
        let outputs = reopened.output_summaries(&run.run_id, 10).expect("outputs");
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].output_id, "right-output");
        let suffix = match policy {
            bcode_workflow::ParallelFailurePolicy::CollectOutcomes => {
                unreachable!("success-only test")
            }
            bcode_workflow::ParallelFailurePolicy::WaitAll => "parallel-wait-all",
            bcode_workflow::ParallelFailurePolicy::FailFast => "parallel-fail-fast",
        };
        let decision = reopened
            .decision(&format!("{}:join:0:{suffix}", run.run_id))
            .expect("decision")
            .expect("failed join");
        assert_eq!(decision.value["outcome"], "failed");
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn fail_fast_parallel_failure_persists_sibling_cancellation_before_signalling() {
    let temp = tempfile::tempdir().expect("temp");
    let definition =
        parallel_join_definition_with_policy(bcode_workflow::ParallelFailurePolicy::FailFast);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("parallel-fail-fast", 1, &definition)
        .expect("definition");
    let run_id = "parallel-fail-fast-run";
    store
        .create_run(&NewWorkflowRun {
            run_id: run_id.to_string(),
            definition_id: "parallel-fail-fast".to_string(),
            definition_version: 1,
            workspace_snapshot: "snapshot".to_string(),
            parent_session_id: None,
            parent_session_generation: None,
            binding: None,
            authored_provenance: None,
            input: Some(serde_json::json!(1)),
            execution_authority: None,
            created_at_ms: 1,
            authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                version: 1,
                provider_id: "test-policy".to_string(),
                profile_id: "build".to_string(),
                policy_digest_sha256: "a".repeat(64),
            },
            authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
            limits: WorkflowRunLimits {
                concurrency_cap: Some(2),
                ..WorkflowRunLimits::default()
            },
        })
        .expect("run");
    let prepare = |store: &mut WorkflowStore, node_id: &str, now| {
        let prepared = store
            .prepare_pending_activation(
                run_id,
                node_id,
                &activation_identity(run_id, node_id, 0),
                DispatchSideEffect::ReadOnly,
                serde_json::json!({"branch": node_id}),
                now,
            )
            .expect("prepare")
            .expect("prepared");
        store
            .persist_dispatch_receipt(&DispatchReceipt {
                run_id: run_id.to_string(),
                node_id: node_id.to_string(),
                activation_id: prepared.activation.activation_id,
                attempt: prepared.attempt,
                dispatch_identity: prepared.dispatch_identity.clone(),
                receipt: serde_json::json!({"accepted": true}),
                admitted_at_ms: now + 1,
            })
            .expect("receipt");
        prepared.dispatch_identity
    };
    let left_identity = prepare(&mut store, "left", 2);
    let right_identity = prepare(&mut store, "right", 4);

    let summary = store
        .apply_attempt_observation(
            &left_identity,
            AttemptObservation::Failed {
                message: "left failed".to_string(),
            },
            6,
        )
        .expect("fail-fast settlement");
    assert_eq!(summary.failed, vec![left_identity]);
    assert_eq!(summary.sibling_cancellations.len(), 1);
    assert_eq!(
        summary.sibling_cancellations[0].dispatch_identity,
        right_identity
    );
    assert_eq!(
        store.run_summary(run_id).expect("run").expect("run").status,
        RunStatus::Failed
    );
    assert_eq!(
        store
            .attempt_history(run_id, None, 10)
            .expect("attempts")
            .into_iter()
            .find(|attempt| attempt.dispatch_identity == right_identity)
            .expect("right attempt")
            .status,
        "sibling_cancelling"
    );
    let decision = store
        .decision(&format!("{run_id}:join:0:parallel-fail-fast"))
        .expect("decision")
        .expect("decision");
    assert_eq!(decision.value["policy"], "fail_fast");
    assert_eq!(
        decision.value["sibling_cancellations"],
        serde_json::json!([right_identity])
    );
    assert!(
        store
            .event_history(run_id, None, 100)
            .expect("events")
            .iter()
            .any(|event| event.event_type == "parallel_sibling_cancellation_requested")
    );
    assert_eq!(
        store
            .pending_sibling_cancellations(10)
            .expect("pending siblings")
            .into_iter()
            .map(|attempt| attempt.dispatch_identity)
            .collect::<Vec<_>>(),
        vec![right_identity.clone()]
    );
    drop(store);
    let mut reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    assert_eq!(
        reopened
            .pending_sibling_cancellations(10)
            .expect("pending siblings after reopen")[0]
            .dispatch_identity,
        right_identity
    );
    assert!(
        reopened
            .mark_sibling_cancellation_signalled(&right_identity, 7)
            .expect("mark signalled")
    );
    assert!(
        reopened
            .pending_sibling_cancellations(10)
            .expect("settled pending siblings")
            .is_empty()
    );
    assert_eq!(
        reopened
            .attempt_history(run_id, None, 10)
            .expect("attempts")
            .into_iter()
            .find(|attempt| attempt.dispatch_identity == right_identity)
            .expect("right attempt")
            .status,
        "cancelling"
    );
    assert!(
        reopened
            .apply_attempt_observation(&right_identity, AttemptObservation::Running, 8)
            .expect("late sibling observation")
            .running
            .contains(&right_identity)
    );
    assert!(
        reopened
            .apply_attempt_observation(&right_identity, AttemptObservation::Cancelled, 9)
            .expect("sibling owner stopped")
            .cancelled
            .contains(&right_identity)
    );
    assert_eq!(
        reopened
            .attempt_history(run_id, None, 10)
            .expect("attempts")
            .into_iter()
            .find(|attempt| attempt.dispatch_identity == right_identity)
            .expect("right attempt")
            .status,
        "cancelled"
    );
    assert_eq!(
        reopened
            .decision(&format!("{run_id}:join:0:parallel-fail-fast"))
            .expect("decision")
            .expect("decision")
            .value["sibling_cancellations"],
        serde_json::json!([right_identity])
    );
}

#[test]
fn transformed_parallel_join_is_deterministic_across_store_restart() {
    let temp = tempfile::tempdir().expect("temp");
    let definition = parallel_join_transform_definition();
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("parallel-transform", 1, &definition)
        .expect("definition");
    let mut run = new_run();
    run.run_id = "parallel-transform-run".to_string();
    run.definition_id = "parallel-transform".to_string();
    store.create_run(&run).expect("run");
    store
        .persist_validated_output(&ValidatedOutput {
            output_id: "right-output".to_string(),
            run_id: run.run_id.clone(),
            node_id: "right".to_string(),
            activation_id: activation_identity(&run.run_id, "right", 0),
            schema_id: "u32".to_string(),
            schema_version: 1,
            value: serde_json::json!(20),
            artifact_reference: None,
            created_at_ms: 20,
        })
        .expect("first member");
    drop(store);

    let mut reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    let result = reopened
        .persist_validated_output(&ValidatedOutput {
            output_id: "left-output".to_string(),
            run_id: run.run_id.clone(),
            node_id: "left".to_string(),
            activation_id: activation_identity(&run.run_id, "left", 0),
            schema_id: "u32".to_string(),
            schema_version: 1,
            value: serde_json::json!(10),
            artifact_reference: None,
            created_at_ms: 21,
        })
        .expect("second member");
    assert_eq!(result.activated.len(), 1);
    assert_eq!(result.activated[0].node_id, "join");
    assert_eq!(result.activated[0].input, Some(serde_json::json!([10, 20])));
}

#[test]
fn parallel_failure_rejects_missing_sibling_before_writes() {
    assert_parallel_settlement_damage(
        |db, run_id| {
            block_on(
                delete("workflow_run_graph_nodes")
                    .where_eq("run_id", run_id)
                    .where_eq("node_id", "right")
                    .execute(db),
            )
            .expect("damage fixture");
        },
        "missing member: right",
    );
}
#[test]
fn parallel_failure_rejects_disconnected_sibling_before_writes() {
    assert_parallel_settlement_damage(
        |db, run_id| {
            block_on(
                delete("workflow_run_graph_edges")
                    .where_eq("run_id", run_id)
                    .where_eq("source_node_id", "right")
                    .execute(db),
            )
            .expect("damage fixture");
        },
        "must exist and have a direct edge",
    );
}
#[test]
fn parallel_failure_rejects_unsupported_member_status_before_writes() {
    assert_parallel_settlement_damage(
        |db, run_id| {
            block_on(
                update("workflow_activations")
                    .value("status", "future_status")
                    .where_eq("run_id", run_id)
                    .where_eq("node_id", "right")
                    .execute(db),
            )
            .expect("damage fixture");
        },
        "unsupported activation status",
    );
}
#[test]
fn parallel_failure_rejects_ambiguous_member_activations_before_writes() {
    assert_parallel_settlement_damage(
        |db, run_id| {
            let rows = fixture_rows(
                db,
                &select("workflow_activations")
                    .where_eq("run_id", run_id)
                    .where_eq("node_id", "right"),
            );
            for row in rows {
                block_on(
                    insert("workflow_activations")
                        .value("run_id", fixture_text(&row, "run_id"))
                        .value("node_id", fixture_text(&row, "node_id"))
                        .value("activation_id", "duplicate")
                        .value(
                            "dependency_generation",
                            fixture_unsigned(&row, "dependency_generation"),
                        )
                        .value("status", fixture_text(&row, "status"))
                        .value("created_at_ms", fixture_unsigned(&row, "created_at_ms"))
                        .execute(db),
                )
                .expect("damage fixture");
            }
        },
        "ambiguous activations",
    );
}
#[test]
fn parallel_failure_rejects_nontext_and_oversized_status_before_writes() {
    let damages: Vec<Box<dyn Expression>> = vec![
        cast_blob("pending").into(),
        zero_blob(1_048_576).into(),
        cast_text(zero_blob(1_048_576)).into(),
    ];
    for value in damages {
        assert_parallel_settlement_damage(
            |db, run_id| {
                block_on(
                    db.exec_update_count(
                        &(update("workflow_activations")
                            .value("status", value)
                            .where_eq("run_id", run_id)
                            .where_eq("node_id", "right")),
                    ),
                )
                .expect("damage fixture");
            },
            "unsupported activation status",
        );
    }
}

fn assert_parallel_settlement_damage(damage: impl FnOnce(&dyn Database, &str), expected: &str) {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    let mut definition = parallel_join_definition();
    definition
        .nodes
        .get_mut("join")
        .expect("join")
        .configuration["failure_policy"] = serde_json::json!("fail_fast");
    store
        .persist_definition("missing-sibling", 1, &definition)
        .expect("definition");
    let mut run = new_run();
    run.definition_id = "missing-sibling".to_string();
    store.create_run(&run).expect("run");
    damage(&store.connection, &run.run_id);
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    let db: &dyn Database = transaction.as_ref();
    let before = fixture_changes(db);
    let error = settle_parallel_failure(
        db,
        &run.run_id,
        "left",
        &activation_identity(&run.run_id, "left", 0),
        true,
        20,
    )
    .expect_err("missing member");
    assert!(error.to_string().contains(expected));
    assert_eq!(fixture_changes(db), before);
}

#[test]
fn fail_fast_success_does_not_cancel_unfinished_sibling() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    let mut definition = parallel_join_transform_definition();
    definition
        .nodes
        .get_mut("join")
        .expect("join")
        .configuration["failure_policy"] = serde_json::json!("fail_fast");
    store
        .persist_definition("successful-fail-fast", 1, &definition)
        .expect("definition");
    let mut run = new_run();
    run.definition_id = "successful-fail-fast".to_string();
    store.create_run(&run).expect("run");
    let output = ValidatedOutput {
        output_id: "left-output".to_string(),
        run_id: run.run_id.clone(),
        node_id: "left".to_string(),
        activation_id: activation_identity(&run.run_id, "left", 0),
        schema_id: "u32".to_string(),
        schema_version: 1,
        value: serde_json::json!(10),
        artifact_reference: None,
        created_at_ms: 20,
    };
    store
        .persist_validated_output(&output)
        .expect("successful member");
    let status: String = fixture_scalar_text(
        &store.connection,
        &select("workflow_activations")
            .columns(&["status"])
            .where_eq("run_id", run.run_id.as_str())
            .where_eq("node_id", "right"),
        "status",
    )
    .expect("sibling status");
    assert_eq!(status, "pending");
    let result = store
        .persist_validated_output(&ValidatedOutput {
            output_id: "right-output".to_string(),
            node_id: "right".to_string(),
            activation_id: activation_identity(&run.run_id, "right", 0),
            value: serde_json::json!(20),
            ..output
        })
        .expect("sibling can finish");
    assert_eq!(result.activated.len(), 1);
    assert_eq!(result.activated[0].input, Some(serde_json::json!([10, 20])));
}

#[test]
fn parallel_failure_uses_exact_activation_generation() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("generations", 1, &parallel_join_definition())
        .expect("definition");
    let mut run = new_run();
    run.definition_id = "generations".to_string();
    store.create_run(&run).expect("run");
    block_on(
        update("workflow_activations")
            .value("status", "completed")
            .where_eq("run_id", run.run_id.as_str())
            .execute(&store.connection),
    )
    .expect("old generation");
    for node_id in ["left", "right"] {
        store
            .create_activation(&NewActivation {
                activation_id: activation_identity(&run.run_id, node_id, 1),
                run_id: run.run_id.clone(),
                node_id: node_id.to_string(),
                dependency_generation: 1,
                input: Some(serde_json::json!(1)),
                created_at_ms: 10,
            })
            .expect("next generation");
    }
    block_on(
        update("workflow_activations")
            .value("status", "completed")
            .where_eq("run_id", run.run_id.as_str())
            .where_eq("dependency_generation", 1)
            .where_not_eq("node_id", "left")
            .execute(&store.connection),
    )
    .expect("new generation sibling outcomes");
    block_on(
        update("workflow_activations")
            .value("status", "failed")
            .where_eq("run_id", run.run_id.as_str())
            .where_eq("dependency_generation", 1)
            .where_eq("node_id", "left")
            .execute(&store.connection),
    )
    .expect("new generation left outcome");
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    let result = settle_parallel_failure(
        transaction.as_ref(),
        &run.run_id,
        "left",
        &activation_identity(&run.run_id, "left", 1),
        true,
        20,
    )
    .expect("settlement")
    .expect("parallel join");
    assert!(
        result.run_failed,
        "must not read successful generation zero"
    );
}

#[test]
fn revised_parallel_failure_requires_current_sibling_admission() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("revised-parallel", 1, &parallel_join_definition())
        .expect("definition");
    let mut run = new_run();
    run.definition_id = "revised-parallel".to_string();
    store.create_run(&run).expect("run");
    block_on(
        update("workflow_run_graphs")
            .value("revision", 2)
            .execute(&store.connection),
    )
    .expect("revision fixture");
    block_on(
        update("workflow_activation_graph_bindings")
            .value("graph_revision", 2)
            .where_eq("node_id", "left")
            .execute(&store.connection),
    )
    .expect("left binding fixture");
    block_on(
        update("workflow_activations")
            .value("status", "completed")
            .where_not_eq("node_id", "left")
            .execute(&store.connection),
    )
    .expect("sibling outcomes");
    block_on(
        update("workflow_activations")
            .value("status", "failed")
            .where_eq("node_id", "left")
            .execute(&store.connection),
    )
    .expect("left outcome");
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    let db: &dyn Database = transaction.as_ref();
    let activation = activation_identity(&run.run_id, "left", 0);
    let error = settle_parallel_failure(db, &run.run_id, "left", &activation, true, 20)
        .expect_err("stale sibling");
    assert!(
        error
            .to_string()
            .contains("parallel member requires exact retention and an unchanged join")
    );
    block_on(
        update("workflow_activation_graph_bindings")
            .value("graph_revision", 2)
            .execute(db),
    )
    .expect("bind siblings");
    let result = settle_parallel_failure(db, &run.run_id, "left", &activation, true, 20)
        .expect("settle")
        .expect("join");
    assert!(result.run_failed);
    database::commit(transaction).expect("commit");
    drop(store);
    let reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    assert!(
        reopened
            .decision(&format!("{}:join:0:parallel-wait-all", run.run_id))
            .expect("decision")
            .is_some()
    );
}

#[test]
fn retained_parallel_failure_settles_without_rebinding_admissions() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("retained-parallel", 1, &parallel_join_definition())
        .expect("definition");
    let mut run = new_run();
    run.definition_id = "retained-parallel".into();
    store.create_run(&run).expect("run");
    block_on(
        update("workflow_run_graphs")
            .value("revision", 2)
            .execute(&store.connection),
    )
    .expect("retained revision fixture");
    block_on(
        update("workflow_activations")
            .value("status", "completed")
            .where_not_eq("node_id", "left")
            .execute(&store.connection),
    )
    .expect("sibling outcomes");
    block_on(
        update("workflow_activations")
            .value("status", "failed")
            .where_eq("node_id", "left")
            .execute(&store.connection),
    )
    .expect("left outcome");
    fixture_record_retained_parallel_bindings(&store.connection);
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    let result = settle_parallel_failure(
        transaction.as_ref(),
        &run.run_id,
        "left",
        &activation_identity(&run.run_id, "left", 0),
        true,
        20,
    )
    .expect("retained settlement")
    .expect("join");
    assert!(result.run_failed);
    database::commit(transaction).expect("commit");
    drop(store);
    let store = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    assert!(
        store
            .decision("run-1:join:0:parallel-wait-all")
            .expect("decision")
            .is_some()
    );
    for member in ["left", "right"] {
        assert_eq!(
            store
                .activation_admitted_graph_revision(
                    &run.run_id,
                    member,
                    &activation_identity(&run.run_id, member, 0)
                )
                .expect("historical admission"),
            Some(1)
        );
    }
}

fn fixture_record_retained_parallel_bindings(db: &dyn Database) {
    block_on(
        insert("workflow_graph_edit_candidates")
            .value("run_id", "run-1")
            .value("mutation_id", "retained-parallel")
            .value("expected_revision", 1)
            .value("request_json", "{}")
            .value("authority_json", "{}")
            .value("created_at_ms", 1)
            .execute(db),
    )
    .expect("retention candidate");
    block_on(
        insert("workflow_graph_edit_publications")
            .value("run_id", "run-1")
            .value("mutation_id", "retained-parallel")
            .value("revision", 2)
            .execute(db),
    )
    .expect("retention publication");
    let activations = fixture_rows(db, &select("workflow_activations"));
    for activation in &activations {
        block_on(
            insert("workflow_leaf_retentions")
                .value("run_id", fixture_text(activation, "run_id"))
                .value("node_id", fixture_text(activation, "node_id"))
                .value("activation_id", fixture_text(activation, "activation_id"))
                .value("revision", 2)
                .execute(db),
        )
        .expect("leaf retention");
    }
    let edges = fixture_rows(
        db,
        &select("workflow_run_graph_edges").where_eq("target_node_id", "join"),
    );
    for edge in &edges {
        for activation in &activations {
            if fixture_text(activation, "run_id") == fixture_text(edge, "run_id")
                && fixture_text(activation, "node_id") == fixture_text(edge, "source_node_id")
            {
                block_on(
                    insert("workflow_retained_edge_bindings")
                        .value("run_id", fixture_text(edge, "run_id"))
                        .value("mutation_id", "retained-parallel")
                        .value("graph_revision", 2)
                        .value("edge_id", fixture_unsigned(edge, "edge_id"))
                        .value("edge_revision", fixture_unsigned(edge, "revision"))
                        .value("source_node_id", fixture_text(activation, "node_id"))
                        .value(
                            "source_activation_id",
                            fixture_text(activation, "activation_id"),
                        )
                        .execute(db),
                )
                .expect("retained edge binding");
            }
        }
    }
}

#[test]
fn parallel_failure_rejects_duplicate_sibling_members_without_writes() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    let mut definition = parallel_join_definition();
    definition
        .nodes
        .get_mut("join")
        .expect("join")
        .configuration["right_exits"] = serde_json::json!(["right", "right"]);
    store
        .persist_definition("duplicate-sibling", 1, &definition)
        .expect("definition");
    let mut run = new_run();
    run.definition_id = "duplicate-sibling".to_string();
    store.create_run(&run).expect("run");
    let transaction =
        database::begin(&store.connection, TransactionMode::Deferred).expect("transaction");
    let db: &dyn Database = transaction.as_ref();
    let before = fixture_changes(db);
    let error = settle_parallel_failure(
        db,
        &run.run_id,
        "left",
        &activation_identity(&run.run_id, "left", 0),
        true,
        20,
    )
    .expect_err("duplicate sibling must not be cancelled twice");
    assert!(error.to_string().contains("members must be unique"));
    assert_eq!(fixture_changes(db), before);
}

fn assert_parallel_definition_independence(damage: impl FnOnce(&dyn Database)) {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    let definition = parallel_join_transform_definition();
    store
        .persist_definition("parallel-damage", 1, &definition)
        .expect("definition");
    let mut run = new_run();
    run.definition_id = "parallel-damage".to_string();
    store.create_run(&run).expect("run");
    damage(&store.connection);
    let output = ValidatedOutput {
        output_id: "left-output".to_string(),
        run_id: run.run_id.clone(),
        node_id: "left".to_string(),
        activation_id: activation_identity(&run.run_id, "left", 0),
        schema_id: "u32".to_string(),
        schema_version: 1,
        value: serde_json::json!(10),
        artifact_reference: None,
        created_at_ms: 20,
    };
    store
        .persist_validated_output(&output)
        .expect("run-owned left output");
    let result = store
        .persist_validated_output(&ValidatedOutput {
            output_id: "right-output".to_string(),
            node_id: "right".to_string(),
            activation_id: activation_identity(&run.run_id, "right", 0),
            value: serde_json::json!(20),
            ..output
        })
        .expect("run-owned join");
    assert_eq!(result.activated.len(), 1);
    assert_eq!(result.activated[0].node_id, "join");
    assert_eq!(result.activated[0].input, Some(serde_json::json!([10, 20])));
}

#[test]
fn parallel_join_rejects_ambiguous_member_configuration_atomically() {
    let temp = tempfile::tempdir().expect("temp");
    let mut definition = parallel_join_definition();
    definition
        .nodes
        .get_mut("join")
        .expect("join")
        .configuration["right_exits"] = serde_json::json!(["left"]);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("ambiguous-parallel", 1, &definition)
        .expect("definition");
    let mut run = new_run();
    run.run_id = "ambiguous-parallel-run".to_string();
    run.definition_id = "ambiguous-parallel".to_string();
    store.create_run(&run).expect("run");
    let output_count = |store: &WorkflowStore| {
        fixture_scalar_unsigned(
            &store.connection,
            &select("workflow_outputs")
                .project_as(switchy_database::query::count_all(), "count")
                .where_eq("run_id", "ambiguous-parallel-run"),
            "count",
        )
        .expect("output count")
    };
    let error = store
        .persist_validated_output(&ValidatedOutput {
            output_id: "left-output".to_string(),
            run_id: run.run_id.clone(),
            node_id: "left".to_string(),
            activation_id: activation_identity(&run.run_id, "left", 0),
            schema_id: "u32".to_string(),
            schema_version: 1,
            value: serde_json::json!(10),
            artifact_reference: None,
            created_at_ms: 20,
        })
        .expect_err("ambiguous members");
    assert!(matches!(error, WorkflowStoreError::InvalidData(_)));
    assert_eq!(output_count(&store), 0);
    assert_eq!(
        store
            .pending_activations(10)
            .expect("pending")
            .iter()
            .filter(|activation| activation.node_id == "join")
            .count(),
        0
    );
}

#[test]
fn durable_branch_predicate_failures_roll_back_output_decision_and_paths() {
    for (name, value, expected) in [
        (
            "missing",
            serde_json::json!({"review": {"result": {}}}),
            "was not present",
        ),
        (
            "incompatible",
            serde_json::json!({"review": {"result": {"passed": "true"}}}),
            "incompatible",
        ),
        (
            "nested",
            serde_json::json!({"review": []}),
            "was not present",
        ),
    ] {
        let temp = tempfile::tempdir().expect("temp");
        let definition = conditional_nested_definition();
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("conditional-nested", 1, &definition)
            .expect("definition");
        let run_id = format!("branch-{name}-run");
        store
            .create_run(&NewWorkflowRun {
                run_id: run_id.clone(),
                definition_id: "conditional-nested".to_string(),
                definition_version: 1,
                workspace_snapshot: "snapshot".to_string(),
                parent_session_id: None,
                parent_session_generation: None,
                binding: None,
                authored_provenance: None,
                input: Some(value.clone()),
                execution_authority: None,
                created_at_ms: 1,
                authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                    version: 1,
                    provider_id: "test-policy".to_string(),
                    profile_id: "build".to_string(),
                    policy_digest_sha256: "a".repeat(64),
                },
                authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
                limits: WorkflowRunLimits::default(),
            })
            .expect("run");
        store
            .persist_validated_output(&ValidatedOutput {
                output_id: format!("{name}-inspect-output"),
                run_id: run_id.clone(),
                node_id: "inspect".to_string(),
                activation_id: activation_identity(&run_id, "inspect", 0),
                schema_id: definition.nodes["inspect"].output.type_name.clone(),
                schema_version: 1,
                value: value.clone(),
                artifact_reference: None,
                created_at_ms: 2,
            })
            .expect("inspect output");
        let branch_output = ValidatedOutput {
            output_id: format!("{name}-branch-output"),
            run_id: run_id.clone(),
            node_id: "choose".to_string(),
            activation_id: activation_identity(&run_id, "choose", 0),
            schema_id: definition.nodes["choose"].output.type_name.clone(),
            schema_version: 1,
            value,
            artifact_reference: None,
            created_at_ms: 3,
        };
        let error = store
            .persist_validated_output(&branch_output)
            .expect_err("predicate failure");
        assert!(error.to_string().contains(expected));
        assert!(
            store
                .decision(&format!("{}:branch", branch_output.activation_id))
                .expect("decision")
                .is_none()
        );
        let output_count: u64 = fixture_scalar_unsigned(
            &store.connection,
            &select("workflow_outputs")
                .project_as(switchy_database::query::count_all(), "count")
                .where_eq("output_id", branch_output.output_id.as_str()),
            "count",
        )
        .expect("output count");
        assert_eq!(output_count, 0);
        assert!(
            store
                .pending_activations(10)
                .expect("pending")
                .iter()
                .all(|activation| {
                    activation.node_id != "selected" && activation.node_id != "other"
                })
        );
    }
}

#[test]
fn branch_decision_commits_before_successor_materialization_and_survives_restart() {
    struct Fault;
    impl WorkflowOutputFault for Fault {
        fn after_boundary(
            &self,
            boundary: WorkflowOutputBoundary,
            _output: &ValidatedOutput,
        ) -> Result<(), WorkflowStoreError> {
            if boundary == WorkflowOutputBoundary::SuccessorsMaterialized {
                return Err(WorkflowStoreError::InvalidData(
                    "crash after successors".to_string(),
                ));
            }
            Ok(())
        }
    }

    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("conditional", 1, &conditional_definition())
        .expect("definition");
    let mut run = new_run();
    run.run_id = "branch-restart-run".to_string();
    run.definition_id = "conditional".to_string();
    store.create_run(&run).expect("run");
    store
        .persist_validated_output(&ValidatedOutput {
            output_id: "inspect-output".to_string(),
            run_id: run.run_id.clone(),
            node_id: "inspect".to_string(),
            activation_id: activation_identity(&run.run_id, "inspect", 0),
            schema_id: "u32".to_string(),
            schema_version: 1,
            value: serde_json::json!(1),
            artifact_reference: None,
            created_at_ms: 20,
        })
        .expect("inspect output");
    let branch_output = ValidatedOutput {
        output_id: "branch-output".to_string(),
        run_id: run.run_id.clone(),
        node_id: "choose".to_string(),
        activation_id: activation_identity(&run.run_id, "choose", 0),
        schema_id: "u32".to_string(),
        schema_version: 1,
        value: serde_json::json!(1),
        artifact_reference: None,
        created_at_ms: 21,
    };
    store
        .persist_validated_output_with_fault(&branch_output, &Fault)
        .expect_err("fault");
    assert!(
        store
            .decision(&format!("{}:branch", branch_output.activation_id))
            .expect("decision query")
            .is_none()
    );
    drop(store);

    let mut reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    let result = reopened
        .persist_validated_output(&branch_output)
        .expect("branch output");
    assert_eq!(result.activated.len(), 1);
    assert_eq!(result.activated[0].node_id, "selected");
    let decision = reopened
        .decision(&format!("{}:branch", branch_output.activation_id))
        .expect("decision query")
        .expect("decision");
    assert_eq!(
        decision.value["selected_entries"],
        serde_json::json!(["selected"])
    );
    let skipped = fixture_scalar_text(
        &reopened.connection,
        &select("workflow_activations")
            .columns(&["status"])
            .where_eq("run_id", "branch-restart-run")
            .where_eq("node_id", "other"),
        "status",
    )
    .expect("skipped");
    assert_eq!(skipped, "skipped");
}

#[test]
fn conditional_edges_from_task_nodes_select_exactly_one_successor() {
    let schema = bcode_workflow::ValueSchema::of::<serde_json::Value>();
    let source = bcode_workflow::NodeDefinition {
        id: "source".to_string(),
        name: "source".to_string(),
        kind: bcode_workflow::NodeKind::Task,
        dataflow: bcode_workflow::WorkflowNodeDataflowPolicy::Direct,
        input: schema.clone(),
        output: schema.clone(),
        resources: Vec::new(),
        configuration: serde_json::Value::Null,
    };
    let target = |id: &str| bcode_workflow::NodeDefinition {
        id: id.to_string(),
        name: id.to_string(),
        kind: bcode_workflow::NodeKind::Task,
        dataflow: bcode_workflow::WorkflowNodeDataflowPolicy::Direct,
        input: schema.clone(),
        output: schema.clone(),
        resources: Vec::new(),
        configuration: serde_json::Value::Null,
    };
    let predicate = bcode_workflow::PredicateExpression::Equals {
        version: bcode_workflow::WORKFLOW_PREDICATE_VERSION,
        path: "status".to_string(),
        value: serde_json::json!("ready"),
    };
    let definition = WorkflowDefinition {
        schema_version: bcode_workflow::WORKFLOW_DEFINITION_SCHEMA_VERSION,
        name: "conditional-task".to_string(),
        input: schema.clone(),
        output: schema.clone(),
        nodes: BTreeMap::from([
            ("source".to_string(), source),
            ("ready".to_string(), target("ready")),
            ("other".to_string(), target("other")),
        ]),
        entries: vec!["source".to_string()],
        exits: vec!["ready".to_string(), "other".to_string()],
        edges: vec![
            bcode_workflow::EdgeDefinition {
                from: "source".to_string(),
                to: "ready".to_string(),
                kind: bcode_workflow::EdgeKind::Conditional {
                    predicate: predicate.clone(),
                    expected: true,
                },
                transform: None,
            },
            bcode_workflow::EdgeDefinition {
                from: "source".to_string(),
                to: "other".to_string(),
                kind: bcode_workflow::EdgeKind::Conditional {
                    predicate,
                    expected: false,
                },
                transform: None,
            },
        ],
    };
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("conditional-task", 1, &definition)
        .expect("definition");
    let mut run = new_run();
    run.run_id = "conditional-task-run".to_string();
    run.definition_id = "conditional-task".to_string();
    store.create_run(&run).expect("run");
    verify_conditional_successor_rollback(&mut store, &run, &definition);
}

fn verify_conditional_successor_rollback(
    store: &mut WorkflowStore,
    run: &NewWorkflowRun,
    definition: &WorkflowDefinition,
) {
    let source_schema_id = definition.nodes["source"].output.type_name.clone();
    let mut damaged = definition.clone();
    if let bcode_workflow::EdgeKind::Conditional { predicate, .. } = &mut damaged.edges[0].kind {
        *predicate = bcode_workflow::PredicateExpression::Equals {
            version: bcode_workflow::WORKFLOW_PREDICATE_VERSION + 1,
            path: "status".to_string(),
            value: serde_json::json!("ready"),
        };
    }
    block_on(
        update("workflow_run_graph_edges")
            .value(
                "edge_json",
                serde_json::to_string(&damaged.edges[0]).expect("damaged edge"),
            )
            .where_eq("run_id", "conditional-task-run")
            .where_eq("edge_id", 0)
            .execute(&store.connection),
    )
    .expect("damage fixture");
    assert!(
        store
            .persist_validated_output(&ValidatedOutput {
                output_id: "source-output".to_string(),
                run_id: run.run_id.clone(),
                node_id: "source".to_string(),
                activation_id: activation_identity(&run.run_id, "source", 0),
                schema_id: source_schema_id.clone(),
                schema_version: 1,
                value: serde_json::json!({"status": "other"}),
                artifact_reference: None,
                created_at_ms: 20,
            })
            .is_err()
    );
    let count = fixture_scalar_unsigned(
        &store.connection,
        &select("workflow_outputs").project_as(switchy_database::query::count_all(), "count"),
        "count",
    )
    .expect("output count");
    assert_eq!(
        count, 0,
        "predicate failure must roll back output persistence"
    );
    block_on(
        update("workflow_run_graph_edges")
            .value(
                "edge_json",
                serde_json::to_string(&definition.edges[0]).expect("edge"),
            )
            .where_eq("run_id", "conditional-task-run")
            .where_eq("edge_id", 0)
            .execute(&store.connection),
    )
    .expect("restore fixture");
    let result = store
        .persist_validated_output(&ValidatedOutput {
            output_id: "source-output".to_string(),
            run_id: run.run_id.clone(),
            node_id: "source".to_string(),
            activation_id: activation_identity(&run.run_id, "source", 0),
            schema_id: source_schema_id,
            schema_version: 1,
            value: serde_json::json!({"status": "other"}),
            artifact_reference: None,
            created_at_ms: 20,
        })
        .expect("output");
    assert_eq!(result.activated.len(), 1);
    assert_eq!(result.activated[0].node_id, "other");
}

#[test]
fn unmatched_conditional_edges_complete_with_the_source_terminal_output() {
    let schema = bcode_workflow::ValueSchema::of::<serde_json::Value>();
    let source = bcode_workflow::NodeDefinition {
        id: "source".to_string(),
        name: "source".to_string(),
        kind: bcode_workflow::NodeKind::Task,
        dataflow: bcode_workflow::WorkflowNodeDataflowPolicy::Direct,
        input: schema.clone(),
        output: schema.clone(),
        resources: Vec::new(),
        configuration: serde_json::Value::Null,
    };
    let target = bcode_workflow::NodeDefinition {
        id: "target".to_string(),
        name: "target".to_string(),
        kind: bcode_workflow::NodeKind::Task,
        dataflow: bcode_workflow::WorkflowNodeDataflowPolicy::Direct,
        input: schema.clone(),
        output: schema,
        resources: Vec::new(),
        configuration: serde_json::Value::Null,
    };
    let definition = WorkflowDefinition {
        schema_version: bcode_workflow::WORKFLOW_DEFINITION_SCHEMA_VERSION,
        name: "conditional-terminal".to_string(),
        input: source.input.clone(),
        output: source.output.clone(),
        nodes: BTreeMap::from([
            (source.id.clone(), source.clone()),
            (target.id.clone(), target),
        ]),
        entries: vec![source.id.clone()],
        exits: vec!["target".to_string()],
        edges: vec![bcode_workflow::EdgeDefinition {
            from: source.id.clone(),
            to: "target".to_string(),
            kind: bcode_workflow::EdgeKind::Conditional {
                predicate: bcode_workflow::PredicateExpression::SelectedEquals {
                    version: bcode_workflow::WORKFLOW_PREDICATE_VERSION,
                    selector: bcode_workflow::WorkflowValueSelector {
                        version: bcode_workflow::WORKFLOW_VALUE_SELECTOR_VERSION,
                        segments: vec![bcode_workflow::WorkflowValueSelectorSegment::Field {
                            name: "status".to_string(),
                        }],
                    },
                    value: serde_json::json!("continue"),
                },
                expected: true,
            },
            transform: None,
        }],
    };
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("conditional-terminal", 1, &definition)
        .expect("definition");
    let mut run = new_run();
    run.run_id = "conditional-terminal-run".to_string();
    run.definition_id = "conditional-terminal".to_string();
    store.create_run(&run).expect("run");
    let value = serde_json::json!({"status": "done"});
    let result = store
        .persist_validated_output(&ValidatedOutput {
            output_id: "source-output".to_string(),
            run_id: run.run_id.clone(),
            node_id: source.id,
            activation_id: activation_identity(&run.run_id, "source", 0),
            schema_id: source.output.type_name,
            schema_version: 1,
            value: value.clone(),
            artifact_reference: None,
            created_at_ms: 20,
        })
        .expect("output");
    assert!(result.activated.is_empty());
    assert_eq!(result.run_status, RunStatus::Completed);
    assert_eq!(
        store
            .canonical_terminal_output(&run.run_id)
            .expect("terminal")
            .expect("terminal")
            .value,
        value
    );
}

#[test]
fn durable_branch_selects_one_path_and_persists_decision() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("conditional", 1, &conditional_definition())
        .expect("definition");
    let mut run = new_run();
    run.run_id = "conditional-run".to_string();
    run.definition_id = "conditional".to_string();
    store.create_run(&run).expect("run");
    let inspect = store
        .persist_validated_output(&ValidatedOutput {
            output_id: "inspect-output".to_string(),
            run_id: "conditional-run".to_string(),
            node_id: "inspect".to_string(),
            activation_id: activation_identity("conditional-run", "inspect", 0),
            schema_id: "u32".to_string(),
            schema_version: 1,
            value: serde_json::json!(1),
            artifact_reference: None,
            created_at_ms: 20,
        })
        .expect("inspect output");
    assert_eq!(inspect.activated[0].node_id, "choose");
    let choose = store
        .persist_validated_output(&ValidatedOutput {
            output_id: "choose-output".to_string(),
            run_id: "conditional-run".to_string(),
            node_id: "choose".to_string(),
            activation_id: activation_identity("conditional-run", "choose", 0),
            schema_id: "u32".to_string(),
            schema_version: 1,
            value: serde_json::json!(1),
            artifact_reference: None,
            created_at_ms: 21,
        })
        .expect("controller output");
    assert_eq!(choose.run_status, RunStatus::Running);
    assert_eq!(choose.activated.len(), 1);
    assert_eq!(choose.activated[0].node_id, "selected");
    let other_status: String = fixture_scalar_text(
        &store.connection,
        &select("workflow_activations")
            .columns(&["status"])
            .where_eq("run_id", "conditional-run")
            .where_eq("node_id", "other"),
        "status",
    )
    .expect("other status");
    assert_eq!(other_status, "skipped");
    let decision = store
        .decision(&format!(
            "{}:branch",
            activation_identity("conditional-run", "choose", 0)
        ))
        .expect("decision query")
        .expect("decision");
    assert_eq!(
        decision.value["predicate_version"],
        bcode_workflow::WORKFLOW_PREDICATE_VERSION
    );
    assert_eq!(decision.value["selected"], true);
    assert_eq!(
        decision.value["selected_entries"],
        serde_json::json!(["selected"])
    );
    assert_eq!(
        decision.value["skipped_nodes"],
        serde_json::json!(["other"])
    );
}

#[test]
fn validated_output_and_activation_complete_atomically() {
    let (_temp, mut store) = initialized_store();
    store
        .persist_validated_output(&ValidatedOutput {
            output_id: "output-1".to_string(),
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: activation_id(),
            schema_id: "u32".to_string(),
            schema_version: 1,
            value: serde_json::json!(1),
            artifact_reference: None,
            created_at_ms: 13,
        })
        .expect("output");
    let status: String = fixture_scalar_text(
        &store.connection,
        &select("workflow_activations")
            .columns(&["status"])
            .where_eq("run_id", "run-1"),
        "status",
    )
    .expect("status");
    assert_eq!(status, "completed");
}

#[test]
fn resumable_pending_runs_exclude_paused_cancelled_repair_and_active_work() {
    let (temp, mut store) = initialized_store();
    assert_eq!(
        store.resumable_pending_run_ids(10).expect("resumable"),
        ["run-1"]
    );

    store.pause_run("run-1", 20).expect("pause");
    assert!(
        store
            .resumable_pending_run_ids(10)
            .expect("paused")
            .is_empty()
    );
    store.resume_run("run-1", 21).expect("resume");
    let prepared = store
        .prepare_attempt(&PreparedAttempt {
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: activation_id(),
            attempt: 1,
            side_effect: DispatchSideEffect::ReadOnly,
            intent: serde_json::json!({}),
            prepared_at_ms: 22,
        })
        .expect("prepare");
    assert!(
        store
            .resumable_pending_run_ids(10)
            .expect("active attempt")
            .is_empty()
    );
    store.request_cancellation("run-1", 23).expect("cancel");
    store
        .settle_orphaned_attempt_cancellation(&prepared, 24)
        .expect("settle");
    assert!(
        store
            .resumable_pending_run_ids(10)
            .expect("cancelled")
            .is_empty()
    );

    let mut repair = initialized_store_at(temp.path().join("repair").as_path());
    let identity = prepare_receipt_backed_attempt(&mut repair, DispatchSideEffect::Mutating);
    repair.request_cancellation("run-1", 20).expect("cancel");
    repair
        .settle_orphaned_attempt_cancellation(&identity, 21)
        .expect("repair required");
    assert!(
        repair
            .resumable_pending_run_ids(10)
            .expect("repair")
            .is_empty()
    );
}

#[test]
fn pending_activation_reads_are_bounded_and_definition_backed() {
    let (_temp, mut store) = initialized_store();
    let pending = store.pending_activations(10).expect("pending");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].node_id, "review");
    assert_eq!(pending[0].input, Some(serde_json::json!(1)));
    assert_eq!(pending[0].node.kind, bcode_workflow::NodeKind::Task);
    store.pause_run("run-1", 20).expect("pause");
    assert!(store.pending_activations(10).expect("paused").is_empty());
    store.resume_run("run-1", 21).expect("resume");
    assert_eq!(store.pending_activations(10).expect("resumed").len(), 1);
    store.request_cancellation("run-1", 22).expect("cancel");
    assert!(store.pending_activations(10).expect("cancelled").is_empty());
}

#[test]
fn run_creation_database_diagnostics_classify_without_payloads() {
    let temp = tempfile::tempdir().expect("diagnostic fixture");
    let connection = fixture_open(&temp.path().join("diagnostics.db"), false);
    block_on(
        switchy_database::schema::create_table("example")
            .columns(vec![switchy_database::schema::Column {
                name: "id".into(),
                nullable: true,
                auto_increment: false,
                data_type: switchy_database::schema::DataType::BigInt,
                default: None,
            }])
            .execute(&connection),
    )
    .expect("schema");
    let errors = [
        (
            block_on(select("secret_table").execute(&connection)).expect_err("missing table"),
            "missing_table",
        ),
        (
            block_on(
                select("example")
                    .project(switchy_database::query::identifier("secret_column"))
                    .execute(&connection),
            )
            .expect_err("missing column"),
            "missing_column",
        ),
        (
            block_on(
                insert("example")
                    .value("secret_column", 1)
                    .execute(&connection),
            )
            .expect_err("missing insert column"),
            "missing_insert_column",
        ),
    ];
    for (error, expected) in errors {
        let reason = run_creation_database_reason(&error);
        assert_eq!(reason, expected);
        assert!(!reason.contains("secret"));
    }
}

#[test]
fn run_creation_atomically_materializes_stable_entry_activations() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("example", 1, &definition("example"))
        .expect("definition");
    store.create_run(&new_run()).expect("run");
    let activation_rows = fixture_rows(
        &store.connection,
        &select("workflow_activations")
            .columns(&[
                "node_id",
                "activation_id",
                "dependency_generation",
                "status",
            ])
            .where_eq("run_id", "run-1"),
    );
    let row = activation_rows.first().expect("activation");
    let activation = (
        fixture_text(row, "node_id"),
        fixture_text(row, "activation_id"),
        fixture_unsigned(row, "dependency_generation"),
        fixture_text(row, "status"),
    );
    assert_eq!(activation.0, "review");
    assert_eq!(activation.1, activation_identity("run-1", "review", 0));
    assert_eq!(activation.2, 0);
    assert_eq!(activation.3, "pending");
    let events = store.event_history("run-1", None, 10).expect("events");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event_type, "run_created");
    assert_eq!(events[1].event_type, "activation_created");
}

#[test]
fn idle_discovery_preserves_publication_read_fence() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    let fence = store.publication_read_fence().expect("fence");
    for _ in 0..3 {
        assert!(
            store
                .advance_continuation_discovery("artifact-a", 1)
                .unwrap()
                .is_empty()
        );
        store
            .validate_publication_read_fence(&fence)
            .expect("idle discovery is non-mutating");
    }
    store
        .persist_definition("example", 1, &definition("example"))
        .unwrap();
    assert!(store.validate_publication_read_fence(&fence).is_err());
}

#[test]
fn durable_discovery_survives_restart_and_isolates_artifacts() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("example", 1, &definition("example"))
        .expect("definition");
    for id in ["a", "b", "c"] {
        let mut run = new_run();
        run.run_id = id.into();
        store.create_run(&run).expect("run");
    }
    assert_eq!(
        store
            .advance_continuation_discovery("artifact-a", 1)
            .expect("first"),
        ["a"]
    );
    drop(store);
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("restart");
    assert_eq!(
        store
            .advance_continuation_discovery("artifact-a", 1)
            .expect("second"),
        ["b"]
    );
    assert_eq!(
        store
            .advance_continuation_discovery("artifact-b", 1)
            .expect("independent"),
        ["a"]
    );
    assert_eq!(
        store
            .advance_continuation_discovery("artifact-a", 1)
            .expect("third"),
        ["c"]
    );
    assert!(
        store
            .advance_continuation_discovery("artifact-a", 1)
            .expect("end")
            .is_empty()
    );
    assert_eq!(
        store
            .advance_continuation_discovery("artifact-a", 1)
            .expect("revisit"),
        ["a"]
    );
    block_on(
        store.connection.exec_update_count(
            &(update("workflow_discovery_cursors")
                .value("after_run_id", zero_blob(1_000_000))
                .where_eq("artifact_id", "artifact-a")),
        ),
    )
    .expect("damage");
    assert!(
        store
            .advance_continuation_discovery("artifact-a", 1)
            .is_err()
    );
    assert_eq!(
        store
            .advance_continuation_discovery("artifact-b", 1)
            .expect("isolated"),
        ["b"]
    );
}

#[test]
fn running_discovery_pages_past_unchanged_runs() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("example", 1, &definition("example"))
        .expect("definition");
    for id in ["a", "b", "c"] {
        let mut run = new_run();
        run.run_id = id.to_owned();
        store.create_run(&run).expect("run");
    }
    block_on(
        update("workflow_runs")
            .value("status", "paused")
            .where_eq("run_id", "b")
            .execute(&store.connection),
    )
    .expect("pause");
    assert_eq!(
        store.continuation_run_ids_after("", 1).expect("first"),
        ["a"]
    );
    assert_eq!(
        store
            .continuation_run_ids_after("a", 1)
            .expect("paused page"),
        ["b"]
    );
    assert_eq!(
        store.continuation_run_ids_after("b", 1).expect("last"),
        ["c"]
    );
    assert!(
        store
            .continuation_run_ids_after("c", 1)
            .expect("end")
            .is_empty()
    );
    assert!(store.continuation_run_ids_after("", 0).is_err());
}

#[test]
fn run_input_is_bounded_and_validated_against_registered_definition() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("example", 1, &definition("example"))
        .expect("definition");
    let mut invalid = new_run();
    invalid.input = Some(serde_json::json!("not-a-number"));
    assert!(store.create_run(&invalid).is_err());
    let mut valid = new_run();
    valid.run_id = "run-valid".to_string();
    store.create_run(&valid).expect("valid input");
    let stored = fixture_scalar_text(
        &store.connection,
        &select("workflow_runs")
            .columns(&["input_json"])
            .where_eq("run_id", "run-valid"),
        "input_json",
    )
    .expect("input");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&stored).expect("json"),
        serde_json::json!(1)
    );
}

#[test]
fn canonical_path_is_below_workflows_directory() {
    assert_eq!(
        workflow_database_path(Path::new("/state")),
        Path::new("/state/workflows/workflow.db")
    );
}

#[test]
fn exact_definition_read_enforces_byte_budget_and_checksum() {
    let temp = tempfile::tempdir().unwrap();
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).unwrap();
    let expected = store.persist_definition("a", 1, &definition("a")).unwrap();
    let bytes = expected.definition_json.len();
    assert_eq!(
        store.definition_with_byte_budget("a", 1, bytes).unwrap(),
        Some(expected)
    );
    assert!(
        store
            .definition_with_byte_budget("a", 1, bytes - 1)
            .is_err()
    );
    assert!(store.definition_with_byte_budget("a", 1, 0).is_err());
    assert!(
        store
            .definition_with_byte_budget("missing", 1, 0)
            .unwrap()
            .is_none()
    );
    block_on(
        store.connection.exec_update_count(
            &update("workflow_definitions")
                .value("checksum_sha256", "corrupt")
                .where_eq("definition_id", "a"),
        ),
    )
    .unwrap();
    assert!(store.definition_with_byte_budget("a", 1, bytes).is_err());
}

#[test]
fn definition_window_enforces_aggregate_budget_without_partial_success() {
    let temp = tempfile::tempdir().unwrap();
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).unwrap();
    let first = store.persist_definition("a", 1, &definition("a")).unwrap();
    let second = store.persist_definition("b", 1, &definition("b")).unwrap();
    let bytes = first.definition_json.len() + second.definition_json.len();
    assert!(store.definition_snapshot_with_budget(1, bytes).is_err());
    assert_eq!(
        store.definition_snapshot_with_budget(2, bytes).unwrap(),
        vec![first.clone(), second.clone()]
    );
    assert!(store.definition_snapshot_with_budget(2, bytes - 1).is_err());
    assert_eq!(
        store.list_definitions_with_byte_budget(2, bytes).unwrap(),
        vec![first.clone(), second]
    );
    assert!(
        store
            .list_definitions_with_byte_budget(2, bytes - 1)
            .is_err()
    );
    assert_eq!(
        store
            .list_definitions_with_byte_budget(1, first.definition_json.len())
            .unwrap(),
        vec![first]
    );
    assert!(store.list_definitions_with_byte_budget(1, 0).is_err());
}

#[test]
fn definitions_persist_idempotently_and_verify_checksum() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    let definition = definition("example");
    let first = store
        .persist_definition("example", 1, &definition)
        .expect("persist");
    let second = store
        .persist_definition("example", 1, &definition)
        .expect("idempotent");
    assert_eq!(first, second);
    assert_eq!(
        store.definition("example", 1).expect("load"),
        Some(first.clone())
    );
    assert_eq!(store.list_definitions(10).expect("list"), [first]);
}

#[test]
fn output_execution_evidence_uses_bounded_retained_activation_input() {
    let temp = tempfile::tempdir().unwrap();
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).unwrap();
    store
        .persist_definition("example", 1, &definition("example"))
        .unwrap();
    store.create_run(&new_run()).unwrap();
    let activation_id = activation_identity("run-1", "review", 0);
    store
        .persist_validated_output(&ValidatedOutput {
            output_id: "execution-output".into(),
            run_id: "run-1".into(),
            node_id: "review".into(),
            activation_id: activation_id.clone(),
            schema_id: definition("example").nodes["review"]
                .output
                .type_name
                .clone(),
            schema_version: 1,
            value: serde_json::json!(7),
            artifact_reference: None,
            created_at_ms: 2,
        })
        .unwrap();
    let before = fixture_changes(&store.connection);
    let evidence = store
        .inspect_output_execution("run-1", "execution-output")
        .unwrap();
    assert_eq!(
        evidence.version,
        bcode_workflow::WorkflowOutputExecutionEvidence::VERSION
    );
    assert_eq!(evidence.admitted_input, serde_json::json!(1));
    assert_eq!(evidence.provenance.output.value, serde_json::json!(7));
    assert!(
        store
            .inspect_output_execution("foreign", "execution-output")
            .is_err()
    );
    assert!(store.inspect_output_execution("run-1", "missing").is_err());
    assert_eq!(fixture_changes(&store.connection), before);
    for damaged in [
        None,
        Some("not json".to_owned()),
        Some("x".repeat(MAX_INLINE_JSON_BYTES + 1)),
    ] {
        block_on(
            update("workflow_activations")
                .value(
                    "input_json",
                    damaged.map_or(DatabaseValue::Null, DatabaseValue::String),
                )
                .where_eq("run_id", "run-1")
                .where_eq("activation_id", activation_id.as_str())
                .execute(&store.connection),
        )
        .unwrap();
        let before = fixture_changes(&store.connection);
        assert!(
            store
                .inspect_output_execution("run-1", "execution-output")
                .is_err()
        );
        assert_eq!(fixture_changes(&store.connection), before);
    }
}

#[test]
fn bounded_run_inspection_queries_exclude_inline_output_values() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("example", 1, &definition("example"))
        .expect("definition");
    store.create_run(&new_run()).expect("run");
    let activation_id = activation_identity("run-1", "review", 0);
    store
        .persist_validated_output(&ValidatedOutput {
            output_id: "output-1".to_string(),
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id,
            schema_id: definition("example").nodes["review"]
                .output
                .type_name
                .clone(),
            schema_version: 1,
            value: serde_json::json!(7),
            artifact_reference: Some("artifact-1".to_string()),
            created_at_ms: 2,
        })
        .expect("output");
    let outputs = store.output_summaries("run-1", 10).expect("outputs");
    assert_eq!(outputs.len(), 1);
    assert_eq!(outputs[0].artifact_reference.as_deref(), Some("artifact-1"));
    let before = fixture_changes(&store.connection);
    let page = store
        .output_summaries_after("run-1", None, 1)
        .expect("page");
    assert_eq!(page[0].output_id, "output-1");
    assert!(
        store
            .output_summaries_after("run-1", Some("output-1"), 1)
            .expect("end")
            .is_empty()
    );
    let exact = store
        .inspect_exact_output("run-1", "output-1")
        .expect("exact output");
    assert_eq!(exact.value, serde_json::json!(7));
    assert_eq!(
        exact.activation_id,
        activation_identity("run-1", "review", 0)
    );
    let provenance = store
        .inspect_output_provenance("run-1", "output-1")
        .expect("producer evidence");
    assert_eq!(provenance.output, exact);
    assert_eq!(provenance.producer, definition("example").nodes["review"]);
    assert_eq!(provenance.producer_revision, 1);
    assert!(
        store
            .inspect_output_provenance("other-run", "output-1")
            .is_err()
    );
    assert!(store.inspect_output_provenance("run-1", "absent").is_err());
    assert!(store.inspect_exact_output("other-run", "output-1").is_err());
    assert!(store.inspect_exact_output("run-1", "absent").is_err());
    assert_eq!(fixture_changes(&store.connection), before);
    block_on(
        update("workflow_outputs")
            .value("value_json", "8")
            .where_eq("output_id", "output-1")
            .execute(&store.connection),
    )
    .expect("damage fixture");
    assert!(store.inspect_exact_output("run-1", "output-1").is_err());
    assert!(
        store
            .grants_for_run("run-1", 10)
            .expect("grants")
            .is_empty()
    );
    assert!(
        store
            .resource_leases_for_run("run-1", 10)
            .expect("leases")
            .is_empty()
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn execution_session_links_are_exact_idempotent_and_conflict_safe() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    let workflow = WorkflowBuilder::new(
        "execution-link",
        Step::task("review", |value: u32, _context| async move { Ok(value) }),
    )
    .build()
    .expect("workflow");
    store
        .persist_definition("execution-link", 1, workflow.definition())
        .expect("definition");
    store
        .create_run(&NewWorkflowRun {
            run_id: "execution-link-run".to_string(),
            definition_id: "execution-link".to_string(),
            definition_version: 1,
            workspace_snapshot: "snapshot-1".to_string(),
            parent_session_id: Some("parent-session".to_string()),
            parent_session_generation: None,
            binding: None,
            authored_provenance: None,
            input: Some(serde_json::json!(1)),
            execution_authority: None,
            created_at_ms: 1,
            limits: WorkflowRunLimits::default(),
            authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                version: 1,
                provider_id: "test-policy".to_string(),
                profile_id: "build".to_string(),
                policy_digest_sha256: "a".repeat(64),
            },
            authorization_ceiling: bcode_workflow::WorkflowToolCapability::Disabled,
        })
        .expect("run");
    let pending = store
        .pending_activations(1)
        .expect("pending")
        .pop()
        .expect("activation");
    let prepared = store
        .prepare_pending_activation(
            &pending.run_id,
            &pending.node_id,
            &pending.activation_id,
            DispatchSideEffect::ReadOnly,
            serde_json::json!({"owner": "test"}),
            2,
        )
        .expect("prepare")
        .expect("prepared");
    let link = WorkflowExecutionSessionLink::new(
        pending.run_id.clone(),
        pending.node_id.clone(),
        pending.activation_id.clone(),
        prepared.attempt,
        "session-1".to_string(),
        "snapshot-1".to_string(),
        3,
    );
    assert!(store.link_execution_session(&link).expect("first link"));
    assert!(
        !store
            .link_execution_session(&link)
            .expect("idempotent link")
    );
    assert_eq!(
        store
            .execution_session_link(
                &link.run_id,
                &link.node_id,
                &link.activation_id,
                link.attempt,
            )
            .expect("load link"),
        Some(link.clone())
    );
    assert_eq!(
        store
            .execution_session_links_for_run(&link.run_id, 10)
            .expect("bounded links"),
        vec![link.clone()]
    );
    assert!(
        store
            .execution_session_links_for_run(&link.run_id, 0)
            .is_err()
    );
    let mut conflict = link.clone();
    conflict.session_id = "session-2".to_string();
    assert!(store.link_execution_session(&conflict).is_err());
    let mut future = link.clone();
    future.version = WORKFLOW_EXECUTION_SESSION_LINK_VERSION + 1;
    assert!(store.link_execution_session(&future).is_err());
    let missing = WorkflowExecutionSessionLink::new(
        link.run_id,
        link.node_id,
        link.activation_id,
        2,
        "session-3".to_string(),
        link.workspace_snapshot,
        4,
    );
    assert!(store.link_execution_session(&missing).is_err());
}

#[test]
fn initial_workflow_and_draft_creation_is_atomic() {
    let temp = tempfile::tempdir().expect("temp");
    let (workflow, draft, _) = authored_store_fixture();
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .create_authored_workflow_with_initial_draft(&workflow, &draft)
        .expect("atomic create");
    assert_eq!(
        store
            .authored_workflow(&workflow.workflow_id)
            .expect("workflow"),
        Some(workflow.clone())
    );
    assert_eq!(
        store
            .workflow_draft(&workflow.workflow_id, &draft.draft_id)
            .expect("draft"),
        Some(draft.clone())
    );

    let mut conflicting = draft.clone();
    conflicting.draft_id = "other-draft".to_string();
    assert!(
        store
            .create_authored_workflow_with_initial_draft(&workflow, &conflicting)
            .is_err()
    );
    assert!(
        store
            .workflow_draft(&workflow.workflow_id, &conflicting.draft_id)
            .expect("conflicting draft")
            .is_none(),
        "workflow identity conflict must roll back the initial draft insert"
    );
}

#[test]
fn exact_revision_import_is_atomic_collision_safe_and_restart_safe() {
    let temp = tempfile::tempdir().expect("temp");
    let (workflow, draft, catalog) = authored_store_fixture();
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store.create_authored_workflow(&workflow).expect("workflow");
    store.create_workflow_draft(&draft).expect("draft");
    let preview = draft.document.compilation_preview(&catalog, None);
    store
        .publish_workflow_draft(
            &workflow.workflow_id,
            &draft.draft_id,
            1,
            &preview,
            true,
            None,
            3,
        )
        .expect("revision 1");
    let mut imported = draft.document.clone();
    imported.definition.name = "imported-revision-2".to_string();
    imported.producer = WorkflowProducerProvenance {
        kind: bcode_workflow::WorkflowProducerKind::Generated,
        producer_id: Some("workflow-import".to_string()),
        source_revision: Some(bcode_workflow::WorkflowRevisionIdentity {
            workflow_id: "source/workflow".to_string(),
            revision: 7,
        }),
    };
    let imported_preview = imported.compilation_preview(&catalog, None);
    let publication = store
        .import_workflow_revision(
            &workflow.workflow_id,
            2,
            &imported,
            &imported.producer,
            &imported_preview,
            true,
            Some(1),
            4,
        )
        .expect("import revision 2");
    assert_eq!(publication.revision.revision, 2);
    assert_eq!(publication.active_revision, Some(2));
    assert!(matches!(
        store.import_workflow_revision(
            &workflow.workflow_id,
            2,
            &imported,
            &imported.producer,
            &imported_preview,
            false,
            None,
            5,
        ),
        Err(WorkflowStoreError::AuthoringConflict { .. })
    ));
    drop(store);
    let reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
    assert_eq!(
        reopened
            .authored_workflow(&workflow.workflow_id)
            .expect("workflow")
            .expect("workflow")
            .active_revision,
        Some(2)
    );
    let revision = reopened
        .workflow_revision(&workflow.workflow_id, 2)
        .expect("revision")
        .expect("revision");
    assert_eq!(revision.document, imported);
    assert_eq!(
        revision.producer.kind,
        bcode_workflow::WorkflowProducerKind::Generated
    );
    assert_eq!(
        reopened
            .workflow_authoring_events(&workflow.workflow_id, None, 10)
            .expect("events")
            .last()
            .expect("event")
            .event_type,
        "revision_imported"
    );
}

#[test]
fn published_revisions_are_mechanically_immutable() {
    let temp = tempfile::tempdir().expect("temp");
    let (workflow, draft, catalog) = authored_store_fixture();
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store.create_authored_workflow(&workflow).expect("workflow");
    store.create_workflow_draft(&draft).expect("draft");
    let preview = draft.document.compilation_preview(&catalog, None);
    store
        .publish_workflow_draft(
            &workflow.workflow_id,
            &draft.draft_id,
            1,
            &preview,
            true,
            None,
            3,
        )
        .expect("publish");
    let update = block_on(
        update("workflow_revisions")
            .value("source_checksum_sha256", "changed")
            .where_eq("workflow_id", workflow.workflow_id.as_str())
            .where_eq("revision", 1)
            .execute(&store.connection),
    );
    let delete = block_on(
        delete("workflow_revisions")
            .where_eq("workflow_id", workflow.workflow_id.as_str())
            .where_eq("revision", 1)
            .execute(&store.connection),
    );
    assert!(update.is_err());
    assert!(delete.is_err());
    assert!(
        store
            .workflow_revision(&workflow.workflow_id, 1)
            .expect("revision")
            .is_some()
    );
}

#[test]
fn definition_registration_rejects_deserialized_invalid_structure() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    let mut definition = definition("example");
    definition.entries = vec!["missing".to_string()];
    let error = store
        .persist_definition("example", 1, &definition)
        .expect_err("invalid definition");
    assert!(error.to_string().contains("unknown step 'missing'"));
    assert!(store.list_definitions(10).expect("list").is_empty());
}

#[test]
fn definition_identity_conflicts_fail_closed() {
    let temp = tempfile::tempdir().expect("temp");
    let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
    store
        .persist_definition("example", 1, &definition("first"))
        .expect("first");
    let error = store
        .persist_definition("example", 1, &definition("second"))
        .expect_err("conflict");
    assert!(error.to_string().contains("identity conflict"));
}

#[path = "tests/authoring_packages.rs"]
mod authoring_packages;
mod bound_package_children;
#[path = "tests/graph_integrity.rs"]
mod graph_integrity;
#[path = "tests/storage_compatibility.rs"]
mod storage_compatibility;

#[test]
fn legacy_execution_cancellation_retains_parent_after_caller_settlement() {
    let (_temp, mut store, run, authority, _) = connected_publication_fixture();
    let (caller, link, target, target_id) =
        authenticated_cancellation_attempts(&mut store, &run, &authority);
    let caller_id = link.activation_id.clone();
    let mut node = store
        .current_run_graph_node(&run.run_id, "second")
        .unwrap()
        .unwrap()
        .node;
    node.kind = bcode_workflow::NodeKind::Agent;
    node.configuration = serde_json::to_value(
        bcode_workflow::WorkflowPromptConfiguration::preserve_input("build", "observe"),
    )
    .unwrap();
    let request = bcode_workflow::WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: run.run_id.clone(),
        mutation_id: "authenticated-cancel".into(),
        expected_revision: 2,
        edits: vec![
            bcode_workflow::WorkflowRunGraphEdit::RemoveEdge { edge_id: 0 },
            bcode_workflow::WorkflowRunGraphEdit::ReplaceNode {
                node,
                entry: false,
                exit: true,
            },
        ],
        reconciliation: vec![
            bcode_workflow::WorkflowRunGraphReconciliation::Retain {
                activation_id: caller_id,
            },
            bcode_workflow::WorkflowRunGraphReconciliation::Cancel {
                activation_id: target_id,
            },
        ],
    };
    store
        .stage_run_graph_edit_from_execution(&request, &authority, &link, 28)
        .unwrap();
    assert!(matches!(
        store
            .accept_pending_run_graph_publication_from_execution(
                &request.mutation_id,
                &authority,
                &link,
                29
            )
            .unwrap(),
        bcode_workflow::WorkflowRunGraphPublicationStatus::Pending { .. }
    ));
    assert_eq!(
        store
            .staged_run_graph_edit(&run.run_id, &request.mutation_id, &authority)
            .unwrap(),
        Some(request.clone())
    );
    assert!(matches!(
        store
            .accept_pending_run_graph_publication_from_execution(
                &request.mutation_id,
                &authority,
                &link,
                29
            )
            .unwrap(),
        bcode_workflow::WorkflowRunGraphPublicationStatus::Pending { .. }
    ));
    settle_authenticated_publication_caller(&mut store, &run.run_id, &caller);
    store
        .apply_attempt_observation(&target.dispatch_identity, AttemptObservation::Cancelled, 31)
        .unwrap();
    assert!(matches!(
        store
            .finalize_pending_run_graph_publication(
                &run.run_id,
                &request.mutation_id,
                &authority,
                32
            )
            .unwrap(),
        bcode_workflow::WorkflowRunGraphPublicationStatus::Committed { revision: 3 }
    ));
    let node = store
        .current_run_graph_node(&run.run_id, "second")
        .unwrap()
        .unwrap()
        .node;
    assert_eq!(
        node.configuration["execution_parent"]["session_id"],
        link.session_id
    );
    assert_eq!(
        store
            .staged_run_graph_edit(&run.run_id, &request.mutation_id, &authority)
            .unwrap(),
        Some(request)
    );
}

fn authenticated_cancellation_attempts(
    store: &mut WorkflowStore,
    run: &NewWorkflowRun,
    authority: &WorkflowExecutionAuthority,
) -> (
    PreparedActivationDispatch,
    WorkflowExecutionSessionLink,
    PreparedActivationDispatch,
    String,
) {
    let caller_id = activation_identity(&run.run_id, "first", 0);
    let caller = store
        .prepare_pending_activation(
            &run.run_id,
            "first",
            &caller_id,
            DispatchSideEffect::ReadOnly,
            serde_json::json!({}),
            25,
        )
        .unwrap()
        .unwrap();
    let link = WorkflowExecutionSessionLink::new(
        run.run_id.clone(),
        "first".into(),
        caller_id.clone(),
        caller.attempt,
        "actual-delegator".into(),
        "snapshot".into(),
        26,
    );
    store
        .persist_dispatch_receipt(&DispatchReceipt {
            run_id: run.run_id.clone(),
            node_id: "first".into(),
            activation_id: caller_id.clone(),
            attempt: caller.attempt,
            dispatch_identity: caller.dispatch_identity.clone(),
            receipt: serde_json::json!({}),
            admitted_at_ms: 26,
        })
        .unwrap();
    store.link_execution_session(&link).unwrap();
    create_local_publication_target(store, &run.run_id, authority);
    let target_id = activation_identity(&run.run_id, "second", 0);
    let target = store
        .prepare_pending_activation(
            &run.run_id,
            "second",
            &target_id,
            DispatchSideEffect::ReadOnly,
            serde_json::json!({}),
            26,
        )
        .unwrap()
        .unwrap();
    store
        .persist_dispatch_receipt(&DispatchReceipt {
            run_id: run.run_id.clone(),
            node_id: "second".into(),
            activation_id: target_id.clone(),
            attempt: target.attempt,
            dispatch_identity: target.dispatch_identity.clone(),
            receipt: serde_json::json!({}),
            admitted_at_ms: 27,
        })
        .unwrap();
    (caller, link, target, target_id)
}

fn settle_authenticated_publication_caller(
    store: &mut WorkflowStore,
    run: &str,
    caller: &PreparedActivationDispatch,
) {
    store
        .apply_attempt_observation(
            &caller.dispatch_identity,
            AttemptObservation::Succeeded {
                output: ValidatedOutput {
                    output_id: "caller-finished".into(),
                    run_id: run.into(),
                    node_id: "first".into(),
                    activation_id: caller.activation.activation_id.clone(),
                    schema_id: "u32".into(),
                    schema_version: 1,
                    value: serde_json::json!(1),
                    artifact_reference: None,
                    created_at_ms: 30,
                },
            },
            30,
        )
        .unwrap();
}
