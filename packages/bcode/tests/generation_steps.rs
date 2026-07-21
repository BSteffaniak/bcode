use bcode::{
    AgentEvent, AgentTurnResponse, GenerateTextResponse, GenerationStep, StopReason, ToolCall,
    ToolResult,
};

#[test]
fn generation_response_exposes_ordered_model_tool_and_final_steps() {
    let call = ToolCall {
        id: "call-1".to_string(),
        name: "lookup".to_string(),
        arguments: serde_json::json!({"query": "rust"}),
    };
    let result = ToolResult {
        call_id: "call-1".to_string(),
        output: "result".to_string(),
        is_error: false,
        content: Vec::new(),
    };
    let response = GenerateTextResponse::from(AgentTurnResponse {
        text: "final answer".to_string(),
        stop_reason: Some(StopReason::EndTurn),
        usage: None,
        latency_ms: 42,
        events: vec![
            AgentEvent::TurnStarted,
            AgentEvent::TextDelta("checking".to_string()),
            AgentEvent::ToolCallFinished(call.clone()),
            AgentEvent::ToolResult(result.clone()),
            AgentEvent::TurnStarted,
            AgentEvent::TextDelta("final answer".to_string()),
        ],
    });

    assert_eq!(
        response.steps,
        [
            GenerationStep::Model {
                round: 0,
                text: "checking".to_string(),
                reasoning: String::new(),
                usage: None,
            },
            GenerationStep::ToolCall { round: 0, call },
            GenerationStep::ToolResult { round: 0, result },
            GenerationStep::Model {
                round: 1,
                text: "final answer".to_string(),
                reasoning: String::new(),
                usage: None,
            },
            GenerationStep::FinalResponse {
                text: "final answer".to_string(),
                stop_reason: Some(StopReason::EndTurn),
                latency_ms: 42,
            },
        ]
    );
}
