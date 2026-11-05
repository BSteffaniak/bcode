//! Plugin-owned managed workflow preparation. Host owns durable dispatch and observation.
use bcode_plugin_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Managed execution block: submission followed by authoritative, correlated settlement.
pub const EXECUTE_OPERATION: &str = "harness.execute";
/// Canonical observation-only destination for explicit responsibility adoption.
pub const AWAIT_OPERATION: &str = "harness.await";

/// Typed execute input; the encoded request is the ordinary managed `followup` request.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedWorkflowExecution {
    /// Retained submission identity, exact binding, instance and text; no auth secrets.
    pub request_json: String,
    /// Boundary required before the workflow may settle; defaults conservatively.
    #[serde(default)]
    pub completion_requirement: bcode_harness_models::HarnessCompletionRequirement,
}

fn prepared_payload(operation: &str, input: &Value) -> Result<Value, String> {
    let op = operation
        .strip_prefix("harness.")
        .ok_or("invalid managed operation")?;
    if matches!(op, "execute" | "await") {
        let execution: ManagedWorkflowExecution =
            serde_json::from_value(input.clone()).map_err(|_| "invalid managed execution input")?;
        if execution.completion_requirement
            == bcode_harness_models::HarnessCompletionRequirement::WorkspaceRelease
        {
            return Err("workspace release requires separate host authorization".into());
        }
        crate::tools::payload("followup", &json!({"request_json":execution.request_json}))
    } else {
        crate::tools::payload(op, input)
    }
}

