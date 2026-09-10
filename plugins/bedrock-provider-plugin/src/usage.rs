//! Bedrock billing semantics, independent of streaming text/tools and capture lifecycle.
use super::{
    anthropic_messages_pricing_details, complete_request_input_tokens, converse_pricing_details,
    json_u32,
};
use bcode_model::{TokenUsage, UsageCaptureSpec, UsageDecoder};
use bcode_session_models::OriginalUsage;

pub struct MessagesUsage;
pub struct ConverseUsage;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn messages_live_partial_fold_matches_offline_and_keeps_mixed_ttl() {
        let reports = [
            (
                "message_start",
                r#"{"input_tokens":10,"output_tokens":0,"cache_read_input_tokens":20,"cache_creation_input_tokens":30,"cache_creation":{"ephemeral_5m_input_tokens":10,"ephemeral_1h_input_tokens":20}}"#,
            ),
            ("message_delta", r#"{"output_tokens":5}"#),
        ];
        let mut original = OriginalUsage {
            provider_id: "bcode.bedrock-provider".into(),
            api_shape: "messages".into(),
            complete: true,
            ..Default::default()
        };
        let mut previous = None;
        for (source, json) in reports {
            let report = bcode_session_models::OriginalUsageReport {
                source: source.into(),
                usage_json: json.into(),
                confirmed: std::collections::BTreeMap::new(),
            };
            let frame = OriginalUsage {
                reports: vec![report.clone()],
                ..original.clone()
            };
            previous = Some(MessagesUsage.observe(previous.as_ref(), &frame).unwrap());
            original.reports.push(report);
        }
        let usage = MessagesUsage.normalize(&original).unwrap();
        assert_eq!(Some(usage.clone()), previous);
        assert_eq!(usage.input_tokens, Some(60));
        assert_eq!(usage.output_tokens, Some(5));
        assert_eq!(
            usage
                .details
                .iter()
                .filter(|detail| detail.bucket == bcode_model::ModelPricingBucket::CacheWriteInput)
                .map(|detail| (detail.cache_ttl_seconds, detail.tokens))
                .collect::<Vec<_>>(),
            vec![(Some(300), 10), (Some(3600), 20)]
        );
    }
}

