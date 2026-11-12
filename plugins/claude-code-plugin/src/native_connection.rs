//! Generic native-account onboarding boundary. No vendor credentials leave native custody.
use crate::{config::InstanceConfig, discovery::ClaudeCodeCandidate};
use bcode_harness_models::{
    HarnessFailure, HarnessInstance,
    native_connections::{
        MAX_NATIVE_CONNECTION_BYTES, MAX_NATIVE_CONNECTIONS, NATIVE_CONNECTION_LOOKUP_INTERFACE,
        NativeCandidate, NativeConnectionError, NativeConnectionLookupRequest,
        NativeConnectionLookupResponse, NativePreparationResponse, NativePrepareRequest,
        NativePreparedConnection,
    },
};
use bcode_plugin_sdk::{NativeServiceContext, ServiceResponse};
use sha2::{Digest, Sha256};
use std::{fs::File, io::Read};

const MAX_EXECUTABLE_BYTES: u64 = 512 * 1024 * 1024;
const PINNED_EXECUTABLE: &str =
    "/nix/store/h49lmqqldcrdhcj38pj9gx0q19wzyr6m-claude-code-with-ca-certs/bin/claude";
const PINNED_WRAPPER_SHA256: &str =
    "f950fad226db7a6a5d41aa667fe81248c09dce5b279fa793e2500046177a6e68";
const PINNED_PAYLOAD: &str =
    "/nix/store/yigdqk8129l4zjwrrwrkcjy1vq522sh5-claude-code-2.1.308/bin/claude";
const PINNED_PAYLOAD_SHA256: &str =
    "5889e5043a4c5abab39f555e406128d1f6fcfcef211a8adec12386cefd7bb746";
const PINNED_VERSION: &str = "2.1.308 (Claude Code)";
const PROTOCOL_PROFILE: &str = "claude-2.1.308-stream-unknown-settlement-v1";
const ISOLATION_PROFILE: &str = "empty-optional-settings-native-default-custody-v1";
const COMPATIBILITY_EVIDENCE: &str = "claude-2.1.308-h49lm-yigdq-status-initialize-limited-v1";

/// Cryptographic installation provenance includes the canonical executable and namespace.
/// No settings or credential files are opened.
fn candidate(source: &ClaudeCodeCandidate) -> Option<NativeCandidate> {
    let executable = source.executable().to_str()?.to_owned();
    let config_namespace = source.config_directory().to_str()?.to_owned();
    let mut file = File::open(source.executable()).ok()?;
    if file.metadata().ok()?.len() > MAX_EXECUTABLE_BYTES {
        return None;
    }
    let mut hash = Sha256::new();
    let mut remaining = MAX_EXECUTABLE_BYTES + 1;
    let mut buffer = [0; 8192];
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        remaining = remaining.checked_sub(u64::try_from(read).ok()?)?;
        hash.update(&buffer[..read]);
    }
    if remaining == 0 {
        return None;
    }
    let installation_id = format!("sha256:{}", hex_digest(&hash.finalize()));
    let mut binding = Sha256::new();
    for part in [
        crate::PLUGIN_ID,
        &executable,
        &config_namespace,
        &installation_id,
    ] {
        binding.update(part.as_bytes());
        binding.update([0]);
    }
    let value = NativeCandidate {
        candidate_id: format!("claude-{}", hex_digest(&binding.finalize())),
        plugin_id: crate::PLUGIN_ID.into(),
        executable,
        config_namespace,
        installation_id,
    };
    value.validate().ok()?;
    Some(value)
}

