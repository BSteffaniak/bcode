//! Parallel and fan-out member settlement and canonical terminal outcomes.
use super::{
    ActiveAttemptCancellation, AttemptReconciliationRequest, BTreeSet, MAX_INLINE_JSON_BYTES,
    NewActivation, ValidatedOutput, WorkflowStore, WorkflowStoreError, activation_at_generation,
    activation_identity, activation_output_value, activation_status_at_generation,
    active_attempt_cancellation, active_attempt_cancellation_row, append_event_in, bounded_json,
    configured_node_ids, database, dependency_input_edge,
    enforce_activation_materialization_limits, fan_out_retirement_pending,
    insert_activation_bound_to_node, persist_validated_output_transaction,
    record_activation_graph_binding, run_graph, successor_already_admitted, validate_output,
    validate_output_schema,
};
use crate::database::block_on;
use switchy_database::query::{
    FilterableQuery, Projection, SortDirection, bounded_text, count, identifier, schema_predicate,
    select,
};
use switchy_database::schema::{SchemaComparison as C, SchemaExpression as E};
use switchy_database::{Database, DatabaseValue, Row};

#[derive(Debug)]
pub struct ParallelFailureSettlement {
    pub run_failed: bool,
    pub sibling_cancellations: Vec<ActiveAttemptCancellation>,
}

pub fn parallel_member_ids(
    join: &bcode_workflow::NodeDefinition,
    member_node_id: &str,
) -> Result<Vec<String>, WorkflowStoreError> {
    let left = configured_node_ids(&join.configuration, "left_exits")?;
    let right = configured_node_ids(&join.configuration, "right_exits")?;
    if left.iter().any(|member| member == member_node_id)
        && right.iter().any(|member| member == member_node_id)
    {
        return Err(WorkflowStoreError::InvalidData(format!(
            "parallel join {} ambiguously includes member {} on both sides",
            join.id, member_node_id
        )));
    }
    bcode_workflow::parallel_join_member_ids(join)
        .map(|members| members.into_iter().map(str::to_string).collect())
        .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))
}

pub fn validate_parallel_run_topology(
    transaction: &dyn Database,
    run_id: &str,
    node: &bcode_workflow::NodeDefinition,
) -> Result<(), WorkflowStoreError> {
    let members = bcode_workflow::parallel_join_member_ids(node)
        .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?;
    let mut missing = members.into_iter().collect::<BTreeSet<_>>();
    let revision = run_graph::graph_revision(transaction, run_id)?.ok_or_else(|| {
        WorkflowStoreError::InvalidData("parallel topology graph is missing".to_string())
    })?;
    let mut cursor = None;
    loop {
        let edges = WorkflowStore::current_run_graph_edges_in_snapshot(
            transaction,
            run_id,
            revision,
            cursor,
            100,
            Some(run_graph::EdgeEndpoint::Target(&node.id)),
        )?;
        if edges.is_empty() {
            break;
        }
        cursor = edges.last().map(|edge| edge.edge_id);
        for record in edges {
            if matches!(record.edge.kind, bcode_workflow::EdgeKind::Direct)
                && missing.contains(record.edge.from.as_str())
                && WorkflowStore::current_run_graph_node_in_snapshot(
                    transaction,
                    run_id,
                    &record.edge.from,
                )?
                .is_some()
            {
                missing.remove(record.edge.from.as_str());
            }
        }
    }
    if let Some(member) = missing.first() {
        return Err(WorkflowStoreError::InvalidData(format!(
            "parallel join member '{member}' must exist and have a direct edge to the join"
        )));
    }
    Ok(())
}

pub fn parallel_join_for_member(
    transaction: &dyn Database,
    run_id: &str,
    member_node_id: &str,
) -> Result<Option<bcode_workflow::NodeDefinition>, WorkflowStoreError> {
    let mut cursor = None;
    let mut selected: Option<bcode_workflow::NodeDefinition> = None;
    let revision = run_graph::graph_revision(transaction, run_id)?.ok_or_else(|| {
        WorkflowStoreError::InvalidData("parallel settlement graph is missing".to_string())
    })?;
    loop {
        let edges = WorkflowStore::current_run_graph_edges_in_snapshot(
            transaction,
            run_id,
            revision,
            cursor,
            100,
            Some(run_graph::EdgeEndpoint::Source(member_node_id)),
        )?;
        if edges.is_empty() {
            return Ok(selected);
        }
        cursor = edges.last().map(|edge| edge.edge_id);
        for record in edges {
            if !matches!(record.edge.kind, bcode_workflow::EdgeKind::Direct) {
                continue;
            }
            let node = WorkflowStore::current_run_graph_node_in_snapshot(
                transaction,
                run_id,
                &record.edge.to,
            )?
            .map(|record| record.node)
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData(format!(
                    "parallel settlement references missing run-graph node: {}",
                    record.edge.to
                ))
            })?;
            if node.kind == bcode_workflow::NodeKind::Parallel
                && parallel_member_ids(&node, member_node_id)?
                    .iter()
                    .any(|id| id == member_node_id)
                && selected
                    .as_ref()
                    .is_none_or(|previous| node.id < previous.id)
            {
                selected = Some(node);
            }
        }
    }
}

