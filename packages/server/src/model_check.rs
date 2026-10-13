//! Bounded, non-session checks through the same provider turn runtime as normal generation.
use super::*;
use bcode_model::{
    ModelCheckReason as Reason, ModelCheckReport, ModelCheckResult, ModelCheckStatus as Status,
};

#[derive(Debug)]
pub struct ActiveCheck {
    pub owner: ClientId,
    pub cancellation: CancellationToken,
}

pub async fn cancel(state: &ServerState, owner: ClientId, id: &str) -> bool {
    let checks = state.model_checks.lock().await;
    checks
        .get(id)
        .filter(|check| check.owner == owner)
        .is_some_and(|check| {
            check.cancellation.cancel();
            true
        })
}

fn valid_request(check_id: &str, model_ids: &[String], timeout_seconds: u64) -> bool {
    !check_id.is_empty()
        && check_id.len() <= 128
        && check_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        && !model_ids.is_empty()
        && model_ids.len() <= 16
        && (1..=60).contains(&timeout_seconds)
        && model_ids
            .iter()
            .all(|id| !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control))
}

fn result(id: &str, status: Status, reason: Reason) -> ModelCheckResult {
    ModelCheckResult {
        model_id: id.to_owned(),
        status,
        reason,
        evidence_persisted: false,
        scope: None,
    }
}

pub struct CheckInput {
    pub check_id: String,
    pub session_id: Option<SessionId>,
    pub provider: Option<String>,
    pub model_ids: Vec<String>,
    pub timeout_seconds: u64,
    pub consent: bool,
}

pub async fn run(
    state: &ServerState,
    client: ClientId,
    input: CheckInput,
) -> Result<ModelCheckReport, &'static str> {
    let CheckInput {
        check_id,
        session_id,
        provider,
        model_ids,
        timeout_seconds,
        consent,
    } = input;
    if !valid_request(&check_id, &model_ids, timeout_seconds) {
        return Err("invalid bounded model check request");
    }
    if !consent {
        return Err("model checks require explicit provider request consent");
    }
    let cancellation = CancellationToken::new();
    {
        let mut checks = state.model_checks.lock().await;
        if checks.len() >= 16 || checks.contains_key(&check_id) {
            return Err("model check capacity or identity conflict");
        }
        checks.insert(
            check_id.clone(),
            ActiveCheck {
                owner: client,
                cancellation: cancellation.clone(),
            },
        );
    }
    let runtime = state.client_runtime_context(client).await;
    let mut selection = if let Some(session_id) = session_id {
        session_model_selection_with_runtime_context(state, session_id, runtime.clone()).await
    } else {
        runtime
            .clone()
            .map_or_else(SessionModelSelection::default, |context| {
                model_selection_from_runtime_context(state, context)
            })
    };
    if provider.is_some() {
        selection.provider_plugin_id = provider;
    }
    let config = runtime
        .as_ref()
        .and_then(|context| context.effective_config_toml.as_deref())
        .and_then(|contents| bcode_config::decode_effective_config(contents).ok())
        .unwrap_or_else(|| state.startup_config.clone());
    let mut results = Vec::with_capacity(model_ids.len());
    for id in &model_ids {
        if cancellation.is_cancelled() {
            results.push(result(id, Status::Cancelled, Reason::Cancelled));
            continue;
        }
        // Complete catalog view retains unverified candidates. Explicit provider lists and local
        // ignore rules still constrain check admission, not merely picker presentation.
        let list = resolved_provider_models(
            state,
            selection.provider_plugin_id.clone(),
            bcode_model::ModelListRequest {
                provider_context: selection.provider_context.clone(),
                selected_model_id: Some(id.clone()),
            },
        )
        .await;
        let admitted = list
            .as_ref()
            .is_ok_and(|list| list.models.iter().any(|model| model.model_id == *id));
        let ignored = selection
            .provider_plugin_id
            .as_deref()
            .and_then(|provider| bcode_config::effective_model_ignore_rules(provider).ok())
            .is_some_and(|rules| {
                let mut models = list
                    .as_ref()
                    .map_or_else(|_| Vec::new(), |list| list.models.clone());
                model_ignores::apply_model_ignores(&mut models, &rules);
                !models
                    .iter()
                    .any(|model| model.model_id == *id && !model_ignores::is_ignored(model))
            });
        if !admitted || ignored {
            results.push(result(id, Status::Inconclusive, Reason::InvalidRequest));
            continue;
        }
        let request = check_request(id, selection.provider_context.clone(), &list);
        results.push(
            check_one(
                state,
                selection.provider_plugin_id.clone(),
                &config,
                request,
                &cancellation,
                timeout_seconds,
            )
            .await,
        );
    }
    state.model_checks.lock().await.remove(&check_id);
    Ok(ModelCheckReport { results })
}

