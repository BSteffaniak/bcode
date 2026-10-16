//! Request-only, bounded auxiliary inference through the ordinary centralized model boundary.

mod error;
use error::GenerationError;

use super::model_request_target::ResolvedModelRequestTarget;
use super::{
    ModelRequestAttempt, ModelRequestTargetInput, ServerState, append_model_usage_event,
    local_request_estimate, record_pending_request_usage, resolve_model_request_target,
    session_token_usage, stable_json_hash,
};
use bcode_model::{
    CancelTurnRequest, ContentBlock, ContextManagementRequest, ConversationReuseHints,
    FinishTurnRequest, MODEL_PROVIDER_INTERFACE_ID, MODEL_PROVIDER_INTERFACE_ID_V3, MessageRole,
    ModelMessage, ModelParameters, ModelTurnRequest, OP_CANCEL_TURN, OP_FINISH_TURN,
    OP_POLL_TURN_EVENTS, OP_RUN_TURN, OP_START_TURN, PollTurnEventsRequest, PollTurnEventsResponse,
    PromptCacheHints, ProviderTurnEvent, StartTurnResponse,
};
use bcode_plugin::{PluginInvocationScope, StreamingServiceInvocationEvent};
use bcode_plugin_sdk::ServiceCancellation;
use bcode_plugin_sdk::auxiliary_generation::{
    AuxiliaryGenerationRequest, AuxiliaryGenerationResult,
};
use std::{collections::BTreeMap, time::Duration};

const MAX_REQUEST_BYTES: usize = 256 * 1024;
const MAX_RESULT_BYTES: usize = 16 * 1024;

/// Run one explicitly selected tool-free request without creating conversational history.
///
/// # Errors
/// Returns normalized errors for invalid selection or allowances, cancellation, deadline,
/// failed provider settlement, malformed output, or unavailable durable usage recording.
pub async fn generate(
    state: &ServerState,
    input: AuxiliaryGenerationRequest,
    cancellation: bcode_plugin_sdk::ServiceCancellation,
) -> Result<AuxiliaryGenerationResult, String> {
    let session_id = input.session_id;
    match generate_inner(state, input, cancellation).await {
        Ok(result) => Ok(result),
        Err(error) => {
            let diagnostic = error.diagnostic().to_string();
            super::append_provider_event_trace(
                state,
                session_id,
                "auxiliary-generation",
                "error",
                Some(diagnostic.clone()),
            )
            .await;
            Err(diagnostic)
        }
    }
}

async fn generate_inner(
    state: &ServerState,
    input: AuxiliaryGenerationRequest,
    cancellation: ServiceCancellation,
) -> Result<AuxiliaryGenerationResult, GenerationError> {
    validate_request(&input)?;
    if cancellation.is_cancelled() {
        return Err("auxiliary generation cancelled".into());
    }
    let (provider, target) = resolve_target(state, &input).await?;
    let request = prepare_request(&input, &target);
    if local_request_estimate(&request).tokens > input.max_input_tokens {
        return Err("auxiliary request exceeds input allowance".into());
    }
    let attempt = prepare_attempt(&provider, &request, target);
    record_pending_request_usage(state, input.session_id, &attempt).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(input.timeout_ms);
    let streamed = state
        .plugins
        .registry()
        .manifests()
        .get(&provider)
        .is_some_and(|manifest| {
            manifest
                .services
                .iter()
                .any(|service| service.interface_id == MODEL_PROVIDER_INTERFACE_ID_V3)
        });
    let mut collected = Collected::default();
    let inference = if streamed {
        stream(
            state,
            &provider,
            &request,
            &cancellation,
            deadline,
            &mut collected,
        )
        .await
    } else {
        poll(
            state,
            &provider,
            &request,
            &cancellation,
            deadline,
            &mut collected,
        )
        .await
    };
    let usage_result = if let Some(usage) = &collected.usage {
        append_model_usage_event(
            state,
            input.session_id,
            request.turn_id.clone(),
            session_token_usage(usage, Some(&attempt)),
            Some(&attempt),
        )
        .await
        .map_err(GenerationError::from)
    } else {
        Ok(())
    };
    match (inference, usage_result) {
        (Err(primary), Err(secondary)) => return Err(primary.secondary(secondary)),
        (Err(error), _) | (_, Err(error)) => return Err(error),
        (Ok(()), Ok(())) => {}
    }
    check_cancel(&cancellation, deadline)?;
    Ok(AuxiliaryGenerationResult {
        text: collected.text,
        usage: collected.usage,
        model_id: request.model_id.clone(),
        provider_plugin_id: provider,
    })
}

