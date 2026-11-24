//! Plugin-owned installation discovery using the trusted host launch environment.
//!
//! Discovery searches absolute `PATH` entries for `agy`, probes `agy --version`, and inspects
//! native account readiness via `session/list` without creating a conversation, refreshing
//! tokens, or sending any model request. A namespace that does not exist yet is reported as
//! needing login without launching the ACP server, so discovery never creates it; inspecting an
//! existing namespace lets the native CLI initialize its own bookkeeping files there.
use crate::config::InstanceConfig;
use crate::transport::{AcpServer, Launch};
use crate::{AUTH_PROVIDER_ID, PLUGIN_ID};
use bcode_harness_models::{
    HARNESS_DISCOVER_CANDIDATES_OPERATION, HARNESS_PREPARE_CANDIDATE_OPERATION,
    HARNESS_SCHEMA_VERSION, HarnessAuthBinding, HarnessAuthCustody,
    HarnessCandidateExecutableIdentity, HarnessCandidateReadiness, HarnessCandidatesRequest,
    HarnessCandidatesResponse, HarnessCandidatesResponseKind, HarnessFailure,
    HarnessInstallationCandidate, HarnessInstance, HarnessPrepareCandidateRequest,
    HarnessPrepareCandidateResponse, HarnessPrepareCandidateResponseKind, HarnessPreparedCandidate,
    MAX_HARNESS_CANDIDATES, MAX_HARNESS_WIRE_BYTES,
};
use bcode_plugin_sdk::prelude::{NativeServiceContext, ServiceResponse};
use bcode_provider_auth_models::managed::AuthReadiness;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

const MAX_PATH_BYTES: usize = 16 * 1024;
/// Per-candidate bound for the version probe and readiness inspection.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

struct SearchEnvironment {
    directories: Vec<PathBuf>,
    path_complete: bool,
    home: PathBuf,
    namespace: PathBuf,
}

impl SearchEnvironment {
    fn from_values(
        path: &std::ffi::OsStr,
        home: PathBuf,
        namespace: Option<PathBuf>,
    ) -> Result<Self, HarnessFailure> {
        if path.len() > MAX_PATH_BYTES || !home.is_absolute() {
            return Err(HarnessFailure::Policy);
        }
        let mut directories = Vec::new();
        let mut seen = BTreeSet::new();
        let mut path_complete = true;
        for directory in std::env::split_paths(path) {
            if !directory.is_absolute() {
                path_complete = false;
                continue;
            }
            if seen.insert(directory.as_os_str().to_owned()) {
                directories.push(directory);
            }
        }
        let namespace = namespace.unwrap_or_else(|| home.join(".gemini"));
        if !namespace.is_absolute() {
            return Err(HarnessFailure::Policy);
        }
        Ok(Self {
            directories,
            path_complete,
            home,
            namespace,
        })
    }

    fn trusted_host() -> Result<Self, HarnessFailure> {
        // Refuse ambient API keys rather than silently selecting a different billing route.
        for key in ["GEMINI_API_KEY", "GOOGLE_API_KEY"] {
            if std::env::var_os(key).is_some() {
                return Err(HarnessFailure::Policy);
            }
        }
        let path = std::env::var_os("PATH").ok_or(HarnessFailure::Unavailable)?;
        let home = std::env::var_os("HOME").ok_or(HarnessFailure::Unavailable)?;
        Self::from_values(&path, home.into(), None)
    }
}

fn search_candidates(
    input: &SearchEnvironment,
    mut resolve: impl FnMut(&Path) -> Result<Option<HarnessCandidateExecutableIdentity>, HarnessFailure>,
) -> (Vec<HarnessCandidateExecutableIdentity>, bool) {
    let name = if cfg!(windows) { "agy.exe" } else { "agy" };
    let mut seen = BTreeSet::new();
    let mut found = Vec::new();
    let mut complete = input.path_complete;
    for directory in &input.directories {
        let identity = match resolve(&directory.join(name)) {
            Ok(Some(identity)) => identity,
            Ok(None) => continue,
            Err(_) => {
                complete = false;
                continue;
            }
        };
        if !seen.insert(identity.canonical_path.clone()) {
            continue;
        }
        if found.len() == MAX_HARNESS_CANDIDATES {
            complete = false;
            continue;
        }
        found.push(identity);
    }
    (found, complete)
}