fn prepared_execution_requirement(
    block: &bcode_workflow::WorkflowBlockDefinition,
    input: &Value,
) -> Result<bcode_harness_models::HarnessCompletionRequirement, String> {
    block
        .input
        .validate_value("managed execution input", input)
        .map_err(|e| e.to_string())?;
    let execution: ManagedWorkflowExecution =
        serde_json::from_value(input.clone()).map_err(|_| "invalid managed execution input")?;
    if block.block_version != 2 || block.output.type_name != "bcode.harness.assignment-result/v2" {
        return Err("unsupported managed execution block contract".into());
    }
    if block.operation == AWAIT_OPERATION
        && execution.completion_requirement
            != bcode_harness_models::HarnessCompletionRequirement::AssociatedWork
    {
        return Err("await must retain associated-work responsibility".into());
    }
    Ok(execution.completion_requirement)
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
    let requirement = if matches!(
        request.block.operation.as_str(),
        EXECUTE_OPERATION | AWAIT_OPERATION
    ) {
        let Ok(requirement) = prepared_execution_requirement(&request.block, &request.input) else {
            return ServiceResponse::error(
                "invalid_request",
                "invalid managed execution requirement",
            );
        };
        requirement
    } else {
        bcode_harness_models::HarnessCompletionRequirement::default()
    };
    let mut descriptor = json!({"interface_id":crate::MANAGED_SESSION_INTERFACE_ID,"operation":request.block.operation,"payload":payload,"execution":matches!(request.block.operation.as_str(), EXECUTE_OPERATION | AWAIT_OPERATION),"completion_requirement":requirement});
    if request.block.operation == AWAIT_OPERATION {
        descriptor["admission"] =
            serde_json::to_value(bcode_workflow::WorkflowPreparedAdmission::DeferredUntilAdopted)
                .expect("serializable admission policy");
    }
    let response = bcode_workflow::WorkflowBlockPreparationResponse {
        version: bcode_workflow::WORKFLOW_BLOCK_PREPARATION_VERSION,
        input_sha256: checksum,
        owner_id: crate::PLUGIN_ID.into(),
        operation_facts: serde_json::to_value(facts).unwrap_or(Value::Null),
        descriptor,
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
    use bcode_plugin_sdk::{PluginConfigContext, ServiceCancellation};

    #[test]
    fn completion_requirement_defaults_and_rejects_workspace_release() {
        let execution: ManagedWorkflowExecution =
            serde_json::from_value(json!({"request_json":"{}"})).unwrap();
        assert_eq!(
            execution.completion_requirement,
            bcode_harness_models::HarnessCompletionRequirement::AssociatedWork
        );
        assert!(
            prepared_payload(
                EXECUTE_OPERATION,
                &json!({
                    "request_json":"{}", "completion_requirement":"workspace_release"
                })
            )
            .unwrap_err()
            .contains("workspace release")
        );
        assert!(
            serde_json::from_value::<ManagedWorkflowExecution>(json!({
                "request_json":"{}", "completion_requirement":"invented"
            }))
            .is_err()
        );
    }

    #[test]
    fn completion_requirement_is_policy_not_native_mutation_payload() {
        let request = json!({"harness": {"schema_version":1,"request_id":"submission", "instance": {
            "instance_id":"instance", "plugin_id":"test.harness", "executable":"/explicit/harness", "config_namespace":"isolated",
            "auth":{"provider_id":"test.auth", "profile":"native", "credential_namespace":"isolated", "custody":"native_managed"}
        }, "kind":{"operation":"submit", "request":{
            "target":{"state":"confirmed","session":{"schema_version":1,"session_id":"child","instance_id":"instance", "native_session_id":"native","native_version":"1.0.0","generation":2}},
            "submission":{"submission_id":"submission","attempt_id":"attempt","session_id":"child","generation":2,"text":"task","placement":{"kind":"next_turn"}}
        }}}, "parent_session_id":null, "assistance_destination_session_id":null});
        for requirement in ["foreground", "associated_work"] {
            let payload = prepared_payload(
                EXECUTE_OPERATION,
                &json!({
                    "request_json":serde_json::to_string(&request).unwrap(),
                    "completion_requirement":requirement,
                }),
            )
            .unwrap();
            assert_eq!(payload, request);
            assert!(payload.get("completion_requirement").is_none());
        }
    }

    #[test]
    fn shipped_execute_manifest_preparation_and_nullable_output_are_versioned() {
        #[derive(Deserialize)]
        struct Manifest {
            services: Vec<Service>,
        }
        #[derive(Deserialize)]
        struct Service {
            #[serde(default)]
            workflow_blocks: Vec<bcode_workflow::WorkflowBlockDefinition>,
        }
        let manifest: Manifest = toml::from_str(include_str!("../bcode-plugin.toml")).unwrap();
        let block = manifest
            .services
            .into_iter()
            .flat_map(|s| s.workflow_blocks)
            .find(|b| b.operation == EXECUTE_OPERATION)
            .unwrap();
        block.validate().unwrap();
        for requirement in [None, Some("foreground"), Some("associated_work")] {
            let mut input = json!({"request_json":"{}"});
            if let Some(requirement) = requirement {
                input["completion_requirement"] = json!(requirement);
            }
            let resolved = prepared_execution_requirement(&block, &input).unwrap();
            assert_eq!(
                serde_json::to_value(resolved).unwrap(),
                json!(requirement.unwrap_or("associated_work"))
            );
        }
        let mut output = json!({"session_id":"session", "submission_id":"submission",
        "native_turn_id":null, "result":"answer", "execution_handle":{
            "session_id":"session", "generation":1, "submission_id":"submission",
            "attempt_id":"attempt", "dispatch_identity":"dispatch"
        }});
        block
            .output
            .validate_value("foreground result", &output)
            .unwrap();
        output["execution_handle"]["authority"] = json!(true);
        assert!(
            block
                .output
                .validate_value("forged authority", &output)
                .is_err()
        );
        let mut legacy = block;
        legacy.block_version = 1;
        legacy.output.type_name = "bcode.harness.assignment-result/v1".into();
        assert!(prepared_execution_requirement(&legacy, &json!({"request_json":"{}"})).is_err());
    }

    #[test]
    fn shipped_manifest_prepare_retains_requirement_and_input_checksum() {
        #[derive(Deserialize)]
        struct Manifest {
            services: Vec<Service>,
        }
        #[derive(Deserialize)]
        struct Service {
            #[serde(default)]
            workflow_blocks: Vec<bcode_workflow::WorkflowBlockDefinition>,
        }
        let manifest: Manifest = toml::from_str(include_str!("../bcode-plugin.toml")).unwrap();
        let block = manifest
            .services
            .into_iter()
            .flat_map(|s| s.workflow_blocks)
            .find(|b| b.operation == EXECUTE_OPERATION)
            .unwrap();
        let payload = json!({"harness":{"schema_version":1,"request_id":"submission",
            "instance":{"instance_id":"instance","plugin_id":"test.harness","executable":"/explicit/harness",
                "config_namespace":"isolated","auth":{"provider_id":"test.auth","profile":"native",
                "credential_namespace":"isolated","custody":"native_managed"}},
            "kind":{"operation":"submit","request":{
                "target":{"state":"confirmed","session":{"schema_version":1,"session_id":"child",
                    "instance_id":"instance","native_session_id":"native","native_version":"1","generation":2}},
                "submission":{"submission_id":"submission","attempt_id":"attempt","session_id":"child",
                    "generation":2,"text":"task","placement":{"kind":"next_turn"}}}}},
            "parent_session_id":null,"assistance_destination_session_id":null});
        let input = json!({"request_json":serde_json::to_string(&payload).unwrap(),
            "completion_requirement":"foreground"});
        let request = bcode_workflow::WorkflowBlockPreparationRequest {
            version: bcode_workflow::WORKFLOW_BLOCK_PREPARATION_VERSION,
            block,
            context: bcode_workflow::WorkflowBlockPreparationContext {
                run_id: "run".into(),
                node_id: "node".into(),
                activation_id: "activation".into(),
                attempt: 0,
                preparation_identity: "preparation".into(),
                workspace_root: "/workspace".into(),
            },
            input: input.clone(),
        };
        let response = invoke(&NativeServiceContext {
            plugin_id: crate::PLUGIN_ID.into(),
            request: ServiceRequest {
                interface_id: "bcode.workflow-block/v1".into(),
                operation: bcode_workflow::WORKFLOW_BLOCK_PREPARE_OPERATION.into(),
                payload: serde_json::to_vec(&request).unwrap(),
            },
            config: PluginConfigContext::default(),
            events: ServiceEventEmitter::default(),
            diagnostics: None,
            cancellation: ServiceCancellation::default(),
            bridge: ServiceBridge::default(),
            transient_progress_limits: TransientProgressLimits::default(),
        });
        assert!(response.error.is_none(), "{:?}", response.error);
        let prepared: bcode_workflow::WorkflowBlockPreparationResponse =
            serde_json::from_slice(&response.payload).unwrap();
        prepared.validate().unwrap();
        assert_eq!(
            prepared.input_sha256,
            bcode_workflow::workflow_block_input_sha256(&input).unwrap()
        );
        assert_eq!(prepared.descriptor["payload"], payload);
        assert_eq!(prepared.descriptor["completion_requirement"], "foreground");
    }

    #[test]
    fn await_rejects_foreground_and_defaults_to_associated_work() {
        let input = json!({"request_json":"{}","completion_requirement":"foreground"});
        let block = bcode_workflow::WorkflowBlockDefinition {
            block_id: AWAIT_OPERATION.into(),
            block_version: 2,
            plugin_id: crate::PLUGIN_ID.into(),
            operation: AWAIT_OPERATION.into(),
            input: bcode_workflow::ValueSchema {
                type_name: "bcode.harness.managed-operation/v1".into(),
                schema: json!({"type":"object"}),
            },
            output: bcode_workflow::ValueSchema {
                type_name: "bcode.harness.assignment-result/v2".into(),
                schema: json!({"type":"object"}),
            },
            effect: bcode_workflow::WorkflowBlockEffect::Mutating,
            resources: vec![],
            authorization: bcode_workflow::WorkflowBlockAuthorization {
                capability: bcode_workflow::WorkflowToolCapability::Mutating,
                explicit_grant_required: true,
            },
            timeout_ms: 30000,
            cancellation_supported: true,
            reconciliation: bcode_workflow::WorkflowBlockReconciliation::ReceiptStatus,
            automatic_retry: None,
            preparation_required: true,
        };
        assert!(prepared_execution_requirement(&block, &input).is_err());
        assert_eq!(
            prepared_execution_requirement(&block, &json!({"request_json":"{}"})).unwrap(),
            bcode_harness_models::HarnessCompletionRequirement::AssociatedWork
        );
    }

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
