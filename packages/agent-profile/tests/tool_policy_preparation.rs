use bcode_agent_profile::{
    ToolPolicyAuthorizationMetadata, ToolPolicyOperation, prepare_tool_policy,
    tool_policy_authorization_metadata,
};
use bcode_tool::{
    ToolDefinition, ToolInvocationDescriptor, ToolPolicyMetadata, ToolPreparationRequest,
    ToolSideEffect, ToolUiMetadata,
};

fn definition(name: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        description: "policy preparation test".to_string(),
        input_schema: serde_json::json!({"type": "object"}),
        side_effect: ToolSideEffect::WriteFiles,
        requires_permission: true,
        policy: ToolPolicyMetadata {
            aliases: vec!["owner-alias".to_string()],
            permission_category: Some("edit".to_string()),
            ..ToolPolicyMetadata::default()
        },
        ui: ToolUiMetadata::default(),
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
    let prepared = prepare_tool_policy(request, definition, operation)
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
        assert_eq!(actual.aliases, vec!["owner-alias"]);
        assert_eq!(actual.permission_category.as_deref(), Some("edit"));
    }
}

#[test]
fn preparation_does_not_infer_from_definition_side_effect_or_arguments() {
    let mut definition = definition("owner.tool");
    definition.side_effect = ToolSideEffect::ReadOnly;
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
        ToolPolicyOperation::ReadOnly,
    )
    .expect_err("mismatched tool identity must fail");
    assert!(error.contains("tool not found"));
}