fn hex_digest(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

fn discover() -> Vec<NativeCandidate> {
    crate::discovery::discover_candidates()
        .iter()
        .filter_map(candidate)
        .take(MAX_NATIVE_CONNECTIONS)
        .collect()
}

/// Constrained profile: status and stream initialization were observed on this exact
/// wrapper/payload pair. No login, authoritative settlement or durable recovery is claimed.
fn prepared_config(
    connection: &NativePreparedConnection,
) -> Result<InstanceConfig, HarnessFailure> {
    if connection.validate().is_err()
        || connection.protocol_profile != PROTOCOL_PROFILE
        || connection.isolation_profile != ISOLATION_PROFILE
        || connection.compatibility_evidence != COMPATIBILITY_EVIDENCE
        || !pinned_candidate(&connection.candidate)
        || connection.instance.instance_id != connection.candidate.candidate_id
        || connection.instance.auth.provider_id != "claude-code"
        || connection.instance.auth.profile != "native"
    {
        return Err(HarnessFailure::Unsupported);
    }
    Ok(InstanceConfig {
        billing: crate::config::ApiBilling::Native,
        executable: connection.candidate.executable.clone().into(),
        config_directory: connection.candidate.config_namespace.clone().into(),
        exact_version: PINNED_VERSION.into(),
        verified_session_state: false,
        verified_native_auth_status: true,
        verified_native_auth: false,
        setting_sources: Some(Vec::new()),
    })
}

fn file_digest(path: &std::path::Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    if file.metadata().ok()?.len() > MAX_EXECUTABLE_BYTES {
        return None;
    }
    let mut hash = Sha256::new();
    let mut remaining = MAX_EXECUTABLE_BYTES + 1;
    let mut bytes = [0; 8192];
    loop {
        let count = file.read(&mut bytes).ok()?;
        if count == 0 {
            break;
        }
        remaining = remaining.checked_sub(u64::try_from(count).ok()?)?;
        hash.update(&bytes[..count]);
    }
    (remaining > 0).then(|| hex_digest(&hash.finalize()))
}

fn pinned_candidate(candidate: &NativeCandidate) -> bool {
    // Only the native default namespace is established for account custody on this
    // installation. Explicit overrides, even to the same path, change native keychain identity.
    let Some(home) = std::env::var_os("HOME") else {
        return false;
    };
    let default = std::path::PathBuf::from(home)
        .join(".claude")
        .canonicalize()
        .ok();
    candidate.executable == PINNED_EXECUTABLE
        && default.as_deref().and_then(std::path::Path::to_str)
            == Some(candidate.config_namespace.as_str())
        && candidate.installation_id == format!("sha256:{PINNED_WRAPPER_SHA256}")
        && file_digest(std::path::Path::new(PINNED_EXECUTABLE)).as_deref()
            == Some(PINNED_WRAPPER_SHA256)
        && std::path::Path::new(PINNED_EXECUTABLE)
            .with_file_name(".claude-wrapped_")
            .canonicalize()
            .ok()
            .as_deref()
            == Some(std::path::Path::new(PINNED_PAYLOAD))
        && file_digest(std::path::Path::new(PINNED_PAYLOAD)).as_deref()
            == Some(PINNED_PAYLOAD_SHA256)
}

fn connection(candidate: NativeCandidate) -> NativePreparedConnection {
    use bcode_harness_models::{HarnessAuthBinding, HarnessAuthCustody};
    NativePreparedConnection {
        instance: HarnessInstance {
            instance_id: candidate.candidate_id.clone(),
            plugin_id: crate::PLUGIN_ID.into(),
            executable: candidate.executable.clone(),
            config_namespace: candidate.config_namespace.clone(),
            auth: HarnessAuthBinding {
                provider_id: "claude-code".into(),
                profile: "native".into(),
                credential_namespace: candidate.config_namespace.clone(),
                custody: HarnessAuthCustody::NativeManaged,
            },
        },
        candidate,
        protocol_profile: PROTOCOL_PROFILE.into(),
        isolation_profile: ISOLATION_PROFILE.into(),
        compatibility_evidence: COMPATIBILITY_EVIDENCE.into(),
    }
}

pub fn service(context: &NativeServiceContext) -> ServiceResponse {
    let response = if context.cancellation.is_cancelled() {
        NativePreparationResponse::Failed {
            error: NativeConnectionError::Unavailable,
        }
    } else if context.request.payload.len() > MAX_NATIVE_CONNECTION_BYTES {
        NativePreparationResponse::Failed {
            error: NativeConnectionError::BoundsExceeded,
        }
    } else {
        match context.request.operation.as_str() {
            "discover"
                if context.request.payload_json::<serde_json::Value>().ok()
                    == Some(serde_json::json!({})) =>
            {
                NativePreparationResponse::Candidates {
                    candidates: discover(),
                }
            }
            "prepare" => prepare(context),
            _ => NativePreparationResponse::Failed {
                error: NativeConnectionError::InvalidRequest,
            },
        }
    };
    match serde_json::to_vec(&response) {
        Ok(payload) if payload.len() <= MAX_NATIVE_CONNECTION_BYTES => ServiceResponse::ok(payload),
        _ => ServiceResponse::error(
            "bounds_exceeded",
            "Native preparation response exceeds bound",
        ),
    }
}

fn prepare(context: &NativeServiceContext) -> NativePreparationResponse {
    let Ok(request) = context.request.payload_json::<NativePrepareRequest>() else {
        return NativePreparationResponse::Failed {
            error: NativeConnectionError::InvalidRequest,
        };
    };
    if request.validate().is_err() || request.candidate.plugin_id != crate::PLUGIN_ID {
        return NativePreparationResponse::Failed {
            error: NativeConnectionError::InvalidRequest,
        };
    }
    // Re-discovery verifies exact canonical paths, namespace and executable digest. Consent
    // is exclusively host-owned: this service never caches approval or accepts verified flags.
    prepare_candidate(&request, &discover())
}

fn prepare_candidate(
    request: &NativePrepareRequest,
    current: &[NativeCandidate],
) -> NativePreparationResponse {
    if !current.contains(&request.candidate) {
        return NativePreparationResponse::Failed {
            error: NativeConnectionError::Conflict,
        };
    }
    if !pinned_candidate(&request.candidate) {
        return NativePreparationResponse::Failed {
            error: NativeConnectionError::CompatibilityUnavailable,
        };
    }
    let prepared = connection(request.candidate.clone());
    let Ok(config) = prepared_config(&prepared) else {
        return NativePreparationResponse::Failed {
            error: NativeConnectionError::CompatibilityUnavailable,
        };
    };
    let probe = || -> Result<(), ()> {
        let workspace = tempfile::tempdir().map_err(|_| ())?;
        let mut options = config
            .options(&prepared.instance, workspace.path().into(), None)
            .map_err(|_| ())?;
        options.limits.request_timeout = std::time::Duration::from_secs(15);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| ())?;
        let status = runtime
            .block_on(bcode_claude_code::auth_status(&options))
            .map_err(|_| ())?;
        if !status.logged_in || status.method != bcode_claude_code::AuthMethod::ClaudeAccount {
            return Err(());
        }
        Ok(())
    };
    if probe().is_err() {
        return NativePreparationResponse::Failed {
            error: NativeConnectionError::Unavailable,
        };
    }
    NativePreparationResponse::Prepared {
        request_id: request.request_id.clone(),
        connection: Box::new(prepared),
    }
}

