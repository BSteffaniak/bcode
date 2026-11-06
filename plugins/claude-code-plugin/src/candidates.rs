//! Plugin-owned installation discovery using the trusted host launch environment.
use super::{
    ClaudeCodePlugin, HARNESS_SCHEMA_VERSION, HarnessFailure, HarnessInstance, InstanceConfig,
    MAX_HARNESS_WIRE_BYTES, NativeServiceContext, PLUGIN_ID, PathBuf, ServiceResponse,
};
use bcode_claude_code::{
    BillingRoute, LaunchOptions, Limits, NativeAuthSupport, NativeEnvironment, SettingsInheritance,
    SettlementSupport,
};
use bcode_harness_models::{
    HarnessAuthCustody, HarnessCandidateExecutableIdentity, HarnessCandidateReadiness,
    HarnessCandidatesRequest, HarnessCandidatesResponse, HarnessCandidatesResponseKind,
    HarnessInstallationCandidate, MAX_HARNESS_CANDIDATES,
};
use std::{collections::BTreeSet, path::Path, time::UNIX_EPOCH};

const MAX_SEARCH_DIRECTORIES: usize = 32;
const MAX_PATH_BYTES: usize = 16 * 1024;

/// Deliberately injectable; never read process globals in the planning/test path.
struct SearchEnvironment {
    directories: Vec<PathBuf>,
    home: PathBuf,
    namespace: PathBuf,
    environment: NativeEnvironment,
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
        let directories: Vec<_> = std::env::split_paths(path).collect();
        if directories.len() > MAX_SEARCH_DIRECTORIES {
            return Err(HarnessFailure::Policy);
        }
        // Native CLI owns this documented default, not the generic configuration plugin.
        let namespace = namespace.unwrap_or_else(|| home.join(".claude"));
        if !namespace.is_absolute() {
            return Err(HarnessFailure::Policy);
        }
        Ok(Self {
            directories,
            home,
            namespace,
            environment: NativeEnvironment::default(),
        })
    }

    fn trusted_host() -> Result<Self, HarnessFailure> {
        // These are deployment-controlled launch inputs, never service request fields.
        // Do not reinterpret an explicitly selected alternative billing route as subscription.
        for key in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_BASE_URL",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
        ] {
            if std::env::var_os(key).is_some() {
                return Err(HarnessFailure::Policy);
            }
        }
        let path = std::env::var_os("PATH").ok_or(HarnessFailure::Unavailable)?;
        let home = std::env::var_os("HOME").ok_or(HarnessFailure::Unavailable)?;
        let mut result = Self::from_values(
            &path,
            home.into(),
            std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from),
        )?;
        for key in [
            "HOME",
            "USER",
            "PATH",
            "TMPDIR",
            "LANG",
            "LC_ALL",
            "SYSTEMROOT",
        ] {
            if let Ok(value) = std::env::var(key) {
                result.environment.0.insert(key.into(), value);
            }
        }
        Ok(result)
    }
}

