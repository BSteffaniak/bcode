#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
//! Scoped normalized text/tool model gateway client. Never handles upstream auth.

#[cfg(feature = "static-bundled")]
mod cli;
pub mod setup;

use bcode_model::{self as model, host_operation as wire};
use bcode_model_provider_runtime::ProviderRuntime;
use bcode_plugin_sdk::prelude::*;
use reqwest::{Client, Url};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const PROVIDER: &str = "bcode.auth-broker-provider";
const TOKEN_ENV: &str = "BCODE_AUTH_BROKER_TOKEN";
const LIMIT: usize = 1_048_576;
const MAX_TURNS: usize = 64;
type Failure = &'static str;

#[derive(Clone)]
struct Broker {
    url: Url,
    token: String,
    expires_at: u64,
}
impl std::fmt::Debug for Broker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Broker { <redacted> }")
    }
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.as_secs())
}
impl Broker {
    fn from_context(context: &model::ProviderRequestContext) -> Result<Self, Failure> {
        if context.auth.is_some() || !context.auth_candidates.is_empty() {
            return Err("broker_requires_opaque_lease_only");
        }
        let url = Url::parse(
            context
                .settings
                .get("broker_url")
                .ok_or("broker_url_required")?,
        )
        .map_err(|_| "invalid_broker_url")?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
        {
            return Err("invalid_broker_url");
        }
        if let (Some(lease_file), Some(expiry_file)) = (
            context.settings.get("broker_lease_file"),
            context.settings.get("broker_expiry_file"),
        ) {
            if context.env.contains_key(TOKEN_ENV)
                || context.env.contains_key("BCODE_AUTH_BROKER_EXPIRES_AT")
            {
                return Err("ambiguous_broker_lease");
            }
            let (token, expires_at) = setup::read_lease(lease_file, expiry_file)?;
            if expires_at <= now() {
                return Err("broker_lease_expired");
            }
            return Ok(Self {
                url,
                token,
                expires_at,
            });
        }
        if context.settings.contains_key("broker_lease_file")
            || context.settings.contains_key("broker_expiry_file")
        {
            return Err("invalid_private_file");
        }
        let token = context
            .env
            .get(TOKEN_ENV)
            .cloned()
            .or_else(|| std::env::var(TOKEN_ENV).ok())
            .ok_or("broker_lease_required")?;
        if token.is_empty()
            || token.len() > 16384
            || !token.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err("invalid_broker_lease");
        }
        let expires_at = context
            .env
            .get("BCODE_AUTH_BROKER_EXPIRES_AT")
            .cloned()
            .or_else(|| std::env::var("BCODE_AUTH_BROKER_EXPIRES_AT").ok())
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or("broker_expiry_required")?;
        if expires_at <= now() {
            return Err("broker_lease_expired");
        }
        Ok(Self {
            url,
            token,
            expires_at,
        })
    }
    async fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<T, Failure> {
        if self.expires_at <= now() {
            return Err("broker_lease_expired");
        }
        if body.as_ref().is_some_and(|body| body.len() > LIMIT) {
            return Err("broker_request_too_large");
        }
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| "broker_transport_unavailable")?;
        let mut request = client
            .post(self.url.join(path).map_err(|_| "invalid_broker_route")?)
            .bearer_auth(&self.token);
        if let Some(body) = body {
            request = request
                .header("content-type", "application/json")
                .body(body);
        }
        let mut response = request
            .send()
            .await
            .map_err(|_| "broker_transport_failed")?;
        if !response.status().is_success() {
            return Err("broker_request_rejected");
        }
        if response
            .content_length()
            .is_some_and(|size| size > LIMIT as u64)
        {
            return Err("broker_response_too_large");
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| "broker_transport_failed")?
        {
            if chunk.len() > LIMIT.saturating_sub(bytes.len()) {
                return Err("broker_response_too_large");
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| "invalid_broker_response")
    }
}
#[derive(Serialize)]
struct StartBody {
    request_id: String,
    messages: Vec<model::ModelMessage>,
    tools: Vec<model::ToolDefinition>,
    tool_call_policy: model::ToolCallRequestPolicy,
    tool_schema_mode: Option<model::ToolSchemaMode>,
    max_output_tokens: Option<u32>,
}
fn start_body(request: &model::ModelTurnRequest) -> Result<StartBody, Failure> {
    if request.structured_output.is_some()
        || !request.provider_context.request.is_empty()
        || request.conversation_reuse.mode.is_enabled()
    {
        return Err("broker_unsupported_content");
    }
    if request.parameters.temperature.is_some()
        || request.parameters.top_p.is_some()
        || !request.parameters.stop_sequences.is_empty()
        || request.parameters.reasoning_effort.is_some()
        || request.parameters.reasoning_budget_tokens.is_some()
        || request.parameters.reasoning_control.is_some()
        || request.parameters.reasoning_effort_value.is_some()
        || request.parameters.reasoning_summary.is_some()
    {
        return Err("broker_unsupported_parameters");
    }
    let mut messages = Vec::new();
    if let Some(text) = &request.system_prompt {
        messages.push(model::ModelMessage {
            role: model::MessageRole::System,
            content: vec![model::ContentBlock::Text { text: text.clone() }],
        });
    }
    messages.extend(request.messages.iter().cloned());
    let body = StartBody {
        request_id: request.turn_id.clone(),
        messages,
        tools: request.tools.clone(),
        tool_call_policy: request.tool_call_policy.clone(),
        tool_schema_mode: request.tool_schema_mode,
        max_output_tokens: request.parameters.max_output_tokens,
    };
    wire::StartRequest {
        scope_id: "client-validation".to_owned(),
        expires_at: now() + 60,
        request_id: body.request_id.clone(),
        messages: body.messages.clone(),
        tools: body.tools.clone(),
        tool_call_policy: body.tool_call_policy.clone(),
        tool_schema_mode: body.tool_schema_mode,
        max_output_tokens: body.max_output_tokens,
    }
    .validate_public()
    .map_err(|_| "broker_invalid_normalized_request")?;
    Ok(body)
}
fn turn_path(id: &str, operation: &str) -> Result<String, Failure> {
    if id.is_empty()
        || id.len() > 256
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("invalid_broker_turn");
    }
    Ok(format!("/v1/turns/{id}/{operation}"))
}
fn page_events(
    page: wire::PollResponse,
    cursor: u64,
) -> Result<(u64, Vec<model::ProviderTurnEvent>), Failure> {
    let mut sequence = cursor;
    let mut events = Vec::new();
    for entry in page.events {
        if entry.sequence != sequence.checked_add(1).ok_or("invalid_broker_cursor")? {
            return Err("invalid_broker_cursor");
        }
        sequence = entry.sequence;
        match entry.event {
            model::ProviderTurnEvent::TurnStarted
            | model::ProviderTurnEvent::TextDelta { .. }
            | model::ProviderTurnEvent::Usage { .. }
            | model::ProviderTurnEvent::ToolCallStarted { .. }
            | model::ProviderTurnEvent::ToolCallDelta { .. }
            | model::ProviderTurnEvent::ToolCallFinished { .. }
            | model::ProviderTurnEvent::TurnFinished { .. }
            | model::ProviderTurnEvent::Cancelled => events.push(entry.event),
            model::ProviderTurnEvent::Output { event, .. } => {
                events.push(match event {
                    model::ProviderOutputEvent::TextDelta { text } => {
                        model::ProviderTurnEvent::TextDelta { text }
                    }
                    model::ProviderOutputEvent::ToolCallStarted { call_id, name } => {
                        model::ProviderTurnEvent::ToolCallStarted { call_id, name }
                    }
                    model::ProviderOutputEvent::ToolCallDelta { call_id, delta } => {
                        model::ProviderTurnEvent::ToolCallDelta { call_id, delta }
                    }
                    model::ProviderOutputEvent::ToolCallFinished { call } => {
                        model::ProviderTurnEvent::ToolCallFinished { call }
                    }
                    model::ProviderOutputEvent::ReasoningActivity { .. } => {
                        return Err("broker_unsupported_event");
                    }
                });
            }
            model::ProviderTurnEvent::Error { .. } => return Err("broker_turn_failed"),
            _ => return Err("broker_unsupported_event"),
        }
    }
    if page.next_sequence != sequence {
        return Err("invalid_broker_cursor");
    }
    Ok((sequence, events))
}
struct Turn {
    broker: Broker,
    cursor: u64,
    busy: bool,
}
#[derive(Default)]
struct State {
    starts: BTreeMap<String, u64>,
    turns: BTreeMap<String, Turn>,
}
impl State {
    fn reserve_start(&mut self, id: &str, expiry: u64) -> Result<(), Failure> {
        self.starts.retain(|_, expiry| *expiry > now());
        if self.starts.contains_key(id) {
            return Err("broker_start_outcome_unverified");
        }
        if self.starts.len() + self.turns.len() >= MAX_TURNS {
            return Err("broker_turn_capacity");
        }
        self.starts.insert(id.to_owned(), expiry);
        Ok(())
    }
    fn claim(&mut self, id: &str) -> Result<(Broker, u64), Failure> {
        let turn = self.turns.get_mut(id).ok_or("unknown_broker_turn")?;
        if turn.busy {
            return Err("broker_turn_busy");
        }
        turn.busy = true;
        Ok((turn.broker.clone(), turn.cursor))
    }
    fn complete(&mut self, id: &str, cursor: Option<u64>, finished: bool) -> Result<(), Failure> {
        let turn = self.turns.get_mut(id).ok_or("unknown_broker_turn")?;
        turn.busy = false;
        if let Some(cursor) = cursor {
            turn.cursor = cursor;
        }
        if finished {
            self.turns.remove(id);
        }
        Ok(())
    }
}
fn tool_support() -> model::ModelFeatureSupport {
    let claim = || model::CapabilitySupport::supported(model::CapabilitySource::Configuration);
    model::ModelFeatureSupport {
        tool_choice: [
            model::ToolChoiceMode::Auto,
            model::ToolChoiceMode::None,
            model::ToolChoiceMode::Required,
            model::ToolChoiceMode::Named,
            model::ToolChoiceMode::Parallel,
        ]
        .into_iter()
        .map(|mode| (mode, claim()))
        .collect(),
        tool_schema: [
            model::ToolSchemaMode::Permissive,
            model::ToolSchemaMode::Strict,
        ]
        .into_iter()
        .map(|mode| (mode, claim()))
        .collect(),
        ..model::ModelFeatureSupport::default()
    }
}
fn capabilities() -> model::ProviderCapabilities {
    model::ProviderCapabilities {
        provider_id: PROVIDER.to_owned(),
        display_name: "Scoped normalized broker".to_owned(),
        capabilities: [
            model::ProviderCapability::Streaming,
            model::ProviderCapability::Cancellation,
            model::ProviderCapability::Tools,
            model::ProviderCapability::ParallelToolCalls,
        ]
        .into_iter()
        .collect(),
        feature_support: tool_support(),
        auth_schemes: BTreeSet::new(),
        retry_rules: Vec::new(),
        metadata: BTreeMap::from([("scope".to_owned(), "broker_admitted_tools".to_owned())]),
    }
}
/// Native and bundled normalized gateway provider.
pub struct AuthBrokerProviderPlugin {
    runtime: Result<ProviderRuntime, String>,
    state: Mutex<State>,
}
impl Default for AuthBrokerProviderPlugin {
    fn default() -> Self {
        Self {
            runtime: ProviderRuntime::new().map_err(|_| "broker_runtime_unavailable".to_owned()),
            state: Mutex::new(State::default()),
        }
    }
}
fn json(value: &impl Serialize) -> ServiceResponse {
    ServiceResponse::json(value).unwrap_or_else(|_| {
        ServiceResponse::error("broker_encoding_failed", "broker response encoding failed")
    })
}
fn failed(code: Failure) -> ServiceResponse {
    ServiceResponse::error(
        code,
        if code == "execution_budget_exceeds_lease" {
            "Execution budget exceeds actual broker lease minus ten seconds; shorten the budget; no model turn was started"
        } else {
            "Scoped broker request failed; verify lease and gateway policy; remote cleanup may remain pending"
        },
    )
}
impl AuthBrokerProviderPlugin {
    fn invoke(&self, context: &NativeServiceContext) -> ServiceResponse {
        match self.handle(context) {
            Ok(response) => response,
            Err(code) => failed(code),
        }
    }
    fn handle(&self, context: &NativeServiceContext) -> Result<ServiceResponse, Failure> {
        let request = &context.request;
        if request.interface_id == setup::INTERFACE {
            if request.operation != "prepare" {
                return Err("unsupported_operation");
            }
            let input: setup::PrepareRequest =
                request.payload_json().map_err(|_| "invalid_request")?;
            let runtime = self
                .runtime
                .as_ref()
                .map_err(|_| "broker_runtime_unavailable")?;
            return runtime
                .block_on(
                    async move { setup::prepare(&input).await.map(|response| json(&response)) },
                )
                .map_err(|_| "broker_runtime_unavailable")?;
        }
        if request.interface_id != model::MODEL_PROVIDER_INTERFACE_ID {
            return Err("unsupported_interface");
        }
        match request.operation.as_str() {
            model::OP_CAPABILITIES => Ok(json(&capabilities())),
            model::OP_MODELS => {
                let _: model::ModelListRequest =
                    request.payload_json().map_err(|_| "invalid_request")?;
                Ok(json(
                    &serde_json::json!({"models": [{"model_id": "broker-scoped",
                    "display_name": "Broker-admitted model", "is_default": true,
                    "capabilities": ["streaming_text", "tool_calls", "parallel_tool_calls"],
                    "feature_support": tool_support()}]}),
                ))
            }
            model::OP_VALIDATE_CONFIG => {
                let value: model::ValidateConfigRequest =
                    request.payload_json().map_err(|_| "invalid_request")?;
                Ok(json(&model::ValidateConfigResponse {
                    valid: Broker::from_context(&value.provider_context).is_ok(),
                    message: Some(
                        "HTTPS scoped normalized broker; no subscription credentials".to_owned(),
                    ),
                    failures: Vec::new(),
                    metadata: BTreeMap::new(),
                }))
            }
            model::OP_START_TURN => self.start(context),
            model::OP_POLL_TURN_EVENTS => self.poll(request),
            model::OP_CANCEL_TURN | model::OP_FINISH_TURN => self.lifecycle(request),
            _ => Err("unsupported_operation"),
        }
    }

