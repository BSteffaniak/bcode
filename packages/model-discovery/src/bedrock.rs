//! Amazon Bedrock live model discovery.

use crate::{Error, Result, generated_at};
use aws_config::BehaviorVersion;
use aws_config::meta::region::RegionProviderChain;
use aws_sdk_bedrock::Client;
use aws_sdk_bedrock::config::Region;
use aws_sdk_bedrock::types::{FoundationModelSummary, ModelModality};
use bcode_model_catalog_models::{CatalogCapabilities, LiveCatalogSnapshot, LiveModel};
use std::collections::BTreeMap;

/// Discover live Bedrock models across the provided regions.
///
/// # Errors
///
/// Returns an error when an AWS API call fails.
pub async fn discover(regions: &[String]) -> Result<LiveCatalogSnapshot> {
    let mut snapshot = LiveCatalogSnapshot::empty("bedrock", generated_at());
    for region in regions {
        discover_region(region, &mut snapshot).await?;
    }
    Ok(snapshot)
}

async fn discover_region(region: &str, snapshot: &mut LiveCatalogSnapshot) -> Result<()> {
    let region_provider = RegionProviderChain::first_try(Region::new(region.to_string()));
    let config = aws_config::defaults(BehaviorVersion::latest())
        .region(region_provider)
        .load()
        .await;
    let client = Client::new(&config);
    let output = client
        .list_foundation_models()
        .send()
        .await
        .map_err(|error| {
            Error::Provider(format!("bedrock list_foundation_models {region}: {error}"))
        })?;

    for summary in output.model_summaries() {
        merge_summary(snapshot, region, summary);
    }
    Ok(())
}

fn merge_summary(
    snapshot: &mut LiveCatalogSnapshot,
    region: &str,
    summary: &FoundationModelSummary,
) {
    let entry = snapshot
        .models
        .entry(summary.model_id.clone())
        .or_insert_with(|| live_model_from_summary(summary));
    entry.regions.insert(region.to_string());
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
