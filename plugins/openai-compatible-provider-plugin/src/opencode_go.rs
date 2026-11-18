//! `OpenCode` Go routing, conversation identification, and public model-listing parsing.

use std::collections::{BTreeMap, BTreeSet};

use bcode_model::{
    ModelCacheCapability, ModelCacheInfo, ModelCapability, ModelFeatureSupport, ModelInfo,
    ModelMetadataSource, ModelPricingInfo, ModelPricingSource, ModelPricingUnit,
    ModelReasoningCapabilitySource, ModelReasoningInfo, ModelTokenPrice, ModelVisibility,
    ReasoningControl,
};

use super::{ModelResponseItem, ModelTurnRequest, Settings};

pub const BASE_URL: &str = "https://opencode.ai/inference/go/openai/v1";
const MODELS_URL: &str = "https://opencode.ai/inference/go/v1/models";

pub fn is_go_endpoint(base_url: &str) -> bool {
    base_url.trim_end_matches('/') == BASE_URL
}

pub fn models_url(settings: &Settings) -> String {
    if is_go_endpoint(&settings.base_url) {
        MODELS_URL.to_owned()
    } else {
        format!("{}/models", settings.base_url.trim_end_matches('/'))
    }
}

pub fn identify_request(
    builder: reqwest::RequestBuilder,
    settings: &Settings,
    request: &ModelTurnRequest,
) -> reqwest::RequestBuilder {
    if is_go_endpoint(&settings.base_url) {
        builder
            .header(
                "x-opencode-session",
                format!("ses_bcode_{}", request.session_id),
            )
            .header("User-Agent", concat!("bcode/", env!("CARGO_PKG_VERSION")))
    } else {
        builder
    }
}

/// Whether the provider-owned Go models listing may be fetched without credentials.
///
/// Only the dedicated Go HTTPS endpoint publishes its listing publicly. Any other base URL,
/// including custom proxies and the default `OpenAI` endpoint, keeps requiring a resolved key.
#[must_use]
pub fn permits_keyless_listing(settings: &Settings) -> bool {
    is_go_endpoint(&settings.base_url)
}

/// Convert Go listing entries into normalized model metadata.
///
/// The Go models endpoint documents limits, pricing, and capabilities per entry. Presence and
/// these declared facts are authoritative for the listing; the shared catalog merge still
/// overlays verified serving quirks. Fields the listing omits stay unknown.
#[must_use]
pub fn model_infos_from_items(items: Vec<ModelResponseItem>) -> Vec<ModelInfo> {
    let mut seen = BTreeSet::new();
    let mut models = Vec::new();
    for item in items {
        if item.id.trim().is_empty() || !seen.insert(item.id.clone()) {
            continue;
        }
        models.push(model_info_from_item(&item));
    }
    models
}

fn model_info_from_item(item: &ModelResponseItem) -> ModelInfo {
    let id = item.id.clone();
    ModelInfo {
        is_default: false,
        model_id: id.clone(),
        display_name: string_at(&item.metadata, &["name"]).unwrap_or(id),
        context_window: u32_at(&item.metadata, &["limit", "context"]),
        max_output_tokens: u32_at(&item.metadata, &["limit", "output"]),
        max_image_input_base64_bytes: None,
        capabilities: go_capabilities(&item.metadata),
        feature_support: ModelFeatureSupport::default(),
        reasoning: go_reasoning(&item.metadata),
        cache: go_cache_info(&item.metadata),
        metadata_source: Some(ModelMetadataSource::ProviderLive),
        pricing: go_pricing(&item.metadata),
        api_surface: None,
        visibility: ModelVisibility::Visible,
    }
}

fn value_at<'a>(
    metadata: &'a BTreeMap<String, serde_json::Value>,
    path: &[&str],
) -> Option<&'a serde_json::Value> {
    let mut value = metadata.get(*path.first()?)?;
    for key in path.iter().skip(1) {
        value = value.get(key)?;
    }
    Some(value)
}