    fn remote<T: DeserializeOwned + Send + 'static>(
        &self,
        broker: Broker,
        path: String,
        body: Option<Vec<u8>>,
    ) -> Result<T, Failure> {
        self.runtime
            .as_ref()
            .map_err(|_| "broker_runtime_unavailable")?
            .block_on(async move { broker.post(&path, body).await })
            .map_err(|_| "broker_runtime_unavailable")?
    }

    fn start(&self, context: &NativeServiceContext) -> Result<ServiceResponse, Failure> {
        if context.cancellation.is_cancelled() {
            return Err("broker_cancelled");
        }
        let value: model::ModelTurnRequest = context
            .request
            .payload_json()
            .map_err(|_| "invalid_request")?;
        if value.model_id != "broker-scoped" {
            return Err("broker_model_must_be_scoped");
        }
        let broker = Broker::from_context(&value.provider_context)?;
        let body = serde_json::to_vec(&start_body(&value)?).map_err(|_| "invalid_request")?;
        {
            let mut state = self.state.lock().map_err(|_| "broker_state_unavailable")?;
            state.reserve_start(&value.turn_id, broker.expires_at)?;
        }
        // An uncertain remote start retains its bounded reservation. Never automatically retry.
        let started: wire::StartResponse =
            self.remote(broker.clone(), "/v1/turns".to_owned(), Some(body))?;
        turn_path(&started.turn_id, "poll")?;
        {
            let mut state = self.state.lock().map_err(|_| "broker_state_unavailable")?;
            if state.turns.contains_key(&started.turn_id) {
                return Err("broker_turn_identity_conflict");
            }
            state.starts.remove(&value.turn_id);
            state.turns.insert(
                started.turn_id.clone(),
                Turn {
                    broker,
                    cursor: 0,
                    busy: false,
                },
            );
        }
        Ok(json(&model::StartTurnResponse {
            provider_turn_id: started.turn_id,
        }))
    }

    fn claim(&self, id: &str) -> Result<(Broker, u64), Failure> {
        let claimed = {
            let mut state = self.state.lock().map_err(|_| "broker_state_unavailable")?;
            state.claim(id)?
        };
        Ok(claimed)
    }

    fn poll(&self, request: &ServiceRequest) -> Result<ServiceResponse, Failure> {
        let value: model::PollTurnEventsRequest =
            request.payload_json().map_err(|_| "invalid_request")?;
        let path = turn_path(&value.provider_turn_id, "poll")?;
        let (broker, cursor) = self.claim(&value.provider_turn_id)?;
        let body = serde_json::to_vec(&serde_json::json!({"after_sequence": cursor}))
            .map_err(|_| "invalid_request");
        let outcome = body
            .and_then(|body| self.remote(broker, path, Some(body)))
            .and_then(|page| page_events(page, cursor));
        {
            let mut state = self.state.lock().map_err(|_| "broker_state_unavailable")?;
            state.complete(
                &value.provider_turn_id,
                outcome.as_ref().ok().map(|(cursor, _)| *cursor),
                false,
            )?;
        }
        let (_, events) = match outcome {
            Ok(page) => page,
            Err(error) => {
                let finish = ServiceRequest {
                    interface_id: model::MODEL_PROVIDER_INTERFACE_ID.to_owned(),
                    operation: model::OP_FINISH_TURN.to_owned(),
                    payload: serde_json::to_vec(&model::FinishTurnRequest {
                        provider_turn_id: value.provider_turn_id,
                    })
                    .map_err(|_| "broker_cleanup_unverified")?,
                };
                self.lifecycle(&finish)?;
                return Err(error);
            }
        };
        Ok(json(&model::PollTurnEventsResponse { events }))
    }

    fn lifecycle(&self, request: &ServiceRequest) -> Result<ServiceResponse, Failure> {
        let value: model::FinishTurnRequest =
            request.payload_json().map_err(|_| "invalid_request")?;
        let finish = request.operation == model::OP_FINISH_TURN;
        let path = turn_path(
            &value.provider_turn_id,
            if finish { "finish" } else { "cancel" },
        )?;
        let (broker, _) = self.claim(&value.provider_turn_id)?;
        let outcome: Result<model::AckResponse, Failure> = self.remote(broker, path, None);
        {
            let mut state = self.state.lock().map_err(|_| "broker_state_unavailable")?;
            // Retain identity on every failed/uncertain cleanup, so explicit retry remains possible.
            state.complete(&value.provider_turn_id, None, finish && outcome.is_ok())?;
        }
        outcome
            .map(|ack| json(&ack))
            .map_err(|_| "broker_cleanup_unverified")
    }
}

