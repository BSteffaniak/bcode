//! Explicit iteration grants for exhausted goals and plain loops.
use super::{
    BcodeClient, ClientError, InvokeCommandResponse, LoopWorkflowIteration, PLUGIN_ID, SessionId,
    WORKFLOW_KIND, run_async, status_response, workflow_binding_key,
};

fn retain_reachable(definition: &mut bcode_workflow::WorkflowDefinition) {
    let mut reachable = definition
        .entries
        .iter()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    loop {
        let before = reachable.len();
        for edge in &definition.edges {
            if reachable.contains(&edge.from) {
                reachable.insert(edge.to.clone());
            }
        }
        if reachable.len() == before {
            break;
        }
    }
    definition.nodes.retain(|id, _| reachable.contains(id));
    definition
        .edges
        .retain(|edge| reachable.contains(&edge.from) && reachable.contains(&edge.to));
    definition.exits.retain(|id| reachable.contains(id));
}

#[cfg(test)]
#[path = "continuation_tests.rs"]
mod tests;

/// Count executable nodes for loop launch and continuation allowance planning.
/// Returns an error if the graph count cannot be represented by the durable allowance type.
pub fn executable_node_count(
    definition: &bcode_workflow::WorkflowDefinition,
) -> Result<u64, String> {
    u64::try_from(
        definition
            .nodes
            .values()
            .filter(|node| {
                matches!(
                    node.kind,
                    bcode_workflow::NodeKind::Task
                        | bcode_workflow::NodeKind::Agent
                        | bcode_workflow::NodeKind::PluginBlock
                        | bcode_workflow::NodeKind::WorkflowCall
                )
            })
            .count(),
    )
    .map_err(|_| "Executable node count exceeds supported range".into())
}

#[cfg(test)]
fn request(
    source: bcode_workflow::WorkflowContinuationSource,
    additional: u32,
) -> Result<bcode_workflow::WorkflowContinuationRequest, String> {
    request_with_allowance(source, additional, 0)
}

fn request_with_allowance(
    source: bcode_workflow::WorkflowContinuationSource,
    additional: u32,
    worker_attempts: u64,
) -> Result<bcode_workflow::WorkflowContinuationRequest, String> {
    if additional == 0 {
        return Err("Additional iterations must be greater than zero".into());
    }
    if source.repeat_node_id != "loop.repeat" {
        return Err("This is not a supported loop exhaustion checkpoint".into());
    }
    let binding = source
        .run
        .binding
        .clone()
        .ok_or("Loop binding is missing")?;
    if binding.owner_plugin_id != PLUGIN_ID || binding.workflow_kind != WORKFLOW_KIND {
        return Err("Not a loop-plugin workflow".into());
    }
    let total = source
        .total_iterations_completed
        .checked_add(u64::from(additional))
        .and_then(|value| u32::try_from(value).ok())
        .ok_or("Cumulative iteration allowance exceeds supported range")?;
    let mut input: LoopWorkflowIteration =
        serde_json::from_value(source.input).map_err(|error| error.to_string())?;
    if input.condition_met {
        return Err("Goal already achieved; start a revised goal instead".into());
    }
    input.iteration =
        u32::try_from(source.total_iterations_completed + 1).map_err(|error| error.to_string())?;
    input.max_iterations = total;
    let mut definition = source.definition;
    let repeat = definition
        .nodes
        .get_mut("loop.repeat")
        .ok_or("Missing loop repeat")?;
    repeat.configuration["max_iterations"] = serde_json::json!(additional);
    let entries: Vec<_> = definition
        .edges
        .iter_mut()
        .filter_map(|edge| {
            if edge.from == "loop.repeat"
                && let bcode_workflow::EdgeKind::Back { max_iterations, .. } = &mut edge.kind
            {
                *max_iterations = additional;
                Some(edge.to.clone())
            } else {
                None
            }
        })
        .collect();
    if entries.len() != 1 {
        return Err("Unsupported loop continuation topology".into());
    }
    definition.entries = entries;
    // Keep the exact persisted implementation/evaluation nodes and transforms. Initialization
    // belongs to the original run, so remove only nodes unreachable from the renewed entry.
    retain_reachable(&mut definition);
    if definition.exits != ["loop.repeat"] {
        return Err("Unsupported loop continuation exits".into());
    }
    let identity =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition(WORKFLOW_KIND, &definition)
            .map_err(|error| error.to_string())?;
    let mut limits = source.limits;
    limits.cycle_cap = additional;
    let nodes_per_iteration = executable_node_count(&definition)?;
    limits.node_execution_cap = u64::from(additional)
        .checked_mul(nodes_per_iteration)
        .and_then(|value| value.checked_mul(u64::from(limits.retry_cap) + 1))
        .and_then(|value| value.checked_add(worker_attempts))
        .ok_or("Node allowance overflow")?;
    limits.concurrency_cap =
        u32::try_from(u64::from(limits.concurrency_cap).min(limits.node_execution_cap))
            .map_err(|_| "Concurrency allowance exceeds supported range")?;
    i64::try_from(limits.node_execution_cap)
        .map_err(|_| "Node allowance exceeds the supported storage integer range")?;
    let session = source
        .run
        .parent_session_id
        .as_deref()
        .ok_or("Missing parent session")?
        .parse()
        .map_err(|_| "Invalid parent session")?;
    Ok(bcode_workflow::WorkflowContinuationRequest {
        source_run_id: source.run.run_id,
        expected_graph_revision: source.graph_revision,
        expected_output_checksum: source.output_checksum,
        additional_iterations: additional,
        successor: bcode_workflow::WorkflowStartRequest {
            identity,
            definition,
            run_id: Some(uuid::Uuid::new_v4().to_string()),
            workspace_snapshot: Some(source.run.workspace_snapshot),
            parent_session_id: session,
            input: serde_json::to_value(input).map_err(|error| error.to_string())?,
            binding,
            limits,
        },
    })
}

