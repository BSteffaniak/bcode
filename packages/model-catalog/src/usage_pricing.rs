//! Domain-owned pricing of immutable session usage using an explicit catalog snapshot.

use crate::{
    ModelCatalog, model_info_from_catalog_entry, model_info_from_catalog_entry_for_target,
};
use bcode_session_models::{SessionCostEstimate, SessionCostUnavailableReason, SessionTokenUsage};

/// Price one request's normalized usage facts using only the supplied catalog.
/// Historical embedded costs are never a fallback and provider discovery is never invoked.
#[must_use]
pub fn price_session_usage(
    catalog: &ModelCatalog,
    usage: &SessionTokenUsage,
) -> SessionCostEstimate {
    if catalog.document().schema_version != bcode_model_catalog_models::SCHEMA_VERSION {
        return SessionCostEstimate::Unavailable {
            reason: SessionCostUnavailableReason::RequestPricingUnavailable,
        };
    }
    let unavailable = |reason| SessionCostEstimate::Unavailable { reason };
    let (Some(provider), Some(model)) = (
        usage.catalog_provider_id.as_deref(),
        usage.catalog_entry_id.as_deref(),
    ) else {
        return unavailable(SessionCostUnavailableReason::RequestIdentityUnavailable);
    };
    // Captured exact entry identity is authoritative; do not resolve a historical alias anew.
    let Some(entry) = catalog
        .document()
        .providers
        .get(provider)
        .and_then(|provider| provider.models.get(model))
    else {
        return unavailable(SessionCostUnavailableReason::RequestPricingUnavailable);
    };
    let pricing = if let Some(target) = &usage.pricing_target {
        if target.provider != provider {
            return unavailable(SessionCostUnavailableReason::ConflictingUsage);
        }
        let target = bcode_model_catalog_models::ModelSupportTarget {
            provider: target.provider.clone(),
            auth_mode: target.auth_mode.clone(),
            api_surface: target.api_surface.clone(),
            integration: target.integration.clone(),
        };
        if entry
            .deployments
            .iter()
            .filter(|deployment| deployment.target.matches(&target))
            .count()
            > 1
        {
            return unavailable(SessionCostUnavailableReason::PricingRuleUnavailableOrAmbiguous);
        }
        model_info_from_catalog_entry_for_target(entry, &target).pricing
    } else {
        // Missing deployment attribution must not choose an arbitrary auth/API tariff.
        if entry.deployments.iter().any(|deployment| {
            deployment.pricing.as_ref().or(entry.pricing.as_ref()) != entry.pricing.as_ref()
        }) {
            return unavailable(SessionCostUnavailableReason::RequiredPricingContextUnavailable);
        }
        model_info_from_catalog_entry(entry).pricing
    };
    let Some(pricing) = pricing else {
        return unavailable(SessionCostUnavailableReason::RequestPricingUnavailable);
    };
    price_usage_with_tariff(usage, &pricing)
}

fn label(value: impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Price normalized session facts with an already resolved tariff (including request overrides).
#[must_use]
pub fn price_usage_with_tariff(
    usage: &SessionTokenUsage,
    pricing: &bcode_model::ModelPricingInfo,
) -> SessionCostEstimate {
    let normalized = match normalized_usage(usage) {
        Ok(usage) => usage,
        Err(reason) => return SessionCostEstimate::Unavailable { reason },
    };
    pricing.estimate_cost(&normalized).map_or_else(
        || SessionCostEstimate::Unavailable {
            reason: pricing.cost_unavailable_reason(&normalized),
        },
        |estimate| SessionCostEstimate::Estimated {
            currency: estimate.currency,
            total_micros: estimate.total_micros,
            components: estimate
                .components
                .into_iter()
                .map(|component| bcode_session_models::SessionCostComponent {
                    bucket: label(component.bucket),
                    modality: component.modality.map(label),
                    tokens: component.tokens,
                    price_micros: component.price.micros,
                    cost_micros: component.cost_micros,
                })
                .collect(),
            source: label(estimate.source),
            revision: estimate.revision,
        },
    )
}

fn normalized_usage(
    usage: &SessionTokenUsage,
) -> Result<bcode_model::TokenUsage, SessionCostUnavailableReason> {
    let parse = |value: &str| serde_json::Value::String(value.to_owned());
    let details = usage
        .pricing_usage_details
        .iter()
        .map(|detail| {
            // Earlier current-format writers used Debug lowercasing rather than serde labels.
            let bucket = match detail.bucket.as_str() {
                "input" => bcode_model::ModelPricingBucket::Input,
                "cache_read_input" | "cachereadinput" => {
                    bcode_model::ModelPricingBucket::CacheReadInput
                }
                "cache_write_input" | "cachewriteinput" => {
                    bcode_model::ModelPricingBucket::CacheWriteInput
                }
                "output" => bcode_model::ModelPricingBucket::Output,
                _ => return Err(SessionCostUnavailableReason::DetailedUsageUnavailable),
            };
            Ok(bcode_model::ModelTokenUsageDetail {
                bucket,
                modality: serde_json::from_value(parse(&detail.modality))
                    .map_err(|_| SessionCostUnavailableReason::DetailedUsageUnavailable)?,
                tokens: detail.tokens,
                cache_ttl_seconds: detail.cache_ttl_seconds,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let invocation_class = match usage.pricing_context.invocation_class.as_deref() {
        Some("ondemand" | "on_demand") => Some(bcode_model::ModelInvocationClass::OnDemand),
        Some("batch") => Some(bcode_model::ModelInvocationClass::Batch),
        None => None,
        Some(_) => return Err(SessionCostUnavailableReason::RequiredPricingContextUnavailable),
    };
    Ok(bcode_model::TokenUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        total_tokens: usage.total_tokens,
        cached_input_tokens: usage.cached_input_tokens,
        cache_write_input_tokens: usage.cache_write_input_tokens,
        reasoning_tokens: usage.reasoning_tokens,
        details: details.into_boxed_slice(),
        pricing_context: Box::new(bcode_model::ModelPricingContext {
            service_tier: usage.pricing_context.service_tier.clone(),
            invocation_class,
            billing_scope: usage.pricing_context.billing_scope.clone(),
            request_input_tokens: usage.pricing_context.request_input_tokens,
            cache_ttl_seconds: usage.pricing_context.cache_ttl_seconds,
        }),
    })
}
