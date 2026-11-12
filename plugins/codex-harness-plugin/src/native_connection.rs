//! Passive installation discovery and consent-gated native status preparation.
//!
//! This module never reads credentials. Account status is read with refresh disabled.
use crate::transport::{AppServer, CodexError, Launch};
use bcode_harness_models::HarnessInstance;
use bcode_harness_models::native_connections::{
    MAX_NATIVE_CONNECTION_BYTES, NATIVE_CONNECTION_LOOKUP_INTERFACE, NativeCandidate,
    NativeConnectionError, NativeConnectionLookupRequest, NativeConnectionLookupResponse,
    NativePreparationResponse, NativePrepareRequest, NativePreparedConnection,
};
use bcode_plugin_sdk::{NativeServiceContext, ServiceResponse};
use sha2::{Digest as _, Sha256};
use std::io::Read as _;
use std::path::{Path, PathBuf};

/// Exact policy identity, independent of the installed CLI's version string.
/// No prepared connection is published until settings isolation is evidenced.
pub const NATIVE_POLICY: &str = "codex-jsonl-native-status-no-refresh-env-allowlist-v1";
const MAX_EXECUTABLE_BYTES: u64 = 256 * 1024 * 1024;

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|byte| {
            [
                char::from(HEX[usize::from(byte >> 4)]),
                char::from(HEX[usize::from(byte & 15)]),
            ]
        })
        .collect()
}

/// Resolve and fingerprint an explicitly selected installation without executing it.
///
/// # Errors
/// Returns unavailable for non-files, oversized executables or unverifiable paths.
pub fn candidate(
    executable: &Path,
    namespace: &Path,
) -> Result<NativeCandidate, NativeConnectionError> {
    if !executable.is_absolute() || !namespace.is_absolute() {
        return Err(NativeConnectionError::InvalidRequest);
    }
    let executable = executable
        .canonicalize()
        .map_err(|_| NativeConnectionError::Unavailable)?;
    let namespace = namespace
        .canonicalize()
        .map_err(|_| NativeConnectionError::Unavailable)?;
    if !namespace.is_dir() {
        return Err(NativeConnectionError::Unavailable);
    }
    let file = std::fs::File::open(&executable).map_err(|_| NativeConnectionError::Unavailable)?;
    let metadata = file
        .metadata()
        .map_err(|_| NativeConnectionError::Unavailable)?;
    if !metadata.is_file() || metadata.len() > MAX_EXECUTABLE_BYTES {
        return Err(NativeConnectionError::Unavailable);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(NativeConnectionError::Unavailable);
        }
    }
    let mut hash = Sha256::new();
    let mut reader = file.take(MAX_EXECUTABLE_BYTES + 1);
    let mut bytes = 0_u64;
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|_| NativeConnectionError::Unavailable)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
        bytes += count as u64;
    }
    if bytes != metadata.len() {
        return Err(NativeConnectionError::Conflict);
    }
    let installation_id = format!("sha256:{}", hex_digest(&hash.finalize()));
    let executable = executable
        .to_str()
        .ok_or(NativeConnectionError::Unavailable)?
        .to_owned();
    let config_namespace = namespace
        .to_str()
        .ok_or(NativeConnectionError::Unavailable)?
        .to_owned();
    let binding = serde_json::to_vec(&(
        &executable,
        &config_namespace,
        &installation_id,
        NATIVE_POLICY,
    ))
    .map_err(|_| NativeConnectionError::InvalidRequest)?;
    let value = NativeCandidate {
        candidate_id: format!("codex:{}", hex_digest(&Sha256::digest(&binding))),
        plugin_id: "bcode.codex-harness".to_owned(),
        executable,
        config_namespace,
        installation_id,
    };
    value
        .validate()
        .map_err(|_| NativeConnectionError::InvalidRequest)?;
    Ok(value)
}

/// Passive bounded default-installation discovery. Does not read native settings,
/// credentials, status, or ambient `CODEX_HOME`. No PATH-selected executable is run.
#[must_use]
pub fn discover_native() -> Vec<NativeCandidate> {
    let Some(home) = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    else {
        return Vec::new();
    };
    let namespace = home.join(".codex");
    let paths = [
        home.join(".nix-profile/bin/codex"),
        home.file_name().map_or_else(PathBuf::new, |name| {
            Path::new("/etc/profiles/per-user")
                .join(name)
                .join("bin/codex")
        }),
        home.join(".local/bin/codex"),
        PathBuf::from("/opt/homebrew/bin/codex"),
        PathBuf::from("/usr/local/bin/codex"),
    ];
    let mut candidates = Vec::new();
    for path in paths {
        if let Ok(value) = candidate(&path, &namespace)
            && !candidates.contains(&value)
        {
            candidates.push(value);
        }
    }
    candidates
}

