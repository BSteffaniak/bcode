//! Structured creation of the canonical baseline tables and indexes.
use crate::WorkflowStoreError;
use switchy_database::query::SortDirection;
use switchy_database::schema::{
    Column, DataType, SchemaComparison as C, SchemaExpression as E, create_index, create_table,
    create_trigger,
};
use switchy_database::{Database, DatabaseValue};

pub fn initialize(db: &dyn Database, version: u32) -> Result<(), WorkflowStoreError> {
    if !matches!(version, 42 | 43) {
        return Err(WorkflowStoreError::UnsupportedStore {
            actual: Some(version),
            expected: crate::WORKFLOW_STORE_SCHEMA_VERSION,
        });
    }
    let transaction = crate::database::begin(db, switchy_database::TransactionMode::Deferred)?;
    install(&*transaction, version)?;
    crate::continuation::initialize(&*transaction)?;
    crate::run_graph::initialize(&*transaction)?;
    crate::run_graph::initialize_edit_candidates(&*transaction)?;
    crate::recovery::initialize(&*transaction)?;
    crate::database::block_on(async {
        create_table("workflow_storage_compatibility")
            .if_not_exists(true)
            .column(column("contract_id", DataType::Int, true, None))
            .primary_key("contract_id")
            .check(E::column("contract_id").compare(C::Equal, E::Integer(1)))
            .column(column("compatibility", DataType::Int, false, None))
            .check(E::column("compatibility").compare(C::Greater, E::Integer(0)))
            .execute(&*transaction)
            .await?;
        transaction
            .insert("workflow_storage_compatibility")
            .value("contract_id", 1_i64)
            .value("compatibility", i64::from(version - 40))
            .execute(&*transaction)
            .await?;
        transaction
            .insert("workflow_store_contract")
            .value("contract_id", 1_i64)
            .value("schema_version", i64::from(version))
            .execute(&*transaction)
            .await?;
        Ok::<(), WorkflowStoreError>(())
    })?;
    crate::database::commit(transaction)
}

fn install(db: &dyn Database, version: u32) -> Result<(), WorkflowStoreError> {
    install_store_contract(db)?;
    install_runs(db, version)?;
    install_run_links(db)?;
    install_activations(db)?;
    install_fan_out_members(db)?;
    install_attempts(db)?;
    install_execution_sessions(db)?;
    install_retry_schedules(db)?;
    install_outputs(db)?;
    install_decisions(db)?;
    install_grants(db)?;
    install_mutation_approvals(db)?;
    install_resource_leases(db)?;
    install_events(db)?;
    install_projection_checkpoints(db)?;
    install_authored_workflows(db)?;
    install_drafts(db)?;
    install_revisions(db)?;
    install_package_publications(db)?;
    install_presets(db)?;
    install_authoring_events(db)?;
    Ok(())
}

fn install_store_contract(db: &dyn Database) -> Result<(), WorkflowStoreError> {
    crate::database::block_on(async {
        create_table("workflow_store_contract")
            .if_not_exists(true)
            .column(column("contract_id", DataType::Int, true, None))
            .primary_key("contract_id")
            .check(E::column("contract_id").compare(C::Equal, E::Integer(1)))
            .column(column("schema_version", DataType::Int, false, None))
            .execute(db)
            .await?;
        create_table("workflow_run_packages")
            .if_not_exists(true)
            .column(column("run_id", DataType::Text, false, None))
            .primary_key("run_id")
            .foreign_key_column("run_id", "workflow_runs", "run_id")
            .column(column("package_id", DataType::Text, false, None))
            .column(column("lock_digest", DataType::Text, false, None))
            .composite_foreign_key(
                ["package_id", "lock_digest"],
                "workflow_package_publications",
                ["package_id", "package_lock_digest_sha256"],
            )
            .execute(db)
            .await?;
        create_table("workflow_definitions")
            .if_not_exists(true)
            .column(column("definition_id", DataType::Text, false, None))
            .column(column("version", DataType::Int, false, None))
            .check(E::column("version").compare(C::Greater, E::Integer(0)))
            .column(column("checksum_sha256", DataType::Text, false, None))
            .column(column("definition_json", DataType::Text, false, None))
            .primary_key_columns(["definition_id", "version"])
            .execute(db)
            .await?;

        Ok::<(), WorkflowStoreError>(())
    })
}

