//! Configured embedded SDK adapter, compiled by the standalone CLI package only.
//! Keeping this outside the recap library avoids a bundled-plugin dependency cycle.
use base64::Engine as _;
use bcode::{Bcode, MessageRole, ModelMessage, StructuredOutputOptions};
use bcode_model::{ContentBlock, ImageContent, ImageMetadata};
use bcode_recap::{
    model::{ModelError, ModelExecutor, ModelFuture, ModelRequest, ModelResponse},
    process::Cancellation,
};
use std::{sync::Mutex, time::Duration};

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
    failure: Mutex<Option<ModelFailure>>,
}
/// Allowlisted SDK failure category; never contains provider text or response content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelFailure {
    Timeout,
    Cancelled,
    ProviderInvocation,
    ProviderAuth,
    ProviderNetwork,
    ProviderInvalidRequest,
    ProviderContextLength,
    ProviderModelNotFound,
    ProviderUnsupported,
    ProviderInternal,
    ProviderOverloaded,
    ProviderExecutionUnverified,
    ProviderAfterOutput,
    RateLimited,
    Configuration,
    StructuredJson,
    StructuredSchema,
    StructuredDecode,
    StructuredRepair,
    Runtime,
    Sdk,
}
impl ModelFailure {
    /// Stable public diagnostic code, independent of raw SDK display/debug output.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Timeout => "model_timeout",
            Self::Cancelled => "model_cancelled",
            Self::ProviderInvocation => "provider_invocation",
            Self::ProviderAuth => "provider_auth",
            Self::ProviderNetwork => "provider_network",
            Self::ProviderInvalidRequest => "provider_invalid_request",
            Self::ProviderContextLength => "provider_context_length",
            Self::ProviderModelNotFound => "provider_model_not_found",
            Self::ProviderUnsupported => "provider_unsupported_feature",
            Self::ProviderInternal => "provider_internal",
            Self::ProviderOverloaded => "provider_overloaded",
            Self::ProviderExecutionUnverified => "provider_execution_unverified",
            Self::ProviderAfterOutput => "provider_after_output",
            Self::RateLimited => "rate_limited",
            Self::Configuration => "model_configuration",
            Self::StructuredJson => "structured_json",
            Self::StructuredSchema => "structured_schema",
            Self::StructuredDecode => "structured_decode",
            Self::StructuredRepair => "structured_repair_exhausted",
            Self::Runtime => "model_runtime",
            Self::Sdk => "model_sdk",
        }
    }
    const fn classify(error: &bcode::BcodeError) -> Self {
        use bcode::{BcodeError, RuntimeError};
        match error {
            BcodeError::Runtime(error) => match error {
                RuntimeError::Timeout { .. } => Self::Timeout,
                RuntimeError::Cancelled => Self::Cancelled,
                RuntimeError::ProviderInvocation(_) => Self::ProviderInvocation,
                RuntimeError::Provider { error, .. } => {
                    use bcode_model::ProviderErrorCategory;
                    match error.category {
                        ProviderErrorCategory::Config => Self::Configuration,
                        ProviderErrorCategory::Auth => Self::ProviderAuth,
                        ProviderErrorCategory::RateLimit => Self::RateLimited,
                        ProviderErrorCategory::Network => Self::ProviderNetwork,
                        ProviderErrorCategory::Timeout => Self::Timeout,
                        ProviderErrorCategory::ModelNotFound => Self::ProviderModelNotFound,
                        ProviderErrorCategory::ContextLength => Self::ProviderContextLength,
                        ProviderErrorCategory::InvalidRequest => Self::ProviderInvalidRequest,
                        ProviderErrorCategory::UnsupportedFeature => Self::ProviderUnsupported,
                        ProviderErrorCategory::ProviderInternal => Self::ProviderInternal,
                        ProviderErrorCategory::Overloaded => Self::ProviderOverloaded,
                        ProviderErrorCategory::Cancelled => Self::Cancelled,
                    }
                }
                RuntimeError::ProviderExecutionUnverified(_) => Self::ProviderExecutionUnverified,
                RuntimeError::ProviderAfterOutput(_) => Self::ProviderAfterOutput,
                _ => Self::Runtime,
            },
            BcodeError::MissingProvider
            | BcodeError::MissingPluginRuntime
            | BcodeError::ProviderConfiguration(_)
            | BcodeError::Config(_)
            | BcodeError::Plugin(_) => Self::Configuration,
            BcodeError::RateLimited { .. } => Self::RateLimited,
            BcodeError::StructuredInvalidJson(_) => Self::StructuredJson,
            BcodeError::StructuredInvalidSchema(_) | BcodeError::StructuredSchemaValidation(_) => {
                Self::StructuredSchema
            }
            BcodeError::StructuredDecode(_) => Self::StructuredDecode,
            BcodeError::StructuredRepairExhausted(_) => Self::StructuredRepair,
            _ => Self::Sdk,
        }
    }
}
impl BcodeExecutor {
    /// Last failed invocation's bounded diagnostic category, without raw SDK data.
    #[must_use]
    pub fn failure(&self) -> Option<ModelFailure> {
        *self
            .failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn execution_error(&self, error: &bcode::BcodeError) -> ModelError {
        let failure = ModelFailure::classify(error);
        *self
            .failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(failure);
        match failure {
            ModelFailure::Cancelled => ModelError::Cancelled,
            ModelFailure::Configuration => ModelError::Unavailable,
            ModelFailure::StructuredJson
            | ModelFailure::StructuredSchema
            | ModelFailure::StructuredDecode => ModelError::InvalidResponse,
            _ => ModelError::ExecutionFailed,
        }
    }

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
            failure: Mutex::new(None),
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
            *self
                .failure
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
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
                        result = &mut operation => break result.map_err(|error| self.execution_error(&error))?,
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

    #[test]
    fn sdk_diagnostics_discard_untrusted_error_payloads() {
        use bcode::{BcodeError, RuntimeError};
        let secret = "credential-do-not-display";
        for (error, expected) in [
            (
                BcodeError::Runtime(RuntimeError::ProviderInvocation(secret.into())),
                ModelFailure::ProviderInvocation,
            ),
            (
                BcodeError::StructuredInvalidJson(secret.into()),
                ModelFailure::StructuredJson,
            ),
            (
                BcodeError::StructuredRepairExhausted(secret.into()),
                ModelFailure::StructuredRepair,
            ),
            (
                BcodeError::Runtime(RuntimeError::Timeout {
                    timeout: Duration::from_secs(1),
                }),
                ModelFailure::Timeout,
            ),
        ] {
            let category = ModelFailure::classify(&error);
            assert_eq!(category, expected);
            assert!(!category.code().contains(secret));
            assert!(!format!("{category:?}").contains(secret));
        }
    }

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