fn string_at(metadata: &BTreeMap<String, serde_json::Value>, path: &[&str]) -> Option<String> {
    value_at(metadata, path)?.as_str().map(str::to_string)
}

fn u32_at(metadata: &BTreeMap<String, serde_json::Value>, path: &[&str]) -> Option<u32> {
    u32::try_from(value_at(metadata, path)?.as_u64()?)
        .ok()
        .filter(|tokens| *tokens > 0)
}

fn bool_at(metadata: &BTreeMap<String, serde_json::Value>, path: &[&str]) -> bool {
    value_at(metadata, path)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or_default()
}

fn go_capabilities(metadata: &BTreeMap<String, serde_json::Value>) -> BTreeSet<ModelCapability> {
    let mut capabilities = BTreeSet::new();
    if bool_at(metadata, &["capabilities", "tools", "supported"]) {
        capabilities.insert(ModelCapability::ToolCalls);
    }
    if bool_at(metadata, &["capabilities", "reasoning", "supported"]) {
        capabilities.insert(ModelCapability::Reasoning);
    }
    if bool_at(metadata, &["capabilities", "structured_output"]) {
        capabilities.insert(ModelCapability::JsonMode);
    }
    if go_declares_cache(metadata) {
        capabilities.insert(ModelCapability::PromptCaching);
    }
    if value_at(metadata, &["modalities", "input"])
        .and_then(serde_json::Value::as_array)
        .is_some_and(|inputs| {
            inputs.iter().any(|input| {
                input
                    .as_str()
                    .is_some_and(|kind| kind.starts_with("image/"))
            })
        })
    {
        capabilities.insert(ModelCapability::ImageInput);
    }
    if value_at(metadata, &["modalities", "output"])
        .and_then(serde_json::Value::as_array)
        .is_some_and(|outputs| outputs.iter().any(|output| output == "text"))
    {
        capabilities.insert(ModelCapability::StreamingText);
    }
    capabilities
}

fn go_declares_cache(metadata: &BTreeMap<String, serde_json::Value>) -> bool {
    bool_at(metadata, &["capabilities", "cache", "implicit"])
        || value_at(metadata, &["capabilities", "cache", "explicit"]).is_some()
}

fn go_cache_info(metadata: &BTreeMap<String, serde_json::Value>) -> ModelCacheInfo {
    let mut cache = ModelCacheInfo::default();
    if bool_at(metadata, &["capabilities", "cache", "implicit"]) {
        cache
            .capabilities
            .insert(ModelCacheCapability::AutomaticPrefixCache);
    }
    if value_at(metadata, &["capabilities", "cache", "explicit"]).is_some() {
        cache
            .capabilities
            .insert(ModelCacheCapability::ExplicitCachePoints);
    }
    // Per-retention explicit write prices declare the cache windows the provider supports.
    if let Some(writes) = value_at(metadata, &["cost", "text", "input", "cache", "write"])
        .and_then(serde_json::Value::as_object)
    {
        for retention in writes.keys() {
            if let Some(ttl) = retention_seconds(retention) {
                cache.ttl_seconds.insert(ttl);
            }
        }
    }
    cache
}

/// Parse Go retention labels (`5m`, `1h`, `12h`) into seconds.
fn retention_seconds(label: &str) -> Option<u64> {
    let unit = *label.as_bytes().last()?;
    let value: u64 = label.strip_suffix(char::from(unit))?.parse().ok()?;
    match unit {
        b'm' => Some(value.saturating_mul(60)),
        b'h' => Some(value.saturating_mul(3600)),
        _ => None,
    }
}

