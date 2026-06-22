#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Shared turn lifecycle support for native model provider plugins.

use bcode_model::{ProviderError, ProviderErrorCategory, ProviderRetryHint, ProviderTurnEvent};
use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::{Notify, oneshot};

/// Outcome from a provider streaming turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamOutcome {
    /// The model finished with a normal assistant response.
    Finished,
    /// The model requested one or more tool calls.
    ToolCall,
    /// The turn was cancelled by the host.
    Cancelled,
}

/// Queued event/cancellation state for one provider turn.
#[derive(Debug, Clone, Default)]
pub struct TurnState {
    events: Arc<Mutex<VecDeque<ProviderTurnEvent>>>,
    cancelled: Arc<AtomicBool>,
    cancel_notify: Arc<Notify>,
}

impl TurnState {
    /// Queue a provider event for the host to poll.
    pub fn push(&self, event: ProviderTurnEvent) {
        if let Ok(mut events) = self.events.lock() {
            events.push_back(event);
        }
    }

    /// Drain currently queued provider events.
    #[must_use]
    pub fn drain(&self) -> Vec<ProviderTurnEvent> {
        self.events
            .lock()
            .map_or_else(|_| Vec::new(), |mut events| events.drain(..).collect())
    }

    /// Mark the turn as cancelled and wake stream workers.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.cancel_notify.notify_waiters();
    }

    /// Return true once the host has requested cancellation.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Notify fired when the host requests cancellation.
    #[must_use]
    pub fn cancel_notify(&self) -> Arc<Notify> {
        self.cancel_notify.clone()
    }
}

/// In-memory active-turn store used by synchronous plugin entrypoints.
#[derive(Debug, Default)]
pub struct TurnStore {
    next_turn: u64,
    turns: BTreeMap<String, TurnState>,
}

impl TurnStore {
    /// Insert a new turn and return its provider turn id and state.
    pub fn insert_started(&mut self, id_prefix: &str) -> (String, TurnState) {
        self.next_turn += 1;
        let provider_turn_id = format!("{id_prefix}-{}", self.next_turn);
        let turn = TurnState::default();
        turn.push(ProviderTurnEvent::TurnStarted);
        self.turns.insert(provider_turn_id.clone(), turn.clone());
        (provider_turn_id, turn)
    }

    /// Drain queued events for a provider turn.
    #[must_use]
    pub fn drain(&self, provider_turn_id: &str) -> Vec<ProviderTurnEvent> {
        self.turns
            .get(provider_turn_id)
            .map_or_else(Vec::new, TurnState::drain)
    }

    /// Cancel a provider turn if it is active.
    pub fn cancel(&self, provider_turn_id: &str) {
        if let Some(turn) = self.turns.get(provider_turn_id) {
            turn.cancel();
        }
    }

    /// Cancel and remove a provider turn from the active store.
    pub fn finish(&mut self, provider_turn_id: &str) {
        if let Some(turn) = self.turns.remove(provider_turn_id) {
            turn.cancel();
        }
    }
}

/// Build a normalized provider error.
#[must_use]
pub fn provider_error(
    code: impl Into<String>,
    category: ProviderErrorCategory,
    message: impl Into<String>,
) -> ProviderError {
    ProviderError {
        code: code.into(),
        category,
        message: message.into(),
        retryable: matches!(
            category,
            ProviderErrorCategory::Network
                | ProviderErrorCategory::Timeout
                | ProviderErrorCategory::RateLimit
                | ProviderErrorCategory::ProviderInternal
                | ProviderErrorCategory::Overloaded
        ),
        provider_message: None,
        retry: None,
    }
}

/// Extract retry timing metadata from provider HTTP headers and an optional body.
#[must_use]
pub fn retry_hint_from_response_parts(
    headers: &BTreeMap<String, String>,
    body: Option<&str>,
) -> Option<ProviderRetryHint> {
    retry_hint_from_headers(headers).or_else(|| body.and_then(retry_hint_from_body))
}

