//! Request-only hydration of artifact-backed tool-result images.

use super::{
    ContentBlock, MAX_ARTIFACT_RANGE_BYTES, ModelTurnRequest, ServerState, SessionId,
    read_session_artifact_range, resolved_provider_models, select_model_info,
};
use base64::Engine as _;
use std::fmt::Write as _;

const HOST_IMAGE_BASE64_SAFETY_CEILING: u64 = 5 * 1024 * 1024;

pub async fn hydrate_tool_result_images(
    state: &ServerState,
    session_id: SessionId,
    provider_plugin_id: Option<&str>,
    request: &mut ModelTurnRequest,
) {
    if !needs_artifact_hydration(&request.messages) {
        return;
    }
    let Some((supported, catalog_limit)) = resolve_image_input_support(
        state,
        provider_plugin_id,
        &request.model_id,
        &request.provider_context,
    )
    .await
    else {
        return;
    };
    if !supported {
        return;
    }
    let encoded_limit = catalog_limit.min(HOST_IMAGE_BASE64_SAFETY_CEILING);
    if encoded_limit == 0 {
        return;
    }

    let mut cache = RequestImageCache::default();
    for message in &mut request.messages {
        for block in &mut message.content {
            let ContentBlock::ToolResult { result } = block else {
                continue;
            };
            for content in &mut result.content {
                let bcode_model::ToolResultContent::ImageRef { image } = content else {
                    continue;
                };
                let Some((artifact_id, reference_key)) = artifact_identity_for_image_ref(image)
                else {
                    continue;
                };
                let identity = (artifact_id.clone(), reference_key.clone());
                let hydrated = if let Some(encoded) = cache.get(&identity) {
                    Ok(encoded.to_owned())
                } else {
                    read_image_artifact_snapshot(
                        state,
                        session_id,
                        &artifact_id,
                        &reference_key,
                        encoded_limit,
                    )
                    .await
                    .map(|(encoded, finalized)| {
                        if finalized {
                            cache.insert(identity, &encoded);
                        }
                        encoded
                    })
                };
                match hydrated {
                    Ok(data_base64) => {
                        *content = bcode_model::ToolResultContent::Image {
                            image: bcode_model::ImageContent {
                                mime_type: image.mime_type.clone(),
                                data_base64,
                                metadata: image.metadata.clone(),
                            },
                        };
                    }
                    Err(reason) => {
                        let _ = write!(
                            result.output,
                            "\n\n[image not inlined: {reason}; reference remains available at {}]",
                            image.path
                        );
                    }
                }
            }
        }
    }
}

fn needs_artifact_hydration(messages: &[bcode_model::ModelMessage]) -> bool {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .any(|block| {
            let ContentBlock::ToolResult { result } = block else {
                return false;
            };
            result.content.iter().any(|content| {
                matches!(content,
            bcode_model::ToolResultContent::ImageRef { image }
                if image.artifact_id.is_some() && image.reference_key.is_some())
            })
        })
}

#[derive(Default)]
struct RequestImageCache {
    bytes: usize,
    images: std::collections::BTreeMap<(String, String), String>,
}

impl RequestImageCache {
    fn get(&self, identity: &(String, String)) -> Option<&str> {
        self.images.get(identity).map(String::as_str)
    }

    fn insert(&mut self, identity: (String, String), encoded: &str) {
        // Bound both retained bytes and keys. Skipping a cache entry never drops context.
        if self.images.len() >= 32
            || self.images.contains_key(&identity)
            || self.bytes.saturating_add(encoded.len()) > 5 * 1024 * 1024
        {
            return;
        }
        self.bytes += encoded.len();
        self.images.insert(identity, encoded.to_owned());
    }
}

