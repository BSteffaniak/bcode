//! Canonical validated output persistence and successor settlement.
use super::{
    AuthoredWorkflowRunProvenance, MAX_ID_BYTES, MAX_INLINE_JSON_BYTES, NewActivation,
    NoopWorkflowOutputFault, OutputPersistenceResult, RunStatus, TargetInputValidationFailure,
    ValidatedOutput, WorkflowExecutionAuthority, WorkflowOutputBoundary, WorkflowOutputFault,
    WorkflowOutputSummary, WorkflowStore, WorkflowStoreError, activation_at_generation,
    activation_identity, activation_status_at_generation, activation_status_for_node,
    append_event_in, bounded_limit, database, enforce_activation_materialization_limits,
    parse_run_status, record_activation_graph_binding, run_graph, settle_parallel_failure,
    sha256_hex, validate_id, validate_json_schema, validate_output,
    validate_output_against_node_schema, validate_parallel_run_topology,
    verify_execution_authority_in,
};
use crate::database::block_on;
use switchy_database::query::{
    FilterableQuery as _, SelectQuery, SortDirection, bounded_text, count_all, identifier, insert,
    select, update, where_in,
};
use switchy_database::{Database, DatabaseError, DatabaseValue as V, Row, TransactionMode};

impl WorkflowStore {
    /// Persist validated node output before marking its activation complete.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed/bounded data, missing activation, conflicting output, or
    /// database failure.
    pub fn persist_validated_output(
        &mut self,
        output: &ValidatedOutput,
    ) -> Result<OutputPersistenceResult, WorkflowStoreError> {
        self.persist_validated_output_with_fault(output, &NoopWorkflowOutputFault)
    }

    /// Settle output against an exact graph revision under current execution authority.
    ///
    /// Ownership, graph selection, output persistence, and successor settlement share one
    /// transaction. This does not authorize unsupported graph reconciliation or replace
    /// operation-owner receipt checks performed by the dispatch coordinator.
    ///
    /// # Errors
    ///
    /// Returns an error for stale authority or revision, malformed output, conflicting output,
    /// unsupported reconciliation, or persistence failure. Errors roll back settlement.
    pub fn persist_validated_output_at_graph_revision(
        &mut self,
        output: &ValidatedOutput,
        expected_revision: u64,
        authority: &WorkflowExecutionAuthority,
    ) -> Result<OutputPersistenceResult, WorkflowStoreError> {
        self.persist_validated_output_at_graph_revision_with_fault(
            output,
            expected_revision,
            authority,
            &NoopWorkflowOutputFault,
        )
    }

    /// Settle an exact graph revision with deterministic in-transaction fault injection.
    ///
    /// Authority and revision checks precede persistence. Any injected failure rolls back
    /// output, activation, successor, and run projection changes together.
    ///
    /// # Errors
    /// Returns an error for stale authority or revision, invalid or conflicting output,
    /// unsupported reconciliation, persistence failure, or a fault-hook error.
    pub fn persist_validated_output_at_graph_revision_with_fault<F>(
        &mut self,
        output: &ValidatedOutput,
        expected_revision: u64,
        authority: &WorkflowExecutionAuthority,
        fault: &F,
    ) -> Result<OutputPersistenceResult, WorkflowStoreError>
    where
        F: WorkflowOutputFault + ?Sized,
    {
        validate_output(output)?;
        let transaction = database::begin(&self.connection, TransactionMode::Immediate)?;
        verify_execution_authority_in(transaction.as_ref(), &output.run_id, authority)?;
        if expected_revision == 0
            || run_graph::graph_revision(transaction.as_ref(), &output.run_id)?
                != Some(expected_revision)
        {
            return Err(WorkflowStoreError::InvalidData(
                "workflow settlement graph revision conflict".to_string(),
            ));
        }
        let result =
            persist_validated_output_transaction_with_fault(transaction.as_ref(), output, fault)?;
        database::commit(transaction)?;
        Ok(result)
    }

    /// Persist validated output with deterministic in-transaction fault injection.
    ///
    /// # Errors
    ///
    /// Returns an error from validation, persistence, successor scheduling, or the fault hook. Any
    /// error rolls back output, activation, successor, decision, and run projection changes.
    pub fn persist_validated_output_with_fault<F>(
        &mut self,
        output: &ValidatedOutput,
        fault: &F,
    ) -> Result<OutputPersistenceResult, WorkflowStoreError>
    where
        F: WorkflowOutputFault + ?Sized,
    {
        validate_output(output)?;
        let transaction = database::begin(&self.connection, TransactionMode::Immediate)?;
        let result =
            persist_validated_output_transaction_with_fault(transaction.as_ref(), output, fault)?;
        database::commit(transaction)?;
        Ok(result)
    }
}

pub fn persist_validated_output_transaction(
    transaction: &dyn Database,
    output: &ValidatedOutput,
) -> Result<OutputPersistenceResult, WorkflowStoreError> {
    persist_validated_output_transaction_with_fault(transaction, output, &NoopWorkflowOutputFault)
}

pub fn persist_validated_output_transaction_with_fault<F>(
    transaction: &dyn Database,
    output: &ValidatedOutput,
    fault: &F,
) -> Result<OutputPersistenceResult, WorkflowStoreError>
where
    F: WorkflowOutputFault + ?Sized,
{
    validate_output(output)?;
    validate_output_against_node_schema(transaction, output)?;
    let value_json = serde_json::to_string(&output.value)?;
    if value_json.len() > MAX_INLINE_JSON_BYTES {
        return Err(WorkflowStoreError::InvalidData(format!(
            "validated output exceeds {MAX_INLINE_JSON_BYTES} bytes"
        )));
    }
    let checksum = sha256_hex(value_json.as_bytes());
    let inserted = block_on(
        insert("workflow_outputs")
            .value("output_id", output.output_id.as_str())
            .value("run_id", output.run_id.as_str())
            .value("node_id", output.node_id.as_str())
            .value("activation_id", output.activation_id.as_str())
            .value("schema_id", output.schema_id.as_str())
            .value("schema_version", i64::from(output.schema_version))
            .value("value_json", value_json.as_str())
            .value(
                "artifact_reference",
                output_optional_text_value(output.artifact_reference.as_deref()),
            )
            .value("checksum_sha256", checksum.as_str())
            .value(
                "created_at_ms",
                database::integer_value(output.created_at_ms)?,
            )
            .on_conflict_do_nothing(["output_id"])
            .execute_count(transaction),
    )?;
    if inserted != 1 {
        return Err(WorkflowStoreError::InvalidData(
            "output insertion did not create exactly one output".into(),
        ));
    }
    fault.after_boundary(WorkflowOutputBoundary::OutputInserted, output)?;
    let changed = block_on(
        transaction.exec_update_count(
            &update("workflow_activations")
                .value("status", "completed")
                .value("output_id", output.output_id.as_str())
                .where_eq("run_id", output.run_id.as_str())
                .where_eq("node_id", output.node_id.as_str())
                .where_eq("activation_id", output.activation_id.as_str())
                .filter(Box::new(where_in(
                    "status",
                    vec![V::from("pending"), V::from("running")],
                ))),
        ),
    )?;
    if changed != 1 {
        return Err(WorkflowStoreError::InvalidData(format!(
            "activation is not pending or running: {}/{}/{}",
            output.run_id, output.node_id, output.activation_id
        )));
    }
    fault.after_boundary(WorkflowOutputBoundary::ActivationCompleted, output)?;
    append_event_in(
        transaction,
        &output.run_id,
        "output_validated",
        &serde_json::to_string(output)?,
        output.created_at_ms,
    )?;
    settle_output_successors(transaction, output, &checksum, fault)
}