/// Extract retry timing metadata from provider HTTP headers.
#[must_use]
pub fn retry_hint_from_headers(headers: &BTreeMap<String, String>) -> Option<ProviderRetryHint> {
    let headers = normalized_headers(headers);
    headers
        .get("retry-after-ms")
        .and_then(|value| parse_millis_value(value))
        .map_or_else(
            || {
                headers
                    .get("retry-after")
                    .and_then(|value| parse_retry_after_value(value, "retry-after"))
                    .or_else(|| x_ratelimit_reset_hint_from_values(&headers))
                    .or_else(|| anthropic_ratelimit_reset_hint(&headers))
                    .or_else(|| codex_window_hint_from_values(&headers))
            },
            |milliseconds| {
                Some(ProviderRetryHint {
                    retry_after_ms: Some(milliseconds),
                    retry_at_unix: Some(
                        unix_timestamp().saturating_add(milliseconds.div_ceil(1_000)),
                    ),
                    source: Some("retry-after-ms".to_string()),
                })
            },
        )
}

fn normalized_headers(headers: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    headers
        .iter()
        .map(|(key, value)| (key.to_ascii_lowercase(), value.trim().to_string()))
        .collect()
}

/// Extract retry timing metadata from a JSON response body.
#[must_use]
pub fn retry_hint_from_body(body: &str) -> Option<ProviderRetryHint> {
    let value = serde_json::from_str::<serde_json::Value>(body).ok()?;
    retry_hint_from_json_value(&value)
}

/// Extract retry timing metadata from a JSON response/event value.
#[must_use]
pub fn retry_hint_from_json_value(value: &serde_json::Value) -> Option<ProviderRetryHint> {
    retry_hint_from_json_headers(value).or_else(|| {
        find_json_reset_value(value).map(|retry_at_unix| ProviderRetryHint {
            retry_after_ms: retry_at_unix
                .saturating_sub(unix_timestamp())
                .checked_mul(1_000),
            retry_at_unix: Some(retry_at_unix),
            source: Some("body".to_string()),
        })
    })
}

fn retry_hint_from_json_headers(value: &serde_json::Value) -> Option<ProviderRetryHint> {
    let headers = value.get("headers")?.as_object()?;
    let mut normalized = BTreeMap::new();
    for (key, value) in headers {
        if let Some(value) = header_json_value(value) {
            normalized.insert(key.to_ascii_lowercase(), value);
        }
    }
    retry_hint_from_headers(&normalized)
}

fn header_json_value(value: &serde_json::Value) -> Option<String> {
    value
        .as_str()
        .map(ToString::to_string)
        .or_else(|| value.as_u64().map(|number| number.to_string()))
        .or_else(|| value.as_i64().map(|number| number.to_string()))
        .or_else(|| value.as_bool().map(|boolean| boolean.to_string()))
}

fn parse_millis_value(value: &str) -> Option<u64> {
    value.parse::<u64>().ok()
}

fn x_ratelimit_reset_hint_from_values(
    headers: &BTreeMap<String, String>,
) -> Option<ProviderRetryHint> {
    headers.iter().find_map(|(name, value)| {
        name.strip_prefix("x-ratelimit-reset-")
            .filter(|suffix| !suffix.is_empty())
            .map_or_else(
                || (name == "x-ratelimit-reset").then_some(name.as_str()),
                |_| Some(name.as_str()),
            )
            .and_then(|source| reset_hint(value, source))
    })
}

fn anthropic_ratelimit_reset_hint(headers: &BTreeMap<String, String>) -> Option<ProviderRetryHint> {
    headers.iter().find_map(|(name, value)| {
        (name.starts_with("anthropic-ratelimit-") && name.ends_with("-reset"))
            .then(|| reset_hint(value, name))
            .flatten()
    })
}

fn codex_window_hint_from_values(headers: &BTreeMap<String, String>) -> Option<ProviderRetryHint> {
    headers
        .get("x-codex-primary-window-minutes")
        .and_then(|value| value.parse::<u64>().ok())
        .map(|minutes| minutes.saturating_mul(60))
        .map(|seconds| ProviderRetryHint {
            retry_after_ms: seconds.checked_mul(1_000),
            retry_at_unix: Some(unix_timestamp().saturating_add(seconds)),
            source: Some("x-codex-primary-window-minutes".to_string()),
        })
}

fn reset_hint(value: &str, source: &str) -> Option<ProviderRetryHint> {
    parse_reset_value(value).map(|retry_at_unix| ProviderRetryHint {
        retry_after_ms: retry_at_unix
            .saturating_sub(unix_timestamp())
            .checked_mul(1_000),
        retry_at_unix: Some(retry_at_unix),
        source: Some(source.to_string()),
    })
}