async fn resolve_image_input_support(
    state: &ServerState,
    provider_plugin_id: Option<&str>,
    model_id: &str,
    provider_context: &bcode_model::ProviderRequestContext,
) -> Option<(bool, u64)> {
    let provider = state
        .plugins
        .invoke_service_json::<
            bcode_model::ProviderCapabilitiesRequest,
            bcode_model::ProviderCapabilities,
        >(
            provider_plugin_id?,
            bcode_model::MODEL_PROVIDER_INTERFACE_ID,
            bcode_model::OP_CAPABILITIES,
            &bcode_model::ProviderCapabilitiesRequest {
                provider_context: provider_context.clone(),
                selected_model_id: Some(model_id.to_owned()),
            },
        )
        .await
        .ok()?;
    let models = resolved_provider_models(
        state,
        provider_plugin_id.map(ToOwned::to_owned),
        bcode_model::ModelListRequest {
            provider_context: provider_context.clone(),
            selected_model_id: Some(model_id.to_owned()),
        },
    )
    .await
    .ok()?;
    let model = select_model_info(&models.models, Some(model_id))?;
    image_support_from_capabilities(&provider, &model)
}

fn image_support_from_capabilities(
    provider: &bcode_model::ProviderCapabilities,
    model: &bcode_model::ModelInfo,
) -> Option<(bool, u64)> {
    let feature = bcode_model::RequestedModelFeature::MediaInput(
        bcode_model::MediaInputFeature::ToolResultImage,
    );
    let guaranteed = matches!(
        provider
            .feature_support
            .negotiate(&model.feature_support, feature),
        bcode_model::NegotiatedFeatureSupport::Guaranteed { .. }
    );
    Some((guaranteed, model.max_image_input_base64_bytes?))
}

fn artifact_identity_for_image_ref(
    image: &bcode_model::ImageRefContent,
) -> Option<(String, String)> {
    Some((image.artifact_id.clone()?, image.reference_key.clone()?))
}

#[cfg(test)]
pub async fn read_image_artifact(
    state: &ServerState,
    session_id: SessionId,
    artifact_id: &str,
    reference_key: &str,
    encoded_limit: u64,
) -> Result<String, String> {
    read_image_artifact_snapshot(state, session_id, artifact_id, reference_key, encoded_limit)
        .await
        .map(|(encoded, _)| encoded)
}

async fn read_image_artifact_snapshot(
    state: &ServerState,
    session_id: SessionId,
    artifact_id: &str,
    reference_key: &str,
    encoded_limit: u64,
) -> Result<(String, bool), String> {
    let raw_limit = encoded_limit.saturating_mul(3) / 4;
    let mut offset = 0_u64;
    let mut expected_total = None;
    let mut revision = None;
    let mut finalized = true;
    let mut bytes = Vec::new();
    loop {
        let remaining = raw_limit.saturating_add(1).saturating_sub(offset);
        if remaining == 0 {
            return Err(format!("encoded image exceeds {encoded_limit} bytes"));
        }
        let length = u32::try_from(remaining.min(u64::from(MAX_ARTIFACT_RANGE_BYTES)))
            .unwrap_or(MAX_ARTIFACT_RANGE_BYTES);
        let response = read_session_artifact_range(
            state,
            session_id,
            artifact_id,
            reference_key,
            offset,
            length,
        )
        .await?;
        if response.offset != offset
            || revision.is_some_and(|value| value != response.reference_revision)
        {
            return Err("image artifact changed revision or returned the wrong offset".to_string());
        }
        revision = Some(response.reference_revision);
        finalized &= response.finalized;
        let total_bytes = response.total_bytes;
        let chunk = response.bytes;
        validate_image_chunk(
            expected_total,
            total_bytes,
            offset,
            chunk.len(),
            u64::from(length),
            raw_limit,
        )?;
        expected_total = Some(total_bytes);
        bytes.extend_from_slice(&chunk);
        offset = offset.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
        if offset >= total_bytes {
            break;
        }
        if offset > raw_limit {
            return Err(format!("encoded image exceeds {encoded_limit} bytes"));
        }
    }
    let encoded = encode_image_bytes(bytes, encoded_limit)?;
    Ok((encoded, finalized))
}

