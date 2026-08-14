//! Request-only hydration of artifact-backed tool-result images.

use super::{
    ContentBlock, MAX_ARTIFACT_RANGE_BYTES, ModelTurnRequest, ResponsePayload, ServerState,
    SessionId, read_session_artifact_range, resolved_provider_models, select_model_info,
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
                match read_image_artifact(
                    state,
                    session_id,
                    &artifact_id,
                    &reference_key,
                    encoded_limit,
                )
                .await
                {
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

pub async fn read_image_artifact(
    state: &ServerState,
    session_id: SessionId,
    artifact_id: &str,
    reference_key: &str,
    encoded_limit: u64,
) -> Result<String, String> {
    let raw_limit = encoded_limit.saturating_mul(3) / 4;
    let mut offset = 0_u64;
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
        let ResponsePayload::SessionArtifactRange {
            total_bytes,
            bytes: chunk,
            ..
        } = response
        else {
            return Err("artifact reader returned an unexpected response".to_owned());
        };
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
    Ok(encoded)
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
