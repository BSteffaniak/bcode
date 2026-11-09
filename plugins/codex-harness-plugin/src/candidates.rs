//! Plugin-owned installation discovery using the trusted host launch environment.
//!
//! Discovery searches absolute `PATH` entries for `codex`, probes the top-level version and
//! inspects native account readiness through `account/read` without refreshing tokens,
//! creating threads or sending any model request.
use crate::config::InstanceConfig;
use crate::transport::{AppServer, CodexError, Launch};
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
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

const MAX_PATH_BYTES: usize = 16 * 1024;
/// Per-candidate bound for the version probe and readiness inspection.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Deliberately injectable search input; tests never read process globals.
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
        // Preserve order and compare raw spellings; never search relative or empty entries.
        for directory in std::env::split_paths(path) {
            if !directory.is_absolute() {
                path_complete = false;
                continue;
            }
            if seen.insert(directory.as_os_str().to_owned()) {
                directories.push(directory);
            }
        }
        // The native CLI documents `CODEX_HOME`, defaulting to `~/.codex`.
        let namespace = namespace.unwrap_or_else(|| home.join(".codex"));
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
        // Deployment-controlled launch inputs, never service request fields. An ambient API
        // key would silently select a different billing route, so refuse rather than guess.
        for key in ["OPENAI_API_KEY", "CODEX_API_KEY"] {
            if std::env::var_os(key).is_some() {
                return Err(HarnessFailure::Policy);
            }
        }
        let path = std::env::var_os("PATH").ok_or(HarnessFailure::Unavailable)?;
        let home = std::env::var_os("HOME").ok_or(HarnessFailure::Unavailable)?;
        Self::from_values(
            &path,
            home.into(),
            std::env::var_os("CODEX_HOME").map(PathBuf::from),
        )
    }
}