fn parse_allowance(arguments: &str) -> Result<(u32, u64), &'static str> {
    let tokens: Vec<_> = arguments.split_whitespace().collect();
    let (rounds, workers) = match tokens.as_slice() {
        [rounds] => (*rounds, None),
        [rounds, "--worker-attempts", workers] => (*rounds, Some(*workers)),
        _ => {
            return Err(
                "Usage: /goal.continue <additional_iterations> [--worker-attempts <positive integer>]",
            );
        }
    };
    let rounds = rounds
        .parse::<std::num::NonZeroU32>()
        .map_err(|_| "Additional iterations must be a positive integer")?;
    let workers = workers
        .map(|value| {
            value
                .parse::<std::num::NonZeroU64>()
                .map(std::num::NonZeroU64::get)
        })
        .transpose()
        .map_err(|_| "Worker attempts must be a positive integer")?
        .unwrap_or(0);
    Ok((rounds.get(), workers))
}

pub fn command(session_id: SessionId, arguments: &str) -> InvokeCommandResponse {
    let (additional, worker_attempts) = match parse_allowance(arguments) {
        Ok(allowance) => allowance,
        Err(error) => {
            let mut response = status_response(error);
            response.success = false;
            return response;
        }
    };
    let result = run_async(async move {
        let client = BcodeClient::default_endpoint();
        let Some(run) = client
            .associated_workflow_run(workflow_binding_key(session_id))
            .await?
        else {
            return Ok(Err("No associated loop".into()));
        };
        let source = client.workflow_continuation_source(run.run_id).await?;
        let request = match request_with_allowance(source, additional, worker_attempts) {
            Ok(request) => request,
            Err(error) => return Ok(Err(error)),
        };
        // A lost response can be retried with this exact request, never a newly generated ID.
        let started = match client.continue_workflow(request.clone()).await {
            Ok(started) => started,
            Err(ClientError::Server { code, message }) => {
                return Err(ClientError::Server { code, message });
            }
            Err(_) => client.continue_workflow(request).await?,
        };
        Ok(Ok(format!(
            "Granted up to {additional} more iterations and {worker_attempts} extra execution attempts · continuation {}",
            started.run.run_id
        )))
    });
    continuation_response(result.unwrap_or_else(|error| Err(error.to_string())))
}

fn continuation_response(result: Result<String, String>) -> InvokeCommandResponse {
    match result {
        Ok(message) => status_response(&message),
        Err(error) => {
            let mut response = status_response(&format!("Continuation unavailable: {error}"));
            response.success = false;
            response
        }
    }
}
