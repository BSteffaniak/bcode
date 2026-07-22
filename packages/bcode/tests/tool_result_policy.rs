use bcode::{
    Agent, AgentRuntime, ToolCall, ToolDefinition, ToolFailurePolicy, ToolResultPolicy,
    ToolSideEffect,
};
use bcode_tool::{
    ImageContent, ImageMetadata, ImageRefContent, ToolInvocationResponse, ToolPolicyMetadata,
    ToolResultContent, ToolUiMetadata,
};
use std::num::NonZeroUsize;

fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "inspect".to_string(),
        description: "return mixed content".to_string(),
        input_schema: serde_json::json!({"type": "object"}),
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: false,
        policy: ToolPolicyMetadata::default(),
        ui: ToolUiMetadata::default(),
    }
}

#[tokio::test]
async fn model_result_is_bounded_and_redacted_without_mutating_application_result() {
    let runtime = AgentRuntime::new().with_tool_result_policy(
        ToolResultPolicy::new()
            .max_text_bytes(NonZeroUsize::new(20).expect("twenty is non-zero"))
            .max_binary_bytes(NonZeroUsize::new(4).expect("four is non-zero"))
            .max_content_items(NonZeroUsize::new(4).expect("four is non-zero"))
            .redact_value("super-secret"),
    );
    let agent = Agent::builder()
        .runtime(runtime)
        .inline_tool(definition(), |_| {
            Ok(ToolInvocationResponse {
                output: "prefix super-secret and a long suffix".to_string(),
                is_error: false,
                content: vec![
                    ToolResultContent::Text {
                        text: "content super-secret and more text".to_string(),
                    },
                    ToolResultContent::Image {
                        image: ImageContent {
                            mime_type: "image/png".to_string(),
                            data_base64: "AAAAAAAAAAAAAAAA".to_string(),
                            metadata: ImageMetadata::default(),
                        },
                    },
                    ToolResultContent::ImageRef {
                        image: ImageRefContent {
                            path: "/private/super-secret/image.png".to_string(),
                            mime_type: "image/png".to_string(),
                            metadata: ImageMetadata {
                                source_path: Some("/private/super-secret/source.png".to_string()),
                                ..ImageMetadata::default()
                            },
                        },
                    },
                ],
                full_output: Some("application-only super-secret full output".to_string()),
                result: None,
            })
        })
        .build();

    let output = agent
        .execute_tool_call(&ToolCall {
            id: "call-result-policy".to_string(),
            name: "inspect".to_string(),
            arguments: serde_json::json!({}),
        })
        .await
        .expect("tool should execute");

    assert!(output.invocation.output.contains("super-secret"));
    assert!(
        output
            .invocation
            .full_output
            .as_deref()
            .is_some_and(|full| full.contains("super-secret"))
    );
    assert!(!output.model_result.output.contains("super-secret"));
    assert!(output.model_result.output.len() <= 20);
    assert_eq!(output.model_result.content.len(), 1);
    assert!(matches!(
        &output.model_result.content[0],
        bcode::ToolResultContent::Text { text }
            if !text.contains("super-secret") && text.len() <= 20
    ));
    assert_eq!(output.model_transform.redaction_count, 4);
    assert_eq!(output.model_transform.truncated_text_fields, 2);
    assert_eq!(output.model_transform.omitted_binary_fields, 1);
    assert_eq!(output.model_transform.omitted_reference_fields, 1);
    assert_eq!(output.model_transform.omitted_content_items, 0);
    let event_result = output
        .events
        .iter()
        .find_map(|event| match event {
            bcode::AgentEvent::ToolResult(result) => Some(result),
            _ => None,
        })
        .expect("model result event");
    assert_eq!(event_result, &output.model_result);
}

#[tokio::test]
async fn model_visible_handler_errors_follow_the_same_result_policy() {
    let runtime = AgentRuntime::new().with_tool_result_policy(
        ToolResultPolicy::new()
            .max_text_bytes(NonZeroUsize::new(18).expect("eighteen is non-zero"))
            .redact_value("handler-secret"),
    );
    let agent = Agent::builder()
        .runtime(runtime)
        .tool_failure_policy(ToolFailurePolicy::ReturnToModel)
        .inline_tool(definition(), |_| {
            Err("handler-secret failure detail that is too long".to_string())
        })
        .build();

    let output = agent
        .execute_tool_call(&ToolCall {
            id: "call-error-policy".to_string(),
            name: "inspect".to_string(),
            arguments: serde_json::json!({}),
        })
        .await
        .expect("handler error should become a model-visible result");

    assert!(output.invocation.output.contains("handler-secret"));
    assert!(!output.model_result.output.contains("handler-secret"));
    assert!(output.model_result.output.len() <= 18);
    assert_eq!(output.model_transform.redaction_count, 1);
    assert_eq!(output.model_transform.truncated_text_fields, 1);
}

#[tokio::test]
async fn text_truncation_preserves_utf8_boundaries() {
    let runtime = AgentRuntime::new().with_tool_result_policy(
        ToolResultPolicy::new().max_text_bytes(NonZeroUsize::new(5).expect("five is non-zero")),
    );
    let agent = Agent::builder()
        .runtime(runtime)
        .inline_tool(definition(), |_| {
            Ok(ToolInvocationResponse {
                output: "ééé".to_string(),
                is_error: false,
                content: Vec::new(),
                full_output: None,
                result: None,
            })
        })
        .build();

    let output = agent
        .execute_tool_call(&ToolCall {
            id: "call-utf8".to_string(),
            name: "inspect".to_string(),
            arguments: serde_json::json!({}),
        })
        .await
        .expect("tool should execute");

    assert_eq!(output.model_result.output, "éé");
    assert_eq!(output.model_transform.truncated_text_fields, 1);
}

#[test]
fn policy_debug_never_exposes_redaction_values() {
    let policy = ToolResultPolicy::new().redact_value("do-not-log-this-secret");
    let debug = format!("{policy:?}");

    assert!(!debug.contains("do-not-log-this-secret"));
    assert!(debug.contains("redacted_value_count: 1"));
}

#[tokio::test]
async fn structured_content_count_is_bounded_deterministically() {
    let runtime = AgentRuntime::new().with_tool_result_policy(
        ToolResultPolicy::new().max_content_items(NonZeroUsize::new(1).expect("one is non-zero")),
    );
    let agent = Agent::builder()
        .runtime(runtime)
        .inline_tool(definition(), |_| {
            Ok(ToolInvocationResponse {
                output: "summary".to_string(),
                is_error: false,
                content: vec![
                    ToolResultContent::Text {
                        text: "first".to_string(),
                    },
                    ToolResultContent::Text {
                        text: "second".to_string(),
                    },
                ],
                full_output: None,
                result: None,
            })
        })
        .build();

    let output = agent
        .execute_tool_call(&ToolCall {
            id: "call-content-count".to_string(),
            name: "inspect".to_string(),
            arguments: serde_json::json!({}),
        })
        .await
        .expect("tool should execute");

    assert_eq!(output.invocation.content.len(), 2);
    assert_eq!(output.model_result.content.len(), 1);
    assert_eq!(output.model_transform.omitted_content_items, 1);
}