/// Resolve only host-published consent, never a plugin-local approval cache or declarative write.
pub fn resolve(
    context: &NativeServiceContext,
    instance: &HarnessInstance,
) -> Result<InstanceConfig, HarnessFailure> {
    use bcode_plugin_sdk::{ServiceBridgeRequest, ServiceBridgeResponse};
    let request = NativeConnectionLookupRequest::Resolve {
        instance_id: instance.instance_id.clone(),
    };
    let response = context
        .bridge
        .request(&ServiceBridgeRequest::InvokeService(
            bcode_tool::ToolInvocationServiceRequest {
                invocation_id: instance.instance_id.clone(),
                request_id: "claude-native-resolve".into(),
                route_id: None,
                interface_id: NATIVE_CONNECTION_LOOKUP_INTERFACE.into(),
                operation: "lookup".into(),
                payload: serde_json::to_value(request).map_err(|_| HarnessFailure::Unavailable)?,
            },
        ))
        .map_err(|_| HarnessFailure::Unavailable)?;
    let ServiceBridgeResponse::Service(bcode_tool::ToolInvocationServiceResolution::Responded {
        payload,
    }) = response
    else {
        return Err(HarnessFailure::Unavailable);
    };
    if serde_json::to_vec(&payload)
        .map_err(|_| HarnessFailure::Unavailable)?
        .len()
        > MAX_NATIVE_CONNECTION_BYTES
    {
        return Err(HarnessFailure::Unavailable);
    }
    let response: NativeConnectionLookupResponse =
        serde_json::from_value(payload).map_err(|_| HarnessFailure::Unavailable)?;
    let [record] = response.records.as_slice() else {
        return Err(HarnessFailure::Unavailable);
    };
    record.validate().map_err(|_| HarnessFailure::Policy)?;
    let connection = record
        .prepared
        .as_ref()
        .filter(|value| record.approved && value.instance == *instance)
        .ok_or(HarnessFailure::Policy)?;
    if !discover().contains(&connection.candidate) {
        return Err(HarnessFailure::Policy);
    }
    prepared_config(connection)
}

