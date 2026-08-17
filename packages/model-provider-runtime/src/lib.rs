#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Shared turn lifecycle support for native model provider plugins.

mod conformance;
pub use conformance::{
    ProviderConformanceCase, ProviderConformanceError, ProviderConformanceOptions,
    ProviderConformanceOutcome, ProviderConformanceReport, ProviderEventSummary,
    ProviderEventValidator, run_provider_conformance_suite,
};

use bcode_model::{
    ProviderError, ProviderErrorCategory, ProviderOutputEvent, ProviderRetryHint,
    ProviderTurnEvent, StructuredOutputRequest, ToolCall, ToolDefinition, TurnOutputPosition,
};
use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::{Notify, oneshot};

/// Provider-local helper for enforcing one structured result through a synthetic tool.
///
/// The synthetic tool is never a host tool: adapters construct it only for the provider request
/// and intercept its completed call before emitting normalized provider events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntheticStructuredOutput {
    tool_name: String,
    output_name: String,
    schema: serde_json::Value,
}

impl SyntheticStructuredOutput {
    /// Construct a provider-local structured-output constraint.
    ///
    /// # Errors
    ///
    /// Returns an error when real tools are present, because forcing the synthetic tool would
    /// suppress or compete with authorized host tool execution.
    pub fn new(
        request: &StructuredOutputRequest,
        has_real_tools: bool,
    ) -> Result<Self, ProviderError> {
        if has_real_tools {
            return Err(simple_provider_error(
                "structured_output_emulation_requires_tool_free_round",
                ProviderErrorCategory::UnsupportedFeature,
                "structured-output emulation requires a tool-free provider round",
            ));
        }
        Ok(Self {
            tool_name: format!("bcode_structured_{}", request.name),
            output_name: request.name.clone(),
            schema: request.schema.clone(),
        })
    }

    /// Return whether a provider tool name identifies this synthetic result tool.
    #[must_use]
    pub fn matches_tool_name(&self, name: &str) -> bool {
        name == self.tool_name
    }

    /// Return the provider-facing synthetic tool definition.
    #[must_use]
    pub fn tool(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.tool_name.clone(),
            description: format!("Return the final {} value", self.output_name),
            input_schema: self.schema.clone(),
        }
    }

    /// Convert the intercepted synthetic call into portable assistant JSON text.
    ///
    /// # Errors
    ///
    /// Returns an error when the call targets another tool or its arguments cannot be serialized.
    pub fn completed_text(&self, call: &ToolCall) -> Result<String, ProviderError> {
        if call.name != self.tool_name {
            return Err(simple_provider_error(
                "unexpected_structured_output_tool",
                ProviderErrorCategory::InvalidRequest,
                format!(
                    "expected synthetic tool '{}', got '{}'",
                    self.tool_name, call.name
                ),
            ));
        }
        serde_json::to_string(&call.arguments).map_err(|error| {
            simple_provider_error(
                "invalid_structured_output_arguments",
                ProviderErrorCategory::InvalidRequest,
                error.to_string(),
            )
        })
    }
}

fn simple_provider_error(
    code: &str,
    category: ProviderErrorCategory,
    message: impl Into<String>,
) -> ProviderError {
    ProviderError {
        code: code.to_string(),
        category,
        message: message.into(),
        retryable: false,
        provider_message: None,
        failure: None,
        request_id: None,
        diagnostic_context: Box::default(),
        sources: Box::default(),
        retry: None,
    }
}

const MAX_STRUCTURED_CANDIDATE_INPUT_BYTES: usize = 1024 * 1024;
const MAX_STRUCTURED_CANDIDATES: usize = 128;
const MAX_STRUCTURED_CANDIDATE_BYTES: usize = 256 * 1024;

/// Syntactic envelope in which a structured JSON candidate was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StructuredCandidateEnvelope {
    /// The complete trimmed model output was one JSON value.
    Exact,
    /// One complete `json` Markdown fence contained the value.
    JsonFence,
    /// One complete unlabeled Markdown fence contained the value.
    UnlabeledFence,
    /// A JSON object or array was embedded in model prose.
    Embedded,
}