async fn resolve_target(
    state: &ServerState,
    input: &AuxiliaryGenerationRequest,
) -> Result<(String, ResolvedModelRequestTarget), String> {
    let config = state.session_config(input.session_id).await;
    let selection = config
        .resolved_model_profile(&input.model_profile)
        .ok_or_else(|| "auxiliary model profile unavailable".to_owned())?;
    let provider = selection
        .provider_plugin_id
        .clone()
        .ok_or_else(|| "auxiliary provider selection unavailable".to_owned())?;
    let context = bcode_provider_auth::resolve_provider_request_context(
        bcode_provider_auth::ProviderRequestContextResolution {
            config: &config,
            selection,
        },
    );
    let target = resolve_model_request_target(
        state,
        ModelRequestTargetInput {
            provider_plugin_id: Some(&provider),
            selected_model_id: config
                .resolved_model_profile(&input.model_profile)
                .and_then(|selection| selection.model_id)
                .as_deref(),
            provider_context: &context,
            native_default: false,
        },
    )
    .await
    .map_err(|error| {
        bcode_model_provider_runtime::sanitize_provider_diagnostic(&format!(
            "auxiliary model resolution failed: {error}"
        ))
    })?;
    Ok((provider, target))
}

fn prepare_request(
    input: &AuxiliaryGenerationRequest,
    target: &ResolvedModelRequestTarget,
) -> ModelTurnRequest {
    ModelTurnRequest {
        context_lineage: None,
        session_id: input.session_id,
        turn_id: format!("{}-auxiliary-{}", input.session_id, uuid::Uuid::new_v4()),
        model_id: target.model_id.clone(),
        provider_context: target.provider_context.clone(),
        system_prompt: Some(input.system_prompt.clone()),
        messages: vec![ModelMessage {
            role: MessageRole::User,
            content: vec![ContentBlock::Text {
                text: input.prompt.clone(),
            }],
        }],
        tools: Vec::new(),
        tool_call_policy: bcode_model::ToolCallRequestPolicy {
            choice: bcode_model::ToolChoice::None,
            ..Default::default()
        },
        tool_schema_mode: None,
        structured_output: None,
        context_management: ContextManagementRequest::default(),
        parameters: ModelParameters {
            max_output_tokens: Some(input.max_output_tokens),
            ..Default::default()
        },
        prompt_cache: PromptCacheHints::default(),
        conversation_reuse: ConversationReuseHints::default(),
        metadata: BTreeMap::new(),
    }
}

fn prepare_attempt(
    provider: &str,
    request: &ModelTurnRequest,
    target: ResolvedModelRequestTarget,
) -> ModelRequestAttempt {
    ModelRequestAttempt {
        identity: bcode_session_models::ModelRequestIdentity {
            provider_plugin_id: provider.to_owned(),
            requested_model_id: target.requested_model_id.clone(),
            effective_model_id: target.model_id.clone(),
            model_turn_id: request.turn_id.clone(),
            request_id: request.turn_id.clone(),
            round: 0,
            request_fingerprint: stable_json_hash(&request),
            effective_auth_profile: request.provider_context.auth_profile.clone(),
            context_format_version: None,
            compatibility_key: None,
            context_epoch: 0,
        },
        provider_turn_id: request.turn_id.clone(),
        pricing: target.pricing,
        catalog_provider_id: target.catalog_provider_id,
        catalog_entry_id: target
            .catalog_identity
            .as_ref()
            .map(|identity| identity.catalog_entry_id.clone()),
        catalog_family: target
            .catalog_identity
            .as_ref()
            .and_then(|identity| identity.family.clone()),
        pricing_target: target.pricing_target,
        catalog_api_surface: target
            .catalog_identity
            .as_ref()
            .and_then(|identity| identity.api_surface),
        reuse_key: None,
        request_message_count: request.messages.len(),
        context_through_sequence: 0,
        portable_context: String::new(),
        local_estimate: local_request_estimate(request),
        original_usage: None,
        managed_compaction_persisted: false,
    }
}