pub fn settle_output_successors<F: WorkflowOutputFault + ?Sized>(
    transaction: &dyn Database,
    output: &ValidatedOutput,
    checksum: &str,
    fault: &F,
) -> Result<OutputPersistenceResult, WorkflowStoreError> {
    let parallel_failure = settle_parallel_failure(
        transaction,
        &output.run_id,
        &output.node_id,
        &output.activation_id,
        false,
        output.created_at_ms,
    )?
    .is_some_and(|settlement| settlement.run_failed);
    let (activated, completed_is_exit) = if parallel_failure {
        // A failed join has no value to materialize. Preserve the successful sibling's
        // output and the durable failure decision instead of rolling both back while
        // trying to construct a tuple from absent results.
        (Vec::new(), false)
    } else {
        materialize_direct_successors(transaction, output, fault)?
    };
    fault.after_boundary(WorkflowOutputBoundary::SuccessorsMaterialized, output)?;
    let has_unfinished = run_has_unfinished_activations(transaction, &output.run_id)?;
    let current_status = parse_run_status(&database::text(
        &output_required_row(
            transaction,
            &select("workflow_runs")
                .project_as(bounded_text(identifier("status"), 64, V::Null), "status")
                .where_eq("run_id", output.run_id.as_str())
                .limit(1),
        )?,
        "status",
    )?)?;
    let run_status = if parallel_failure {
        RunStatus::Failed
    } else if !has_unfinished
        && completed_is_exit
        && matches!(current_status, RunStatus::Running | RunStatus::Paused)
    {
        let changed = block_on(
            transaction.exec_update_count(
                &update("workflow_runs")
                    .value("status", "completed")
                    .value("terminal_output_id", output.output_id.as_str())
                    .value("terminal_output_checksum_sha256", checksum)
                    .value(
                        "updated_at_ms",
                        database::integer_value(output.created_at_ms)?,
                    )
                    .where_eq("run_id", output.run_id.as_str())
                    .filter(Box::new(where_in(
                        "status",
                        vec![V::from("running"), V::from("paused")],
                    ))),
            ),
        )?;
        if changed != 1 {
            return Err(WorkflowStoreError::InvalidData(
                "workflow completion did not update exactly one run".into(),
            ));
        }
        append_event_in(
            transaction,
            &output.run_id,
            "run_completed",
            "{}",
            output.created_at_ms,
        )?;
        RunStatus::Completed
    } else {
        current_status
    };
    Ok(OutputPersistenceResult {
        completed_activation_id: output.activation_id.clone(),
        activated,
        run_status,
    })
}

pub fn run_has_unfinished_activations(
    transaction: &dyn Database,
    run_id: &str,
) -> Result<bool, WorkflowStoreError> {
    let count = |statuses: Option<Vec<V>>| -> Result<u64, WorkflowStoreError> {
        let mut query = select("workflow_activations")
            .project_as(count_all(), "count")
            .where_eq("run_id", run_id);
        if let Some(statuses) = statuses {
            query = query.filter(Box::new(where_in("status", statuses)));
        }
        database::unsigned(&output_required_row(transaction, &query)?, "count")
    };
    let valid = [
        "pending",
        "running",
        "waiting_input",
        "waiting_approval",
        "waiting_mutation_approval",
        "completed",
        "skipped",
        "failed",
        "cancelled",
    ];
    if count(None)? != count(Some(valid.into_iter().map(V::from).collect()))? {
        return Err(WorkflowStoreError::InvalidData(
            "workflow run has unsupported activation status".into(),
        ));
    }
    Ok(count(Some(valid[..5].iter().copied().map(V::from).collect()))? != 0)
}

pub fn evaluate_predicate(
    expression: &bcode_workflow::PredicateExpression,
    value: &serde_json::Value,
) -> Result<bool, WorkflowStoreError> {
    expression
        .evaluate_value(value)
        .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))
}

pub fn configured_node_ids(
    configuration: &serde_json::Value,
    field: &str,
) -> Result<Vec<String>, WorkflowStoreError> {
    configuration
        .get(field)
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            WorkflowStoreError::InvalidData(format!(
                "workflow branch configuration is missing {field}"
            ))
        })?
        .iter()
        .map(|value| {
            value.as_str().map(ToString::to_string).ok_or_else(|| {
                WorkflowStoreError::InvalidData(format!(
                    "workflow branch configuration {field} must contain node IDs"
                ))
            })
        })
        .collect()
}