/// One bounded parseable object or array discovered in model output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredJsonCandidate {
    /// Byte range in the original model output.
    pub range: std::ops::Range<usize>,
    /// Parsed JSON value.
    pub value: serde_json::Value,
    /// Canonical compact JSON used to deduplicate equivalent candidates.
    pub canonical_json: String,
    /// Syntactic envelope used to discover the value.
    pub envelope: StructuredCandidateEnvelope,
}

/// Catalog every bounded parseable JSON object or array in model output.
///
/// The cataloger never chooses a value and never interprets surrounding prose. It parses only
/// complete objects and arrays, bounds both work and memory, and records containment ranges so the
/// application can prefer maximal candidates after canonical schema validation.
///
/// # Errors
///
/// Returns an error when the response exceeds its byte bound, candidate enumeration exceeds its
/// bound, or a parsed candidate cannot be canonically encoded.
pub fn catalog_structured_json_candidates(
    text: &str,
) -> Result<Vec<StructuredJsonCandidate>, String> {
    if text.len() > MAX_STRUCTURED_CANDIDATE_INPUT_BYTES {
        return Err(format!(
            "structured output exceeds {MAX_STRUCTURED_CANDIDATE_INPUT_BYTES} bytes"
        ));
    }
    let mut candidates = Vec::new();
    let trimmed_start = text.len().saturating_sub(text.trim_start().len());
    let trimmed = text.trim();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed)
        && matches!(
            value,
            serde_json::Value::Object(_) | serde_json::Value::Array(_)
        )
    {
        push_structured_candidate(
            &mut candidates,
            trimmed_start..trimmed_start + trimmed.len(),
            value,
            StructuredCandidateEnvelope::Exact,
        )?;
    }
    catalog_fenced_candidates(text, &mut candidates)?;
    for (start, character) in text.char_indices() {
        if !matches!(character, '{' | '[') {
            continue;
        }
        if candidates.len() >= MAX_STRUCTURED_CANDIDATES {
            return Err(format!(
                "structured output exceeds {MAX_STRUCTURED_CANDIDATES} candidate starts"
            ));
        }
        let mut stream = serde_json::Deserializer::from_str(&text[start..]).into_iter();
        let Ok(Some(value)) = stream.next().transpose() else {
            continue;
        };
        let consumed = stream.byte_offset();
        if consumed == 0 || consumed > MAX_STRUCTURED_CANDIDATE_BYTES {
            continue;
        }
        if !matches!(
            value,
            serde_json::Value::Object(_) | serde_json::Value::Array(_)
        ) {
            continue;
        }
        if inside_non_json_fence(text, start) {
            continue;
        }
        push_structured_candidate(
            &mut candidates,
            start..start + consumed,
            value,
            StructuredCandidateEnvelope::Embedded,
        )?;
    }
    candidates.sort_by_key(|candidate| (candidate.range.start, candidate.range.end));
    candidates.dedup_by(|left, right| {
        left.range == right.range && left.canonical_json == right.canonical_json
    });
    Ok(candidates)
}

/// Select the uniquely strongest valid structured candidate.
///
/// Selection prefers an exact whole response, then a complete JSON/unlabeled fence, then a maximal
/// embedded value. Equivalent canonical values are deduplicated. Distinct equally strong values
/// fail closed rather than using prose, position, provider identity, or renderer metadata as a
/// semantic tie-breaker.
///
/// # Errors
///
/// Returns an error when no candidate satisfies `is_valid` or multiple distinct strongest values
/// remain after deterministic structural ranking and deduplication.
pub fn select_structured_json_candidate(
    candidates: &[StructuredJsonCandidate],
    mut is_valid: impl FnMut(&serde_json::Value) -> bool,
) -> Result<serde_json::Value, String> {
    let valid = candidates
        .iter()
        .filter(|candidate| is_valid(&candidate.value))
        .collect::<Vec<_>>();
    if valid.is_empty() {
        return Err("structured output contains no schema-valid JSON candidate".to_string());
    }
    let maximal = valid
        .iter()
        .copied()
        .filter(|candidate| {
            candidate.envelope != StructuredCandidateEnvelope::Embedded
                || !valid.iter().any(|other| {
                    other.envelope == StructuredCandidateEnvelope::Embedded
                        && other.range.start <= candidate.range.start
                        && other.range.end >= candidate.range.end
                        && other.range != candidate.range
                })
        })
        .collect::<Vec<_>>();
    let Some(best_rank) = maximal
        .iter()
        .map(|candidate| structured_candidate_rank(candidate.envelope))
        .max()
    else {
        return Err("structured output contains no maximal schema-valid candidate".to_string());
    };
    let mut best = maximal
        .into_iter()
        .filter(|candidate| structured_candidate_rank(candidate.envelope) == best_rank)
        .collect::<Vec<_>>();
    best.sort_by(|left, right| left.canonical_json.cmp(&right.canonical_json));
    best.dedup_by(|left, right| left.canonical_json == right.canonical_json);
    if best.len() != 1 {
        return Err(format!(
            "structured output is ambiguous: {} distinct strongest candidates match the schema",
            best.len()
        ));
    }
    Ok(best[0].value.clone())
}