fn executable_identity(
    path: &Path,
) -> Result<Option<HarnessCandidateExecutableIdentity>, HarnessFailure> {
    let canonical = match std::fs::canonicalize(path) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(HarnessFailure::Unavailable),
    };
    let metadata = std::fs::metadata(&canonical).map_err(|_| HarnessFailure::Unavailable)?;
    if !metadata.is_file() {
        return Ok(None);
    }
    #[cfg(unix)]
    let file_key = {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.permissions().mode() & 0o111 == 0 {
            return Ok(None);
        }
        Some(format!("{}:{}", metadata.dev(), metadata.ino()))
    };
    #[cfg(not(unix))]
    let file_key = None;
    let invocation = path.to_str().ok_or(HarnessFailure::Policy)?;
    let canonical_path: String = canonical.to_str().ok_or(HarnessFailure::Policy)?.into();
    Ok(Some(HarnessCandidateExecutableIdentity {
        invocation_path: (invocation != canonical_path).then(|| invocation.to_owned()),
        canonical_path,
        byte_len: metadata.len(),
        modified_unix_nanos: metadata
            .modified()
            .ok()
            .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
            .map(|value| value.as_nanos()),
        file_key,
    }))
}

fn inspect_account(launch: &Launch) -> HarnessCandidateReadiness {
    if !launch.gemini_dir.is_dir() {
        return HarnessCandidateReadiness::LoginRequired;
    }
    let result = AcpServer::spawn(launch).and_then(|mut server| {
        server.initialize(launch)?;
        Ok(crate::auth::probe_readiness(&mut server))
    });
    match result {
        Ok(AuthReadiness::Ready | AuthReadiness::Verified) => {
            HarnessCandidateReadiness::ExistingNativeLogin
        }
        Ok(AuthReadiness::LoginRequired) => HarnessCandidateReadiness::LoginRequired,
        Ok(AuthReadiness::Unknown | AuthReadiness::Blocked(_)) | Err(_) => {
            HarnessCandidateReadiness::Unknown
        }
    }
}

fn discover(input: &SearchEnvironment) -> HarnessCandidatesResponseKind {
    let (executables, mut complete) = search_candidates(input, executable_identity);
    let mut candidates = Vec::new();
    for identity in executables {
        let launch = Launch {
            executable: PathBuf::from(identity.invocation()),
            gemini_dir: input.namespace.clone(),
            working_directory: input.home.clone(),
            timeout: PROBE_TIMEOUT,
        };
        let Ok(version) = crate::transport::probe_version(&launch) else {
            complete = false;
            continue;
        };
        let readiness = inspect_account(&launch);
        if executable_identity(&launch.executable)
            .ok()
            .flatten()
            .as_ref()
            != Some(&identity)
        {
            complete = false;
            continue;
        }
        let Some(config_namespace) = input.namespace.to_str() else {
            complete = false;
            continue;
        };
        candidates.push(HarnessInstallationCandidate {
            plugin_id: PLUGIN_ID.into(),
            executable: identity,
            native_version: version,
            config_namespace: config_namespace.into(),
            custody: HarnessAuthCustody::NativeManaged,
            readiness,
        });
    }
    HarnessCandidatesResponseKind::Candidates {
        candidates,
        complete,
    }
}

