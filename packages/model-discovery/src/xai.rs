//! xAI live model discovery.

use crate::{Error, Result, generated_at};
use bcode_model_catalog_models::{CatalogCapabilities, LiveCatalogSnapshot, LiveModel};
use reqwest::Client;

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

    let url = format!("{XAI_API_BASE}/language-models");
    let response = client
        .get(&url)
        .bearer_auth(&key)
        .send()
        .await
        .map_err(|e| Error::Provider(format!("xAI request failed: {e}")))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(Error::Provider(format!("xAI API error {status}: {body}")));
    }

    let payload: LanguageModelsResponse = response
        .json()
        .await
        .map_err(|e| Error::Provider(format!("failed to parse xAI response: {e}")))?;

    let mut snapshot = LiveCatalogSnapshot::empty("xai", generated_at());
    for model in payload.models {
        // Only include chat/language models for now (those with max_prompt_length)
        if model.max_prompt_length.is_some() {
            let live_model = live_model_from_api(model);
            snapshot
                .models
                .insert(live_model.model_id.clone(), live_model);
        }
    }

    Ok(snapshot)
}

/// Response shape for /v1/language-models.
#[derive(Debug, Clone, serde::Deserialize)]
struct LanguageModelsResponse {
    models: Vec<LanguageModel>,
}

/// Individual language model from xAI API.
#[derive(Debug, Clone, serde::Deserialize)]
struct LanguageModel {
    id: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    max_prompt_length: Option<u32>,
    #[serde(default)]
    input_modalities: Vec<String>,
    #[serde(default)]
    output_modalities: Vec<String>,
    #[allow(dead_code)]
    #[serde(default)]
    owned_by: Option<String>,
}

fn live_model_from_api(model: LanguageModel) -> LiveModel {
    let context_window = model.max_prompt_length;
    LiveModel {
        model_id: model.id,
        display_name: model.version,
        status: None,
        regions: std::collections::BTreeSet::new(),
        capabilities: capabilities_from_modalities(
            &model.input_modalities,
            &model.output_modalities,
        ),
        context_window,
        max_output_tokens: None,
        raw: None,
    }
}

fn capabilities_from_modalities(input: &[String], output: &[String]) -> CatalogCapabilities {
    CatalogCapabilities {
        text_input: input.iter().any(|m| m.eq_ignore_ascii_case("text")),
        image_input: input.iter().any(|m| m.eq_ignore_ascii_case("image")),
        text_output: output.iter().any(|m| m.eq_ignore_ascii_case("text")),
        tool_use: false,
        structured_outputs: false,
        reasoning: false,
        prompt_cache: false,
        native_web_search: false,
    }
}
