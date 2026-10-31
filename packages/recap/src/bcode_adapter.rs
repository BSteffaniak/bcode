//! Configured embedded SDK adapter, compiled by the standalone CLI package only.
//! Keeping this outside the recap library avoids a bundled-plugin dependency cycle.
use base64::Engine as _;
use bcode::{Bcode, MessageRole, ModelMessage, StructuredOutputOptions};
use bcode_model::{ContentBlock, ImageContent, ImageMetadata};
use bcode_recap::{
    analysis_contracts::analysis_response_schema,
    diagnostics::{CallOutcome, Measurement, Observer, RequestStage, TokenUsage},
    model::{
        AnalysisFuture, AnalysisRequest, AnalysisResponse, ModelError, ModelExecutor, ModelFuture,
        ModelRequest, ModelResponse,
    },
    process::Cancellation,
};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

static NEXT_CALL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Resolve through Bcode's configured selection and provider authentication.
pub use bcode_recap::model_configuration::ModelConfiguration as BcodeOptions;

fn configuration_fingerprint(options: &BcodeOptions) -> Result<Option<String>, ModelFailure> {
    crate::connections::Selection {
        config_path: (!options.config_path.as_os_str().is_empty())
            .then(|| options.config_path.clone()),
        profile: options.profile.clone(),
        context: options.context.clone(),
        model: options.model.clone(),
        provider: options.provider.clone(),
        endpoint: options.endpoint.clone(),
        auth_profile: options.auth_profile.clone(),
    }
    .options()
    .configuration_fingerprint()
    .map(Some)
    .map_err(|_| ModelFailure::Configuration)
}

fn retained_options(
    selected: &crate::connections::Selection,
    max_output_tokens: u32,
    timeout: Duration,
) -> Result<BcodeOptions, ModelFailure> {
    let mut resume_options = BcodeOptions {
        config_path: selected.config_path.clone().unwrap_or_default(),
        profile: selected.profile.clone(),
        context: selected.context.clone(),
        model: selected.model.clone(),
        provider: selected.provider.clone(),
        endpoint: selected.endpoint.clone(),
        auth_profile: selected.auth_profile.clone(),
        max_output_tokens,
        timeout,
    };
    if !resume_options.config_path.as_os_str().is_empty() {
        resume_options.config_path = std::fs::canonicalize(&resume_options.config_path)
            .map_err(|_| ModelFailure::Configuration)?;
    }
    Ok(resume_options)
}

#[cfg(test)]
#[path = "bcode_adapter/resume_tests.rs"]
mod resume_tests;