fn find_json_reset_value(value: &serde_json::Value) -> Option<u64> {
    match value {
        serde_json::Value::Object(map) => {
            for key in ["retry_after_ms", "retryAfterMs"] {
                if let Some(number) = map.get(key).and_then(serde_json::Value::as_u64) {
                    return Some(unix_timestamp().saturating_add(number.div_ceil(1_000)));
                }
            }
            for key in ["retry_after", "retryAfter", "reset_at", "resetAt"] {
                if let Some(value) = map.get(key)
                    && let Some(reset) = parse_json_reset_value(value)
                {
                    return Some(reset);
                }
            }
            map.values().find_map(find_json_reset_value)
        }
        serde_json::Value::Array(values) => values.iter().find_map(find_json_reset_value),
        _ => None,
    }
}

fn parse_json_reset_value(value: &serde_json::Value) -> Option<u64> {
    value
        .as_u64()
        .map(|seconds| unix_timestamp().saturating_add(seconds))
        .or_else(|| value.as_str().and_then(parse_reset_value))
}

fn parse_reset_value(value: &str) -> Option<u64> {
    parse_duration_seconds(value).map_or_else(
        || {
            value.parse::<u64>().ok().map_or_else(
                || {
                    httpdate::parse_http_date(value)
                        .ok()
                        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|duration| duration.as_secs())
                },
                |number| {
                    if number > 2_000_000_000 {
                        Some(number)
                    } else {
                        Some(unix_timestamp().saturating_add(number))
                    }
                },
            )
        },
        |seconds| Some(unix_timestamp().saturating_add(seconds)),
    )
}

fn parse_retry_after_value(value: &str, source: &str) -> Option<ProviderRetryHint> {
    parse_seconds_value(value)
        .or_else(|| parse_duration_seconds(value))
        .map(|seconds| ProviderRetryHint {
            retry_after_ms: seconds.checked_mul(1_000),
            retry_at_unix: Some(unix_timestamp().saturating_add(seconds)),
            source: Some(source.to_string()),
        })
        .or_else(|| {
            httpdate::parse_http_date(value)
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs())
                .map(|retry_at_unix| ProviderRetryHint {
                    retry_after_ms: retry_at_unix
                        .saturating_sub(unix_timestamp())
                        .checked_mul(1_000),
                    retry_at_unix: Some(retry_at_unix),
                    source: Some(source.to_string()),
                })
        })
}

fn parse_seconds_value(value: &str) -> Option<u64> {
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(seconds);
    }
    let (whole, fraction) = value.split_once('.')?;
    let seconds = whole.parse::<u64>().ok()?;
    if fraction.chars().any(|character| character != '0') {
        Some(seconds.saturating_add(1))
    } else {
        Some(seconds)
    }
}

fn parse_duration_seconds(value: &str) -> Option<u64> {
    let value = value.trim();
    if value.is_empty() || value.chars().all(|character| character.is_ascii_digit()) {
        return None;
    }
    let mut total_millis = 0_u64;
    let mut number = String::new();
    let mut parsed_unit = false;
    let mut chars = value.chars().peekable();
    while let Some(character) = chars.next() {
        if character.is_ascii_digit() || character == '.' {
            number.push(character);
            continue;
        }
        if character.is_whitespace() {
            continue;
        }
        let unit = if character == 'm' && chars.peek() == Some(&'s') {
            chars.next();
            "ms"
        } else if matches!(character, 'd' | 'h' | 'm' | 's') {
            match character {
                'd' => "d",
                'h' => "h",
                'm' => "m",
                's' => "s",
                _ => return None,
            }
        } else {
            return None;
        };
        let millis = duration_component_millis(&number, unit)?;
        total_millis = total_millis.saturating_add(millis);
        number.clear();
        parsed_unit = true;
    }
    if !number.is_empty() || !parsed_unit {
        return None;
    }
    Some(total_millis.div_ceil(1_000))
}

fn duration_component_millis(number: &str, unit: &str) -> Option<u64> {
    let (whole, fraction) = number
        .split_once('.')
        .map_or((number, ""), |(whole, fraction)| (whole, fraction));
    let whole = whole.parse::<u64>().ok()?;
    let multiplier = match unit {
        "ms" => 1,
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return None,
    };
    let whole_millis = whole.checked_mul(multiplier)?;
    if fraction.is_empty() {
        return Some(whole_millis);
    }
    let denominator = 10_u64.checked_pow(u32::try_from(fraction.len()).ok()?)?;
    let numerator = fraction.parse::<u64>().ok()?;
    Some(whole_millis.saturating_add(numerator.saturating_mul(multiplier) / denominator))
}

