//! Amazon Bedrock pricing resolution from AWS-published pricing data.

use bcode_model::{
    ModelInvocationClass, ModelPricingBucket, ModelPricingInfo, ModelPricingRule,
    ModelPricingSource, ModelPricingUnit, ModelTokenModality, ModelTokenPrice,
};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

const PRICE_LIST_BASE_URL: &str =
    "https://pricing.us-east-1.amazonaws.com/offers/v1.0/aws/AmazonBedrock/current";
const SHORT_CONTEXT_MAX_TOKENS: u64 = 272_000;
const LONG_CONTEXT_MIN_TOKENS: u64 = SHORT_CONTEXT_MAX_TOKENS + 1;
const THIRTY_MINUTES_SECONDS: u64 = 30 * 60;

/// Fetch the current public Bedrock price list for one AWS region.
pub async fn fetch_region(
    region: &str,
) -> Result<BedrockPricingCatalog, Box<dyn std::error::Error + Send + Sync>> {
    let url = format!("{PRICE_LIST_BASE_URL}/{region}/index.json");
    let response = reqwest::Client::new()
        .get(url)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await?
        .error_for_status()?;
    let document = response.json::<PriceListDocument>().await?;
    Ok(BedrockPricingCatalog::from_document(document))
}

/// Region-specific token prices indexed by AWS's public model display name.
#[derive(Debug, Clone, Default)]
pub struct BedrockPricingCatalog {
    by_model_name: BTreeMap<String, ModelPricingInfo>,
}

impl BedrockPricingCatalog {
    fn from_document(document: PriceListDocument) -> Self {
        let revision = document.version.clone();
        let Some(terms) = document.terms.on_demand else {
            return Self::default();
        };
        let mut model_names = BTreeMap::<String, BTreeSet<String>>::new();
        for product in document.products.values() {
            if let Some(model) = product.attributes.model.as_deref() {
                model_names
                    .entry(normalize_name(model))
                    .or_default()
                    .insert(model.to_string());
            }
        }
        let ambiguous_names = model_names
            .into_iter()
            .filter_map(|(name, originals)| (originals.len() > 1).then_some(name))
            .collect::<BTreeSet<_>>();
        let mut rules = BTreeMap::<String, BTreeSet<ModelPricingRule>>::new();
        for (sku, product) in document.products {
            let attributes = product.attributes;
            let Some(model) = attributes
                .model
                .as_deref()
                .filter(|model| !model.is_empty())
            else {
                continue;
            };
            let normalized_model = normalize_name(model);
            if ambiguous_names.contains(&normalized_model) {
                continue;
            }
            let Some(product_terms) = terms.get(&sku) else {
                continue;
            };
            for term in product_terms.values() {
                for dimension in term.price_dimensions.values() {
                    let Some(usd) = dimension.price_per_unit.get("USD") else {
                        continue;
                    };
                    let Some(price) = price_per_million_micros(usd, &dimension.unit)
                        .map(ModelTokenPrice::from_micros)
                    else {
                        continue;
                    };
                    if let Some(rule) = pricing_rule(&attributes, price) {
                        rules
                            .entry(normalized_model.clone())
                            .or_default()
                            .insert(rule);
                    }
                }
            }
        }
        Self {
            by_model_name: rules
                .into_iter()
                .map(|(model, rules)| {
                    let rules = rules.into_iter().collect::<Vec<_>>();
                    let (input, cached_input, cache_write_input, output) = flat_prices(&rules);
                    (
                        model,
                        ModelPricingInfo {
                            currency: "USD".to_string(),
                            unit: ModelPricingUnit::PerMillionTokens,
                            input,
                            cached_input,
                            cache_write_input,
                            output,
                            rules,
                            revision: revision.clone(),
                            source: ModelPricingSource::ProviderApi,
                        },
                    )
                })
                .collect(),
        }
    }

    /// Resolve all known token-pricing rules by AWS model display name.
    #[must_use]
    pub fn pricing_for_model_name(&self, model_name: &str) -> Option<ModelPricingInfo> {
        self.by_model_name.get(&normalize_name(model_name)).cloned()
    }
}

