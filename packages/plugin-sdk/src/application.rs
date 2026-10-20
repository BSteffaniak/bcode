//! Caller-bound portable plugin application contracts.
//!
//! These contracts preserve the existing service payload and context representation. Opaque
//! payloads remain plugin-owned; invocation identity and workspace hints do not grant authority.

use bcode_session_models::SessionId;
use serde::{Deserialize, Serialize};
use std::future::Future;

/// Secret-safe failure from the application boundary, not a native-loader error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginApplicationFailure {
    /// Stable machine-readable code.
    pub code: String,
    /// Normalized public diagnostic.
    pub message: String,
}

/// Service interface provided by a loaded plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginServiceSummary {
    /// Owning enabled plugin.
    pub plugin_id: String,
    /// Compatibility-defined service identity.
    pub interface_id: String,
    /// Optional display name.
    pub name: Option<String>,
    /// Optional description.
    pub description: Option<String>,
    /// Plugin-owned workflow contributions.
    #[serde(default)]
    pub workflow_blocks: Vec<bcode_workflow::WorkflowBlockDefinition>,
}

/// Context hints and lifetime for one caller-owned invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginServiceContext {
    /// Enabled agent hint; omitted hints use explicit default policy.
    #[serde(default)]
    pub agent_id: Option<String>,
    /// Unique identity within the connected caller; reuse is rejected.
    pub invocation_id: String,
    /// Workspace hints checked by the application, never grants.
    pub workspace: crate::WorkspaceContext,
    /// Total deadline between 1 and 30,000 milliseconds.
    pub deadline_ms: u64,
}

/// Plugin-owned service failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginServiceError {
    /// Stable plugin-owned code.
    pub code: String,
    /// Public diagnostic supplied by the service contract.
    pub message: String,
}

/// Normalized result retaining the plugin-owned payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginServiceResponse {
    /// Opaque service response bytes.
    pub payload: Vec<u8>,
    /// Service-level failure, distinct from host routing failure.
    pub error: Option<PluginServiceError>,
}

/// Plugin access supplied by the host for an authenticated caller.
///
/// Dropping observation or a handle is not a durable resume contract. Contextual invocation
/// cancellation selects only that caller's invocation identity, never another caller's work.
pub trait PluginApplication: Sync {
    /// Discover services without executing them.
    fn list_plugin_services(
        &self,
    ) -> impl Future<Output = Result<Vec<PluginServiceSummary>, PluginApplicationFailure>> + Send;
    /// Read renderer-neutral contributions without executing commands.
    fn list_plugin_contributions(
        &self,
    ) -> impl Future<Output = Result<crate::PluginContributions, PluginApplicationFailure>> + Send;
    /// Invoke an explicitly selected producer through existing host routing.
    fn invoke_plugin_service(
        &self,
        plugin_id: String,
        interface_id: String,
        operation: String,
        payload: Vec<u8>,
    ) -> impl Future<Output = Result<PluginServiceResponse, PluginApplicationFailure>> + Send;
    /// Resolve a unique producer, rejecting ambiguous service ownership.
    fn call_plugin_service(
        &self,
        interface_id: String,
        operation: String,
        payload: Vec<u8>,
    ) -> impl Future<Output = Result<PluginServiceResponse, PluginApplicationFailure>> + Send;
    /// Invoke with checked workspace, permission, agent, and lifetime context.
    fn invoke_plugin_service_contextual(
        &self,
        context: PluginServiceContext,
        plugin_id: String,
        interface_id: String,
        operation: String,
        payload: Vec<u8>,
    ) -> impl Future<Output = Result<PluginServiceResponse, PluginApplicationFailure>> + Send;
    /// Request cancellation of this caller's invocation. Acceptance is not terminal proof.
    fn cancel_plugin_service_invocation(
        &self,
        invocation_id: String,
    ) -> impl Future<Output = Result<bool, PluginApplicationFailure>> + Send;
    /// Publish through existing host event routing and return the delivered subscriber count.
    fn publish_plugin_event(
        &self,
        topic: String,
        payload: Vec<u8>,
    ) -> impl Future<Output = Result<usize, PluginApplicationFailure>> + Send;
    /// Route bounded producer-owned input, claiming control only on successful enqueue.
    fn send_plugin_invocation_input(
        &self,
        session_id: SessionId,
        input: bcode_tool::ToolInvocationInput,
    ) -> impl Future<Output = Result<(), PluginApplicationFailure>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_preserves_optional_agent_and_rejects_unknown_fields() {
        let wire = serde_json::json!({"invocation_id":"one", "workspace":{}, "deadline_ms":100});
        let context: PluginServiceContext = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(context.agent_id, None);
        assert_eq!(context.workspace, crate::WorkspaceContext::default());
        let mut unknown = wire;
        unknown["authority"] = true.into();
        assert!(serde_json::from_value::<PluginServiceContext>(unknown).is_err());
    }

    #[test]
    fn response_preserves_opaque_payload_and_service_failure() {
        let response = PluginServiceResponse {
            payload: vec![0, 255],
            error: Some(PluginServiceError {
                code: "unsupported".into(),
                message: "unsupported operation".into(),
            }),
        };
        let decoded: PluginServiceResponse =
            serde_json::from_slice(&serde_json::to_vec(&response).unwrap()).unwrap();
        assert_eq!(decoded, response);
    }
}
