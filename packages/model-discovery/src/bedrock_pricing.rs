//! Amazon Bedrock public price-list normalization for model catalog discovery.
//!
//! Bedrock pricing is published as two AWS Price List offers:
//!
//! * `AmazonBedrock` covers first-party and open-weight models (Nova, Llama, Mistral, `DeepSeek`,
//!   Qwen, ...). Each product carries a `model` attribute plus `inferenceType`/`tokenType`
//!   dimensions.
//! * `AmazonBedrockFoundationModels` covers AWS Marketplace-listed models, which is where every
//!   Anthropic model newer than Claude 3 is priced. Products name the model through
//!   `servicename` (`"Claude Fable 5.1 (Amazon Bedrock Edition)"`) and encode the billed dimension
//!   in `usagetype` (`"USE1-MP:USE1_input_tokens_global_standard-Units"`).
//!
//! Both are normalized into the same [`CatalogPricing`] shape keyed by normalized model name.

use bcode_model_catalog_models::{
    CatalogInvocationClass, CatalogPricing, CatalogPricingBucket, CatalogPricingRule,
    CatalogPricingUnit, CatalogTokenModality,
};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

const PRICE_LIST_BASE_URL: &str = "https://pricing.us-east-1.amazonaws.com/offers/v1.0/aws";
const BEDROCK_OFFER: &str = "AmazonBedrock";
const BEDROCK_MARKETPLACE_OFFER: &str = "AmazonBedrockFoundationModels";
const MARKETPLACE_SERVICE_NAME_SUFFIX: &str = " (Amazon Bedrock Edition)";
const PRICE_LIST_CONTEXT_THRESHOLD_TOKENS: u64 = 272_000;
const FIVE_MINUTES_SECONDS: u64 = 5 * 60;
const THIRTY_MINUTES_SECONDS: u64 = 30 * 60;
const ONE_HOUR_SECONDS: u64 = 60 * 60;

pub async fn fetch_region(
    region: &str,
) -> Result<BTreeMap<String, CatalogPricing>, Box<dyn std::error::Error + Send + Sync>> {
    let client = reqwest::Client::new();
    let mut pricing = parse(fetch_offer(&client, BEDROCK_OFFER, region).await?);
    // Marketplace-listed models are priced in a separate offer. A model appears in exactly one
    // offer, so the union is disjoint; the first-party offer wins on any unexpected overlap.
    for (model, marketplace) in
        parse(fetch_offer(&client, BEDROCK_MARKETPLACE_OFFER, region).await?)
    {
        pricing.entry(model).or_insert(marketplace);
    }
    Ok(pricing)
}

async fn fetch_offer(
    client: &reqwest::Client,
    offer: &str,
    region: &str,
) -> Result<PriceListDocument, Box<dyn std::error::Error + Send + Sync>> {
    let url = format!("{PRICE_LIST_BASE_URL}/{offer}/current/{region}/index.json");
    Ok(client
        .get(url)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await?
        .error_for_status()?
        .json::<PriceListDocument>()
        .await?)
}