fn run_limit_columns(version: u32) -> Vec<Column> {
    vec![
        column("deadline_at_ms", DataType::Int, true, None),
        column("node_execution_cap", DataType::Int, false, None),
        column("concurrency_cap", DataType::Int, version == 43, None),
        column("cycle_cap", DataType::Int, false, None),
        column("retry_cap", DataType::Int, false, None),
        column(
            "recursion_depth_cap",
            DataType::Int,
            false,
            Some(DatabaseValue::Int64(8)),
        ),
        column(
            "descendant_cap",
            DataType::Int,
            false,
            Some(DatabaseValue::Int64(64)),
        ),
    ]
}

fn install_runs(db: &dyn Database, version: u32) -> Result<(), WorkflowStoreError> {
    crate::database::block_on(async {
        create_table("workflow_runs")
            .if_not_exists(true)
            .column(column("run_id", DataType::Text, false, None))
            .primary_key("run_id")
            .column(column("definition_id", DataType::Text, false, None))
            .column(column("definition_version", DataType::Int, false, None))
            .column(column("workspace_snapshot", DataType::Text, false, None))
            .column(column("parent_session_id", DataType::Text, true, None))
            .column(column(
                "parent_session_generation",
                DataType::Int,
                true,
                None,
            ))
            .column(column("owner_plugin_id", DataType::Text, true, None))
            .column(column("workflow_kind", DataType::Text, true, None))
            .column(column("scope_key", DataType::Text, true, None))
            .column(column("display_label", DataType::Text, true, None))
            .column(column(
                "single_active",
                DataType::Int,
                false,
                Some(DatabaseValue::Int64(0)),
            ))
            .column(column(
                "authored_provenance_json",
                DataType::Text,
                true,
                None,
            ))
            .column(column("input_json", DataType::Text, true, None))
            .column(column(
                "authorization_profile_json",
                DataType::Text,
                false,
                None,
            ))
            .column(column(
                "authorization_ceiling",
                DataType::Text,
                false,
                Some(DatabaseValue::String("disabled".into())),
            ))
            .column(column("target_artifact_id", DataType::Text, true, None))
            .column(column(
                "coordinator_daemon_instance_id",
                DataType::Text,
                true,
                None,
            ))
            .column(column("coordinator_generation", DataType::Int, true, None))
            .column(column(
                "coordinator_fencing_token",
                DataType::Text,
                true,
                None,
            ))
            .column(column("terminal_output_id", DataType::Text, true, None))
            .column(column(
                "terminal_output_checksum_sha256",
                DataType::Text,
                true,
                None,
            ))
            .column(column("status", DataType::Text, false, None))
            .column(column(
                "cancellation_requested_at_ms",
                DataType::Int,
                true,
                None,
            ))
            .columns(run_limit_columns(version))
            .column(column("created_at_ms", DataType::Int, false, None))
            .column(column("updated_at_ms", DataType::Int, false, None))
            .composite_foreign_key(
                ["definition_id", "definition_version"],
                "workflow_definitions",
                ["definition_id", "version"],
            )
            .execute(db)
            .await?;

        Ok::<(), WorkflowStoreError>(())
    })
}