#[allow(clippy::too_many_lines)]
pub fn settle_parallel_failure(
    transaction: &dyn Database,
    run_id: &str,
    member_node_id: &str,
    activation_id: &str,
    member_failed: bool,
    settled_at_ms: u64,
) -> Result<Option<ParallelFailureSettlement>, WorkflowStoreError> {
    let graph_revision = run_graph::graph_revision(transaction, run_id)?.ok_or_else(|| {
        WorkflowStoreError::InvalidData("parallel settlement graph is missing".to_string())
    })?;
    if graph_revision != 1 {
        run_graph::reconciled_activation_exit(transaction, run_id, member_node_id, activation_id)?;
    }
    let generation = database::unsigned(
        &composition_required(
            transaction,
            select("workflow_activations")
                .columns(&["dependency_generation"])
                .where_eq("run_id", run_id)
                .where_eq("node_id", member_node_id)
                .where_eq("activation_id", activation_id),
        )?,
        "dependency_generation",
    )?;
    if let Some(join) = parallel_join_for_member(transaction, run_id, member_node_id)? {
        let members = parallel_member_ids(&join, member_node_id)?;
        for member in &members {
            if WorkflowStore::current_run_graph_node_in_snapshot(transaction, run_id, member)?
                .is_none()
            {
                return Err(WorkflowStoreError::InvalidData(format!(
                    "parallel settlement references missing member: {member}"
                )));
            }
            if graph_revision != 1 {
                let Some((member_activation, _)) =
                    activation_at_generation(transaction, run_id, member, generation)?
                else {
                    let record = WorkflowStore::current_run_graph_node_in_snapshot(
                        transaction,
                        run_id,
                        member,
                    )?
                    .ok_or_else(|| {
                        WorkflowStoreError::InvalidData("parallel member is missing".into())
                    })?;
                    // A dependency at this revision may not be ready in this generation
                    // (for example a nested join on a later repeat iteration). Earlier
                    // generations do not supply or invalidate its current activation.
                    if record.revision == graph_revision {
                        continue;
                    }
                    return Err(WorkflowStoreError::InvalidData(format!(
                        "parallel member activation is missing: {member}"
                    )));
                };
                let admitted = composition_one(
                    transaction,
                    select("workflow_activation_graph_bindings")
                        .columns(&["graph_revision"])
                        .where_eq("run_id", run_id)
                        .where_eq("node_id", member)
                        .where_eq("activation_id", &member_activation),
                )?
                .map(|row| database::unsigned(&row, "graph_revision"))
                .transpose()?;
                if admitted != Some(graph_revision) {
                    let (_, edge) = dependency_input_edge(transaction, run_id, member, &join.id)?;
                    let edge = edge.ok_or_else(|| {
                        WorkflowStoreError::InvalidData(
                            "retained parallel member edge is missing".to_string(),
                        )
                    })?;
                    let join_record = WorkflowStore::current_run_graph_node_in_snapshot(
                        transaction,
                        run_id,
                        &join.id,
                    )?
                    .ok_or_else(|| {
                        WorkflowStoreError::InvalidData("parallel join is missing".to_string())
                    })?;
                    let compatible_binding = E::exists(
                        "workflow_activation_graph_bindings",
                        E::And(vec![
                            same_column(
                                "workflow_activation_graph_bindings",
                                "run_id",
                                "workflow_activations",
                                "run_id",
                            ),
                            same_column(
                                "workflow_activation_graph_bindings",
                                "node_id",
                                "workflow_activations",
                                "node_id",
                            ),
                            same_column(
                                "workflow_activation_graph_bindings",
                                "activation_id",
                                "workflow_activations",
                                "activation_id",
                            ),
                            E::Compare(
                                Box::new(column(
                                    "workflow_activation_graph_bindings",
                                    "graph_revision",
                                )),
                                C::GreaterEqual,
                                Box::new(E::Integer(i64::try_from(join_record.revision).map_err(
                                    |_| {
                                        WorkflowStoreError::InvalidData(
                                            "graph revision overflow".into(),
                                        )
                                    },
                                )?)),
                            ),
                        ]),
                    );
                    let incompatible_join = composition_one(
                        transaction,
                        select("workflow_activations")
                            .columns(&["activation_id"])
                            .where_eq("run_id", run_id)
                            .where_eq("node_id", &join.id)
                            .filter(Box::new(schema_predicate(&not_exists(compatible_binding))?)),
                    )?
                    .is_some();
                    if admitted.is_none()
                        || incompatible_join
                        || run_graph::retained_edge_activation(
                            transaction,
                            run_id,
                            graph_revision,
                            &edge,
                        )?
                        .as_deref()
                            != Some(member_activation.as_str())
                    {
                        return Err(WorkflowStoreError::InvalidData(
                            "parallel member requires exact retention and an unchanged join"
                                .to_string(),
                        ));
                    }
                }
                run_graph::reconciled_activation_exit(
                    transaction,
                    run_id,
                    member,
                    &member_activation,
                )?;
            }
        }
        validate_parallel_run_topology(transaction, run_id, &join)?;
        let policy: bcode_workflow::ParallelFailurePolicy = serde_json::from_value(
            join.configuration
                .get("failure_policy")
                .cloned()
                .unwrap_or_else(|| serde_json::json!("wait_all")),
        )?;
        let mut outcomes = Vec::with_capacity(members.len());
        let mut all_terminal = true;
        let mut has_failure = false;
        let mut sibling_cancellations = Vec::new();
        let statuses = members
            .iter()
            .map(|member| activation_status_at_generation(transaction, run_id, member, generation))
            .collect::<Result<Vec<_>, _>>()?;
        for (member, status) in members.into_iter().zip(statuses) {
            let terminal = status.as_deref().is_some_and(|status| {
                matches!(status, "completed" | "skipped" | "failed" | "cancelled")
            });
            all_terminal &= terminal;
            has_failure |= status
                .as_deref()
                .is_some_and(|status| matches!(status, "failed" | "cancelled"));
            if member_failed
                && policy == bcode_workflow::ParallelFailurePolicy::FailFast
                && member != member_node_id
                && !terminal
            {
                block_on(
                    transaction.exec_update_count(
                        &(transaction
                            .update("workflow_activations")
                            .value("status", DatabaseValue::from("cancelled"))
                            .where_eq("run_id", DatabaseValue::from(run_id))
                            .where_eq("node_id", DatabaseValue::from(&member))
                            .where_eq("dependency_generation", DatabaseValue::from(generation))
                            .where_in(
                                "status",
                                vec![
                                    DatabaseValue::from("pending"),
                                    DatabaseValue::from("waiting_input"),
                                    DatabaseValue::from("waiting_approval"),
                                    DatabaseValue::from("waiting_mutation_approval"),
                                ],
                            )
                            .where_eq("output_id", DatabaseValue::Null)),
                    ),
                )?;
                let predicate = E::exists(
                    "workflow_activations",
                    E::And(vec![
                        same_column(
                            "workflow_activations",
                            "run_id",
                            "workflow_attempts",
                            "run_id",
                        ),
                        same_column(
                            "workflow_activations",
                            "node_id",
                            "workflow_attempts",
                            "node_id",
                        ),
                        same_column(
                            "workflow_activations",
                            "activation_id",
                            "workflow_attempts",
                            "activation_id",
                        ),
                        E::Compare(
                            Box::new(column("workflow_activations", "dependency_generation")),
                            C::Equal,
                            Box::new(E::Integer(i64::try_from(generation).map_err(|_| {
                                WorkflowStoreError::InvalidData(
                                    "dependency generation overflow".into(),
                                )
                            })?)),
                        ),
                    ]),
                );
                let attempt = composition_one(
                    transaction,
                    attempt_query()
                        .where_eq("run_id", run_id)
                        .where_eq("node_id", &member)
                        .where_in("status", active_statuses())
                        .filter(Box::new(schema_predicate(&predicate)?)),
                )?
                .as_ref()
                .map(|row| {
                    super::controls::controls_validate_cancellation_receipt(row)
                        .and_then(active_attempt_cancellation_row)
                })
                .transpose()?
                .map(active_attempt_cancellation)
                .transpose()?;
                if let Some(attempt) = attempt {
                    block_on(
                        transaction.exec_update_count(
                            &(transaction
                                .update("workflow_attempts")
                                .value("status", DatabaseValue::from("sibling_cancelling"))
                                .where_eq(
                                    "dispatch_identity",
                                    DatabaseValue::from(&attempt.dispatch_identity),
                                )
                                .where_in(
                                    "status",
                                    vec![
                                        DatabaseValue::from("prepared"),
                                        DatabaseValue::from("admitted"),
                                        DatabaseValue::from("running"),
                                    ],
                                )),
                        ),
                    )?;
                    append_event_in(
                        transaction,
                        run_id,
                        "parallel_sibling_cancellation_requested",
                        &serde_json::json!({
                            "join_node_id": join.id,
                            "failed_node_id": member_node_id,
                            "sibling_node_id": member,
                            "dispatch_identity": attempt.dispatch_identity,
                            "generation": generation,
                        })
                        .to_string(),
                        settled_at_ms,
                    )?;
                    sibling_cancellations.push(attempt);
                }
            }
            outcomes.push(serde_json::json!({"node_id": member, "status": status}));
        }
        if policy == bcode_workflow::ParallelFailurePolicy::CollectOutcomes {
            if all_terminal {
                materialize_terminal_outcomes(
                    transaction,
                    run_id,
                    &join,
                    generation,
                    settled_at_ms,
                )?;
            }
            return Ok(Some(ParallelFailureSettlement {
                run_failed: false,
                sibling_cancellations: Vec::new(),
            }));
        }
        // Skipped alternatives are valid, but each side still needs a completed
        // result. All-terminal is not evidence that a tuple can be constructed.
        if all_terminal {
            for field in ["left_exits", "right_exits"] {
                let exits = configured_node_ids(&join.configuration, field)?;
                let mut completed = false;
                for exit in exits {
                    completed |=
                        activation_status_at_generation(transaction, run_id, &exit, generation)?
                            .as_deref()
                            == Some("completed");
                }
                has_failure |= !completed;
            }
        }
        let should_fail = match policy {
            bcode_workflow::ParallelFailurePolicy::CollectOutcomes => unreachable!("handled above"),
            bcode_workflow::ParallelFailurePolicy::WaitAll => all_terminal && has_failure,
            bcode_workflow::ParallelFailurePolicy::FailFast => {
                member_failed || (all_terminal && has_failure)
            }
        };
        if should_fail {
            let (decision_type, decision_suffix) = match policy {
                bcode_workflow::ParallelFailurePolicy::CollectOutcomes => {
                    unreachable!("handled above")
                }
                bcode_workflow::ParallelFailurePolicy::WaitAll => {
                    ("parallel_wait_all", "parallel-wait-all")
                }
                bcode_workflow::ParallelFailurePolicy::FailFast => {
                    ("parallel_fail_fast", "parallel-fail-fast")
                }
            };
            let value = serde_json::json!({
                "policy": policy,
                "generation": generation,
                "members": outcomes,
                "outcome": "failed",
                "sibling_cancellations": sibling_cancellations
                    .iter()
                    .map(|attempt| attempt.dispatch_identity.as_str())
                    .collect::<Vec<_>>(),
            });
            block_on(
                transaction
                    .insert("workflow_decisions")
                    .value(
                        "decision_id",
                        format!("{run_id}:{}:{generation}:{decision_suffix}", join.id),
                    )
                    .value("run_id", run_id)
                    .value("node_id", &join.id)
                    .value("decision_type", decision_type)
                    .value("value_json", value.to_string())
                    .value("created_at_ms", database::integer_value(settled_at_ms)?)
                    .on_conflict_do_nothing(["decision_id"])
                    .execute_count(transaction),
            )?;
            block_on(
                transaction.exec_update_count(
                    &(transaction
                        .update("workflow_runs")
                        .value("status", DatabaseValue::from("failed"))
                        .value("updated_at_ms", DatabaseValue::from(settled_at_ms))
                        .where_eq("run_id", DatabaseValue::from(run_id))
                        .where_eq("status", DatabaseValue::from("running"))),
                ),
            )?;
            append_event_in(
                transaction,
                run_id,
                "parallel_join_failed",
                &serde_json::json!({"join_node_id": join.id, "decision": value}).to_string(),
                settled_at_ms,
            )?;
            return Ok(Some(ParallelFailureSettlement {
                run_failed: true,
                sibling_cancellations,
            }));
        }
        return Ok(Some(ParallelFailureSettlement {
            run_failed: false,
            sibling_cancellations: Vec::new(),
        }));
    }
    Ok(None)
}