fn parse(document: PriceListDocument) -> BTreeMap<String, CatalogPricing> {
    let publication_date = document.publication_date.clone();
    let Some(terms) = document.terms.on_demand else {
        return BTreeMap::new();
    };
    let mut rules = BTreeMap::<String, BTreeSet<CatalogPricingRule>>::new();
    for (sku, product) in document.products {
        let attributes = product.attributes;
        let Some(model) = product_model_name(&attributes) else {
            continue;
        };
        let Some(product_terms) = terms.get(&sku) else {
            continue;
        };
        for term in product_terms.values() {
            for dimension in term.price_dimensions.values() {
                let Some(price) = dimension
                    .price_per_unit
                    .get("USD")
                    .and_then(|price| price_per_million_micros(price, &dimension.unit))
                else {
                    continue;
                };
                if let Some(rule) = pricing_rule(&attributes, price) {
                    rules
                        .entry(normalize_name(&model))
                        .or_default()
                        .insert(rule);
                }
            }
        }
    }
    rules
        .into_iter()
        .map(|(model, rules)| {
            let rules = with_geo_scope_fallback(rules.into_iter().collect::<Vec<_>>());
            let flat = |bucket| {
                let prices = rules
                    .iter()
                    .filter(|rule| {
                        rule.bucket == bucket
                            && rule.modality == Some(CatalogTokenModality::Text)
                            && rule.service_tier.as_deref() == Some("standard")
                            && rule.invocation_class == Some(CatalogInvocationClass::OnDemand)
                            && rule.cache_ttl_seconds.is_none()
                            && rule.min_request_input_tokens.is_none()
                            && rule.max_request_input_tokens.is_none()
                            && rule.billing_scope.as_deref() == Some("in_region")
                    })
                    .map(|rule| rule.price_micros)
                    .collect::<BTreeSet<_>>();
                (prices.len() == 1)
                    .then(|| prices.first().copied())
                    .flatten()
            };
            let threshold = rules
                .iter()
                .any(|rule| rule.min_request_input_tokens.is_some())
                .then_some(PRICE_LIST_CONTEXT_THRESHOLD_TOKENS);
            (
                model,
                CatalogPricing {
                    currency: "USD".to_string(),
                    unit: CatalogPricingUnit::PerMillionTokens,
                    input_micros: flat(CatalogPricingBucket::Input),
                    cached_input_micros: flat(CatalogPricingBucket::CacheReadInput),
                    cache_write_input_micros: flat(CatalogPricingBucket::CacheWriteInput),
                    output_micros: flat(CatalogPricingBucket::Output),
                    context_threshold_tokens: threshold,
                    revision: publication_date.clone(),
                    rules,
                },
            )
        })
        .collect()
}

/// Mirror in-region rules into the `geo` billing scope when the price list has no explicit
/// cross-region rows.
///
/// Geographic inference profiles (`us.`/`eu.`/`apac.` IDs) bill at the signing region's standard
/// rate, so the in-region price is the correct value; without these rows the runtime pricing
/// context for a `us.` model would match no rule and cost estimation would be unavailable.
fn with_geo_scope_fallback(mut rules: Vec<CatalogPricingRule>) -> Vec<CatalogPricingRule> {
    if rules
        .iter()
        .any(|rule| rule.billing_scope.as_deref() == Some("geo"))
    {
        return rules;
    }
    let geo = rules
        .iter()
        .filter(|rule| rule.billing_scope.as_deref() == Some("in_region"))
        .map(|rule| CatalogPricingRule {
            billing_scope: Some("geo".to_string()),
            ..rule.clone()
        })
        .collect::<Vec<_>>();
    rules.extend(geo);
    rules
}

/// Model name a price-list product belongs to, for either offer shape.
fn product_model_name(attributes: &ProductAttributes) -> Option<String> {
    if let Some(model) = attributes
        .model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
    {
        return Some(model.to_string());
    }
    attributes
        .service_name
        .as_deref()
        .and_then(|name| name.strip_suffix(MARKETPLACE_SERVICE_NAME_SUFFIX))
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(ToString::to_string)
}

fn pricing_rule(attributes: &ProductAttributes, price_micros: u64) -> Option<CatalogPricingRule> {
    if attributes
        .model
        .as_deref()
        .is_some_and(|model| !model.is_empty())
    {
        first_party_pricing_rule(attributes, price_micros)
    } else {
        marketplace_pricing_rule(attributes, price_micros)
    }
}