/// Catalog and select one structured candidate with deterministic ambiguity handling.
///
/// Primitive JSON contracts are intentionally unsupported for embedded scanning because arbitrary
/// prose contains too many ambiguous primitive tokens. Callers that need primitive output should
/// require exact JSON at their own boundary.
///
/// # Errors
///
/// Returns cataloging errors and the selection errors documented by
/// [`select_structured_json_candidate`].
pub fn extract_structured_json_candidate(
    text: &str,
    mut is_valid: impl FnMut(&serde_json::Value) -> bool,
) -> Result<serde_json::Value, String> {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text.trim())
        && is_structured_result_shape(&value)
        && is_valid(&value)
    {
        return Ok(value);
    }
    let candidates = catalog_structured_json_candidates(text)?;
    select_structured_json_candidate(&candidates, is_valid)
}

const fn is_structured_result_shape(value: &serde_json::Value) -> bool {
    matches!(
        value,
        serde_json::Value::Object(_) | serde_json::Value::Array(_)
    )
}

const fn structured_candidate_rank(envelope: StructuredCandidateEnvelope) -> u8 {
    match envelope {
        StructuredCandidateEnvelope::Exact => 3,
        StructuredCandidateEnvelope::JsonFence | StructuredCandidateEnvelope::UnlabeledFence => 2,
        StructuredCandidateEnvelope::Embedded => 1,
    }
}

fn push_structured_candidate(
    candidates: &mut Vec<StructuredJsonCandidate>,
    range: std::ops::Range<usize>,
    value: serde_json::Value,
    envelope: StructuredCandidateEnvelope,
) -> Result<(), String> {
    if range.len() > MAX_STRUCTURED_CANDIDATE_BYTES {
        return Ok(());
    }
    let canonical_json = serde_json::to_string(&value)
        .map_err(|error| format!("failed to canonicalize structured candidate: {error}"))?;
    candidates.push(StructuredJsonCandidate {
        range,
        value,
        canonical_json,
        envelope,
    });
    Ok(())
}

fn inside_non_json_fence(text: &str, candidate_start: usize) -> bool {
    let prefix = &text[..candidate_start];
    let Some(open) = prefix.rfind("```") else {
        return false;
    };
    if prefix[open + 3..].contains("```") {
        return false;
    }
    let Some(newline) = text[open + 3..].find('\n') else {
        return false;
    };
    let language = text[open + 3..open + 3 + newline].trim();
    !language.is_empty() && !language.eq_ignore_ascii_case("json")
}

fn catalog_fenced_candidates(
    text: &str,
    candidates: &mut Vec<StructuredJsonCandidate>,
) -> Result<(), String> {
    let mut offset = 0;
    while let Some(relative_open) = text[offset..].find("```") {
        let open = offset + relative_open;
        let after_marker = open + 3;
        let Some(relative_newline) = text[after_marker..].find('\n') else {
            break;
        };
        let newline = after_marker + relative_newline;
        let language = text[after_marker..newline].trim();
        let body_start = newline + 1;
        let Some(relative_close) = text[body_start..].find("```") else {
            break;
        };
        let close = body_start + relative_close;
        if (language.is_empty() || language.eq_ignore_ascii_case("json"))
            && let Ok(value) =
                serde_json::from_str::<serde_json::Value>(text[body_start..close].trim())
            && matches!(
                value,
                serde_json::Value::Object(_) | serde_json::Value::Array(_)
            )
        {
            push_structured_candidate(
                candidates,
                body_start..close,
                value,
                if language.is_empty() {
                    StructuredCandidateEnvelope::UnlabeledFence
                } else {
                    StructuredCandidateEnvelope::JsonFence
                },
            )?;
        }
        offset = close + 3;
    }
    Ok(())
}