fn install_run_links(db: &dyn Database) -> Result<(), WorkflowStoreError> {
    crate::database::block_on(async {
        create_table("workflow_run_links")
            .if_not_exists(true)
            .column(column("root_run_id", DataType::Text, false, None))
            .column(column("parent_run_id", DataType::Text, false, None))
            .column(column("parent_node_id", DataType::Text, false, None))
            .column(column("parent_activation_id", DataType::Text, false, None))
            .column(column("parent_attempt", DataType::Int, false, None))
            .check(E::column("parent_attempt").compare(C::Greater, E::Integer(0)))
            .column(column("child_run_id", DataType::Text, false, None))
            .primary_key("child_run_id")
            .column(column("version", DataType::Int, false, None))
            .column(column("target_json", DataType::Text, false, None))
            .column(column("depth", DataType::Int, false, None))
            .check(E::column("depth").compare(C::Greater, E::Integer(1)))
            .column(column("created_at_ms", DataType::Int, false, None))
            .unique_columns([
                "parent_run_id",
                "parent_node_id",
                "parent_activation_id",
                "parent_attempt",
            ])
            .composite_foreign_key(["root_run_id"], "workflow_runs", ["run_id"])
            .composite_foreign_key(["parent_run_id"], "workflow_runs", ["run_id"])
            .composite_foreign_key(["child_run_id"], "workflow_runs", ["run_id"])
            .execute(db)
            .await?;

        Ok::<(), WorkflowStoreError>(())
    })
}

fn install_activations(db: &dyn Database) -> Result<(), WorkflowStoreError> {
    crate::database::block_on(async {
        create_table("workflow_activations")
            .if_not_exists(true)
            .column(column("run_id", DataType::Text, false, None))
            .column(column("node_id", DataType::Text, false, None))
            .column(column("activation_id", DataType::Text, false, None))
            .column(column("dependency_generation", DataType::Int, false, None))
            .column(column("input_json", DataType::Text, true, None))
            .column(column("status", DataType::Text, false, None))
            .column(column("output_id", DataType::Text, true, None))
            .column(column("created_at_ms", DataType::Int, false, None))
            .column(column(
                "node_revision",
                DataType::Int,
                false,
                Some(DatabaseValue::Int64(1)),
            ))
            .check(E::column("node_revision").compare(C::Greater, E::Integer(0)))
            .primary_key_columns(["run_id", "node_id", "activation_id"])
            .composite_foreign_key(["run_id"], "workflow_runs", ["run_id"])
            .execute(db)
            .await?;

        Ok::<(), WorkflowStoreError>(())
    })
}

fn install_fan_out_members(db: &dyn Database) -> Result<(), WorkflowStoreError> {
    crate::database::block_on(async {
        create_table("workflow_fan_out_members")
            .if_not_exists(true)
            .column(column("run_id", DataType::Text, false, None))
            .column(column("controller_node_id", DataType::Text, false, None))
            .column(column(
                "controller_activation_id",
                DataType::Text,
                false,
                None,
            ))
            .column(column("member_index", DataType::Int, false, None))
            .check(E::column("member_index").compare(C::GreaterEqual, E::Integer(0)))
            .column(column("member_node_id", DataType::Text, false, None))
            .column(column("member_activation_id", DataType::Text, false, None))
            .unique_columns(["member_activation_id"])
            .column(column("status", DataType::Text, false, None))
            .column(column("input_json", DataType::Text, false, None))
            .column(column("output_json", DataType::Text, true, None))
            .column(column("created_at_ms", DataType::Int, false, None))
            .column(column("terminal_at_ms", DataType::Int, true, None))
            .primary_key_columns(["run_id", "controller_activation_id", "member_index"])
            .composite_foreign_key(
                ["run_id", "controller_node_id", "controller_activation_id"],
                "workflow_activations",
                ["run_id", "node_id", "activation_id"],
            )
            .execute(db)
            .await?;

        Ok::<(), WorkflowStoreError>(())
    })
}

fn install_attempts(db: &dyn Database) -> Result<(), WorkflowStoreError> {
    crate::database::block_on(async {
        create_table("workflow_attempts")
            .if_not_exists(true)
            .column(column("run_id", DataType::Text, false, None))
            .column(column("node_id", DataType::Text, false, None))
            .column(column("activation_id", DataType::Text, false, None))
            .column(column("attempt", DataType::Int, false, None))
            .check(E::column("attempt").compare(C::Greater, E::Integer(0)))
            .column(column("dispatch_identity", DataType::Text, false, None))
            .unique_columns(["dispatch_identity"])
            .column(column("side_effect", DataType::Text, false, None))
            .column(column("status", DataType::Text, false, None))
            .column(column("intent_json", DataType::Text, false, None))
            .column(column("intent_checksum", DataType::Text, false, None))
            .column(column("receipt_json", DataType::Text, true, None))
            .column(column("prepared_at_ms", DataType::Int, false, None))
            .column(column("admitted_at_ms", DataType::Int, true, None))
            .column(column("terminal_at_ms", DataType::Int, true, None))
            .primary_key_columns(["run_id", "node_id", "activation_id", "attempt"])
            .composite_foreign_key(
                ["run_id", "node_id", "activation_id"],
                "workflow_activations",
                ["run_id", "node_id", "activation_id"],
            )
            .execute(db)
            .await?;

        Ok::<(), WorkflowStoreError>(())
    })
}

