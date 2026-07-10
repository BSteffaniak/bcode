//! `OpenAI` Responses context-compaction support.
//!
//! This module owns surface capability identity, explicit `/responses/compact` requests, opaque
//! compaction-item validation, and provider-managed compaction stream projection. Keeping these
//! paths together guarantees that advertised support and accepted opaque formats use one predicate.

use super::*;

const OPENAI_CONTEXT_FORMAT_VERSION: u16 = 1;

pub fn openai_context_format(settings: &Settings) -> ProviderContextFormat {
    ProviderContextFormat {
        version: OPENAI_CONTEXT_FORMAT_VERSION,
        compatibility_key: format!(
            "{}|{}",
            settings.dialect.metadata_value(),
            settings.base_url.trim_end_matches('/')
        ),
    }
}

pub fn openai_context_compaction_opted_in(provider_context: &ProviderRequestContext) -> bool {
    provider_context
        .settings
        .get("native_context_compaction")
        .is_some_and(|value| value.eq_ignore_ascii_case("true") || value == "1")
}

pub fn supports_openai_context_compaction(
    dialect: OpenAiCompatibleDialect,
    base_url: &str,
    opted_in: bool,
) -> bool {
    dialect == OpenAiCompatibleDialect::ResponsesApi
        && (base_url.trim_end_matches('/') == DEFAULT_BASE_URL || opted_in)
}

fn valid_compaction_item(item: &serde_json::Value) -> bool {
    item.get("type").and_then(serde_json::Value::as_str) == Some("compaction")
        && item
            .get("id")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| !value.is_empty())
        && item
            .get("encrypted_content")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| !value.is_empty())
}

