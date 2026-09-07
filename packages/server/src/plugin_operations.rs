//! Transport-neutral application operations for plugin service plumbing.

use super::{ServerState, plugin_event_metric_labels, plugin_service_metric_labels};
use tokio::sync::mpsc;

/// Failure while routing input to an active plugin invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteInvocationInputError {
    /// No matching active invocation exists.
    NotActive,
    /// The producer does not own the selected invocation.
    ProducerMismatch,
    /// The producer identifier is empty.
    InvalidProducer,
    /// The schema identifier or version is invalid.
    InvalidSchema,
    /// The input identifier is empty.
    InvalidInputId,
    /// The encoded input exceeds the operation limit.
    TooLarge,
    /// The bounded invocation input queue is full.
    QueueFull,
    /// The invocation input route has closed.
    RouteClosed,
}

impl RouteInvocationInputError {
    /// Stable public operation error code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::NotActive => "plugin_invocation_not_active",
            Self::ProducerMismatch => "plugin_invocation_producer_mismatch",
            Self::InvalidProducer => "invalid_invocation_input_producer",
            Self::InvalidSchema => "invalid_invocation_input_schema",
            Self::InvalidInputId => "invalid_invocation_input_id",
            Self::TooLarge => "invocation_input_too_large",
            Self::QueueFull => "invocation_input_queue_full",
            Self::RouteClosed => "invocation_input_route_closed",
        }
    }

    /// Secret-safe public operation error message.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::NotActive => "plugin invocation is not active",
            Self::ProducerMismatch => "invocation input producer does not own the invocation",
            Self::InvalidProducer => "invocation input producer id must not be empty",
            Self::InvalidSchema => "invocation input schema and version must be valid",
            Self::InvalidInputId => "invocation input id must not be empty",
            Self::TooLarge => "invocation input exceeds 64 KiB",
            Self::QueueFull => "plugin invocation input queue is full",
            Self::RouteClosed => "plugin invocation input route is closed",
        }
    }
}

/// Return the current plugin service inventory without transport framing.
pub fn list_services(state: &ServerState) -> Vec<bcode_ipc::PluginServiceSummary> {
    state
        .plugins
        .service_summaries()
        .into_iter()
        .map(|(plugin_id, service)| bcode_ipc::PluginServiceSummary {
            plugin_id,
            interface_id: service.interface_id,
            name: service.name,
            description: service.description,
            workflow_blocks: service.workflow_blocks,
        })
        .collect()
}

/// Return the current renderer-neutral plugin contributions without transport framing.
pub fn list_contributions(state: &ServerState) -> bcode_ipc::PluginContributions {
    let mut command_contributions = state
        .plugins
        .registered_command_contributions(&bcode_command::CommandSurface::Palette);
    command_contributions.extend(
        state
            .plugins
            .registered_command_contributions(&bcode_command::CommandSurface::Slash),
    );
    command_contributions.sort_by(|left, right| left.id.cmp(&right.id));
    command_contributions.dedup_by(|left, right| left.id == right.id);
    bcode_ipc::PluginContributions {
        command_contributions,
        commands: state.plugins.command_contributions(),
        config_extensions: state.plugins.config_extensions(),
    }
}

/// Normalized plugin-service failure safe for public transport boundaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicPluginError {
    /// Stable public error code.
    pub code: &'static str,
    /// Secret-safe normalized message.
    pub message: &'static str,
}

/// Normalize a private plugin-host failure for public callers.
#[must_use]
pub const fn normalize_error(_error: &bcode_plugin::PluginLoadError) -> PublicPluginError {
    PublicPluginError {
        code: "plugin_error",
        message: "plugin operation failed; inspect local daemon diagnostics",
    }
}

/// Route one bounded producer-owned input to an active plugin invocation.
pub fn route_invocation_input(
    state: &ServerState,
    session_id: bcode_session_models::SessionId,
    input: bcode_tool::ToolInvocationInput,
) -> Result<(), RouteInvocationInputError> {
    validate_invocation_input(&input)?;
    let active = {
        let invocations = state
            .active_plugin_invocations
            .lock()
            .map_err(|_| RouteInvocationInputError::NotActive)?;
        invocations
            .get(&(session_id, input.invocation_id.clone()))
            .cloned()
            .ok_or(RouteInvocationInputError::NotActive)?
    };
    if active.producer_plugin_id != input.producer_id {
        return Err(RouteInvocationInputError::ProducerMismatch);
    }
    enqueue_validated_invocation_input(&active, input)
}

const MAX_INVOCATION_INPUT_BYTES: usize = 64 * 1024;

struct InvocationInputBudget {
    remaining: usize,
}