fn install_execution_sessions(db: &dyn Database) -> Result<(), WorkflowStoreError> {
    crate::database::block_on(async {
        create_table("workflow_execution_sessions")
            .if_not_exists(true)
            .column(column("version", DataType::Int, false, None))
            .check(E::column("version").compare(C::Greater, E::Integer(0)))
            .column(column("run_id", DataType::Text, false, None))
            .column(column("node_id", DataType::Text, false, None))
            .column(column("activation_id", DataType::Text, false, None))
            .column(column("attempt", DataType::Int, false, None))
            .check(E::column("attempt").compare(C::Greater, E::Integer(0)))
            .column(column("session_id", DataType::Text, false, None))
            .unique_columns(["session_id"])
            .column(column("workspace_snapshot", DataType::Text, false, None))
            .column(column("created_at_ms", DataType::Int, false, None))
            .primary_key_columns(["run_id", "node_id", "activation_id", "attempt"])
            .composite_foreign_key(
                ["run_id", "node_id", "activation_id", "attempt"],
                "workflow_attempts",
                ["run_id", "node_id", "activation_id", "attempt"],
            )
            .execute(db)
            .await?;

        Ok::<(), WorkflowStoreError>(())
    })
}

fn install_retry_schedules(db: &dyn Database) -> Result<(), WorkflowStoreError> {
    crate::database::block_on(async {
        create_table("workflow_retry_schedules")
            .if_not_exists(true)
            .column(column("run_id", DataType::Text, false, None))
            .column(column("node_id", DataType::Text, false, None))
            .column(column("activation_id", DataType::Text, false, None))
            .column(column("failed_attempt", DataType::Int, false, None))
            .check(E::column("failed_attempt").compare(C::Greater, E::Integer(0)))
            .column(column("next_attempt", DataType::Int, false, None))
            .check(E::column("next_attempt").compare(C::Greater, E::column("failed_attempt")))
            .column(column("failure_kind", DataType::Text, false, None))
            .column(column("backoff_ms", DataType::Int, false, None))
            .column(column("next_attempt_at_ms", DataType::Int, false, None))
            .column(column("scheduled_at_ms", DataType::Int, false, None))
            .primary_key_columns(["run_id", "node_id", "activation_id"])
            .composite_foreign_key(
                ["run_id", "node_id", "activation_id"],
                "workflow_activations",
                ["run_id", "node_id", "activation_id"],
            )
            .execute(db)
            .await?;

        Ok::<(), WorkflowStoreError>(())
    })
}

fn install_outputs(db: &dyn Database) -> Result<(), WorkflowStoreError> {
    crate::database::block_on(async {
        create_table("workflow_outputs")
            .if_not_exists(true)
            .column(column("output_id", DataType::Text, false, None))
            .primary_key("output_id")
            .column(column("run_id", DataType::Text, false, None))
            .column(column("node_id", DataType::Text, false, None))
            .column(column("activation_id", DataType::Text, false, None))
            .column(column("schema_id", DataType::Text, false, None))
            .column(column("schema_version", DataType::Int, false, None))
            .check(E::column("schema_version").compare(C::Greater, E::Integer(0)))
            .column(column("value_json", DataType::Text, false, None))
            .column(column("artifact_reference", DataType::Text, true, None))
            .column(column("checksum_sha256", DataType::Text, false, None))
            .column(column("created_at_ms", DataType::Int, false, None))
            .composite_foreign_key(
                ["run_id", "node_id", "activation_id"],
                "workflow_activations",
                ["run_id", "node_id", "activation_id"],
            )
            .execute(db)
            .await?;

        Ok::<(), WorkflowStoreError>(())
    })
}

