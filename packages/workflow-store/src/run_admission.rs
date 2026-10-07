//! Atomic run admission on a caller-owned database lease.
use crate::*;
use switchy_database::query::{bounded_text, identifier, select};
use switchy_database::{Database, DatabaseValue as V};

pub fn create(db: &dyn Database, run: &NewWorkflowRun) -> Result<(), WorkflowStoreError> {
    validate_run(run)?;
    if !database::rows(
        db,
        &select("workflow_replacement_intents")
            .projections(database::projections(&["old_run_id"]))
            .where_eq("successor_run_id", run.run_id.as_str())
            .limit(1),
    )?
    .is_empty()
    {
        return Err(WorkflowStoreError::InvalidData(
            "run identity reserved by pending replacement".into(),
        ));
    }
    validate_authored_provenance(db, run)?;
    validate_binding_admission(db, run)?;
    let definition = admission_definition(db, run)?;
    persist_run(db, run, &definition)?;
    run_graph::materialize(db, &run.run_id, &definition)?;
    append_event_in(
        db,
        &run.run_id,
        "run_created",
        &serde_json::to_string(run)?,
        run.created_at_ms,
    )?;
    seed_entry_activations(db, run, &definition)?;
    Ok(())
}

fn validate_authored_provenance(
    db: &dyn Database,
    run: &NewWorkflowRun,
) -> Result<(), WorkflowStoreError> {
    if let Some(provenance) = &run.authored_provenance {
        let revisions = database::rows(
            db,
            &select("workflow_revisions")
                .projections(database::projections(&["definition_version"]))
                .project_as(
                    bounded_text(
                        identifier("definition_id"),
                        MAX_ID_BYTES as u64,
                        V::String(String::new()),
                    ),
                    "definition_id",
                )
                .where_eq("workflow_id", provenance.workflow_id.as_str())
                .where_eq("revision", database::integer_value(provenance.revision)?)
                .limit(1),
        )?;
        let revision = revisions.first().ok_or_else(|| {
            WorkflowStoreError::InvalidData("authored workflow revision not found".into())
        })?;
        if database::text(revision, "definition_id")? != run.definition_id
            || database::unsigned(revision, "definition_version")?
                != u64::from(run.definition_version)
        {
            return Err(WorkflowStoreError::InvalidData(
                "authored run provenance does not match the persisted revision definition".into(),
            ));
        }
        if let (Some(preset), Some(generation)) =
            (&provenance.preset_id, provenance.preset_generation)
        {
            let presets = database::rows(
                db,
                &select("workflow_presets")
                    .projections(database::projections(&["revision", "generation"]))
                    .where_eq("workflow_id", provenance.workflow_id.as_str())
                    .where_eq("preset_id", preset.as_str())
                    .limit(1),
            )?;
            let matches = presets
                .first()
                .map(|row| {
                    Ok::<_, WorkflowStoreError>((
                        database::unsigned(row, "revision")?,
                        database::unsigned(row, "generation")?,
                    ))
                })
                .transpose()?;
            if matches != Some((provenance.revision, generation)) {
                return Err(WorkflowStoreError::InvalidData(
                    "authored run provenance does not match the persisted preset generation".into(),
                ));
            }
        }
    }
    Ok(())
}

fn validate_binding_admission(
    db: &dyn Database,
    run: &NewWorkflowRun,
) -> Result<(), WorkflowStoreError> {
    if let Some(binding) = &run.binding {
        // A correlated existence probe remains bounded independently of replacement count.
        let replacements = select("workflow_replacement_intents").projections(
            std::iter::once("old_run_id")
                .map(|column| {
                    switchy_database::query::Projection::new(switchy_database::query::identifier(
                        column,
                    ))
                    .alias(column)
                })
                .collect(),
        );
        if !database::rows(
            db,
            &select("workflow_runs")
                .projections(database::projections(&["run_id"]))
                .where_eq("owner_plugin_id", binding.owner_plugin_id.as_str())
                .where_eq("workflow_kind", binding.workflow_kind.as_str())
                .where_eq("scope_key", binding.scope_key.as_str())
                .where_eq("single_active", 1_i64)
                .where_in("run_id", replacements)
                .limit(1),
        )?
        .is_empty()
        {
            return Err(WorkflowStoreError::InvalidData(
                "single-active binding reserved by pending replacement".into(),
            ));
        }
        if binding.single_active {
            let active = database::rows(
                db,
                &select("workflow_runs")
                    .project_as(
                        bounded_text(
                            identifier("run_id"),
                            MAX_ID_BYTES as u64,
                            V::String(String::new()),
                        ),
                        "run_id",
                    )
                    .project_as(
                        bounded_text(
                            identifier("status"),
                            MAX_ID_BYTES as u64,
                            V::String(String::new()),
                        ),
                        "status",
                    )
                    .where_eq("owner_plugin_id", binding.owner_plugin_id.as_str())
                    .where_eq("workflow_kind", binding.workflow_kind.as_str())
                    .where_eq("scope_key", binding.scope_key.as_str())
                    .where_eq("single_active", 1_i64)
                    .where_not_eq("status", "completed")
                    .where_not_eq("status", "failed")
                    .where_not_eq("status", "cancelled")
                    .limit(1),
            )?;
            if let Some(row) = active.first() {
                return Err(WorkflowStoreError::ActiveBindingConflict {
                    run_id: database::text(row, "run_id")?,
                    status: parse_run_status(&database::text(row, "status")?)?,
                });
            }
        }
    }
    Ok(())
}

