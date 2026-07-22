//! Optional SDK evaluation integration.
//!
//! This module adapts high-level generation results into provider-independent `bcode_eval`
//! subjects. The `evaluation` crate feature is disabled by default, so model/agent applications
//! that do not score outputs do not compile the eval runner and reporting dependency graph.

pub use bcode_eval::{
    AgentStepCount, LatencyAtMost, NoopSdkEvalObserver, OutputContains, SDK_EVAL_RUN_FILENAME,
    SDK_EVAL_RUN_SCHEMA_VERSION, SdkCriterionScore, SdkEvalCase, SdkEvalCaseResult,
    SdkEvalCriterion, SdkEvalError, SdkEvalObserver, SdkEvalProvenance, SdkEvalReport, SdkEvalRun,
    SdkEvalRunConfig, SdkEvalRunEvent, SdkEvalSubject, SdkEvaluator, StructuredEquals,
    ToolTraceCount, UsageAtMost, load_sdk_eval_run, write_sdk_eval_run,
};

use crate::{GenerateTextResponse, GenerationStep};

/// Build a complete provider-independent eval subject from one text-generation response.
///
/// Ordered SDK steps are serialized without dropping model/tool/final fields. Tool-result steps are
/// also projected into `tool_trace`. Provider-reported token usage and measured latency become
/// numeric measurements; absent usage stays absent rather than being estimated.
///
/// # Errors
///
/// Returns an error when a public generation step cannot be serialized.
pub fn subject_from_response(
    response: &GenerateTextResponse,
) -> Result<SdkEvalSubject, SdkEvalError> {
    let agent_steps = response
        .steps
        .iter()
        .map(|step| {
            serde_json::to_value(step).map_err(|error| SdkEvalError::Criterion(error.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let tool_trace = response
        .steps
        .iter()
        .filter(|step| matches!(step, GenerationStep::ToolResult { .. }))
        .map(|step| {
            serde_json::to_value(step).map_err(|error| SdkEvalError::Criterion(error.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut subject = SdkEvalSubject::new(response.text.clone())
        .agent_steps(agent_steps)
        .tool_trace(tool_trace)
        .latency_ms(response.runtime.latency_ms);
    if let Some(usage) = &response.runtime.usage {
        if let Some(tokens) = usage.input_tokens {
            subject = subject.usage("input_tokens", f64::from(tokens));
        }
        if let Some(tokens) = usage.output_tokens {
            subject = subject.usage("output_tokens", f64::from(tokens));
        }
        if let Some(tokens) = usage.metered_total_tokens() {
            subject = subject.usage("total_tokens", f64::from(tokens));
        }
        if let Some(tokens) = usage.cached_input_tokens {
            subject = subject.usage("cached_input_tokens", f64::from(tokens));
        }
        if let Some(tokens) = usage.reasoning_tokens {
            subject = subject.usage("reasoning_tokens", f64::from(tokens));
        }
    }
    Ok(subject)
}

/// Build an eval subject from text generation and one decoded structured value.
///
/// # Errors
///
/// Returns an error when the structured value or a public generation step cannot be serialized.
pub fn subject_from_structured_response<T: serde::Serialize>(
    response: &GenerateTextResponse,
    value: &T,
) -> Result<SdkEvalSubject, SdkEvalError> {
    let value =
        serde_json::to_value(value).map_err(|error| SdkEvalError::Criterion(error.to_string()))?;
    subject_from_response(response).map(|subject| subject.structured_value(value))
}