fn install_decisions(db: &dyn Database) -> Result<(), WorkflowStoreError> {
    crate::database::block_on(async {
        create_table("workflow_decisions")
            .if_not_exists(true)
            .column(column("decision_id", DataType::Text, false, None))
            .primary_key("decision_id")
            .column(column("run_id", DataType::Text, false, None))
            .column(column("node_id", DataType::Text, true, None))
            .column(column("decision_type", DataType::Text, false, None))
            .column(column("value_json", DataType::Text, false, None))
            .column(column("created_at_ms", DataType::Int, false, None))
            .composite_foreign_key(["run_id"], "workflow_runs", ["run_id"])
            .execute(db)
            .await?;

        Ok::<(), WorkflowStoreError>(())
    })
}

fn install_grants(db: &dyn Database) -> Result<(), WorkflowStoreError> {
    crate::database::block_on(async {
        create_table("workflow_grants")
            .if_not_exists(true)
            .column(column("grant_id", DataType::Text, false, None))
            .primary_key("grant_id")
            .column(column("run_id", DataType::Text, false, None))
            .column(column("node_id", DataType::Text, false, None))
            .column(column("scope_json", DataType::Text, false, None))
            .column(column("granted_at_ms", DataType::Int, false, None))
            .column(column("expires_at_ms", DataType::Int, true, None))
            .column(column("max_uses", DataType::Int, true, None))
            .column(column(
                "uses_consumed",
                DataType::Int,
                false,
                Some(DatabaseValue::Int64(0)),
            ))
            .composite_foreign_key(["run_id"], "workflow_runs", ["run_id"])
            .execute(db)
            .await?;

        Ok::<(), WorkflowStoreError>(())
    })
}

fn install_mutation_approvals(db: &dyn Database) -> Result<(), WorkflowStoreError> {
    crate::database::block_on(async {
        create_table("workflow_mutation_approvals")
            .if_not_exists(true)
            .column(column("approval_id", DataType::Text, false, None))
            .primary_key("approval_id")
            .column(column("run_id", DataType::Text, false, None))
            .column(column("node_id", DataType::Text, false, None))
            .column(column("activation_id", DataType::Text, false, None))
            .column(column("scope_json", DataType::Text, false, None))
            .column(column("status", DataType::Text, false, None))
            .check(E::Or(vec![
                E::column("status").compare(C::Equal, E::Text("pending".into())),
                E::column("status").compare(C::Equal, E::Text("approved".into())),
                E::column("status").compare(C::Equal, E::Text("denied".into())),
                E::column("status").compare(C::Equal, E::Text("cancelled".into())),
                E::column("status").compare(C::Equal, E::Text("expired".into())),
            ]))
            .column(column("requested_at_ms", DataType::Int, false, None))
            .column(column("resolved_at_ms", DataType::Int, true, None))
            .column(column("expires_at_ms", DataType::Int, true, None))
            .column(column("grant_id", DataType::Text, true, None))
            .composite_foreign_key(
                ["run_id", "node_id", "activation_id"],
                "workflow_activations",
                ["run_id", "node_id", "activation_id"],
            )
            .execute(db)
            .await?;

        Ok::<(), WorkflowStoreError>(())
    })
}

fn install_resource_leases(db: &dyn Database) -> Result<(), WorkflowStoreError> {
    crate::database::block_on(async {
        create_table("workflow_resource_leases")
            .if_not_exists(true)
            .column(column("lease_id", DataType::Text, false, None))
            .primary_key("lease_id")
            .column(column("run_id", DataType::Text, false, None))
            .column(column("node_id", DataType::Text, false, None))
            .column(column("activation_id", DataType::Text, false, None))
            .column(column("resource_key", DataType::Text, false, None))
            .column(column("mode", DataType::Text, false, None))
            .check(E::Or(vec![
                E::column("mode").compare(C::Equal, E::Text("read".into())),
                E::column("mode").compare(C::Equal, E::Text("write".into())),
            ]))
            .column(column("acquired_at_ms", DataType::Int, false, None))
            .column(column("expires_at_ms", DataType::Int, true, None))
            .column(column("released_at_ms", DataType::Int, true, None))
            .composite_foreign_key(
                ["run_id", "node_id", "activation_id"],
                "workflow_activations",
                ["run_id", "node_id", "activation_id"],
            )
            .execute(db)
            .await?;

        Ok::<(), WorkflowStoreError>(())
    })
}