fn prepare_from_discovery(
    request: &HarnessPrepareCandidateRequest,
    fresh: HarnessCandidatesResponseKind,
) -> Result<HarnessPreparedCandidate, HarnessFailure> {
    request
        .validate()
        .map_err(|_| HarnessFailure::InvalidRequest)?;
    if request.candidate.plugin_id != PLUGIN_ID {
        return Err(HarnessFailure::InvalidRequest);
    }
    let HarnessCandidatesResponseKind::Candidates { candidates, .. } = fresh else {
        return Err(HarnessFailure::Unavailable);
    };
    let candidate = candidates
        .into_iter()
        .find(|candidate| candidate == &request.candidate)
        .ok_or(HarnessFailure::Conflict)?;
    let instance = HarnessInstance {
        instance_id: request.instance_id.clone(),
        plugin_id: PLUGIN_ID.into(),
        executable: candidate.executable.invocation().to_owned(),
        config_namespace: candidate.config_namespace.clone(),
        auth: HarnessAuthBinding {
            provider_id: AUTH_PROVIDER_ID.into(),
            profile: "native".into(),
            credential_namespace: candidate.config_namespace.clone(),
            custody: HarnessAuthCustody::NativeManaged,
        },
    };
    instance
        .validate()
        .map_err(|_| HarnessFailure::InvalidRequest)?;
    let adapter_instance = InstanceConfig {
        executable: instance.executable.clone(),
        gemini_dir: instance.config_namespace.clone(),
        exact_version: candidate.native_version.clone(),
    };
    let adapter_settings =
        serde_json::to_value(&adapter_instance).map_err(|_| HarnessFailure::InvalidRequest)?;
    let decoded: InstanceConfig = serde_json::from_value(adapter_settings.clone())
        .map_err(|_| HarnessFailure::InvalidRequest)?;
    decoded
        .validate_for(&instance)
        .map_err(|_| HarnessFailure::Policy)?;
    Ok(HarnessPreparedCandidate {
        candidate,
        instance,
        adapter_settings,
    })
}

/// Serve `bcode.harness.candidates/v1` (`discover_candidates` and `prepare_candidate`).
pub fn candidates_service(context: &NativeServiceContext) -> ServiceResponse {
    if context.request.payload.len() > MAX_HARNESS_WIRE_BYTES {
        return ServiceResponse::error("invalid_request", "Candidates request exceeds bounds");
    }
    if context.cancellation.is_cancelled() {
        return ServiceResponse::error("cancelled", "Candidates request cancelled");
    }
    match context.request.operation.as_str() {
        HARNESS_DISCOVER_CANDIDATES_OPERATION => {
            let Ok(request) = context.request.payload_json::<HarnessCandidatesRequest>() else {
                return ServiceResponse::error("invalid_request", "Invalid candidates request");
            };
            if request.validate().is_err() {
                return ServiceResponse::error("invalid_request", "Invalid candidates request");
            }
            let kind = match SearchEnvironment::trusted_host() {
                Ok(env) => discover(&env),
                Err(failure) => HarnessCandidatesResponseKind::Failure(failure),
            };
            let response = HarnessCandidatesResponse {
                schema_version: HARNESS_SCHEMA_VERSION,
                request_id: request.request_id.clone(),
                plugin_id: PLUGIN_ID.into(),
                kind,
            };
            if response.validate_for(&request, PLUGIN_ID).is_err() {
                return ServiceResponse::error(
                    "invalid_response",
                    "Candidates response failed validation",
                );
            }
            ServiceResponse::json(&response)
                .unwrap_or_else(|_| ServiceResponse::error("encode_failed", "Encoding failed"))
        }
        HARNESS_PREPARE_CANDIDATE_OPERATION => {
            let Ok(request) = context
                .request
                .payload_json::<HarnessPrepareCandidateRequest>()
            else {
                return ServiceResponse::error("invalid_request", "Invalid prepare request");
            };
            if request.validate().is_err() {
                return ServiceResponse::error("invalid_request", "Invalid prepare request");
            }
            let kind = match SearchEnvironment::trusted_host() {
                Ok(env) => match prepare_from_discovery(&request, discover(&env)) {
                    Ok(prepared) => {
                        HarnessPrepareCandidateResponseKind::Prepared(Box::new(prepared))
                    }
                    Err(failure) => HarnessPrepareCandidateResponseKind::Failure(failure),
                },
                Err(failure) => HarnessPrepareCandidateResponseKind::Failure(failure),
            };
            let response = HarnessPrepareCandidateResponse {
                schema_version: HARNESS_SCHEMA_VERSION,
                request_id: request.request_id.clone(),
                plugin_id: PLUGIN_ID.into(),
                kind,
            };
            if response.validate_for(&request, PLUGIN_ID).is_err() {
                return ServiceResponse::error(
                    "invalid_response",
                    "Prepare candidate response failed validation",
                );
            }
            ServiceResponse::json(&response)
                .unwrap_or_else(|_| ServiceResponse::error("encode_failed", "Encoding failed"))
        }
        _ => ServiceResponse::error("unsupported_operation", "Unsupported candidates operation"),
    }
}
