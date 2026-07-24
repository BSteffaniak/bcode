//! Compact context occupancy and token-usage presentation.

use super::theme::{color, radius, space, surface, typeface};
use bcode_session_models::SessionTokenUsage;
use bcode_session_view_models::{SessionRuntimeViewState, UsageView};
use hyperchad::template::{Containers, container};

fn token_value(value: Option<u32>) -> String {
    value.map_or_else(|| "—".to_owned(), |tokens| tokens.to_string())
}

fn usage_details(usage: &SessionTokenUsage) -> Containers {
    container! {
        div margin-top=((space::SM)) color=(color::MUTED) font-size=((typeface::DETAIL)) {
            div direction=row gap=((space::SM)) { span { "input" } span color=(color::TEXT) { (token_value(usage.input_tokens)) } }
            div direction=row gap=((space::SM)) { span { "output" } span color=(color::TEXT) { (token_value(usage.output_tokens)) } }
            div direction=row gap=((space::SM)) { span { "cached input" } span color=(color::TEXT) { (token_value(usage.cached_input_tokens)) } }
            div direction=row gap=((space::SM)) { span { "cache write" } span color=(color::TEXT) { (token_value(usage.cache_write_input_tokens)) } }
            div direction=row gap=((space::SM)) { span { "reasoning" } span color=(color::TEXT) { (token_value(usage.reasoning_tokens)) } }
            div direction=row gap=((space::SM)) { span { "total" } span color=(color::TEXT) { (token_value(usage.metered_total_tokens())) } }
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
        div background=(surface::APP) border=((1, surface::BORDER)) border-radius=((radius::CARD)) padding=((space::S10)) margin-top=((space::S10)) {
            div direction=row gap=((space::S18)) font-size=((typeface::LABEL)) {
                div {
                    span color=(color::MUTED) { "current context " }
                    @if let Some((tokens, qualifier, _)) = context {
                        span color=(color::STRONG) { (tokens.to_string()) " tokens" }
                        span color=(color::MUTED) font-size=((typeface::DETAIL)) { " · " (qualifier) }
                    } @else {
                        span color=(color::MUTED) { "not observed" }
                    }
                }
                div {
                    span color=(color::MUTED) { "session usage " }
                    span color=(color::STRONG) { (runtime.cumulative_metered_tokens.to_string()) " tokens" }
                }
            }
            @if context.is_some() || runtime.latest_usage.is_some() {
                details margin-top=((space::SM)) {
                    summary color=(color::INFO) font-size=((typeface::DETAIL)) { "usage details" }
                    @if let Some((_, _, sequence)) = context {
                        div color=(color::MUTED) font-size=((typeface::DETAIL)) margin-top=((space::SM)) { "context observed at event " (sequence.to_string()) }
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
            div color=(color::STRONG) {
                "Model usage · "
                (usage.usage.metered_total_tokens().map_or_else(|| "total unavailable".to_owned(), |tokens| format!("{tokens} tokens")))
            }
            details margin-top=((space::S6)) {
                summary color=(color::INFO) font-size=((typeface::DETAIL)) { "token breakdown" }
                (usage_details(&usage.usage))
            }
        }
    }
}