fn install_events(db: &dyn Database) -> Result<(), WorkflowStoreError> {
    crate::database::block_on(async {
        create_table("workflow_events")
            .if_not_exists(true)
            .column(Column {
                name: "event_seq".into(),
                data_type: DataType::Int,
                nullable: true,
                auto_increment: true,
                default: None,
            })
            .primary_key("event_seq")
            .column(column("run_id", DataType::Text, false, None))
            .column(column("event_type", DataType::Text, false, None))
            .column(column("payload_json", DataType::Text, false, None))
            .column(column("created_at_ms", DataType::Int, false, None))
            .composite_foreign_key(["run_id"], "workflow_runs", ["run_id"])
            .execute(db)
            .await?;

        Ok::<(), WorkflowStoreError>(())
    })
}

fn install_projection_checkpoints(db: &dyn Database) -> Result<(), WorkflowStoreError> {
    crate::database::block_on(async {
        create_table("workflow_projection_checkpoints")
            .if_not_exists(true)
            .column(column("projection_name", DataType::Text, false, None))
            .primary_key("projection_name")
            .column(column("projection_version", DataType::Int, false, None))
            .column(column("last_event_seq", DataType::Int, false, None))
            .execute(db)
            .await?;
        create_index("idx_workflow_run_links_root")
            .table("workflow_run_links")
            .if_not_exists(true)
            .column_ordered("root_run_id", SortDirection::Asc)
            .column_ordered("created_at_ms", SortDirection::Asc)
            .column_ordered("child_run_id", SortDirection::Asc)
            .execute(db)
            .await?;
        create_index("idx_workflow_fan_out_members_dispatch")
            .table("workflow_fan_out_members")
            .if_not_exists(true)
            .column_ordered("run_id", SortDirection::Asc)
            .column_ordered("controller_activation_id", SortDirection::Asc)
            .column_ordered("status", SortDirection::Asc)
            .column_ordered("member_index", SortDirection::Asc)
            .execute(db)
            .await?;
        create_index("idx_workflow_execution_sessions_run")
            .table("workflow_execution_sessions")
            .if_not_exists(true)
            .column_ordered("run_id", SortDirection::Asc)
            .column_ordered("created_at_ms", SortDirection::Asc)
            .column_ordered("session_id", SortDirection::Asc)
            .execute(db)
            .await?;
        create_index("idx_workflow_mutation_approvals_pending")
            .table("workflow_mutation_approvals")
            .if_not_exists(true)
            .column_ordered("run_id", SortDirection::Asc)
            .column_ordered("status", SortDirection::Asc)
            .column_ordered("requested_at_ms", SortDirection::Asc)
            .execute(db)
            .await?;
        create_index("idx_workflow_resource_leases_active")
            .table("workflow_resource_leases")
            .if_not_exists(true)
            .column_ordered("run_id", SortDirection::Asc)
            .column_ordered("resource_key", SortDirection::Asc)
            .column_ordered("released_at_ms", SortDirection::Asc)
            .execute(db)
            .await?;
        create_index("idx_workflow_events_run_seq")
            .table("workflow_events")
            .if_not_exists(true)
            .column_ordered("run_id", SortDirection::Asc)
            .column_ordered("event_seq", SortDirection::Asc)
            .execute(db)
            .await?;

        Ok::<(), WorkflowStoreError>(())
    })
}

