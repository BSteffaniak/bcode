//! Amazon Bedrock live model discovery.

use crate::{Error, Result, generated_at};
use aws_config::BehaviorVersion;
use aws_config::meta::region::RegionProviderChain;
use aws_sdk_bedrock::Client;
use aws_sdk_bedrock::config::Region;
use aws_sdk_bedrock::types::{FoundationModelSummary, ModelModality};
use bcode_model_catalog_models::{CatalogCapabilities, LiveCatalogSnapshot, LiveModel};
use std::collections::BTreeMap;

/// Bedrock discovery behavior.
#[derive(Debug, Clone, Copy, Default)]
pub struct DiscoveryOptions {
    /// Require price-list retrieval and at least one model-price match per region.
    pub require_pricing: bool,
}

/// Discover live Bedrock models across the provided regions.
///
/// Price-list failures are best-effort by default. Use [`discover_with_options`] when a generated
/// artifact must contain pricing.
///
/// # Errors
///
/// Returns an error when an AWS API call fails.
pub async fn discover(regions: &[String]) -> Result<LiveCatalogSnapshot> {
    discover_with_options(regions, DiscoveryOptions::default()).await
}

/// Discover live Bedrock models with explicit generation requirements.
///
/// # Errors
///
/// Returns an error when an AWS API call fails, or when required pricing cannot be fetched or
/// matched to a discovered model.
pub async fn discover_with_options(
    regions: &[String],
    options: DiscoveryOptions,
) -> Result<LiveCatalogSnapshot> {
    let mut snapshot = LiveCatalogSnapshot::empty("bedrock", generated_at());
    for region in regions {
        discover_region(region, &mut snapshot, options).await?;
    }
    Ok(snapshot)
}

async fn discover_region(
    region: &str,
    snapshot: &mut LiveCatalogSnapshot,
    options: DiscoveryOptions,
) -> Result<()> {
    let region_provider = RegionProviderChain::first_try(Region::new(region.to_string()));
    let config = aws_config::defaults(BehaviorVersion::latest())
        .region(region_provider)
        .load()
        .await;
    let client = Client::new(&config);
    let pricing_result = crate::bedrock_pricing::fetch_region(region).await;
    let pricing = match pricing_result {
        Ok(pricing) => pricing,
        Err(error) if options.require_pricing => {
            return Err(Error::Provider(format!(
                "bedrock pricing fetch {region}: {error}"
            )));
        }
        Err(_) => BTreeMap::new(),
    };
    let output = client
        .list_foundation_models()
        .send()
        .await
        .map_err(|error| {
            Error::Provider(format!("bedrock list_foundation_models {region}: {error}"))
        })?;

    let pricing_matched = output.model_summaries().iter().any(|summary| {
        pricing.contains_key(&normalize_pricing_name(
            summary.model_name.as_deref().unwrap_or(&summary.model_id),
        ))
    });
    for summary in output.model_summaries() {
        merge_summary(snapshot, region, summary, &pricing);
    }
    if options.require_pricing && !pricing_matched {
        return Err(Error::Provider(format!(
            "bedrock pricing for {region} did not match any discovered models"
        )));
    }
    Ok(())
}

fn merge_summary(
    snapshot: &mut LiveCatalogSnapshot,
    region: &str,
    summary: &FoundationModelSummary,
    pricing: &BTreeMap<String, bcode_model_catalog_models::CatalogPricing>,
) {
    let entry = snapshot
        .models
        .entry(summary.model_id.clone())
        .or_insert_with(|| live_model_from_summary(summary));
    entry.regions.insert(region.to_string());
    if entry.pricing.is_none() {
        entry.pricing = pricing_for_model(
            summary.model_name.as_deref().unwrap_or(&summary.model_id),
            pricing,
        );
    }
}

fn pricing_for_model(
    model_name: &str,
    pricing: &BTreeMap<String, bcode_model_catalog_models::CatalogPricing>,
) -> Option<bcode_model_catalog_models::CatalogPricing> {
    pricing.get(&normalize_pricing_name(model_name)).cloned()
}

