//! Host context-compaction orchestration.
//!
//! Canonical compaction boundaries replace only structurally complete history prefixes; cuts may
//! not split tool protocols or active conversational units. Explicit provider-native compaction
//! operates under the owning manual command or model turn, while provider-managed compaction is a
//! normal request capability and is never invoked here as a separate operation. Cancellation is
//! checked before provider/summary work and persistence. Usage observations retain exact request
//! attribution, and opaque provider context always carries a portable compatibility fallback.

use super::*;

#[derive(Debug, Error)]
pub enum CompactionError {
    #[error("nothing to compact: {0}")]
    NothingToCompact(String),
    #[error("insufficient compaction progress through #{compacted_through_sequence}: {message}")]
    InsufficientProgress {
        message: String,
        compacted_through_sequence: u64,
    },
    #[error("session error: {0}")]
    Session(#[from] bcode_session::SessionError),
    #[error("model provider unavailable")]
    ProviderUnavailable,
    #[error("compaction cancelled")]
    Cancelled,
    #[error(
        "current indivisible turn cannot fit available input capacity: estimated {estimated_tokens} tokens > available {available_input_tokens}"
    )]
    UncompactableCurrentTurn {
        estimated_tokens: u64,
        available_input_tokens: u64,
    },
    #[error(
        "request remains too large after compaction: estimated {estimated_tokens} tokens >= threshold {threshold_tokens}"
    )]
    RequestStillTooLarge {
        estimated_tokens: u64,
        threshold_tokens: u64,
    },
    #[error("provider error: {0}")]
    Provider(String),
}

pub struct CompactionCompletion {
    pub message: String,
    pub compacted_through_sequence: u64,
}

pub struct CompactionPlan {
    pub prior_boundary: Option<u64>,
    pub compactable_prefix: Vec<bcode_session_models::SessionEvent>,
    pub retained_tail: Vec<bcode_session_models::SessionEvent>,
    pub compacted_through_sequence: u64,
    pub summary_input: Vec<String>,
    pub provider_native_messages: Vec<ModelMessage>,
    pub portable_fallback: String,
    pub estimated_prefix_tokens: u64,
    pub estimated_tail_tokens: u64,
}

pub struct ConversationalUnit {
    pub events: Vec<bcode_session_models::SessionEvent>,
    pub summary_input: Vec<String>,
    pub estimated_tokens: u64,
    pub begins_with_user: bool,
    pub interrupted_tool_chain: bool,
}