/// Revalidate consent identity without launching an unsupported installation.
///
/// # Errors
/// Returns conflict on installation drift, otherwise compatibility-unavailable:
/// no settings-source isolation profile is supported. Status inspection is a
/// separate explicitly authorized operation, never preparation authority.
pub fn prepare_native(
    request: &NativePrepareRequest,
) -> Result<NativePreparedConnection, NativeConnectionError> {
    if request.validate().is_err() || request.candidate.plugin_id != "bcode.codex-harness" {
        return Err(NativeConnectionError::InvalidRequest);
    }
    let current = candidate(
        Path::new(&request.candidate.executable),
        Path::new(&request.candidate.config_namespace),
    )?;
    if current != request.candidate {
        return Err(NativeConnectionError::Conflict);
    }
    // No execution profile is supported. Do not launch an account inspection
    // process with inherited native settings just to rediscover that fact.
    // Explicitly authorized status checks use inspect_native_status separately.
    Err(NativeConnectionError::CompatibilityUnavailable)
}

/// Inspect native-account presence through a dedicated bounded process.
///
/// The caller must authorize this exact launch; this does not establish execution
/// readiness or settings isolation. No login or refresh request is issued.
///
/// # Errors
/// Rejects transport/protocol failures, namespace mismatch and non-native accounts.
pub fn inspect_native_status(launch: &Launch) -> Result<(), CodexError> {
    let mut server = AppServer::spawn(launch)?;
    server.initialize(launch)?;
    let mut result = server.call("account/read", serde_json::json!({"refreshToken":false}))?;
    let native_account = result["account"]["type"].as_str() == Some("chatgpt");
    crate::transport::wipe_value(&mut result);
    if !native_account {
        return Err(CodexError::Unsupported);
    }
    Ok(())
}

pub(crate) fn handle_native(context: &NativeServiceContext) -> ServiceResponse {
    if context.cancellation.is_cancelled() || context.request.payload.len() > 16 * 1024 {
        return ServiceResponse::error("invalid_request", "Invalid native preparation request");
    }
    let response = match context.request.operation.as_str() {
        "discover"
            if context
                .request
                .payload_json::<serde_json::Value>()
                .is_ok_and(|value| value == serde_json::json!({})) =>
        {
            NativePreparationResponse::Candidates {
                candidates: discover_native(),
            }
        }
        "prepare" => {
            let Ok(request) = context.request.payload_json::<NativePrepareRequest>() else {
                return ServiceResponse::error(
                    "invalid_request",
                    "Invalid native preparation request",
                );
            };
            match prepare_native(&request) {
                Ok(connection) => NativePreparationResponse::Prepared {
                    request_id: request.request_id,
                    connection: Box::new(connection),
                },
                Err(error) => NativePreparationResponse::Failed { error },
            }
        }
        _ => {
            return ServiceResponse::error(
                "invalid_request",
                "Unsupported native preparation operation",
            );
        }
    };
    ServiceResponse::json(&response)
        .unwrap_or_else(|_| ServiceResponse::error("unavailable", "Native preparation unavailable"))
}

/// Resolve an exact native instance from the authenticated host publication boundary.
/// No declarative instance or request-carried preparation can substitute for consent.
///
/// # Errors
/// Rejects missing, revoked, foreign, conflicting, stale or unsupported preparations.
pub fn resolve_native_launch(
    context: &NativeServiceContext,
    instance: &HarnessInstance,
    working_directory: &Path,
) -> Result<Launch, CodexError> {
    use bcode_plugin_sdk::{ServiceBridgeRequest, ServiceBridgeResponse};
    let response = context
        .bridge
        .request(&ServiceBridgeRequest::InvokeService(
            bcode_tool::ToolInvocationServiceRequest {
                invocation_id: instance.instance_id.clone(),
                request_id: "codex-native-resolve".to_owned(),
                route_id: None,
                interface_id: NATIVE_CONNECTION_LOOKUP_INTERFACE.to_owned(),
                operation: "lookup".to_owned(),
                payload: serde_json::to_value(NativeConnectionLookupRequest::Resolve {
                    instance_id: instance.instance_id.clone(),
                })
                .map_err(|_| CodexError::Configuration)?,
            },
        ))
        .map_err(|_| CodexError::Disconnected)?;
    let ServiceBridgeResponse::Service(bcode_tool::ToolInvocationServiceResolution::Responded {
        payload,
    }) = response
    else {
        return Err(CodexError::Disconnected);
    };
    if serde_json::to_vec(&payload)
        .map_err(|_| CodexError::Protocol)?
        .len()
        > MAX_NATIVE_CONNECTION_BYTES
    {
        return Err(CodexError::Bounds);
    }
    let response: NativeConnectionLookupResponse =
        serde_json::from_value(payload).map_err(|_| CodexError::Protocol)?;
    let [record] = response.records.as_slice() else {
        return Err(CodexError::Rejected);
    };
    record.validate().map_err(|_| CodexError::Configuration)?;
    let connection = record
        .prepared
        .as_ref()
        .filter(|prepared| record.approved && prepared.instance == *instance)
        .ok_or(CodexError::Rejected)?;
    prepared_launch(connection, working_directory)
}

