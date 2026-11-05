//! Plugin-owned managed workflow preparation. Host owns durable dispatch and observation.
use bcode_plugin_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Managed execution block: submission followed by authoritative, correlated settlement.
pub const EXECUTE_OPERATION: &str = "harness.execute";

/// Typed execute input; the encoded request is the ordinary managed `followup` request.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedWorkflowExecution {
    /// Retained submission identity, exact binding, instance and text; no auth secrets.
    pub request_json: String,
}

fn prepared_payload(operation: &str, input: &Value) -> Result<Value, String> {
    let op = operation
        .strip_prefix("harness.")
        .ok_or("invalid managed operation")?;
    crate::tools::payload(if op == "execute" { "followup" } else { op }, input)
}

fn prepare(context: &NativeServiceContext) -> ServiceResponse {
    let Ok(request) = context
        .request
        .payload_json::<bcode_workflow::WorkflowBlockPreparationRequest>()
    else {
        return ServiceResponse::error("invalid_request", "invalid managed workflow preparation");
    };
    if request.validate().is_err() || request.block.plugin_id != crate::PLUGIN_ID {
        return ServiceResponse::error(
            "invalid_request",
            "invalid managed workflow preparation owner",
        );
    }
    let Ok(payload) = prepared_payload(&request.block.operation, &request.input) else {
        return ServiceResponse::error("invalid_request", "invalid managed workflow input");
    };
    let Ok(checksum) = bcode_workflow::workflow_block_input_sha256(&request.input) else {
        return ServiceResponse::error("invalid_request", "managed workflow checksum unavailable");
    };
    let read = request.block.operation == "harness.inspect";
    let operation = if read {
        bcode_agent_profile::ToolPolicyOperation::ReadOnly
    } else {
        bcode_agent_profile::ToolPolicyOperation::Mutating
    };
    let facts = bcode_agent_profile::ToolPolicyAuthorizationMetadata {
        requires_permission: !read,
        aliases: vec![request.block.operation.clone()],
        compatibility_aliases: Vec::new(),
        capabilities: Vec::new(),
        permission_category: None,
        operation,
    };
    let response = bcode_workflow::WorkflowBlockPreparationResponse {
        version: bcode_workflow::WORKFLOW_BLOCK_PREPARATION_VERSION,
        input_sha256: checksum,
        owner_id: crate::PLUGIN_ID.into(),
        operation_facts: serde_json::to_value(facts).unwrap_or(Value::Null),
        descriptor: json!({"interface_id":crate::MANAGED_SESSION_INTERFACE_ID,"operation":request.block.operation,"payload":payload,"execution":request.block.operation == EXECUTE_OPERATION}),
        diagnostics: Vec::new(),
    };
    if response.validate().is_err() {
        return ServiceResponse::error(
            "invalid_request",
            "managed workflow preparation exceeds bound",
        );
    }
    crate::json_response(&response)
}

pub fn invoke(context: &NativeServiceContext) -> ServiceResponse {
    if context.request.operation == bcode_workflow::WORKFLOW_BLOCK_PREPARE_OPERATION {
        return prepare(context);
    }
    let Some(op) = context
        .request
        .operation
        .strip_prefix("harness.")
        .filter(|op| crate::OPERATIONS.contains(op))
    else {
        // execute is a durable host-owned operation: it must not run as a synchronous plugin
        // callback whose response/timeout would incorrectly settle an external assignment.
        return ServiceResponse::error(
            "invalid_dispatch",
            "managed execution requires its durable workflow operation owner",
        );
    };
    let Ok(invocation) = context
        .request
        .payload_json::<bcode_workflow::WorkflowBlockInvocation>()
    else {
        return ServiceResponse::error("invalid_request", "invalid managed workflow invocation");
    };
    let Ok(input) = invocation.typed_input::<Value>() else {
        return ServiceResponse::error("invalid_request", "invalid managed workflow envelope");
    };
    let Ok(payload) = crate::tools::payload(op, &input) else {
        return ServiceResponse::error("invalid_request", "invalid managed workflow request");
    };
    let Some(preparation) = invocation.preparation.as_ref() else {
        return ServiceResponse::error(
            "invalid_request",
            "managed workflow requires retained preparation",
        );
    };
    if preparation.validate().is_err()
        || preparation.owner_id != crate::PLUGIN_ID
        || bcode_workflow::workflow_block_input_sha256(&input)
            .ok()
            .as_ref()
            != Some(&preparation.input_sha256)
        || preparation.descriptor["payload"] != payload
        || preparation.descriptor["operation"].as_str() != Some(context.request.operation.as_str())
    {
        return ServiceResponse::error("invalid_request", "managed workflow preparation changed");
    }
    match crate::tools::bridge(context, invocation.dispatch_identity, None, op, payload) {
        Ok(value) => crate::json_response(&value),
        Err(()) => ServiceResponse::error(
            "managed_session_unavailable",
            "managed workflow operation requires admission or retained-identity reconciliation",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn workflow_request_json_is_exact_and_bounded() {
        assert!(
            prepared_payload(
                EXECUTE_OPERATION,
                &json!({"request_json":"{}", "authority":"parent"})
            )
            .is_err()
        );
        assert!(prepared_payload("harness.respond", &json!({"request_json":"{}"})).is_err());
    }
}