/// Standalone inference using centrally configured selection and provider authentication.
pub struct BcodeExecutor {
    sdk: Bcode,
    parameters: bcode_model::ModelParameters,
    timeout: Duration,
    failure: Mutex<Option<ModelFailure>>,
    selection: (String, String, Option<String>),
    observer: Option<Arc<dyn Observer>>,
    recovery: Option<String>,
    resume: Option<bcode_recap::model_configuration::ResumeModelConfiguration>,
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
    pub(crate) const fn classify(error: &bcode::BcodeError) -> Self {
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
    /// Report normalized selected identities without endpoint/authentication data.
    pub fn observe_selection(&self, cancellation: &Cancellation) {
        cancellation.emit(bcode_recap::diagnostics::Event::ModelSelection {
            provider: bcode_recap::diagnostics::identity(&self.selection.0),
            model: bcode_recap::diagnostics::identity(&self.selection.1),
            profile: self
                .selection
                .2
                .as_deref()
                .map(bcode_recap::diagnostics::identity),
        });
    }
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
    pub async fn configured(options: BcodeOptions) -> Result<Self, ModelFailure> {
        Self::configured_inner(options, true).await
    }

    /// Resolve saved settings without consulting current Recap preferences.
    /// # Errors
    /// Rejects unsupported settings, changed referenced files, and unavailable selections.
    pub async fn configured_resume(
        saved: bcode_recap::model_configuration::ResumeModelConfiguration,
        options: BcodeOptions,
        explicit_override: bool,
    ) -> Result<Self, ModelFailure> {
        if saved.version != 1 {
            return Err(ModelFailure::Configuration);
        }
        if !explicit_override
            && configuration_fingerprint(&saved.options)? != saved.config_fingerprint
        {
            return Err(ModelFailure::Configuration);
        }
        let expected_selection = options.model == saved.options.model
            && options.provider == saved.options.provider
            && options.profile == saved.options.profile
            && options.context == saved.options.context
            && options.config_path == saved.options.config_path;
        let mut executor = Self::configured_inner(options, false).await?;
        if !explicit_override
            && expected_selection
            && bcode_recap::generation::identity(&format!("{:?}", executor.selection))
                != saved.selection_fingerprint
        {
            return Err(ModelFailure::Configuration);
        }
        executor.recovery = Some(saved.identity.clone());
        if let Some(configuration) = &mut executor.resume {
            configuration.identity = saved.identity;
        }
        Ok(executor)
    }

    /// Recover legacy settings only when current preferences or explicit options
    /// reproduce the recorded identity exactly. Never guesses a historical model.
    /// # Errors
    /// Fails before inference when the old settings cannot be verified.
    pub async fn configured_legacy(
        options: BcodeOptions,
        expected: &str,
    ) -> Result<Self, ModelFailure> {
        let mut executor = Self::configured(options.clone()).await?;
        if executor.recovery.as_deref() != Some(expected) {
            let effective = executor
                .resume
                .as_ref()
                .ok_or(ModelFailure::Configuration)?
                .options
                .clone();
            let bytes_identity = if effective.config_path.as_os_str().is_empty() {
                "discovered".to_owned()
            } else {
                let bytes = std::fs::read(&effective.config_path)
                    .map_err(|_| ModelFailure::Configuration)?;
                bcode_recap::generation::identity(&String::from_utf8_lossy(&bytes))
            };
            let original =
                format!("{effective:?}").replacen("ModelConfiguration {", "BcodeOptions {", 1);
            let key = bcode_recap::generation::identity(&format!(
                "recap-bcode-v1:{original}:{bytes_identity}"
            ));
            let key = bcode_recap::generation::identity(&format!("{key}:{:?}", executor.selection));
            if key != expected {
                return Err(ModelFailure::Configuration);
            }
            executor.recovery = Some(key.clone());
            executor
                .resume
                .as_mut()
                .ok_or(ModelFailure::Configuration)?
                .identity = key;
        }
        Ok(executor)
    }

    async fn configured_inner(
        options: BcodeOptions,
        preferences: bool,
    ) -> Result<Self, ModelFailure> {
        if options.max_output_tokens == 0 || options.timeout.is_zero() {
            return Err(ModelFailure::Configuration);
        }
        let config_identity = if options.config_path.as_os_str().is_empty() {
            // Discovery selection is verified below; no ambient credentials are persisted.
            "discovered".into()
        } else {
            let bytes =
                std::fs::read(&options.config_path).map_err(|_| ModelFailure::Configuration)?;
            bcode_recap::generation::identity(&String::from_utf8_lossy(&bytes))
        };
        // Preserve the legacy request identity despite extracting its portable type.
        let legacy_options =
            format!("{options:?}").replacen("ModelConfiguration {", "BcodeOptions {", 1);
        let recovery = Some(bcode_recap::generation::identity(&format!(
            "recap-bcode-v1:{legacy_options}:{config_identity}"
        )));
        let parameters = bcode_model::ModelParameters {
            max_output_tokens: Some(options.max_output_tokens),
            ..bcode_model::ModelParameters::default()
        };
        let selection_options = crate::connections::Selection {
            config_path: (!options.config_path.as_os_str().is_empty())
                .then_some(options.config_path),
            profile: options.profile,
            context: options.context,
            model: options.model,
            provider: options.provider,
            endpoint: options.endpoint,
            auth_profile: options.auth_profile,
        };
        let selected = if preferences {
            crate::connections::effective_selection(&selection_options)
                .map_err(|_| ModelFailure::Configuration)?
        } else {
            selection_options
        };
        let resume_options =
            retained_options(&selected, options.max_output_tokens, options.timeout)?;
        let effective_profile = selected.profile.clone();
        let sdk = crate::connections::resolve(&selected)
            .await
            .map_err(|error| match error {
                crate::connections::ConnectionError::Model(failure) => failure,
                _ => ModelFailure::Configuration,
            })?;
        let selector = sdk
            .default_model_selector()
            .cloned()
            .ok_or(ModelFailure::Configuration)?;
        let owner = selector
            .provider_plugin_id()
            .ok_or(ModelFailure::Configuration)?
            .to_owned();
        let selection = (
            owner.clone(),
            selector.model_id().to_owned(),
            effective_profile,
        );
        let context = sdk.provider_context().clone();
        let sdk = tokio::time::timeout(
            options.timeout.min(Duration::from_secs(10)),
            sdk.discover_provider(owner, context, Some(selector.model_id().to_owned())),
        )
        .await
        .map_err(|_| ModelFailure::Timeout)?
        .map_err(|error| ModelFailure::classify(&error))?;
        let recovery =
            recovery.map(|key| bcode_recap::generation::identity(&format!("{key}:{selection:?}")));
        let resume = recovery
            .as_ref()
            .map(|identity| {
                Ok(bcode_recap::model_configuration::ResumeModelConfiguration {
                    version: 1,
                    identity: identity.clone(),
                    selection_fingerprint: bcode_recap::generation::identity(&format!(
                        "{selection:?}"
                    )),
                    config_fingerprint: configuration_fingerprint(&resume_options)?,
                    options: resume_options,
                })
            })
            .transpose()?;
        Ok(Self {
            sdk,
            parameters,
            timeout: options.timeout,
            failure: Mutex::new(None),
            selection: selection.clone(),
            observer: None,
            recovery,
            resume,
        })
    }
}
impl ModelExecutor for BcodeExecutor {
    fn recovery_identity(&self) -> Option<String> {
        self.recovery.clone()
    }
    fn resume_configuration(
        &self,
    ) -> Option<bcode_recap::model_configuration::ResumeModelConfiguration> {
        self.resume.clone()
    }