pub struct CompactionTranscript {
    pub previous_summary: Option<String>,
    pub summary_input: Vec<String>,
    pub compacted_through_sequence: u64,
    pub event_count: usize,
    pub estimated_reclaimable_tokens: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct CompactionProgressRequirement {
    pub minimum_reclaimable_tokens: u64,
    pub previous_compacted_through_sequence: Option<u64>,
}

pub const COMPACTION_SYSTEM_PROMPT: &str = "You compact coding-agent session history. Produce only a durable continuation summary for future model turns. Preserve all facts needed to continue the work, including user goals, decisions, constraints, files changed, commands run, validation results, current blockers, and next steps. Do not invent details. Do not include markdown fences.";
#[cfg(test)]
pub const COMPACTION_DEFAULT_KEEP_RECENT_CHARS: usize = 8_000;
pub const COMPACTION_MAX_SUMMARY_INPUT_CHARS: usize = 16_000;
pub const COMPACTION_MAX_CARRIED_SUMMARY_CHARS: usize = 6_000;
pub const COMPACTION_MAX_EVENT_CONTENT_CHARS: usize = 4_000;
pub const COMPACTION_TOOL_RESULT_CHARS: usize = 2_000;

pub fn compaction_plan_meets_progress_requirement(
    transcript: &CompactionTranscript,
    requirement: CompactionProgressRequirement,
) -> bool {
    transcript.estimated_reclaimable_tokens >= requirement.minimum_reclaimable_tokens
        && requirement
            .previous_compacted_through_sequence
            .is_none_or(|boundary| transcript.compacted_through_sequence > boundary)
}

pub async fn compact_session_context_before_sequence(
    state: &ServerState,
    session_id: SessionId,
    selection: &SessionModelSelection,
    first_kept_sequence: u64,
    cancel_state: &TurnCancelState,
) -> Result<CompactionCompletion, CompactionError> {
    compact_session_context_with_limit(
        state,
        session_id,
        selection,
        Some(first_kept_sequence),
        None,
        cancel_state,
        None,
    )
    .await
}

#[allow(clippy::too_many_lines)]
pub async fn compact_session_context_with_limit(
    state: &ServerState,
    session_id: SessionId,
    selection: &SessionModelSelection,
    first_kept_sequence: Option<u64>,
    command_context: Option<&mut RuntimeCommandContext<'_>>,
    cancel_state: &TurnCancelState,
    progress_requirement: Option<CompactionProgressRequirement>,
) -> Result<CompactionCompletion, CompactionError> {
    if cancel_state.is_cancelled() {
        return Err(CompactionError::Cancelled);
    }

    let history = state.sessions.model_context_events(session_id).await?;
    let transcript_history = first_kept_sequence.map_or_else(
        || history.clone(),
        |first_kept_sequence| {
            history
                .iter()
                .filter(|event| event.sequence < first_kept_sequence)
                .cloned()
                .collect()
        },
    );
    let Some(plan) = structural_compaction_plan(
        &transcript_history,
        state.tool_output_context_chars,
        usize::try_from(state.auto_compaction.keep_recent_tokens).unwrap_or(usize::MAX),
    ) else {
        return Err(CompactionError::NothingToCompact(
            "nothing new to compact".to_string(),
        ));
    };
    let transcript = compaction_transcript_from_plan(&plan);
    debug_assert!(
        plan.retained_tail
            .iter()
            .all(|event| event.sequence > plan.compacted_through_sequence)
    );
    debug_assert_eq!(
        plan.provider_native_messages,
        session_events_to_model_messages_with_limit(
            &plan.compactable_prefix,
            state.tool_output_context_chars,
        )
    );
    debug_assert_eq!(plan.summary_input, transcript.summary_input);
    debug_assert!(
        plan.prior_boundary
            .is_none_or(|boundary| plan.compacted_through_sequence > boundary)
    );
    if let Some(requirement) = progress_requirement
        && !compaction_plan_meets_progress_requirement(&transcript, requirement)
    {
        return Err(CompactionError::InsufficientProgress {
            message: format!(
                "plan would reclaim approximately {} tokens, below the required {} tokens or without advancing the prior boundary",
                transcript.estimated_reclaimable_tokens, requirement.minimum_reclaimable_tokens
            ),
            compacted_through_sequence: transcript.compacted_through_sequence,
        });
    }

    if !has_model_provider(state, selection.provider_plugin_id.as_deref()) {
        return Err(CompactionError::ProviderUnavailable);
    }

    let native_snapshot = compact_context_with_selected_backend(
        state,
        session_id,
        selection,
        &plan.compactable_prefix,
        cancel_state,
    )
    .await?;
    let portable_summary = if native_snapshot.is_some() {
        match collect_compaction_summary(
            state,
            session_id,
            selection,
            &transcript,
            command_context,
            cancel_state,
        )
        .await
        {
            Ok(summary) if !summary.trim().is_empty() => summary.trim().to_string(),
            Ok(_) => local_compaction_summary(&transcript, "provider returned an empty summary"),
            Err(CompactionError::Cancelled) => return Err(CompactionError::Cancelled),
            Err(error) => local_compaction_summary(&transcript, &compaction_error_detail(error)),
        }
    } else {
        let summary = collect_compaction_summary(
            state,
            session_id,
            selection,
            &transcript,
            command_context,
            cancel_state,
        )
        .await?;
        let summary = summary.trim().to_string();
        if summary.is_empty() {
            return Err(CompactionError::Provider(
                "provider returned an empty compaction summary".to_string(),
            ));
        }
        summary
    };
    if cancel_state.is_cancelled() {
        return Err(CompactionError::Cancelled);
    }
    let event = if let Some(mut snapshot) = native_snapshot {
        snapshot.portable_summary = portable_summary;
        state
            .sessions
            .append_provider_context_compacted(
                session_id,
                snapshot,
                transcript.compacted_through_sequence,
            )
            .await?
    } else {
        state
            .sessions
            .append_context_compacted(
                session_id,
                portable_summary,
                transcript.compacted_through_sequence,
            )
            .await?
    };
    publish_session_event(state, &event).await;
    state.invalidate_session_continuations(session_id).await;

    Ok(CompactionCompletion {
        message: format!(
            "compacted {} events through #{} (retained approximately {} tokens)",
            transcript.event_count,
            transcript.compacted_through_sequence,
            plan.estimated_tail_tokens,
        ),
        compacted_through_sequence: transcript.compacted_through_sequence,
    })
}

pub async fn provider_context_management_capabilities(
    state: &ServerState,
    selection: &SessionModelSelection,
) -> Option<bcode_model::ContextManagementCapabilities> {
    let provider_plugin_id = selection.provider_plugin_id.as_deref()?;
    state
        .plugins
        .invoke_service_json::<_, bcode_model::ContextManagementCapabilities>(
            provider_plugin_id,
            bcode_model::MODEL_PROVIDER_INTERFACE_ID,
            bcode_model::OP_CONTEXT_MANAGEMENT_CAPABILITIES,
            &bcode_model::ContextManagementCapabilitiesRequest {
                provider_context: selection.provider_context.clone(),
                model_id: selection.model_id.clone(),
            },
        )
        .await
        .ok()
}

#[derive(Debug, Clone)]
pub struct AutomaticCompactionPolicy {
    pub decision: CompactionDecision,
    pub capabilities: Option<bcode_model::ContextManagementCapabilities>,
}

pub async fn automatic_compaction_policy(
    state: &ServerState,
    selection: &SessionModelSelection,
) -> AutomaticCompactionPolicy {
    let capabilities = provider_context_management_capabilities(state, selection).await;
    let decision = resolve_compaction_decision(state.auto_compaction.mode, capabilities.as_ref());
    AutomaticCompactionPolicy {
        decision,
        capabilities,
    }
}

#[allow(clippy::too_many_lines)]
pub async fn compact_context_with_selected_backend(
    state: &ServerState,
    session_id: SessionId,
    selection: &SessionModelSelection,
    history: &[bcode_session_models::SessionEvent],
    cancel_state: &TurnCancelState,
) -> Result<Option<bcode_session_models::ProviderContextSnapshot>, CompactionError> {
    if cancel_state.is_cancelled() {
        return Err(CompactionError::Cancelled);
    }
    if matches!(
        state.auto_compaction.backend,
        bcode_config::CompactionBackend::Local
    ) {
        return Ok(None);
    }
    let provider_plugin_id = selection
        .provider_plugin_id
        .as_deref()
        .ok_or(CompactionError::ProviderUnavailable)?;
    let capabilities = provider_context_management_capabilities(state, selection).await;
    if cancel_state.is_cancelled() {
        return Err(CompactionError::Cancelled);
    }
    let context_format = capabilities
        .as_ref()
        .and_then(|capabilities| capabilities.context_format.as_ref())
        .filter(|format| format.version > 0 && !format.compatibility_key.trim().is_empty());
    let native_supported = capabilities
        .as_ref()
        .is_some_and(|capabilities| capabilities.native_compaction)
        && context_format.is_some();
    if !native_supported {
        return match state.auto_compaction.backend {
            bcode_config::CompactionBackend::ProviderNative => Err(CompactionError::Provider(
                "active provider surface does not support native context compaction".to_string(),
            )),
            bcode_config::CompactionBackend::Auto | bcode_config::CompactionBackend::Local => {
                Ok(None)
            }
        };
    }
    let model_id = model_id_for_provider_request(selection.model_id.as_deref());
    let messages = session_events_to_model_messages_for_target(
        history,
        state.tool_output_context_chars,
        Some(provider_plugin_id),
        Some(&model_id),
        selection.provider_context.auth_profile.as_deref(),
        context_format.map(|format| format.version),
        context_format.map(|format| format.compatibility_key.as_str()),
    );
    let request = bcode_model::CompactContextRequest {
        session_id,
        provider_context: selection.provider_context.clone(),
        model_id: model_id.clone(),
        system_prompt: None,
        messages,
        tools: Vec::new(),
    };
    let response = tokio::select! {
        () = cancel_state.cancelled() => return Err(CompactionError::Cancelled),
        response = state.plugins.invoke_service_json::<_, bcode_model::CompactContextResponse>(
            provider_plugin_id,
            bcode_model::MODEL_PROVIDER_INTERFACE_ID,
            bcode_model::OP_COMPACT_CONTEXT,
            &request,
        ) => response,
    };
    if cancel_state.is_cancelled() {
        return Err(CompactionError::Cancelled);
    }
    match response {
        Ok(response)
            if !response.messages.is_empty()
                && response.context_format.version > 0
                && !response.context_format.compatibility_key.trim().is_empty() =>
        {
            let encoded = serde_json::to_string(&response.messages).map_err(|error| {
                CompactionError::Provider(format!(
                    "failed to encode provider-native compacted context: {error}"
                ))
            })?;
            Ok(Some(bcode_session_models::ProviderContextSnapshot {
                format_version: response.context_format.version,
                request_fingerprint: None,
                request_id: None,
                provider_plugin_id: provider_plugin_id.to_string(),
                model_id,
                compatibility_key: response.context_format.compatibility_key,
                auth_profile: selection.provider_context.auth_profile.clone(),
                origin: bcode_session_models::ProviderContextSnapshotOrigin::Explicit,
                messages_json: encoded,
                portable_summary: String::new(),
            }))
        }
        Ok(response) if response.messages.is_empty() => Err(CompactionError::Provider(
            "provider returned empty native compacted context".to_string(),
        )),
        Ok(_) => Err(CompactionError::Provider(
            "provider returned native compacted context without a non-empty versioned compatibility key"
                .to_string(),
        )),
        Err(error)
            if matches!(
                state.auto_compaction.backend,
                bcode_config::CompactionBackend::Auto
            ) =>
        {
            append_context_compaction_trace(
                state,
                session_id,
                "provider_native_fallback",
                0,
                false,
                Some(format!(
                    "provider-native compaction failed ({error}); falling back to local compaction"
                )),
            )
            .await;
            Ok(None)
        }
        Err(error) => Err(CompactionError::Provider(format!(
            "provider-native compaction failed: {error}"
        ))),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomaticCompactionStrategy {
    ProviderManaged,
    LocalProactive,
    OverflowOnly,
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionDecision {
    pub strategy: AutomaticCompactionStrategy,
    pub overflow_recovery: bool,
    pub reason: &'static str,
}

pub fn resolve_compaction_decision(
    mode: bcode_config::CompactionMode,
    capabilities: Option<&bcode_model::ContextManagementCapabilities>,
) -> CompactionDecision {
    let provider_managed = capabilities.is_some_and(|capabilities| {
        capabilities.provider_managed
            && capabilities.context_format.as_ref().is_some_and(|format| {
                format.version > 0 && !format.compatibility_key.trim().is_empty()
            })
    });
    match mode {
        bcode_config::CompactionMode::Off => CompactionDecision {
            strategy: AutomaticCompactionStrategy::Disabled,
            overflow_recovery: false,
            reason: "automatic compaction is disabled",
        },
        bcode_config::CompactionMode::OnOverflow => CompactionDecision {
            strategy: AutomaticCompactionStrategy::OverflowOnly,
            overflow_recovery: true,
            reason: "configured for overflow recovery only",
        },
        bcode_config::CompactionMode::Proactive => CompactionDecision {
            strategy: AutomaticCompactionStrategy::LocalProactive,
            overflow_recovery: false,
            reason: "configured for host proactive compaction",
        },
        bcode_config::CompactionMode::ProactiveAndOverflow => CompactionDecision {
            strategy: AutomaticCompactionStrategy::LocalProactive,
            overflow_recovery: true,
            reason: "configured for host proactive compaction with overflow recovery",
        },
        bcode_config::CompactionMode::Auto if provider_managed => CompactionDecision {
            strategy: AutomaticCompactionStrategy::ProviderManaged,
            overflow_recovery: true,
            reason: "active provider surface advertises managed compaction",
        },
        bcode_config::CompactionMode::Auto => CompactionDecision {
            strategy: AutomaticCompactionStrategy::OverflowOnly,
            overflow_recovery: true,
            reason: "active provider surface does not advertise managed compaction",
        },
    }
}

pub fn request_output_reserve_tokens(
    context_window: u32,
    requested_max_output_tokens: Option<u32>,
    provider_max_output_tokens: Option<u32>,
) -> u32 {
    let requested_or_default =
        requested_max_output_tokens.unwrap_or_else(|| (context_window / 8).max(1));
    provider_max_output_tokens
        .map_or(requested_or_default, |maximum| {
            requested_or_default.min(maximum)
        })
        .min(context_window)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputReserveSource {
    Request,
    Default,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionCapacity {
    pub threshold_tokens: u64,
    pub output_reserve_tokens: u64,
    pub output_reserve_source: OutputReserveSource,
    pub safety_margin_tokens: u64,
    pub available_input_tokens: u64,
}

pub fn compaction_capacity_tokens(
    context_window: u32,
    threshold_percent: u8,
    requested_max_output_tokens: Option<u32>,
    provider_max_output_tokens: Option<u32>,
) -> CompactionCapacity {
    let output_reserve_source = if requested_max_output_tokens.is_some() {
        OutputReserveSource::Request
    } else {
        OutputReserveSource::Default
    };
    let output_reserve_tokens = u64::from(request_output_reserve_tokens(
        context_window,
        requested_max_output_tokens,
        provider_max_output_tokens,
    ));
    let safety_margin_tokens = u64::from(context_window) / 50;
    let available_input_tokens = u64::from(context_window)
        .saturating_sub(output_reserve_tokens)
        .saturating_sub(safety_margin_tokens);
    let threshold_tokens = (u64::from(context_window)
        .saturating_mul(u64::from(threshold_percent.clamp(1, 100)))
        / 100)
        .min(available_input_tokens);
    CompactionCapacity {
        threshold_tokens,
        output_reserve_tokens,
        output_reserve_source,
        safety_margin_tokens,
        available_input_tokens,
    }
}

pub struct ProactiveCompactionEvaluation {
    pub candidate_input_tokens: Option<u64>,
    pub requested_max_output_tokens: Option<u32>,
    pub decision: CompactionDecision,
    pub previous_compacted_through_sequence: Option<u64>,
}

pub async fn request_exceeds_compaction_capacity(
    state: &ServerState,
    session_id: SessionId,
    selection: &SessionModelSelection,
    request: &ModelTurnRequest,
) -> Option<(u64, CompactionCapacity)> {
    let model_status = model_status_for_selection(state, selection.clone(), Some(session_id)).await;
    let context_window = model_status.context_window?;
    let capacity = compaction_capacity_tokens(
        context_window,
        state.auto_compaction.proactive_threshold_percent,
        request.parameters.max_output_tokens,
        model_status.max_output_tokens,
    );
    let estimated_tokens = estimated_model_request_tokens(request);
    (estimated_tokens > capacity.available_input_tokens).then_some((estimated_tokens, capacity))
}

#[allow(clippy::too_many_lines)]
pub fn ensure_compactable_current_turn(
    history: &[bcode_session_models::SessionEvent],
    tool_output_context_chars: usize,
    available_input_tokens: u64,
) -> Result<(), CompactionError> {
    let active_unit_tokens = conversational_units(history, tool_output_context_chars)
        .last()
        .map_or(0, |unit| unit.estimated_tokens);
    if active_unit_tokens > available_input_tokens {
        return Err(CompactionError::UncompactableCurrentTurn {
            estimated_tokens: active_unit_tokens,
            available_input_tokens,
        });
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
pub async fn maybe_auto_compact_session_context(
    state: &ServerState,
    session_id: SessionId,
    selection: &SessionModelSelection,
    cancel_state: &TurnCancelState,
    command_context: &mut RuntimeCommandContext<'_>,
    evaluation: ProactiveCompactionEvaluation,
) -> Result<Option<u64>, CompactionError> {
    if evaluation.decision.strategy != AutomaticCompactionStrategy::LocalProactive {
        return Ok(None);
    }

    let history = state.sessions.model_context_events(session_id).await?;
    let projected_context_chars =
        projected_model_context_chars(&history, state.tool_output_context_chars);
    let projected_context_tokens = evaluation.candidate_input_tokens.unwrap_or_else(|| {
        context_occupancy_tokens(
            &history,
            selection,
            projected_context_chars,
            state.tool_output_context_chars,
        )
    });
    let model_status = model_status_for_selection(state, selection.clone(), Some(session_id)).await;
    let Some(context_window_tokens) = model_status.context_window else {
        append_context_compaction_trace(
            state,
            session_id,
            "context_window_unknown",
            projected_context_chars,
            false,
            Some(
                "skipping proactive compaction because the model context window is unknown"
                    .to_string(),
            ),
        )
        .await;
        return Ok(None);
    };
    let threshold_percent = state.auto_compaction.proactive_threshold_percent;
    let capacity = compaction_capacity_tokens(
        context_window_tokens,
        threshold_percent,
        evaluation.requested_max_output_tokens,
        model_status.max_output_tokens,
    );
    ensure_compactable_current_turn(
        &history,
        state.tool_output_context_chars,
        capacity.available_input_tokens,
    )?;
    let threshold_tokens = capacity.threshold_tokens;
    if projected_context_tokens < threshold_tokens {
        append_context_compaction_trace(
            state,
            session_id,
            "below_threshold",
            projected_context_chars,
            false,
            Some(format!(
                "projected context ~{projected_context_tokens} tokens < threshold {threshold_tokens} tokens ({threshold_percent}% of {context_window_tokens}; output reserve {} from {:?}; safety margin {}; available input {})",
                capacity.output_reserve_tokens,
                capacity.output_reserve_source,
                capacity.safety_margin_tokens,
                capacity.available_input_tokens,
            )),
        )
        .await;
        return Ok(None);
    }

    append_context_compaction_trace(
        state,
        session_id,
        "threshold_exceeded",
        projected_context_chars,
        false,
        Some(format!(
            "projected context ~{projected_context_tokens} tokens >= threshold {threshold_tokens} tokens ({threshold_percent}% of {context_window_tokens}; output reserve {} from {:?}; safety margin {}; available input {})",
            capacity.output_reserve_tokens,
            capacity.output_reserve_source,
            capacity.safety_margin_tokens,
            capacity.available_input_tokens,
        )),
    )
    .await;
    if cancel_state.is_cancelled() {
        return Err(CompactionError::Cancelled);
    }
    let completion = compact_session_context_with_limit(
        state,
        session_id,
        selection,
        None,
        Some(command_context),
        cancel_state,
        Some(CompactionProgressRequirement {
            minimum_reclaimable_tokens: projected_context_tokens
                .saturating_sub(threshold_tokens)
                .saturating_add(estimated_tokens_from_chars(
                    COMPACTION_MAX_CARRIED_SUMMARY_CHARS,
                )),
            previous_compacted_through_sequence: evaluation.previous_compacted_through_sequence,
        }),
    )
    .await?;
    append_context_compaction_trace(
        state,
        session_id,
        "threshold_exceeded",
        projected_context_chars,
        true,
        Some(completion.message),
    )
    .await;
    Ok(Some(completion.compacted_through_sequence))
}

pub async fn append_context_compaction_trace(
    state: &ServerState,
    session_id: SessionId,
    reason: &str,
    projected_context_chars: usize,
    compacted: bool,
    message: Option<String>,
) {
    let phase = if compacted {
        SessionTracePhase::ContextCompactionFinished
    } else if reason == "below_threshold" {
        SessionTracePhase::ContextCompactionSkipped
    } else {
        SessionTracePhase::ContextCompactionStarted
    };
    append_trace_event(
        state,
        session_id,
        None,
        phase,
        SessionTracePayload::ContextCompaction {
            reason: reason.to_string(),
            projected_context_chars,
            compacted,
            message,
        },
    )
    .await;
}

pub async fn collect_compaction_summary(
    state: &ServerState,
    session_id: SessionId,
    selection: &SessionModelSelection,
    transcript: &CompactionTranscript,
    command_context: Option<&mut RuntimeCommandContext<'_>>,
    cancel_state: &TurnCancelState,
) -> Result<String, CompactionError> {
    append_context_compaction_trace(
        state,
        session_id,
        "summary_request",
        0,
        false,
        Some("compacting older context in one bounded request".to_string()),
    )
    .await;

    let prompt_text = compaction_prompt_text(transcript);
    match collect_compaction_summary_once(
        state,
        session_id,
        selection,
        transcript,
        &prompt_text,
        command_context,
        cancel_state,
    )
    .await
    {
        Ok(summary) if !summary.trim().is_empty() => Ok(truncate_text(
            summary.trim(),
            COMPACTION_MAX_CARRIED_SUMMARY_CHARS,
        )),
        Ok(_) => Ok(local_compaction_summary(
            transcript,
            "provider returned an empty summary",
        )),
        Err(CompactionError::Cancelled) => Err(CompactionError::Cancelled),
        Err(CompactionError::Provider(error)) if is_retriable_compaction_error(&error) => {
            append_context_compaction_trace(
                state,
                session_id,
                "local_fallback",
                0,
                true,
                Some(format!(
                    "compaction provider request failed ({error}); using bounded local summary"
                )),
            )
            .await;
            Ok(local_compaction_summary(transcript, &error))
        }
        Err(CompactionError::Provider(error)) => Err(CompactionError::Provider(error)),
        Err(error) => Err(error),
    }
}

pub async fn collect_compaction_summary_once(
    state: &ServerState,
    session_id: SessionId,
    selection: &SessionModelSelection,
    transcript: &CompactionTranscript,
    prompt_text: &str,
    mut command_context: Option<&mut RuntimeCommandContext<'_>>,
    cancel_state: &TurnCancelState,
) -> Result<String, CompactionError> {
    let turn_id = format!(
        "{session_id}-compact-{}",
        transcript.compacted_through_sequence
    );
    let mut request = build_compaction_request(session_id, selection, prompt_text, turn_id.clone());
    request.model_id = effective_model_id(state, selection)
        .await
        .map_err(CompactionError::Provider)?;
    if cancel_state.is_cancelled() {
        return Err(CompactionError::Cancelled);
    }
    let provider_turn_id = if let Some(context) = &mut command_context {
        match wait_for_finalizable_provider_call(
            state,
            session_id,
            context,
            cancel_state,
            Box::pin(invoke_model_provider_json_blocking::<_, StartTurnResponse>(
                state,
                selection.provider_plugin_id.clone(),
                OP_START_TURN,
                request,
            )),
        )
        .await
        {
            FinalizedProviderCall::Completed(result) => {
                result.map_err(CompactionError::Provider)?.provider_turn_id
            }
            FinalizedProviderCall::Cancelled(result) => {
                if let Ok(response) = result {
                    finish_provider_turn(
                        state,
                        selection.provider_plugin_id.clone(),
                        response.provider_turn_id,
                    )
                    .await;
                }
                return Err(CompactionError::Cancelled);
            }
        }
    } else {
        invoke_model_provider_json_blocking::<_, StartTurnResponse>(
            state,
            selection.provider_plugin_id.clone(),
            OP_START_TURN,
            request,
        )
        .await
        .map_err(CompactionError::Provider)?
        .provider_turn_id
    };

    let result = if let Some(context) = command_context {
        poll_compaction_summary_actor_aware(
            state,
            session_id,
            selection,
            &provider_turn_id,
            &turn_id,
            context,
            cancel_state,
        )
        .await
    } else {
        poll_compaction_summary(
            state,
            session_id,
            selection,
            &provider_turn_id,
            &turn_id,
            cancel_state,
        )
        .await
    };
    finish_provider_turn(
        state,
        selection.provider_plugin_id.clone(),
        provider_turn_id,
    )
    .await;
    result
}

pub fn build_compaction_request(
    session_id: SessionId,
    selection: &SessionModelSelection,
    prompt_text: &str,
    turn_id: String,
) -> ModelTurnRequest {
    ModelTurnRequest {
        session_id,
        turn_id,
        model_id: model_id_for_provider_request(selection.model_id.as_deref()),
        provider_context: selection.provider_context.clone(),
        system_prompt: Some(COMPACTION_SYSTEM_PROMPT.to_string()),
        messages: vec![ModelMessage {
            role: MessageRole::User,
            content: vec![ContentBlock::Text {
                text: prompt_text.to_string(),
            }],
        }],
        tools: Vec::new(),
        structured_output: None,
        context_management: bcode_model::ContextManagementRequest::default(),
        parameters: ModelParameters::default(),
        prompt_cache: bcode_model::PromptCacheHints::default(),
        conversation_reuse: bcode_model::ConversationReuseHints::default(),
        metadata: BTreeMap::from([("bcode_request_kind".to_string(), "compaction".to_string())]),
    }
}

pub fn compaction_prompt_text(transcript: &CompactionTranscript) -> String {
    let previous_summary = transcript
        .previous_summary
        .as_deref()
        .unwrap_or_default()
        .trim();
    let carried_summary = truncate_text(previous_summary, COMPACTION_MAX_CARRIED_SUMMARY_CHARS);
    let transcript_text = bounded_compaction_body(
        &transcript.summary_input,
        COMPACTION_MAX_SUMMARY_INPUT_CHARS,
    );
    if carried_summary.is_empty() {
        return format!(
            "Compact this Bcode session transcript for future continuation. Return only the durable continuation summary.\n\nTranscript excerpt:\n\n{transcript_text}"
        );
    }
    format!(
        "Update the existing compacted Bcode session summary with the transcript excerpt. Return only the updated durable continuation summary.\n\nExisting summary:\n\n{carried_summary}\n\nTranscript excerpt:\n\n{transcript_text}"
    )
}

pub fn bounded_compaction_body(lines: &[String], max_chars: usize) -> String {
    truncate_text(
        &lines
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n\n"),
        max_chars,
    )
}

pub fn local_compaction_summary(transcript: &CompactionTranscript, reason: &str) -> String {
    let mut parts = Vec::new();
    if let Some(previous) = transcript.previous_summary.as_deref()
        && !previous.trim().is_empty()
    {
        parts.push("## Previous Summary".to_string());
        parts.push(truncate_text(
            previous.trim(),
            COMPACTION_MAX_CARRIED_SUMMARY_CHARS / 2,
        ));
    }
    parts.push("## Local Compaction Fallback".to_string());
    parts.push(format!(
        "Bcode compacted older session context locally because the provider compaction request could not be used: {reason}. The full canonical history remains in durable session storage."
    ));
    parts.push("## Older Context Outline".to_string());
    parts.push(bounded_compaction_body(
        &transcript.summary_input,
        COMPACTION_MAX_CARRIED_SUMMARY_CHARS / 2,
    ));
    truncate_text(&parts.join("\n\n"), COMPACTION_MAX_CARRIED_SUMMARY_CHARS)
}

pub fn is_retriable_compaction_error(error: &str) -> bool {
    is_context_length_compaction_error(error) || is_timeout_compaction_error(error)
}

pub fn is_context_length_compaction_error(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("context_length")
        || error.contains("context length")
        || error.contains("context window")
        || error.contains("maximum context")
        || error.contains("input exceeds")
        || error.contains("prompt is too long")
        || error.contains("input is too long")
        || error.contains("too many tokens")
}

pub fn is_timeout_compaction_error(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("did not finish compaction turn")
        || error.contains("compaction turn timed out")
        || error.contains("provider was idle")
}

pub fn compaction_error_detail(error: CompactionError) -> String {
    match error {
        CompactionError::Provider(message) => message,
        error => error.to_string(),
    }
}

pub async fn poll_compaction_summary_actor_aware(
    state: &ServerState,
    session_id: SessionId,
    selection: &SessionModelSelection,
    provider_turn_id: &str,
    turn_id: &str,
    command_context: &mut RuntimeCommandContext<'_>,
    cancel_state: &TurnCancelState,
) -> Result<String, CompactionError> {
    let mut summary = String::new();
    let mut idle_for = Duration::ZERO;
    loop {
        let poll = PollTurnEventsRequest {
            provider_turn_id: provider_turn_id.to_string(),
        };
        let response = match wait_for_provider_call(
            state,
            session_id,
            command_context,
            cancel_state,
            Box::pin(poll_model_turn(
                state,
                session_id,
                selection.provider_plugin_id.as_deref(),
                &poll,
            )),
        )
        .await
        {
            ProviderCallWait::Completed(result) => result.map_err(CompactionError::Provider)?,
            ProviderCallWait::Cancelled => return Err(CompactionError::Cancelled),
        };
        if response.events.is_empty() {
            idle_for = wait_for_compaction_progress_actor_aware(
                state,
                session_id,
                command_context,
                cancel_state,
                idle_for,
            )
            .await?;
            continue;
        }
        let saw_progress = compaction_events_include_progress(&response.events);
        match handle_compaction_events(state, session_id, turn_id, &mut summary, response.events)
            .await
        {
            CompactionPollStatus::Continue => {
                if saw_progress {
                    idle_for = Duration::ZERO;
                } else {
                    idle_for = wait_for_compaction_progress_actor_aware(
                        state,
                        session_id,
                        command_context,
                        cancel_state,
                        idle_for,
                    )
                    .await?;
                }
            }
            CompactionPollStatus::Finished => return Ok(summary),
            CompactionPollStatus::Failed(error) => return Err(CompactionError::Provider(error)),
        }
    }
}

pub async fn wait_for_compaction_progress_actor_aware(
    state: &ServerState,
    session_id: SessionId,
    command_context: &mut RuntimeCommandContext<'_>,
    cancel_state: &TurnCancelState,
    idle_for: Duration,
) -> Result<Duration, CompactionError> {
    let idle_for = idle_for.saturating_add(MODEL_POLL_INTERVAL);
    let timeout = Duration::from_secs(state.model_streaming.no_progress_timeout_secs);
    if idle_for > timeout {
        return Err(CompactionError::Provider(format!(
            "model provider made no compaction progress for {} seconds before timeout",
            timeout.as_secs()
        )));
    }
    tokio::select! {
        () = tokio::time::sleep(MODEL_POLL_INTERVAL) => Ok(idle_for),
        cancel_command = command_context.cancel_commands.recv() => {
            if let Some(command) = cancel_command {
                let cancelled = process_cancel_turn_command(
                    state,
                    session_id,
                    command_context.followup_commands,
                    command_context.queued_followups,
                    command.clear_queue,
                    command.requested_by,
                )
                .await;
                let _sent = command.response.send(cancelled);
            }
            if cancel_state.is_cancelled() {
                Err(CompactionError::Cancelled)
            } else {
                Ok(idle_for)
            }
        }
        steering_command = command_context.steering_commands.recv() => {
            if let Some(command) = steering_command {
                process_steering_message_command(
                    state,
                    session_id,
                    command.client_id,
                    command.text,
                    command.completion,
                )
                .await;
            }
            Ok(idle_for)
        }
    }
}

pub async fn poll_compaction_summary(
    state: &ServerState,
    session_id: SessionId,
    selection: &SessionModelSelection,
    provider_turn_id: &str,
    turn_id: &str,
    cancel_state: &TurnCancelState,
) -> Result<String, CompactionError> {
    let mut summary = String::new();
    let mut idle_for = Duration::ZERO;
    loop {
        let poll = PollTurnEventsRequest {
            provider_turn_id: provider_turn_id.to_string(),
        };
        let response = tokio::select! {
            () = cancel_state.cancelled() => return Err(CompactionError::Cancelled),
            response = poll_model_turn(
                state,
                session_id,
                selection.provider_plugin_id.as_deref(),
                &poll,
            ) => response.map_err(CompactionError::Provider)?,
        };
        if response.events.is_empty() {
            idle_for = wait_for_compaction_progress(&state.model_streaming, idle_for, cancel_state)
                .await?;
            continue;
        }
        let saw_progress = compaction_events_include_progress(&response.events);
        match handle_compaction_events(state, session_id, turn_id, &mut summary, response.events)
            .await
        {
            CompactionPollStatus::Continue => {
                if saw_progress {
                    idle_for = Duration::ZERO;
                } else {
                    idle_for = wait_for_compaction_progress(
                        &state.model_streaming,
                        idle_for,
                        cancel_state,
                    )
                    .await?;
                }
            }
            CompactionPollStatus::Finished => return Ok(summary),
            CompactionPollStatus::Failed(error) => return Err(CompactionError::Provider(error)),
        }
    }
}

pub fn compaction_events_include_progress(events: &[ProviderTurnEvent]) -> bool {
    events.iter().any(compaction_event_is_progress)
}

pub const fn compaction_event_is_progress(event: &ProviderTurnEvent) -> bool {
    match event {
        ProviderTurnEvent::TextDelta { text } | ProviderTurnEvent::ReasoningDelta { text } => {
            !text.is_empty()
        }
        _ => false,
    }
}

pub async fn wait_for_compaction_progress(
    streaming: &bcode_config::StreamingConfig,
    idle_for: Duration,
    cancel_state: &TurnCancelState,
) -> Result<Duration, CompactionError> {
    let idle_for = idle_for.saturating_add(MODEL_POLL_INTERVAL);
    let timeout = Duration::from_secs(streaming.no_progress_timeout_secs);
    if idle_for > timeout {
        return Err(CompactionError::Provider(format!(
            "model provider made no compaction progress for {} seconds before timeout",
            timeout.as_secs()
        )));
    }
    tokio::select! {
        () = tokio::time::sleep(MODEL_POLL_INTERVAL) => Ok(idle_for),
        () = cancel_state.cancelled() => Err(CompactionError::Cancelled),
    }
}

pub enum CompactionPollStatus {
    Continue,
    Finished,
    Failed(String),
}

pub async fn handle_compaction_events(
    state: &ServerState,
    session_id: SessionId,
    turn_id: &str,
    summary: &mut String,
    events: Vec<ProviderTurnEvent>,
) -> CompactionPollStatus {
    for event in events {
        match event {
            ProviderTurnEvent::TextDelta { text } => summary.push_str(&text),
            ProviderTurnEvent::Usage { usage } => {
                append_model_usage_event(state, session_id, turn_id.to_string(), usage).await;
            }
            ProviderTurnEvent::Warning { message } => {
                append_system_event(state, session_id, format!("model warning: {message}")).await;
            }
            ProviderTurnEvent::Error { error } => {
                return CompactionPollStatus::Failed(format!(
                    "model error {}: {}",
                    error.code, error.message
                ));
            }
            ProviderTurnEvent::Cancelled => {
                return CompactionPollStatus::Failed("model turn cancelled".to_string());
            }
            ProviderTurnEvent::TurnFinished { stop_reason } => match stop_reason {
                bcode_model::StopReason::Error => {
                    return CompactionPollStatus::Failed("model turn ended with error".to_string());
                }
                bcode_model::StopReason::Cancelled => {
                    return CompactionPollStatus::Failed("model turn cancelled".to_string());
                }
                _ => return CompactionPollStatus::Finished,
            },
            ProviderTurnEvent::ToolCallStarted { .. }
            | ProviderTurnEvent::ToolCallDelta { .. }
            | ProviderTurnEvent::ToolCallFinished { .. } => {
                return CompactionPollStatus::Failed(
                    "compaction summary unexpectedly requested a tool".to_string(),
                );
            }
            ProviderTurnEvent::TurnStarted
            | ProviderTurnEvent::ReasoningDelta { .. }
            | ProviderTurnEvent::RequestProjection { .. }
            | ProviderTurnEvent::ContextCompacted { .. }
            | ProviderTurnEvent::ProviderMetadata { .. }
            | ProviderTurnEvent::RetryScheduled { .. } => {}
        }
    }
    CompactionPollStatus::Continue
}

pub async fn finish_provider_turn(
    state: &ServerState,
    provider_plugin_id: Option<String>,
    provider_turn_id: String,
) {
    let finish = FinishTurnRequest { provider_turn_id };
    let _ = invoke_model_provider_json_blocking::<_, bcode_model::AckResponse>(
        state,
        provider_plugin_id,
        OP_FINISH_TURN,
        finish,
    )
    .await;
}

pub fn conversational_units(
    events: &[bcode_session_models::SessionEvent],
    tool_output_context_chars: usize,
) -> Vec<ConversationalUnit> {
    let mut units = Vec::<ConversationalUnit>::new();
    let mut pending_tool_calls = BTreeSet::new();

    for event in events {
        let starts_user_unit = matches!(event.kind, SessionEventKind::UserMessage { .. })
            && pending_tool_calls.is_empty();
        let starts_orphan_assistant_unit =
            matches!(event.kind, SessionEventKind::AssistantMessage { .. })
                && pending_tool_calls.is_empty()
                && units
                    .last()
                    .is_some_and(|unit| !unit.begins_with_user && !unit.events.is_empty());
        let starts_unit = starts_user_unit
            || starts_orphan_assistant_unit
            || units.is_empty()
            || (pending_tool_calls.is_empty()
                && matches!(
                    event.kind,
                    SessionEventKind::SystemMessage { .. }
                        | SessionEventKind::WorkingDirectoryChanged { .. }
                ));
        if starts_unit {
            units.push(ConversationalUnit {
                events: Vec::new(),
                summary_input: Vec::new(),
                estimated_tokens: 0,
                begins_with_user: matches!(event.kind, SessionEventKind::UserMessage { .. }),
                interrupted_tool_chain: false,
            });
        }
        let unit = units.last_mut().expect("unit is created before use");
        match &event.kind {
            SessionEventKind::ToolCallRequested { tool_call_id, .. } => {
                pending_tool_calls.insert(tool_call_id.clone());
            }
            SessionEventKind::ToolCallFinished { tool_call_id, .. } => {
                pending_tool_calls.remove(tool_call_id);
            }
            _ => {}
        }
        if let Some(text) = session_event_compaction_line(event, tool_output_context_chars) {
            unit.summary_input.push(text);
        }
        unit.events.push(event.clone());
    }
    if !pending_tool_calls.is_empty()
        && let Some(unit) = units.last_mut()
    {
        unit.interrupted_tool_chain = true;
    }
    for unit in &mut units {
        let messages =
            session_events_to_model_messages_with_limit(&unit.events, tool_output_context_chars);
        unit.estimated_tokens =
            estimated_tokens_from_chars(messages.iter().map(model_message_context_chars).sum());
    }
    units
}

pub fn structural_compaction_plan(
    history: &[bcode_session_models::SessionEvent],
    tool_output_context_chars: usize,
    keep_recent_tokens: usize,
) -> Option<CompactionPlan> {
    let history = compact_attach_history(history.to_vec());
    let latest_marker =
        history
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, event)| match &event.kind {
                SessionEventKind::ContextCompacted {
                    summary,
                    compacted_through_sequence,
                } => Some((index, *compacted_through_sequence, summary.clone())),
                SessionEventKind::ProviderContextCompacted {
                    snapshot,
                    compacted_through_sequence,
                } => Some((
                    index,
                    *compacted_through_sequence,
                    snapshot.portable_summary.clone(),
                )),
                _ => None,
            });
    let prior_boundary = latest_marker.as_ref().map(|(_, boundary, _)| *boundary);
    let portable_fallback = latest_marker
        .as_ref()
        .map_or_else(String::new, |(_, _, summary)| summary.clone());
    let start_index = latest_marker.map_or(0, |(index, _, _)| index.saturating_add(1));
    let units = conversational_units(&history[start_index..], tool_output_context_chars);
    if units.len() <= 1 {
        return None;
    }

    let mut retained_tokens = 0_u64;
    let mut retained_start = units.len().saturating_sub(1);
    for (index, unit) in units.iter().enumerate().rev() {
        if retained_tokens >= u64::try_from(keep_recent_tokens).unwrap_or(u64::MAX) {
            break;
        }
        retained_tokens = retained_tokens.saturating_add(unit.estimated_tokens);
        retained_start = index;
    }
    if retained_start == 0 {
        return None;
    }
    // An interrupted tool chain and the active newest user turn are retained as complete units.
    let compactable_units = &units[..retained_start];
    let retained_units = &units[retained_start..];
    let compactable_prefix = compactable_units
        .iter()
        .flat_map(|unit| unit.events.iter().cloned())
        .collect::<Vec<_>>();
    let retained_tail = retained_units
        .iter()
        .flat_map(|unit| unit.events.iter().cloned())
        .collect::<Vec<_>>();
    let compacted_through_sequence = compactable_prefix.last()?.sequence;
    let summary_input = compactable_units
        .iter()
        .flat_map(|unit| unit.summary_input.iter().cloned())
        .collect::<Vec<_>>();
    let provider_native_messages =
        session_events_to_model_messages_with_limit(&compactable_prefix, tool_output_context_chars);
    let estimated_prefix_tokens = compactable_units.iter().fold(0_u64, |total, unit| {
        total.saturating_add(unit.estimated_tokens)
    });
    let estimated_tail_tokens = retained_units.iter().fold(0_u64, |total, unit| {
        total.saturating_add(unit.estimated_tokens)
    });

    Some(CompactionPlan {
        prior_boundary,
        compactable_prefix,
        retained_tail,
        compacted_through_sequence,
        summary_input,
        provider_native_messages,
        portable_fallback,
        estimated_prefix_tokens,
        estimated_tail_tokens,
    })
}

pub fn compaction_transcript_from_plan(plan: &CompactionPlan) -> CompactionTranscript {
    let previous_summary =
        (!plan.portable_fallback.is_empty()).then(|| plan.portable_fallback.clone());
    CompactionTranscript {
        previous_summary,
        summary_input: plan.summary_input.clone(),
        event_count: plan.compactable_prefix.len(),
        estimated_reclaimable_tokens: plan.estimated_prefix_tokens,
        compacted_through_sequence: plan.compacted_through_sequence,
    }
}

#[cfg(test)]
pub fn compaction_transcript(
    history: &[bcode_session_models::SessionEvent],
    tool_output_context_chars: usize,
    keep_recent_tokens: usize,
) -> Option<CompactionTranscript> {
    let plan = structural_compaction_plan(history, tool_output_context_chars, keep_recent_tokens)?;
    Some(compaction_transcript_from_plan(&plan))
}

pub fn session_event_compaction_line(
    event: &bcode_session_models::SessionEvent,
    tool_output_context_chars: usize,
) -> Option<String> {
    match &event.kind {
        SessionEventKind::UserMessage { text, .. } => Some(format!(
            "#{} user:\n{}",
            event.sequence,
            truncate_text(text, COMPACTION_MAX_EVENT_CONTENT_CHARS)
        )),
        SessionEventKind::AssistantMessage { text } => Some(format!(
            "#{} assistant:\n{}",
            event.sequence,
            truncate_text(text, COMPACTION_MAX_EVENT_CONTENT_CHARS)
        )),
        SessionEventKind::ToolCallRequested {
            tool_call_id,
            tool_name,
            arguments_json,
            ..
        } => Some(format!(
            "#{} assistant tool call {tool_call_id} ({tool_name}):\n{}",
            event.sequence,
            truncate_text(arguments_json, COMPACTION_MAX_EVENT_CONTENT_CHARS)
        )),
        SessionEventKind::ToolCallFinished {
            tool_call_id,
            result,
            is_error,
            output,
            ..
        } => Some(format!(
            "#{} tool result {tool_call_id} (error={is_error}):\n{}",
            event.sequence,
            project_tool_result_for_model_context(
                result,
                output.as_ref().map(trace_blob_read_path),
                tool_output_context_chars.min(COMPACTION_TOOL_RESULT_CHARS),
            )
        )),
        SessionEventKind::SystemMessage { text } => Some(format!(
            "#{} system:\n{}",
            event.sequence,
            truncate_text(text, COMPACTION_MAX_EVENT_CONTENT_CHARS)
        )),
        SessionEventKind::WorkingDirectoryChanged {
            old_working_directory,
            new_working_directory,
        } => Some(format!(
            "#{} working directory changed from {} to {}",
            event.sequence,
            old_working_directory.display(),
            new_working_directory.display()
        )),
        _ => None,
    }
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_progress_wait_returns_cancelled() {
        let cancel_state = TurnCancelState::default();
        cancel_state.cancel();

        let result = wait_for_compaction_progress(
            &bcode_config::StreamingConfig::default(),
            Duration::ZERO,
            &cancel_state,
        )
        .await;

        assert!(matches!(result, Err(CompactionError::Cancelled)));
    }
}