fn go_pricing(metadata: &BTreeMap<String, serde_json::Value>) -> Option<ModelPricingInfo> {
    let input = price_micros_at(metadata, &["cost", "text", "input", "price"]);
    let output = price_micros_at(metadata, &["cost", "text", "output", "price"]);
    if input.is_none() && output.is_none() {
        return None;
    }
    Some(ModelPricingInfo {
        currency: "USD".to_string(),
        unit: ModelPricingUnit::PerMillionTokens,
        input,
        cached_input: price_micros_at(metadata, &["cost", "text", "input", "cache", "read"]),
        // Go publishes cache writes only as per-retention prices; the flat bucket stays
        // unknown rather than guessing one retention as the default.
        cache_write_input: None,
        output,
        rules: Vec::new(),
        revision: None,
        context_threshold_tokens: None,
        source: ModelPricingSource::ProviderApi,
    })
}

/// Convert a per-million-token USD price into currency micros.
///
/// Uses exact integer math on the JSON number's decimal form, mirroring the model-discovery
/// pricing parser. Returns `None` for non-numeric, negative, or out-of-range prices.
fn price_micros_at(
    metadata: &BTreeMap<String, serde_json::Value>,
    path: &[&str],
) -> Option<ModelTokenPrice> {
    let value = value_at(metadata, path)?;
    // JSON numbers print their shortest exact decimal form; quoted values are rejected unless
    // they are plain decimals.
    let literal = value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_string);
    let (whole, fractional) = literal.split_once('.').unwrap_or((literal.as_str(), ""));
    let digits = fractional.len().min(12);
    let scale = 10_u128.checked_pow(u32::try_from(digits).ok()?)?;
    let whole = whole.parse::<u128>().ok()?;
    let fractional = if digits > 0 {
        fractional.get(..digits)?.parse::<u128>().ok()?
    } else {
        0
    };
    let numerator = whole.checked_mul(scale)?.checked_add(fractional)?;
    let micros = numerator.checked_mul(1_000_000)?.checked_div(scale)?;
    Some(ModelTokenPrice::from_micros(u64::try_from(micros).ok()?))
}