#[cfg(test)]
#[path = "native_connection_execution_tests.rs"]
mod execution_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_harness_models::native_connections::NATIVE_PREPARATION_INTERFACE;

    #[cfg(unix)]
    #[test]
    fn provenance_is_content_and_namespace_bound_without_running_candidate() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().expect("fixture");
        let home = root.path().join("home");
        let bin = root.path().join("bin");
        std::fs::create_dir_all(home.join(".claude")).expect("namespace");
        std::fs::create_dir_all(&bin).expect("bin");
        let executable = bin.join("claude");
        // Discovery/preparation must not run this deliberately failing executable.
        std::fs::write(&executable, "#!/bin/sh\nexit 99\n").expect("fake");
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
            .expect("mode");
        let input = crate::discovery::ClaudeCodeDiscovery {
            home,
            search_path: bin.into_os_string(),
            config_directory: None,
        };
        let discover = |input: &crate::discovery::ClaudeCodeDiscovery| {
            candidate(&crate::discovery::discover_candidates_in(input)[0])
                .expect("portable candidate")
        };
        let first = discover(&input);
        assert_eq!(first, discover(&input));
        std::fs::write(&executable, "#!/bin/sh\nexit 98\n").expect("changed artifact");
        let second = discover(&input);
        assert_ne!(first.installation_id, second.installation_id);
        assert_ne!(first.candidate_id, second.candidate_id);
        let other_namespace = root.path().join("isolated");
        std::fs::create_dir(&other_namespace).expect("other namespace");
        let third = discover(&crate::discovery::ClaudeCodeDiscovery {
            config_directory: Some(other_namespace),
            ..input
        });
        assert_eq!(second.installation_id, third.installation_id);
        assert_ne!(second.candidate_id, third.candidate_id);
    }

    #[test]
    fn fabricated_profile_never_enables_verified_capabilities() {
        let candidate = NativeCandidate {
            candidate_id: "synthetic".into(),
            plugin_id: crate::PLUGIN_ID.into(),
            executable: "/synthetic/claude".into(),
            config_namespace: "/synthetic/config".into(),
            installation_id: "sha256:synthetic".into(),
        };
        let mut prepared = connection(candidate);
        prepared.protocol_profile = "verified-everything".into();
        assert!(matches!(
            prepared_config(&prepared),
            Err(HarnessFailure::Unsupported)
        ));
    }

    #[test]
    #[ignore = "explicit installed status-only preparation authorization required"]
    fn installed_pinned_preparation_and_reconstruction() {
        assert_eq!(
            std::env::var("BCODE_CLAUDE_PREPARATION_PROBE").as_deref(),
            Ok("authorized")
        );
        let candidate = discover()
            .into_iter()
            .find(pinned_candidate)
            .expect("exact installation");
        let request = NativePrepareRequest {
            schema_version: 1,
            request_id: "installed-prepare".into(),
            candidate: candidate.clone(),
        };
        let NativePreparationResponse::Prepared { connection, .. } =
            prepare_candidate(&request, &[candidate])
        else {
            panic!("bounded native preparation failed");
        };
        let config = prepared_config(&connection).expect("reconstructed plugin profile");
        assert!(!config.verified_session_state);
        assert!(!config.verified_native_auth);
        assert!(config.verified_native_auth_status);
        assert_eq!(config.setting_sources, Some(Vec::new()));
    }

    #[test]
    fn exact_candidate_still_requires_execution_and_isolation_evidence() {
        let candidate = NativeCandidate {
            candidate_id: "candidate".into(),
            plugin_id: crate::PLUGIN_ID.into(),
            executable: "/synthetic/claude".into(),
            config_namespace: "/synthetic/config".into(),
            installation_id: "sha256:synthetic".into(),
        };
        let request = NativePrepareRequest {
            schema_version: 1,
            request_id: "probe".into(),
            candidate: candidate.clone(),
        };
        assert!(matches!(
            prepare_candidate(&request, std::slice::from_ref(&candidate)),
            NativePreparationResponse::Failed {
                error: NativeConnectionError::CompatibilityUnavailable
            }
        ));
        let mut changed = candidate;
        changed.installation_id = "sha256:changed".into();
        assert!(matches!(
            prepare_candidate(&request, &[changed]),
            NativePreparationResponse::Failed {
                error: NativeConnectionError::Conflict
            }
        ));
    }

    #[test]
    fn unproven_preparation_never_publishes() {
        let context = NativeServiceContext {
            request: bcode_plugin_sdk::ServiceRequest { interface_id: NATIVE_PREPARATION_INTERFACE.into(), operation: "prepare".into(), payload: br#"{"schema_version":1,"verified":true}"#.to_vec() },
            ..serde_json::from_value(serde_json::json!({"plugin_id":crate::PLUGIN_ID,"request":{"interface_id":"unused","operation":"unused","payload":[]}})).expect("context")
        };
        let response = service(&context);
        assert!(matches!(
            serde_json::from_slice::<NativePreparationResponse>(&response.payload)
                .expect("typed reply"),
            NativePreparationResponse::Failed {
                error: NativeConnectionError::InvalidRequest
            }
        ));
    }
}