#[derive(Debug, Serialize)]
struct ResponsesCompactRequest {
    model: String,
    input: Vec<ResponsesInputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResponsesCompactBody {
    object: String,
    #[serde(default)]
    output: Vec<serde_json::Value>,
}

fn compact_endpoint(settings: &Settings) -> String {
    match settings.dialect {
        OpenAiCompatibleDialect::ChatGptCodex => {
            format!("{OPENAI_CODEX_API_ENDPOINT}/compact")
        }
        OpenAiCompatibleDialect::ResponsesApi => format!(
            "{}/responses/compact",
            settings.base_url.trim_end_matches('/')
        ),
        OpenAiCompatibleDialect::ChatCompletions => unreachable!(),
    }
}

fn validate_compact_response(
    response: ResponsesCompactBody,
) -> Result<Vec<serde_json::Value>, ProviderError> {
    if response.object != "response.compaction" {
        return Err(provider_error(
            "invalid_compaction_response",
            ProviderErrorCategory::ProviderInternal,
            format!("unexpected compaction object type: {}", response.object),
        ));
    }
    if response.output.is_empty() {
        return Err(provider_error(
            "invalid_compaction_response",
            ProviderErrorCategory::ProviderInternal,
            "compaction response output was empty",
        ));
    }
    if !response.output.iter().any(valid_compaction_item) {
        return Err(provider_error(
            "invalid_compaction_response",
            ProviderErrorCategory::ProviderInternal,
            "compaction response did not contain a valid compaction item",
        ));
    }
    Ok(response.output)
}

#[allow(clippy::too_many_lines)]
pub async fn compact_context_inner(
    request: CompactContextRequest,
) -> Result<CompactContextResponse, ProviderError> {
    let settings = settings_for_context(&request.provider_context);
    if !supports_openai_context_compaction(
        settings.dialect,
        &settings.base_url,
        openai_context_compaction_opted_in(&request.provider_context),
    ) {
        return Err(provider_error(
            "native_compaction_unsupported",
            ProviderErrorCategory::UnsupportedFeature,
            "provider-native compaction requires an official OpenAI Responses API surface or explicit native context compaction opt-in",
        ));
    }
    let Some(access_token) = settings.auth.token() else {
        return Err(provider_error(
            "missing_openai_auth",
            ProviderErrorCategory::Auth,
            "provider-native compaction requires OpenAI authentication",
        ));
    };
    let turn_request = ModelTurnRequest {
        session_id: request.session_id,
        turn_id: "context-compaction".to_string(),
        model_id: request.model_id.clone(),
        provider_context: request.provider_context,
        system_prompt: request.system_prompt,
        messages: request.messages,
        tools: request.tools,
        parameters: bcode_model::ModelParameters::default(),
        structured_output: None,
        context_management: bcode_model::ContextManagementRequest::default(),
        prompt_cache: bcode_model::PromptCacheHints::default(),
        conversation_reuse: bcode_model::ConversationReuseHints::default(),
        metadata: BTreeMap::new(),
    };
    let projection = responses_projection(
        &turn_request,
        responses_instruction_strategy(&settings),
        false,
        settings.dialect,
    );
    let body = ResponsesCompactRequest {
        model: request.model_id,
        input: projection.input,
        instructions: projection.instructions,
    };
    let client = model_stream_client(settings.request_timeout).map_err(|error| {
        provider_error(
            "client_build_failed",
            ProviderErrorCategory::ProviderInternal,
            error.to_string(),
        )
    })?;
    let endpoint = compact_endpoint(&settings);
    let mut builder = client
        .post(endpoint)
        .bearer_auth(access_token)
        .header("originator", "bcode")
        .header("User-Agent", "bcode/0.0.1")
        .header("session_id", request.session_id.to_string())
        .json(&body);
    if settings.dialect.uses_codex_request_shape() {
        builder = builder.header("OpenAI-Beta", "responses=experimental");
    }
    if let AuthSettings::ChatGpt {
        account_id: Some(account_id),
        ..
    } = &settings.auth
    {
        builder = builder.header("ChatGPT-Account-Id", account_id);
    }
    let response = builder.send().await.map_err(|error| {
        provider_error(
            "request_failed",
            if error.is_timeout() {
                ProviderErrorCategory::Timeout
            } else {
                ProviderErrorCategory::Network
            },
            error.to_string(),
        )
    })?;
    let status = response.status();
    if !status.is_success() {
        let headers = response.headers().clone();
        let body = response.text().await.unwrap_or_default();
        return Err(error_from_status_and_headers(
            status.as_u16(),
            Some(&headers),
            &body,
        ));
    }
    let response = response
        .json::<ResponsesCompactBody>()
        .await
        .map_err(|error| {
            provider_error(
                "invalid_compaction_response",
                ProviderErrorCategory::ProviderInternal,
                error.to_string(),
            )
        })?;
    let output = validate_compact_response(response)?;
    let content = output
        .into_iter()
        .map(|value| ContentBlock::ProviderExtension { value })
        .collect::<Vec<_>>();
    Ok(CompactContextResponse {
        messages: (!content.is_empty())
            .then_some(ModelMessage {
                role: MessageRole::Assistant,
                content,
            })
            .into_iter()
            .collect(),
        context_format: openai_context_format(&settings),
    })
}

pub fn process_responses_compaction_output_item(
    event: &serde_json::Value,
    turn: &TurnState,
    context_format: &ProviderContextFormat,
    completed_items: &std::cell::RefCell<BTreeSet<(u32, String)>>,
) {
    let Some(item) = event.get("item") else {
        return;
    };
    if item.get("type").and_then(serde_json::Value::as_str) != Some("compaction") {
        return;
    }
    if !valid_compaction_item(item) {
        turn.push(ProviderTurnEvent::Warning {
            message:
                "provider emitted a malformed compaction item; no context boundary was created"
                    .to_string(),
        });
        return;
    }
    let Some(id) = item.get("id").and_then(serde_json::Value::as_str) else {
        return;
    };
    let output_index = event
        .get("output_index")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(u32::MAX);
    if !completed_items
        .borrow_mut()
        .insert((output_index, id.to_string()))
    {
        return;
    }
    turn.push(ProviderTurnEvent::ContextCompacted {
        messages: vec![ModelMessage {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::ProviderExtension {
                value: item.clone(),
            }],
        }],
        context_format: context_format.clone(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_request_serializes_only_supported_fields() {
        let request = ResponsesCompactRequest {
            model: "gpt-5".to_string(),
            input: vec![ResponsesInputItem::Message {
                role: "user".to_string(),
                content: vec![ResponsesContent::InputText {
                    text: "hello".to_string(),
                }],
            }],
            instructions: None,
        };
        assert_eq!(
            serde_json::to_value(request).expect("serialize compact request"),
            serde_json::json!({
                "model": "gpt-5",
                "input": [{
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "hello"}]
                }]
            })
        );
    }

    #[test]
    fn compact_endpoint_uses_exact_responses_url() {
        let mut context = ProviderRequestContext::default();
        context.settings.insert(
            "base_url".to_string(),
            "https://example.test/v1/".to_string(),
        );
        context.settings.insert(
            "dialect".to_string(),
            OpenAiCompatibleDialect::ResponsesApi
                .metadata_value()
                .to_string(),
        );
        let settings = settings_for_context(&context);
        assert_eq!(
            compact_endpoint(&settings),
            "https://example.test/v1/responses/compact"
        );
    }

    #[test]
    fn compact_response_requires_object_nonempty_output_and_valid_item() {
        for body in [
            ResponsesCompactBody {
                object: "response".to_string(),
                output: vec![
                    serde_json::json!({"type": "compaction", "id": "id", "encrypted_content": "opaque"}),
                ],
            },
            ResponsesCompactBody {
                object: "response.compaction".to_string(),
                output: Vec::new(),
            },
            ResponsesCompactBody {
                object: "response.compaction".to_string(),
                output: vec![
                    serde_json::json!({"type": "compaction", "id": "", "encrypted_content": "opaque"}),
                ],
            },
        ] {
            assert!(validate_compact_response(body).is_err());
        }
    }

    #[test]
    fn compact_response_preserves_exact_unknown_nested_json() {
        let item = serde_json::json!({
            "type": "compaction",
            "id": "cmp_1",
            "encrypted_content": "opaque",
            "unknown": {"nested": [1, {"future": true}]}
        });
        let output = validate_compact_response(ResponsesCompactBody {
            object: "response.compaction".to_string(),
            output: vec![item.clone()],
        })
        .expect("valid response");
        assert_eq!(output, vec![item]);
    }

    #[test]
    fn unknown_nested_fields_survive_model_message_persistence_round_trip() {
        let item = serde_json::json!({
            "type": "compaction", "id": "cmp", "encrypted_content": "opaque",
            "future": {"nested": [true, 42, {"key": "value"}]}
        });
        let messages = vec![ModelMessage {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::ProviderExtension {
                value: item.clone(),
            }],
        }];
        let persisted = serde_json::to_string(&messages).expect("persist messages");
        let replayed: Vec<ModelMessage> =
            serde_json::from_str(&persisted).expect("replay messages");
        assert_eq!(replayed, messages);
        assert!(matches!(
            &replayed[0].content[0],
            ContentBlock::ProviderExtension { value } if value == &item
        ));
    }