pub fn skip_branch_nodes(
    transaction: &dyn Database,
    run_id: &str,
    generation: u64,
    node_ids: &[String],
    created_at_ms: u64,
) -> Result<(), WorkflowStoreError> {
    let graph_revision = run_graph::graph_revision(transaction, run_id)?
        .ok_or_else(|| WorkflowStoreError::InvalidData("branch run is missing".into()))?;
    let mut absent_nodes = std::collections::BTreeSet::new();
    for node_id in node_ids {
        WorkflowStore::current_run_graph_node_in_snapshot(transaction, run_id, node_id)?
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData(format!(
                    "workflow branch skip references missing run-graph node: {node_id}"
                ))
            })?;
        if activation_status_at_generation(transaction, run_id, node_id, generation)?.is_none() {
            absent_nodes.insert(node_id);
        }
    }
    for node_id in absent_nodes {
        let changed = block_on(
            insert("workflow_activations")
                .value("run_id", run_id)
                .value("node_id", node_id.as_str())
                .value(
                    "activation_id",
                    activation_identity(run_id, node_id, generation),
                )
                .value(
                    "dependency_generation",
                    database::integer_value(generation)?,
                )
                .value("input_json", V::Null)
                .value("status", "skipped")
                .value("created_at_ms", database::integer_value(created_at_ms)?)
                .on_conflict_do_nothing(["run_id", "node_id", "activation_id"])
                .execute_count(transaction),
        )?;
        if changed != 1 {
            return Err(WorkflowStoreError::InvalidData(
                "branch skip did not insert exactly one activation".into(),
            ));
        }
        record_activation_graph_binding(
            transaction,
            run_id,
            node_id,
            &activation_identity(run_id, node_id, generation),
            graph_revision,
        )?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub enum TransformRunField {
    Input,
    AuthoredProvenance,
}

impl TransformRunField {
    const fn column(self) -> &'static str {
        match self {
            Self::Input => "input_json",
            Self::AuthoredProvenance => "authored_provenance_json",
        }
    }
}

pub fn transform_run_json(
    transaction: &dyn Database,
    run_id: &str,
    field: TransformRunField,
) -> Result<Option<String>, WorkflowStoreError> {
    let column = field.column();
    let row = output_required_row(
        transaction,
        &select("workflow_runs")
            .project_as(
                bounded_text(identifier(column), MAX_INLINE_JSON_BYTES as u64, V::Null),
                "value",
            )
            .where_eq("run_id", run_id)
            .limit(1),
    )?;
    let value = database::optional_text(&row, "value")?;
    if value.is_none() {
        // Distinguish absent source state from a refused, damaged/oversized value.
        let absent = database::rows(
            transaction,
            &select("workflow_runs")
                .columns(&["run_id"])
                .where_eq("run_id", run_id)
                .where_eq(column, V::Null)
                .limit(1),
        )?;
        if absent.is_empty() {
            return Err(WorkflowStoreError::InvalidData(format!(
                "workflow transform {column} is invalid or oversized"
            )));
        }
    }
    Ok(value)
}

pub fn activation_output_value(
    transaction: &dyn Database,
    run_id: &str,
    node_id: &str,
    generation: u64,
) -> Result<serde_json::Value, WorkflowStoreError> {
    let (activation_id, status) =
        activation_at_generation(transaction, run_id, node_id, generation)?
            .ok_or(DatabaseError::NoRow)?;
    if status != "completed" {
        return Err(DatabaseError::NoRow.into());
    }
    activation_output_value_by_identity(transaction, run_id, node_id, &activation_id)
}

pub fn activation_output_value_by_identity(
    transaction: &dyn Database,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
) -> Result<serde_json::Value, WorkflowStoreError> {
    let rows = database::rows(
        transaction,
        &select("workflow_activations")
            .project_as(
                bounded_text(identifier("output_id"), MAX_ID_BYTES as u64, V::Null),
                "output_id",
            )
            .where_eq("run_id", run_id)
            .where_eq("node_id", node_id)
            .where_eq("activation_id", activation_id)
            .where_eq("status", "completed")
            .limit(2),
    )?;
    let row = rows.first().ok_or(DatabaseError::NoRow)?;
    if rows.len() != 1 {
        return Err(WorkflowStoreError::InvalidData(
            "multiple completed workflow activations claim the same node generation".into(),
        ));
    }
    let output_id = database::text(row, "output_id")?;
    let output = WorkflowStore::inspect_exact_output_in(transaction, run_id, &output_id).map_err(
        |error| match error {
            WorkflowStoreError::StructuredDatabase(DatabaseError::NoRow) => {
                WorkflowStoreError::InvalidData(
                    "completed workflow activation output is missing, inconsistent, or oversized"
                        .into(),
                )
            }
            error => error,
        },
    )?;
    if output.node_id != node_id || output.activation_id != activation_id {
        return Err(WorkflowStoreError::InvalidData(
            "completed workflow activation output identity mismatch".into(),
        ));
    }
    Ok(output.value)
}

pub fn parallel_join_members(
    transaction: &dyn Database,
    run_id: &str,
    target: &bcode_workflow::NodeDefinition,
    generation: u64,
) -> Result<(serde_json::Value, serde_json::Value), WorkflowStoreError> {
    let member = |field: &str| {
        let exits = configured_node_ids(&target.configuration, field)?;
        let mut completed = None;
        for node_id in exits {
            WorkflowStore::current_run_graph_node_in_snapshot(transaction, run_id, &node_id)?
                .ok_or_else(|| {
                    WorkflowStoreError::InvalidData(format!(
                        "parallel join references missing run-graph member: {node_id}"
                    ))
                })?;
            let (revision, edge) =
                dependency_input_edge(transaction, run_id, &node_id, &target.id)?;
            if revision != 1 && edge.is_none() {
                return Err(WorkflowStoreError::InvalidData(format!(
                    "parallel join member has no dependency edge: {node_id}"
                )));
            }
            let retained = edge
                .as_ref()
                .map(|edge| {
                    run_graph::retained_edge_activation(transaction, run_id, revision, edge)
                })
                .transpose()?
                .flatten();
            let value = if let Some(identity) = retained {
                // An explicit retained source must not be treated as an absent alternative.
                Ok(activation_output_value_by_identity(
                    transaction,
                    run_id,
                    &node_id,
                    &identity,
                )?)
            } else {
                current_dependency_output_value(transaction, run_id, &node_id, generation, revision)
            };
            match value {
                Ok(value) => {
                    if completed.replace(value).is_some() {
                        return Err(WorkflowStoreError::InvalidData(format!(
                            "parallel join {} has ambiguous completed {field} members",
                            target.id
                        )));
                    }
                }
                Err(WorkflowStoreError::StructuredDatabase(DatabaseError::NoRow)) => {}
                Err(error) => return Err(error),
            }
        }
        completed.ok_or_else(|| {
            WorkflowStoreError::InvalidData(format!(
                "parallel join {} has no completed {field} member output",
                target.id
            ))
        })
    };
    Ok((member("left_exits")?, member("right_exits")?))
}

#[allow(clippy::too_many_lines)]
pub fn retained_source_output(
    transaction: &dyn Database,
    trigger: &ValidatedOutput,
    activation_id: &str,
) -> Result<ValidatedOutput, WorkflowStoreError> {
    stored_activation_output(
        transaction,
        &trigger.run_id,
        &trigger.node_id,
        activation_id,
    )
}