/// Outcome from a provider streaming turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamOutcome {
    /// The model finished with a normal assistant response.
    Finished,
    /// The model requested one or more tool calls.
    ToolCall,
    /// The model exhausted its output token budget before finishing.
    ///
    /// Distinct from [`Self::ToolCall`] because a truncated turn may have started a tool call
    /// without completing it, so no complete tool call is available to execute.
    MaxTokens,
    /// The turn was cancelled by the host.
    Cancelled,
}

/// Allocates stable monotonic positions for legacy semantic provider events.
#[derive(Debug, Clone, Default)]
pub struct ProviderOutputPositionAllocator {
    next_position: u64,
    active_text_position: Option<TurnOutputPosition>,
    reasoning_positions: BTreeMap<String, TurnOutputPosition>,
    tool_positions: BTreeMap<String, TurnOutputPosition>,
}

impl ProviderOutputPositionAllocator {
    /// Convert a legacy semantic event to a positioned v2 envelope.
    ///
    /// Already-positioned and non-semantic lifecycle events pass through unchanged.
    #[must_use]
    pub fn position(&mut self, event: ProviderTurnEvent) -> ProviderTurnEvent {
        match event {
            ProviderTurnEvent::Output { position, event } => {
                self.next_position = self.next_position.max(position.get().saturating_add(1));
                self.remember_position(position, &event);
                ProviderTurnEvent::Output { position, event }
            }
            ProviderTurnEvent::TextDelta { text } => {
                let position = self
                    .active_text_position
                    .unwrap_or_else(|| self.allocate_text_position());
                ProviderTurnEvent::output(position, ProviderOutputEvent::TextDelta { text })
            }
            ProviderTurnEvent::ReasoningActivity { event } => {
                self.active_text_position = None;
                let activity_id = event.activity_id().to_owned();
                let position = self
                    .reasoning_positions
                    .get(&activity_id)
                    .copied()
                    .unwrap_or_else(|| {
                        let position = self.allocate();
                        self.reasoning_positions.insert(activity_id, position);
                        position
                    });
                ProviderTurnEvent::output(
                    position,
                    ProviderOutputEvent::ReasoningActivity { event },
                )
            }
            ProviderTurnEvent::ToolCallStarted { call_id, name } => {
                self.active_text_position = None;
                let position = self.tool_position(&call_id);
                ProviderTurnEvent::output(
                    position,
                    ProviderOutputEvent::ToolCallStarted { call_id, name },
                )
            }
            ProviderTurnEvent::ToolCallDelta { call_id, delta } => {
                self.active_text_position = None;
                let position = self.tool_position(&call_id);
                ProviderTurnEvent::output(
                    position,
                    ProviderOutputEvent::ToolCallDelta { call_id, delta },
                )
            }
            ProviderTurnEvent::ToolCallFinished { call } => {
                self.active_text_position = None;
                let position = self.tool_position(&call.id);
                ProviderTurnEvent::output(position, ProviderOutputEvent::ToolCallFinished { call })
            }
            event => event,
        }
    }

    const fn allocate_text_position(&mut self) -> TurnOutputPosition {
        let position = self.allocate();
        self.active_text_position = Some(position);
        position
    }

    fn tool_position(&mut self, call_id: &str) -> TurnOutputPosition {
        self.tool_positions
            .get(call_id)
            .copied()
            .unwrap_or_else(|| {
                let position = self.allocate();
                self.tool_positions.insert(call_id.to_owned(), position);
                position
            })
    }

    const fn allocate(&mut self) -> TurnOutputPosition {
        let position = TurnOutputPosition::new(self.next_position);
        self.next_position = self.next_position.saturating_add(1);
        position
    }

