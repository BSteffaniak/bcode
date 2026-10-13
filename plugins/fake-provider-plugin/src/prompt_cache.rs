//! Cache-simulating fake models.
//!
//! `fake-cache-explicit` and `fake-cache-prefix` back their usage reports with
//! [`bcode_prompt_cache::simulation::PromptCacheSimulator`] so host planning, provider
//! conformance, and eval telemetry can be verified against a deterministic reference cache
//! without credentials. `fake-echo` keeps rejecting cache hints so callers that must not send
//! them still fail fast.

use bcode_model::{
    CapabilitySource, CapabilitySupport, ContentBlock, MessageRole, ModelCapability,
    ModelFeatureSupport, ModelInfo, ModelMessage, ModelTurnRequest, PromptCacheFeature,
    ProviderError, ProviderTurnEvent, StopReason, ToolCall, ToolChoice,
};
use bcode_prompt_cache::PromptCacheMechanism;
use bcode_prompt_cache::simulation::{
    PromptCacheSimulator, PromptCacheSimulatorProfile, SimulatedCacheRound,
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Explicit-breakpoint cache model that reports cache writes (Anthropic-style).
pub const FAKE_CACHE_EXPLICIT_MODEL_ID: &str = "fake-cache-explicit";
/// Automatic-prefix cache model that reports reads only (OpenAI-style).
pub const FAKE_CACHE_PREFIX_MODEL_ID: &str = "fake-cache-prefix";

/// Minimum simulated prefix tokens before the fake cache stores an entry.
///
/// Small enough that conformance workloads stay cheap, large enough that a bare user message
/// does not qualify, so "too short to cache" is exercised.
pub const FAKE_CACHE_MIN_PREFIX_TOKENS: u64 = 64;

/// Explicit breakpoint budget (system + tools + messages) for the explicit fake model.
pub const FAKE_CACHE_MAX_CACHE_POINTS: usize = 4;

/// Whether a model id is served by the cache simulator.
#[must_use]
pub fn is_cache_model(model_id: &str) -> bool {
    matches!(
        model_id,
        FAKE_CACHE_EXPLICIT_MODEL_ID | FAKE_CACHE_PREFIX_MODEL_ID
    )
}

/// Simulator profile for a cache model id, if it is one.
#[must_use]
pub fn profile_for(model_id: &str) -> Option<PromptCacheSimulatorProfile> {
    match model_id {
        FAKE_CACHE_EXPLICIT_MODEL_ID => Some(PromptCacheSimulatorProfile {
            mechanism: PromptCacheMechanism::ExplicitPoints,
            reports_cache_writes: true,
            ttl_seconds: BTreeSet::from([300, 3_600]),
            min_prefix_tokens: FAKE_CACHE_MIN_PREFIX_TOKENS,
            max_cache_points: FAKE_CACHE_MAX_CACHE_POINTS,
            provider_id: "bcode.fake-provider".to_string(),
            api_shape: "fake_cache_explicit".to_string(),
        }),
        FAKE_CACHE_PREFIX_MODEL_ID => Some(PromptCacheSimulatorProfile {
            mechanism: PromptCacheMechanism::AutomaticPrefix,
            reports_cache_writes: false,
            ttl_seconds: BTreeSet::new(),
            min_prefix_tokens: FAKE_CACHE_MIN_PREFIX_TOKENS,
            max_cache_points: 0,
            provider_id: "bcode.fake-provider".to_string(),
            api_shape: "fake_cache_prefix".to_string(),
        }),
        _ => None,
    }
}

/// Model listings for the cache models.
#[must_use]
pub fn models() -> Vec<ModelInfo> {
    [FAKE_CACHE_EXPLICIT_MODEL_ID, FAKE_CACHE_PREFIX_MODEL_ID]
        .into_iter()
        .filter_map(|model_id| profile_for(model_id).map(|profile| (model_id, profile)))
        .map(|(model_id, profile)| ModelInfo {
            model_id: model_id.to_string(),
            display_name: match profile.mechanism {
                PromptCacheMechanism::ExplicitPoints => "Fake Cache (explicit points)",
                PromptCacheMechanism::AutomaticPrefix => "Fake Cache (automatic prefix)",
            }
            .to_string(),
            is_default: false,
            context_window: Some(64_000),
            max_output_tokens: Some(1_000),
            max_image_input_base64_bytes: None,
            capabilities: [
                ModelCapability::StreamingText,
                ModelCapability::ToolCalls,
                ModelCapability::PromptCaching,
            ]
            .into_iter()
            .collect(),
            feature_support: feature_support(&profile),
            reasoning: None,
            cache: profile.cache_info(),
            metadata_source: Some(bcode_model::ModelMetadataSource::BundledCatalog),
            pricing: None,
            api_surface: None,
            visibility: bcode_model::ModelVisibility::Visible,
        })
        .collect()
}

/// Prompt-cache feature claims matching a simulator profile.
#[must_use]
pub fn feature_support(profile: &PromptCacheSimulatorProfile) -> ModelFeatureSupport {
    let supported = || CapabilitySupport::supported(CapabilitySource::TestContract);
    let unsupported = |reason: &str| CapabilitySupport::Unsupported {
        source: CapabilitySource::TestContract,
        reason: reason.to_string(),
    };
    let explicit = profile.mechanism == PromptCacheMechanism::ExplicitPoints;
    let mut support =
        super::fake_feature_support_for_execution(bcode_model::CapabilityExecution::Direct);
    support.prompt_cache = [
        (PromptCacheFeature::ConversationPrefix, true),
        (PromptCacheFeature::ExplicitSystem, explicit),
        (PromptCacheFeature::ExplicitTools, explicit),
        (PromptCacheFeature::ExplicitMessage, explicit),
        (
            PromptCacheFeature::Ttl,
            explicit && !profile.ttl_seconds.is_empty(),
        ),
    ]
    .into_iter()
    .map(|(feature, is_supported)| {
        (
            feature,
            if is_supported {
                supported()
            } else {
                unsupported("fake cache model does not implement this cache feature")
            },
        )
    })
    .collect();
    support
}

/// Provider-side cache feature claims: the union of what any fake cache model can do.
#[must_use]
pub fn provider_feature_claims() -> Vec<(PromptCacheFeature, CapabilitySupport)> {
    let supported = CapabilitySupport::supported(CapabilitySource::TestContract);
    [
        PromptCacheFeature::ConversationPrefix,
        PromptCacheFeature::ExplicitSystem,
        PromptCacheFeature::ExplicitTools,
        PromptCacheFeature::ExplicitMessage,
        PromptCacheFeature::Ttl,
    ]
    .into_iter()
    .map(|feature| (feature, supported.clone()))
    .collect()
}

/// Plugin-instance-owned simulator. Durable rounds reload under exclusive ownership so
/// different plugin instances cannot overwrite each other's updates.
#[derive(Default)]
pub struct CacheStore {
    memory: Mutex<PromptCacheSimulator>,
}

const SIMULATOR_STATE_FILE: &str = "fake-provider-prompt-cache.json";

// Explicit request-scoped fake-provider diagnostics, never response/session metadata.
const TRACE_ENV: &str = "FAKE_PROMPT_CACHE_TRACE_DIR";
const TRACE_LIMIT: usize = 32;

fn trace_prefixes(
    request: &ModelTurnRequest,
    usage: &bcode_model::TokenUsage,
) -> Result<(), ProviderError> {
    use sha2::{Digest, Sha256};
    use std::io::Write;

    let Some(directory) = request.provider_context.env.get(TRACE_ENV) else {
        return Ok(());
    };
    let directory = Path::new(directory);
    if !directory.is_absolute()
        || !std::fs::symlink_metadata(directory).is_ok_and(|metadata| metadata.file_type().is_dir())
    {
        return Err(state_error());
    }
    let root = directory.canonicalize().map_err(|_| state_error())?;
    let mut file = None;
    for index in 0..TRACE_LIMIT {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(root.join(format!("fake-cache-prefix-{index:02}.json"))) {
            Ok(created) => {
                file = Some(created);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return Err(state_error()),
        }
    }
    let Some(mut file) = file else {
        return Ok(());
    };
    let mut hasher = Sha256::new();
    let mut tokens = 0_u64;
    let mut prefixes = Vec::new();
    let mut add = |value: &serde_json::Value,
                   index: Option<usize>,
                   point: bool,
                   retain: bool|
     -> Result<(), ProviderError> {
        let bytes = serde_json::to_vec(value).map_err(|_| state_error())?;
        hasher.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_le_bytes());
        hasher.update(&bytes);
        tokens = tokens.saturating_add(
            u64::try_from(String::from_utf8_lossy(&bytes).split_whitespace().count())
                .unwrap_or(u64::MAX),
        );
        if retain {
            prefixes.push(serde_json::json!({"message_index": index, "sha256": hex::encode(hasher.clone().finalize()), "simulated_prefix_tokens": tokens, "ends_with_cache_point": point}));
        }
        Ok(())
    };
    add(
        &serde_json::to_value((&request.system_prompt, &request.tools))
            .map_err(|_| state_error())?,
        None,
        false,
        true,
    )?;
    for (index, message) in request.messages.iter().enumerate() {
        let content = message
            .content
            .iter()
            .filter(|block| !matches!(block, ContentBlock::CachePoint { .. }))
            .collect::<Vec<_>>();
        add(
            &serde_json::to_value((&message.role, content)).map_err(|_| state_error())?,
            Some(index),
            matches!(
                message.content.last(),
                Some(ContentBlock::CachePoint { .. })
            ),
            index < TRACE_LIMIT - 2 || index + 1 == request.messages.len(),
        )?;
    }
    let record = serde_json::json!({
        "version": 1,
        "message_count": request.messages.len(),
        "prefixes_truncated": request.messages.len() + 1 > TRACE_LIMIT,
        "prefixes": prefixes,
        "cache_system_prompt": request.prompt_cache.cache_system_prompt,
        "cache_tools": request.prompt_cache.cache_tools,
        "input_tokens": usage.input_tokens,
        "cached_input_tokens": usage.cached_input_tokens,
        "cache_write_input_tokens": usage.cache_write_input_tokens,
        "request_input_tokens": usage.pricing_context.request_input_tokens,
    });
    let bytes = serde_json::to_vec(&record).map_err(|_| state_error())?;
    file.write_all(&bytes).map_err(|_| state_error())
}

fn state_error() -> ProviderError {
    ProviderError {
        code: "fake_cache_state_unavailable".into(),
        category: bcode_model::ProviderErrorCategory::ProviderInternal,
        message: "fake cache state is unavailable, unsafe, or requires maintenance".into(),
        retryable: false,
        provider_message: None,
        failure: None,
        request_id: None,
        diagnostic_context: Box::default(),
        sources: Box::default(),
        retry: None,
    }
}

fn confined_file(root: &Path, name: &str) -> Result<PathBuf, ProviderError> {
    let path = root.join(name);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(path),
        Ok(_) | Err(_) => Err(state_error()),
    }
}