    fn execute<'a>(
        &'a self,
        request: ModelRequest,
        cancellation: &'a Cancellation,
    ) -> ModelFuture<'a> {
        Box::pin(async move {
            let max_bytes = request.max_response_bytes;
            let encoded = self
                .execute_structured(
                    request,
                    bcode_recap::model::response_schema(),
                    RequestStage::Composition,
                    cancellation,
                )
                .await?;
            ModelResponse::from_json(&encoded, max_bytes)
        })
    }

    fn execute_analysis<'a>(
        &'a self,
        request: AnalysisRequest,
        cancellation: &'a Cancellation,
    ) -> AnalysisFuture<'a> {
        Box::pin(async move {
            let stage = request.stage;
            let max_bytes = request.max_response_bytes;
            let request = ModelRequest {
                instructions: request.instructions,
                input_text: serde_json::to_string(&request.input)
                    .map_err(|_| ModelError::InvalidResponse)?,
                images: request.images,
                max_response_bytes: max_bytes,
            };
            let encoded = self
                .execute_structured(
                    request,
                    analysis_response_schema(stage),
                    request_stage(stage),
                    cancellation,
                )
                .await?;
            AnalysisResponse::from_json_slice(stage, &encoded, max_bytes)
        })
    }
}

impl BcodeExecutor {
    /// Attach an explicit run-scoped count-only observer; nothing is persisted implicitly.
    #[must_use]
    pub fn with_observer(mut self, observer: Arc<dyn Observer>) -> Self {
        self.observer = Some(observer);
        self
    }

