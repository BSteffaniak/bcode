#![cfg(feature = "testing")]

use bcode::{
    Agent, ModelPricingInfo, ModelPricingSource, ModelPricingUnit, ModelTokenPrice,
    ProviderTurnEvent, StopReason, TokenUsage, ToolCall, ToolDefinition, ToolSideEffect,
    ToolUiMetadata,
    testing::{ScriptedPermissionPolicy, ScriptedProvider, ScriptedProviderTurn},
};
use bcode::{PermissionDecision, ToolInvocationResponse, ToolPolicyMetadata};

fn tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "lookup".to_string(),
        description: "Lookup data".to_string(),
        input_schema: serde_json::json!({"type":"object"}),
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: false,
        policy: ToolPolicyMetadata::default(),
        ui: ToolUiMetadata::default(),
    }
}

fn tool_response() -> ToolInvocationResponse {
    ToolInvocationResponse {
        output: "done".to_string(),
        is_error: false,
        content: Vec::new(),
        full_output: None,
        result: None,
    }
}

#[tokio::test]
async fn multi_round_response_exposes_aggregate_usage_and_cost() {
    let mut provider = ScriptedProvider::new([
        ScriptedProviderTurn::new().events([
            ProviderTurnEvent::ToolCallFinished {
                call: ToolCall {
                    id: "call-1".to_string(),
                    name: "lookup".to_string(),
                    arguments: serde_json::json!({}),
                },
            },
            ProviderTurnEvent::Usage {
                usage: TokenUsage {
                    input_tokens: Some(10),
                    output_tokens: Some(2),
                    total_tokens: Some(12),
                    cached_input_tokens: Some(3),
                    reasoning_tokens: Some(0),
                    ..TokenUsage::default()
                },
            },
            ProviderTurnEvent::TurnFinished {
                stop_reason: StopReason::ToolCall,
            },
        ]),
        ScriptedProviderTurn::new().events([
            ProviderTurnEvent::TextDelta {
                text: "answer".to_string(),
            },
            ProviderTurnEvent::Usage {
                usage: TokenUsage {
                    input_tokens: Some(20),
                    output_tokens: Some(4),
                    total_tokens: Some(24),
                    cached_input_tokens: Some(0),
                    reasoning_tokens: Some(1),
                    ..TokenUsage::default()
                },
            },
            ProviderTurnEvent::TurnFinished {
                stop_reason: StopReason::EndTurn,
            },
        ]),
    ]);
    let response = Agent::builder()
        .custom_permission_policy(ScriptedPermissionPolicy::new([PermissionDecision::Allow]))
        .inline_tool(tool_definition(), |_| Ok(tool_response()))
        .build()
        .run(&mut provider, "go")
        .await
        .expect("agent turn");

    assert_eq!(response.text, "answer");
    let usage = response.usage().expect("aggregate usage");
    assert_eq!(usage.input_tokens, Some(30));
    assert_eq!(usage.output_tokens, Some(6));
    assert_eq!(usage.total_tokens, Some(36));
    assert_eq!(usage.cached_input_tokens, Some(3));
    assert_eq!(usage.reasoning_tokens, Some(1));

    let pricing = ModelPricingInfo {
        currency: "USD".to_string(),
        unit: ModelPricingUnit::PerMillionTokens,
        input: Some(ModelTokenPrice::from_micros(1_000_000)),
        cached_input: Some(ModelTokenPrice::from_micros(100_000)),
        cache_write_input: None,
        output: Some(ModelTokenPrice::from_micros(2_000_000)),
        source: ModelPricingSource::UserOverride,
    };
    let cost = response.estimated_cost(&pricing).expect("estimated cost");
    assert_eq!(cost.currency, "USD");
    assert_eq!(cost.total_micros, 39);
    assert_eq!(cost.source, ModelPricingSource::UserOverride);
}
