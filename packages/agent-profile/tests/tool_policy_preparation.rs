use bcode_agent_profile::{
    ToolPolicyAuthorizationMetadata, ToolPolicyOperation, prepare_tool_policy,
    tool_policy_authorization_metadata,
};
use bcode_tool::{
    ToolArgumentExtractor, ToolArgumentKind, ToolDefinition, ToolInvocationDescriptor,
    ToolPolicyMetadata, ToolPreparationRequest, ToolSideEffect, ToolUiMetadata,
};

fn definition(name: &str, kind: ToolArgumentKind, argument: &str) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        description: "policy preparation test".to_string(),
        input_schema: serde_json::json!({"type": "object"}),
        side_effect: ToolSideEffect::WriteFiles,
        requires_permission: true,
        policy: ToolPolicyMetadata {
            aliases: vec!["owner-alias".to_string()],
            permission_category: Some("edit".to_string()),
            argument_extractors: vec![ToolArgumentExtractor {
                kind,
                argument: argument.to_string(),
            }],
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
) -> ToolPolicyAuthorizationMetadata {
    let prepared = prepare_tool_policy(request, definition).expect("preparation should succeed");
    tool_policy_authorization_metadata(&prepared.authorization, &definition.name)
        .expect("policy fact should decode")
}

#[test]
fn preparation_extracts_command_before_authorization() {
    let definition = definition("owner.exec", ToolArgumentKind::Command, "cmd");
    let metadata = metadata(
        &request("owner.exec", serde_json::json!({"cmd": "cargo check"})),
        &definition,
    );

    assert_eq!(
        metadata.operation,
        ToolPolicyOperation::Command {
            command: Some("cargo check".to_string()),
        }
    );
    assert!(metadata.aliases.contains(&"owner.exec".to_string()));
    assert!(metadata.aliases.contains(&"owner-alias".to_string()));
}

#[test]
fn preparation_selects_the_present_resource_for_multi_source_tools() {
    let mut definition = definition("owner.extract", ToolArgumentKind::ReadPath, "path");
    definition
        .policy
        .argument_extractors
        .push(ToolArgumentExtractor {
            kind: ToolArgumentKind::Url,
            argument: "url".to_string(),
        });

    let path_metadata = metadata(
        &request("owner.extract", serde_json::json!({"path": "local.pdf"})),
        &definition,
    );
    assert_eq!(
        path_metadata.operation,
        ToolPolicyOperation::Read {
            paths: vec!["local.pdf".to_string()],
        }
    );

    let url_metadata = metadata(
        &request(
            "owner.extract",
            serde_json::json!({"url": "https://example.com/doc.pdf"}),
        ),
        &definition,
    );
    assert_eq!(
        url_metadata.operation,
        ToolPolicyOperation::Web {
            url: Some("https://example.com/doc.pdf".to_string()),
        }
    );
}

#[test]
fn preparation_extracts_owned_path_arrays_without_policy_argument_parsing() {
    let definition = definition("owner.edit", ToolArgumentKind::WritePath, "files");
    let metadata = metadata(
        &request(
            "owner.edit",
            serde_json::json!({
                "files": ["src/lib.rs", {"path": "src/main.rs"}, {"ignored": true}]
            }),
        ),
        &definition,
    );

    assert_eq!(
        metadata.operation,
        ToolPolicyOperation::Write {
            paths: vec!["src/lib.rs".to_string(), "src/main.rs".to_string()],
            category: "edit".to_string(),
        }
    );
}
