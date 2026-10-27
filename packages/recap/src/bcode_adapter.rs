//! Configured embedded SDK adapter, compiled by the standalone CLI package only.
//! Keeping this outside the recap library avoids a bundled-plugin dependency cycle.
use base64::Engine as _;
use bcode::{Bcode, MessageRole, ModelMessage, StructuredOutputOptions};
use bcode_model::{ContentBlock, ImageContent, ImageMetadata};
use bcode_recap::{
    model::{ModelError, ModelExecutor, ModelFuture, ModelRequest, ModelResponse},
    process::Cancellation,
};
use std::time::Duration;

/// Explicit standalone configuration; no ambient selection substitution.
#[derive(Clone, Debug)]
pub struct BcodeOptions {
    pub config_path: std::path::PathBuf,
    pub profile: Option<String>,
    pub context: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    /// Normalized provider output-token ceiling.
    pub max_output_tokens: u32,
    /// Deadline for one model composition operation.
    pub timeout: Duration,
}

/// Standalone inference using centrally configured selection and provider authentication.
pub struct BcodeExecutor {
    sdk: Bcode,
    parameters: bcode_model::ModelParameters,
    timeout: Duration,
}
impl BcodeExecutor {
    /// Load enabled provider plugins and resolve the configured selection, without a daemon.
    /// # Errors
    /// Returns a secret-safe unavailable error when configuration/discovery fails.
    pub async fn configured(options: BcodeOptions) -> Result<Self, ModelError> {
        if options.max_output_tokens == 0 || options.timeout.is_zero() {
            return Err(ModelError::Unavailable);
        }
        let parameters = bcode_model::ModelParameters {
            max_output_tokens: Some(options.max_output_tokens),
            ..bcode_model::ModelParameters::default()
        };
        let providers = [bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/openai-compatible-provider-plugin/bcode-plugin.toml"),
            bcode_openai_compatible_provider_plugin::static_plugin(),
        )];
        let sdk = Bcode::configured_embedded_from_file(
            options.config_path,
            options.profile.as_deref(),
            options.context.as_deref(),
            options.model.as_deref(),
            options.provider.as_deref(),
            &providers,
        )
        .await
        .map_err(|_| ModelError::Unavailable)?;
        Ok(Self {
            sdk,
            parameters,
            timeout: options.timeout,
        })
    }
}
impl ModelExecutor for BcodeExecutor {
    fn execute<'a>(
        &'a self,
        request: ModelRequest,
        cancellation: &'a Cancellation,
    ) -> ModelFuture<'a> {
        Box::pin(async move {
            if cancellation.is_cancelled() {
                return Err(ModelError::Cancelled);
            }
            let max_bytes = request.max_response_bytes;
            let mut content = vec![ContentBlock::Text {
                text: request.input_text,
            }];
            for image in request.images {
                content.push(ContentBlock::Text {
                    text: format!("Source asset: {}", image.asset_id),
                });
                content.push(ContentBlock::Image {
                    image: ImageContent {
                        mime_type: image.mime_type,
                        data_base64: base64::engine::general_purpose::STANDARD.encode(image.bytes),
                        metadata: ImageMetadata::default(),
                    },
                });
            }
            let agent = self
                .sdk
                .agent()
                .system(request.instructions)
                .parameters(self.parameters.clone())
                .timeout(self.timeout)
                .build();
            let result = {
                let token = bcode::CancellationToken::new();
                let operation = agent.generate_object_from_messages::<serde_json::Value>(
                    vec![ModelMessage {
                        role: MessageRole::User,
                        content,
                    }],
                    StructuredOutputOptions::json_schema(
                        "recap",
                        bcode_recap::model::response_schema(),
                    ),
                    token.clone(),
                );
                tokio::pin!(operation);
                loop {
                    tokio::select! {
                        result = &mut operation => break result.map_err(|_| ModelError::ExecutionFailed)?,
                        () = tokio::time::sleep(Duration::from_millis(25)) => {
                            if cancellation.is_cancelled() {
                                token.cancel();
                                let _ = operation.await;
                                return Err(ModelError::Cancelled);
                            }
                        }
                    }
                }
            };
            drop(agent);
            let encoded = serde_json::to_vec(&result).map_err(|_| ModelError::InvalidResponse)?;
            ModelResponse::from_json(&encoded, max_bytes)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn invalid_budgets_fail_before_configuration_or_provider_access() {
        for (max_output_tokens, timeout) in [(0, Duration::from_secs(1)), (1, Duration::ZERO)] {
            let options = BcodeOptions {
                config_path: "nonexistent-recap-test-config.toml".into(),
                profile: None,
                context: None,
                model: None,
                provider: None,
                max_output_tokens,
                timeout,
            };
            assert!(matches!(
                BcodeExecutor::configured(options).await,
                Err(ModelError::Unavailable)
            ));
        }
    }
}
