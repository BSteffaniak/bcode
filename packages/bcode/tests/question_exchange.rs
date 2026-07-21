#![cfg(feature = "embedded-plugins")]

use bcode::{
    Agent, Bcode, HeadlessExchangePolicy, ToolCall, ToolDefinition, ToolExchangeResolution,
    ToolInvocationResponse,
};
use bcode_tool::{ToolPolicyMetadata, ToolSideEffect, ToolUiMetadata};
use std::sync::{Arc, Mutex};

fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "question".to_string(),
        description: "question exchange test".to_string(),
        input_schema: serde_json::json!({"type": "object"}),
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: false,
        policy: ToolPolicyMetadata::default(),
        ui: ToolUiMetadata::default(),
    }
}

fn plugin_runtime() -> bcode_plugin::PluginRuntimeHost {
    let bundled = [bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/question-plugin/bcode-plugin.toml"),
        bcode_question_plugin::static_plugin(),
    )];
    let selected = bcode_plugin::filter_selected_static_plugins(
        &bundled,
        &bcode_plugin::PluginSelection::all_enabled(),
    )
    .expect("question plugin manifest should parse");
    bcode_plugin::PluginRuntimeHost::from(
        bcode_plugin::PluginHost::load_static_plugins(&selected)
            .expect("question plugin should load statically"),
    )
}

fn agent(policy: HeadlessExchangePolicy) -> Agent {
    Agent::builder()
        .plugin_runtime(plugin_runtime())
        .plugin_tool(definition(), "bcode.question")
        .headless_exchange_policy(policy)
        .build()
}

fn question_call(id: &str, required: bool) -> ToolCall {
    ToolCall {
        id: id.to_string(),
        name: "question".to_string(),
        arguments: serde_json::json!({
            "questions": [{
                "question": "Proceed?",
                "options": [{"label": "Yes", "value": "yes"}],
                "required": required
            }]
        }),
    }
}

fn output(response: ToolInvocationResponse) -> serde_json::Value {
    serde_json::from_str(response.full_output.as_deref().expect("full outcome JSON"))
        .expect("outcome JSON parses")
}

#[tokio::test]
async fn sdk_discovers_manifest_declared_plugin_tools_with_owned_metadata() {
    let sdk = Bcode::builder().plugin_runtime(plugin_runtime()).build();

    let tools = sdk
        .discover_tools()
        .await
        .expect("question tool discovery should succeed");
    let question = tools
        .iter()
        .find(|tool| tool.definition.name == "question")
        .expect("question plugin should advertise its tool");

    assert_eq!(question.plugin_id, "bcode.question");
    assert!(!question.definition.description.is_empty());
    assert!(question.definition.input_schema.is_object());
    assert_eq!(question.definition.side_effect, ToolSideEffect::ReadOnly);
}

#[tokio::test]
async fn discovered_tools_register_without_redeclaring_plugin_metadata() {
    let sdk = Bcode::builder().plugin_runtime(plugin_runtime()).build();
    let agent = sdk
        .agent_with_discovered_tools()
        .await
        .expect("question tools should discover")
        .headless_exchange_policy(HeadlessExchangePolicy::Reject)
        .build();

    let output = agent
        .execute_tool_call(&question_call("auto-discovered", true))
        .await
        .expect("routing should reach the plugin and produce a model-visible rejection");
    assert!(output.model_result.is_error);
    assert!(
        output
            .model_result
            .output
            .contains("headless_exchange_rejected")
    );
}

#[tokio::test]
async fn question_exchange_stays_in_one_invocation_and_validates_response() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&observed);
    let response = agent(HeadlessExchangePolicy::Callback(Arc::new(move |request| {
        captured
            .lock()
            .expect("captured exchange lock")
            .push(request.clone());
        ToolExchangeResolution::Responded {
            payload: serde_json::json!({
                "status": "answered",
                "questions": [{
                    "question_index": 0,
                    "selected": ["yes"]
                }]
            }),
        }
    })))
    .execute_tool_call(&question_call("question-call", true))
    .await
    .expect("question invocation should complete")
    .invocation;

    assert!(!response.is_error);
    assert_eq!(output(response)["status"], "answered");
    let exchanges = observed.lock().expect("captured exchange lock");
    assert_eq!(exchanges.len(), 1);
    assert_eq!(exchanges[0].invocation_id, "question-call");
    assert_eq!(exchanges[0].exchange_id, "question-call-question");
    assert_eq!(exchanges[0].schema, "bcode.question.request");
}

#[tokio::test]
async fn question_exchange_handles_optional_fallback_and_required_unsupported() {
    let optional = agent(HeadlessExchangePolicy::Callback(Arc::new(|_| {
        ToolExchangeResolution::NoCompatibleConsumer
    })))
    .execute_tool_call(&question_call("optional", false))
    .await
    .expect("optional question invocation completes")
    .invocation;
    assert!(!optional.is_error);
    assert_eq!(output(optional)["status"], "unanswered");

    let required = agent(HeadlessExchangePolicy::Callback(Arc::new(|_| {
        ToolExchangeResolution::NoCompatibleConsumer
    })))
    .execute_tool_call(&question_call("required", true))
    .await
    .expect("plugin returns a model-visible tool error")
    .invocation;
    assert!(required.is_error);
    assert!(required.output.contains("no compatible consumer"));
}

#[tokio::test]
async fn question_exchange_rejects_invalid_response_payload() {
    let response = agent(HeadlessExchangePolicy::AutoResponse(
        serde_json::json!({"status": "answered", "questions": "invalid"}),
    ))
    .execute_tool_call(&question_call("invalid", true))
    .await
    .expect("plugin returns a model-visible tool error")
    .invocation;

    assert!(response.is_error);
    assert!(
        response
            .output
            .contains("invalid question response payload")
    );
}
