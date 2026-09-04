//! Amazon Bedrock live model discovery.

use crate::{Error, Result, generated_at};
use aws_config::BehaviorVersion;
use aws_config::meta::region::RegionProviderChain;
use aws_sdk_bedrock::Client;
use aws_sdk_bedrock::config::Region;
use aws_sdk_bedrock::types::{FoundationModelSummary, ModelModality};
use bcode_model_catalog_models::{CatalogCapabilities, LiveCatalogSnapshot, LiveModel};
use std::collections::{BTreeMap, BTreeSet};

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
        pricing_for_model(
            summary.model_name.as_deref().unwrap_or(&summary.model_id),
            &summary.model_id,
            &pricing,
        )
        .is_some()
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
            &summary.model_id,
            pricing,
        );
    }
}

/// Resolve price-list pricing for one discovered model.
///
/// Inventory (`ListFoundationModels`) and the price list name the same model differently, so
/// matching proceeds from most to least specific and stops at the first hit:
///
/// 1. exact normalized display name (`"Claude Fable 5.1"` ↔ `"Claude Fable 5.1"`),
/// 2. exact normalized model ID (`"xai.grok-4.6"` is the price-list name for Grok),
/// 3. the display name with a trailing variant qualifier removed
///    (`"Llama 3.1 70B Instruct"` → `"Llama 3.1 70B"`, `"Mistral Large (24.07)"` →
///    `"Mistral Large"`, `"DeepSeek-R1"` → `"R1"` via the vendor-prefix rule below),
/// 4. the longest price-list name that is a prefix of the normalized display name, provided the
///    remainder is a variant qualifier rather than a different generation (so `"Nova Pro"` does
///    not price `"Nova Pro Latency Optimized"` in reverse, and `"Claude 3"` never prices
///    `"Claude 3.5 Sonnet"`).
pub(crate) fn pricing_for_model(
    model_name: &str,
    model_id: &str,
    pricing: &BTreeMap<String, bcode_model_catalog_models::CatalogPricing>,
) -> Option<bcode_model_catalog_models::CatalogPricing> {
    let name = normalize_pricing_name(model_name);
    if let Some(found) = pricing.get(&name) {
        return Some(found.clone());
    }
    if let Some(found) = pricing.get(&normalize_pricing_name(model_id)) {
        return Some(found.clone());
    }
    for candidate in pricing_name_candidates(model_name) {
        if let Some(found) = pricing.get(&candidate) {
            return Some(found.clone());
        }
    }
    pricing
        .iter()
        .filter(|(price_name, _)| {
            name.len() > price_name.len()
                && name.starts_with(price_name.as_str())
                && is_variant_qualifier(&name[price_name.len()..])
        })
        .max_by_key(|(price_name, _)| price_name.len())
        .map(|(_, found)| found.clone())
}

/// Alternate normalized names derived from a display name by stripping variant qualifiers.
fn pricing_name_candidates(model_name: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    let mut base = model_name.trim().to_string();
    // Parenthesised release dates / notes: "Mistral Large (24.07)".
    if let Some(index) = base.find('(') {
        base.truncate(index);
        candidates.push(normalize_pricing_name(base.trim()));
    }
    // Vendor prefix joined with a hyphen: "DeepSeek-R1" is priced as "R1".
    if let Some((_, rest)) = base.split_once('-')
        && rest.chars().next().is_some_and(char::is_alphanumeric)
    {
        candidates.push(normalize_pricing_name(rest));
    }
    // Trailing variant words: "Instruct", "Chat", quantization / precision tags.
    let words = base.split_whitespace().collect::<Vec<_>>();
    for end in (1..words.len()).rev() {
        if VARIANT_WORDS
            .iter()
            .any(|variant| words[end].eq_ignore_ascii_case(variant))
        {
            candidates.push(normalize_pricing_name(&words[..end].join(" ")));
        } else {
            break;
        }
    }
    candidates
}

/// Words that distinguish a serving variant of the same priced model rather than a new model.
const VARIANT_WORDS: &[&str] = &["instruct", "chat", "it", "pt", "bf16", "fp8", "vl", "dense"];

