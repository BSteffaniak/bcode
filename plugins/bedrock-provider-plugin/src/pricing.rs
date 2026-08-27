//! Amazon Bedrock pricing resolution from AWS-published pricing data.

use bcode_model::{ModelPricingInfo, ModelPricingSource, ModelPricingUnit, ModelTokenPrice};
use serde::Deserialize;
use std::collections::BTreeMap;

const PRICE_LIST_BASE_URL: &str =
    "https://pricing.us-east-1.amazonaws.com/offers/v1.0/aws/AmazonBedrock/current";

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

/// Region-specific, on-demand token prices indexed by AWS's public model display name.
#[derive(Debug, Clone, Default)]
pub struct BedrockPricingCatalog {
    by_model_name: BTreeMap<String, ModelPricingInfo>,
}

impl BedrockPricingCatalog {
    fn from_document(document: PriceListDocument) -> Self {
        let mut buckets = BTreeMap::<String, PriceBuckets>::new();
        let Some(terms) = document.terms.on_demand else {
            return Self::default();
        };
        for (sku, product) in document.products {
            let attributes = product.attributes;
            if attributes.feature.as_deref() != Some("On-demand Inference")
                || attributes
                    .service_tier
                    .as_deref()
                    .is_some_and(|tier| !tier.is_empty())
            {
                continue;
            }
            let Some(model) = attributes.model.filter(|model| !model.is_empty()) else {
                continue;
            };
            let Some(inference_type) = attributes.inference_type else {
                continue;
            };
            let Some(product_terms) = terms.get(&sku) else {
                continue;
            };
            for term in product_terms.values() {
                for dimension in term.price_dimensions.values() {
                    let Some(usd) = dimension.price_per_unit.get("USD") else {
                        continue;
                    };
                    let Some(micros) = price_per_million_micros(usd, &dimension.unit) else {
                        continue;
                    };
                    buckets
                        .entry(normalize_name(&model))
                        .or_default()
                        .set(&inference_type, micros);
                }
            }
        }
        Self {
            by_model_name: buckets
                .into_iter()
                .filter_map(|(name, prices)| prices.complete().map(|pricing| (name, pricing)))
                .collect(),
        }
    }

    /// Resolve complete standard on-demand token pricing by AWS model display name.
    #[must_use]
    pub fn pricing_for_model_name(&self, model_name: &str) -> Option<ModelPricingInfo> {
        self.by_model_name.get(&normalize_name(model_name)).cloned()
    }
}

#[derive(Debug, Clone, Default)]
struct PriceBuckets {
    input: std::collections::BTreeSet<u64>,
    cached_input: std::collections::BTreeSet<u64>,
    cache_write_input: std::collections::BTreeSet<u64>,
    output: std::collections::BTreeSet<u64>,
}

impl PriceBuckets {
    fn set(&mut self, inference_type: &str, micros: u64) {
        let dimension = inference_type.to_ascii_lowercase();
        let bucket = match dimension.as_str() {
            "input tokens" | "text input tokens" => &mut self.input,
            "output tokens" | "text output tokens" => &mut self.output,
            "prompt cache read input tokens" => &mut self.cached_input,
            "prompt cache write input tokens" => &mut self.cache_write_input,
            _ => return,
        };
        bucket.insert(micros);
    }

    fn complete(self) -> Option<ModelPricingInfo> {
        Some(ModelPricingInfo {
            currency: "USD".to_string(),
            unit: ModelPricingUnit::PerMillionTokens,
            input: Some(ModelTokenPrice::from_micros(single_price(&self.input)?)),
            cached_input: optional_single_price(&self.cached_input)
                .map(ModelTokenPrice::from_micros),
            cache_write_input: optional_single_price(&self.cache_write_input)
                .map(ModelTokenPrice::from_micros),
            output: Some(ModelTokenPrice::from_micros(single_price(&self.output)?)),
            source: ModelPricingSource::ProviderApi,
        })
    }
}

fn single_price(prices: &std::collections::BTreeSet<u64>) -> Option<u64> {
    (prices.len() == 1)
        .then(|| prices.iter().next().copied())
        .flatten()
}

fn optional_single_price(prices: &std::collections::BTreeSet<u64>) -> Option<u64> {
    single_price(prices)
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
    fn converts_public_price_units_to_micros_per_million_tokens() {
        assert_eq!(
            price_per_million_micros("0.0008000000", "1K tokens"),
            Some(800_000)
        );
        assert_eq!(
            price_per_million_micros("3.25", "1M tokens"),
            Some(3_250_000)
        );
    }

    #[test]
    fn parses_complete_standard_on_demand_pricing() {
        let document = serde_json::from_value::<PriceListDocument>(serde_json::json!({
            "products": {
                "input": {"attributes": {"model": "Nova Pro", "feature": "On-demand Inference", "inferenceType": "Input tokens"}},
                "output": {"attributes": {"model": "Nova Pro", "feature": "On-demand Inference", "inferenceType": "Output tokens"}},
                "priority": {"attributes": {"model": "Nova Pro", "feature": "On-demand Inference", "inferenceType": "Input tokens priority", "serviceTier": "priority"}}
            },
            "terms": {"OnDemand": {
                "input": {"term": {"priceDimensions": {"rate": {"unit": "1K tokens", "pricePerUnit": {"USD": "0.0008"}}}}},
                "output": {"term": {"priceDimensions": {"rate": {"unit": "1K tokens", "pricePerUnit": {"USD": "0.0032"}}}}},
                "priority": {"term": {"priceDimensions": {"rate": {"unit": "1K tokens", "pricePerUnit": {"USD": "9"}}}}}
            }}
        })).expect("price list");
        let pricing = BedrockPricingCatalog::from_document(document)
            .pricing_for_model_name("nova-pro")
            .expect("complete pricing");
        assert_eq!(pricing.input, Some(ModelTokenPrice::from_micros(800_000)));
        assert_eq!(
            pricing.output,
            Some(ModelTokenPrice::from_micros(3_200_000))
        );
        assert_eq!(pricing.source, ModelPricingSource::ProviderApi);
    }
}