fn admission_definition(
    db: &dyn Database,
    run: &NewWorkflowRun,
) -> Result<WorkflowDefinition, WorkflowStoreError> {
    let definitions = database::rows(
        db,
        &select("workflow_definitions")
            .projections(database::projections(&["checksum_sha256"]))
            .project_as(
                bounded_text(
                    identifier("definition_json"),
                    MAX_INLINE_JSON_BYTES as u64,
                    V::Null,
                ),
                "definition_json",
            )
            .where_eq("definition_id", run.definition_id.as_str())
            .where_eq("version", i64::from(run.definition_version))
            .limit(1),
    )?;
    let row = definitions.first().ok_or_else(|| {
        WorkflowStoreError::InvalidData(format!(
            "workflow definition not found: {} v{}",
            run.definition_id, run.definition_version
        ))
    })?;
    let json = database::text(row, "definition_json").map_err(|_| {
        WorkflowStoreError::InvalidData("workflow definition JSON is invalid or oversized".into())
    })?;
    if sha256_hex(json.as_bytes()) != database::text(row, "checksum_sha256")? {
        return Err(WorkflowStoreError::InvalidData(
            "workflow definition checksum mismatch".into(),
        ));
    }
    let definition: WorkflowDefinition = serde_json::from_str(&json)?;
    definition.validate().map_err(|error| {
        WorkflowStoreError::InvalidData(format!("invalid stored workflow definition: {error}"))
    })?;
    Ok(definition)
}

fn persist_run(
    db: &dyn Database,
    run: &NewWorkflowRun,
    definition: &WorkflowDefinition,
) -> Result<(), WorkflowStoreError> {
    let input = run
        .input
        .as_ref()
        .map(|input| validate_run_input(definition, input))
        .transpose()?;
    let binding = run.binding.as_ref();
    let authority = run.execution_authority.as_ref();
    crate::database::block_on(
        db.insert("workflow_runs")
            .value("run_id", run.run_id.as_str())
            .value("definition_id", run.definition_id.as_str())
            .value("definition_version", i64::from(run.definition_version))
            .value("workspace_snapshot", run.workspace_snapshot.as_str())
            .value("parent_session_id", run.parent_session_id.clone())
            .value(
                "parent_session_generation",
                database::optional_integer_value(run.parent_session_generation)?,
            )
            .value(
                "owner_plugin_id",
                binding.map(|b| b.owner_plugin_id.clone()),
            )
            .value("workflow_kind", binding.map(|b| b.workflow_kind.clone()))
            .value("scope_key", binding.map(|b| b.scope_key.clone()))
            .value(
                "display_label",
                binding.and_then(|b| b.display_label.clone()),
            )
            .value(
                "single_active",
                i64::from(binding.is_some_and(|b| b.single_active)),
            )
            .value(
                "authored_provenance_json",
                run.authored_provenance
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?,
            )
            .value("input_json", input)
            .value(
                "authorization_profile_json",
                serde_json::to_string(&run.authorization_profile)?,
            )
            .value(
                "authorization_ceiling",
                workflow_capability_name(run.authorization_ceiling),
            )
            .value(
                "target_artifact_id",
                authority.map(|a| a.target_artifact_id.clone()),
            )
            .value(
                "coordinator_daemon_instance_id",
                authority.map(|a| a.daemon_instance_id.clone()),
            )
            .value(
                "coordinator_generation",
                database::optional_integer_value(authority.map(|a| a.generation))?,
            )
            .value(
                "coordinator_fencing_token",
                authority.map(|a| a.fencing_token.clone()),
            )
            .value("status", RunStatus::Running.as_str())
            .value(
                "deadline_at_ms",
                database::optional_integer_value(run.limits.deadline_at_ms)?,
            )
            .value(
                "node_execution_cap",
                database::integer_value(run.limits.node_execution_cap)?,
            )
            .value(
                "concurrency_cap",
                database::optional_integer_value(run.limits.concurrency_cap.map(u64::from))?,
            )
            .value("cycle_cap", i64::from(run.limits.cycle_cap))
            .value("retry_cap", i64::from(run.limits.retry_cap))
            .value(
                "recursion_depth_cap",
                i64::from(run.limits.recursion_depth_cap),
            )
            .value("descendant_cap", i64::from(run.limits.descendant_cap))
            .value("created_at_ms", database::integer_value(run.created_at_ms)?)
            .value("updated_at_ms", database::integer_value(run.created_at_ms)?)
            .execute(db),
    )?;
    Ok(())
}

fn seed_entry_activations(
    db: &dyn Database,
    run: &NewWorkflowRun,
    definition: &WorkflowDefinition,
) -> Result<(), WorkflowStoreError> {
    if definition.entries.is_empty() {
        return Err(WorkflowStoreError::InvalidData(
            "workflow definition has no entry nodes".into(),
        ));
    }
    for node_id in &definition.entries {
        let node = run_graph::initial_node(db, &run.run_id, node_id)?.ok_or_else(|| {
            WorkflowStoreError::InvalidData(format!("workflow entry node is missing: {node_id}"))
        })?;
        let entry_input = run.input.clone().unwrap_or(serde_json::Value::Null);
        validate_json_schema(
            &format!("workflow entry input for node {node_id}"),
            &node.input.schema,
            &entry_input,
        )?;
        let activation = NewActivation {
            run_id: run.run_id.clone(),
            node_id: node_id.clone(),
            activation_id: activation_identity(&run.run_id, node_id, 0),
            dependency_generation: 0,
            input: Some(entry_input),
            created_at_ms: run.created_at_ms,
        };
        validate_activation(&activation)?;
        insert_activation_with_status_in(db, &activation, activation_status_for_node(&node))?;
    }
    Ok(())
}
