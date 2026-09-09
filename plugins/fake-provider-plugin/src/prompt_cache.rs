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
                id: format!("fake-cache-read-{completed_calls}"),
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