#[must_use]
fn unix_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}
/// Shared Tokio runtime for native model provider plugins.
///
/// The plugin service ABI is synchronous, but providers need async networking for
/// streaming turns, model discovery, and token refresh. This runtime keeps one
/// current-thread Tokio runtime alive on a dedicated background thread so plugins
/// can spawn long-lived async work without creating a new runtime per operation.
pub struct ProviderRuntime {
    handle: tokio::runtime::Handle,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl std::fmt::Debug for ProviderRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderRuntime")
            .finish_non_exhaustive()
    }
}

impl ProviderRuntime {
    /// Start a reusable provider runtime on a dedicated thread.
    ///
    /// # Errors
    ///
    /// Returns an error when the background thread or Tokio runtime cannot be
    /// created, or when the runtime thread exits before startup completes.
    pub fn new() -> Result<Self, ProviderRuntimeError> {
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();
        let thread = thread::Builder::new()
            .name("bcode-provider-runtime".to_string())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_io()
                    .enable_time()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready_sender.send(Err(error));
                        return;
                    }
                };
                let handle = runtime.handle().clone();
                if ready_sender.send(Ok(handle)).is_err() {
                    return;
                }
                runtime.block_on(async {
                    let _ = shutdown_receiver.await;
                });
            })
            .map_err(ProviderRuntimeError::ThreadSpawn)?;
        let handle = ready_receiver
            .recv()
            .map_err(|_| ProviderRuntimeError::StartupDropped)?
            .map_err(ProviderRuntimeError::RuntimeBuild)?;
        Ok(Self {
            handle,
            shutdown: Some(shutdown_sender),
            thread: Some(thread),
        })
    }

    /// Spawn async provider work onto the shared runtime.
    ///
    /// The returned handle may be dropped when the caller does not need the task
    /// result, such as provider turn streaming where completion is reported via
    /// queued provider events.
    pub fn spawn<F>(&self, future: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.handle.spawn(future)
    }

    /// Run an async operation to completion from synchronous plugin code.
    ///
    /// This schedules the future on the background runtime and waits for its
    /// result without constructing a throwaway runtime.
    ///
    /// # Errors
    ///
    /// Returns an error if the background runtime stops before the operation
    /// returns its result.
    pub fn block_on<F>(&self, future: F) -> Result<F::Output, ProviderRuntimeError>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let (sender, receiver) = mpsc::sync_channel(1);
        self.handle.spawn(async move {
            let output = future.await;
            let _ = sender.send(output);
        });
        receiver
            .recv()
            .map_err(|_| ProviderRuntimeError::TaskDropped)
    }
}

impl Drop for ProviderRuntime {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Errors returned by [`ProviderRuntime`].
#[derive(Debug)]
pub enum ProviderRuntimeError {
    /// Tokio runtime construction failed on the background thread.
    RuntimeBuild(std::io::Error),
    /// Runtime thread creation failed.
    ThreadSpawn(std::io::Error),
    /// Runtime thread exited before reporting startup success or failure.
    StartupDropped,
    /// A scheduled operation did not return a result before the runtime stopped.
    TaskDropped,
}

impl std::fmt::Display for ProviderRuntimeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RuntimeBuild(error) => write!(formatter, "runtime build failed: {error}"),
            Self::ThreadSpawn(error) => write!(formatter, "runtime thread spawn failed: {error}"),
            Self::StartupDropped => write!(formatter, "runtime thread exited during startup"),
            Self::TaskDropped => write!(formatter, "runtime task ended without returning a result"),
        }
    }
}

impl std::error::Error for ProviderRuntimeError {}

/// Request for a single provider model turn.
#[derive(Debug, Clone)]
pub struct SingleTurnRequest {
    pub provider_plugin_id: Option<String>,
    pub model_id: String,
    pub provider_context: bcode_model::ProviderRequestContext,
    pub prompt: String,
    pub system_prompt: Option<String>,
    pub parameters: bcode_model::ModelParameters,
    pub metadata: BTreeMap<String, String>,
    pub timeout: Duration,
}

/// Result of a single provider model turn.
#[derive(Debug, Clone)]
pub struct SingleTurnResult {
    pub status: SingleTurnStatus,
    pub text: String,
    pub latency_ms: u128,
    pub error: Option<bcode_model::ProviderError>,
}