fn normalize_pricing_name(value: &str) -> String {
    value
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect()
}

fn live_model_from_summary(summary: &FoundationModelSummary) -> LiveModel {
    LiveModel {
        model_id: summary.model_id.clone(),
        target: None,
        display_name: summary.model_name.clone(),
        aliases: std::collections::BTreeSet::new(),
        status: summary
            .model_lifecycle
            .as_ref()
            .map(|lifecycle| format!("{lifecycle:?}")),
        regions: std::collections::BTreeSet::new(),
        capabilities: capabilities_from_summary(summary),
        context_window: None,
        max_output_tokens: None,
        reasoning: None,
        pricing: None,
        raw: Some(raw_summary(summary)),
    }
}

fn capabilities_from_summary(summary: &FoundationModelSummary) -> CatalogCapabilities {
    let input_modalities = summary.input_modalities.as_deref().unwrap_or_default();
    let output_modalities = summary.output_modalities.as_deref().unwrap_or_default();
    CatalogCapabilities {
        text_input: has_modality(input_modalities, "Text"),
        image_input: has_modality(input_modalities, "Image"),
        text_output: has_modality(output_modalities, "Text"),
        tool_use: false,
        parallel_tool_calls: None,
        structured_outputs: false,
        reasoning: false,
        prompt_cache: false,
        native_web_search: false,
    }
}

fn has_modality(modalities: &[ModelModality], expected: &str) -> bool {
    modalities
        .iter()
        .any(|modality| format!("{modality:?}").eq_ignore_ascii_case(expected))
}

fn raw_summary(summary: &FoundationModelSummary) -> serde_json::Value {
    let mut object = BTreeMap::new();
    object.insert("model_arn", serde_json::json!(summary.model_arn));
    object.insert("model_id", serde_json::json!(summary.model_id));
    object.insert("model_name", serde_json::json!(summary.model_name));
    object.insert("provider_name", serde_json::json!(summary.provider_name));
    object.insert(
        "input_modalities",
        serde_json::json!(debug_strings(
            summary.input_modalities.as_deref().unwrap_or_default()
        )),
    );
    object.insert(
        "output_modalities",
        serde_json::json!(debug_strings(
            summary.output_modalities.as_deref().unwrap_or_default()
        )),
    );
    object.insert(
        "response_streaming_supported",
        serde_json::json!(summary.response_streaming_supported),
    );
    object.insert(
        "inference_types_supported",
        serde_json::json!(debug_strings(
            summary
                .inference_types_supported
                .as_deref()
                .unwrap_or_default()
        )),
    );
    object.insert(
        "model_lifecycle",
        serde_json::json!(
            summary
                .model_lifecycle
                .as_ref()
                .map(|lifecycle| format!("{lifecycle:?}"))
        ),
    );
    serde_json::json!(object)
}

fn debug_strings<T: std::fmt::Debug>(values: &[T]) -> Vec<String> {
    values.iter().map(|value| format!("{value:?}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_model_catalog_models::{CatalogPricing, CatalogPricingUnit};

    #[test]
    fn matches_normalized_display_name_and_serializes_pricing() {
        let expected = CatalogPricing {
            currency: "USD".to_string(),
            unit: CatalogPricingUnit::PerMillionTokens,
            input_micros: Some(3_000_000),
            cached_input_micros: None,
            cache_write_input_micros: None,
            output_micros: Some(15_000_000),
            context_threshold_tokens: None,
            rules: Vec::new(),
        };
        let pricing = BTreeMap::from([("claude35sonnet".to_string(), expected.clone())]);

        let matched = pricing_for_model("Claude 3.5 Sonnet", &pricing).expect("pricing match");
        assert_eq!(matched, expected);
        let serialized = serde_json::to_value(matched).expect("serialized pricing");
        assert_eq!(serialized["input_micros"], 3_000_000);
    }
}