async fn stream(
    state: &ServerState,
    provider: &str,
    request: &ModelTurnRequest,
    cancellation: &ServiceCancellation,
    deadline: tokio::time::Instant,
    collected: &mut Collected,
) -> Result<(), GenerationError> {
    let payload =
        serde_json::to_vec(request).map_err(bcode_plugin::PluginServiceCallError::RequestEncode)?;
    let mut invocation = tokio::time::timeout_at(
        deadline,
        state.plugins.invoke_service_with_events_and_bridge_scoped(
            provider,
            MODEL_PROVIDER_INTERFACE_ID_V3,
            OP_RUN_TURN,
            payload,
            PluginInvocationScope::session(request.session_id.to_string()),
            Some(super::server_model_provider_bridge(
                state.plugins.clone(),
                state.session_config(request.session_id).await,
                state.optional_judgement.invalidation_handle(),
                &request.turn_id,
                provider,
            )),
        ),
    )
    .await
    .map_err(|_| "auxiliary provider admission deadline exceeded")?
    .map_err(GenerationError::from)?;
    let result = async {
        loop {
            check_cancel(cancellation, deadline)?;
            let next =
                tokio::time::timeout(Duration::from_millis(100), invocation.next_event()).await;
            let Ok(next) = next else {
                continue;
            };
            match next.map_err(GenerationError::from)? {
                StreamingServiceInvocationEvent::Event(payload) => {
                    let event = serde_json::from_slice::<ProviderTurnEvent>(&payload)
                        .map_err(bcode_plugin::PluginServiceCallError::ResponseDecode)?;
                    if collected.apply(event)? {
                        return Ok(());
                    }
                }
                StreamingServiceInvocationEvent::Response(result) => {
                    let response = result.map_err(GenerationError::from)?;
                    let _: serde_json::Value = decode_response(response)?;
                    return Err("auxiliary provider ended without terminal outcome".into());
                }
            }
        }
    }
    .await;
    // Preserve executor ownership while cancellation drains the remote invocation.
    let settlement = invocation.cancel_and_settle(Duration::from_secs(2)).await;
    drop(invocation);
    let cleanup = match settlement {
        bcode_plugin::PluginInvocationSettlement::Settled(response) => {
            response.error.map(|error| {
                GenerationError::from(bcode_plugin::PluginServiceCallError::Service {
                    code: error.code,
                    message: error.message,
                })
            })
        }
        bcode_plugin::PluginInvocationSettlement::Unknown => Some(GenerationError::from(
            "auxiliary provider cleanup outcome unknown",
        )),
    };
    match (result, cleanup) {
        (Err(primary), Some(secondary)) => Err(primary.secondary(secondary)),
        (Err(error), None) | (Ok(()), Some(error)) => Err(error),
        (Ok(()), None) => Ok(()),
    }
}

fn decode_response<R: serde::de::DeserializeOwned>(
    response: bcode_plugin_sdk::ServiceResponse,
) -> Result<R, GenerationError> {
    if response.error.is_some()
        && let Ok(ProviderTurnEvent::Error { error }) = serde_json::from_slice(&response.payload)
    {
        return Err(GenerationError::Provider(Box::new(error)));
    }
    bcode_plugin::decode_service_response(response).map_err(GenerationError::from)
}