    #[test]
    fn managed_compaction_deduplicates_and_warns_without_boundaries_for_malformed_items() {
        let turn = TurnState::default();
        let format = ProviderContextFormat {
            version: 1,
            compatibility_key: "surface".to_string(),
        };
        let completed = std::cell::RefCell::new(BTreeSet::new());
        let valid = serde_json::json!({
            "output_index": 2,
            "item": {"type": "compaction", "id": "cmp", "encrypted_content": "opaque"}
        });
        process_responses_compaction_output_item(&valid, &turn, &format, &completed);
        process_responses_compaction_output_item(&valid, &turn, &format, &completed);
        process_responses_compaction_output_item(
            &serde_json::json!({
                "output_index": 3,
                "item": {"type": "compaction", "id": "bad", "encrypted_content": ""}
            }),
            &turn,
            &format,
            &completed,
        );
        let events = turn.drain();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ProviderTurnEvent::ContextCompacted { .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ProviderTurnEvent::Warning { .. }))
                .count(),
            1
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, ProviderTurnEvent::TextDelta { .. }))
        );
    }

    #[test]
    fn capability_predicate_is_truthful_for_every_surface() {
        assert!(supports_openai_context_compaction(
            OpenAiCompatibleDialect::ResponsesApi,
            DEFAULT_BASE_URL,
            false
        ));
        assert!(supports_openai_context_compaction(
            OpenAiCompatibleDialect::ResponsesApi,
            "https://custom.example/v1",
            true
        ));
        assert!(!supports_openai_context_compaction(
            OpenAiCompatibleDialect::ResponsesApi,
            "https://custom.example/v1",
            false
        ));
        assert!(!supports_openai_context_compaction(
            OpenAiCompatibleDialect::ChatCompletions,
            DEFAULT_BASE_URL,
            true
        ));
        assert!(!supports_openai_context_compaction(
            OpenAiCompatibleDialect::ChatGptCodex,
            OPENAI_CODEX_API_ENDPOINT,
            true
        ));
    }

    #[test]
    fn compaction_item_validation_is_shared_by_explicit_and_managed_paths() {
        let valid = serde_json::json!({
            "type": "compaction",
            "id": "cmp_1",
            "encrypted_content": "opaque",
            "future_field": { "preserved": true }
        });
        assert!(valid_compaction_item(&valid));

        for invalid in [
            serde_json::json!({ "type": "message", "id": "cmp_1", "encrypted_content": "opaque" }),
            serde_json::json!({ "type": "compaction", "id": "", "encrypted_content": "opaque" }),
            serde_json::json!({ "type": "compaction", "id": "cmp_1", "encrypted_content": "" }),
        ] {
            assert!(!valid_compaction_item(&invalid));
        }
    }
}