/// Normalize one `AmazonBedrock` offer product (first-party and open-weight models).
fn first_party_pricing_rule(
    attributes: &ProductAttributes,
    price_micros: u64,
) -> Option<CatalogPricingRule> {
    let dimension = attributes
        .token_type
        .as_deref()
        .filter(|value| !value.is_empty())
        .or(attributes.inference_type.as_deref())?
        .to_ascii_lowercase();
    let bucket = if dimension.contains("cache") && dimension.contains("read") {
        CatalogPricingBucket::CacheReadInput
    } else if dimension.contains("cache") && dimension.contains("write") {
        CatalogPricingBucket::CacheWriteInput
    } else if dimension.contains("input") && !dimension.contains("output") {
        CatalogPricingBucket::Input
    } else if dimension.contains("output") {
        CatalogPricingBucket::Output
    } else {
        return None;
    };
    let modality = if dimension.contains("image") {
        CatalogTokenModality::Image
    } else if dimension.contains("audio") || dimension.contains("speech") {
        CatalogTokenModality::Audio
    } else if dimension.contains("video") {
        CatalogTokenModality::Video
    } else {
        CatalogTokenModality::Text
    };
    let long_context = dimension.contains("long-ctx") || dimension.contains("long context");
    let feature = attributes.feature.as_deref().unwrap_or_default();
    let usage = attributes
        .usage_type
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    // Model customization / provisioned rows share the on-demand dimension vocabulary but are
    // not on-demand token prices.
    if feature == "Model Customization" || usage.contains("custom-model") {
        return None;
    }
    let invocation_class = if feature == "Batch Inference"
        || attributes.service_tier.as_deref() == Some("batch")
        || dimension.contains("batch")
    {
        CatalogInvocationClass::Batch
    } else if feature.is_empty()
        || feature == "On-demand Inference"
        || attributes.token_type.is_some()
    {
        CatalogInvocationClass::OnDemand
    } else {
        return None;
    };
    // Newer rows carry `service_tier`; older rows only encode the tier as a suffix on
    // `inferenceType` ("Output tokens priority") / `usagetype` ("...-input-tokens-flex").
    let service_tier = attributes
        .service_tier
        .clone()
        .or_else(|| {
            ["priority", "flex", "batch"]
                .into_iter()
                .find(|tier| {
                    dimension.ends_with(&format!(" {tier}")) || usage.ends_with(&format!("-{tier}"))
                })
                .map(ToString::to_string)
        })
        .or_else(|| {
            (attributes.token_type.is_some() || feature == "On-demand Inference")
                .then_some("standard".to_string())
        });
    Some(CatalogPricingRule {
        bucket,
        modality: Some(modality),
        service_tier,
        invocation_class: Some(invocation_class),
        cache_ttl_seconds: (bucket == CatalogPricingBucket::CacheWriteInput
            && dimension.contains("30m"))
        .then_some(THIRTY_MINUTES_SECONDS),
        min_request_input_tokens: long_context
            .then_some(PRICE_LIST_CONTEXT_THRESHOLD_TOKENS.saturating_add(1)),
        max_request_input_tokens: (!long_context && attributes.token_type.is_some())
            .then_some(PRICE_LIST_CONTEXT_THRESHOLD_TOKENS),
        billing_scope: Some(billing_scope(attributes)),
        price_micros,
    })
}

fn billing_scope(attributes: &ProductAttributes) -> String {
    let usage = attributes.usage_type.as_deref().unwrap_or_default();
    if attributes
        .service_tier
        .as_deref()
        .is_some_and(|tier| tier.starts_with("global-"))
        || usage.contains("cross-region-global")
    {
        "global".to_string()
    } else if usage.contains("cross-region") {
        "geo".to_string()
    } else {
        "in_region".to_string()
    }
}

/// Normalize one `AmazonBedrockFoundationModels` offer product (Marketplace-listed models).
///
/// The billed dimension is only encoded in `usagetype`, after the `<REGION>-MP:<REGION>_` prefix
/// and before the `-Units` suffix. Two vocabularies appear:
///
/// * Newer listings: `input_tokens_standard`, `output_tokens_global_standard`,
///   `cache_read_tokens_standard`, `cache_write_tokens_1h_global_standard`, `input_tokens_batch`.
/// * Older listings: `InputTokenCount`, `OutputTokenCount_Global`, `CacheReadInputTokenCount`,
///   `CacheWrite1hInputTokenCount_Global`, `InputTokenCount_Batch`, `MillionBatchInputTokens`.
///
/// Provisioned/reserved throughput, model storage, customization, image, audio, video, and
/// latency-optimized dimensions are not per-token on-demand text prices and are skipped.
fn marketplace_pricing_rule(
    attributes: &ProductAttributes,
    price_micros: u64,
) -> Option<CatalogPricingRule> {
    let usage = attributes.usage_type.as_deref()?;
    let dimension = usage.rsplit_once(':').map_or(usage, |(_, rest)| rest);
    let dimension = dimension
        .split_once('_')
        .map_or(dimension, |(_, rest)| rest)
        .strip_suffix("-Units")
        .unwrap_or(dimension)
        .to_ascii_lowercase();
    if dimension.contains("provisioned")
        || dimension.contains("reserved")
        || dimension.contains("storage")
        || dimension.contains("customization")
        || dimension.contains("latencyoptimized")
        || dimension.contains("image")
        || dimension.contains("audio")
        || dimension.contains("video")
    {
        return None;
    }
    let is_token = dimension.contains("token");
    if !is_token {
        return None;
    }
    let bucket = if dimension.contains("cache") && dimension.contains("read") {
        CatalogPricingBucket::CacheReadInput
    } else if dimension.contains("cache") && dimension.contains("write") {
        CatalogPricingBucket::CacheWriteInput
    } else if dimension.contains("input") && !dimension.contains("output") {
        CatalogPricingBucket::Input
    } else if dimension.contains("output") {
        CatalogPricingBucket::Output
    } else {
        return None;
    };
    let invocation_class = if dimension.contains("batch") {
        CatalogInvocationClass::Batch
    } else {
        CatalogInvocationClass::OnDemand
    };
    let cache_ttl_seconds = (bucket == CatalogPricingBucket::CacheWriteInput).then(|| {
        if dimension.contains("1h") {
            ONE_HOUR_SECONDS
        } else if dimension.contains("30m") {
            THIRTY_MINUTES_SECONDS
        } else {
            FIVE_MINUTES_SECONDS
        }
    });
    let billing_scope = if dimension.contains("global") {
        "global"
    } else {
        "in_region"
    };
    Some(CatalogPricingRule {
        bucket,
        modality: Some(CatalogTokenModality::Text),
        service_tier: Some("standard".to_string()),
        invocation_class: Some(invocation_class),
        cache_ttl_seconds,
        min_request_input_tokens: None,
        max_request_input_tokens: None,
        billing_scope: Some(billing_scope.to_string()),
        price_micros,
    })
}