    fn remember_position(&mut self, position: TurnOutputPosition, event: &ProviderOutputEvent) {
        match event {
            ProviderOutputEvent::TextDelta { .. } => self.active_text_position = Some(position),
            ProviderOutputEvent::ReasoningActivity { event } => {
                self.active_text_position = None;
                self.reasoning_positions
                    .insert(event.activity_id().to_owned(), position);
            }
            ProviderOutputEvent::ToolCallStarted { call_id, .. }
            | ProviderOutputEvent::ToolCallDelta { call_id, .. } => {
                self.active_text_position = None;
                self.tool_positions.insert(call_id.clone(), position);
            }
            ProviderOutputEvent::ToolCallFinished { call } => {
                self.active_text_position = None;
                self.tool_positions.insert(call.id.clone(), position);
            }
        }
    }
}

/// Queued event/cancellation state for one provider turn.
#[derive(Debug, Clone, Default)]
pub struct TurnState {
    events: Arc<Mutex<VecDeque<ProviderTurnEvent>>>,
    output_positions: Arc<Mutex<ProviderOutputPositionAllocator>>,
    positioned_output: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    cancel_notify: Arc<Notify>,
}

impl TurnState {
    /// Queue a provider event for the host to poll.
    pub fn push(&self, event: ProviderTurnEvent) {
        let event = if self.positioned_output.load(Ordering::Acquire) {
            match self.output_positions.lock() {
                Ok(mut positions) => positions.position(event),
                Err(_) => event,
            }
        } else {
            event
        };
        if let Ok(mut events) = self.events.lock() {
            events.push_back(event);
        }
    }

