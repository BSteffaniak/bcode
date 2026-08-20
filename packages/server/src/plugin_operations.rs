//! Transport-neutral application operations for plugin service plumbing.

use super::{ServerState, plugin_event_metric_labels, plugin_service_metric_labels};

/// Route one bounded producer-owned input to an active plugin invocation.
pub fn route_invocation_input(
    state: &ServerState,
    session_id: bcode_session_models::SessionId,
    input: bcode_tool::ToolInvocationInput,
) -> Result<(), String> {
    state
        .active_plugin_invocations
        .lock()
        .map_err(|_| "active plugin invocation registry poisoned".to_owned())
        .and_then(|invocations| {
            let active = invocations
                .get(&(session_id, input.invocation_id.clone()))
                .ok_or_else(|| "plugin invocation is not active".to_owned())?;
            if active.producer_plugin_id != input.producer_id {
                return Err("invocation input producer does not own the invocation".to_owned());
            }
            super::enqueue_invocation_input(active, input)
        })
}

/// Call the unique provider of one typed plugin service interface.
pub async fn call_service(
    state: &ServerState,
    interface_id: &str,
    operation: String,
    payload: Vec<u8>,
) -> Result<bcode_plugin::ServiceResponse, bcode_plugin::PluginLoadError> {
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
}

/// Publish one plugin event through host routing.
pub async fn publish_event(
    state: &ServerState,
    topic: &str,
    payload: &[u8],
) -> Result<usize, bcode_plugin::PluginLoadError> {
    state
        .metrics
        .time_result_async(
            "plugin.event_delivery",
            plugin_event_metric_labels(topic),
            state.plugins.publish_event(topic, payload),
        )
        .await
}
