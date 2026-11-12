//! Versioned passive discovery query. This service never creates a native process.
use crate::discovery::{ClaudeCodeCandidate, discover_candidates};
use bcode_plugin_sdk::{NativeServiceContext, ServiceResponse};
use serde::{Deserialize, Serialize};

/// Renderer-neutral passive discovery interface, independent of harness execution.
pub const CLAUDE_DISCOVERY_INTERFACE_ID: &str = "bcode.harness.discovery/v1";
/// Only supported query operation.
pub const CLAUDE_DISCOVERY_OPERATION: &str = "discover";
/// Maximum candidates returned in one non-paginated snapshot.
pub const MAX_DISCOVERY_CANDIDATES: usize = 16;
/// Maximum UTF-8 bytes in each returned canonical path (never truncated).
pub const MAX_DISCOVERY_PATH_BYTES: usize = 1024;
const MAX_REQUEST_BYTES: usize = 1024;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// Schema-one request. Caller-controlled search paths are deliberately not accepted.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeDiscoveryRequest {
    /// Must equal one; unsupported versions fail closed before filesystem discovery.
    pub schema_version: u16,
    /// Nonempty correlation identity, at most 128 bytes, without whitespace/control characters.
    pub request_id: String,
}

/// Passive discovery establishes neither compatibility nor authentication readiness.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClaudeDiscoveryEvidence {
    /// No executable/version/status or native credential inspection has occurred.
    Unknown,
}

/// Required setup, not permission to perform that setup automatically.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClaudeDiscoverySetup {
    /// Obtain explicit first-use consent with the configured application scope.
    ExplicitConsent,
    /// Admit the executable and wrapped dependencies with exact-version compatibility evidence.
    ExactInstallationCompatibility,
    /// Explicitly prepare the native-managed harness with minimal settings sources.
    NativeManagedHarnessPreparation,
}

/// Bounded, non-secret metadata only. A path is not a persistent source identity or authority.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeDiscoveryCandidate {
    /// Canonical native executable path, at most 1024 bytes, never executed by discovery.
    pub executable: String,
    /// Canonical existing config namespace, at most 1024 bytes; contents were not read.
    pub config_directory: String,
    /// Always unknown: discovery does not execute `--version` or infer verified flags.
    pub compatibility: ClaudeDiscoveryEvidence,
    /// Always unknown: namespace presence does not imply a logged-in account.
    pub readiness: ClaudeDiscoveryEvidence,
}

/// Schema-one snapshot; not an authentication grant, executable admission or launch plan.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeDiscoveryResponse {
    /// Envelope version, currently one.
    pub schema_version: u16,
    /// Echo of the validated request identity.
    pub request_id: String,
    /// Owning enabled plugin identity.
    pub plugin_id: String,
    /// At most 16 candidates, with bounded paths and no credential data.
    pub candidates: Vec<ClaudeDiscoveryCandidate>,
    /// Candidates were omitted by response bounds. Search itself inspects at most 128 PATH entries.
    pub truncated: bool,
    /// Explicit setup still needed for every discovered candidate.
    pub required_setup: Vec<ClaudeDiscoverySetup>,
}

pub fn service(context: &NativeServiceContext) -> ServiceResponse {
    service_with(context, discover_candidates)
}

fn service_with(
    context: &NativeServiceContext,
    discover: impl FnOnce() -> Vec<ClaudeCodeCandidate>,
) -> ServiceResponse {
    if context.plugin_id != crate::PLUGIN_ID
        || context.request.interface_id != CLAUDE_DISCOVERY_INTERFACE_ID
        || context.request.operation != CLAUDE_DISCOVERY_OPERATION
        || context.request.payload.len() > MAX_REQUEST_BYTES
    {
        return ServiceResponse::error(
            "invalid_request",
            "Unsupported or oversized discovery query",
        );
    }
    let Ok(request) = context.request.payload_json::<ClaudeDiscoveryRequest>() else {
        return ServiceResponse::error("invalid_request", "Invalid typed discovery query");
    };
    if request.schema_version != 1
        || request.request_id.is_empty()
        || request.request_id.len() > 128
        || request
            .request_id
            .chars()
            .any(|value| value.is_whitespace() || value.is_control())
    {
        return ServiceResponse::error("invalid_request", "Invalid discovery identity or version");
    }
    let mut candidates = Vec::new();
    let mut truncated = false;
    for candidate in discover() {
        let (Some(executable), Some(config_directory)) = (
            bounded_path(candidate.executable()),
            bounded_path(candidate.config_directory()),
        ) else {
            truncated = true;
            continue;
        };
        if candidates.len() == MAX_DISCOVERY_CANDIDATES {
            truncated = true;
            break;
        }
        candidates.push(ClaudeDiscoveryCandidate {
            executable,
            config_directory,
            compatibility: ClaudeDiscoveryEvidence::Unknown,
            readiness: ClaudeDiscoveryEvidence::Unknown,
        });
    }
    let response = ClaudeDiscoveryResponse {
        schema_version: 1,
        request_id: request.request_id,
        plugin_id: crate::PLUGIN_ID.into(),
        candidates,
        truncated,
        required_setup: vec![
            ClaudeDiscoverySetup::ExplicitConsent,
            ClaudeDiscoverySetup::ExactInstallationCompatibility,
            ClaudeDiscoverySetup::NativeManagedHarnessPreparation,
        ],
    };
    match serde_json::to_vec(&response) {
        Ok(payload) if payload.len() <= MAX_RESPONSE_BYTES => ServiceResponse::ok(payload),
        _ => ServiceResponse::error("bounds_exceeded", "Discovery response exceeds wire bound"),
    }
}

fn bounded_path(path: &std::path::Path) -> Option<String> {
    let value = path.to_str()?;
    (value.len() <= MAX_DISCOVERY_PATH_BYTES && !value.chars().any(char::is_control))
        .then(|| value.to_owned())
}

#[cfg(test)]
#[path = "discovery_service_tests.rs"]
mod tests;
