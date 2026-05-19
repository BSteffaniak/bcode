#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Fake model provider plugin for deterministic tests and smoke flows.

use bcode_model::{
    AckResponse, CancelTurnRequest, ContentBlock, FinishTurnRequest, MODEL_PROVIDER_INTERFACE_ID,
    MessageRole, ModelCapability, ModelInfo, ModelList, ModelMessage, ModelTurnRequest,
    OP_CANCEL_TURN, OP_CAPABILITIES, OP_FINISH_TURN, OP_MODELS, OP_POLL_TURN_EVENTS, OP_START_TURN,
    OP_VALIDATE_CONFIG, PollTurnEventsRequest, PollTurnEventsResponse, ProviderCapabilities,
    ProviderCapability, ProviderTurnEvent, StartTurnResponse, StopReason, TokenUsage, ToolCall,
    ValidateConfigResponse,
};
use bcode_plugin_sdk::prelude::*;
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Deterministic fake model provider.
#[derive(Default)]
pub struct FakeProviderPlugin {
    next_turn: u64,
    turns: BTreeMap<String, FakeTurn>,
}

#[derive(Debug, Clone, Default)]
struct FakeTurn {
    events: Arc<Mutex<VecDeque<ProviderTurnEvent>>>,
    cancelled: Arc<AtomicBool>,
}

impl FakeTurn {
    fn push(&self, event: ProviderTurnEvent) {
        if let Ok(mut events) = self.events.lock() {
            events.push_back(event);
        }
    }

    fn drain(&self) -> Vec<ProviderTurnEvent> {
        self.events
            .lock()
            .map_or_else(|_| Vec::new(), |mut events| events.drain(..).collect())
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.push(ProviderTurnEvent::Cancelled);
        self.push(ProviderTurnEvent::TurnFinished {
            stop_reason: StopReason::Cancelled,
        });
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
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
                metadata: std::collections::BTreeMap::new(),
            }),
            OP_START_TURN => self.start_turn(&context.request),
            OP_POLL_TURN_EVENTS => self.poll_turn_events(&context.request),
            OP_CANCEL_TURN => self.cancel_turn(&context.request),
            OP_FINISH_TURN => self.finish_turn(&context.request),
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
        let user_text = last_user_text(&request.messages);
        let tool_result = last_tool_result(&request.messages);
        let tool_call = if tool_result.is_none() {
            fake_tool_call(&user_text, self.next_turn)
        } else {
            None
        };
        let text = tool_result.map_or_else(
            || format!("fake: {user_text}"),
            |result| format!("fake tool result: {result}"),
        );
        let turn = FakeTurn::default();
        turn.push(ProviderTurnEvent::TurnStarted);
        self.turns.insert(provider_turn_id.clone(), turn.clone());
        if let Some(tool_call) = tool_call {
            finish_fake_tool_turn(&turn, tool_call);
        } else if let Some(delay) = fake_delay() {
            std::thread::spawn(move || FakeTurnWorker { turn, text, delay }.run());
        } else {
            finish_fake_turn(&turn, text);
        }
        json_response(&StartTurnResponse { provider_turn_id })
    }

    fn poll_turn_events(&self, request: &ServiceRequest) -> ServiceResponse {
        let request = match request.payload_json::<PollTurnEventsRequest>() {
            Ok(request) => request,
            Err(error) => return invalid_request(&error),
        };
        let events = self
            .turns
            .get(&request.provider_turn_id)
            .map_or_else(Vec::new, FakeTurn::drain);
        json_response(&PollTurnEventsResponse { events })
    }

    fn cancel_turn(&self, request: &ServiceRequest) -> ServiceResponse {
        let request = match request.payload_json::<CancelTurnRequest>() {
            Ok(request) => request,
            Err(error) => return invalid_request(&error),
        };
        if let Some(turn) = self.turns.get(&request.provider_turn_id) {
            turn.cancel();
        }
        json_response(&AckResponse::default())
    }

    fn finish_turn(&mut self, request: &ServiceRequest) -> ServiceResponse {
        let request = match request.payload_json::<FinishTurnRequest>() {
            Ok(request) => request,
            Err(error) => return invalid_request(&error),
        };
        if let Some(turn) = self.turns.remove(&request.provider_turn_id) {
            turn.cancel();
        }
        json_response(&AckResponse::default())
    }
}