pub fn stored_activation_output(
    transaction: &dyn Database,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
) -> Result<ValidatedOutput, WorkflowStoreError> {
    let value = activation_output_value_by_identity(transaction, run_id, node_id, activation_id)?;
    let row = output_required_row(
        transaction,
        &select("workflow_activations")
            .project_as(
                bounded_text(identifier("output_id"), MAX_ID_BYTES as u64, V::Null),
                "output_id",
            )
            .where_eq("run_id", run_id)
            .where_eq("node_id", node_id)
            .where_eq("activation_id", activation_id)
            .limit(1),
    )?;
    let stored = WorkflowStore::inspect_exact_output_in(
        transaction,
        run_id,
        &database::text(&row, "output_id")?,
    )?;
    let output = ValidatedOutput {
        output_id: stored.output_id,
        run_id: run_id.to_owned(),
        node_id: node_id.to_owned(),
        activation_id: activation_id.to_owned(),
        schema_id: stored.schema_id,
        schema_version: stored.schema_version,
        value,
        artifact_reference: stored.artifact_reference,
        created_at_ms: stored.created_at_ms,
    };
    validate_output(&output)?;
    Ok(output)
}

pub fn dependency_input_value(
    transaction: &dyn Database,
    output: &ValidatedOutput,
    source_node_id: &str,
    target_node_id: &str,
    generation: u64,
) -> Result<serde_json::Value, WorkflowStoreError> {
    let (revision, edge) =
        dependency_input_edge(transaction, &output.run_id, source_node_id, target_node_id)?;
    let edge = edge.ok_or_else(|| {
        WorkflowStoreError::InvalidData(format!(
            "transform references a non-dependency node: {source_node_id}"
        ))
    })?;
    if let Some(identity) =
        run_graph::retained_edge_activation(transaction, &output.run_id, revision, &edge)?
    {
        return activation_output_value_by_identity(
            transaction,
            &output.run_id,
            source_node_id,
            &identity,
        );
    }
    if source_node_id == output.node_id {
        Ok(output.value.clone())
    } else {
        current_dependency_output_value(
            transaction,
            &output.run_id,
            source_node_id,
            generation,
            revision,
        )
    }
}

pub fn current_dependency_output_value(
    transaction: &dyn Database,
    run_id: &str,
    node_id: &str,
    generation: u64,
    revision: u64,
) -> Result<serde_json::Value, WorkflowStoreError> {
    if revision == 1 {
        return activation_output_value(transaction, run_id, node_id, generation);
    }
    let (activation_id, status) =
        activation_at_generation(transaction, run_id, node_id, generation)?
            .ok_or(DatabaseError::NoRow)?;
    if status != "completed" {
        return Err(DatabaseError::NoRow.into());
    }
    let admitted = database::rows(
        transaction,
        &select("workflow_activation_graph_bindings")
            .columns(&["graph_revision"])
            .where_eq("run_id", run_id)
            .where_eq("node_id", node_id)
            .where_eq("activation_id", activation_id.as_str())
            .limit(1),
    )?
    .first()
    .map(|row| database::unsigned(row, "graph_revision"))
    .transpose()?;
    if admitted != Some(revision) {
        return Err(WorkflowStoreError::InvalidData(
            "dependency input requires graph reconciliation".to_string(),
        ));
    }
    run_graph::reconciled_activation_exit(transaction, run_id, node_id, &activation_id)?;
    activation_output_value_by_identity(transaction, run_id, node_id, &activation_id)
}

pub fn dependency_input_edge(
    transaction: &dyn Database,
    run_id: &str,
    source_node_id: &str,
    target_node_id: &str,
) -> Result<(u64, Option<run_graph::RunGraphEdge>), WorkflowStoreError> {
    let revision = run_graph::graph_revision(transaction, run_id)?
        .ok_or_else(|| WorkflowStoreError::InvalidData("input graph is missing".to_string()))?;
    let mut cursor = None;
    let mut selected = None;
    loop {
        let edges = WorkflowStore::current_run_graph_edges_in_snapshot(
            transaction,
            run_id,
            revision,
            cursor,
            100,
            Some(run_graph::EdgeEndpoint::Target(target_node_id)),
        )?;
        if edges.is_empty() {
            break;
        }
        cursor = edges.last().map(|edge| edge.edge_id);
        for edge in edges {
            if edge.edge.from == source_node_id && selected.replace(edge).is_some() {
                return Err(WorkflowStoreError::InvalidData(
                    "successor input requires an unambiguous edge binding".to_string(),
                ));
            }
        }
    }
    Ok((revision, selected))
}

pub fn activation_input(
    transaction: &dyn Database,
    output: &ValidatedOutput,
    target: &bcode_workflow::NodeDefinition,
    generation: u64,
    edge: &bcode_workflow::EdgeDefinition,
) -> Result<serde_json::Value, WorkflowStoreError> {
    if edge.from != output.node_id || edge.to != target.id {
        return Err(WorkflowStoreError::InvalidData(
            "successor input edge does not match activation endpoints".to_owned(),
        ));
    }
    let input = if let Some(transform) = &edge.transform {
        transform
            .validate()
            .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
        let run_input_json =
            transform_run_json(transaction, &output.run_id, TransformRunField::Input)?;
        let authored_provenance_json = transform_run_json(
            transaction,
            &output.run_id,
            TransformRunField::AuthoredProvenance,
        )?;
        let run_input = run_input_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?
            .unwrap_or(serde_json::Value::Null);
        let configuration = authored_provenance_json
            .as_deref()
            .map(serde_json::from_str::<AuthoredWorkflowRunProvenance>)
            .transpose()?
            .map_or(serde_json::Value::Null, |provenance| {
                provenance.configuration
            });
        let join_members = (target.kind == bcode_workflow::NodeKind::Parallel)
            .then(|| parallel_join_members(transaction, &output.run_id, target, generation))
            .transpose()?;
        let mut dependency_values = Vec::new();
        for source in transform.referenced_sources() {
            let Some(node_id) =
                source.strip_prefix(bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_DEPENDENCY_PREFIX)
            else {
                continue;
            };
            let value =
                dependency_input_value(transaction, output, node_id, &target.id, generation)?;
            dependency_values.push((source, value));
        }
        let mut inputs = vec![
            bcode_workflow::WorkflowTransformInput {
                name: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_CURRENT,
                value: &output.value,
            },
            bcode_workflow::WorkflowTransformInput {
                name: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_STATE,
                value: &run_input,
            },
            bcode_workflow::WorkflowTransformInput {
                name: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_CONFIGURATION,
                value: &configuration,
            },
        ];
        for (name, value) in &dependency_values {
            inputs.push(bcode_workflow::WorkflowTransformInput { name, value });
        }
        if let Some((left, right)) = &join_members {
            inputs.extend([
                bcode_workflow::WorkflowTransformInput {
                    name: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_JOIN_LEFT,
                    value: left,
                },
                bcode_workflow::WorkflowTransformInput {
                    name: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_JOIN_RIGHT,
                    value: right,
                },
            ]);
        }
        transform
            .evaluate(&inputs)
            .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?
    } else if target.kind == bcode_workflow::NodeKind::Parallel {
        let (left, right) = parallel_join_members(transaction, &output.run_id, target, generation)?;
        serde_json::Value::Array(vec![left, right])
    } else {
        output.value.clone()
    };
    validate_json_schema(
        &format!("workflow activation input for node {}", target.id),
        &target.input.schema,
        &input,
    )
    .map_err(|error| {
        WorkflowStoreError::TargetInputValidation(Box::new(TargetInputValidationFailure {
            run_id: output.run_id.clone(),
            source_node_id: output.node_id.clone(),
            source_activation_id: output.activation_id.clone(),
            target_node_id: target.id.clone(),
            target_schema_id: target.input.type_name.clone(),
            message: error.to_string(),
        }))
    })?;
    Ok(input)
}