pub fn materialize_terminal_outcomes(
    transaction: &dyn Database,
    run_id: &str,
    join: &bcode_workflow::NodeDefinition,
    generation: u64,
    settled_at_ms: u64,
) -> Result<(), WorkflowStoreError> {
    let row = composition_required(
        transaction,
        select("workflow_runs")
            .columns(&["status", "cancellation_requested_at_ms"])
            .where_eq("run_id", run_id),
    )?;
    let permits_collection =
        matches!(
            database::text(&row, "status")?.as_str(),
            "running" | "paused"
        ) && database::optional_unsigned(&row, "cancellation_requested_at_ms")?.is_none();
    if !permits_collection || successor_already_admitted(transaction, run_id, &join.id, generation)?
    {
        return Ok(());
    }
    let mut outcomes = serde_json::Map::new();
    for member in bcode_workflow::parallel_join_member_ids(join)
        .map_err(|error| WorkflowStoreError::InvalidData(error.to_string()))?
    {
        let status = activation_status_at_generation(transaction, run_id, member, generation)?;
        // A graph-edit cancellation is retirement, not an independent worker outcome.
        // A successful sibling can arrive after retirement settled, so checking only
        // the currently settling attempt would admit a controller for the old graph.
        if status.as_deref() == Some("cancelled")
            && composition_publication_cancelled(transaction, run_id, member, generation)?
        {
            return Ok(());
        }
        let value = match status.as_deref() {
            Some("completed") => serde_json::json!({"status":"completed", "value":
                activation_output_value(transaction, run_id, member, generation)?}),
            Some(status @ ("failed" | "cancelled" | "skipped")) => {
                serde_json::json!({"status":status})
            }
            _ => {
                return Err(WorkflowStoreError::InvalidData(
                    "outcome collection requires terminal members".into(),
                ));
            }
        };
        outcomes.insert(member.to_string(), value);
    }
    let record = WorkflowStore::current_run_graph_node_in_snapshot(transaction, run_id, &join.id)?
        .ok_or_else(|| WorkflowStoreError::InvalidData("outcome join is missing".into()))?;
    let activation = NewActivation {
        run_id: run_id.to_string(),
        node_id: join.id.clone(),
        activation_id: activation_identity(run_id, &join.id, generation),
        dependency_generation: generation,
        input: Some(serde_json::Value::Object(outcomes)),
        created_at_ms: settled_at_ms,
    };
    enforce_activation_materialization_limits(transaction, &activation, true)?;
    insert_activation_bound_to_node(transaction, &activation, "pending", join, record.revision)?;
    let revision = run_graph::graph_revision(transaction, run_id)?
        .ok_or_else(|| WorkflowStoreError::InvalidData("outcome graph is missing".into()))?;
    record_activation_graph_binding(
        transaction,
        run_id,
        &join.id,
        &activation.activation_id,
        revision,
    )?;
    Ok(())
}

