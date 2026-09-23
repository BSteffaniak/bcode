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

fn request(
    source: bcode_workflow::WorkflowContinuationSource,
    additional: u32,
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
    let nodes_per_iteration = if definition.nodes.contains_key("loop.judgement.evaluate") {
        3
    } else {
        2
    };
    limits.node_execution_cap = u64::from(additional)
        .checked_mul(nodes_per_iteration)
        .and_then(|value| value.checked_mul(u64::from(limits.retry_cap) + 1))
        .ok_or("Node allowance overflow")?;
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

pub fn command(session_id: SessionId, arguments: &str) -> InvokeCommandResponse {
    let Ok(additional) = arguments.trim().parse::<u32>() else {
        return status_response("Usage: /goal.continue <additional_iterations> (positive integer)");
    };
    if additional == 0 {
        return status_response("Additional iterations must be greater than zero");
    }
    let result = run_async(async move {
        let client = BcodeClient::default_endpoint();
        let Some(run) = client
            .associated_workflow_run(workflow_binding_key(session_id))
            .await?
        else {
            return Ok("No associated loop".into());
        };
        let source = client.workflow_continuation_source(run.run_id).await?;
        let request = match request(source, additional) {
            Ok(request) => request,
            Err(error) => return Ok(error),
        };
        // A lost response can be retried with this exact request, never a newly generated ID.
        let started = match client.continue_workflow(request.clone()).await {
            Ok(started) => started,
            Err(ClientError::Server { code, message }) => {
                return Err(ClientError::Server { code, message });
            }
            Err(_) => client.continue_workflow(request).await?,
        };
        Ok(format!(
            "Granted up to {additional} more iterations · continuation {}",
            started.run.run_id
        ))
    });
    match result {
        Ok(message) => status_response(&message),
        Err(error) => status_response(&format!("Continuation unavailable: {error}")),
    }
}
