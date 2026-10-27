//! Injected, provider-neutral model execution for recap composition.
//!
//! Adapters own authentication and wire formats. No implementation may manufacture a
//! successful AI result when a model is unavailable or returns invalid data.
use std::{collections::BTreeMap, future::Future, pin::Pin};

use serde::{Deserialize, Serialize};

use crate::{contracts::Section, process::Cancellation};

/// An in-memory source image, with identity matching the document asset registry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageAsset {
    /// Registered asset identity.
    pub asset_id: String,
    /// Passive raster image media type.
    pub mime_type: String,
    /// Encoded image bytes, not a path, URL, or provider-specific data URI.
    pub bytes: Vec<u8>,
}

/// Normalized multimodal request. The adapter must preserve the separation between
/// trusted instructions and untrusted source text/images.
#[derive(Clone, Debug)]
pub struct ModelRequest {
    /// Trusted composition policy, not source content.
    pub instructions: String,
    /// JSON-encoded source evidence and explicit generation/refinement preferences.
    pub input_text: String,
    /// Actual extracted frames, not fabricated illustrations.
    pub images: Vec<ImageAsset>,
    /// Maximum accepted serialized response bytes.
    pub max_response_bytes: usize,
}

/// Strict structured model result. Only editorial content is model-controlled;
/// source metadata, timestamps, evidence and asset registries are host-owned.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelResponse {
    /// Plain-text recap title.
    pub title: String,
    /// Ordered chapters with stable identities and grounded blocks.
    pub sections: Vec<Section>,
    /// Every selected visual asset must have a nonempty explanation of why it helps.
    /// Serialized as strict `{asset_id, reason}` entries for portable structured output.
    #[serde(with = "choice_entries")]
    pub image_choices: BTreeMap<String, String>,
}

mod choice_entries {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Entry {
        asset_id: String,
        reason: String,
    }

    pub fn serialize<S: serde::Serializer>(
        choices: &BTreeMap<String, String>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        choices
            .iter()
            .map(|(asset_id, reason)| Entry {
                asset_id: asset_id.clone(),
                reason: reason.clone(),
            })
            .collect::<Vec<_>>()
            .serialize(serializer)
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<String, String>, D::Error> {
        let entries = Vec::<Entry>::deserialize(deserializer)?;
        let mut choices = BTreeMap::new();
        for entry in entries {
            if choices.insert(entry.asset_id, entry.reason).is_some() {
                return Err(serde::de::Error::custom("duplicate visual choice"));
            }
        }
        Ok(choices)
    }
}

/// Return the portable strict structured-output schema for [`ModelResponse`].
///
/// All objects are closed and every declared property is required. Tagged block
/// variants use nested `anyOf`; visual-choice maps serialize as named entries,
/// avoiding open-ended object properties unsupported by strict model decoders.
/// Schema validation does not replace composition's source-grounding validation.
#[must_use]
pub fn response_schema() -> serde_json::Value {
    use serde_json::json;
    let string = json!({"type":"string"});
    let text = schema_object(
        json!({"type":{"type":"string","enum":["text"]},"text":string}),
        &["type", "text"],
    );
    let image = schema_object(
        json!({"type":{"type":"string","enum":["image"]},"asset_id":string,"caption":string}),
        &["type", "asset_id", "caption"],
    );
    let clip = schema_object(
        json!({"type":{"type":"string","enum":["clip"]},"asset_id":string,"caption":string}),
        &["type", "asset_id", "caption"],
    );
    let item = schema_object(
        json!({"label":string,"asset_id":string}),
        &["label", "asset_id"],
    );
    let comparison = schema_object(
        json!({"type":{"type":"string","enum":["comparison"]},"items":{"type":"array","items":item}}),
        &["type", "items"],
    );
    let block = schema_object(
        json!({"id":string,"evidence_ids":{"type":"array","items":string},"content":{"anyOf":[text,image,clip,comparison]}}),
        &["id", "evidence_ids", "content"],
    );
    let section = schema_object(
        json!({"id":string,"title":string,"blocks":{"type":"array","items":block}}),
        &["id", "title", "blocks"],
    );
    let choice = schema_object(
        json!({"asset_id":string,"reason":string}),
        &["asset_id", "reason"],
    );
    schema_object(
        json!({"title":string,"sections":{"type":"array","items":section},"image_choices":{"type":"array","items":choice}}),
        &["title", "sections", "image_choices"],
    )
}

fn schema_object(properties: serde_json::Value, required: &[&str]) -> serde_json::Value {
    let mut object = serde_json::Map::new();
    object.insert("type".into(), serde_json::Value::String("object".into()));
    object.insert("properties".into(), properties);
    object.insert("required".into(), serde_json::json!(required));
    object.insert(
        "additionalProperties".into(),
        serde_json::Value::Bool(false),
    );
    serde_json::Value::Object(object)
}

impl ModelResponse {
    /// Decode a bounded strict response; no markdown fences or guessed JSON extraction.
    ///
    /// # Errors
    /// Returns an error for oversized or malformed structured output.
    pub fn from_json(bytes: &[u8], max_bytes: usize) -> Result<Self, ModelError> {
        if bytes.len() > max_bytes {
            return Err(ModelError::InvalidResponse);
        }
        serde_json::from_slice(bytes).map_err(|_| ModelError::InvalidResponse)
    }
}

/// Secret-safe model failures. Provider-native messages must not cross this boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ModelError {
    /// Explicit cancellation, not a successful empty response.
    #[error("recap model execution cancelled")]
    Cancelled,
    /// No configured compatible model or adapter.
    #[error("recap model executor unavailable")]
    Unavailable,
    /// Provider invocation failed; details remain adapter-owned.
    #[error("recap model execution failed")]
    ExecutionFailed,
    /// Response is malformed, oversized, or unsupported.
    #[error("invalid structured recap model response")]
    InvalidResponse,
}