impl std::io::Write for InvocationInputBudget {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.remaining = self
            .remaining
            .checked_sub(bytes.len())
            .ok_or_else(|| std::io::Error::other("invocation input exceeds encoded size limit"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn validate_invocation_input(
    input: &bcode_tool::ToolInvocationInput,
) -> Result<(), RouteInvocationInputError> {
    if input.producer_id.trim().is_empty() {
        return Err(RouteInvocationInputError::InvalidProducer);
    }
    if input.schema.trim().is_empty() || input.schema_version == 0 {
        return Err(RouteInvocationInputError::InvalidSchema);
    }
    if input.input_id.trim().is_empty() {
        return Err(RouteInvocationInputError::InvalidInputId);
    }
    serde_json::to_writer(
        InvocationInputBudget {
            remaining: MAX_INVOCATION_INPUT_BYTES,
        },
        input,
    )
    .map_err(|_| RouteInvocationInputError::TooLarge)
}

/// Validate and enqueue input from an already authorized invocation route.
#[cfg(test)]
pub fn enqueue_invocation_input(
    active: &super::ActivePluginInvocation,
    input: bcode_tool::ToolInvocationInput,
) -> Result<(), RouteInvocationInputError> {
    validate_invocation_input(&input)?;
    enqueue_validated_invocation_input(active, input)
}

fn enqueue_validated_invocation_input(
    active: &super::ActivePluginInvocation,
    input: bcode_tool::ToolInvocationInput,
) -> Result<(), RouteInvocationInputError> {
    active.inputs.try_send(input).map_err(|error| match error {
        mpsc::error::TrySendError::Full(_) => RouteInvocationInputError::QueueFull,
        mpsc::error::TrySendError::Closed(_) => RouteInvocationInputError::RouteClosed,
    })
}

/// Normalized plugin service result returned by application operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginServiceOperationResult {
    /// Opaque plugin-owned response payload.
    pub payload: Vec<u8>,
    /// Optional normalized plugin service error.
    pub error: Option<PluginServiceOperationError>,
}

/// Normalized plugin service error without implementation details.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginServiceOperationError {
    /// Stable plugin-owned error code.
    pub code: String,
    /// Secret-safe public error message.
    pub message: String,
}

/// Convert one internal plugin response into the normalized operation result.
#[must_use]
fn project_service_response(
    response: bcode_plugin::ServiceResponse,
) -> PluginServiceOperationResult {
    PluginServiceOperationResult {
        payload: response.payload,
        error: response.error.map(|error| PluginServiceOperationError {
            code: error.code,
            message: error.message,
        }),
    }
}

pub async fn invoke_service(
    state: &ServerState,
    plugin_id: &str,
    interface_id: &str,
    operation: String,
    payload: Vec<u8>,
) -> Result<PluginServiceOperationResult, PublicPluginError> {
    let plugin_id = plugin_id.to_owned();
    let interface_id = interface_id.to_owned();
    let labels = plugin_service_metric_labels(Some(&plugin_id), &interface_id, &operation);
    let authorized_session_id =
        super::command_invocation_session(&interface_id, &operation, &payload);
    let (bridge, bridge_requests) = super::server_plugin_bridge();
    let invocation = state.plugins.invoke_service_with_bridge_scoped(
        &plugin_id,
        interface_id,
        operation,
        payload,
        bcode_plugin::PluginInvocationScope::Global,
        Some(bridge),
    );
    Box::pin(state.metrics.time_result_async(
        "plugin.service",
        labels,
        drive_service_bridge(state, authorized_session_id, bridge_requests, invocation),
    ))
    .await
    .map(project_service_response)
    .map_err(|error| normalize_error(&error))
}

async fn drive_service_bridge<T>(
    state: &ServerState,
    authorized_session_id: Option<bcode_session_models::SessionId>,
    mut bridge_requests: mpsc::UnboundedReceiver<super::ServerPluginBridgeCall>,
    invocation: impl std::future::Future<Output = T>,
) -> T {
    tokio::pin!(invocation);
    loop {
        tokio::select! {
            result = &mut invocation => break result,
            bridge_call = bridge_requests.recv() => {
                let Some(bridge_call) = bridge_call else {
                    return invocation.await;
                };
                let response = super::resolve_command_plugin_bridge_request(
                    &state.sessions,
                    authorized_session_id,
                    bridge_call.request,
                    &bridge_call.cancellation,
                ).await;
                let _sent = bridge_call.response.send(response);
            }
        }
    }
}

/// Call the unique provider of one typed plugin service interface.
pub async fn call_service(
    state: &ServerState,
    interface_id: &str,
    operation: String,
    payload: Vec<u8>,
) -> Result<PluginServiceOperationResult, PublicPluginError> {
    let labels = plugin_service_metric_labels(None, interface_id, &operation);
    state
        .metrics
        .time_result_async(
            "plugin.service",
            labels,
            state
                .plugins
                .invoke_service_by_interface(interface_id, operation, payload),
        )
        .await
        .map(project_service_response)
        .map_err(|error| normalize_error(&error))
}

/// Publish one plugin event through host routing.
pub async fn publish_event(
    state: &ServerState,
    topic: &str,
    payload: &[u8],
) -> Result<usize, PublicPluginError> {
    state
        .metrics
        .time_result_async(
            "plugin.event_delivery",
            plugin_event_metric_labels(topic),
            state.plugins.publish_event(topic, payload),
        )
        .await
        .map_err(|error| normalize_error(&error))
}

#[cfg(test)]
mod tests {
    #[test]
    fn invocation_input_budget_counts_envelope_escaping_and_utf8() {
        for character in ['a', '\n', 'λ'] {
            let mut input = bcode_tool::ToolInvocationInput {
                invocation_id: "invocation".into(),
                input_id: "input".into(),
                producer_id: "producer".into(),
                schema: "schema".into(),
                schema_version: 1,
                payload: serde_json::json!(""),
            };
            let overhead = serde_json::to_vec(&input).unwrap().len();
            let width = serde_json::to_string(&character.to_string()).unwrap().len() - 2;
            let remaining = super::MAX_INVOCATION_INPUT_BYTES - overhead;
            let mut payload = character.to_string().repeat(remaining / width);
            payload.push_str(&"a".repeat(remaining % width));
            input.payload = serde_json::json!(payload);
            assert_eq!(
                serde_json::to_vec(&input).unwrap().len(),
                super::MAX_INVOCATION_INPUT_BYTES
            );
            assert_eq!(super::validate_invocation_input(&input), Ok(()));
            payload.push('a');
            input.payload = serde_json::json!(payload);
            assert_eq!(
                super::validate_invocation_input(&input),
                Err(super::RouteInvocationInputError::TooLarge)
            );
        }
    }