#[derive(Default)]
struct MessagesTotals {
    usage: Option<TokenUsage>,
    cache_ttl_seconds: Option<u64>,
    billing_scope: Option<String>,
}
impl MessagesTotals {
    fn record_usage(&mut self, usage: Option<&serde_json::Value>) {
        let Some(usage) = usage else {
            return;
        };
        let read = |key| usage.get(key).and_then(json_u32);
        // Messages reports cumulative snapshots, not deltas. A later reported field
        // replaces the earlier value; omitted fields retain their previous observation.
        let merge = |previous: Option<u32>, current: Option<u32>| current.or(previous);
        let previous = self.usage.take().unwrap_or_default();
        let previous_ordinary_input = previous.uncached_input_tokens();
        let ordinary_input_tokens = merge(previous_ordinary_input, read("input_tokens"));
        let output_tokens = merge(previous.output_tokens, read("output_tokens"));
        let cached_input_tokens = merge(
            previous.cached_input_tokens,
            read("cache_read_input_tokens"),
        );
        let cache_write_input_tokens = merge(
            previous.cache_write_input_tokens,
            read("cache_creation_input_tokens"),
        );
        let input_tokens = ordinary_input_tokens.and_then(|ordinary| {
            ordinary
                .checked_add(cached_input_tokens.unwrap_or_default())?
                .checked_add(cache_write_input_tokens.unwrap_or_default())
        });
        let cache_ttl_seconds = (cache_write_input_tokens.unwrap_or_default() > 0)
            .then_some(self.cache_ttl_seconds.unwrap_or(300));
        let mut details = anthropic_messages_pricing_details(
            input_tokens,
            output_tokens,
            cached_input_tokens,
            cache_write_input_tokens,
            cache_ttl_seconds,
        )
        .into_vec();
        // A single request can write both short- and long-lived cache entries.
        let writes = if let Some(creation) = usage.get("cache_creation") {
            [
                ("ephemeral_5m_input_tokens", 300),
                ("ephemeral_1h_input_tokens", 3600),
            ]
            .into_iter()
            .filter_map(|(key, ttl)| {
                creation
                    .get(key)
                    .and_then(json_u32)
                    .map(|tokens| (ttl, tokens))
            })
            .collect::<Vec<_>>()
        } else {
            previous
                .details
                .iter()
                .filter(|detail| detail.bucket == bcode_model::ModelPricingBucket::CacheWriteInput)
                .filter_map(|detail| detail.cache_ttl_seconds.map(|ttl| (ttl, detail.tokens)))
                .collect()
        };
        if !writes.is_empty() {
            details
                .retain(|detail| detail.bucket != bcode_model::ModelPricingBucket::CacheWriteInput);
            details.extend(writes.into_iter().map(|(ttl, tokens)| {
                bcode_model::ModelTokenUsageDetail {
                    bucket: bcode_model::ModelPricingBucket::CacheWriteInput,
                    modality: bcode_model::ModelTokenModality::Text,
                    tokens,
                    cache_ttl_seconds: Some(ttl),
                }
            }));
        }
        self.usage = Some(TokenUsage {
            input_tokens,
            output_tokens,
            cached_input_tokens,
            cache_write_input_tokens,
            details: details.into_boxed_slice(),
            pricing_context: Box::new(bcode_model::ModelPricingContext {
                service_tier: Some("standard".to_string()),
                invocation_class: Some(bcode_model::ModelInvocationClass::OnDemand),
                request_input_tokens: input_tokens.map(u64::from),
                billing_scope: self.billing_scope.clone(),
                cache_ttl_seconds,
            }),
            ..TokenUsage::default()
        });
    }
}
impl UsageDecoder for MessagesUsage {
    fn capture_spec(&self) -> UsageCaptureSpec {
        UsageCaptureSpec {
            api_shape: "messages",
            containers: &[&["message"], &[]],
            default_source: "usage",
            confirmed_fields: &["model", "service_tier", "prompt_cache_retention"],
            complete_sources: &["message_stop"],
        }
    }
    fn normalize(&self, original: &OriginalUsage) -> Result<TokenUsage, String> {
        self.observe(None, original)
    }
    fn observe(
        &self,
        previous: Option<&TokenUsage>,
        original: &OriginalUsage,
    ) -> Result<TokenUsage, String> {
        let mut totals = MessagesTotals {
            usage: previous.cloned(),
            cache_ttl_seconds: original
                .requested
                .get("cache_ttl_seconds")
                .and_then(|ttl| ttl.parse().ok()),
            billing_scope: original.requested.get("billing_scope").cloned(),
        };
        for report in &original.reports {
            if !matches!(report.source.as_str(), "message_start" | "message_delta") {
                return Err("unsupported Messages usage source".into());
            }
            let value =
                serde_json::from_str(&report.usage_json).map_err(|_| "invalid Messages usage")?;
            totals.record_usage(Some(&value));
        }
        totals.usage.ok_or_else(|| "missing Messages usage".into())
    }
}
impl UsageDecoder for ConverseUsage {
    fn capture_spec(&self) -> UsageCaptureSpec {
        UsageCaptureSpec {
            api_shape: "converse_sdk",
            containers: &[&[]],
            default_source: "metadata",
            confirmed_fields: &[],
            complete_sources: &["metadata"],
        }
    }
    fn normalize(&self, original: &OriginalUsage) -> Result<TokenUsage, String> {
        let report = original.reports.last().ok_or("missing SDK usage")?;
        let value: serde_json::Value =
            serde_json::from_str(&report.usage_json).map_err(|_| "invalid SDK usage")?;
        let read = |key| value.get(key).and_then(json_u32);
        let ordinary = read("inputTokens");
        let cached = read("cacheReadInputTokens");
        let written = read("cacheWriteInputTokens");
        let output = read("outputTokens");
        let input = ordinary.and_then(|input| {
            u32::try_from(complete_request_input_tokens(input, cached, written)).ok()
        });
        Ok(TokenUsage {
            input_tokens: input,
            output_tokens: output,
            total_tokens: input
                .zip(output)
                .and_then(|(input, output)| input.checked_add(output)),
            cached_input_tokens: cached,
            cache_write_input_tokens: written,
            details: converse_pricing_details(ordinary, output, cached, written),
            pricing_context: Box::new(bcode_model::ModelPricingContext {
                service_tier: Some("standard".into()),
                invocation_class: Some(bcode_model::ModelInvocationClass::OnDemand),
                request_input_tokens: input.map(u64::from),
                ..Default::default()
            }),
            ..Default::default()
        })
    }
}