impl CacheStore {
    fn serve(
        &self,
        root: Option<&Path>,
        profile: &PromptCacheSimulatorProfile,
        request: &ModelTurnRequest,
        output_tokens: u32,
    ) -> Result<SimulatedCacheRound, ProviderError> {
        let Some(root) = root else {
            return self
                .memory
                .lock()
                .map_err(|_| state_error())
                .map(|mut simulator| simulator.serve(profile, request, output_tokens));
        };
        if !root.is_absolute() {
            return Err(state_error());
        }
        // Normalize the host-authorized root, including platform aliases such as /var.
        let mut ancestor = root;
        let mut missing = Vec::new();
        let mut root = loop {
            match std::fs::canonicalize(ancestor) {
                Ok(path) => break path,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    missing.push(ancestor.file_name().ok_or_else(state_error)?.to_owned());
                    ancestor = ancestor.parent().ok_or_else(state_error)?;
                }
                Err(_) => return Err(state_error()),
            }
        };
        for component in missing.iter().rev() {
            root.push(component);
        }
        std::fs::create_dir_all(&root).map_err(|_| state_error())?;
        let root = std::fs::canonicalize(root).map_err(|_| state_error())?;
        let lock_path = confined_file(&root, "fake-provider-prompt-cache.lock")?;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .map_err(|_| state_error())?;
        lock.try_lock().map_err(|_| state_error())?;
        let path = confined_file(&root, SIMULATOR_STATE_FILE)?;
        let mut simulator: PromptCacheSimulator = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| state_error())?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                PromptCacheSimulator::default()
            }
            Err(_) => return Err(state_error()),
        };
        let round = simulator.serve(profile, request, output_tokens);
        let bytes = serde_json::to_vec(&simulator).map_err(|_| state_error())?;
        let mut temporary = tempfile::NamedTempFile::new_in(&root).map_err(|_| state_error())?;
        std::io::Write::write_all(&mut temporary, &bytes).map_err(|_| state_error())?;
        temporary.persist(&path).map_err(|_| state_error())?;
        drop(lock);
        Ok(round)
    }
}