    async fn execute_structured(
        &self,
        request: ModelRequest,
        schema: serde_json::Value,
        stage: RequestStage,
        cancellation: &Cancellation,
    ) -> Result<Vec<u8>, ModelError> {
        let mut timing = RequestTiming::new(self.observer.as_deref(), stage, &request);
        let result = self
            .execute_structured_inner(request, schema, cancellation, &mut timing)
            .await;
        timing.outcome = match &result {
            Ok(_) => CallOutcome::Succeeded,
            Err(ModelError::Cancelled) => CallOutcome::Cancelled,
            Err(_) => CallOutcome::Failed,
        };
        result
    }

    async fn execute_structured_inner(
        &self,
        request: ModelRequest,
        schema: serde_json::Value,
        cancellation: &Cancellation,
        timing: &mut RequestTiming<'_>,
    ) -> Result<Vec<u8>, ModelError> {
        *self
            .failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        if cancellation.is_cancelled() {
            return Err(ModelError::Cancelled);
        }
        let max_bytes = request.max_response_bytes;
        let input: serde_json::Value =
            serde_json::from_str(&request.input_text).unwrap_or(serde_json::Value::Null);
        let unit = input["source_unit"]["id"]
            .as_str()
            .filter(|id| {
                id.len() <= 128
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
            })
            .unwrap_or("");
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
        let call_id = NEXT_CALL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let live = Arc::new(LiveCall {
            id: call_id,
            cancel: cancellation.clone(),
            succeeded: std::sync::atomic::AtomicBool::new(false),
        });
        cancellation.activity(bcode_recap::activity::Activity::Started {
            id: call_id,
            label: format!("{:?} {unit}", timing.stage),
        });
        let agent = self
            .sdk
            .agent()
            // Each recap request is an independent turn. SDK runtime clones share one
            // active-turn owner, so reusing that owner would supersede concurrent calls.
            // The configured Recap SDK uses default runtime settings. Auth/selection,
            // agent permission policy and request cancellation remain on this builder.
            .runtime(bcode::AgentRuntime::default())
            .retry_policy(
                bcode::RetryPolicy::new(3, Duration::from_secs(1))
                    .with_max_delay(Duration::from_secs(30))
                    .with_jitter_millis(250)
                    .normalized_only(),
            )
            .invocation_event_sink(live.clone())
            .system(request.instructions)
            .parameters(self.parameters.clone())
            .timeout(self.timeout)
            .build();
        let result = {
            let token = bcode::CancellationToken::new();
            timing.invoked();
            let operation = agent.generate_object_from_messages::<serde_json::Value>(
                vec![ModelMessage {
                    role: MessageRole::User,
                    content,
                }],
                StructuredOutputOptions::json_schema("recap", schema),
                token.clone(),
            );
            tokio::pin!(operation);
            loop {
                tokio::select! {
                    result = &mut operation => {
                        timing.inference_finished();
                        break result.map_err(|error| self.execution_error(&error))?;
                    },
                    () = tokio::time::sleep(Duration::from_millis(25)) => {
                        if cancellation.is_cancelled() {
                            token.cancel();
                            let _ = operation.await;
                            timing.inference_finished();
                            return Err(ModelError::Cancelled);
                        }
                    }
                }
            }
        };
        drop(agent);
        let encoded = serde_json::to_vec(&result).map_err(|_| ModelError::InvalidResponse)?;
        if encoded.len() > max_bytes {
            return Err(ModelError::InvalidResponse);
        }
        live.succeeded
            .store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(encoded)
    }
}
#[derive(Debug)]
struct LiveCall {
    id: u64,
    cancel: Cancellation,
    succeeded: std::sync::atomic::AtomicBool,
}
impl bcode::TurnEventSink for LiveCall {
    fn emit(&self, event: bcode::ScopedTurnEvent) -> bool {
        let text = match event {
            bcode::ScopedTurnEvent::Runtime(
                bcode::AgentEvent::TextDelta(text)
                | bcode::AgentEvent::Output {
                    event: bcode_model::ProviderOutputEvent::TextDelta { text },
                    ..
                },
            ) => Some(text),
            _ => None,
        };
        if let Some(text) = text {
            self.cancel
                .activity(bcode_recap::activity::Activity::Text { id: self.id, text });
        }
        true
    }
}
impl Drop for LiveCall {
    fn drop(&mut self) {
        self.cancel
            .activity(bcode_recap::activity::Activity::Finished {
                id: self.id,
                succeeded: self.succeeded.load(std::sync::atomic::Ordering::Relaxed),
            });
    }
}

