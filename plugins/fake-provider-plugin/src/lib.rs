#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Fake model provider plugin for deterministic tests and smoke flows.

use bcode_model::{
    AckResponse, ContentBlock, MODEL_PROVIDER_INTERFACE_ID, MessageRole, ModelCapability,
    ModelInfo, ModelList, ModelMessage, ModelTurnRequest, OP_CANCEL_TURN, OP_CAPABILITIES,
    OP_FINISH_TURN, OP_MODELS, OP_POLL_TURN_EVENTS, OP_START_TURN, OP_VALIDATE_CONFIG,
    PollTurnEventsRequest, PollTurnEventsResponse, ProviderCapabilities, ProviderCapability,
    ProviderTurnEvent, StartTurnResponse, StopReason, ValidateConfigResponse,
};
use bcode_plugin_sdk::prelude::*;
use std::collections::BTreeMap;

/// Deterministic fake model provider.
#[derive(Default)]
pub struct FakeProviderPlugin {
    next_turn: u64,
    turns: BTreeMap<String, Vec<ProviderTurnEvent>>,
}

impl RustPlugin for FakeProviderPlugin {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        if context.request.interface_id != MODEL_PROVIDER_INTERFACE_ID {
            return ServiceResponse::error(
                "unsupported_interface",
                "unsupported model provider service interface",
            );
        }

        match context.request.operation.as_str() {
            OP_CAPABILITIES => json_response(&capabilities()),
            OP_MODELS => json_response(&models()),
            OP_VALIDATE_CONFIG => json_response(&ValidateConfigResponse {
                valid: true,
                message: Some("fake provider is always valid".to_string()),
            }),
            OP_START_TURN => self.start_turn(&context.request),
            OP_POLL_TURN_EVENTS => self.poll_turn_events(&context.request),
            OP_CANCEL_TURN | OP_FINISH_TURN => json_response(&AckResponse::default()),
            _ => ServiceResponse::error(
                "unsupported_operation",
                "unsupported model provider operation",
            ),
        }
    }
}

impl FakeProviderPlugin {
    fn start_turn(&mut self, request: &ServiceRequest) -> ServiceResponse {
        let request = match request.payload_json::<ModelTurnRequest>() {
            Ok(request) => request,
            Err(error) => return invalid_request(&error),
        };
        self.next_turn += 1;
        let provider_turn_id = format!("fake-turn-{}", self.next_turn);
        let text = format!("fake: {}", last_user_text(&request.messages));
        self.turns.insert(
            provider_turn_id.clone(),
            vec![
                ProviderTurnEvent::TurnStarted,
                ProviderTurnEvent::TextDelta { text },
                ProviderTurnEvent::TurnFinished {
                    stop_reason: StopReason::EndTurn,
                },
            ],
        );
        json_response(&StartTurnResponse { provider_turn_id })
    }

    fn poll_turn_events(&mut self, request: &ServiceRequest) -> ServiceResponse {
        let request = match request.payload_json::<PollTurnEventsRequest>() {
            Ok(request) => request,
            Err(error) => return invalid_request(&error),
        };
        let events = self
            .turns
            .remove(&request.provider_turn_id)
            .unwrap_or_default();
        json_response(&PollTurnEventsResponse { events })
    }
}

fn capabilities() -> ProviderCapabilities {
    ProviderCapabilities {
        provider_id: "bcode.fake-provider".to_string(),
        display_name: "Bcode Fake Provider".to_string(),
        capabilities: [
            ProviderCapability::Streaming,
            ProviderCapability::Cancellation,
        ]
        .into_iter()
        .collect(),
        metadata: BTreeMap::new(),
    }
}

fn models() -> ModelList {
    ModelList {
        models: vec![ModelInfo {
            model_id: "fake-echo".to_string(),
            display_name: "Fake Echo".to_string(),
            is_default: true,
            context_window: Some(8_000),
            max_output_tokens: Some(1_000),
            capabilities: std::iter::once(ModelCapability::StreamingText).collect(),
        }],
    }
}

fn last_user_text(messages: &[ModelMessage]) -> String {
    messages
        .iter()
        .rev()
        .find(|message| message.role == MessageRole::User)
        .and_then(|message| {
            message.content.iter().find_map(|block| match block {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
        })
        .unwrap_or_default()
}

fn json_response<T: serde::Serialize>(value: &T) -> ServiceResponse {
    match ServiceResponse::json(value) {
        Ok(response) => response,
        Err(error) => ServiceResponse::error("encode_failed", error.to_string()),
    }
}

fn invalid_request(error: &serde_json::Error) -> ServiceResponse {
    ServiceResponse::error("invalid_request", error.to_string())
}

bcode_plugin_sdk::export_plugin!(FakeProviderPlugin, include_str!("../bcode-plugin.toml"));
