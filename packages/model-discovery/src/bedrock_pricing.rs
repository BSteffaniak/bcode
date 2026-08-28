//! Amazon Bedrock public price-list normalization for model catalog discovery.

use bcode_model_catalog_models::{
    CatalogInvocationClass, CatalogPricing, CatalogPricingBucket, CatalogPricingRule,
    CatalogPricingUnit, CatalogTokenModality,
};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

const PRICE_LIST_BASE_URL: &str =
    "https://pricing.us-east-1.amazonaws.com/offers/v1.0/aws/AmazonBedrock/current";
const PRICE_LIST_CONTEXT_THRESHOLD_TOKENS: u64 = 272_000;
const THIRTY_MINUTES_SECONDS: u64 = 30 * 60;

pub async fn fetch_region(
    region: &str,
) -> Result<BTreeMap<String, CatalogPricing>, Box<dyn std::error::Error + Send + Sync>> {
    let url = format!("{PRICE_LIST_BASE_URL}/{region}/index.json");
    let document = reqwest::Client::new()
        .get(url)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await?
        .error_for_status()?
        .json::<PriceListDocument>()
        .await?;
    Ok(parse(document))
}

fn parse(document: PriceListDocument) -> BTreeMap<String, CatalogPricing> {
    let Some(terms) = document.terms.on_demand else {
        return BTreeMap::new();
    };
    let mut rules = BTreeMap::<String, BTreeSet<CatalogPricingRule>>::new();
    for (sku, product) in document.products {
        let attributes = product.attributes;
        let Some(model) = attributes
            .model
            .as_deref()
            .filter(|model| !model.is_empty())
        else {
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
                    rules.entry(normalize_name(model)).or_default().insert(rule);
                }
            }
        }
    }
    rules
        .into_iter()
        .map(|(model, rules)| {
            let rules = rules.into_iter().collect::<Vec<_>>();
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
                    rules,
                },
            )
        })
        .collect()
}

fn pricing_rule(attributes: &ProductAttributes, price_micros: u64) -> Option<CatalogPricingRule> {
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
    Some(CatalogPricingRule {
        bucket,
        modality: Some(modality),
        service_tier: attributes.service_tier.clone().or_else(|| {
            (attributes.token_type.is_some() || feature == "On-demand Inference")
                .then_some("standard".to_string())
        }),
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
    usage_type: Option<String>,
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