struct FakeTurnWorker {
    turn: FakeTurn,
    text: String,
    delay: Duration,
}

impl FakeTurnWorker {
    fn run(self) {
        std::thread::sleep(self.delay);
        if !self.turn.is_cancelled() {
            finish_fake_turn(&self.turn, self.text);
        }
    }
}

fn finish_fake_turn(turn: &FakeTurn, text: String) {
    let output_tokens = u32::try_from(text.split_whitespace().count()).unwrap_or(u32::MAX);
    turn.push(ProviderTurnEvent::TextDelta { text });
    turn.push(ProviderTurnEvent::Usage {
        usage: TokenUsage {
            input_tokens: Some(1),
            output_tokens: Some(output_tokens),
            total_tokens: Some(output_tokens.saturating_add(1)),
            ..TokenUsage::default()
        },
    });
    turn.push(ProviderTurnEvent::TurnFinished {
        stop_reason: StopReason::EndTurn,
    });
}

fn finish_fake_tool_turn(turn: &FakeTurn, call: ToolCall) {
    turn.push(ProviderTurnEvent::ToolCallStarted {
        call_id: call.id.clone(),
        name: call.name.clone(),
    });
    turn.push(ProviderTurnEvent::ToolCallFinished { call });
    turn.push(ProviderTurnEvent::TurnFinished {
        stop_reason: StopReason::ToolCall,
    });
}

fn fake_delay() -> Option<Duration> {
    let millis = std::env::var("BCODE_FAKE_PROVIDER_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())?;
    if millis == 0 {
        None
    } else {
        Some(Duration::from_millis(millis))
    }
}

fn capabilities() -> ProviderCapabilities {
    ProviderCapabilities {
        provider_id: "bcode.fake-provider".to_string(),
        display_name: "Bcode Fake Provider".to_string(),
        capabilities: [
            ProviderCapability::Streaming,
            ProviderCapability::Tools,
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
            capabilities: [ModelCapability::StreamingText, ModelCapability::ToolCalls]
                .into_iter()
                .collect(),
        }],
    }
}

fn fake_tool_call(user_text: &str, next_turn: u64) -> Option<ToolCall> {
    if let Some(path) = user_text.strip_prefix("tool-read ") {
        return Some(ToolCall {
            id: format!("fake-tool-{next_turn}"),
            name: "filesystem.read".to_string(),
            arguments: serde_json::json!({ "path": path }),
        });
    }
    if let Some(rest) = user_text.strip_prefix("tool-write ") {
        let (path, contents) = rest.split_once(' ').unwrap_or((rest, "fake write"));
        return Some(ToolCall {
            id: format!("fake-tool-{next_turn}"),
            name: "filesystem.write".to_string(),
            arguments: serde_json::json!({ "path": path, "contents": contents }),
        });
    }
    if let Some(command) = user_text.strip_prefix("tool-shell ") {
        return Some(ToolCall {
            id: format!("fake-tool-{next_turn}"),
            name: "shell.run".to_string(),
            arguments: serde_json::json!({ "command": command }),
        });
    }
    None
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

fn last_tool_result(messages: &[ModelMessage]) -> Option<String> {
    messages.iter().rev().find_map(|message| {
        if message.role != MessageRole::Tool {
            return None;
        }
        message.content.iter().find_map(|block| match block {
            ContentBlock::ToolResult { result } => Some(result.output.clone()),
            _ => None,
        })
    })
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