fn normalize_name(value: &str) -> String {
    value
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect()
}

fn price_per_million_micros(price: &str, unit: &str) -> Option<u64> {
    let tokens_per_unit = match unit.to_ascii_lowercase().as_str() {
        "1k tokens" => 1_000_u128,
        "1m tokens" => 1_000_000_u128,
        "token" | "tokens" => 1_u128,
        _ => return None,
    };
    let (whole, fractional) = price.split_once('.').unwrap_or((price, ""));
    let digits = fractional.len().min(12);
    let scale = 10_u128.checked_pow(u32::try_from(digits).ok()?)?;
    let numerator = whole
        .parse::<u128>()
        .ok()?
        .checked_mul(scale)?
        .checked_add(fractional.get(..digits)?.parse::<u128>().ok()?)?;
    let micros = numerator
        .checked_mul(1_000_000)?
        .checked_mul(1_000_000)?
        .checked_div(scale.checked_mul(tokens_per_unit)?)?;
    u64::try_from(micros).ok()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PriceListDocument {
    #[serde(default)]
    publication_date: Option<String>,
    #[serde(default)]
    products: BTreeMap<String, Product>,
    #[serde(default)]
    terms: Terms,
}
#[derive(Debug, Deserialize)]
struct Product {
    attributes: ProductAttributes,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProductAttributes {
    model: Option<String>,
    feature: Option<String>,
    inference_type: Option<String>,
    service_tier: Option<String>,
    token_type: Option<String>,
    /// AWS spells this attribute in lowercase rather than camelCase.
    #[serde(rename = "usagetype")]
    usage_type: Option<String>,
    /// Marketplace offer product name, e.g. `Claude Fable 5.1 (Amazon Bedrock Edition)`.
    #[serde(rename = "servicename")]
    service_name: Option<String>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Terms {
    on_demand: Option<BTreeMap<String, BTreeMap<String, PriceTerm>>>,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PriceTerm {
    #[serde(default)]
    price_dimensions: BTreeMap<String, PriceDimension>,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PriceDimension {
    unit: String,
    #[serde(default)]
    price_per_unit: BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_cache_and_long_context_rates() {
        let document: PriceListDocument = serde_json::from_value(serde_json::json!({
            "publicationDate": "2026-08-28T00:00:00Z",
            "products": {
                "input": {"attributes": {"model": "Claude Test", "feature": "On-demand Inference", "inferenceType": "Input tokens"}},
                "output": {"attributes": {"model": "Claude Test", "feature": "On-demand Inference", "inferenceType": "Output tokens"}},
                "cache": {"attributes": {"model": "Claude Test", "feature": "On-demand Inference", "tokenType": "Cache Read Input Tokens"}},
                "long": {"attributes": {"model": "Claude Test", "feature": "On-demand Inference", "tokenType": "Input tokens long context"}}
            },
            "terms": {"OnDemand": {
                "input": {"term": {"priceDimensions": {"dimension": {"unit": "1K tokens", "pricePerUnit": {"USD": "0.003"}}}}},
                "output": {"term": {"priceDimensions": {"dimension": {"unit": "1K tokens", "pricePerUnit": {"USD": "0.015"}}}}},
                "cache": {"term": {"priceDimensions": {"dimension": {"unit": "1M tokens", "pricePerUnit": {"USD": "0.30"}}}}},
                "long": {"term": {"priceDimensions": {"dimension": {"unit": "1K tokens", "pricePerUnit": {"USD": "0.006"}}}}}
            }}
        }))
        .expect("price-list fixture");

        let pricing = parse(document)
            .remove("claudetest")
            .expect("normalized model pricing");
        assert_eq!(pricing.revision.as_deref(), Some("2026-08-28T00:00:00Z"));
        assert_eq!(pricing.output_micros, Some(15_000_000));
        assert!(pricing.rules.iter().any(|rule| {
            rule.bucket == CatalogPricingBucket::CacheReadInput && rule.price_micros == 300_000
        }));
        assert_eq!(
            pricing.context_threshold_tokens,
            Some(PRICE_LIST_CONTEXT_THRESHOLD_TOKENS)
        );
        assert!(pricing.rules.iter().any(|rule| {
            rule.bucket == CatalogPricingBucket::Input
                && rule.min_request_input_tokens == Some(PRICE_LIST_CONTEXT_THRESHOLD_TOKENS + 1)
        }));
    }

    #[test]
    fn parses_marketplace_offer_usagetype_dimensions() {
        let product = |usagetype: &str| {
            serde_json::json!({"attributes": {
                "servicename": "Claude Fable 5.1 (Amazon Bedrock Edition)",
                "usagetype": format!("USE1-MP:USE1_{usagetype}-Units"),
                "regionCode": "us-east-1"
            }})
        };
        let term = |usd: &str| serde_json::json!({"term": {"priceDimensions": {"dimension": {"unit": "1M tokens", "pricePerUnit": {"USD": usd}}}}});
        let document: PriceListDocument = serde_json::from_value(serde_json::json!({
            "publicationDate": "2026-09-01T18:36:49Z",
            "products": {
                "in": product("input_tokens_standard"),
                "in_global": product("input_tokens_global_standard"),
                "out": product("output_tokens_standard"),
                "cache_read": product("cache_read_tokens_standard"),
                "cache_write": product("cache_write_tokens_standard"),
                "cache_write_1h_global": product("cache_write_tokens_1h_global_standard"),
                "batch": product("input_tokens_batch"),
                "legacy_in": product("InputTokenCount"),
                "legacy_cache_write_1h_global": product("CacheWrite1hInputTokenCount_Global"),
                "reserved": product("Reserved_1Month_InputTPM_Global"),
                "provisioned": product("ProvisionedThroughput_NoCommit_ModelUnits_Usage")
            },
            "terms": {"OnDemand": {
                "in": term("11.00"),
                "in_global": term("10.00"),
                "out": term("55.00"),
                "cache_read": term("0.275"),
                "cache_write": term("13.75"),
                "cache_write_1h_global": term("20.00"),
                "batch": term("5.50"),
                "legacy_in": term("11.00"),
                "legacy_cache_write_1h_global": term("20.00"),
                "reserved": term("0.06"),
                "provisioned": term("49.50")
            }}
        }))
        .expect("price-list fixture");

        let pricing = parse(document)
            .remove("claudefable51")
            .expect("marketplace model pricing keyed by servicename");
        assert_eq!(pricing.input_micros, Some(11_000_000));
        assert_eq!(pricing.output_micros, Some(55_000_000));
        assert_eq!(pricing.cached_input_micros, Some(275_000));
        // Default-TTL cache writes are flat; the 1h TTL stays rule-only.
        assert_eq!(pricing.cache_write_input_micros, None);
        let find = |bucket, scope: &str, ttl: Option<u64>, class| {
            pricing
                .rules
                .iter()
                .find(|rule| {
                    rule.bucket == bucket
                        && rule.billing_scope.as_deref() == Some(scope)
                        && rule.cache_ttl_seconds == ttl
                        && rule.invocation_class == Some(class)
                })
                .map(|rule| rule.price_micros)
        };
        assert_eq!(
            find(
                CatalogPricingBucket::Input,
                "global",
                None,
                CatalogInvocationClass::OnDemand
            ),
            Some(10_000_000)
        );
        assert_eq!(
            find(
                CatalogPricingBucket::CacheWriteInput,
                "in_region",
                Some(FIVE_MINUTES_SECONDS),
                CatalogInvocationClass::OnDemand
            ),
            Some(13_750_000)
        );
        assert_eq!(
            find(
                CatalogPricingBucket::CacheWriteInput,
                "global",
                Some(ONE_HOUR_SECONDS),
                CatalogInvocationClass::OnDemand
            ),
            Some(20_000_000)
        );
        assert_eq!(
            find(
                CatalogPricingBucket::Input,
                "in_region",
                None,
                CatalogInvocationClass::Batch
            ),
            Some(5_500_000)
        );
        // Throughput/reserved dimensions are not per-token prices.
        assert!(
            pricing
                .rules
                .iter()
                .all(|rule| rule.price_micros != 60_000 && rule.price_micros != 49_500_000)
        );
    }

    /// Exercise the parser against real downloaded price lists.
    ///
    /// Set `BCODE_BEDROCK_PRICE_LIST_FIXTURES` to a comma-separated list of local
    /// `index.json` paths (one per offer) to print match statistics. Optionally set
    /// `BCODE_BEDROCK_LIVE_FIXTURE` to a live snapshot JSON to report which discovered text
    /// models would remain unpriced.
    #[test]
    #[ignore = "requires downloaded AWS price-list fixtures"]
    fn parses_downloaded_price_lists() {
        let Ok(paths) = std::env::var("BCODE_BEDROCK_PRICE_LIST_FIXTURES") else {
            return;
        };
        let mut merged = BTreeMap::new();
        for path in paths.split(',') {
            let document: PriceListDocument =
                serde_json::from_slice(&std::fs::read(path.trim()).expect("fixture read"))
                    .expect("fixture parse");
            for (model, pricing) in parse(document) {
                merged.entry(model).or_insert(pricing);
            }
        }
        let flat = merged
            .values()
            .filter(|pricing| pricing.input_micros.is_some() && pricing.output_micros.is_some())
            .count();
        eprintln!(
            "priced models: {} (flat input+output: {flat})",
            merged.len()
        );
        for (model, pricing) in &merged {
            eprintln!(
                "  {model:40} in={:?} out={:?} cache_read={:?} rules={}",
                pricing.input_micros,
                pricing.output_micros,
                pricing.cached_input_micros,
                pricing.rules.len()
            );
        }
        assert!(flat > 0);
        if let Ok(live) = std::env::var("BCODE_BEDROCK_LIVE_FIXTURE") {
            let snapshot: bcode_model_catalog_models::LiveCatalogSnapshot =
                serde_json::from_slice(&std::fs::read(live).expect("live read"))
                    .expect("live parse");
            let mut priced = 0;
            let mut unpriced = Vec::new();
            for model in snapshot.models.values() {
                if !model.capabilities.text_output {
                    continue;
                }
                let name = model.display_name.as_deref().unwrap_or(&model.model_id);
                if crate::bedrock::pricing_for_model(name, &model.model_id, &merged).is_some() {
                    priced += 1;
                } else {
                    unpriced.push(format!("{} ({name})", model.model_id));
                }
            }
            eprintln!(
                "live text models priced: {priced}, unpriced: {}",
                unpriced.len()
            );
            for model in unpriced {
                eprintln!("  UNPRICED {model}");
            }
            if let Ok(output) = std::env::var("BCODE_BEDROCK_PRICED_SNAPSHOT_OUTPUT") {
                let mut priced_snapshot = snapshot;
                for model in priced_snapshot.models.values_mut() {
                    let name = model
                        .display_name
                        .clone()
                        .unwrap_or_else(|| model.model_id.clone());
                    model.pricing =
                        crate::bedrock::pricing_for_model(&name, &model.model_id, &merged);
                }
                crate::write_snapshot(std::path::Path::new(&output), &priced_snapshot)
                    .expect("priced snapshot write");
            }
        }
    }

    #[test]
    fn normalizes_model_names_and_token_units() {
        assert_eq!(normalize_name("Claude 3.5-Sonnet"), "claude35sonnet");
        assert_eq!(
            price_per_million_micros("0.003", "1K tokens"),
            Some(3_000_000)
        );
        assert_eq!(
            price_per_million_micros("3.00", "1M tokens"),
            Some(3_000_000)
        );
        assert_eq!(price_per_million_micros("1", "image"), None);
    }
}