impl RustPlugin for AuthBrokerProviderPlugin {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        self.invoke(&context)
    }
}
impl ConcurrentRustPlugin for AuthBrokerProviderPlugin {
    fn invoke_service_concurrent(&self, context: NativeServiceContext) -> ServiceResponse {
        self.invoke(&context)
    }
}
/// Local-only OIDC bootstrap CLI contribution.
#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_cli_registration() -> bcode_plugin_sdk::StaticCliRegistration {
    cli::registration()
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    let mut vtable = bcode_plugin_sdk::static_concurrent_plugin_vtable!(
        AuthBrokerProviderPlugin,
        include_str!("../bcode-plugin.toml")
    );
    vtable.cli_registration = Some(static_cli_registration);
    vtable
}
#[cfg(not(feature = "static-bundled"))]
bcode_plugin_sdk::export_concurrent_plugin!(
    AuthBrokerProviderPlugin,
    include_str!("../bcode-plugin.toml")
);

#[cfg(test)]
mod http_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "static-bundled")]
    #[test]
    fn bundled_vtable_exposes_local_bootstrap_cli() {
        let vtable = static_plugin();
        let registration = vtable.cli_registration.expect("bundled CLI registration")();
        let matches = (registration.command)()
            .try_get_matches_from([
                "auth-broker",
                "prepare",
                "--directory",
                "relative-invalid-directory",
                "--broker-url",
                "https://broker.example",
                "--audience",
                "bcode",
                "--timeout-secs",
                "600",
            ])
            .expect("public command syntax");
        assert!(!(registration.requires_daemon)(&matches));
        let runtime = ProviderRuntime::new().expect("runtime");
        let result = runtime
            .block_on((registration.invoke)(matches))
            .expect("invoke local CLI");
        assert_eq!(
            result.expect_err("unsafe directory rejected before network"),
            "invalid_private_directory"
        );
    }
    fn context(url: &str) -> model::ProviderRequestContext {
        model::ProviderRequestContext {
            settings: BTreeMap::from([("broker_url".into(), url.into())]),
            env: BTreeMap::from([
                (TOKEN_ENV.into(), "private-lease".into()),
                (
                    "BCODE_AUTH_BROKER_EXPIRES_AT".into(),
                    (now() + 300).to_string(),
                ),
            ]),
            ..Default::default()
        }
    }
    #[cfg(unix)]
    #[test]
    fn provider_reads_private_lease_without_inherited_secret_environment() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().expect("private fixture");
        let path = std::fs::canonicalize(directory.path()).expect("canonical");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .expect("permissions");
        for (name, contents) in [
            ("broker-token", "synthetic-file-lease".to_owned()),
            ("broker-expiry", (now() + 900).to_string()),
        ] {
            std::fs::write(path.join(name), contents).expect("fixture");
            std::fs::set_permissions(path.join(name), std::fs::Permissions::from_mode(0o600))
                .expect("permissions");
        }
        let mut context = context("https://broker.example");
        context.env.clear();
        context.settings.insert(
            "broker_lease_file".into(),
            path.join("broker-token").to_str().expect("path").into(),
        );
        context.settings.insert(
            "broker_expiry_file".into(),
            path.join("broker-expiry").to_str().expect("path").into(),
        );
        let broker = Broker::from_context(&context).expect("file-backed lease");
        assert_eq!(broker.token, "synthetic-file-lease");
        assert!(!format!("{broker:?}").contains("synthetic-file-lease"));
        context.env.insert(TOKEN_ENV.into(), "other-lease".into());
        assert_eq!(
            Broker::from_context(&context).expect_err("ambiguous selection"),
            "ambiguous_broker_lease"
        );
        context.env.clear();
        context.settings.remove("broker_expiry_file");
        assert_eq!(
            Broker::from_context(&context).expect_err("partial selection"),
            "invalid_private_file"
        );
    }
    #[test]
    fn endpoint_and_lease_fail_closed_and_debug_redacts() {
        for url in [
            "http://broker.example",
            "https://user@broker.example",
            "https://broker.example/path",
            "https://broker.example/?token=x",
            "https://broker.example/#x",
        ] {
            assert!(Broker::from_context(&context(url)).is_err());
        }
        let broker =
            Broker::from_context(&context("https://broker.example")).expect("valid broker");
        assert!(!format!("{broker:?}").contains("private-lease"));
        let mut expired = context("https://broker.example");
        expired
            .env
            .insert("BCODE_AUTH_BROKER_EXPIRES_AT".into(), "1".into());
        assert!(Broker::from_context(&expired).is_err());
    }
    #[test]
    fn route_cannot_escape_and_poll_rejects_private_events_or_gaps() {
        for id in ["../leases", "x?token=y", "x/y", ""] {
            assert!(turn_path(id, "poll").is_err());
        }
        let page = wire::PollResponse {
            events: vec![wire::SequencedEvent {
                sequence: 2,
                event: model::ProviderTurnEvent::TurnStarted,
            }],
            next_sequence: 2,
            terminal: false,
        };
        assert!(page_events(page, 0).is_err());
        let page = wire::PollResponse {
            events: vec![wire::SequencedEvent {
                sequence: 1,
                event: model::ProviderTurnEvent::ProviderMetadata {
                    key: "private".into(),
                    value: "untrusted".into(),
                },
            }],
            next_sequence: 1,
            terminal: false,
        };
        assert!(page_events(page, 0).is_err());
    }
    #[test]
    fn normalized_request_preserves_tools_and_has_no_auth() {
        let mut request: model::ModelTurnRequest = serde_json::from_value(serde_json::json!({
            "session_id": "00000000-0000-0000-0000-000000000001", "turn_id": "test-turn",
            "model_id": "broker-scoped", "messages": [{"role": "user", "content": [{"type": "text", "text": "hello"}]}]
        })).expect("model request");
        request.provider_context = context("https://broker.example");
        let body = start_body(&request).expect("text accepted");
        let encoded = serde_json::to_string(&body).expect("body");
        assert!(!encoded.contains("private-lease"));
        assert!(!encoded.contains("provider_context"));
        assert!(encoded.contains("hello"));
        request.tools.push(model::ToolDefinition {
            name: "shell".to_owned(),
            description: "execute".to_owned(),
            input_schema: serde_json::json!({"type":"object"}),
        });
        let body = start_body(&request).expect("normalized tools accepted");
        assert_eq!(body.tools, request.tools);
        let remote: wire::StartRequest = serde_json::from_value(serde_json::json!({
            "scope_id": "test", "expires_at": now() + 60, "request_id": body.request_id,
            "messages": body.messages, "tools": body.tools,
            "tool_call_policy": body.tool_call_policy, "tool_schema_mode": body.tool_schema_mode,
            "max_output_tokens": body.max_output_tokens,
        }))
        .expect("exact gateway schema");
        assert!(remote.validate_public().is_ok());
        request.tools.clear();
        request.messages[0].role = model::MessageRole::Tool;
        assert!(start_body(&request).is_err());
    }
    #[test]
    fn tool_call_poll_and_result_continuation_round_trip() {
        let call = model::ToolCall {
            id: "call-1".to_owned(),
            name: "read".to_owned(),
            arguments: serde_json::json!({"path":"README.md"}),
        };
        let page = wire::PollResponse {
            events: vec![wire::SequencedEvent {
                sequence: 1,
                event: model::ProviderTurnEvent::ToolCallFinished { call: call.clone() },
            }],
            next_sequence: 1,
            terminal: false,
        };
        let (cursor, events) = page_events(page, 0).expect("tool event");
        assert_eq!(cursor, 1);
        assert!(
            matches!(&events[0], model::ProviderTurnEvent::ToolCallFinished { call: delivered } if delivered == &call)
        );
        let request: model::ModelTurnRequest = serde_json::from_value(serde_json::json!({
            "session_id": "00000000-0000-0000-0000-000000000001", "turn_id": "continuation",
            "model_id": "broker-scoped", "tools": [{"name":"read","description":"Read text","input_schema":{"type":"object"}}],
            "tool_call_policy": {"choice":{"mode":"tool","name":"read"}, "parallel":false},
            "tool_schema_mode":"permissive",
            "messages": [
                {"role":"assistant", "content":[{"type":"tool_call","call":call}]},
                {"role":"tool", "content":[{"type":"tool_result","result":{"call_id":"call-1","output":"file text","is_error":false,"content":[{"type":"text","text":"file text"}]}}]}
            ]
        })).expect("continuation");
        let body = start_body(&request).expect("tool result admitted");
        assert_eq!(body.messages, request.messages);
        assert_eq!(body.tool_call_policy, request.tool_call_policy);
        let encoded = serde_json::to_string(&body).expect("body");
        assert!(encoded.contains("call-1"));
        assert!(!encoded.contains("provider_context"));
    }
    #[test]
    fn uncertain_cleanup_keeps_identity_and_cursor_for_retry() {
        let broker = Broker::from_context(&context("https://broker.example")).expect("broker");
        let mut state = State::default();
        state.turns.insert(
            "turn".to_owned(),
            Turn {
                broker,
                cursor: 7,
                busy: false,
            },
        );
        assert_eq!(state.claim("turn").expect("claim").1, 7);
        assert!(state.claim("turn").is_err());
        state
            .complete("turn", None, false)
            .expect("failed finish retains");
        assert_eq!(state.claim("turn").expect("retry").1, 7);
        state.complete("turn", None, true).expect("verified finish");
        assert!(!state.turns.contains_key("turn"));
        state.reserve_start("start", now() + 60).expect("reserve");
        assert_eq!(
            state.reserve_start("start", now() + 60),
            Err("broker_start_outcome_unverified")
        );
    }
    #[test]
    fn subscription_auth_is_never_accepted() {
        let mut context = context("https://broker.example");
        context.auth = Some(model::ProviderAuthContext::default());
        assert!(Broker::from_context(&context).is_err());
    }
    #[test]
    fn poll_preserves_text_and_cursor() {
        let page = wire::PollResponse {
            events: vec![wire::SequencedEvent {
                sequence: 1,
                event: model::ProviderTurnEvent::TextDelta {
                    text: "hello".into(),
                },
            }],
            next_sequence: 1,
            terminal: false,
        };
        let (cursor, events) = page_events(page, 0).expect("page");
        assert_eq!(cursor, 1);
        assert!(
            matches!(&events[0], model::ProviderTurnEvent::TextDelta { text } if text == "hello")
        );
    }
}