fn install_authored_workflows(db: &dyn Database) -> Result<(), WorkflowStoreError> {
    crate::database::block_on(async {
        create_table("authored_workflows")
            .if_not_exists(true)
            .column(column("workflow_id", DataType::Text, false, None))
            .primary_key("workflow_id")
            .column(column("title", DataType::Text, false, None))
            .column(column("description", DataType::Text, true, None))
            .column(column(
                "archived",
                DataType::Int,
                false,
                Some(DatabaseValue::Int64(0)),
            ))
            .column(column("active_revision", DataType::Int, true, None))
            .column(column("created_at_ms", DataType::Int, false, None))
            .column(column("updated_at_ms", DataType::Int, false, None))
            .composite_foreign_key(
                ["workflow_id", "active_revision"],
                "workflow_revisions",
                ["workflow_id", "revision"],
            )
            .execute(db)
            .await?;

        Ok::<(), WorkflowStoreError>(())
    })
}

fn install_drafts(db: &dyn Database) -> Result<(), WorkflowStoreError> {
    crate::database::block_on(async {
        create_table("workflow_drafts")
            .if_not_exists(true)
            .column(column("workflow_id", DataType::Text, false, None))
            .column(column("draft_id", DataType::Text, false, None))
            .column(column("base_revision", DataType::Int, true, None))
            .column(column("generation", DataType::Int, false, None))
            .check(E::column("generation").compare(C::Greater, E::Integer(0)))
            .column(column("checksum_sha256", DataType::Text, false, None))
            .column(column("document_json", DataType::Text, false, None))
            .column(column("producer_json", DataType::Text, false, None))
            .column(column("created_at_ms", DataType::Int, false, None))
            .column(column("updated_at_ms", DataType::Int, false, None))
            .primary_key_columns(["workflow_id", "draft_id"])
            .composite_foreign_key(["workflow_id"], "authored_workflows", ["workflow_id"])
            .composite_foreign_key(
                ["workflow_id", "base_revision"],
                "workflow_revisions",
                ["workflow_id", "revision"],
            )
            .execute(db)
            .await?;

        Ok::<(), WorkflowStoreError>(())
    })
}

fn install_revisions(db: &dyn Database) -> Result<(), WorkflowStoreError> {
    crate::database::block_on(async {
        create_table("workflow_revisions")
            .if_not_exists(true)
            .column(column("workflow_id", DataType::Text, false, None))
            .column(column("revision", DataType::Int, false, None))
            .check(E::column("revision").compare(C::Greater, E::Integer(0)))
            .column(column(
                "source_checksum_sha256",
                DataType::Text,
                false,
                None,
            ))
            .column(column(
                "executable_source_checksum_sha256",
                DataType::Text,
                false,
                None,
            ))
            .column(column("definition_id", DataType::Text, false, None))
            .column(column("definition_version", DataType::Int, false, None))
            .column(column("document_json", DataType::Text, false, None))
            .column(column("producer_json", DataType::Text, false, None))
            .column(column("published_at_ms", DataType::Int, false, None))
            .primary_key_columns(["workflow_id", "revision"])
            .composite_foreign_key(["workflow_id"], "authored_workflows", ["workflow_id"])
            .composite_foreign_key(
                ["definition_id", "definition_version"],
                "workflow_definitions",
                ["definition_id", "version"],
            )
            .execute(db)
            .await?;

        Ok::<(), WorkflowStoreError>(())
    })
}

fn install_package_publications(db: &dyn Database) -> Result<(), WorkflowStoreError> {
    crate::database::block_on(async {
        create_table("workflow_package_publications")
            .if_not_exists(true)
            .column(column("package_id", DataType::Text, false, None))
            .column(column(
                "package_lock_digest_sha256",
                DataType::Text,
                false,
                None,
            ))
            .column(column("lock_json", DataType::Text, false, None))
            .column(column("receipt_json", DataType::Text, false, None))
            .column(column("published_at_ms", DataType::Int, false, None))
            .primary_key_columns(["package_id", "package_lock_digest_sha256"])
            .execute(db)
            .await?;

        Ok::<(), WorkflowStoreError>(())
    })
}