pub fn fan_out_member_controller(
    transaction: &dyn Database,
    request: &AttemptReconciliationRequest,
) -> Result<Option<(String, String, u32)>, WorkflowStoreError> {
    composition_one(
        transaction,
        select("workflow_fan_out_members")
            .columns(&[
                "controller_node_id",
                "controller_activation_id",
                "member_index",
            ])
            .where_eq("run_id", &request.run_id)
            .where_eq("member_node_id", &request.node_id)
            .where_eq("member_activation_id", &request.activation_id),
    )?
    .map(|row| {
        Ok((
            database::text(&row, "controller_node_id")?,
            database::text(&row, "controller_activation_id")?,
            u32::try_from(database::unsigned(&row, "member_index")?).map_err(|_| {
                WorkflowStoreError::InvalidData("fan-out member index overflow".into())
            })?,
        ))
    })
    .transpose()
}

#[allow(clippy::too_many_lines)]
pub fn settle_fan_out_member_failure(
    transaction: &dyn Database,
    request: &AttemptReconciliationRequest,
    message: &str,
    settled_at_ms: u64,
) -> Result<Vec<ActiveAttemptCancellation>, WorkflowStoreError> {
    let (controller_node_id, controller_activation_id, member_index) =
        fan_out_member_controller(transaction, request)?.ok_or_else(|| {
            WorkflowStoreError::InvalidData("fan-out member controller is missing".to_string())
        })?;
    let controller = run_graph::initial_activation_node(
        transaction,
        &request.run_id,
        &controller_node_id,
        &controller_activation_id,
    )?
    .ok_or_else(|| {
        WorkflowStoreError::InvalidData("fan-out controller run-graph node is missing".to_string())
    })?;
    let configuration: bcode_workflow::WorkflowFanOutConfiguration =
        serde_json::from_value(controller.configuration)?;
    let changed = block_on(
        transaction.exec_update_count(
            &(transaction
                .update("workflow_fan_out_members")
                .value("status", DatabaseValue::from("failed"))
                .value("terminal_at_ms", DatabaseValue::from(settled_at_ms))
                .where_eq("run_id", DatabaseValue::from(&request.run_id))
                .where_eq("member_node_id", DatabaseValue::from(&request.node_id))
                .where_eq(
                    "member_activation_id",
                    DatabaseValue::from(&request.activation_id),
                )
                .where_eq("status", DatabaseValue::from("running"))),
        ),
    )?;
    if changed != 1 {
        return Err(WorkflowStoreError::InvalidData(
            "fan-out member is not running or already settled".to_string(),
        ));
    }
    let changed = block_on(
        transaction.exec_update_count(
            &(transaction
                .update("workflow_activations")
                .value("status", DatabaseValue::from("failed"))
                .where_eq("run_id", DatabaseValue::from(&request.run_id))
                .where_eq("node_id", DatabaseValue::from(&request.node_id))
                .where_eq("activation_id", DatabaseValue::from(&request.activation_id))
                .where_eq("status", DatabaseValue::from("running"))),
        ),
    )?;
    if changed != 1 {
        return Err(WorkflowStoreError::InvalidData(
            "fan-out member activation is not running".to_string(),
        ));
    }
    if fan_out_retirement_pending(transaction, &request.run_id, &request.activation_id)? {
        append_event_in(transaction, &request.run_id, "fan_out_member_failed",
            &serde_json::json!({"controller_activation_id": controller_activation_id, "member_activation_id": request.activation_id, "message": message}).to_string(), settled_at_ms)?;
        return Ok(Vec::new());
    }
    let mut sibling_cancellations = Vec::new();
    if configuration.failure_policy == bcode_workflow::ParallelFailurePolicy::FailFast {
        let query = composition_receipt_projection(
            select("workflow_attempts")
                .projections(
                    [
                        "run_id",
                        "node_id",
                        "activation_id",
                        "attempt",
                        "dispatch_identity",
                    ]
                    .into_iter()
                    .map(|name| {
                        Projection::new(switchy_database::query::qualified_column(
                            "workflow_attempts",
                            name,
                        ))
                        .alias(name)
                    })
                    .collect(),
                )
                .joins(vec![switchy_database::query::join_columns(
                    "workflow_fan_out_members",
                    switchy_database::query::qualified_column(
                        "workflow_fan_out_members",
                        "member_activation_id",
                    ),
                    switchy_database::query::qualified_column("workflow_attempts", "activation_id"),
                    false,
                )])
                .filter(Box::new(switchy_database::query::where_eq_expression(
                    switchy_database::query::qualified_column(
                        "workflow_fan_out_members",
                        "controller_activation_id",
                    ),
                    DatabaseValue::from(&controller_activation_id),
                )))
                .filter(Box::new(switchy_database::query::where_not_eq_expression(
                    switchy_database::query::qualified_column(
                        "workflow_attempts",
                        "dispatch_identity",
                    ),
                    DatabaseValue::from(&request.dispatch_identity),
                )))
                .filter(Box::new(schema_predicate(&E::In(
                    Box::new(column("workflow_attempts", "status")),
                    [
                        "prepared",
                        "admitted",
                        "running",
                        "cancelling",
                        "sibling_cancelling",
                    ]
                    .into_iter()
                    .map(|status| E::Text(status.into()))
                    .collect(),
                ))?))
                .sorts(vec![switchy_database::query::sort(
                    switchy_database::query::qualified_column(
                        "workflow_fan_out_members",
                        "member_index",
                    ),
                    SortDirection::Asc,
                )]),
        );
        sibling_cancellations = database::rows(transaction, &query)?
            .iter()
            .map(|row| {
                super::controls::controls_validate_cancellation_receipt(row)
                    .and_then(active_attempt_cancellation_row)
            })
            .map(|row| row.and_then(active_attempt_cancellation))
            .collect::<Result<Vec<_>, _>>()?;
        block_on(
            transaction.exec_update_count(
                &(transaction
                    .update("workflow_attempts")
                    .value("status", "sibling_cancelling")
                    .where_eq("run_id", &request.run_id)
                    .where_not_eq("dispatch_identity", &request.dispatch_identity)
                    .where_in(
                        "status",
                        vec![
                            DatabaseValue::from("prepared"),
                            DatabaseValue::from("admitted"),
                            DatabaseValue::from("running"),
                        ],
                    )
                    .filter(Box::new(schema_predicate(&fan_out_activation_members(
                        "workflow_attempts",
                        &controller_activation_id,
                        None,
                    ))?))),
            ),
        )?;
        block_on(
            transaction.exec_update_count(
                &(transaction
                    .update("workflow_fan_out_members")
                    .value("status", DatabaseValue::from("cancelled"))
                    .value("terminal_at_ms", DatabaseValue::from(settled_at_ms))
                    .where_eq("run_id", DatabaseValue::from(&request.run_id))
                    .where_eq(
                        "controller_activation_id",
                        DatabaseValue::from(&controller_activation_id),
                    )
                    .where_in(
                        "status",
                        vec![
                            DatabaseValue::from("pending"),
                            DatabaseValue::from("waiting"),
                        ],
                    )),
            ),
        )?;
        block_on(
            transaction.exec_update_count(
                &(transaction
                    .update("workflow_activations")
                    .value("status", "cancelled")
                    .where_eq("run_id", &request.run_id)
                    .where_in(
                        "status",
                        vec![
                            DatabaseValue::from("pending"),
                            DatabaseValue::from("waiting"),
                        ],
                    )
                    .filter(Box::new(schema_predicate(&fan_out_activation_members(
                        "workflow_activations",
                        &controller_activation_id,
                        Some("cancelled"),
                    ))?))),
            ),
        )?;
    }
    if configuration.failure_policy == bcode_workflow::ParallelFailurePolicy::WaitAll {
        let active: u32 =
            fan_out_active_count(transaction, &request.run_id, &controller_activation_id)?;
        if active < configuration.max_concurrency {
            promote_fan_out_waiting(
                transaction,
                &request.run_id,
                &controller_activation_id,
                configuration.max_concurrency - active,
            )?;
            block_on(
                transaction.exec_update_count(
                    &(transaction
                        .update("workflow_activations")
                        .value("status", "pending")
                        .where_eq("run_id", &request.run_id)
                        .where_in("status", vec![DatabaseValue::from("waiting")])
                        .filter(Box::new(schema_predicate(&fan_out_activation_members(
                            "workflow_activations",
                            &controller_activation_id,
                            Some("pending"),
                        ))?))),
                ),
            )?;
        }
        let active_or_waiting: bool = composition_one(
            transaction,
            select("workflow_fan_out_members")
                .columns(&["member_index"])
                .where_eq("run_id", &request.run_id)
                .where_eq("controller_activation_id", &controller_activation_id)
                .where_in(
                    "status",
                    vec![
                        DatabaseValue::from("pending"),
                        DatabaseValue::from("running"),
                        DatabaseValue::from("waiting"),
                    ],
                ),
        )
        .map(|row| row.is_some())?;
        if active_or_waiting {
            append_event_in(
                transaction,
                &request.run_id,
                "fan_out_member_failed",
                &serde_json::json!({
                    "controller_activation_id": controller_activation_id,
                    "member_index": member_index,
                    "member_activation_id": request.activation_id,
                    "message": message,
                })
                .to_string(),
                settled_at_ms,
            )?;
            return Ok(Vec::new());
        }
    }
    block_on(
        transaction.exec_update_count(
            &(transaction
                .update("workflow_activations")
                .value("status", DatabaseValue::from("failed"))
                .where_eq("run_id", DatabaseValue::from(&request.run_id))
                .where_eq("node_id", DatabaseValue::from(&controller_node_id))
                .where_eq(
                    "activation_id",
                    DatabaseValue::from(&controller_activation_id),
                )
                .where_eq("status", DatabaseValue::from("running"))),
        ),
    )?;
    block_on(
        transaction.exec_update_count(
            &(transaction
                .update("workflow_runs")
                .value("status", DatabaseValue::from("failed"))
                .value("updated_at_ms", DatabaseValue::from(settled_at_ms))
                .where_eq("run_id", DatabaseValue::from(&request.run_id))
                .where_eq("status", DatabaseValue::from("running"))),
        ),
    )?;
    append_event_in(
        transaction,
        &request.run_id,
        "fan_out_member_failed",
        &serde_json::json!({
            "controller_activation_id": controller_activation_id,
            "member_index": member_index,
            "member_activation_id": request.activation_id,
            "message": message,
        })
        .to_string(),
        settled_at_ms,
    )?;
    Ok(sibling_cancellations)
}