const fn request_stage(stage: bcode_recap::model::AnalysisStage) -> RequestStage {
    use bcode_recap::model::AnalysisStage;
    match stage {
        AnalysisStage::EditionPlan
        | AnalysisStage::VisualPlan
        | AnalysisStage::VisualReview
        | AnalysisStage::Participants
        | AnalysisStage::ExtractionInventory => RequestStage::Inventory,
        AnalysisStage::Outline => RequestStage::Outline,
        AnalysisStage::ChapterComposition | AnalysisStage::PassageComposition => {
            RequestStage::Chapter
        }
        AnalysisStage::SourceAudit | AnalysisStage::PassageReview => RequestStage::Audit,
        AnalysisStage::SourceRepair | AnalysisStage::PassagePatch => RequestStage::Repair,
        AnalysisStage::VisualInspection => RequestStage::VisualInspection,
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

// Drop reports interrupted futures too; it never retains request content or SDK payloads.
struct RequestTiming<'a> {
    observer: Option<&'a dyn Observer>,
    stage: RequestStage,
    started: Instant,
    inference_started: Option<Instant>,
    inference_ms: Option<u64>,
    outcome: CallOutcome,
}
impl<'a> RequestTiming<'a> {
    fn new(
        observer: Option<&'a dyn Observer>,
        stage: RequestStage,
        request: &ModelRequest,
    ) -> Self {
        if let Some(observer) = observer {
            let bytes = |value: usize| u64::try_from(value).unwrap_or(u64::MAX);
            observer.measure(Measurement::Request {
                stage,
                context_bytes: bytes(request.instructions.len())
                    .saturating_add(bytes(request.input_text.len())),
                image_bytes: request.images.iter().fold(0_u64, |sum, image| {
                    sum.saturating_add(bytes(image.bytes.len()))
                }),
                images: bytes(request.images.len()),
            });
        }
        Self {
            observer,
            stage,
            started: Instant::now(),
            inference_started: None,
            inference_ms: None,
            outcome: CallOutcome::Interrupted,
        }
    }
    fn invoked(&mut self) {
        self.inference_started = Some(Instant::now());
        if let Some(observer) = self.observer {
            observer.measure(Measurement::Invocation { stage: self.stage });
        }
    }
    fn inference_finished(&mut self) {
        self.inference_ms = self.inference_started.map(elapsed_ms);
    }
}
impl Drop for RequestTiming<'_> {
    fn drop(&mut self) {
        if let Some(observer) = self.observer {
            observer.measure(Measurement::Finished {
                stage: self.stage,
                request_ms: elapsed_ms(self.started),
                inference_ms: self
                    .inference_ms
                    .or_else(|| self.inference_started.map(elapsed_ms)),
                outcome: self.outcome,
                // The current structured SDK API exposes no usage. Never invent estimates.
                usage: TokenUsage::default(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    #[derive(Clone)]
    struct OverlappingProvider {
        started: std::sync::Arc<tokio::sync::Barrier>,
        next: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        release: std::sync::Arc<[tokio::sync::Notify; 3]>,
    }

    impl bcode::InProcessModelProvider for OverlappingProvider {
        fn run_turn(
            &self,
            request: bcode_model::ModelTurnRequest,
            context: bcode::InProcessProviderContext,
        ) -> bcode::InProcessProviderFuture<'_> {
            Box::pin(async move {
                assert_eq!(request.model_id, "test");
                assert_eq!(request.parameters.max_output_tokens, Some(37));
                assert_eq!(
                    request.provider_context.model_profile.as_deref(),
                    Some("offline-profile")
                );
                assert_eq!(
                    request.provider_context.auth_profile.as_deref(),
                    Some("offline-auth")
                );
                assert_eq!(
                    request
                        .provider_context
                        .settings
                        .get("test-setting")
                        .map(String::as_str),
                    Some("retained")
                );
                assert!(request.tools.is_empty());
                let index = self.next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                assert!(index < 3, "unexpected retry or additional invocation");
                self.started.wait().await;
                self.release[index].notified().await;
                assert!(!context.cancellation().is_cancelled());
                context
                    .events()
                    .emit(bcode_model::ProviderTurnEvent::TextDelta {
                        text: "{\"ok\":true}".into(),
                    })
                    .unwrap();
                Ok(bcode::InProcessProviderOutcome::EndTurn)
            })
        }
    }

    fn concurrent_test_registry() -> bcode::ProviderRegistry {
        let mut features = bcode_model::ModelFeatureSupport::default();
        features.structured_output.insert(
            bcode_model::StructuredOutputMode::StrictJsonSchema,
            bcode_model::CapabilitySupport::Supported {
                source: bcode_model::CapabilitySource::ProviderApi,
                mechanism: bcode_model::CapabilityMechanism::Native,
                fidelity: bcode_model::CapabilityFidelity::Exact,
                execution: bcode_model::CapabilityExecution::Direct,
            },
        );
        bcode::ProviderRegistry::new()
            .provider_capabilities(bcode_model::ProviderCapabilities {
                provider_id: "test".into(),
                display_name: "Offline test provider".into(),
                capabilities: std::collections::BTreeSet::new(),
                feature_support: features.clone(),
                auth_schemes: std::collections::BTreeSet::new(),
                retry_rules: Vec::new(),
                metadata: std::collections::BTreeMap::new(),
            })
            .provider_models(
                "test",
                bcode_model::ModelList {
                    models: vec![bcode_model::ModelInfo {
                        model_id: "test".into(),
                        display_name: "Offline test model".into(),
                        is_default: true,
                        context_window: None,
                        max_output_tokens: None,
                        max_image_input_base64_bytes: None,
                        api_surface: None,
                        capabilities: std::collections::BTreeSet::new(),
                        feature_support: features,
                        reasoning: None,
                        cache: bcode_model::ModelCacheInfo::default(),
                        metadata_source: None,
                        pricing: None,
                        visibility: bcode_model::ModelVisibility::default(),
                    }],
                    catalog: bcode_model::ModelCatalogHints::default(),
                },
            )
            .default_model(bcode::ModelSelector::with_provider("test", "test"))
    }

    #[tokio::test]
    async fn shared_executor_concurrent_calls_do_not_supersede_or_cancel_siblings() {
        use super::*;
        let started = Arc::new(tokio::sync::Barrier::new(4));
        let release = Arc::new(std::array::from_fn(|_| tokio::sync::Notify::new()));
        let provider = OverlappingProvider {
            started: started.clone(),
            next: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            release: release.clone(),
        };
        let sdk = Bcode::builder()
            .provider_factory(move || {
                Box::new(bcode::InProcessModelProviderAdapter::new(provider.clone()))
            })
            .provider_registry(concurrent_test_registry())
            .provider_context(bcode_model::ProviderRequestContext {
                model_profile: Some("offline-profile".into()),
                auth_profile: Some("offline-auth".into()),
                settings: [("test-setting".into(), "retained".into())].into(),
                ..bcode_model::ProviderRequestContext::default()
            })
            .build();
        let executor = Arc::new(BcodeExecutor {
            sdk,
            parameters: bcode_model::ModelParameters {
                max_output_tokens: Some(37),
                ..bcode_model::ModelParameters::default()
            },
            timeout: Duration::from_secs(5),
            failure: Mutex::new(None),
            selection: ("test".into(), "test".into(), None),
            observer: None,
            recovery: Some("test".into()),
            resume: None,
        });
        let cancellation = Cancellation::default();
        let mut calls = tokio::task::JoinSet::new();
        for _ in 0..3 {
            let executor = executor.clone();
            let cancellation = cancellation.clone();
            calls.spawn(async move {
                executor.execute_structured(
                    ModelRequest {
                        instructions: "Return the requested object".into(),
                        input_text: "test".into(),
                        images: Vec::new(),
                        max_response_bytes: 1024,
                    },
                    serde_json::json!({"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"],"additionalProperties":false}),
                    RequestStage::Inventory,
                    &cancellation,
                ).await
            });
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            // No sleeps: all three provider rounds must be active before any may complete.
            tokio::select! {
                _ = started.wait() => {}
                result = calls.join_next() => panic!("adapter finished before all provider rounds entered: {result:?}"),
            }
            release[0].notify_one();
            let first = calls.join_next().await.unwrap().unwrap().unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&first).unwrap(),
                serde_json::json!({"ok":true})
            );
            // The first agent and invoker have now dropped while two siblings remain active.
            assert!(!cancellation.is_cancelled());
            release[1].notify_one();
            release[2].notify_one();
            while let Some(result) = calls.join_next().await {
                let encoded = result.unwrap().unwrap();
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(&encoded).unwrap(),
                    serde_json::json!({"ok":true})
                );
            }
        })
        .await
        .expect("concurrent independent calls must finish without cancellation or deadlock");
        assert!(!cancellation.is_cancelled());
        assert_eq!(executor.failure(), None);
    }
    #[test]
    fn count_only_timing_reports_invocation_and_interrupted_drop() {
        use bcode_recap::diagnostics::{Operation, RequestStage};
        let operation = Operation::new("adapter-test".into());
        let request = bcode_recap::model::ModelRequest {
            instructions: "PRIVATE".into(),
            input_text: "秘密".into(),
            images: vec![bcode_recap::model::ImageAsset {
                asset_id: "private-asset".into(),
                mime_type: "image/png".into(),
                bytes: vec![1, 2, 3],
            }],
            max_response_bytes: 10,
        };
        {
            let mut timing =
                super::RequestTiming::new(Some(&operation), RequestStage::Inventory, &request);
            timing.invoked();
            timing.inference_finished();
        }
        let measurements = operation.measurements();
        let counts = &measurements.stages[&RequestStage::Inventory];
        assert_eq!(counts.requests, 1);
        assert_eq!(counts.invocations, 1);
        assert_eq!(counts.interrupted, 1);
        assert_eq!(counts.context_bytes, 13);
        assert_eq!(counts.image_bytes, 3);
        assert_eq!(counts.images, 1);
        assert_eq!(counts.inference_timing_samples, 1);
        assert_eq!(counts.usage.input_tokens, None);
        let json = serde_json::to_string(&measurements).unwrap();
        assert!(!json.contains("PRIVATE"));
        assert!(!json.contains("秘密"));
        assert!(!json.contains("private-asset"));
    }

    #[test]
    fn pre_invocation_failure_does_not_claim_inference() {
        use bcode_recap::diagnostics::{CallOutcome, Operation, RequestStage};
        let operation = Operation::new("adapter-test".into());
        let request = bcode_recap::model::ModelRequest {
            instructions: String::new(),
            input_text: String::new(),
            images: Vec::new(),
            max_response_bytes: 10,
        };
        {
            let mut timing =
                super::RequestTiming::new(Some(&operation), RequestStage::Composition, &request);
            timing.outcome = CallOutcome::Cancelled;
        }
        let measurements = operation.measurements();
        let counts = &measurements.stages[&RequestStage::Composition];
        assert_eq!(counts.requests, 1);
        assert_eq!(counts.invocations, 0);
        assert_eq!(counts.cancelled, 1);
        assert_eq!(counts.inference_timing_samples, 0);
    }
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
                endpoint: None,
                auth_profile: None,
                max_output_tokens,
                timeout,
            };
            assert!(matches!(
                BcodeExecutor::configured(options).await,
                Err(ModelFailure::Configuration)
            ));
        }
    }
}
