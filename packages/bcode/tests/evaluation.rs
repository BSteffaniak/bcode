#![cfg(feature = "evaluation")]

use bcode::{
    AgentLoopTerminationReason, AgentTurnResponse, GenerateTextResponse, GenerationStep,
    ModelResponseCacheStatus, StopReason, TokenUsage,
    evaluation::{
        AgentStepCount, LatencyAtMost, OutputContains, SdkCriterionScore, SdkEvalCriterion,
        SdkEvalError, SdkEvalSubject, SdkEvaluator, StructuredEquals, ToolTraceCount, UsageAtMost,
        subject_from_response, subject_from_structured_response,
    },
};

fn response() -> GenerateTextResponse {
    let usage = TokenUsage {
        input_tokens: Some(7),
        output_tokens: Some(3),
        total_tokens: Some(10),
        ..TokenUsage::default()
    };
    GenerateTextResponse {
        text: "useful answer".to_string(),
        steps: vec![
            GenerationStep::Model {
                round: 0,
                text: "".to_string(),
                reasoning: String::new(),
                usage: Some(usage.clone()),
                metadata: Vec::new(),
            },
            GenerationStep::ToolResult {
                round: 0,
                result: bcode::ToolResult {
                    call_id: "call-1".to_string(),
                    output: "tool output".to_string(),
                    is_error: false,
                    content: Vec::new(),
                },
            },
            GenerationStep::FinalResponse {
                text: "useful answer".to_string(),
                stop_reason: Some(StopReason::EndTurn),
                termination_reason: AgentLoopTerminationReason::ProviderStop,
                latency_ms: 25,
            },
        ],
        cache_status: ModelResponseCacheStatus::Bypassed,
        runtime: AgentTurnResponse {
            text: "useful answer".to_string(),
            usage: Some(usage),
            stop_reason: Some(StopReason::EndTurn),
            latency_ms: 25,
            termination_reason: AgentLoopTerminationReason::ProviderStop,
            events: Vec::new(),
        },
    }
}

#[derive(Debug)]
struct CustomCriterion;

impl SdkEvalCriterion for CustomCriterion {
    fn score(&self, subject: &SdkEvalSubject) -> Result<SdkCriterionScore, SdkEvalError> {
        let passed = subject.output.starts_with("useful");
        SdkCriterionScore::new("custom", f64::from(passed), passed)
    }
}

#[test]
fn response_adapter_and_criteria_score_all_claimed_dimensions() {
    let subject = subject_from_structured_response(&response(), &serde_json::json!({"ok": true}))
        .expect("SDK response adapts");
    assert_eq!(subject.tool_trace.len(), 1);
    assert_eq!(subject.agent_steps.len(), 3);
    assert_eq!(subject.latency_ms, Some(25));
    assert_eq!(subject.usage.get("total_tokens"), Some(&10.0));

    let report = SdkEvaluator::new()
        .criterion(OutputContains::new("answer"))
        .criterion(StructuredEquals::new(serde_json::json!({"ok": true})))
        .criterion(ToolTraceCount::new(1))
        .criterion(AgentStepCount::new(3))
        .criterion(LatencyAtMost::new(30))
        .criterion(UsageAtMost::new("total_tokens", 10.0))
        .criterion(CustomCriterion)
        .evaluate(&subject)
        .expect("all criteria score");
    assert!(report.passed);
    assert_eq!(report.scores.len(), 7);
}

#[test]
fn text_response_adapter_preserves_honest_absent_usage() {
    let mut response = response();
    response.runtime.usage = None;
    let subject = subject_from_response(&response).expect("text response adapts");
    assert!(subject.usage.is_empty());
    assert!(matches!(
        SdkEvaluator::new()
            .criterion(UsageAtMost::new("total_tokens", 10.0))
            .evaluate(&subject),
        Err(SdkEvalError::MissingSubjectField { field: "usage" })
    ));
}