fn search_candidates(
    input: &SearchEnvironment,
    mut resolve: impl FnMut(&Path) -> Result<Option<HarnessCandidateExecutableIdentity>, HarnessFailure>,
) -> (Vec<HarnessCandidateExecutableIdentity>, bool) {
    let name = if cfg!(windows) { "codex.exe" } else { "codex" };
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

/// Map a sanitized `account/read` result to setup readiness.
///
/// A signed-in account, or a native configuration that needs no `OpenAI` login (for example a
/// configured custom provider), is ready. An absent account that requires login is not.
pub fn readiness(account: Result<&serde_json::Value, CodexError>) -> HarnessCandidateReadiness {
    match account {
        Ok(value) if value["account"].is_object() => HarnessCandidateReadiness::ExistingNativeLogin,
        Ok(value) if value["requiresOpenaiAuth"] == false => {
            HarnessCandidateReadiness::ExistingNativeLogin
        }
        Ok(value) if value["account"].is_null() && value["requiresOpenaiAuth"] == true => {
            HarnessCandidateReadiness::LoginRequired
        }
        Ok(_) | Err(CodexError::Protocol | CodexError::Configuration) => {
            HarnessCandidateReadiness::Unsupported
        }
        Err(_) => HarnessCandidateReadiness::Unknown,
    }
}

fn inspect_account(launch: &Launch) -> HarnessCandidateReadiness {
    let result = AppServer::spawn(launch).and_then(|mut server| {
        server.initialize(launch)?;
        let mut account =
            server.call("account/read", serde_json::json!({"refreshToken": false}))?;
        let readiness = readiness(Ok(&account));
        crate::transport::wipe_value(&mut account);
        Ok(readiness)
    });
    result.unwrap_or_else(|error| readiness(Err(error)))
}

fn discover(input: &SearchEnvironment) -> HarnessCandidatesResponseKind {
    let (executables, mut complete) = search_candidates(input, executable_identity);
    let mut candidates = Vec::new();
    for identity in executables {
        // Probe through the same path the configured instance will invoke.
        let launch = Launch {
            executable: PathBuf::from(identity.invocation()),
            codex_home: input.namespace.clone(),
            working_directory: input.home.clone(),
            ephemeral_credentials: false,
            timeout: PROBE_TIMEOUT,
        };
        let Ok(version) = crate::transport::probe_version(&launch) else {
            complete = false;
            continue;
        };
        let readiness = inspect_account(&launch);
        // Never publish evidence collected across an observable executable replacement.
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
    let settings = InstanceConfig {
        executable: instance.executable.clone(),
        codex_home: instance.config_namespace.clone(),
        exact_version: candidate.native_version.clone(),
    };
    let adapter_settings =
        serde_json::to_value(&settings).map_err(|_| HarnessFailure::InvalidRequest)?;
    // Keep generated settings compatible with the adapter's own decoder.
    let decoded: InstanceConfig = serde_json::from_value(adapter_settings.clone())
        .map_err(|_| HarnessFailure::InvalidRequest)?;
    decoded
        .validate_for(&instance)
        .map_err(|_| HarnessFailure::InvalidRequest)?;
    Ok(HarnessPreparedCandidate {
        candidate,
        instance,
        adapter_settings,
    })
}

fn fresh_candidates() -> Result<HarnessCandidatesResponseKind, HarnessFailure> {
    Ok(discover(&SearchEnvironment::trusted_host()?))
}

/// Serve `bcode.harness.candidates/v1` (`discover_candidates` and `prepare_candidate`).
pub fn candidates_service(context: &NativeServiceContext) -> ServiceResponse {
    if context.request.operation == HARNESS_PREPARE_CANDIDATE_OPERATION {
        return prepare_candidate_service(context);
    }
    if context.request.operation != HARNESS_DISCOVER_CANDIDATES_OPERATION
        || context.request.payload.len() > 4096
    {
        return ServiceResponse::error("invalid_request", "Invalid candidate discovery operation");
    }
    let Ok(request) = serde_json::from_slice::<HarnessCandidatesRequest>(&context.request.payload)
    else {
        return ServiceResponse::error("invalid_request", "Invalid candidate discovery request");
    };
    if request.validate().is_err() {
        return ServiceResponse::error(
            "invalid_request",
            "Incompatible candidate discovery request",
        );
    }
    let response = HarnessCandidatesResponse {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: request.request_id.clone(),
        plugin_id: PLUGIN_ID.into(),
        kind: fresh_candidates().unwrap_or_else(HarnessCandidatesResponseKind::Failure),
    };
    if response.validate_for(&request, PLUGIN_ID).is_err() {
        return ServiceResponse::error("invalid_response", "Invalid candidate discovery metadata");
    }
    ServiceResponse::json(&response)
        .unwrap_or_else(|_| ServiceResponse::error("invalid_response", "Candidate encoding failed"))
}

fn prepare_candidate_service(context: &NativeServiceContext) -> ServiceResponse {
    if context.request.payload.len() > MAX_HARNESS_WIRE_BYTES {
        return ServiceResponse::error("invalid_request", "Candidate preparation exceeds bounds");
    }
    let Ok(request) =
        serde_json::from_slice::<HarnessPrepareCandidateRequest>(&context.request.payload)
    else {
        return ServiceResponse::error("invalid_request", "Invalid candidate preparation request");
    };
    if request.validate().is_err() || request.candidate.plugin_id != PLUGIN_ID {
        return ServiceResponse::error(
            "invalid_request",
            "Incompatible candidate preparation request",
        );
    }
    // Only fresh trusted search output may be prepared. Never execute a request-supplied path.
    let result = fresh_candidates().and_then(|fresh| prepare_from_discovery(&request, fresh));
    let response = HarnessPrepareCandidateResponse {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: request.request_id.clone(),
        plugin_id: PLUGIN_ID.into(),
        kind: result.map_or_else(HarnessPrepareCandidateResponseKind::Failure, |prepared| {
            HarnessPrepareCandidateResponseKind::Prepared(Box::new(prepared))
        }),
    };
    if response.validate_for(&request, PLUGIN_ID).is_err() {
        return ServiceResponse::error(
            "invalid_response",
            "Invalid candidate preparation metadata",
        );
    }
    ServiceResponse::json(&response).unwrap_or_else(|_| {
        ServiceResponse::error("invalid_response", "Candidate preparation encoding failed")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn identity(path: &str) -> HarnessCandidateExecutableIdentity {
        HarnessCandidateExecutableIdentity {
            canonical_path: path.into(),
            invocation_path: None,
            byte_len: 42,
            modified_unix_nanos: None,
            file_key: Some("1:2".into()),
        }
    }

    fn candidate() -> HarnessInstallationCandidate {
        HarnessInstallationCandidate {
            plugin_id: PLUGIN_ID.into(),
            executable: HarnessCandidateExecutableIdentity {
                invocation_path: Some("/profile/bin/codex".into()),
                ..identity("/store/codex-0.164.0/bin/codex")
            },
            native_version: "0.164.0".into(),
            config_namespace: "/home/test/.codex".into(),
            custody: HarnessAuthCustody::NativeManaged,
            readiness: HarnessCandidateReadiness::ExistingNativeLogin,
        }
    }

    #[cfg(unix)]
    #[test]
    fn profile_symlinks_are_invoked_while_identity_stays_canonical() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join("store/bin");
        let profile = root.path().join("profile/bin");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::create_dir_all(&profile).unwrap();
        let target = store.join("codex");
        std::fs::write(&target, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink(&target, profile.join("codex")).unwrap();
        let identity = executable_identity(&profile.join("codex"))
            .unwrap()
            .unwrap();
        let canonical = std::fs::canonicalize(&target).unwrap();
        assert_eq!(identity.canonical_path, canonical.to_str().unwrap());
        assert_eq!(
            identity.invocation(),
            profile.join("codex").to_str().unwrap()
        );
        assert_eq!(
            executable_identity(&canonical)
                .unwrap()
                .unwrap()
                .invocation_path,
            None
        );
    }

    #[test]
    fn search_skips_relative_entries_deduplicates_and_defaults_codex_home() {
        let input = SearchEnvironment::from_values(
            std::ffi::OsStr::new("/trusted/bin:relative:/other/bin:/trusted/bin"),
            "/home/test".into(),
            None,
        )
        .unwrap();
        assert_eq!(input.namespace, PathBuf::from("/home/test/.codex"));
        let mut inspected = Vec::new();
        let (found, complete) = search_candidates(&input, |path| {
            inspected.push(path.to_owned());
            Ok(Some(identity("/installed/codex")))
        });
        assert_eq!(inspected.len(), 2);
        assert!(inspected.iter().all(|path| path.ends_with("codex")));
        assert_eq!(found, vec![identity("/installed/codex")]);
        assert!(!complete);
        let explicit = SearchEnvironment::from_values(
            std::ffi::OsStr::new("/trusted"),
            "/home/test".into(),
            Some("/explicit/codex-home".into()),
        )
        .unwrap();
        assert_eq!(explicit.namespace, PathBuf::from("/explicit/codex-home"));
        assert!(
            SearchEnvironment::from_values(
                std::ffi::OsStr::new("/trusted"),
                "/home/test".into(),
                Some("relative".into()),
            )
            .is_err()
        );
    }

    #[test]
    fn search_is_bounded_without_hiding_metadata_coverage() {
        let directories: Vec<_> = (0..20)
            .map(|index| PathBuf::from(format!("/bin/{index}")))
            .collect();
        let path = std::env::join_paths(&directories).unwrap();
        let input = SearchEnvironment::from_values(&path, "/home/test".into(), None).unwrap();
        let (found, complete) =
            search_candidates(&input, |path| Ok(Some(identity(path.to_str().unwrap()))));
        assert_eq!(found.len(), MAX_HARNESS_CANDIDATES);
        assert!(!complete);
        let (found, complete) = search_candidates(&input, |_| Err(HarnessFailure::Unavailable));
        assert_eq!(found, Vec::new());
        assert!(!complete);
    }

    #[test]
    fn readiness_reflects_native_account_state_without_guessing() {
        assert_eq!(
            readiness(Ok(
                &json!({"account":{"type":"chatgpt"},"requiresOpenaiAuth":true})
            )),
            HarnessCandidateReadiness::ExistingNativeLogin
        );
        assert_eq!(
            readiness(Ok(&json!({"account":null,"requiresOpenaiAuth":true}))),
            HarnessCandidateReadiness::LoginRequired
        );
        assert_eq!(
            readiness(Ok(&json!({"account":null,"requiresOpenaiAuth":false}))),
            HarnessCandidateReadiness::ExistingNativeLogin
        );
        assert_eq!(
            readiness(Ok(&json!({"unexpected":true}))),
            HarnessCandidateReadiness::Unsupported
        );
        assert_eq!(
            readiness(Err(CodexError::Disconnected)),
            HarnessCandidateReadiness::Unknown
        );
    }

    #[test]
    fn preparation_requires_fresh_exact_candidate_and_decodes_with_the_adapter() {
        let request = HarnessPrepareCandidateRequest {
            schema_version: HARNESS_SCHEMA_VERSION,
            request_id: "prepare-1".into(),
            instance_id: "codex".into(),
            candidate: candidate(),
        };
        let fresh = |candidate| HarnessCandidatesResponseKind::Candidates {
            candidates: vec![candidate],
            complete: true,
        };
        let prepared = prepare_from_discovery(&request, fresh(candidate())).unwrap();
        assert_eq!(prepared.instance.executable, "/profile/bin/codex");
        assert_eq!(prepared.instance.config_namespace, "/home/test/.codex");
        assert_eq!(prepared.instance.auth.provider_id, AUTH_PROVIDER_ID);
        assert_eq!(
            prepared.instance.auth.credential_namespace,
            prepared.instance.config_namespace
        );
        let decoded: InstanceConfig =
            serde_json::from_value(prepared.adapter_settings.clone()).unwrap();
        assert_eq!(decoded.exact_version, "0.164.0");
        assert!(decoded.validate_for(&prepared.instance).is_ok());
        // The adapter's launch selection accepts the prepared instance unchanged.
        assert!(crate::launch(&prepared.instance, "/work").is_ok());
        let response = HarnessPrepareCandidateResponse {
            schema_version: HARNESS_SCHEMA_VERSION,
            request_id: request.request_id.clone(),
            plugin_id: PLUGIN_ID.into(),
            kind: HarnessPrepareCandidateResponseKind::Prepared(Box::new(prepared)),
        };
        assert!(response.validate_for(&request, PLUGIN_ID).is_ok());
        let mut changed = candidate();
        changed.executable.byte_len += 1;
        assert_eq!(
            prepare_from_discovery(&request, fresh(changed)).unwrap_err(),
            HarnessFailure::Conflict
        );
        let mut changed = candidate();
        changed.readiness = HarnessCandidateReadiness::LoginRequired;
        assert_eq!(
            prepare_from_discovery(&request, fresh(changed)).unwrap_err(),
            HarnessFailure::Conflict
        );
    }
}