    /// Enable positioned v2 semantic output for this turn.
    pub fn enable_positioned_output(&self) {
        self.positioned_output.store(true, Ordering::Release);
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

const REDACTED_DIAGNOSTIC: &str = "[REDACTED]";
const MAX_PROVIDER_DIAGNOSTIC_CHARS: usize = 4_096;
const SENSITIVE_DIAGNOSTIC_KEYS: &[&str] = &[
    "authorization",
    "api_key",
    "apikey",
    "x-api-key",
    "access_token",
    "refresh_token",
    "client_secret",
    "password",
    "passwd",
    "secret",
    "secret_access_key",
    "aws_secret_access_key",
    "session_token",
    "aws_session_token",
];

/// Redact credential-shaped values from an upstream provider diagnostic and bound its size.
///
/// Adapters must apply this before copying an upstream message into `ProviderError::message`,
/// `provider_message`, or a preserved source. The sanitizer recognizes common header, JSON, form,
/// query-string, URL-userinfo, bearer/basic-token, and AWS access-key shapes. It deliberately does
/// not promise reversible or lossless diagnostics.
#[must_use]
pub fn sanitize_provider_diagnostic(message: &str) -> String {
    let lower = message.to_ascii_lowercase();
    let mut spans = Vec::new();
    for key in SENSITIVE_DIAGNOSTIC_KEYS {
        collect_assignment_spans(message, &lower, key, &mut spans);
    }
    for scheme in ["bearer ", "basic "] {
        collect_scheme_spans(message, &lower, scheme, &mut spans);
    }
    collect_url_userinfo_spans(message, &mut spans);
    collect_aws_access_key_spans(message, &mut spans);
    let redacted = replace_spans(message, spans);
    truncate_diagnostic(&redacted)
}

fn collect_assignment_spans(
    message: &str,
    lower: &str,
    key: &str,
    spans: &mut Vec<(usize, usize)>,
) {
    let bytes = message.as_bytes();
    let mut search_from = 0;
    while let Some(relative) = lower[search_from..].find(key) {
        let start = search_from + relative;
        let key_end = start + key.len();
        search_from = key_end;
        if start > 0 && is_identifier_byte(bytes[start - 1]) {
            continue;
        }
        let mut cursor = key_end;
        if cursor < bytes.len() && matches!(bytes[cursor], b'\'' | b'"') {
            cursor += 1;
        }
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() || !matches!(bytes[cursor], b':' | b'=') {
            continue;
        }
        cursor += 1;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() {
            continue;
        }
        let (value_start, value_end) = assignment_value_span(bytes, cursor);
        if value_start < value_end {
            spans.push((value_start, value_end));
        }
    }
}

const fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
}

fn assignment_value_span(bytes: &[u8], cursor: usize) -> (usize, usize) {
    if matches!(bytes[cursor], b'\'' | b'"') {
        let quote = bytes[cursor];
        let mut end = cursor + 1;
        while end < bytes.len() {
            if bytes[end] == quote && bytes[end.saturating_sub(1)] != b'\\' {
                break;
            }
            end += 1;
        }
        return (cursor + 1, end);
    }
    let mut end = cursor;
    while end < bytes.len()
        && !bytes[end].is_ascii_whitespace()
        && !matches!(bytes[end], b'&' | b',' | b';' | b'}' | b']')
    {
        end += 1;
    }
    (cursor, end)
}

fn collect_scheme_spans(message: &str, lower: &str, scheme: &str, spans: &mut Vec<(usize, usize)>) {
    let bytes = message.as_bytes();
    let mut search_from = 0;
    while let Some(relative) = lower[search_from..].find(scheme) {
        let start = search_from + relative + scheme.len();
        let mut end = start;
        while end < bytes.len()
            && !bytes[end].is_ascii_whitespace()
            && !matches!(bytes[end], b',' | b';' | b'"' | b'\'')
        {
            end += 1;
        }
        if start < end {
            spans.push((start, end));
        }
        search_from = end.max(start + 1);
    }
}

fn collect_url_userinfo_spans(message: &str, spans: &mut Vec<(usize, usize)>) {
    let bytes = message.as_bytes();
    let mut search_from = 0;
    while let Some(relative) = message[search_from..].find("://") {
        let authority_start = search_from + relative + 3;
        let authority_end = bytes[authority_start..]
            .iter()
            .position(|byte| matches!(byte, b'/' | b'?' | b'#' | b' ' | b'\n' | b'\r'))
            .map_or(bytes.len(), |offset| authority_start + offset);
        if let Some(at_offset) = bytes[authority_start..authority_end]
            .iter()
            .rposition(|byte| *byte == b'@')
        {
            let at = authority_start + at_offset;
            if let Some(colon_offset) = bytes[authority_start..at]
                .iter()
                .rposition(|byte| *byte == b':')
            {
                let password_start = authority_start + colon_offset + 1;
                if password_start < at {
                    spans.push((password_start, at));
                }
            }
        }
        search_from = authority_end.max(authority_start + 1);
    }
}

fn collect_aws_access_key_spans(message: &str, spans: &mut Vec<(usize, usize)>) {
    let bytes = message.as_bytes();
    for start in 0..bytes.len().saturating_sub(19) {
        if matches!(&bytes[start..start + 4], b"AKIA" | b"ASIA")
            && bytes[start + 4..start + 20]
                .iter()
                .all(u8::is_ascii_alphanumeric)
        {
            spans.push((start, start + 20));
        }
    }
}

fn replace_spans(message: &str, mut spans: Vec<(usize, usize)>) -> String {
    spans.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in spans {
        if let Some((_, previous_end)) = merged.last_mut()
            && start <= *previous_end
        {
            *previous_end = (*previous_end).max(end);
        } else {
            merged.push((start, end));
        }
    }
    let mut output = String::with_capacity(message.len());
    let mut cursor = 0;
    for (start, end) in merged {
        if !message.is_char_boundary(start) || !message.is_char_boundary(end) || start < cursor {
            continue;
        }
        output.push_str(&message[cursor..start]);
        output.push_str(REDACTED_DIAGNOSTIC);
        cursor = end;
    }
    output.push_str(&message[cursor..]);
    output
}

fn truncate_diagnostic(message: &str) -> String {
    let mut chars = message.chars();
    let prefix = chars
        .by_ref()
        .take(MAX_PROVIDER_DIAGNOSTIC_CHARS)
        .collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}…[TRUNCATED]")
    } else {
        prefix
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
        failure: None,
        request_id: None,
        diagnostic_context: Box::default(),
        sources: Box::default(),
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
        tool_call_policy: bcode_model::ToolCallRequestPolicy::default(),
        tool_schema_mode: None,
        structured_output: None,
        context_management: bcode_model::ContextManagementRequest::default(),
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

#[cfg(test)]
mod output_position_tests {
    use super::*;
    use bcode_model::{ProviderOutputEvent, ToolCall};

    #[test]
    fn structured_candidate_extraction_accepts_one_safe_envelope_and_rejects_ambiguity() {
        assert_eq!(
            extract_structured_json_candidate(" {\"ok\":true} ", |_| true).expect("exact"),
            serde_json::json!({"ok": true})
        );
        assert_eq!(
            extract_structured_json_candidate("```json\n{\"ok\":true}\n```", |_| true)
                .expect("JSON fence"),
            serde_json::json!({"ok": true})
        );
        assert_eq!(
            extract_structured_json_candidate("Result:\n{\"ok\":true}", |_| true)
                .expect("single embedded value"),
            serde_json::json!({"ok": true})
        );
        assert!(
            extract_structured_json_candidate(
                "```json\n{\"one\":1}\n```\n```json\n{\"two\":2}\n```",
                |_| true,
            )
            .is_err()
        );
        assert!(extract_structured_json_candidate("```rust\n{}\n```", |_| true).is_err());
        assert!(
            extract_structured_json_candidate("prefix {\"one\":1} {\"two\":2}", |_| true).is_err()
        );
    }

    #[test]
    fn allocator_keeps_semantic_units_stable_and_monotonic() {
        let mut allocator = ProviderOutputPositionAllocator::default();
        let events = [
            ProviderTurnEvent::TextDelta {
                text: "first ".to_owned(),
            },
            ProviderTurnEvent::TextDelta {
                text: "segment".to_owned(),
            },
            ProviderTurnEvent::ToolCallStarted {
                call_id: "call-1".to_owned(),
                name: "filesystem.read".to_owned(),
            },
            ProviderTurnEvent::ToolCallDelta {
                call_id: "call-1".to_owned(),
                delta: "{}".to_owned(),
            },
            ProviderTurnEvent::ToolCallFinished {
                call: ToolCall {
                    id: "call-1".to_owned(),
                    name: "filesystem.read".to_owned(),
                    arguments: serde_json::Value::Null,
                },
            },
            ProviderTurnEvent::TextDelta {
                text: "after tool".to_owned(),
            },
        ]
        .map(|event| allocator.position(event));

        let positions = events
            .iter()
            .filter_map(ProviderTurnEvent::positioned_output)
            .map(|(position, _)| position.get())
            .collect::<Vec<_>>();
        assert_eq!(positions, vec![0, 0, 1, 1, 1, 2]);
        assert!(matches!(
            &events[2],
            ProviderTurnEvent::Output {
                event: ProviderOutputEvent::ToolCallStarted { call_id, .. },
                ..
            } if call_id == "call-1"
        ));
    }

    #[test]
    fn synthetic_structured_output_intercepts_completed_call() {
        let request = StructuredOutputRequest {
            name: "answer".to_string(),
            schema: serde_json::json!({"type": "object"}),
            strict: true,
        };
        let helper = SyntheticStructuredOutput::new(&request, false)
            .expect("tool-free synthetic output should be supported");
        let tool = helper.tool();
        assert_eq!(tool.input_schema, request.schema);
        let text = helper
            .completed_text(&ToolCall {
                id: "synthetic-call".to_string(),
                name: tool.name,
                arguments: serde_json::json!({"ok": true}),
            })
            .expect("synthetic call should become text");
        assert_eq!(text, r#"{"ok":true}"#);
    }

    #[test]
    fn synthetic_structured_output_rejects_real_tools() {
        let request = StructuredOutputRequest {
            name: "answer".to_string(),
            schema: serde_json::json!({"type": "object"}),
            strict: true,
        };
        let error = SyntheticStructuredOutput::new(&request, true)
            .expect_err("real tools must not compete with the synthetic constraint");
        assert_eq!(
            error.code,
            "structured_output_emulation_requires_tool_free_round"
        );
        assert_eq!(error.category, ProviderErrorCategory::UnsupportedFeature);
    }

    #[test]
    fn allocator_preserves_explicit_positions_and_advances_after_them() {
        let mut allocator = ProviderOutputPositionAllocator::default();
        let explicit = allocator.position(ProviderTurnEvent::output(
            TurnOutputPosition::new(7),
            ProviderOutputEvent::TextDelta {
                text: "explicit".to_owned(),
            },
        ));
        let next = allocator.position(ProviderTurnEvent::ToolCallStarted {
            call_id: "call-1".to_owned(),
            name: "tool".to_owned(),
        });
        assert_eq!(
            explicit
                .positioned_output()
                .map(|(position, _)| position.get()),
            Some(7)
        );
        assert_eq!(
            next.positioned_output().map(|(position, _)| position.get()),
            Some(8)
        );
    }
}
