//! Plugin-owned bounded contributor evidence tool; authority remains host-owned.
use bcode_plugin_sdk::prelude::{
    NativeServiceContext, ServiceBridgeRequest, ServiceBridgeResponse, ServiceResponse,
};
use bcode_tool::{
    ToolDefinition, ToolInvocationRequest, ToolInvocationServiceRequest,
    ToolInvocationServiceResolution,
};
use bcode_workflow::WORKFLOW_APPLICATION_INTERFACE_ID;
use bcode_workflow::contributor_evidence::{
    CONTRACT_ID, ContributorEvidenceAction, ContributorEvidenceStatus,
    ExecutionContributorEvidenceRequest, ExecutionContributorEvidenceResponse, OPERATION,
};
use serde_json::json;

pub(super) const NAME: &str = "workflow.contributor_evidence";
#[cfg(test)]
mod tests;

pub(super) fn definition() -> ToolDefinition {
    let tool = ToolDefinition {
        name: NAME.into(),
        description: "Read bounded recorded tool request/result evidence from the authenticated prior direct contributor to this active workflow execution. Discover canonical event references, then read an exact invocation pair. The host selects the contributor/session; callers cannot choose them. Requires the current inspected graph revision. Generated delegation and other unproved scopes return explicit unsupported or provenance_unavailable; missing or truncated evidence never proves completion. Does not mutate, replay history, or open artifacts.".into(),
        input_schema: json!({"type":"object","additionalProperties":false,
            "required":["expected_revision","action"],"properties":{
                "expected_revision":{"type":"integer","minimum":1},
                "action":{"oneOf":[
                    {"type":"object","additionalProperties":false,"required":["kind","after_sequence","limit"],
                     "properties":{"kind":{"const":"discover"},"after_sequence":{"type":["integer","null"],"minimum":0},
                         "limit":{"type":"integer","minimum":1,"maximum":100}}},
                    {"type":"object","additionalProperties":false,"required":["kind","invocation_id","request_sequence","result_sequence","byte_limit"],
                     "properties":{"kind":{"const":"read"},"invocation_id":{"type":"string","minLength":1,"maxLength":1024},
                         "request_sequence":{"type":"integer","minimum":0},"result_sequence":{"type":"integer","minimum":0},
                         "byte_limit":{"type":"integer","minimum":1,"maximum":65536}}}
                ]}
            }}),
    };
    super::portable_task_definition(tool)
}

pub(super) fn parse(
    arguments: serde_json::Value,
) -> Result<ExecutionContributorEvidenceRequest, String> {
    let arguments = if let Some(encoded) = arguments.get("request_json") {
        if arguments.as_object().is_none_or(|object| object.len() != 1) {
            return Err("request_json cannot be combined with evidence fields".into());
        }
        let text = encoded
            .as_str()
            .ok_or("request_json must be a JSON string")?;
        serde_json::from_str(text)
            .map_err(|_| "invalid contributor evidence request JSON".to_owned())?
    } else {
        arguments
    };
    let request: ExecutionContributorEvidenceRequest = serde_json::from_value(arguments)
        .map_err(|_| "invalid contributor evidence request".to_owned())?;
    request.validate().map_err(str::to_owned)?;
    Ok(request)
}

pub(super) fn invoke(
    context: &NativeServiceContext,
    request: ToolInvocationRequest,
) -> ServiceResponse {
    let Ok(query) = parse(request.arguments) else {
        return ServiceResponse::error("invalid_request", "invalid contributor evidence request");
    };
    let Ok(payload) = serde_json::to_value(&query) else {
        return ServiceResponse::error(
            "invalid_request",
            "cannot encode contributor evidence request",
        );
    };
    if !prepared_matches(&request.preparation_descriptor, &payload)
        || context.cancellation.is_cancelled()
    {
        return ServiceResponse::error(
            "invalid_request",
            "contributor evidence preparation mismatch",
        );
    }
    let Some(route) = request
        .preparation_descriptor
        .get("route_id")
        .and_then(serde_json::Value::as_str)
    else {
        return ServiceResponse::error("invalid_request", "contributor evidence route missing");
    };
    context
        .bridge
        .request(&ServiceBridgeRequest::InvokeService(
            ToolInvocationServiceRequest {
                invocation_id: request.tool_call_id.clone(),
                request_id: request.tool_call_id,
                route_id: Some(route.into()),
                interface_id: WORKFLOW_APPLICATION_INTERFACE_ID.into(),
                operation: OPERATION.into(),
                payload,
            },
        ))
        .map_or_else(
            |_| {
                ServiceResponse::error(
                    "evidence_unavailable",
                    "authenticated contributor evidence unavailable",
                )
            },
            |response| invocation_response(response, &query),
        )
}