pub fn retained_activation_ready(
    transaction: &dyn Database,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
) -> Result<bool, WorkflowStoreError> {
    let row = output_required_row(
        transaction,
        &select("workflow_activations")
            .project_as(bounded_text(identifier("status"), 64, V::Null), "status")
            .where_eq("run_id", run_id)
            .where_eq("node_id", node_id)
            .where_eq("activation_id", activation_id)
            .limit(1),
    )?;
    let status = database::optional_text(&row, "status")?;
    match status.as_deref() {
        Some("completed") => {
            activation_output_value_by_identity(transaction, run_id, node_id, activation_id)?;
            Ok(true)
        }
        Some(
            "pending"
            | "running"
            | "waiting_input"
            | "waiting_approval"
            | "waiting_mutation_approval"
            | "failed"
            | "cancelled"
            | "skipped",
        ) => Ok(false),
        _ => Err(WorkflowStoreError::InvalidData(
            "retained activation status is invalid".to_string(),
        )),
    }
}

pub fn successor_dependencies_ready(
    transaction: &dyn Database,
    output: &ValidatedOutput,
    node_id: &str,
    generation: u64,
) -> Result<bool, WorkflowStoreError> {
    let mut cursor = None;
    let mut direct_ready = true;
    let mut conditional_ready = true;
    let mut reached_by_conditional = false;
    let revision = run_graph::graph_revision(transaction, &output.run_id)?
        .ok_or_else(|| WorkflowStoreError::InvalidData("readiness graph is missing".to_string()))?;
    loop {
        let edges = WorkflowStore::current_run_graph_edges_in_snapshot(
            transaction,
            &output.run_id,
            revision,
            cursor,
            100,
            Some(run_graph::EdgeEndpoint::Target(node_id)),
        )?;
        if edges.is_empty() {
            break;
        }
        cursor = edges.last().map(|edge| edge.edge_id);
        for record in edges {
            let retained = run_graph::retained_edge_activation(
                transaction,
                &output.run_id,
                revision,
                &record,
            )?;
            let edge = record.edge;
            let conditional = edge.from == output.node_id
                && matches!(edge.kind, bcode_workflow::EdgeKind::Conditional { .. });
            let direct = matches!(edge.kind, bcode_workflow::EdgeKind::Direct);
            if !conditional && !direct {
                continue;
            }
            let completed = if let Some(activation_id) = retained {
                retained_activation_ready(transaction, &output.run_id, &edge.from, &activation_id)?
            } else {
                let activation =
                    activation_at_generation(transaction, &output.run_id, &edge.from, generation)?;
                if let Some((activation_id, status)) = activation {
                    let ready = matches!(status.as_str(), "completed" | "skipped");
                    if ready && revision != 1 {
                        let admitted = output_activation_graph_revision(
                            transaction,
                            &output.run_id,
                            &edge.from,
                            &activation_id,
                        )?;
                        if admitted != Some(revision) {
                            return Err(WorkflowStoreError::InvalidData(
                                "successor dependency requires graph reconciliation".to_string(),
                            ));
                        }
                        run_graph::reconciled_activation_exit(
                            transaction,
                            &output.run_id,
                            &edge.from,
                            &activation_id,
                        )?;
                    }
                    ready
                } else {
                    false
                }
            };
            if conditional {
                reached_by_conditional = true;
                conditional_ready &= completed;
            } else {
                direct_ready &= completed;
            }
        }
    }
    Ok(if reached_by_conditional {
        conditional_ready
    } else {
        direct_ready
    })
}