fn go_reasoning(metadata: &BTreeMap<String, serde_json::Value>) -> Option<ModelReasoningInfo> {
    if !bool_at(metadata, &["capabilities", "reasoning", "supported"]) {
        return None;
    }
    let effort_values = value_at(metadata, &["capabilities", "reasoning", "effort"])
        .and_then(serde_json::Value::as_array)
        .map_or_else(Vec::new, |values| {
            values
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect()
        });
    let default_effort = if string_at(metadata, &["capabilities", "reasoning", "default", "type"])
        .as_deref()
        == Some("effort")
    {
        string_at(metadata, &["capabilities", "reasoning", "default", "value"])
    } else {
        None
    };
    // The chat-completions surface steers depth with named effort levels; a declared token
    // budget is the control shape only when no effort ladder is published.
    let control = if !effort_values.is_empty() {
        Some(ReasoningControl::Adaptive)
    } else if value_at(metadata, &["capabilities", "reasoning", "budget"]).is_some() {
        Some(ReasoningControl::Budget)
    } else {
        None
    };
    Some(ModelReasoningInfo {
        control,
        effort_values,
        default_effort,
        visible_summary_supported: false,
        summary_values: Vec::new(),
        default_summary: None,
        raw_reasoning_supported: false,
        source: ModelReasoningCapabilitySource::ProviderMetadata,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_listing_facts_are_normalized_without_inventing_metadata() {
        let items = vec![
            ModelResponseItem {
                id: "glm-5.3".into(),
                created: None,
                metadata: serde_json::from_value(serde_json::json!({
                    "name": "GLM-5.3",
                    "limit": {"context": 1_000_000, "output": 131_072},
                    "cost": {"text": {
                        "input": {"price": 1.4, "cache": {"read": 0.26}},
                        "output": {"price": 4.4},
                    }},
                    "capabilities": {
                        "tools": {"supported": true},
                        "reasoning": {"supported": true, "effort": ["low", "high", "max"]},
                        "structured_output": true,
                    },
                    "modalities": {"input": ["text"], "output": ["text"]},
                }))
                .unwrap(),
            },
            ModelResponseItem {
                id: "gemini-4-argon".into(),
                created: None,
                metadata: serde_json::from_value(serde_json::json!({
                    "name": "Gemini 4 Argon",
                    "limit": {"context": 1_048_576, "output": 65_536},
                    "capabilities": {
                        "tools": {"supported": true},
                        "reasoning": {"supported": true, "effort": ["low", "medium", "high"]},
                        "cache": {"implicit": true},
                        "temperature": false,
                    },
                }))
                .unwrap(),
            },
            ModelResponseItem {
                id: "   ".into(),
                created: None,
                metadata: BTreeMap::new(),
            },
        ];
        let models = model_infos_from_items(items);
        assert_eq!(models.len(), 2);

        let glm = &models[0];
        assert_eq!(glm.model_id, "glm-5.3");
        assert_eq!(glm.display_name, "GLM-5.3");
        assert_eq!(glm.context_window, Some(1_000_000));
        assert_eq!(glm.max_output_tokens, Some(131_072));
        assert!(glm.capabilities.contains(&ModelCapability::ToolCalls));
        assert!(glm.capabilities.contains(&ModelCapability::Reasoning));
        assert!(glm.capabilities.contains(&ModelCapability::JsonMode));
        assert!(!glm.capabilities.contains(&ModelCapability::PromptCaching));
        let pricing = glm.pricing.as_ref().unwrap();
        assert_eq!(pricing.input.as_ref().unwrap().micros, 1_400_000);
        assert_eq!(pricing.output.as_ref().unwrap().micros, 4_400_000);
        assert_eq!(pricing.cached_input.as_ref().unwrap().micros, 260_000);
        assert!(pricing.cache_write_input.is_none());
        let reasoning = glm.reasoning.as_ref().unwrap();
        assert_eq!(reasoning.effort_values, ["low", "high", "max"]);
        assert_eq!(reasoning.control, Some(ReasoningControl::Adaptive));
        assert!(!glm.capabilities.contains(&ModelCapability::ImageInput));

        let gemini = &models[1];
        assert!(
            gemini
                .cache
                .capabilities
                .contains(&bcode_model::ModelCacheCapability::AutomaticPrefixCache)
        );
        assert!(
            gemini
                .capabilities
                .contains(&ModelCapability::PromptCaching)
        );
        assert!(gemini.pricing.is_none());
        assert_eq!(gemini.reasoning.as_ref().unwrap().default_effort, None);
    }

    #[test]
    fn go_explicit_cache_windows_become_ttl_seconds() {
        let items = vec![ModelResponseItem {
            id: "qwen4-max".into(),
            created: None,
            metadata: serde_json::from_value(serde_json::json!({
                "capabilities": {
                    "cache": {"implicit": true, "explicit": {"ttl": ["5m", "1h"]}},
                },
                "cost": {"text": {"input": {"price": 1.5, "cache": {
                    "read": 0.1875,
                    "write": {"5m": 1.875, "1h": 3.0},
                }}}},
            }))
            .unwrap(),
        }];
        let models = model_infos_from_items(items);
        let cache = &models[0].cache;
        assert!(
            cache
                .capabilities
                .contains(&bcode_model::ModelCacheCapability::ExplicitCachePoints)
        );
        assert_eq!(cache.ttl_seconds, BTreeSet::from([300, 3600]));
    }

    #[test]
    fn go_budget_only_reasoning_keeps_budget_control() {
        let items = vec![ModelResponseItem {
            id: "qwen3.7-plus".into(),
            created: None,
            metadata: serde_json::from_value(serde_json::json!({
                "capabilities": {
                    "reasoning": {"supported": true, "toggle": true, "budget": {"max": 262_144}},
                },
            }))
            .unwrap(),
        }];
        let models = model_infos_from_items(items);
        let reasoning = models[0].reasoning.as_ref().unwrap();
        assert!(
            reasoning
                .control
                .is_some_and(|control| control == ReasoningControl::Budget)
        );
        assert_eq!(reasoning.effort_values, [] as [String; 0]);
    }

    #[test]
    fn go_default_effort_and_invalid_prices_are_handled() {
        let items = vec![
            ModelResponseItem {
                id: "kimi-k3.1-pro".into(),
                created: None,
                metadata: serde_json::from_value(serde_json::json!({
                    "capabilities": {
                        "reasoning": {"supported": true, "effort": ["low", "max"],
                            "default": {"type": "effort", "value": "max"}},
                    },
                }))
                .unwrap(),
            },
            ModelResponseItem {
                id: "broken-price".into(),
                created: None,
                metadata: serde_json::from_value(serde_json::json!({
                    "cost": {"text": {"input": {"price": -2}, "output": {"price": "free"}}},
                }))
                .unwrap(),
            },
        ];
        let models = model_infos_from_items(items);
        assert_eq!(
            models[0]
                .reasoning
                .as_ref()
                .unwrap()
                .default_effort
                .as_deref(),
            Some("max")
        );
        // Negative and non-numeric prices are rejected, not clamped to zero.
        assert!(models[1].pricing.is_none());
    }

    #[test]
    fn keyless_listing_is_confined_to_the_dedicated_go_endpoint() {
        let mut settings = super::super::tests::test_settings(
            super::super::AuthSettings::Missing,
            super::super::OpenAiCompatibleDialect::ChatCompletions,
        );
        settings.base_url = BASE_URL.to_owned();
        assert!(permits_keyless_listing(&settings));
        settings.base_url = "https://example.com/inference/go/openai/v1".into();
        assert!(!permits_keyless_listing(&settings));
        settings.base_url = "https://api.openai.com/v1".into();
        assert!(!permits_keyless_listing(&settings));
    }

    #[test]
    fn go_credentials_never_fall_back_to_other_accounts_and_grok_keeps_go_route() {
        use bcode_model::ProviderRequestContext;
        let mut context = ProviderRequestContext::default();
        context
            .settings
            .insert("base_url".to_owned(), BASE_URL.to_owned());
        context
            .settings
            .insert("dialect".to_owned(), "chat_completions".to_owned());
        context.settings.insert(
            bcode_model::CATALOG_PROVIDER_ID_SETTING.to_owned(),
            "opencode-go".to_owned(),
        );
        context.env.insert(
            "OPENAI_API_KEY".to_owned(),
            "other-provider-secret".to_owned(),
        );
        let missing = super::super::settings_for_context(&context);
        assert!(matches!(missing.auth, super::super::AuthSettings::Missing));
        context
            .env
            .insert("OPENCODE_GO_API_KEY".to_owned(), "test-go-key".to_owned());
        context
            .env
            .insert("BCODE_OPENAI_MODEL".to_owned(), "grok-4.7".to_owned());
        let settings = super::super::settings_for_context(&context);
        assert!(
            matches!(settings.auth, super::super::AuthSettings::ApiKey(ref key) if key == "test-go-key")
        );
        assert_eq!(settings.base_url, BASE_URL);
        assert_eq!(
            settings.dialect,
            super::super::OpenAiCompatibleDialect::ChatCompletions
        );
    }

    #[test]
    fn go_endpoint_precedence_fails_closed_and_environment_route_is_recognized() {
        let mut context = bcode_model::ProviderRequestContext::default();
        context.settings.insert("base_url".into(), BASE_URL.into());
        context
            .settings
            .insert("openai.base_url".into(), "https://example.com/v1".into());
        context.settings.insert(
            bcode_model::CATALOG_PROVIDER_ID_SETTING.into(),
            "opencode-go".into(),
        );
        context
            .env
            .insert("OPENCODE_GO_API_KEY".into(), "test-go-key".into());
        assert!(matches!(
            super::super::settings_for_context(&context).auth,
            super::super::AuthSettings::Missing
        ));
        context.settings.clear();
        context
            .env
            .insert("BCODE_OPENAI_BASE_URL".into(), BASE_URL.into());
        context
            .env
            .insert("BCODE_OPENAI_MODEL".into(), "grok-4.7".into());
        let settings = super::super::settings_for_context(&context);
        assert_eq!(settings.base_url, BASE_URL);
        assert!(
            matches!(settings.auth, super::super::AuthSettings::ApiKey(ref key) if key == "test-go-key")
        );
    }

    #[test]
    fn go_model_list_keeps_its_support_target_and_unknown_cache_metadata() {
        let mut request = bcode_model::ModelListRequest::default();
        request.provider_context.settings = std::collections::BTreeMap::from([
            ("base_url".into(), BASE_URL.into()),
            ("dialect".into(), "chat_completions".into()),
            ("live_discovery".into(), "false".into()),
            (
                bcode_model::CATALOG_PROVIDER_ID_SETTING.into(),
                "opencode-go".into(),
            ),
        ]);
        let plugin = super::super::OpenAiCompatibleProviderPlugin::default();
        let result = plugin.models(&request);
        assert_ne!(result.models, []);
        for model in result.models {
            assert_eq!(model.cache, bcode_model::ModelCacheInfo::default());
        }
        let bcode_model::ModelCatalogPolicy::ExpandSupported {
            provider_id,
            target,
            ..
        } = result.catalog.policy
        else {
            panic!("Go catalog expansion expected")
        };
        assert_eq!(provider_id, "opencode-go");
        assert_eq!(target.provider, "opencode-go");
        assert_eq!(target.api_surface, "chat_completions");
        assert_eq!(target.auth_mode, "api_key");
    }

    #[test]
    fn go_headers_are_stable_per_conversation_and_not_sent_elsewhere() {
        let mut settings = super::super::settings();
        settings.base_url = BASE_URL.to_owned();
        let request = super::super::tests::test_request(Vec::new());
        let client = reqwest::Client::new();
        let build = |settings: &Settings, request: &ModelTurnRequest| {
            identify_request(client.post(BASE_URL), settings, request)
                .build()
                .unwrap()
        };
        let first = build(&settings, &request);
        let retry = build(&settings, &request);
        assert_eq!(
            first.headers()["x-opencode-session"],
            retry.headers()["x-opencode-session"]
        );
        assert_eq!(
            first.headers()["User-Agent"],
            concat!("bcode/", env!("CARGO_PKG_VERSION"))
        );
        let mut another = request.clone();
        another.session_id = bcode_session_models::SessionId::new();
        assert_ne!(
            first.headers()["x-opencode-session"],
            build(&settings, &another).headers()["x-opencode-session"]
        );
        settings.base_url = "https://example.com/v1".to_owned();
        assert!(
            !build(&settings, &request)
                .headers()
                .contains_key("x-opencode-session")
        );
    }

    #[test]
    fn go_route_is_exact_and_discovery_uses_documented_endpoint() {
        assert!(is_go_endpoint(BASE_URL));
        assert!(is_go_endpoint(&format!("{BASE_URL}/")));
        for other in [
            "https://opencode.ai/inference/openai/v1",
            "https://opencode.ai.evil.example/inference/go/openai/v1",
            "https://example.com/inference/go/openai/v1",
            "http://opencode.ai/inference/go/openai/v1",
        ] {
            assert!(!is_go_endpoint(other));
        }
        let mut settings = super::super::settings();
        settings.base_url = BASE_URL.to_owned();
        assert_eq!(models_url(&settings), MODELS_URL);
        settings.base_url = "https://example.com/v1/".to_owned();
        assert_eq!(models_url(&settings), "https://example.com/v1/models");
    }
}
