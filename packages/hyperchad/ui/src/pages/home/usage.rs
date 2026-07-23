//! Compact context occupancy and token-usage presentation.

use bcode_session_models::SessionTokenUsage;
use bcode_session_view_models::{SessionRuntimeViewState, UsageView};
use hyperchad::template::{Containers, container};

fn token_value(value: Option<u32>) -> String {
    value.map_or_else(|| "—".to_owned(), |tokens| tokens.to_string())
}

fn usage_details(usage: &SessionTokenUsage) -> Containers {
    container! {
        div margin-top=8 color="#8b949e" font-size=11 {
            div direction=row gap=8 { span { "input" } span color="#c9d1d9" { (token_value(usage.input_tokens)) } }
            div direction=row gap=8 { span { "output" } span color="#c9d1d9" { (token_value(usage.output_tokens)) } }
            div direction=row gap=8 { span { "cached input" } span color="#c9d1d9" { (token_value(usage.cached_input_tokens)) } }
            div direction=row gap=8 { span { "cache write" } span color="#c9d1d9" { (token_value(usage.cache_write_input_tokens)) } }
            div direction=row gap=8 { span { "reasoning" } span color="#c9d1d9" { (token_value(usage.reasoning_tokens)) } }
            div direction=row gap=8 { span { "total" } span color="#c9d1d9" { (token_value(usage.metered_total_tokens())) } }
        }
    }
}

pub(super) fn runtime_usage(runtime: &SessionRuntimeViewState) -> Containers {
    let context = runtime.context_occupancy.as_ref().map(|occupancy| {
        let count = occupancy.observation.context_tokens;
        let qualifier = if count.is_estimated() {
            "estimated"
        } else {
            "measured"
        };
        (count.tokens(), qualifier, occupancy.observation_sequence)
    });
    container! {
        div background="#0d1117" border="1, #30363d" border-radius=8 padding=10 margin-top=10 {
            div direction=row gap=18 font-size=12 {
                div {
                    span color="#8b949e" { "current context " }
                    @if let Some((tokens, qualifier, _)) = context {
                        span color="#f0f6fc" { (tokens.to_string()) " tokens" }
                        span color="#8b949e" font-size=11 { " · " (qualifier) }
                    } @else {
                        span color="#8b949e" { "not observed" }
                    }
                }
                div {
                    span color="#8b949e" { "session usage " }
                    span color="#f0f6fc" { (runtime.cumulative_metered_tokens.to_string()) " tokens" }
                }
            }
            @if context.is_some() || runtime.latest_usage.is_some() {
                details margin-top=8 {
                    summary color="#58a6ff" font-size=11 { "usage details" }
                    @if let Some((_, _, sequence)) = context {
                        div color="#8b949e" font-size=11 margin-top=8 { "context observed at event " (sequence.to_string()) }
                    }
                    @if let Some(usage) = &runtime.latest_usage {
                        (usage_details(usage))
                    }
                }
            }
        }
    }
}

pub(super) fn usage_transcript_item(usage: &UsageView) -> Containers {
    container! {
        div {
            div color="#f0f6fc" {
                "Model usage · "
                (usage.usage.metered_total_tokens().map_or_else(|| "total unavailable".to_owned(), |tokens| format!("{tokens} tokens")))
            }
            details margin-top=6 {
                summary color="#58a6ff" font-size=11 { "token breakdown" }
                (usage_details(&usage.usage))
            }
        }
    }
}