fn validate_image_chunk(
    expected_total: Option<u64>,
    total: u64,
    offset: u64,
    chunk_len: usize,
    requested: u64,
    raw_limit: u64,
) -> Result<(), String> {
    let len = u64::try_from(chunk_len).map_err(|_| "invalid image artifact range")?;
    if expected_total.is_some_and(|expected| expected != total)
        || total > raw_limit
        || offset > total
        || len > requested
        || len > total.saturating_sub(offset)
        || (len == 0 && offset < total)
    {
        return Err(
            "image artifact returned an inconsistent, oversized, or non-progressing range"
                .to_string(),
        );
    }
    Ok(())
}

fn encode_image_bytes(bytes: Vec<u8>, encoded_limit: u64) -> Result<String, String> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    if u64::try_from(encoded.len()).unwrap_or(u64::MAX) > encoded_limit {
        return Err(format!("encoded image exceeds {encoded_limit} bytes"));
    }
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capability_support() -> bcode_model::CapabilitySupport {
        bcode_model::CapabilitySupport::supported(bcode_model::CapabilitySource::BundledCatalog)
    }

    fn provider_capabilities(supported: bool) -> bcode_model::ProviderCapabilities {
        let mut feature_support = bcode_model::ModelFeatureSupport::default();
        if supported {
            feature_support.media_input.insert(
                bcode_model::MediaInputFeature::ToolResultImage,
                capability_support(),
            );
        }
        bcode_model::ProviderCapabilities {
            provider_id: "provider".to_owned(),
            display_name: "Provider".to_owned(),
            capabilities: std::collections::BTreeSet::new(),
            feature_support,
            auth_schemes: std::collections::BTreeSet::new(),
            retry_rules: Vec::new(),
            metadata: std::collections::BTreeMap::new(),
        }
    }

    fn model_info(supported: bool) -> bcode_model::ModelInfo {
        let mut feature_support = bcode_model::ModelFeatureSupport::default();
        if supported {
            feature_support.media_input.insert(
                bcode_model::MediaInputFeature::ToolResultImage,
                capability_support(),
            );
        }
        bcode_model::ModelInfo {
            model_id: "model".to_owned(),
            display_name: "Model".to_owned(),
            is_default: false,
            context_window: None,
            max_output_tokens: None,
            max_image_input_base64_bytes: Some(5_242_880),
            capabilities: std::collections::BTreeSet::new(),
            feature_support,
            reasoning: None,
            cache: bcode_model::ModelCacheInfo::default(),
            metadata_source: None,
            pricing: None,
            api_surface: None,
            visibility: bcode_model::ModelVisibility::Visible,
        }
    }

    #[test]
    fn artifact_ranges_require_progress_consistent_totals_and_requested_bounds() {
        assert!(validate_image_chunk(None, 8, 0, 4, 4, 8).is_ok());
        assert!(validate_image_chunk(Some(8), 8, 4, 4, 4, 8).is_ok());
        assert!(validate_image_chunk(None, 0, 0, 0, 4, 8).is_ok());
        for (expected, total, offset, len, requested, limit) in [
            (None, 8, 0, 0, 4, 8),
            (Some(8), 9, 4, 4, 4, 16),
            (None, 9, 0, 4, 4, 8),
            (None, 8, 0, 5, 4, 8),
            (Some(8), 8, 7, 2, 4, 8),
            (Some(8), 8, 9, 0, 4, 8),
        ] {
            assert!(validate_image_chunk(expected, total, offset, len, requested, limit).is_err());
        }
    }

    #[test]
    fn request_cache_is_identity_scoped_bounded_and_disposable() {
        let mut cache = RequestImageCache::default();
        let key = ("artifact".to_string(), "image".to_string());
        cache.insert(key.clone(), "AAAA");
        cache.insert(key.clone(), "BBBB");
        assert_eq!(cache.get(&key), Some("AAAA"));
        assert_eq!(cache.bytes, 4);
        assert!(
            cache
                .get(&("other".to_string(), "image".to_string()))
                .is_none()
        );
        for n in 0..40 {
            cache.insert((n.to_string(), "image".to_string()), "AQID");
        }
        assert_eq!(cache.images.len(), 32);
        assert!(RequestImageCache::default().get(&key).is_none());
        let mut bounded = RequestImageCache::default();
        bounded.insert(key, &"A".repeat(5 * 1024 * 1024 + 1));
        assert!(bounded.images.is_empty());
    }

    #[test]
    fn only_artifact_backed_tool_images_require_hydration() {
        use bcode_model::{
            ImageContent, ImageMetadata, ImageRefContent, MessageRole, ModelMessage, ToolResult,
            ToolResultContent,
        };
        assert!(!needs_artifact_hydration(&[]));
        let mut message = ModelMessage {
            role: MessageRole::Tool,
            content: vec![ContentBlock::Text {
                text: "hello".to_string(),
            }],
        };
        assert!(!needs_artifact_hydration(std::slice::from_ref(&message)));
        message.content = vec![ContentBlock::Image {
            image: ImageContent {
                mime_type: "image/png".to_string(),
                data_base64: "AQID".to_string(),
                metadata: ImageMetadata::default(),
            },
        }];
        assert!(!needs_artifact_hydration(std::slice::from_ref(&message)));
        for (artifact_id, reference_key, expected) in [
            (None, None, false),
            (Some("artifact"), None, false),
            (None, Some("image"), false),
            (Some("artifact"), Some("image"), true),
        ] {
            message.content = vec![ContentBlock::ToolResult {
                result: ToolResult {
                    call_id: "call".to_string(),
                    output: String::new(),
                    is_error: false,
                    content: vec![ToolResultContent::ImageRef {
                        image: ImageRefContent {
                            path: "/not-authority/image.png".to_string(),
                            mime_type: "image/png".to_string(),
                            artifact_id: artifact_id.map(str::to_string),
                            reference_key: reference_key.map(str::to_string),
                            metadata: ImageMetadata::default(),
                        },
                    }],
                },
            }];
            assert_eq!(
                needs_artifact_hydration(std::slice::from_ref(&message)),
                expected
            );
        }
    }

    #[test]
    fn image_support_fails_closed_unless_both_scopes_guarantee_it() {
        assert_eq!(
            image_support_from_capabilities(&provider_capabilities(true), &model_info(true)),
            Some((true, 5_242_880))
        );
        assert_eq!(
            image_support_from_capabilities(&provider_capabilities(false), &model_info(true)),
            Some((false, 5_242_880))
        );
        assert_eq!(
            image_support_from_capabilities(&provider_capabilities(true), &model_info(false)),
            Some((false, 5_242_880))
        );
    }

    #[test]
    fn artifact_identity_requires_typed_capability_fields() {
        let mut image = bcode_model::ImageRefContent {
            path: "/workspace/image.png".to_owned(),
            mime_type: "image/png".to_owned(),
            artifact_id: None,
            reference_key: None,
            metadata: bcode_model::ImageMetadata::default(),
        };
        assert!(artifact_identity_for_image_ref(&image).is_none());
        image.artifact_id = Some("artifact".to_owned());
        image.reference_key = Some("image".to_owned());
        assert_eq!(
            artifact_identity_for_image_ref(&image),
            Some(("artifact".to_owned(), "image".to_owned()))
        );
    }

    #[test]
    fn encoded_limit_is_enforced_after_base64_expansion() {
        assert!(encode_image_bytes(vec![0; 3], 4).is_ok());
        let error = encode_image_bytes(vec![0; 4], 4).expect_err("four raw bytes encode to eight");
        assert!(error.contains("exceeds 4 bytes"));
    }
}