/// Whether the text left over after a price-list name prefix only names a serving variant.
fn is_variant_qualifier(remainder: &str) -> bool {
    if remainder.is_empty() {
        return true;
    }
    // A leading digit would mean a different generation/size ("claude3" + "5sonnet").
    if remainder.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return false;
    }
    let mut rest = remainder;
    while !rest.is_empty() {
        let Some(word) = VARIANT_WORDS
            .iter()
            .find(|variant| rest.starts_with(**variant))
        else {
            return false;
        };
        rest = &rest[word.len()..];
    }
    true
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
        required_tool_choice: None,
        named_tool_choice: None,
        structured_outputs: false,
        reasoning: false,
        prompt_cache: false,
        explicit_prompt_cache: false,
        prompt_cache_ttl_seconds: BTreeSet::new(),
        prompt_cache_min_prefix_tokens: None,
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
            revision: None,
            rules: Vec::new(),
        };
        let pricing = BTreeMap::from([("claude35sonnet".to_string(), expected.clone())]);

        let matched = pricing_for_model(
            "Claude 3.5 Sonnet",
            "anthropic.claude-3-5-sonnet-20240620-v1:0",
            &pricing,
        )
        .expect("pricing match");
        assert_eq!(matched, expected);
        let serialized = serde_json::to_value(matched).expect("serialized pricing");
        assert_eq!(serialized["input_micros"], 3_000_000);
    }

    #[test]
    fn matches_inventory_names_to_price_list_names() {
        let price = |input_micros: u64| CatalogPricing {
            currency: "USD".to_string(),
            unit: CatalogPricingUnit::PerMillionTokens,
            input_micros: Some(input_micros),
            cached_input_micros: None,
            cache_write_input_micros: None,
            output_micros: None,
            context_threshold_tokens: None,
            revision: None,
            rules: Vec::new(),
        };
        let pricing = BTreeMap::from([
            ("llama3170b".to_string(), price(1)),
            ("llama3170blatencyoptimized".to_string(), price(2)),
            ("mistrallarge".to_string(), price(3)),
            ("r1".to_string(), price(4)),
            ("xaigrok46".to_string(), price(5)),
            ("gemma312b".to_string(), price(6)),
            ("novapro".to_string(), price(7)),
            ("claude3".to_string(), price(8)),
            ("qwen332b".to_string(), price(9)),
        ]);
        let input = |name: &str, id: &str| {
            pricing_for_model(name, id, &pricing).and_then(|pricing| pricing.input_micros)
        };

        // Trailing serving-variant word.
        assert_eq!(
            input("Llama 3.1 70B Instruct", "meta.llama3-1-70b-instruct-v1:0"),
            Some(1)
        );
        // Exact display name still wins over the shorter prefix.
        assert_eq!(
            input(
                "Llama 3.1 70B Latency Optimized",
                "meta.llama3-1-70b-instruct-v1:0"
            ),
            Some(2)
        );
        // Parenthesised release qualifier.
        assert_eq!(
            input("Mistral Large (24.07)", "mistral.mistral-large-2407-v1:0"),
            Some(3)
        );
        // Vendor prefix joined with a hyphen.
        assert_eq!(input("DeepSeek-R1", "deepseek.r1-v1:0"), Some(4));
        // Price list keyed by model ID rather than display name.
        assert_eq!(input("Grok 4.6", "xai.grok-4.6"), Some(5));
        // Quantization / task tags.
        assert_eq!(input("Gemma 3 12B IT", "google.gemma-3-12b-it"), Some(6));
        assert_eq!(input("Qwen3 32B (dense)", "qwen.qwen3-32b-v1:0"), Some(9));
        // A shorter prefix must not price a different generation.
        assert_eq!(
            input(
                "Claude 3.5 Sonnet",
                "anthropic.claude-3-5-sonnet-20240620-v1:0"
            ),
            None
        );
        // Nor a different model that merely shares a prefix.
        assert_eq!(input("Nova Pro 2", "amazon.nova-pro-2"), None);
        assert_eq!(input("Nova Premier", "amazon.nova-premier-v1:0"), None);
    }
}