async fn call<Q: serde::Serialize + Sync, R: serde::de::DeserializeOwned>(
    state: &ServerState,
    provider: &str,
    operation: &str,
    request: &Q,
    cancellation: &ServiceCancellation,
    deadline: tokio::time::Instant,
) -> Result<R, GenerationError> {
    check_cancel(cancellation, deadline)?;
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    let response = state
        .plugins
        .invoke_service_json_response_with_bridge_scoped_cancellable(
            provider,
            MODEL_PROVIDER_INTERFACE_ID,
            operation,
            request,
            PluginInvocationScope::Global,
            None,
            remaining,
            cancellation,
        )
        .await?;
    decode_response(response)
}

async fn poll(
    state: &ServerState,
    provider: &str,
    request: &ModelTurnRequest,
    cancellation: &ServiceCancellation,
    deadline: tokio::time::Instant,
    collected: &mut Collected,
) -> Result<(), GenerationError> {
    check_cancel(cancellation, deadline)?;
    let response = state
        .plugins
        .invoke_service_json_response_with_bridge_scoped_cancellable(
            provider,
            MODEL_PROVIDER_INTERFACE_ID,
            OP_START_TURN,
            request,
            PluginInvocationScope::session(request.session_id.to_string()),
            Some(super::server_model_provider_bridge(
                state.plugins.clone(),
                state.session_config(request.session_id).await,
                state.optional_judgement.invalidation_handle(),
                &request.turn_id,
                provider,
            )),
            deadline.saturating_duration_since(tokio::time::Instant::now()),
            cancellation,
        )
        .await?;
    let started: StartTurnResponse = decode_response(response)?;
    let result: Result<(), GenerationError> = async {
        loop {
            let page: PollTurnEventsResponse = call(
                state,
                provider,
                OP_POLL_TURN_EVENTS,
                &PollTurnEventsRequest {
                    provider_turn_id: started.provider_turn_id.clone(),
                },
                cancellation,
                deadline,
            )
            .await?;
            for event in page.events {
                if collected.apply(event)? {
                    return Ok(());
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    .await;
    let cleanup = ServiceCancellation::default();
    let cleanup_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut result = result;
    if result.is_err() {
        let cancellation_result: Result<bcode_model::AckResponse, GenerationError> = call(
            state,
            provider,
            OP_CANCEL_TURN,
            &CancelTurnRequest {
                provider_turn_id: started.provider_turn_id.clone(),
            },
            &cleanup,
            cleanup_deadline,
        )
        .await;
        if let Err(error) = cancellation_result {
            result = result.map_err(|primary| primary.secondary(error));
        }
    }
    let finish_result: Result<bcode_model::AckResponse, GenerationError> = call(
        state,
        provider,
        OP_FINISH_TURN,
        &FinishTurnRequest {
            provider_turn_id: started.provider_turn_id,
        },
        &cleanup,
        tokio::time::Instant::now() + Duration::from_secs(2),
    )
    .await;
    match (result, finish_result) {
        (Err(primary), Err(secondary)) => Err(primary.secondary(secondary)),
        (Err(error), _) | (_, Err(error)) => Err(error),
        (Ok(()), Ok(_)) => Ok(()),
    }
}

fn validate_request(request: &AuxiliaryGenerationRequest) -> Result<(), String> {
    if request.model_profile.trim().is_empty()
        || request.model_profile.len() > 256
        || request.prompt.trim().is_empty()
        || request
            .prompt
            .len()
            .saturating_add(request.system_prompt.len())
            > MAX_REQUEST_BYTES
        || request.max_input_tokens == 0
        || request.max_input_tokens > 64_000
        || request.max_output_tokens == 0
        || request.max_output_tokens > 4096
        || request.timeout_ms == 0
        || request.timeout_ms > 120_000
    {
        return Err("invalid auxiliary generation request".into());
    }
    Ok(())
}

fn check_cancel(
    cancellation: &bcode_plugin_sdk::ServiceCancellation,
    deadline: tokio::time::Instant,
) -> Result<(), String> {
    if cancellation.is_cancelled() {
        Err("auxiliary generation cancelled".into())
    } else if tokio::time::Instant::now() >= deadline {
        Err("auxiliary generation deadline exceeded".into())
    } else {
        Ok(())
    }
}

#[derive(Default)]
struct Collected {
    text: String,
    usage: Option<bcode_model::TokenUsage>,
}

impl Collected {
    fn apply(&mut self, event: ProviderTurnEvent) -> Result<bool, GenerationError> {
        match event {
            ProviderTurnEvent::TextDelta { text }
            | ProviderTurnEvent::Output {
                event: bcode_model::ProviderOutputEvent::TextDelta { text },
                ..
            } => {
                if self.text.len().saturating_add(text.len()) > MAX_RESULT_BYTES {
                    return Err("auxiliary output exceeds allowance".into());
                }
                self.text.push_str(&text);
            }
            ProviderTurnEvent::Usage { usage } => self.usage = Some(usage),
            ProviderTurnEvent::ToolCallStarted { .. }
            | ProviderTurnEvent::ToolCallDelta { .. }
            | ProviderTurnEvent::ToolCallFinished { .. }
            | ProviderTurnEvent::Output {
                event:
                    bcode_model::ProviderOutputEvent::ToolCallStarted { .. }
                    | bcode_model::ProviderOutputEvent::ToolCallDelta { .. }
                    | bcode_model::ProviderOutputEvent::ToolCallFinished { .. },
                ..
            } => return Err("auxiliary tool calls are forbidden".into()),
            ProviderTurnEvent::Cancelled => return Err("auxiliary generation cancelled".into()),
            ProviderTurnEvent::Error { error } => {
                return Err(GenerationError::Provider(Box::new(error)));
            }
            ProviderTurnEvent::TurnFinished { stop_reason } => {
                if !matches!(
                    stop_reason,
                    bcode_model::StopReason::EndTurn | bcode_model::StopReason::StopSequence
                ) {
                    return Err("auxiliary generation did not complete".into());
                }
                if self.text.trim().is_empty() {
                    return Err("auxiliary provider returned empty output".into());
                }
                return Ok(true);
            }
            _ => {}
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::{Collected, MAX_RESULT_BYTES, check_cancel};
    use bcode_model::ProviderTurnEvent;
    use bcode_plugin_sdk::ServiceCancellation;
    use std::time::Duration;
    #[test]
    fn observed_usage_survives_malformed_output_and_cancellation() {
        let usage = bcode_model::TokenUsage {
            input_tokens: Some(12),
            output_tokens: Some(3),
            ..bcode_model::TokenUsage::default()
        };
        let mut collected = Collected::default();
        collected
            .apply(ProviderTurnEvent::Usage {
                usage: usage.clone(),
            })
            .unwrap();
        assert!(
            collected
                .apply(ProviderTurnEvent::ToolCallStarted {
                    call_id: "bad".into(),
                    name: "forbidden".into()
                })
                .is_err()
        );
        assert_eq!(collected.usage.as_ref(), Some(&usage));
        let cancellation = ServiceCancellation::default();
        cancellation.cancel();
        assert!(
            check_cancel(
                &cancellation,
                tokio::time::Instant::now() + Duration::from_secs(1)
            )
            .is_err()
        );
        assert_eq!(collected.usage.as_ref(), Some(&usage));
    }

    #[test]
    fn output_is_bounded_and_errors_are_normalized() {
        let mut result = Collected::default();
        assert!(
            result
                .apply(ProviderTurnEvent::TextDelta {
                    text: "x".repeat(MAX_RESULT_BYTES)
                })
                .is_ok()
        );
        assert!(
            result
                .apply(ProviderTurnEvent::TextDelta { text: "x".into() })
                .is_err()
        );
        assert!(
            Collected::default()
                .apply(ProviderTurnEvent::TurnFinished {
                    stop_reason: bcode_model::StopReason::EndTurn
                })
                .is_err()
        );
        assert!(
            Collected::default()
                .apply(ProviderTurnEvent::ToolCallStarted {
                    call_id: "id".into(),
                    name: "shell.run".into()
                })
                .is_err()
        );
    }
}