fn pricing_rule(
    attributes: &ProductAttributes,
    price: ModelTokenPrice,
) -> Option<ModelPricingRule> {
    let dimension = attributes
        .token_type
        .as_deref()
        .filter(|value| !value.is_empty())
        .or(attributes.inference_type.as_deref())?
        .to_ascii_lowercase();
    let bucket = if dimension.contains("cache") && dimension.contains("read") {
        ModelPricingBucket::CacheReadInput
    } else if dimension.contains("cache") && dimension.contains("write") {
        ModelPricingBucket::CacheWriteInput
    } else if dimension.contains("input") && !dimension.contains("output") {
        ModelPricingBucket::Input
    } else if dimension.contains("output") {
        ModelPricingBucket::Output
    } else {
        return None;
    };
    let modality = if dimension.contains("image") {
        Some(ModelTokenModality::Image)
    } else if dimension.contains("audio") || dimension.contains("speech") {
        Some(ModelTokenModality::Audio)
    } else if dimension.contains("video") {
        Some(ModelTokenModality::Video)
    } else {
        Some(ModelTokenModality::Text)
    };
    let long_context = dimension.contains("long-ctx") || dimension.contains("long context");
    let feature = attributes.feature.as_deref().unwrap_or_default();
    let invocation_class = if feature == "Batch Inference"
        || attributes.service_tier.as_deref() == Some("batch")
        || dimension.contains("batch")
    {
        ModelInvocationClass::Batch
    } else if feature.is_empty()
        || feature == "On-demand Inference"
        || attributes.token_type.is_some()
    {
        ModelInvocationClass::OnDemand
    } else {
        return None;
    };
    Some(ModelPricingRule {
        bucket,
        modality,
        service_tier: attributes.service_tier.clone().or_else(|| {
            (attributes.token_type.is_some() || feature == "On-demand Inference")
                .then_some("standard".to_string())
        }),
        invocation_class: Some(invocation_class),
        cache_ttl_seconds: (bucket == ModelPricingBucket::CacheWriteInput
            && dimension.contains("30m"))
        .then_some(THIRTY_MINUTES_SECONDS),
        min_request_input_tokens: long_context.then_some(LONG_CONTEXT_MIN_TOKENS),
        max_request_input_tokens: (!long_context && attributes.token_type.is_some())
            .then_some(SHORT_CONTEXT_MAX_TOKENS),
        billing_scope: Some(billing_scope(attributes)),
        price,
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

fn flat_prices(
    rules: &[ModelPricingRule],
) -> (
    Option<ModelTokenPrice>,
    Option<ModelTokenPrice>,
    Option<ModelTokenPrice>,
    Option<ModelTokenPrice>,
) {
    let price = |bucket| {
        let prices = rules
            .iter()
            .filter(|rule| {
                rule.bucket == bucket
                    && rule.modality == Some(ModelTokenModality::Text)
                    && rule.service_tier.is_none()
                    && rule.invocation_class == Some(ModelInvocationClass::OnDemand)
                    && rule.cache_ttl_seconds.is_none()
                    && rule.min_request_input_tokens.is_none()
                    && rule.max_request_input_tokens.is_none()
                    && rule.billing_scope.is_none()
            })
            .map(|rule| rule.price)
            .collect::<BTreeSet<_>>();
        (prices.len() == 1)
            .then(|| prices.first().copied())
            .flatten()
    };
    (
        price(ModelPricingBucket::Input),
        price(ModelPricingBucket::CacheReadInput),
        price(ModelPricingBucket::CacheWriteInput),
        price(ModelPricingBucket::Output),
    )
}

fn normalize_name(value: &str) -> String {
    value
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect()
}

pub fn price_per_million_micros(price: &str, unit: &str) -> Option<u64> {
    let tokens_per_unit = match unit.to_ascii_lowercase().as_str() {
        "1k tokens" => 1_000_u128,
        "1m tokens" => 1_000_000_u128,
        "token" | "tokens" => 1_u128,
        _ => return None,
    };
    let (whole, fractional) = price.split_once('.').unwrap_or((price, ""));
    let whole = whole.parse::<u128>().ok()?;
    let fractional_digits = fractional.len().min(12);
    let fractional = fractional.get(..fractional_digits)?.parse::<u128>().ok()?;
    let scale = 10_u128.checked_pow(u32::try_from(fractional_digits).ok()?)?;
    let numerator = whole.checked_mul(scale)?.checked_add(fractional)?;
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
    version: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_model::{ModelPricingContext, TokenUsage};

    #[test]
    fn converts_public_price_units_to_micros_per_million_tokens() {
        assert_eq!(
            price_per_million_micros("0.0008", "1K tokens"),
            Some(800_000)
        );
        assert_eq!(
            price_per_million_micros("3.25", "1M tokens"),
            Some(3_250_000)
        );
    }

    #[test]
    fn selects_long_context_rates_for_the_entire_request() {
        let document = serde_json::from_value::<PriceListDocument>(serde_json::json!({
            "version": "fixture-v1",
            "products": {
                "short-in": {"attributes": {"model": "openai.gpt-5.6-terra", "tokenType": "input_tokens_mantle", "serviceTier": "standard"}},
                "long-in": {"attributes": {"model": "openai.gpt-5.6-terra", "tokenType": "input-tokens-long-ctx", "serviceTier": "standard"}},
                "short-out": {"attributes": {"model": "openai.gpt-5.6-terra", "tokenType": "output_tokens_mantle", "serviceTier": "standard"}},
                "long-out": {"attributes": {"model": "openai.gpt-5.6-terra", "tokenType": "output-tokens-long-ctx", "serviceTier": "standard"}}
            },
            "terms": {"OnDemand": {
                "short-in": {"term": {"priceDimensions": {"rate": {"unit": "1M tokens", "pricePerUnit": {"USD": "2.20"}}}}},
                "long-in": {"term": {"priceDimensions": {"rate": {"unit": "1M tokens", "pricePerUnit": {"USD": "4.40"}}}}},
                "short-out": {"term": {"priceDimensions": {"rate": {"unit": "1M tokens", "pricePerUnit": {"USD": "13.20"}}}}},
                "long-out": {"term": {"priceDimensions": {"rate": {"unit": "1M tokens", "pricePerUnit": {"USD": "19.80"}}}}}
            }}
        })).expect("price list");
        let pricing = BedrockPricingCatalog::from_document(document)
            .pricing_for_model_name("openai.gpt-5.6-terra")
            .expect("pricing");
        let usage = TokenUsage {
            input_tokens: Some(300_000),
            output_tokens: Some(10_000),
            pricing_context: Box::new(ModelPricingContext {
                service_tier: Some("standard".to_string()),
                invocation_class: Some(ModelInvocationClass::OnDemand),
                billing_scope: Some("in_region".to_string()),
                request_input_tokens: Some(300_000),
                ..ModelPricingContext::default()
            }),
            ..TokenUsage::default()
        };
        let estimate = pricing
            .estimate_cost_with_context(&usage, &usage.pricing_context)
            .expect("long-context estimate");
        assert_eq!(estimate.total_micros, 1_518_000);
        assert_eq!(estimate.revision.as_deref(), Some("fixture-v1"));
    }
}
