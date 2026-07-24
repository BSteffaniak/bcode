use bcode_agent_profile::{
    ToolPolicyAuthorizationMetadata, ToolPolicyIdentity, ToolPolicyOperation,
    ToolPolicyPreparation, prepare_tool_policy, tool_policy_authorization_metadata,
};
use bcode_tool::{ToolDefinition, ToolInvocationDescriptor, ToolPreparationRequest};

fn definition(name: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        description: "policy preparation test".to_string(),
        input_schema: serde_json::json!({"type": "object"}),
    }
}

fn request(name: &str, arguments: serde_json::Value) -> ToolPreparationRequest {
    ToolPreparationRequest {
        invocation: ToolInvocationDescriptor {
            invocation_id: "call-1".to_string(),
            tool_name: name.to_string(),
            arguments,
        },
        host_context: Vec::new(),
    }
}

fn metadata(
    request: &ToolPreparationRequest,
    definition: &ToolDefinition,
    operation: ToolPolicyOperation,
) -> ToolPolicyAuthorizationMetadata {
    let prepared = prepare_tool_policy(
        request,
        definition,
        ToolPolicyPreparation::new(true, operation).with_identity(ToolPolicyIdentity {
            aliases: vec!["prepared-alias".to_string()],
            compatibility_aliases: Vec::new(),
            capabilities: vec!["prepared-capability".to_string()],
            permission_category: Some("prepared-category".to_string()),
        }),
    )
    .expect("owner policy operation should encode");
    tool_policy_authorization_metadata(&prepared.authorization, &definition.name)
        .expect("owner policy fact should decode")
}

#[test]
fn preparation_preserves_owner_computed_operations_and_resources() {
    for operation in [
        ToolPolicyOperation::Command {
            command: Some("cargo test".to_owned()),
        },
        ToolPolicyOperation::Web {
            url: Some("https://example.com/doc.pdf".to_owned()),
        },
        ToolPolicyOperation::Read {
            paths: vec!["src/lib.rs".to_owned()],
        },
        ToolPolicyOperation::Write {
            paths: vec!["src/main.rs".to_owned()],
            category: "edit".to_owned(),
        },
        ToolPolicyOperation::ReadOnly,
        ToolPolicyOperation::Mutating,
    ] {
        let actual = metadata(
            &request(
                "owner.tool",
                serde_json::json!({"command": "tampered", "path": "wrong"}),
            ),
            &definition("owner.tool"),
            operation.clone(),
        );
        assert_eq!(actual.operation, operation);
        assert!(actual.requires_permission);
        assert_eq!(actual.aliases, vec!["prepared-alias"]);
        assert_eq!(actual.capabilities, vec!["prepared-capability"]);
        assert_eq!(
            actual.permission_category.as_deref(),
            Some("prepared-category")
        );
    }
}

#[test]
fn preparation_does_not_infer_from_definition_side_effect_or_arguments() {
    let definition = definition("owner.tool");
    let metadata = metadata(
        &request(
            "owner.tool",
            serde_json::json!({"command": "rm -rf ignored", "url": "https://ignored"}),
        ),
        &definition,
        ToolPolicyOperation::Mutating,
    );
    assert_eq!(metadata.operation, ToolPolicyOperation::Mutating);
}

#[test]
fn preparation_rejects_mismatched_tool_identity() {
    let error = prepare_tool_policy(
        &request("other.tool", serde_json::Value::Null),
        &definition("owner.tool"),
        bcode_agent_profile::ToolPolicyPreparation::read_only(),
    )
    .expect_err("mismatched tool identity must fail");
    assert!(error.contains("tool not found"));
}