/// Validate cache hints for a cache model without mutating state.
///
/// # Errors
///
/// Returns the simulator's `UnsupportedFeature` error for unadvertised TTLs or misplaced cache
/// points.
pub fn validate(
    profile: &PromptCacheSimulatorProfile,
    request: &ModelTurnRequest,
) -> Result<(), ProviderError> {
    PromptCacheSimulator::validate(profile, request)
}

/// Serve one turn through the simulator, pushing the complete event stream onto `turn`.
///
/// The response is deterministic. A user prompt beginning with `read-files` followed by
/// whitespace-separated paths drives a real tool loop: each round calls the first offered tool
/// (normally `filesystem.read`) on the next unread path until every path has a tool result, then
/// replies with text. Otherwise a required/named tool choice produces one call, an `Auto` prompt
/// mentioning `probe` produces a `cache_probe` style call, and everything else is a short text
/// reply. Usage always comes from the simulator so cache accounting reflects the real prefix.
pub fn serve_turn(
    profile: &PromptCacheSimulatorProfile,
    request: &ModelTurnRequest,
    push: &dyn Fn(ProviderTurnEvent),
    store: &CacheStore,
    root: Option<&Path>,
) {
    let user_text = last_user_text(&request.messages);
    let completed_calls = request
        .messages
        .iter()
        .rev()
        .take_while(|message| message.role != MessageRole::User)
        .flat_map(|message| &message.content)
        .filter(|block| matches!(block, ContentBlock::ToolCall { .. }))
        .count();
    let tool_call = if request.tools.is_empty()
        || matches!(request.tool_call_policy.choice, ToolChoice::None)
    {
        None
    } else if let Some(paths) = user_text.strip_prefix("read-files") {
        paths
            .split_whitespace()
            .nth(completed_calls)
            .map(|path| ToolCall {
                id: format!("fake-cache-read-{}-{completed_calls}", request.turn_id),
                name: request.tools[0].name.clone(),
                arguments: serde_json::json!({ "path": path }),
            })
    } else {
        let last_role = request.messages.last().map(|message| message.role);
        let wants_tool_call = last_role == Some(MessageRole::User)
            && match &request.tool_call_policy.choice {
                ToolChoice::None => false,
                ToolChoice::Required | ToolChoice::Tool { .. } => true,
                ToolChoice::Auto => user_text.contains("probe"),
            };
        wants_tool_call.then(|| ToolCall {
            id: format!("fake-cache-probe-{completed_calls}"),
            name: match &request.tool_call_policy.choice {
                ToolChoice::Tool { name } => name.clone(),
                _ => request.tools[0].name.clone(),
            },
            arguments: serde_json::json!({"index": completed_calls}),
        })
    };
    let text = if tool_call.is_some() {
        String::new()
    } else {
        format!("fake cache reply: {user_text}")
    };
    let output_tokens = u32::try_from(text.split_whitespace().count().max(1)).unwrap_or(u32::MAX);
    let SimulatedCacheRound { usage, projection } =
        match store.serve(root, profile, request, output_tokens) {
            Ok(round) => round,
            Err(error) => {
                push(ProviderTurnEvent::Error { error });
                push(ProviderTurnEvent::TurnFinished {
                    stop_reason: StopReason::Error,
                });
                return;
            }
        };
    if let Err(error) = trace_prefixes(request, &usage) {
        push(ProviderTurnEvent::Error { error });
        push(ProviderTurnEvent::TurnFinished {
            stop_reason: StopReason::Error,
        });
        return;
    }
    push(ProviderTurnEvent::RequestProjection { projection });
    if let Some(call) = tool_call {
        push(ProviderTurnEvent::ToolCallStarted {
            call_id: call.id.clone(),
            name: call.name.clone(),
        });
        push(ProviderTurnEvent::ToolCallFinished { call });
        push(ProviderTurnEvent::Usage { usage });
        push(ProviderTurnEvent::TurnFinished {
            stop_reason: StopReason::ToolCall,
        });
    } else {
        push(ProviderTurnEvent::TextDelta { text });
        push(ProviderTurnEvent::Usage { usage });
        push(ProviderTurnEvent::TurnFinished {
            stop_reason: StopReason::EndTurn,
        });
    }
}