fn check_request(
    id: &str,
    mut context: bcode_model::ProviderRequestContext,
    list: &Result<bcode_model::ModelList, String>,
) -> ModelTurnRequest {
    context.api_surface = list
        .as_ref()
        .ok()
        .and_then(|list| list.models.iter().find(|model| model.model_id == id))
        .and_then(|model| model.api_surface)
        .or(context.api_surface);
    ModelTurnRequest {
        context_lineage: None,
        session_id: SessionId::new(),
        turn_id: uuid::Uuid::new_v4().to_string(),
        model_id: id.to_owned(),
        provider_context: context,
        system_prompt: None,
        messages: vec![ModelMessage {
            role: MessageRole::User,
            content: vec![ContentBlock::Text {
                text: "Reply OK.".to_owned(),
            }],
        }],
        tools: Vec::new(),
        tool_call_policy: bcode_model::ToolCallRequestPolicy::default(),
        tool_schema_mode: None,
        parameters: ModelParameters {
            max_output_tokens: Some(32),
            ..Default::default()
        },
        structured_output: None,
        context_management: bcode_model::ContextManagementRequest::default(),
        prompt_cache: bcode_model::PromptCacheHints::default(),
        conversation_reuse: bcode_model::ConversationReuseHints::default(),
        metadata: BTreeMap::new(),
    }
}

fn check_route(
    state: &ServerState,
    provider: Option<&str>,
) -> Result<(String, &'static str), String> {
    provider.map_or_else(
        || unique_model_provider_route(state),
        |provider| {
            model_provider_interface_for_plugin(state, provider)
                .map(|interface| (provider.to_owned(), interface))
                .ok_or_else(|| "model provider unavailable".to_owned())
        },
    )
}