fn prepared_launch(
    connection: &NativePreparedConnection,
    working_directory: &Path,
) -> Result<Launch, CodexError> {
    connection
        .validate()
        .map_err(|_| CodexError::Configuration)?;
    if connection.candidate.plugin_id != "bcode.codex-harness" || !working_directory.is_absolute() {
        return Err(CodexError::Configuration);
    }
    let current = candidate(
        Path::new(&connection.candidate.executable),
        Path::new(&connection.candidate.config_namespace),
    )
    .map_err(|_| CodexError::Stale)?;
    if current != connection.candidate {
        return Err(CodexError::Stale);
    }
    // There are currently no evidenced execution profiles. In particular the
    // status-only NATIVE_POLICY is not an execution protocol/isolation profile.
    // Fail even for a structurally valid host record: publication is consent,
    // not proof that this plugin implements the advertised compatibility policy.
    Err(CodexError::Unsupported)
}

#[cfg(test)]
#[path = "native_connection_execution_tests.rs"]
mod execution_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn exact_candidate_drift_and_native_status_never_publish_execution() {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("codex");
        let namespace = temp.path().join(".codex");
        std::fs::create_dir(&namespace).unwrap();
        std::fs::write(
            &executable,
            r"#!/usr/bin/python3
import os,sys,json
if sys.argv[1:] == ['--version']:
 print('codex-cli 0.164.0');sys.exit(0)
assert not any(key in os.environ for key in ['OPENAI_API_KEY','CODEX_API_KEY','OPENAI_BASE_URL','CODEX_REMOTE','HTTP_PROXY','HTTPS_PROXY','DYLD_INSERT_LIBRARIES'])
for line in sys.stdin:
 r=json.loads(line);m=r['method']
 with open(os.path.join(os.environ['CODEX_HOME'],'calls'), 'a') as log: log.write(m+'\n')
 if m=='initialize': result={'codexHome':os.environ['CODEX_HOME']}
 elif m=='initialized': continue
 elif m=='account/read':
  assert r['params']=={'refreshToken':False}
  result={'account':{'type':'chatgpt','email':'must-not-export@example.invalid'}}
 else: sys.exit(2)
 print(json.dumps({'id':r['id'],'result':result}),flush=True)
",
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let original = candidate(&executable, &namespace).unwrap();
        let request = NativePrepareRequest {
            schema_version: 1,
            request_id: "probe".to_owned(),
            candidate: original.clone(),
        };
        assert_eq!(
            prepare_native(&request),
            Err(NativeConnectionError::CompatibilityUnavailable)
        );
        assert!(
            !namespace.join("calls").exists(),
            "unsupported preparation must not spawn"
        );
        let context: NativeServiceContext = serde_json::from_value(serde_json::json!({
            "plugin_id": "bcode.codex-harness",
            "request": {
                "interface_id": bcode_harness_models::native_connections::NATIVE_PREPARATION_INTERFACE,
                "operation": "prepare",
                "payload": serde_json::to_vec(&request).unwrap()
            }
        })).unwrap();
        let response = handle_native(&context);
        assert!(response.error.is_none());
        assert_eq!(
            serde_json::from_slice::<NativePreparationResponse>(&response.payload).unwrap(),
            NativePreparationResponse::Failed {
                error: NativeConnectionError::CompatibilityUnavailable
            }
        );
        assert!(!namespace.join("calls").exists());
        inspect_native_status(&Launch {
            executable: executable.clone(),
            codex_home: namespace.clone(),
            working_directory: temp.path().to_owned(),
            ephemeral_credentials: false,
            timeout: std::time::Duration::from_secs(5),
        })
        .unwrap();
        let calls = std::fs::read_to_string(namespace.join("calls")).unwrap();
        assert_eq!(
            calls.lines().collect::<Vec<_>>(),
            ["initialize", "initialized", "account/read"]
        );
        std::fs::write(&executable, "#!/bin/sh\nexit 99\n").unwrap();
        assert_ne!(candidate(&executable, &namespace).unwrap(), original);
        assert_eq!(
            prepare_native(&request),
            Err(NativeConnectionError::Conflict)
        );
        assert_eq!(
            std::fs::read_to_string(namespace.join("calls")).unwrap(),
            calls
        );
    }
}
