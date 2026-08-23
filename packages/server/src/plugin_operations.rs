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
    if input.producer_id.trim().is_empty() {
        return Err(RouteInvocationInputError::InvalidProducer);
    }
    if input.schema.trim().is_empty() || input.schema_version == 0 {
        return Err(RouteInvocationInputError::InvalidSchema);
    }
    if input.input_id.trim().is_empty() {
        return Err(RouteInvocationInputError::InvalidInputId);
    }
    let encoded = serde_json::to_vec(&input).map_err(|_| RouteInvocationInputError::TooLarge)?;
    if encoded.len() > 64 * 1024 {
        return Err(RouteInvocationInputError::TooLarge);
    }
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
    enqueue_invocation_input(&active, input)
}

pub fn enqueue_invocation_input(
    active: &super::ActivePluginInvocation,
    input: bcode_tool::ToolInvocationInput,
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
    let encoded = serde_json::to_vec(&input).map_err(|_| RouteInvocationInputError::TooLarge)?;
    if encoded.len() > 64 * 1024 {
        return Err(RouteInvocationInputError::TooLarge);
    }
    active.inputs.try_send(input).map_err(|error| match error {
        mpsc::error::TrySendError::Full(_) => RouteInvocationInputError::QueueFull,
        mpsc::error::TrySendError::Closed(_) => RouteInvocationInputError::RouteClosed,
    })
}

/// Portable normalized plugin service result projected at the transport boundary.
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
pub fn project_service_response(
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
) -> Result<bcode_plugin::ServiceResponse, PublicPluginError> {
    let plugin_id = plugin_id.to_owned();
    let interface_id = interface_id.to_owned();
    let labels = plugin_service_metric_labels(Some(&plugin_id), &interface_id, &operation);
    let authorized_session_id =
        super::command_invocation_session(&interface_id, &operation, &payload);
    let (bridge, mut bridge_requests) = super::server_plugin_bridge();
    let invocation = state.plugins.invoke_service_with_bridge_scoped(
        &plugin_id,
        interface_id,
        operation,
        payload,
        bcode_plugin::PluginInvocationScope::Global,
        Some(bridge),
    );
    Box::pin(
        state
            .metrics
            .time_result_async("plugin.service", labels, async {
                tokio::pin!(invocation);
                loop {
                    tokio::select! {
                        result = &mut invocation => break result,
                        bridge_call = bridge_requests.recv() => {
                            let Some(bridge_call) = bridge_call else {
                                continue;
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
            }),
    )
    .await
    .map_err(|error| normalize_error(&error))
}

/// Call the unique provider of one typed plugin service interface.
pub async fn call_service(
    state: &ServerState,
    interface_id: &str,
    operation: String,
    payload: Vec<u8>,
) -> Result<bcode_plugin::ServiceResponse, PublicPluginError> {
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
