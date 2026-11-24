//! Native Antigravity authentication inspection.
//!
//! Readiness is probed non-mutatingly via `session/list` on a short-lived ACP server:
//! * A successful `session/list` response proves the native account is ready.
//! * JSON-RPC error `-32000` ("Authentication required") reports `AuthenticationRequired`.
//!
//! No session is created, no prompt is sent, and no native credentials are copied or mutated.
use crate::PLUGIN_ID;
use crate::transport::{AcpError, AcpServer, Launch, wipe_value};
use bcode_harness_models::HarnessAuthBinding;
use bcode_provider_auth_models::managed::{
    AuthCustodyMode, AuthNamespace, AuthReadiness, ManagedAuthStatus,
};
use serde_json::json;

/// Inspect native account readiness without creating a session or mutating credentials.
///
/// # Errors
/// * Returns configuration/protocol/disconnected errors if the ACP server cannot be initialized.
pub fn inspect_status(
    launch: &Launch,
    binding: &HarnessAuthBinding,
) -> Result<ManagedAuthStatus, AcpError> {
    if binding.custody != AuthCustodyMode::NativeManaged {
        return Err(AcpError::Unsupported);
    }
    let mut server = AcpServer::spawn(launch)?;
    let _version = server.initialize(launch)?;
    let readiness = probe_readiness(&mut server);
    Ok(ManagedAuthStatus {
        namespace: AuthNamespace {
            plugin_id: PLUGIN_ID.to_owned(),
            credential_namespace: binding.credential_namespace.clone(),
        },
        custody: AuthCustodyMode::NativeManaged,
        readiness,
    })
}

/// Probe readiness on an already-initialized ACP server using non-mutating `session/list`.
#[must_use]
pub fn probe_readiness(server: &mut AcpServer) -> AuthReadiness {
    server
        .call_raw("session/list", &json!({}))
        .map_or(AuthReadiness::Unknown, |mut response| {
            let result = if response
                .get("result")
                .is_some_and(serde_json::Value::is_object)
            {
                AuthReadiness::Ready
            } else if is_auth_required_error(response.get("error")) {
                AuthReadiness::LoginRequired
            } else {
                AuthReadiness::Unknown
            };
            wipe_value(&mut response);
            result
        })
}

/// Whether a JSON-RPC error object indicates ACP authentication is required (`-32000`).
#[must_use]
pub fn is_auth_required_error(error: Option<&serde_json::Value>) -> bool {
    let Some(err) = error else {
        return false;
    };
    let code = err.get("code").and_then(serde_json::Value::as_i64);
    let message = err
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    code == Some(-32000)
        || message
            .to_ascii_lowercase()
            .contains("authentication required")
}