#[allow(clippy::too_many_lines)]
pub fn materialize_direct_successors<F>(
    transaction: &dyn Database,
    output: &ValidatedOutput,
    fault: &F,
) -> Result<(Vec<NewActivation>, bool), WorkflowStoreError>
where
    F: WorkflowOutputFault + ?Sized,
{
    let generation = database::unsigned(
        &output_required_row(
            transaction,
            &select("workflow_activations")
                .columns(&["dependency_generation"])
                .where_eq("run_id", output.run_id.as_str())
                .where_eq("node_id", output.node_id.as_str())
                .where_eq("activation_id", output.activation_id.as_str())
                .limit(1),
        )?,
        "dependency_generation",
    )?;
    let graph_revision =
        run_graph::graph_revision(transaction, &output.run_id)?.ok_or_else(|| {
            WorkflowStoreError::InvalidData("settlement graph is missing".to_string())
        })?;
    let admitted = output_activation_graph_revision(
        transaction,
        &output.run_id,
        &output.node_id,
        &output.activation_id,
    )?
    .ok_or(DatabaseError::NoRow)?;
    let retained_from_prior_revision = graph_revision != 1 && admitted != graph_revision;
    let completed_node = run_graph::bound_activation_node(
        transaction,
        &output.run_id,
        &output.node_id,
        &output.activation_id,
    )?
    .ok_or_else(|| {
        WorkflowStoreError::InvalidData("completed activation executable is missing".to_string())
    })?;
    let mut completed_is_exit = run_graph::reconciled_activation_exit(
        transaction,
        &output.run_id,
        &output.node_id,
        &output.activation_id,
    )?;
    let mut targets = Vec::new();
    let mut selected_edges = std::collections::BTreeMap::new();
    let mut cursor = None;
    let mut has_outgoing = false;
    let mut all_conditional = true;
    loop {
        let edges = WorkflowStore::current_run_graph_edges_in_snapshot(
            transaction,
            &output.run_id,
            graph_revision,
            cursor,
            100,
            Some(run_graph::EdgeEndpoint::Source(&output.node_id)),
        )?;
        if edges.is_empty() {
            break;
        }
        cursor = edges.last().map(|edge| edge.edge_id);
        for record in edges {
            if retained_from_prior_revision
                && run_graph::retained_edge_activation(
                    transaction,
                    &output.run_id,
                    graph_revision,
                    &record,
                )?
                .as_deref()
                    != Some(output.activation_id.as_str())
            {
                return Err(WorkflowStoreError::InvalidData(
                    "retained successor settlement requires an exact activation binding"
                        .to_string(),
                ));
            }
            let edge = record.edge;
            has_outgoing = true;
            all_conditional &= matches!(edge.kind, bcode_workflow::EdgeKind::Conditional { .. });
            let selected = match &edge.kind {
                bcode_workflow::EdgeKind::Direct => true,
                bcode_workflow::EdgeKind::Conditional {
                    predicate,
                    expected,
                } => evaluate_predicate(predicate, &output.value)? == *expected,
                bcode_workflow::EdgeKind::Back { .. } | bcode_workflow::EdgeKind::Retry { .. } => {
                    false
                }
            };
            if selected {
                targets.push(edge.to.clone());
                if selected_edges
                    .insert(edge.to.clone(), (record.edge_id, record.revision, edge))
                    .is_some()
                {
                    return Err(WorkflowStoreError::InvalidData(
                        "successor input requires one selected edge binding".to_owned(),
                    ));
                }
            }
        }
    }
    if targets.is_empty() && has_outgoing && all_conditional {
        completed_is_exit = true;
    }
    for target in &targets {
        let node =
            WorkflowStore::current_run_graph_node_in_snapshot(transaction, &output.run_id, target)?
                .ok_or_else(|| {
                    WorkflowStoreError::InvalidData(format!(
                        "workflow successor node is missing from the run graph: {target}"
                    ))
                })?;
        if node.node.kind == bcode_workflow::NodeKind::Parallel {
            validate_parallel_run_topology(transaction, &output.run_id, &node.node)?;
        }
    }
    if completed_node.kind == bcode_workflow::NodeKind::Branch {
        let branch = completed_node;
        let expression: bcode_workflow::PredicateExpression =
            serde_json::from_value(branch.configuration.get("predicate").cloned().ok_or_else(
                || {
                    WorkflowStoreError::InvalidData(
                        "workflow branch configuration is missing predicate".to_string(),
                    )
                },
            )?)?;
        let selected = evaluate_predicate(&expression, &output.value)?;
        let selected_entries = configured_node_ids(
            &branch.configuration,
            if selected {
                "true_entries"
            } else {
                "false_entries"
            },
        )?;
        let skipped_nodes = configured_node_ids(
            &branch.configuration,
            if selected {
                "false_nodes"
            } else {
                "true_nodes"
            },
        )?;
        skip_branch_nodes(
            transaction,
            &output.run_id,
            generation,
            &skipped_nodes,
            output.created_at_ms,
        )?;
        targets.extend(selected_entries.iter().cloned());
        let decision = serde_json::to_string(&serde_json::json!({
            "predicate_version": bcode_workflow::WORKFLOW_PREDICATE_VERSION,
            "selected": selected, "predicate": expression,
            "selected_entries": selected_entries, "skipped_nodes": skipped_nodes,
        }))?;
        let changed = block_on(
            insert("workflow_decisions")
                .value("decision_id", format!("{}:branch", output.activation_id))
                .value("run_id", output.run_id.as_str())
                .value("node_id", output.node_id.as_str())
                .value("decision_type", "branch")
                .value("value_json", decision)
                .value(
                    "created_at_ms",
                    database::integer_value(output.created_at_ms)?,
                )
                .on_conflict_do_nothing(["decision_id"])
                .execute_count(transaction),
        )?;
        if changed != 1 {
            return Err(WorkflowStoreError::InvalidData(
                "branch decision did not insert exactly one row".into(),
            ));
        }
        fault.after_boundary(WorkflowOutputBoundary::BranchDecisionPersisted, output)?;
    }
    let activated = materialize_selected_successors(
        transaction,
        output,
        generation,
        graph_revision,
        targets,
        &selected_edges,
    )?;
    Ok((activated, completed_is_exit))
}

pub fn successor_input_source_identity(
    transaction: &dyn Database,
    run_id: &str,
    revision: u64,
    generation: u64,
    edge: &run_graph::RunGraphEdge,
) -> Result<Option<String>, WorkflowStoreError> {
    if matches!(
        edge.edge.kind,
        bcode_workflow::EdgeKind::Back { .. } | bcode_workflow::EdgeKind::Retry { .. }
    ) {
        return Ok(None);
    }
    if let Some(identity) =
        run_graph::retained_edge_activation(transaction, run_id, revision, edge)?
    {
        return Ok(Some(identity));
    }
    let Some((identity, status)) =
        activation_at_generation(transaction, run_id, &edge.edge.from, generation)?
    else {
        return Ok(None);
    };
    if status != "completed" {
        return Ok(None);
    }
    let admitted =
        output_activation_graph_revision(transaction, run_id, &edge.edge.from, &identity)?;
    if admitted != Some(revision) {
        return Err(WorkflowStoreError::InvalidData(
            "successor input source requires graph reconciliation".to_string(),
        ));
    }
    run_graph::reconciled_activation_exit(transaction, run_id, &edge.edge.from, &identity)?;
    Ok(Some(identity))
}