async fn check_one(
    state: &ServerState,
    provider: Option<String>,
    config: &bcode_config::BcodeConfig,
    request: ModelTurnRequest,
    cancellation: &CancellationToken,
    timeout_seconds: u64,
) -> ModelCheckResult {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_seconds);
    let route = check_route(state, provider.as_deref());
    let Ok((provider_id, interface)) = route else {
        return result(
            &request.model_id,
            Status::Inconclusive,
            Reason::InvalidRequest,
        );
    };
    let bridge = server_model_provider_bridge(
        state.plugins.clone(),
        config.clone(),
        state.optional_judgement.invalidation_handle(),
        &request.turn_id,
        &provider_id,
    );
    let service_cancel = bcode_plugin_sdk::ServiceCancellation::default();
    let start_call = state
        .plugins
        .invoke_service_json_response_with_bridge_scoped_cancellable(
            &provider_id,
            interface,
            OP_START_TURN,
            &request,
            PluginInvocationScope::Global,
            Some(bridge),
            Duration::from_secs(timeout_seconds),
            &service_cancel,
        );
    tokio::pin!(start_call);
    let start_response = tokio::select! {
        biased;
        () = cancellation.cancelled() => {
            service_cancel.cancel();
            let _ = start_call.await;
            return result(&request.model_id, Status::Cancelled, Reason::Cancelled);
        },
        response = &mut start_call => response,
    };
    let start = start_response.ok().and_then(|response| {
        bcode_plugin::decode_service_response::<StartTurnResponse>(response).ok()
    });
    let Some(start) = start else {
        let timed_out = tokio::time::Instant::now() >= deadline;
        return result(
            &request.model_id,
            if timed_out {
                Status::Timeout
            } else {
                Status::Inconclusive
            },
            if timed_out {
                Reason::Timeout
            } else {
                Reason::InconclusiveProviderFailure
            },
        );
    };
    let mut report = result(
        &request.model_id,
        Status::Inconclusive,
        Reason::NoDefinitiveObservation,
    );
    let mut terminal = false;
    while !terminal {
        let polled = tokio::select! {
            biased;
            () = cancellation.cancelled() => { report.status = Status::Cancelled; report.reason = Reason::Cancelled; break; },
            () = tokio::time::sleep_until(deadline) => { report.status = Status::Timeout; report.reason = Reason::Timeout; break; },
            response = invoke_model_provider_json_blocking::<_, PollTurnEventsResponse>(state, provider.clone(), OP_POLL_TURN_EVENTS, PollTurnEventsRequest { provider_turn_id: start.provider_turn_id.clone() }) => response,
        };
        let Ok(polled) = polled else {
            break;
        };
        for event in polled.events {
            match event {
                ProviderTurnEvent::ModelAvailability { observation } => {
                    report.evidence_persisted = model_evidence::persist(state, &observation);
                    report.status = observation.status;
                    report.reason = observation.reason;
                    report.scope = Some(observation.scope);
                }
                ProviderTurnEvent::TurnFinished { .. } | ProviderTurnEvent::Cancelled => {
                    terminal = true;
                }
                _ => {}
            }
        }
        if !terminal {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    settle_check(state, provider, start.provider_turn_id, terminal).await;

    report
}

async fn settle_check(
    state: &ServerState,
    provider: Option<String>,
    provider_turn_id: String,
    mut terminal: bool,
) {
    if !terminal {
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            invoke_model_provider_json_blocking::<_, bcode_model::AckResponse>(
                state,
                provider.clone(),
                OP_CANCEL_TURN,
                CancelTurnRequest {
                    provider_turn_id: provider_turn_id.clone(),
                },
            ),
        )
        .await;
        // Cancellation acknowledgment is not settlement. Drain a bounded terminal observation
        // window before releasing the provider handle; do not claim completed cancellation.
        let cleanup_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !terminal && tokio::time::Instant::now() < cleanup_deadline {
            let response = tokio::time::timeout_at(
                cleanup_deadline,
                invoke_model_provider_json_blocking::<_, PollTurnEventsResponse>(
                    state,
                    provider.clone(),
                    OP_POLL_TURN_EVENTS,
                    PollTurnEventsRequest {
                        provider_turn_id: provider_turn_id.clone(),
                    },
                ),
            )
            .await;
            let Ok(Ok(response)) = response else {
                break;
            };
            for event in response.events {
                match event {
                    ProviderTurnEvent::ModelAvailability { observation } => {
                        model_evidence::persist(state, &observation);
                    }
                    ProviderTurnEvent::TurnFinished { .. } | ProviderTurnEvent::Cancelled => {
                        terminal = true;
                    }
                    _ => {}
                }
            }
            if !terminal {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
    let _ = tokio::time::timeout(
        Duration::from_secs(2),
        invoke_model_provider_json_blocking::<_, bcode_model::AckResponse>(
            state,
            provider,
            OP_FINISH_TURN,
            FinishTurnRequest { provider_turn_id },
        ),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_bounds_and_control_characters() {
        let ids = vec!["gpt-example".to_owned()];
        assert!(valid_request("check-1", &ids, 20));
        assert!(!valid_request("", &ids, 20));
        assert!(!valid_request("check 1", &ids, 20));
        assert!(!valid_request("check-1", &[], 20));
        assert!(!valid_request("check-1", &vec!["model".to_owned(); 17], 20));
        assert!(!valid_request("check-1", &ids, 0));
        assert!(!valid_request("check-1", &ids, 61));
        assert!(!valid_request("check-1", &["model\n".to_owned()], 20));
    }

    #[test]
    fn unadmitted_result_has_no_evidence_or_scope() {
        let result = result("model", Status::Cancelled, Reason::Cancelled);
        assert!(!result.evidence_persisted);
        assert!(result.scope.is_none());
    }
}
