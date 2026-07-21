//! xAI live model discovery.

use crate::{Error, Result, generated_at};
use bcode_model_catalog_models::{CatalogCapabilities, LiveCatalogSnapshot, LiveModel};
use reqwest::Client;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// xAI API base URL.
const XAI_API_BASE: &str = "https://api.x.ai/v1";

/// Environment variable for xAI API key.
const XAI_API_KEY_ENV: &str = "XAI_API_KEY";

/// Discover live xAI models using the provided API key or environment.
///
/// # Errors
///
/// Returns an error when the API request fails or the response cannot be parsed.
pub async fn discover(api_key: Option<String>) -> Result<LiveCatalogSnapshot> {
    let key = api_key
        .or_else(|| std::env::var(XAI_API_KEY_ENV).ok())
        .ok_or_else(|| {
            Error::Provider(format!(
                "xAI API key required: pass via argument or set {XAI_API_KEY_ENV}"
            ))
        })?;

    let client = Client::builder()
        .user_agent("bcode-model-discovery/1.0")
        .build()
        .map_err(|e| Error::Provider(format!("failed to build http client: {e}")))?;

    let payload: LanguageModelsResponse = get_json(&client, &key, "language-models").await?;
    let mut snapshot = LiveCatalogSnapshot::empty("xai", generated_at());
    for model in payload.into_models() {
        let live_model = live_model_from_language_model(model);
        snapshot
            .models
            .insert(live_model.model_id.clone(), live_model);
    }

    Ok(snapshot)
}

async fn get_json<T: serde::de::DeserializeOwned>(
    client: &Client,
    key: &str,
    path: &str,
) -> Result<T> {
    let url = format!("{XAI_API_BASE}/{path}");
    let response = client
        .get(&url)
        .bearer_auth(key)
        .send()
        .await
        .map_err(|e| Error::Provider(format!("xAI request failed for /v1/{path}: {e}")))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(Error::Provider(format!(
            "xAI API error for /v1/{path} {status}: {body}"
        )));
    }

    response
        .json()
        .await
        .map_err(|e| Error::Provider(format!("failed to parse xAI /v1/{path} response: {e}")))
}

/// Response shape for xAI model-list endpoints.
#[derive(Debug, Clone, serde::Deserialize)]
struct LanguageModelsResponse {
    /// `/v1/language-models` documented shape.
    #[serde(default)]
    models: Vec<LanguageModel>,
    /// OpenAI-compatible/list-like fallback shape.
    #[serde(default)]
    data: Vec<LanguageModel>,
}

impl LanguageModelsResponse {
    fn into_models(self) -> Vec<LanguageModel> {
        if self.models.is_empty() {
            self.data
        } else {
            self.models
        }
    }
}

/// Individual language model from xAI model metadata endpoints.
#[derive(Debug, Clone, serde::Deserialize)]
struct LanguageModel {
    id: String,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    max_prompt_length: Option<u64>,
    #[serde(default)]
    context_window: Option<u64>,
    #[serde(default)]
    max_context_length: Option<u64>,
    #[serde(default)]
    context_length: Option<u64>,
    #[serde(default)]
    max_output_tokens: Option<u64>,
    #[serde(default)]
    output_token_limit: Option<u64>,
    #[serde(default)]
    input_modalities: Vec<String>,
    #[serde(default)]
    output_modalities: Vec<String>,
    #[serde(default)]
    owned_by: Option<String>,
    #[serde(default)]
    fingerprint: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

fn live_model_from_language_model(model: LanguageModel) -> LiveModel {
    let context_window = context_window_from_model(&model);
    let aliases = model
        .aliases
        .into_iter()
        .filter(|alias| !alias.trim().is_empty())
        .collect::<BTreeSet<_>>();
    let max_output_tokens = first_u32([
        model.max_output_tokens,
        model.output_token_limit,
        value_path_u64(&model.extra, &["max_completion_tokens"]),
        value_path_u64(&model.extra, &["max_output"]),
        value_path_u64(&model.extra, &["limits", "max_output_tokens"]),
    ]);
    let raw = Some(json!({
        "owned_by": model.owned_by,
        "fingerprint": model.fingerprint,
        "input_modalities": model.input_modalities,
        "output_modalities": model.output_modalities,
        "extra": model.extra,
    }));
    LiveModel {
        model_id: model.id,
        target: None,
        display_name: model.version,
        aliases,
        status: None,
        regions: BTreeSet::new(),
        capabilities: capabilities_from_modalities(
            raw.as_ref()
                .and_then(|value| value.get("input_modalities"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str),
            raw.as_ref()
                .and_then(|value| value.get("output_modalities"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str),
        ),
        context_window,
        max_output_tokens,
        reasoning: None,
        raw,
    }
}

fn context_window_from_model(model: &LanguageModel) -> Option<u32> {
    first_u32([
        model.max_prompt_length,
        model.context_window,
        model.max_context_length,
        model.context_length,
        value_path_u64(&model.extra, &["max_prompt_tokens"]),
        value_path_u64(&model.extra, &["max_input_tokens"]),
        value_path_u64(&model.extra, &["input_token_limit"]),
        value_path_u64(&model.extra, &["prompt_token_limit"]),
        value_path_u64(&model.extra, &["context_window_tokens"]),
        value_path_u64(&model.extra, &["limits", "max_prompt_length"]),
        value_path_u64(&model.extra, &["limits", "max_prompt_tokens"]),
        value_path_u64(&model.extra, &["limits", "context_window"]),
        value_path_u64(&model.extra, &["limits", "max_context_length"]),
    ])
}

fn first_u32(values: impl IntoIterator<Item = Option<u64>>) -> Option<u32> {
    values
        .into_iter()
        .flatten()
        .find_map(|value| u32::try_from(value).ok().filter(|value| *value > 0))
}

fn value_path_u64(extra: &BTreeMap<String, Value>, path: &[&str]) -> Option<u64> {
    let (first, rest) = path.split_first()?;
    let mut value = extra.get(*first)?;
    for segment in rest {
        value = value.get(*segment)?;
    }
    value.as_u64().or_else(|| value.as_str()?.parse().ok())
}

fn capabilities_from_modalities<'a>(
    input: impl Iterator<Item = &'a str>,
    output: impl Iterator<Item = &'a str>,
) -> CatalogCapabilities {
    let input = input.collect::<Vec<_>>();
    let output = output.collect::<Vec<_>>();
    CatalogCapabilities {
        text_input: input.iter().any(|m| m.eq_ignore_ascii_case("text")),
        image_input: input.iter().any(|m| m.eq_ignore_ascii_case("image")),
        text_output: output.iter().any(|m| m.eq_ignore_ascii_case("text")),
        // xAI documents function calling, including parallel function calling, for its language
        // model API: https://docs.x.ai/docs/guides/function-calling#parallel-function-calling.
        // This discovery module only consumes `/v1/language-models`, so every model represented
        // here belongs to that documented API surface.
        tool_use: true,
        parallel_tool_calls: true,
        structured_outputs: false,
        reasoning: false,
        prompt_cache: false,
        native_web_search: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovered_xai_language_models_advertise_documented_tool_capabilities() {
        let capabilities = capabilities_from_modalities(["text"].into_iter(), ["text"].into_iter());

        assert!(capabilities.tool_use);
        assert!(capabilities.parallel_tool_calls);
    }

    #[test]
    fn documented_tool_capabilities_do_not_depend_on_optional_modality_metadata() {
        let capabilities = capabilities_from_modalities(std::iter::empty(), std::iter::empty());

        assert!(capabilities.tool_use);
        assert!(capabilities.parallel_tool_calls);
    }
}
