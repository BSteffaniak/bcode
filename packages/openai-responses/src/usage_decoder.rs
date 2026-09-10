//! `OpenAI` billing protocol selection; contains no transport, credentials or catalog behavior.

#[cfg(test)]
mod tests;

use super::usage::{OpenAiUsage, OpenAiUsageContext, normalize_usage};
use bcode_model::{TokenUsage, UsageCaptureSpec, UsageDecoder};
use bcode_session_models::OriginalUsage;

/// Serving differences that affect normalization, explicitly selected by the provider.
#[derive(Debug, Clone, Copy)]
pub enum ResponsesUsageDialect {
    /// Native `OpenAI` API or Codex Responses.
    OpenAi,
    /// Bedrock Mantle Responses with per-bucket cache retention.
    Bedrock,
    /// Chat Completions prompt/completion field aliases.
    ChatCompletions,
}

impl UsageDecoder for ResponsesUsageDialect {
    fn capture_spec(&self) -> UsageCaptureSpec {
        match self {
            Self::OpenAi | Self::Bedrock => UsageCaptureSpec {
                api_shape: "responses",
                containers: &[&["response"], &[]],
                default_source: "usage",
                confirmed_fields: &["model", "service_tier", "prompt_cache_retention"],
                complete_sources: &[
                    "response.completed",
                    "response.done",
                    "response.incomplete",
                    "response.failed",
                ],
            },
            Self::ChatCompletions => UsageCaptureSpec {
                api_shape: "chat_completions",
                containers: &[&[]],
                default_source: "usage",
                confirmed_fields: &["model", "service_tier", "prompt_cache_retention"],
                complete_sources: &["usage"],
            },
        }
    }
    fn normalize(&self, original: &OriginalUsage) -> Result<TokenUsage, String> {
        let report = original.reports.last().ok_or("missing usage report")?;
        if !self
            .capture_spec()
            .complete_sources
            .contains(&report.source.as_str())
            && report.source != "usage"
        {
            return Err("unsupported usage source".into());
        }
        let mut usage: OpenAiUsage =
            serde_json::from_str(&report.usage_json).map_err(|_| "invalid usage report")?;
        if let Some(tier) = report.confirmed.get("service_tier") {
            usage.service_tier = Some(tier.clone());
        }
        let model = report
            .confirmed
            .get("model")
            .or_else(|| original.requested.get("model"))
            .cloned();
        let retention = report
            .confirmed
            .get("prompt_cache_retention")
            .or_else(|| original.requested.get("prompt_cache_retention"));
        let tier = report
            .confirmed
            .get("service_tier")
            .or_else(|| {
                original.requested.get("service_tier").filter(|tier| {
                    matches!(self, Self::Bedrock) || matches!(tier.as_str(), "default" | "standard")
                })
            })
            .cloned();
        let mut normalized = normalize_usage(
            usage,
            OpenAiUsageContext {
                model,
                service_tier: tier,
                prompt_cache_retention: retention.cloned(),
            },
        );
        if matches!(self, Self::Bedrock) {
            normalized.pricing_context.cache_ttl_seconds =
                retention.and_then(|ttl| match ttl.as_str() {
                    "30m" => Some(1800),
                    "1h" => Some(3600),
                    _ => None,
                });
            if !normalized.has_valid_input_breakdown() {
                normalized.pricing_context.request_input_tokens = None;
            }
            if normalized.details.is_empty() {
                normalized.details = super::usage::text_usage_details(&normalized);
            } else {
                for detail in &mut normalized.details {
                    if matches!(
                        detail.bucket,
                        bcode_model::ModelPricingBucket::CacheReadInput
                            | bcode_model::ModelPricingBucket::CacheWriteInput
                    ) {
                        detail.cache_ttl_seconds = normalized.pricing_context.cache_ttl_seconds;
                    }
                }
            }
        }
        Ok(normalized)
    }
}