fn invocation_response(
    response: ServiceBridgeResponse,
    query: &ExecutionContributorEvidenceRequest,
) -> ServiceResponse {
    match response {
        ServiceBridgeResponse::Service(ToolInvocationServiceResolution::Responded { payload }) => {
            render(payload, query)
        }
        _ => ServiceResponse::error(
            "evidence_unavailable",
            "authenticated contributor evidence unavailable",
        ),
    }
}

fn prepared_matches(descriptor: &serde_json::Value, payload: &serde_json::Value) -> bool {
    descriptor
        .get("operation")
        .and_then(serde_json::Value::as_str)
        == Some(OPERATION)
        && descriptor.get("edit") == Some(payload)
}

fn render(
    payload: serde_json::Value,
    query: &ExecutionContributorEvidenceRequest,
) -> ServiceResponse {
    let Ok(response) = serde_json::from_value::<ExecutionContributorEvidenceResponse>(payload)
    else {
        return ServiceResponse::error("invalid_response", "invalid contributor evidence response");
    };
    if !valid_response(&response, query) {
        return ServiceResponse::error("invalid_response", "invalid contributor evidence response");
    }
    let Ok(output) = serde_json::to_string(&response) else {
        return ServiceResponse::error(
            "invalid_response",
            "cannot encode contributor evidence response",
        );
    };
    // Semantic text is already byte-bounded; include finite locator/identity overhead only.
    if output.len() > 524_288 {
        return ServiceResponse::error(
            "invalid_response",
            "contributor evidence response exceeds bound",
        );
    }
    super::super::json_response(&bcode_tool::ToolInvocationResponse {
        output,
        is_error: false,
        content: Vec::new(),
        full_output: None,
        result: None,
    })
}

fn valid_response(
    response: &ExecutionContributorEvidenceResponse,
    query: &ExecutionContributorEvidenceRequest,
) -> bool {
    if response.contract != CONTRACT_ID {
        return false;
    }
    if response.status != ContributorEvidenceStatus::Available {
        return response.scope.is_none()
            && response.events.is_empty()
            && response.scanned_events == 0
            && response.next_sequence.is_none()
            && response.evidence.is_none();
    }
    let Some(scope) = &response.scope else {
        return false;
    };
    if scope.graph_revision != query.expected_revision
        || scope.attempt == 0
        || [
            &scope.run_id,
            &scope.consumer_activation_id,
            &scope.node_id,
            &scope.activation_id,
            &scope.session_id,
            &scope.output_id,
            &scope.output_checksum_sha256,
        ]
        .iter()
        .any(|id| id.is_empty() || id.len() > 1024)
    {
        return false;
    }
    match &query.action {
        ContributorEvidenceAction::Discover { limit, .. } => {
            response.evidence.is_none()
                && response.scanned_events <= usize::from(*limit)
                && response.events.len() <= response.scanned_events
                && response.events.iter().all(|event| {
                    !event.invocation_id.is_empty() && event.invocation_id.len() <= 1024
                })
                && response
                    .events
                    .windows(2)
                    .all(|events| events[0].sequence < events[1].sequence)
        }
        ContributorEvidenceAction::Read {
            invocation_id,
            request_sequence,
            result_sequence,
            byte_limit,
        } => {
            let Some(evidence) = &response.evidence else {
                return false;
            };
            response.events.is_empty()
                && response.scanned_events == 2
                && response.next_sequence.is_none()
                && evidence.invocation_id == *invocation_id
                && evidence.request_sequence == *request_sequence
                && evidence.result_sequence == *result_sequence
                && evidence.tool_name.len() <= 1024
                && evidence
                    .producer_plugin_id
                    .as_ref()
                    .is_none_or(|id| id.len() <= 1024)
                && [&evidence.arguments_json, &evidence.model_output]
                    .iter()
                    .all(|text| {
                        text.retained_bytes == text.text.len()
                            && text.retained_bytes <= text.original_bytes
                            && text.truncated == (text.retained_bytes < text.original_bytes)
                    })
                && evidence
                    .arguments_json
                    .retained_bytes
                    .saturating_add(evidence.model_output.retained_bytes)
                    <= usize::try_from(*byte_limit).unwrap_or(0)
        }
    }
}