    #[test]
    fn invocation_input_budget_rejects_overflow_without_consuming_budget() {
        use std::io::Write as _;
        let mut budget = super::InvocationInputBudget { remaining: 3 };
        assert!(budget.write_all(b"four").is_err());
        assert_eq!(budget.remaining, 3);
        budget.write_all(b"abc").unwrap();
        assert_eq!(budget.remaining, 0);
        assert!(budget.write_all(b"x").is_err());
    }
    #[tokio::test]
    async fn service_bridge_drains_queued_request_before_closure() {
        let state = crate::tests::test_server_state(bcode_session::SessionManager::default());
        let (sender, requests) = tokio::sync::mpsc::unbounded_channel();
        let (response, received) = std::sync::mpsc::sync_channel(1);
        sender
            .send(crate::ServerPluginBridgeCall {
                request: bcode_plugin_sdk::ServiceBridgeRequest::Exchange(
                    bcode_tool::ToolExchangeRequest {
                        invocation_id: "test-invocation".to_owned(),
                        exchange_id: "test-exchange".to_owned(),
                        producer_id: "test".to_owned(),
                        schema: "test.exchange".to_owned(),
                        schema_version: 1,
                        payload: serde_json::Value::Null,
                        response_policy: bcode_tool::ToolExchangeResponsePolicy::Required,
                    },
                ),
                cancellation: bcode_plugin_sdk::ServiceCancellation::default(),
                response,
            })
            .expect("queue bridge request");
        drop(sender);
        let (complete, result) = tokio::sync::oneshot::channel::<u32>();
        let value = async {
            let driver = super::drive_service_bridge(&state, None, requests, result);
            tokio::pin!(driver);
            let mut context = std::task::Context::from_waker(std::task::Waker::noop());
            assert!(std::future::Future::poll(driver.as_mut(), &mut context).is_pending());
            assert_eq!(
                received.try_recv().expect("queued request answered"),
                Err(
                    "command invocation bridge supports nested application services only"
                        .to_owned()
                )
            );
            complete.send(42).expect("complete invocation");
            driver.await.expect("invocation result")
        }
        .await;
        drop(state);
        assert_eq!(value, 42);
    }

    #[tokio::test]
    async fn closed_service_bridge_yields_until_invocation_completes() {
        let state = crate::tests::test_server_state(bcode_session::SessionManager::default());
        let (bridge, requests) = crate::server_plugin_bridge();
        drop(bridge);
        let (complete, result) = tokio::sync::oneshot::channel::<u32>();
        let value = async {
            let driver = super::drive_service_bridge(&state, None, requests, result);
            tokio::pin!(driver);
            let waker = std::task::Waker::noop();
            let mut context = std::task::Context::from_waker(waker);
            assert!(std::future::Future::poll(driver.as_mut(), &mut context).is_pending());
            complete.send(42).expect("invocation still awaited");
            driver.await.expect("invocation result")
        }
        .await;
        drop(state);
        assert_eq!(value, 42);
    }

    #[tokio::test]
    async fn closed_service_bridge_preserves_invocation_failure() {
        let state = crate::tests::test_server_state(bcode_session::SessionManager::default());
        let (bridge, requests) = crate::server_plugin_bridge();
        drop(bridge);
        let (complete, result) = tokio::sync::oneshot::channel::<u32>();
        let outcome = async {
            let driver = super::drive_service_bridge(&state, None, requests, result);
            tokio::pin!(driver);
            let mut context = std::task::Context::from_waker(std::task::Waker::noop());
            assert!(std::future::Future::poll(driver.as_mut(), &mut context).is_pending());
            drop(complete);
            driver.await
        }
        .await;
        drop(state);
        assert!(outcome.is_err());
    }
}