fn last_user_text(messages: &[ModelMessage]) -> String {
    messages
        .iter()
        .rev()
        .find(|message| message.role == MessageRole::User)
        .map(|message| {
            message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> ModelTurnRequest {
        ModelTurnRequest {
            context_lineage: None,
            session_id: bcode_session_models::SessionId::new(),
            turn_id: "cache-test".into(),
            model_id: FAKE_CACHE_PREFIX_MODEL_ID.into(),
            provider_context: bcode_model::ProviderRequestContext::default(),
            system_prompt: Some("stable prefix ".repeat(100)),
            messages: Vec::new(),
            tools: Vec::new(),
            tool_call_policy: bcode_model::ToolCallRequestPolicy::default(),
            tool_schema_mode: None,
            parameters: bcode_model::ModelParameters::default(),
            structured_output: None,
            context_management: bcode_model::ContextManagementRequest::default(),
            prompt_cache: bcode_model::PromptCacheHints {
                mode: bcode_model::PromptCacheMode::Auto,
                key: Some("same-key".into()),
                ..Default::default()
            },
            conversation_reuse: bcode_model::ConversationReuseHints::default(),
            metadata: std::collections::BTreeMap::default(),
        }
    }

    #[test]
    fn read_files_restarts_for_new_user_turn_without_reusing_call_identity() {
        let mut request = request();
        request.tools.push(bcode_model::ToolDefinition {
            name: "filesystem.read".into(),
            description: "read".into(),
            input_schema: serde_json::json!({"type":"object"}),
        });
        let user = || bcode_model::ModelMessage {
            role: MessageRole::User,
            content: vec![ContentBlock::Text {
                text: "read-files first.txt second.txt".into(),
            }],
        };
        request.messages.push(user());
        let profile = profile_for(FAKE_CACHE_PREFIX_MODEL_ID).unwrap();
        let store = CacheStore::default();
        let next_call = |request: &ModelTurnRequest| {
            let events = std::sync::Mutex::new(Vec::new());
            serve_turn(
                &profile,
                request,
                &|event| events.lock().unwrap().push(event),
                &store,
                None,
            );
            events
                .into_inner()
                .unwrap()
                .into_iter()
                .find_map(|event| match event {
                    ProviderTurnEvent::ToolCallFinished { call } => Some(call),
                    _ => None,
                })
                .expect("tool call")
        };
        let first = next_call(&request);
        request.messages.push(bcode_model::ModelMessage {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::ToolCall {
                call: first.clone(),
            }],
        });
        assert_eq!(next_call(&request).arguments["path"], "second.txt");
        request.turn_id = "next-user-turn".into();
        request.messages.push(user());
        let next = next_call(&request);
        assert_eq!(next.arguments["path"], "first.txt");
        assert_ne!(first.id, next.id);
    }

    #[test]
    fn cache_roots_and_memory_instances_are_isolated_and_restart_is_warm() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let first = root.join("first");
        let second = root.join("second");
        let request = request();
        let profile = profile_for(FAKE_CACHE_PREFIX_MODEL_ID).unwrap();
        let store = CacheStore::default();
        let cold = store.serve(Some(&first), &profile, &request, 1).unwrap();
        let warm = CacheStore::default()
            .serve(Some(&first), &profile, &request, 1)
            .unwrap();
        assert_ne!(cold.usage, warm.usage);
        assert_eq!(
            cold.usage,
            store
                .serve(Some(&second), &profile, &request, 1)
                .unwrap()
                .usage
        );
        assert_eq!(
            cold.usage,
            store.serve(None, &profile, &request, 1).unwrap().usage
        );
        assert_eq!(
            warm.usage,
            store.serve(None, &profile, &request, 1).unwrap().usage
        );
        assert_eq!(
            cold.usage,
            CacheStore::default()
                .serve(None, &profile, &request, 1)
                .unwrap()
                .usage
        );
    }

    #[test]
    fn explicit_conversation_prefix_survives_reconstruction_and_store_restart() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let profile = profile_for(FAKE_CACHE_EXPLICIT_MODEL_ID).unwrap();
        let mut original = request();
        original.model_id = FAKE_CACHE_EXPLICIT_MODEL_ID.into();
        original.messages = vec![ModelMessage {
            role: MessageRole::User,
            content: vec![ContentBlock::Text {
                text: "synthetic older context ".repeat(400),
            }],
        }];
        original.messages[0].content.push(ContentBlock::CachePoint {
            hint: bcode_model::PromptCachePoint {
                ttl_seconds: Some(300),
                label: None,
            },
        });
        let cold = CacheStore::default()
            .serve(Some(&root), &profile, &original, 1)
            .unwrap();
        let encoded = serde_json::to_vec(&original).unwrap();
        let mut resumed: ModelTurnRequest = serde_json::from_slice(&encoded).unwrap();
        resumed.turn_id = "resumed-user-turn".into();
        resumed.messages.push(ModelMessage {
            role: MessageRole::User,
            content: vec![ContentBlock::Text {
                text: "continue synthetic history".into(),
            }],
        });
        let warm = CacheStore::default()
            .serve(Some(&root), &profile, &resumed, 1)
            .unwrap();
        assert_eq!(cold.usage.cached_input_tokens, Some(0));
        assert!(
            warm.usage.cached_input_tokens.unwrap_or_default()
                > warm.usage.input_tokens.unwrap_or_default() / 2
        );
    }

    #[test]
    fn prefix_trace_is_request_opt_in_bounded_and_content_free() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let mut original = request();
        let usage = bcode_model::TokenUsage::default();
        trace_prefixes(&original, &usage).unwrap();
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        original
            .provider_context
            .env
            .insert(TRACE_ENV.into(), root.to_string_lossy().into_owned());
        original.provider_context.env.insert(
            "PRIVATE_CREDENTIAL".into(),
            "synthetic-secret-do-not-export".into(),
        );
        original.messages = (0..40)
            .map(|_| ModelMessage {
                role: MessageRole::User,
                content: vec![ContentBlock::Text {
                    text: "synthetic-private-text-do-not-export".into(),
                }],
            })
            .collect();
        for _ in 0..=TRACE_LIMIT {
            trace_prefixes(&original, &usage).unwrap();
        }
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), TRACE_LIMIT);
        let bytes = std::fs::read(root.join("fake-cache-prefix-00.json")).unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(!text.contains("synthetic-private-text-do-not-export"));
        assert!(!text.contains("synthetic-secret-do-not-export"));
        assert!(!text.contains("PRIVATE_CREDENTIAL"));
        let record: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(record["prefixes_truncated"], true);
        assert_eq!(record["prefixes"].as_array().unwrap().len(), TRACE_LIMIT);
        assert_eq!(record["prefixes"][TRACE_LIMIT - 1]["message_index"], 39);
    }

    #[test]
    fn prefix_trace_distinguishes_static_and_ordered_history_drift() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let mut original = request();
        original
            .provider_context
            .env
            .insert(TRACE_ENV.into(), root.to_string_lossy().into_owned());
        original.messages = vec![ModelMessage {
            role: MessageRole::User,
            content: vec![ContentBlock::Text {
                text: "synthetic first turn".into(),
            }],
        }];
        let usage = bcode_model::TokenUsage::default();
        trace_prefixes(&original, &usage).unwrap();
        let mut resumed: ModelTurnRequest =
            serde_json::from_slice(&serde_json::to_vec(&original).unwrap()).unwrap();
        resumed.messages.push(ModelMessage {
            role: MessageRole::User,
            content: vec![ContentBlock::Text {
                text: "synthetic next turn".into(),
            }],
        });
        trace_prefixes(&resumed, &usage).unwrap();
        resumed.messages[0].content = vec![ContentBlock::Text {
            text: "changed synthetic history".into(),
        }];
        trace_prefixes(&resumed, &usage).unwrap();
        resumed.system_prompt = Some("changed synthetic system".into());
        trace_prefixes(&resumed, &usage).unwrap();
        let read = |index| -> serde_json::Value {
            serde_json::from_slice(
                &std::fs::read(root.join(format!("fake-cache-prefix-{index:02}.json"))).unwrap(),
            )
            .unwrap()
        };
        let cold = read(0);
        let warm = read(1);
        let history = read(2);
        let system = read(3);
        assert_eq!(
            cold["prefixes"],
            serde_json::json!(&warm["prefixes"].as_array().unwrap()[..2])
        );
        assert_eq!(warm["prefixes"][0], history["prefixes"][0]);
        assert_ne!(
            warm["prefixes"][1]["sha256"],
            history["prefixes"][1]["sha256"]
        );
        assert_ne!(
            history["prefixes"][0]["sha256"],
            system["prefixes"][0]["sha256"]
        );
    }

    #[test]
    fn damaged_or_locked_cache_is_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let path = root.join(SIMULATOR_STATE_FILE);
        let store = CacheStore::default();
        let profile = profile_for(FAKE_CACHE_PREFIX_MODEL_ID).unwrap();
        let request = request();
        std::fs::write(&path, b"invalid cache").unwrap();
        assert!(store.serve(Some(&root), &profile, &request, 1).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"invalid cache");
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(root.join("fake-provider-prompt-cache.lock"))
            .unwrap();
        lock.try_lock().unwrap();
        assert!(store.serve(Some(&root), &profile, &request, 1).is_err());
        drop(lock);
        assert_eq!(std::fs::read(&path).unwrap(), b"invalid cache");
    }

    #[cfg(unix)]
    #[test]
    fn authorized_root_accepts_platform_alias_ancestors() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let alias = root.join("alias");
        let target = root.join("target");
        std::fs::create_dir(&target).unwrap();
        std::os::unix::fs::symlink(&target, &alias).unwrap();
        CacheStore::default()
            .serve(
                Some(&alias.join("nested/cache")),
                &profile_for(FAKE_CACHE_PREFIX_MODEL_ID).unwrap(),
                &request(),
                1,
            )
            .unwrap();
        assert!(
            target
                .join("nested/cache")
                .join(SIMULATOR_STATE_FILE)
                .is_file()
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_cache_cannot_escape_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let outside = root.join("outside");
        std::fs::write(&outside, b"preserve").unwrap();
        let cache = root.join("cache");
        std::fs::create_dir(&cache).unwrap();
        std::os::unix::fs::symlink(&outside, cache.join(SIMULATOR_STATE_FILE)).unwrap();
        assert!(
            CacheStore::default()
                .serve(
                    Some(&cache),
                    &profile_for(FAKE_CACHE_PREFIX_MODEL_ID).unwrap(),
                    &request(),
                    1
                )
                .is_err()
        );
        assert_eq!(std::fs::read(&outside).unwrap(), b"preserve");
    }
}
