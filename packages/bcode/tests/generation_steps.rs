use bcode::{
    AgentEvent, AgentLoopTerminationReason, AgentTurnResponse, GenerateTextResponse,
    GenerationStep, ReasoningActivityEvent, ReasoningActivityStatus, ReasoningContentKind,
    ReasoningContentRole, StopReason, TokenUsage, ToolCall, ToolResult,
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
        termination_reason: AgentLoopTerminationReason::ProviderStop,
        events: vec![
            AgentEvent::TurnStarted,
            AgentEvent::TextDelta("checking".to_string()),
            AgentEvent::Usage(TokenUsage {
                input_tokens: Some(10),
                output_tokens: Some(2),
                total_tokens: Some(12),
                ..TokenUsage::default()
            }),
            AgentEvent::Warning("round warning".to_string()),
            AgentEvent::ToolCallFinished(call.clone()),
            AgentEvent::ToolResult(result.clone()),
            AgentEvent::TurnStarted,
            AgentEvent::ReasoningActivity(ReasoningActivityEvent::Started {
                activity_id: "reasoning-1".to_string(),
                order: 0,
            }),
            AgentEvent::ReasoningActivity(ReasoningActivityEvent::PartDelta {
                activity_id: "reasoning-1".to_string(),
                activity_order: 0,
                part_id: "summary-0".to_string(),
                kind: ReasoningContentKind::Summary,
                role: ReasoningContentRole::Milestone,
                part_order: 0,
                text: "first".to_string(),
            }),
            AgentEvent::ReasoningActivity(ReasoningActivityEvent::PartCompleted {
                activity_id: "reasoning-1".to_string(),
                activity_order: 0,
                part_id: "summary-0".to_string(),
                kind: ReasoningContentKind::Summary,
                role: ReasoningContentRole::Milestone,
                part_order: 0,
                text: "first complete".to_string(),
            }),
            AgentEvent::ReasoningActivity(ReasoningActivityEvent::PartCompleted {
                activity_id: "reasoning-1".to_string(),
                activity_order: 0,
                part_id: "raw-0".to_string(),
                kind: ReasoningContentKind::Raw,
                role: ReasoningContentRole::Detail,
                part_order: 1,
                text: "raw detail".to_string(),
            }),
            AgentEvent::ReasoningActivity(ReasoningActivityEvent::Finished {
                activity_id: "reasoning-1".to_string(),
                activity_order: 0,
                status: ReasoningActivityStatus::Completed,
            }),
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
                reasoning_events: Vec::new(),
                usage: Some(TokenUsage {
                    input_tokens: Some(10),
                    output_tokens: Some(2),
                    total_tokens: Some(12),
                    ..TokenUsage::default()
                }),
                metadata: vec![AgentEvent::Warning("round warning".to_string())],
            },
            GenerationStep::ToolCall { round: 0, call },
            GenerationStep::ToolResult { round: 0, result },
            GenerationStep::Model {
                round: 1,
                text: "final answer".to_string(),
                reasoning: "first complete\n\nraw detail".to_string(),
                reasoning_events: vec![
                    ReasoningActivityEvent::Started {
                        activity_id: "reasoning-1".to_string(),
                        order: 0,
                    },
                    ReasoningActivityEvent::PartDelta {
                        activity_id: "reasoning-1".to_string(),
                        activity_order: 0,
                        part_id: "summary-0".to_string(),
                        kind: ReasoningContentKind::Summary,
                        role: ReasoningContentRole::Milestone,
                        part_order: 0,
                        text: "first".to_string(),
                    },
                    ReasoningActivityEvent::PartCompleted {
                        activity_id: "reasoning-1".to_string(),
                        activity_order: 0,
                        part_id: "summary-0".to_string(),
                        kind: ReasoningContentKind::Summary,
                        role: ReasoningContentRole::Milestone,
                        part_order: 0,
                        text: "first complete".to_string(),
                    },
                    ReasoningActivityEvent::PartCompleted {
                        activity_id: "reasoning-1".to_string(),
                        activity_order: 0,
                        part_id: "raw-0".to_string(),
                        kind: ReasoningContentKind::Raw,
                        role: ReasoningContentRole::Detail,
                        part_order: 1,
                        text: "raw detail".to_string(),
                    },
                    ReasoningActivityEvent::Finished {
                        activity_id: "reasoning-1".to_string(),
                        activity_order: 0,
                        status: ReasoningActivityStatus::Completed,
                    },
                ],
                usage: None,
                metadata: Vec::new(),
            },
            GenerationStep::FinalResponse {
                text: "final answer".to_string(),
                stop_reason: Some(StopReason::EndTurn),
                termination_reason: AgentLoopTerminationReason::ProviderStop,
                latency_ms: 42,
            },
        ]
    );
}