pub fn validate_retained_successor_inputs(
    transaction: &dyn Database,
    trigger: &ValidatedOutput,
    target: &bcode_workflow::NodeDefinition,
    generation: u64,
    input: &serde_json::Value,
) -> Result<(), WorkflowStoreError> {
    let revision = run_graph::graph_revision(transaction, &trigger.run_id)?
        .ok_or_else(|| WorkflowStoreError::InvalidData("successor graph is missing".to_string()))?;
    if revision == 1 {
        return Ok(());
    }
    let mut cursor = None;
    loop {
        let edges = WorkflowStore::current_run_graph_edges_in_snapshot(
            transaction,
            &trigger.run_id,
            revision,
            cursor,
            100,
            Some(run_graph::EdgeEndpoint::Target(&target.id)),
        )?;
        if edges.is_empty() {
            break;
        }
        cursor = edges.last().map(|edge| edge.edge_id);
        for record in edges {
            let Some(identity) = successor_input_source_identity(
                transaction,
                &trigger.run_id,
                revision,
                generation,
                &record,
            )?
            else {
                continue;
            };
            if !retained_activation_ready(
                transaction,
                &trigger.run_id,
                &record.edge.from,
                &identity,
            )? {
                continue;
            }
            let source = stored_activation_output(
                transaction,
                &trigger.run_id,
                &record.edge.from,
                &identity,
            )?;
            let selected = match &record.edge.kind {
                bcode_workflow::EdgeKind::Direct => true,
                bcode_workflow::EdgeKind::Conditional {
                    predicate,
                    expected,
                } => evaluate_predicate(predicate, &source.value)? == *expected,
                bcode_workflow::EdgeKind::Back { .. } | bcode_workflow::EdgeKind::Retry { .. } => {
                    false
                }
            };
            if selected
                && activation_input(transaction, &source, target, generation, &record.edge)?
                    != *input
            {
                return Err(WorkflowStoreError::InvalidData(
                    "retained successor requires consistent inputs across selected sources"
                        .to_string(),
                ));
            }
        }
    }
    Ok(())
}

pub fn successor_already_admitted(
    transaction: &dyn Database,
    run_id: &str,
    node_id: &str,
    generation: u64,
) -> Result<bool, WorkflowStoreError> {
    let Some((activation_id, _)) =
        activation_at_generation(transaction, run_id, node_id, generation).map_err(|error| {
            match error {
                WorkflowStoreError::InvalidData(message)
                    if message == "invalid workflow database column: status" =>
                {
                    WorkflowStoreError::InvalidData(format!(
                        "workflow node has unsupported activation status: {node_id}"
                    ))
                }
                other => other,
            }
        })?
    else {
        return Ok(false);
    };
    run_graph::reconciled_activation_exit(transaction, run_id, node_id, &activation_id)?;
    Ok(true)
}

pub fn materialize_selected_successors(
    transaction: &dyn Database,
    output: &ValidatedOutput,
    generation: u64,
    graph_revision: u64,
    mut targets: Vec<String>,
    selected_edges: &std::collections::BTreeMap<String, (u64, u64, bcode_workflow::EdgeDefinition)>,
) -> Result<Vec<NewActivation>, WorkflowStoreError> {
    targets.sort();
    targets.dedup();
    let mut activated = Vec::new();
    for node_id in targets {
        if successor_already_admitted(transaction, &output.run_id, &node_id, generation)? {
            continue;
        }
        if !successor_dependencies_ready(transaction, output, &node_id, generation)? {
            continue;
        }
        let target_record = WorkflowStore::current_run_graph_node_in_snapshot(
            transaction,
            &output.run_id,
            &node_id,
        )?
        .ok_or_else(|| {
            WorkflowStoreError::InvalidData(format!(
                "workflow successor node is missing from the run graph: {node_id}"
            ))
        })?;
        let target = &target_record.node;
        let (edge_id, edge_revision, edge) = selected_edges.get(&node_id).ok_or_else(|| {
            WorkflowStoreError::InvalidData("selected successor edge is missing".to_owned())
        })?;
        let retained_source = run_graph::retained_edge_activation(
            transaction,
            &output.run_id,
            graph_revision,
            &run_graph::RunGraphEdge {
                edge_id: *edge_id,
                revision: *edge_revision,
                edge: edge.clone(),
            },
        )?;
        let retained_output = retained_source
            .as_deref()
            .map(|identity| retained_source_output(transaction, output, identity))
            .transpose()?;
        let input_source = retained_output.as_ref().unwrap_or(output);
        let input = activation_input(transaction, input_source, target, generation, edge)?;
        validate_retained_successor_inputs(transaction, output, target, generation, &input)?;
        let activation = NewActivation {
            run_id: output.run_id.clone(),
            node_id: node_id.clone(),
            activation_id: activation_identity(&output.run_id, &node_id, generation),
            dependency_generation: generation,
            input: Some(input),
            created_at_ms: output.created_at_ms,
        };
        // Completion may materialize pending work while paused; attempt admission remains fenced.
        enforce_activation_materialization_limits(transaction, &activation, true)?;
        let status = activation_status_for_node(target);
        insert_successor_activation(transaction, &activation, status, target_record.revision)?;
        record_activation_graph_binding(
            transaction,
            &activation.run_id,
            &activation.node_id,
            &activation.activation_id,
            graph_revision,
        )?;
        append_successor_event(
            transaction,
            &activation,
            status,
            graph_revision,
            (*edge_id, *edge_revision),
            input_source,
        )?;
        activated.push(activation);
    }
    Ok(activated)
}

pub fn append_successor_event(
    transaction: &dyn Database,
    activation: &NewActivation,
    status: &str,
    graph_revision: u64,
    edge: (u64, u64),
    input_source: &ValidatedOutput,
) -> Result<(), WorkflowStoreError> {
    append_event_in(
        transaction,
        &activation.run_id,
        if status == "pending" {
            "activation_created"
        } else {
            "activation_waiting"
        },
        &serde_json::json!({
            "activation": activation, "status": status,
            "input_binding": {
                "version": 1, "graph_revision": graph_revision,
                "edge_id": edge.0, "edge_revision": edge.1,
                "source_activation_id": input_source.activation_id,
                "source_output_id": input_source.output_id,
            },
        })
        .to_string(),
        activation.created_at_ms,
    )
}

fn output_optional_text_value(value: Option<&str>) -> V {
    value.map_or(V::Null, |value| V::String(value.to_owned()))
}

fn output_required_row(
    db: &dyn Database,
    query: &SelectQuery<'_>,
) -> Result<Row, WorkflowStoreError> {
    database::rows(db, query)?
        .into_iter()
        .next()
        .ok_or_else(|| DatabaseError::NoRow.into())
}

fn output_metadata_query() -> SelectQuery<'static> {
    let mut query = select("workflow_outputs")
        .project(identifier("schema_version"))
        .project(identifier("created_at_ms"));
    for column in [
        "output_id",
        "run_id",
        "node_id",
        "activation_id",
        "schema_id",
    ] {
        query = query.project_as(
            bounded_text(identifier(column), MAX_ID_BYTES as u64, V::Null),
            column,
        );
    }
    query
        .project_as(
            bounded_text(
                identifier("artifact_reference"),
                MAX_ID_BYTES as u64,
                V::Null,
            ),
            "artifact_reference",
        )
        .project_as(
            bounded_text(identifier("checksum_sha256"), 64, V::Null),
            "checksum_sha256",
        )
}