/// Object-safe execution future used without an async-trait dependency.
pub type ModelFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ModelResponse, ModelError>> + Send + 'a>>;

/// Injected executor shared by standalone and Bcode-hosted adapters.
///
/// Implementations must honor cancellation while awaiting the model, bound response
/// bytes before decoding, and return actual structured model output or an error.
pub trait ModelExecutor: Send + Sync {
    /// Execute one normalized text-plus-images request.
    fn execute<'a>(
        &'a self,
        request: ModelRequest,
        cancellation: &'a Cancellation,
    ) -> ModelFuture<'a>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_choice_entries_round_trip_and_reject_duplicates() {
        let response = ModelResponse {
            title: "A recap".into(),
            sections: vec![],
            image_choices: BTreeMap::from([("frame-1".into(), "Relevant source state".into())]),
        };
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["image_choices"][0]["asset_id"], "frame-1");
        assert_eq!(
            serde_json::from_value::<ModelResponse>(json.clone()).unwrap(),
            response
        );
        let mut duplicate = json;
        let entry = duplicate["image_choices"][0].clone();
        duplicate["image_choices"]
            .as_array_mut()
            .unwrap()
            .push(entry);
        assert!(serde_json::from_value::<ModelResponse>(duplicate).is_err());
    }

    #[test]
    fn structured_schema_closes_all_objects_and_requires_their_properties() {
        fn inspect(value: &serde_json::Value) {
            match value {
                serde_json::Value::Object(fields) => {
                    if fields.get("type").is_some_and(|kind| kind == "object") {
                        assert_eq!(fields["additionalProperties"], false);
                        let properties = fields["properties"].as_object().unwrap();
                        let required = fields["required"].as_array().unwrap();
                        assert_eq!(properties.len(), required.len());
                        for name in properties.keys() {
                            assert!(required.iter().any(|item| item.as_str() == Some(name)));
                        }
                    }
                    for child in fields.values() {
                        inspect(child);
                    }
                }
                serde_json::Value::Array(values) => {
                    for child in values {
                        inspect(child);
                    }
                }
                _ => {}
            }
        }
        inspect(&response_schema());
    }
}