fn search_candidates(
    input: &SearchEnvironment,
    mut resolve: impl FnMut(&Path) -> Result<Option<HarnessCandidateExecutableIdentity>, HarnessFailure>,
) -> (Vec<HarnessCandidateExecutableIdentity>, bool) {
    let mut seen = BTreeSet::new();
    let mut found = Vec::new();
    let mut complete = true;
    for directory in &input.directories {
        // Never search cwd, empty PATH entries or shell wrappers.
        if !directory.is_absolute() {
            complete = false;
            continue;
        }
        let name = if cfg!(windows) {
            "claude.exe"
        } else {
            "claude"
        };
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
            break;
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
    Ok(Some(HarnessCandidateExecutableIdentity {
        canonical_path: canonical.to_str().ok_or(HarnessFailure::Policy)?.into(),
        byte_len: metadata.len(),
        modified_unix_nanos: metadata
            .modified()
            .ok()
            .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
            .map(|value| value.as_nanos()),
        file_key,
    }))
}

fn readiness(
    status: Result<bcode_claude_code::AuthStatus, bcode_claude_code::Error>,
) -> HarnessCandidateReadiness {
    use bcode_claude_code::AuthMethod;
    match status {
        Ok(status) if status.logged_in && status.method == AuthMethod::ClaudeAccount => {
            HarnessCandidateReadiness::ExistingNativeLogin
        }
        Ok(status) if !status.logged_in && status.method == AuthMethod::None => {
            HarnessCandidateReadiness::LoginRequired
        }
        Err(
            bcode_claude_code::Error::Protocol | bcode_claude_code::Error::InvalidConfiguration(_),
        ) => HarnessCandidateReadiness::Unsupported,
        _ => HarnessCandidateReadiness::Unknown,
    }
}

async fn discover(
    input: SearchEnvironment,
) -> Result<HarnessCandidatesResponseKind, HarnessFailure> {
    let (executables, mut complete) = search_candidates(&input, executable_identity);
    let mut candidates = Vec::new();
    for identity in executables {
        let executable = PathBuf::from(&identity.canonical_path);
        let Ok(version) =
            bcode_claude_code::installed_version(&executable, &input.home, &input.environment)
                .await
        else {
            complete = false;
            continue;
        };
        let options = LaunchOptions {
            executable: executable.clone(),
            expected_version: version.clone(),
            cwd: input.home.clone(),
            config_directory: input.namespace.clone(),
            environment: input.environment.clone(),
            billing_route: BillingRoute::NativeAccount,
            settings: SettingsInheritance::Native,
            setting_sources: Vec::new(),
            resume_session_id: None,
            settlement_support: SettlementSupport::Unknown,
            auth_support: NativeAuthSupport::Unknown,
            limits: Limits {
                request_timeout: std::time::Duration::from_secs(3),
                ..Limits::default()
            },
        };
        let status = readiness(bcode_claude_code::inspect_installed_account(&options).await);
        // Never publish evidence collected across an observable executable replacement.
        if executable_identity(&executable).ok().flatten().as_ref() != Some(&identity) {
            complete = false;
            continue;
        }
        candidates.push(HarnessInstallationCandidate {
            plugin_id: PLUGIN_ID.into(),
            executable: identity,
            native_version: version,
            config_namespace: input
                .namespace
                .to_str()
                .ok_or(HarnessFailure::Policy)?
                .into(),
            custody: HarnessAuthCustody::NativeManaged,
            readiness: status,
        });
    }
    Ok(HarnessCandidatesResponseKind::Candidates {
        candidates,
        complete,
    })
}

fn prepare_from_discovery(
    request: &bcode_harness_models::HarnessPrepareCandidateRequest,
    fresh: HarnessCandidatesResponseKind,
) -> Result<bcode_harness_models::HarnessPreparedCandidate, HarnessFailure> {
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
    // Unknown status cannot confer native-auth support. Preparation is still reviewable,
    // but no capability attestation is guessed from the version string.
    let verified_status = matches!(
        candidate.readiness,
        HarnessCandidateReadiness::ExistingNativeLogin | HarnessCandidateReadiness::LoginRequired
    );
    let instance = HarnessInstance {
        instance_id: request.instance_id.clone(),
        plugin_id: PLUGIN_ID.into(),
        executable: candidate.executable.canonical_path.clone(),
        config_namespace: candidate.config_namespace.clone(),
        auth: bcode_harness_models::HarnessAuthBinding {
            provider_id: "claude-code".into(),
            profile: "native".into(),
            credential_namespace: candidate.config_namespace.clone(),
            custody: HarnessAuthCustody::NativeManaged,
        },
    };
    instance
        .validate()
        .map_err(|_| HarnessFailure::InvalidRequest)?;
    let adapter_settings = serde_json::json!({
        "billing": {"route":"native"},
        "executable": instance.executable,
        "config_directory": instance.config_namespace,
        "exact_version": candidate.native_version,
        "verified_native_auth_status": verified_status,
        "verified_native_auth": false,
        "verified_session_state": false,
        "setting_sources": null,
    });
    // Keep generated settings compatible with the actual native adapter decoder.
    let settings: InstanceConfig = serde_json::from_value(adapter_settings.clone())
        .map_err(|_| HarnessFailure::InvalidRequest)?;
    settings.credential_selection(&instance)?;
    Ok(bcode_harness_models::HarnessPreparedCandidate {
        candidate,
        instance,
        adapter_settings,
    })
}

impl ClaudeCodePlugin {
    fn fresh_candidates(&mut self) -> Result<HarnessCandidatesResponseKind, HarnessFailure> {
        let input = SearchEnvironment::trusted_host()?;
        if self.runtime.is_none() {
            self.runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .ok();
        }
        let runtime = self.runtime.as_ref().ok_or(HarnessFailure::Unavailable)?;
        runtime.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(12), discover(input))
                .await
                .map_err(|_| HarnessFailure::Unavailable)?
        })
    }

    fn prepare_candidate_service(&mut self, context: &NativeServiceContext) -> ServiceResponse {
        use bcode_harness_models::{
            HarnessPrepareCandidateRequest, HarnessPrepareCandidateResponse,
            HarnessPrepareCandidateResponseKind,
        };
        if context.request.payload.len() > MAX_HARNESS_WIRE_BYTES {
            return ServiceResponse::error(
                "invalid_request",
                "Candidate preparation exceeds bounds",
            );
        }
        let request: HarnessPrepareCandidateRequest =
            match serde_json::from_slice(&context.request.payload) {
                Ok(request) => request,
                Err(_) => {
                    return ServiceResponse::error(
                        "invalid_request",
                        "Invalid candidate preparation request",
                    );
                }
            };
        if request.validate().is_err() || request.candidate.plugin_id != PLUGIN_ID {
            return ServiceResponse::error(
                "invalid_request",
                "Incompatible candidate preparation request",
            );
        }
        // Only fresh trusted search output may be prepared. Never execute a request-supplied path.
        let result = self
            .fresh_candidates()
            .and_then(|fresh| prepare_from_discovery(&request, fresh));
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

    pub(super) fn candidates_service(&mut self, context: &NativeServiceContext) -> ServiceResponse {
        if context.request.operation == bcode_harness_models::HARNESS_PREPARE_CANDIDATE_OPERATION {
            return self.prepare_candidate_service(context);
        }
        if context.request.operation != bcode_harness_models::HARNESS_DISCOVER_CANDIDATES_OPERATION
            || context.request.payload.len() > 4096
        {
            return ServiceResponse::error(
                "invalid_request",
                "Invalid candidate discovery operation",
            );
        }
        let request: HarnessCandidatesRequest =
            match serde_json::from_slice(&context.request.payload) {
                Ok(request) => request,
                Err(_) => {
                    return ServiceResponse::error(
                        "invalid_request",
                        "Invalid candidate discovery request",
                    );
                }
            };
        if request.validate().is_err() {
            return ServiceResponse::error(
                "invalid_request",
                "Incompatible candidate discovery request",
            );
        }
        let result = self.fresh_candidates();
        let response = HarnessCandidatesResponse {
            schema_version: HARNESS_SCHEMA_VERSION,
            request_id: request.request_id.clone(),
            plugin_id: PLUGIN_ID.into(),
            kind: result.unwrap_or_else(HarnessCandidatesResponseKind::Failure),
        };
        if response.validate_for(&request, PLUGIN_ID).is_err() {
            return ServiceResponse::error(
                "invalid_response",
                "Invalid candidate discovery metadata",
            );
        }
        ServiceResponse::json(&response).unwrap_or_else(|_| {
            ServiceResponse::error("invalid_response", "Candidate encoding failed")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn identity(path: &str) -> HarnessCandidateExecutableIdentity {
        HarnessCandidateExecutableIdentity {
            canonical_path: path.into(),
            byte_len: 42,
            modified_unix_nanos: None,
            file_key: Some("1:2".into()),
        }
    }
    #[test]
    fn selected_candidate_preparation_requires_fresh_exact_equality_and_native_custody() {
        use bcode_harness_models::{
            HarnessPrepareCandidateRequest, HarnessPrepareCandidateResponse,
            HarnessPrepareCandidateResponseKind,
        };
        let candidate = HarnessInstallationCandidate {
            plugin_id: PLUGIN_ID.into(),
            executable: identity("/installed/claude"),
            native_version: "2.1.308 (Claude Code)".into(),
            config_namespace: "/native/account".into(),
            custody: HarnessAuthCustody::NativeManaged,
            readiness: HarnessCandidateReadiness::ExistingNativeLogin,
        };
        let request = HarnessPrepareCandidateRequest {
            schema_version: HARNESS_SCHEMA_VERSION,
            request_id: "prepare-1".into(),
            instance_id: "chosen-native".into(),
            candidate: candidate.clone(),
        };
        let fresh = |candidate| HarnessCandidatesResponseKind::Candidates {
            candidates: vec![candidate],
            complete: true,
        };
        let prepared = prepare_from_discovery(&request, fresh(candidate.clone())).unwrap();
        assert_eq!(prepared.instance.auth.provider_id, "claude-code");
        assert_eq!(prepared.instance.auth.profile, "native");
        assert_eq!(
            prepared.instance.auth.credential_namespace,
            "/native/account"
        );
        assert_eq!(
            prepared.instance.auth.custody,
            HarnessAuthCustody::NativeManaged
        );
        assert_eq!(
            prepared.adapter_settings["billing"],
            serde_json::json!({"route":"native"})
        );
        assert_eq!(
            prepared.adapter_settings["verified_native_auth_status"],
            true
        );
        assert_eq!(prepared.adapter_settings["verified_native_auth"], false);
        assert_eq!(prepared.adapter_settings["verified_session_state"], false);
        let mut response = HarnessPrepareCandidateResponse {
            schema_version: HARNESS_SCHEMA_VERSION,
            request_id: request.request_id.clone(),
            plugin_id: PLUGIN_ID.into(),
            kind: HarnessPrepareCandidateResponseKind::Prepared(Box::new(prepared)),
        };
        assert!(response.validate_for(&request, PLUGIN_ID).is_ok());
        response.request_id = "wrong-request".into();
        assert!(response.validate_for(&request, PLUGIN_ID).is_err());
        let mut changed = candidate.clone();
        changed.executable.byte_len += 1;
        assert_eq!(
            prepare_from_discovery(&request, fresh(changed)).unwrap_err(),
            HarnessFailure::Conflict
        );
        let mut changed = candidate;
        changed.readiness = HarnessCandidateReadiness::LoginRequired;
        assert_eq!(
            prepare_from_discovery(&request, fresh(changed)).unwrap_err(),
            HarnessFailure::Conflict
        );
        assert_eq!(
            prepare_from_discovery(
                &request,
                HarnessCandidatesResponseKind::Candidates {
                    candidates: Vec::new(),
                    complete: false,
                }
            )
            .unwrap_err(),
            HarnessFailure::Conflict
        );
    }

    #[test]
    fn injected_search_skips_relative_paths_deduplicates_and_never_executes() {
        let input = SearchEnvironment::from_values(
            std::ffi::OsStr::new("/trusted/bin:relative:/other/bin"),
            "/home/test".into(),
            None,
        )
        .unwrap();
        let mut inspected = Vec::new();
        let (found, complete) = search_candidates(&input, |path| {
            inspected.push(path.to_owned());
            Ok(Some(identity("/installed/claude")))
        });
        assert_eq!(inspected.len(), 2);
        assert_eq!(found.len(), 1);
        assert!(!complete);
        assert_eq!(input.namespace, PathBuf::from("/home/test/.claude"));
    }
    #[test]
    fn injected_namespace_is_exact_and_search_is_bounded() {
        let input = SearchEnvironment::from_values(
            std::ffi::OsStr::new("/trusted"),
            "/home/test".into(),
            Some("/native/account".into()),
        )
        .unwrap();
        assert_eq!(input.namespace, PathBuf::from("/native/account"));
        assert!(
            SearchEnvironment::from_values(
                std::ffi::OsStr::new("/trusted"),
                "/home/test".into(),
                Some("relative".into())
            )
            .is_err()
        );
        let (candidates, complete) = search_candidates(&input, |_| Ok(None));
        assert_eq!(candidates, Vec::<HarnessCandidateExecutableIdentity>::new());
        assert!(complete);
        let (candidates, complete) =
            search_candidates(&input, |_| Err(HarnessFailure::Unavailable));
        assert_eq!(candidates, Vec::<HarnessCandidateExecutableIdentity>::new());
        assert!(!complete);
        let mut bounded = input;
        bounded.directories = (0..12)
            .map(|index| PathBuf::from(format!("/bin/{index}")))
            .collect();
        let (found, complete) =
            search_candidates(&bounded, |path| Ok(Some(identity(path.to_str().unwrap()))));
        assert_eq!(found.len(), MAX_HARNESS_CANDIDATES);
        assert!(!complete);
    }
    #[test]
    fn injected_status_never_promotes_other_routes_or_unknown_evidence() {
        let status = |logged_in, method| {
            Ok(bcode_claude_code::AuthStatus {
                logged_in,
                method,
                config_directory: "/native/account".into(),
            })
        };
        assert_eq!(
            readiness(status(true, bcode_claude_code::AuthMethod::ClaudeAccount)),
            HarnessCandidateReadiness::ExistingNativeLogin
        );
        assert_eq!(
            readiness(status(false, bcode_claude_code::AuthMethod::None)),
            HarnessCandidateReadiness::LoginRequired
        );
        assert_eq!(
            readiness(status(true, bcode_claude_code::AuthMethod::ApiKey)),
            HarnessCandidateReadiness::Unknown
        );
        assert_eq!(
            readiness(Err(bcode_claude_code::Error::Protocol)),
            HarnessCandidateReadiness::Unsupported
        );
    }
}