#[allow(clippy::too_many_lines)]
pub fn settle_fan_out_member_success(
    transaction: &dyn Database,
    request: &AttemptReconciliationRequest,
    output: &ValidatedOutput,
    settled_at_ms: u64,
) -> Result<(), WorkflowStoreError> {
    let (controller_node_id, controller_activation_id, member_index) =
        fan_out_member_controller(transaction, request)?.ok_or_else(|| {
            WorkflowStoreError::InvalidData("fan-out member controller is missing".to_string())
        })?;
    let controller = run_graph::initial_activation_node(
        transaction,
        &request.run_id,
        &controller_node_id,
        &controller_activation_id,
    )?
    .ok_or_else(|| {
        WorkflowStoreError::InvalidData("fan-out controller run-graph node is missing".to_string())
    })?;
    let configuration: bcode_workflow::WorkflowFanOutConfiguration =
        serde_json::from_value(controller.configuration.clone())?;
    validate_output(output)?;
    validate_output_schema(output, &configuration.member_node.output)?;
    let output_json = bounded_json("fan-out member output", &output.value)?;
    let changed = block_on(
        transaction.exec_update_count(
            &(transaction
                .update("workflow_fan_out_members")
                .value("status", DatabaseValue::from("completed"))
                .value("output_json", DatabaseValue::from(&output_json))
                .value("terminal_at_ms", DatabaseValue::from(settled_at_ms))
                .where_eq("run_id", DatabaseValue::from(&request.run_id))
                .where_eq("member_node_id", DatabaseValue::from(&request.node_id))
                .where_eq(
                    "member_activation_id",
                    DatabaseValue::from(&request.activation_id),
                )
                .where_eq("status", DatabaseValue::from("running"))),
        ),
    )?;
    if changed != 1 {
        return Err(WorkflowStoreError::InvalidData(
            "fan-out member is not running or already settled".to_string(),
        ));
    }
    let changed = block_on(
        transaction.exec_update_count(
            &(transaction
                .update("workflow_activations")
                .value("status", DatabaseValue::from("completed"))
                .where_eq("run_id", DatabaseValue::from(&request.run_id))
                .where_eq("node_id", DatabaseValue::from(&request.node_id))
                .where_eq("activation_id", DatabaseValue::from(&request.activation_id))
                .where_eq("status", DatabaseValue::from("running"))),
        ),
    )?;
    if changed != 1 {
        return Err(WorkflowStoreError::InvalidData(
            "fan-out member activation is not running".to_string(),
        ));
    }
    append_event_in(
        transaction,
        &request.run_id,
        "fan_out_member_completed",
        &serde_json::json!({
            "controller_activation_id": controller_activation_id,
            "member_index": member_index,
            "member_activation_id": request.activation_id,
        })
        .to_string(),
        settled_at_ms,
    )?;
    if fan_out_retirement_pending(transaction, &request.run_id, &request.activation_id)? {
        // Preserve the owner's result, but leave graph reconciliation to the publisher.
        // Do not admit queued members or aggregate a controller being retired.
        return Ok(());
    }
    let running: u32 =
        fan_out_active_count(transaction, &request.run_id, &controller_activation_id)?;
    if running < configuration.max_concurrency {
        promote_fan_out_waiting(
            transaction,
            &request.run_id,
            &controller_activation_id,
            configuration.max_concurrency - running,
        )?;
        block_on(
            transaction.exec_update_count(
                &(transaction
                    .update("workflow_activations")
                    .value("status", "pending")
                    .where_eq("run_id", &request.run_id)
                    .where_in("status", vec![DatabaseValue::from("waiting")])
                    .filter(Box::new(schema_predicate(&fan_out_activation_members(
                        "workflow_activations",
                        &controller_activation_id,
                        Some("pending"),
                    ))?))),
            ),
        )?;
    }
    let incomplete: bool = composition_one(
        transaction,
        select("workflow_fan_out_members")
            .columns(&["member_index"])
            .where_eq("run_id", &request.run_id)
            .where_eq("controller_activation_id", &controller_activation_id)
            .where_not_in(
                "status",
                vec![
                    DatabaseValue::from("completed"),
                    DatabaseValue::from("failed"),
                ],
            ),
    )
    .map(|row| row.is_some())?;
    if incomplete {
        return Ok(());
    }
    let failed: bool = composition_one(
        transaction,
        select("workflow_fan_out_members")
            .columns(&["member_index"])
            .where_eq("run_id", &request.run_id)
            .where_eq("controller_activation_id", &controller_activation_id)
            .where_eq("status", "failed"),
    )
    .map(|row| row.is_some())?;
    if failed {
        block_on(
            transaction.exec_update_count(
                &(transaction
                    .update("workflow_activations")
                    .value("status", DatabaseValue::from("failed"))
                    .where_eq("run_id", DatabaseValue::from(&request.run_id))
                    .where_eq("node_id", DatabaseValue::from(&controller_node_id))
                    .where_eq(
                        "activation_id",
                        DatabaseValue::from(&controller_activation_id),
                    )
                    .where_eq("status", DatabaseValue::from("running"))),
            ),
        )?;
        block_on(
            transaction.exec_update_count(
                &(transaction
                    .update("workflow_runs")
                    .value("status", DatabaseValue::from("failed"))
                    .value("updated_at_ms", DatabaseValue::from(settled_at_ms))
                    .where_eq("run_id", DatabaseValue::from(&request.run_id))
                    .where_eq("status", DatabaseValue::from("running"))),
            ),
        )?;
        append_event_in(
            transaction,
            &request.run_id,
            "fan_out_failed",
            &serde_json::json!({"controller_activation_id": controller_activation_id}).to_string(),
            settled_at_ms,
        )?;
        return Ok(());
    }
    let members = database::rows(transaction,&select("workflow_fan_out_members").projections(vec![Projection::new(identifier("member_index")),Projection::new(bounded_text(identifier("output_json"),MAX_INLINE_JSON_BYTES as u64,DatabaseValue::Null)).alias("output_json")]).where_eq("run_id",&request.run_id).where_eq("controller_activation_id",&controller_activation_id).sort("member_index",SortDirection::Asc))?.iter().map(|row| {
        let index = u32::try_from(database::unsigned(row,"member_index")?).map_err(|_| WorkflowStoreError::InvalidData("fan-out member index overflow".into()))?;
        let json = database::text(row,"output_json")?;
        Ok(serde_json::json!({"index":index,"value":serde_json::from_str::<serde_json::Value>(&json)?}))
    }).collect::<Result<Vec<_>,WorkflowStoreError>>()?;
    let controller_output = ValidatedOutput {
        output_id: format!("{controller_activation_id}:fan-out-output"),
        run_id: request.run_id.clone(),
        node_id: controller_node_id,
        activation_id: controller_activation_id,
        schema_id: controller.output.type_name,
        schema_version: 1,
        value: serde_json::json!({
            "version": bcode_workflow::WORKFLOW_FAN_OUT_RESULT_VERSION,
            "members": members,
        }),
        artifact_reference: None,
        created_at_ms: settled_at_ms,
    };
    persist_validated_output_transaction(transaction, &controller_output)?;
    Ok(())
}