fn output_summary(row: &Row) -> Result<WorkflowOutputSummary, WorkflowStoreError> {
    let summary = WorkflowOutputSummary {
        output_id: database::text(row, "output_id")?,
        run_id: database::text(row, "run_id")?,
        node_id: database::text(row, "node_id")?,
        activation_id: database::text(row, "activation_id")?,
        schema_id: database::text(row, "schema_id")?,
        schema_version: u32::try_from(database::unsigned(row, "schema_version")?).map_err(
            |_| WorkflowStoreError::InvalidData("output schema version exceeds u32".into()),
        )?,
        artifact_reference: database::optional_text(row, "artifact_reference")?,
        checksum_sha256: database::text(row, "checksum_sha256")?,
        created_at_ms: database::unsigned(row, "created_at_ms")?,
    };
    for (name, value) in [
        ("output_id", &summary.output_id),
        ("run_id", &summary.run_id),
        ("node_id", &summary.node_id),
        ("activation_id", &summary.activation_id),
        ("schema_id", &summary.schema_id),
    ] {
        validate_id(name, value)?;
    }
    if let Some(reference) = &summary.artifact_reference {
        validate_id("artifact_reference", reference)?;
    }
    if summary.schema_version == 0
        || summary.checksum_sha256.len() != 64
        || !summary
            .checksum_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(WorkflowStoreError::InvalidData(
            "invalid workflow output metadata".into(),
        ));
    }
    Ok(summary)
}

impl WorkflowStore {
    /// Read bounded canonical metadata through the caller's database snapshot lease.
    pub(super) fn output_summaries_after_in(
        db: &dyn Database,
        run_id: &str,
        after_output_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<WorkflowOutputSummary>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        if let Some(cursor) = after_output_id {
            validate_id("output_id", cursor)?;
        }
        let limit = usize::try_from(bounded_limit(limit)?)
            .map_err(|_| WorkflowStoreError::InvalidData("invalid output page limit".into()))?;
        let rows = database::rows(
            db,
            &output_metadata_query()
                .where_eq("run_id", run_id)
                .where_gt("output_id", after_output_id.unwrap_or(""))
                .sort("output_id", SortDirection::Asc)
                .limit(limit),
        )?;
        rows.iter()
            .map(|row| {
                let summary = output_summary(row)?;
                output_validate_artifact_reference(
                    db,
                    run_id,
                    &summary.output_id,
                    summary.artifact_reference.as_deref(),
                )?;
                Ok(summary)
            })
            .collect()
    }

    /// Read one bounded, checksum-verified canonical output through the caller's lease.
    pub(super) fn inspect_exact_output_in(
        db: &dyn Database,
        run_id: &str,
        output_id: &str,
    ) -> Result<bcode_workflow::WorkflowOutputInspection, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        validate_id("output_id", output_id)?;
        let row = output_required_row(
            db,
            &output_metadata_query()
                .project_as(
                    bounded_text(
                        identifier("value_json"),
                        MAX_INLINE_JSON_BYTES as u64,
                        V::Null,
                    ),
                    "value_json",
                )
                .where_eq("run_id", run_id)
                .where_eq("output_id", output_id)
                .limit(1),
        )?;
        let summary = output_summary(&row)?;
        output_validate_artifact_reference(
            db,
            run_id,
            output_id,
            summary.artifact_reference.as_deref(),
        )?;
        let value_json = database::text(&row, "value_json")?;
        if sha256_hex(value_json.as_bytes()) != summary.checksum_sha256 {
            return Err(WorkflowStoreError::InvalidData(
                "workflow output checksum mismatch".into(),
            ));
        }
        Ok(bcode_workflow::WorkflowOutputInspection {
            version: bcode_workflow::WORKFLOW_OUTPUT_INSPECTION_VERSION,
            output_id: summary.output_id,
            run_id: summary.run_id,
            node_id: summary.node_id,
            activation_id: summary.activation_id,
            schema_id: summary.schema_id,
            schema_version: summary.schema_version,
            value: serde_json::from_str(&value_json)?,
            artifact_reference: summary.artifact_reference,
            checksum_sha256: summary.checksum_sha256,
            created_at_ms: summary.created_at_ms,
        })
    }
}

fn output_validate_artifact_reference(
    db: &dyn Database,
    run_id: &str,
    output_id: &str,
    value: Option<&str>,
) -> Result<(), WorkflowStoreError> {
    if value.is_none()
        && database::rows(
            db,
            &select("workflow_outputs")
                .columns(&["output_id"])
                .where_eq("run_id", run_id)
                .where_eq("output_id", output_id)
                .where_eq("artifact_reference", V::Null)
                .limit(1),
        )?
        .is_empty()
    {
        return Err(WorkflowStoreError::InvalidData(
            "invalid or oversized output artifact reference".into(),
        ));
    }
    Ok(())
}

fn output_activation_graph_revision(
    db: &dyn Database,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
) -> Result<Option<u64>, WorkflowStoreError> {
    database::rows(
        db,
        &select("workflow_activation_graph_bindings")
            .columns(&["graph_revision"])
            .where_eq("run_id", run_id)
            .where_eq("node_id", node_id)
            .where_eq("activation_id", activation_id)
            .limit(1),
    )?
    .first()
    .map(|row| database::unsigned(row, "graph_revision"))
    .transpose()
}

fn insert_successor_activation(
    transaction: &dyn Database,
    activation: &NewActivation,
    status: &str,
    node_revision: u64,
) -> Result<(), WorkflowStoreError> {
    let node_id = &activation.node_id;
    let input_json = activation
        .input
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;
    let statement = insert("workflow_activations")
        .value("run_id", activation.run_id.as_str())
        .value("node_id", activation.node_id.as_str())
        .value("activation_id", activation.activation_id.as_str())
        .value(
            "dependency_generation",
            database::integer_value(activation.dependency_generation)?,
        )
        .value(
            "input_json",
            output_optional_text_value(input_json.as_deref()),
        )
        .value("status", status)
        .value(
            "created_at_ms",
            database::integer_value(activation.created_at_ms)?,
        )
        .value("node_revision", database::integer_value(node_revision)?);
    let changed = block_on(transaction.exec_insert_count(&statement))?;
    if changed != 1 {
        return Err(WorkflowStoreError::InvalidData(format!(
            "workflow successor insertion did not create exactly one activation: {node_id}"
        )));
    }
    Ok(())
}
