#![cfg(feature = "embedded-plugins")]

use bcode::{
    Agent, HeadlessExchangePolicy, ToolCall, ToolDefinition, ToolExchangeResolution,
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

fn agent(policy: HeadlessExchangePolicy) -> Agent {
    let bundled = [bcode_plugin::StaticBundledPlugin::new(
        include_str!("../../../plugins/question-plugin/bcode-plugin.toml"),
        bcode_question_plugin::static_plugin(),
    )];
    let selected = bcode_plugin::filter_selected_static_plugins(
        &bundled,
        &bcode_plugin::PluginSelection::all_enabled(),
    )
    .expect("question plugin manifest should parse");
    let plugins = bcode_plugin::PluginRuntimeHost::from(
        bcode_plugin::PluginHost::load_static_plugins(&selected)
            .expect("question plugin should load statically"),
    );
    Agent::builder()
        .plugin_runtime(plugins)
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