fn composition_one(
    db: &dyn Database,
    query: switchy_database::query::SelectQuery<'_>,
) -> Result<Option<Row>, WorkflowStoreError> {
    Ok(block_on(query.limit(1).execute_first(db))?)
}
fn composition_required(
    db: &dyn Database,
    query: switchy_database::query::SelectQuery<'_>,
) -> Result<Row, WorkflowStoreError> {
    composition_one(db, query)?.ok_or_else(|| {
        WorkflowStoreError::InvalidData("required composition row is missing".into())
    })
}
fn column(table: &str, name: &str) -> E {
    E::Column {
        table: Some(table.into()),
        column: name.into(),
    }
}
fn same_column(left_table: &str, left: &str, right_table: &str, right: &str) -> E {
    E::Compare(
        Box::new(column(left_table, left)),
        C::Equal,
        Box::new(column(right_table, right)),
    )
}
fn not_exists(exists: E) -> E {
    E::Compare(Box::new(exists), C::Equal, Box::new(E::Integer(0)))
}
fn active_statuses() -> Vec<DatabaseValue> {
    [
        "prepared",
        "admitted",
        "running",
        "cancelling",
        "sibling_cancelling",
    ]
    .into_iter()
    .map(DatabaseValue::from)
    .collect()
}
fn attempt_query() -> switchy_database::query::SelectQuery<'static> {
    composition_receipt_projection(select("workflow_attempts").columns(&[
        "run_id",
        "node_id",
        "activation_id",
        "attempt",
        "dispatch_identity",
    ]))
}
fn composition_publication_cancelled(
    db: &dyn Database,
    run_id: &str,
    member: &str,
    generation: u64,
) -> Result<bool, WorkflowStoreError> {
    let intent = E::exists(
        "workflow_publication_cancellations",
        E::And(vec![
            same_column(
                "workflow_publication_cancellations",
                "run_id",
                "workflow_attempts",
                "run_id",
            ),
            same_column(
                "workflow_publication_cancellations",
                "dispatch_identity",
                "workflow_attempts",
                "dispatch_identity",
            ),
        ]),
    );
    let attempt = E::exists(
        "workflow_attempts",
        E::And(vec![
            same_column(
                "workflow_attempts",
                "run_id",
                "workflow_activations",
                "run_id",
            ),
            same_column(
                "workflow_attempts",
                "node_id",
                "workflow_activations",
                "node_id",
            ),
            same_column(
                "workflow_attempts",
                "activation_id",
                "workflow_activations",
                "activation_id",
            ),
            intent,
        ]),
    );
    Ok(composition_one(
        db,
        select("workflow_activations")
            .columns(&["activation_id"])
            .where_eq("run_id", run_id)
            .where_eq("node_id", member)
            .where_eq(
                "dependency_generation",
                database::integer_value(generation)?,
            )
            .filter(Box::new(schema_predicate(&attempt)?)),
    )?
    .is_some())
}
fn fan_out_activation_members(table: &str, controller: &str, status: Option<&str>) -> E {
    let mut predicates = vec![
        same_column(
            "workflow_fan_out_members",
            "member_activation_id",
            table,
            "activation_id",
        ),
        E::Compare(
            Box::new(column(
                "workflow_fan_out_members",
                "controller_activation_id",
            )),
            C::Equal,
            Box::new(E::Text(controller.into())),
        ),
    ];
    // The original attempt cancellation query is controller-qualified only. Activation
    // promotion/cancellation is additionally confined to the outer run.
    if table == "workflow_activations" {
        predicates.push(same_column(
            "workflow_fan_out_members",
            "run_id",
            table,
            "run_id",
        ));
    }
    if let Some(status) = status {
        predicates.push(E::Compare(
            Box::new(column("workflow_fan_out_members", "status")),
            C::Equal,
            Box::new(E::Text(status.into())),
        ));
    }
    E::exists("workflow_fan_out_members", E::And(predicates))
}
fn fan_out_active_count(
    db: &dyn Database,
    run_id: &str,
    controller: &str,
) -> Result<u32, WorkflowStoreError> {
    let row = composition_required(
        db,
        select("workflow_fan_out_members")
            .projections(vec![
                Projection::new(count(identifier("member_index"))).alias("count"),
            ])
            .where_eq("run_id", run_id)
            .where_eq("controller_activation_id", controller)
            .where_in(
                "status",
                vec![
                    DatabaseValue::from("pending"),
                    DatabaseValue::from("running"),
                ],
            ),
    )?;
    u32::try_from(database::unsigned(&row, "count")?)
        .map_err(|_| WorkflowStoreError::InvalidData("fan-out active count overflow".into()))
}
fn promote_fan_out_waiting(
    db: &dyn Database,
    run_id: &str,
    controller: &str,
    allowance: u32,
) -> Result<(), WorkflowStoreError> {
    let members =
        select("workflow_fan_out_members")
            .columns(&["rowid"])
            .where_eq("run_id", run_id)
            .where_eq("controller_activation_id", controller)
            .where_eq("status", "waiting")
            .sort("member_index", SortDirection::Asc)
            .limit(usize::try_from(allowance).map_err(|_| {
                WorkflowStoreError::InvalidData("fan-out allowance overflow".into())
            })?);
    let update = db
        .update("workflow_fan_out_members")
        .value("status", "pending")
        .where_in("rowid", members);
    block_on(db.exec_update_count(&update))?;
    Ok(())
}

fn composition_receipt_projection(
    mut query: switchy_database::query::SelectQuery<'_>,
) -> switchy_database::query::SelectQuery<'_> {
    if query.projections.is_empty() {
        query.projections = query
            .columns
            .iter()
            .map(|name| Projection::new(identifier(name)))
            .collect();
    }
    query
        .project_as(
            bounded_text(
                switchy_database::query::qualified_column("workflow_attempts", "receipt_json"),
                MAX_INLINE_JSON_BYTES as u64,
                DatabaseValue::Null,
            ),
            "receipt_json",
        )
        .project_as(
            Box::new(switchy_database::query::where_eq_expression(
                switchy_database::query::qualified_column("workflow_attempts", "receipt_json"),
                DatabaseValue::Null,
            )) as Box<dyn switchy_database::query::Expression>,
            "receipt_is_null",
        )
}