fn install_presets(db: &dyn Database) -> Result<(), WorkflowStoreError> {
    crate::database::block_on(async {
        create_table("workflow_presets")
            .if_not_exists(true)
            .column(column("workflow_id", DataType::Text, false, None))
            .column(column("preset_id", DataType::Text, false, None))
            .column(column("revision", DataType::Int, false, None))
            .column(column("name", DataType::Text, false, None))
            .column(column("generation", DataType::Int, false, None))
            .check(E::column("generation").compare(C::Greater, E::Integer(0)))
            .column(column("configuration_json", DataType::Text, false, None))
            .column(column("run_limits_json", DataType::Text, true, None))
            .column(column("producer_json", DataType::Text, false, None))
            .column(column("created_at_ms", DataType::Int, false, None))
            .column(column("updated_at_ms", DataType::Int, false, None))
            .primary_key_columns(["workflow_id", "preset_id"])
            .composite_foreign_key(
                ["workflow_id", "revision"],
                "workflow_revisions",
                ["workflow_id", "revision"],
            )
            .execute(db)
            .await?;

        Ok::<(), WorkflowStoreError>(())
    })
}

fn install_authoring_events(db: &dyn Database) -> Result<(), WorkflowStoreError> {
    crate::database::block_on(async {
        create_table("workflow_authoring_events")
            .if_not_exists(true)
            .column(Column {
                name: "event_seq".into(),
                data_type: DataType::Int,
                nullable: true,
                auto_increment: true,
                default: None,
            })
            .primary_key("event_seq")
            .column(column("workflow_id", DataType::Text, false, None))
            .column(column("event_type", DataType::Text, false, None))
            .column(column("payload_json", DataType::Text, false, None))
            .column(column("created_at_ms", DataType::Int, false, None))
            .composite_foreign_key(["workflow_id"], "authored_workflows", ["workflow_id"])
            .execute(db)
            .await?;
        create_index("idx_workflow_package_publications_latest")
            .table("workflow_package_publications")
            .if_not_exists(true)
            .column_ordered("package_id", SortDirection::Asc)
            .column_ordered("published_at_ms", SortDirection::Desc)
            .execute(db)
            .await?;
        create_index("idx_authored_workflows_updated")
            .table("authored_workflows")
            .if_not_exists(true)
            .column_ordered("updated_at_ms", SortDirection::Desc)
            .column_ordered("workflow_id", SortDirection::Asc)
            .execute(db)
            .await?;
        create_index("idx_workflow_drafts_updated")
            .table("workflow_drafts")
            .if_not_exists(true)
            .column_ordered("workflow_id", SortDirection::Asc)
            .column_ordered("updated_at_ms", SortDirection::Desc)
            .column_ordered("draft_id", SortDirection::Asc)
            .execute(db)
            .await?;
        create_index("idx_workflow_revisions_published")
            .table("workflow_revisions")
            .if_not_exists(true)
            .column_ordered("workflow_id", SortDirection::Asc)
            .column_ordered("revision", SortDirection::Desc)
            .execute(db)
            .await?;
        create_index("idx_workflow_presets_updated")
            .table("workflow_presets")
            .if_not_exists(true)
            .column_ordered("workflow_id", SortDirection::Asc)
            .column_ordered("updated_at_ms", SortDirection::Desc)
            .column_ordered("preset_id", SortDirection::Asc)
            .execute(db)
            .await?;
        create_index("idx_workflow_authoring_events_identity")
            .table("workflow_authoring_events")
            .if_not_exists(true)
            .column_ordered("workflow_id", SortDirection::Asc)
            .column_ordered("event_seq", SortDirection::Asc)
            .execute(db)
            .await?;
        create_trigger("prevent_workflow_revision_update", "workflow_revisions")
            .if_not_exists(true)
            .before_update()
            .raise_abort("published workflow revisions are immutable")
            .execute(db)
            .await?;
        create_trigger("prevent_workflow_revision_delete", "workflow_revisions")
            .if_not_exists(true)
            .before_delete()
            .raise_abort("published workflow revisions are immutable")
            .execute(db)
            .await?;
        Ok::<(), WorkflowStoreError>(())
    })
}

fn column(
    name: &str,
    data_type: DataType,
    nullable: bool,
    default: Option<DatabaseValue>,
) -> Column {
    Column {
        name: name.into(),
        data_type,
        nullable,
        auto_increment: false,
        default,
    }
}