/// Status of a single provider model turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SingleTurnStatus {
    Finished,
    Cancelled,
    Timeout,
    ProviderError,
}

/// Blocking provider invoker used by the reusable single-turn executor.
pub trait BlockingModelProviderInvoker {
    /// Invoke one typed model provider operation.
    ///
    /// # Errors
    ///
    /// Returns an error when provider routing, request encoding, service invocation, service
    /// response status, or response decoding fails.
    fn invoke_json<Q, R>(
        &mut self,
        provider_plugin_id: Option<&str>,
        operation: &'static str,
        request: &Q,
    ) -> Result<R, String>
    where
        Q: serde::Serialize,
        R: serde::de::DeserializeOwned;
}

/// Run a small single-turn provider request through the normal provider operation pipeline.
///
/// # Errors
///
/// Returns an error when provider service invocation fails before a provider turn can be
/// represented as a model result.
pub fn run_single_turn_blocking<I>(
    invoker: &mut I,
    request: SingleTurnRequest,
) -> Result<SingleTurnResult, String>
where
    I: BlockingModelProviderInvoker,
{
    let start = Instant::now();
    let session_id = bcode_session_models::SessionId::new();
    let turn_request = bcode_model::ModelTurnRequest {
        session_id,
        turn_id: format!("single-turn-{session_id}"),
        model_id: request.model_id,
        provider_context: request.provider_context,
        system_prompt: request.system_prompt,
        messages: vec![bcode_model::ModelMessage {
            role: bcode_model::MessageRole::User,
            content: vec![bcode_model::ContentBlock::Text {
                text: request.prompt,
            }],
        }],
        tools: Vec::new(),
        parameters: request.parameters,
        prompt_cache: bcode_model::PromptCacheHints::default(),
        conversation_reuse: bcode_model::ConversationReuseHints::default(),
        metadata: request.metadata,
    };
    let start_response: bcode_model::StartTurnResponse = invoker.invoke_json(
        request.provider_plugin_id.as_deref(),
        bcode_model::OP_START_TURN,
        &turn_request,
    )?;
    let mut text = String::new();
    let mut last_error = None;
    loop {
        if start.elapsed() >= request.timeout {
            finish_single_turn(
                invoker,
                request.provider_plugin_id.as_deref(),
                &start_response.provider_turn_id,
            );
            return Ok(SingleTurnResult {
                status: SingleTurnStatus::Timeout,
                text,
                latency_ms: start.elapsed().as_millis(),
                error: last_error,
            });
        }
        let poll: bcode_model::PollTurnEventsResponse = invoker.invoke_json(
            request.provider_plugin_id.as_deref(),
            bcode_model::OP_POLL_TURN_EVENTS,
            &bcode_model::PollTurnEventsRequest {
                provider_turn_id: start_response.provider_turn_id.clone(),
            },
        )?;
        for event in poll.events {
            match event {
                bcode_model::ProviderTurnEvent::TextDelta { text: delta } => text.push_str(&delta),
                bcode_model::ProviderTurnEvent::Error { error } => last_error = Some(error),
                bcode_model::ProviderTurnEvent::TurnFinished { .. } => {
                    finish_single_turn(
                        invoker,
                        request.provider_plugin_id.as_deref(),
                        &start_response.provider_turn_id,
                    );
                    return Ok(SingleTurnResult {
                        status: if last_error.is_some() {
                            SingleTurnStatus::ProviderError
                        } else {
                            SingleTurnStatus::Finished
                        },
                        text,
                        latency_ms: start.elapsed().as_millis(),
                        error: last_error,
                    });
                }
                bcode_model::ProviderTurnEvent::Cancelled => {
                    finish_single_turn(
                        invoker,
                        request.provider_plugin_id.as_deref(),
                        &start_response.provider_turn_id,
                    );
                    return Ok(SingleTurnResult {
                        status: SingleTurnStatus::Cancelled,
                        text,
                        latency_ms: start.elapsed().as_millis(),
                        error: last_error,
                    });
                }
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn finish_single_turn<I>(invoker: &mut I, provider_plugin_id: Option<&str>, provider_turn_id: &str)
where
    I: BlockingModelProviderInvoker,
{
    let _: Result<bcode_model::AckResponse, String> = invoker.invoke_json(
        provider_plugin_id,
        bcode_model::OP_FINISH_TURN,
        &bcode_model::FinishTurnRequest {
            provider_turn_id: provider_turn_id.to_string(),
        },
    );
}
