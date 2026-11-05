//! Native auth inspection and explicit terminal handoff. No credential-store reads.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Plugin-owned, read-only native-login plan extension; not an authentication mutation.
pub const NATIVE_HANDOFF_INTERFACE_ID: &str = "bcode.claude-code.handoff/v1";
/// Operation returning an explicit namespace-bound plan for user-authorized client execution.
pub const NATIVE_HANDOFF_OPERATION: &str = "prepare_login";

/// Bounded namespace selection for a non-executing native login handoff plan.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeLoginHandoffRequest {
    /// Must equal one; unknown contract versions fail closed.
    pub schema_version: u16,
    /// Correlation identity, never execution authorization.
    pub request_id: String,
    /// Must match an enabled resolved plugin instance exactly.
    pub instance: HarnessInstance,
}

/// Correlated handoff plan. Never enters canonical session history or model context.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NativeLoginHandoffResponse {
    /// Independently versioned plugin extension envelope.
    pub schema_version: u16,
    /// Exact request correlation.
    pub request_id: String,
    /// Direct-execution plan; no shell command or secret material.
    pub handoff: NativeLoginHandoffData,
}

/// Supported execution surface, not proof of namespace reservation or login inactivity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeLoginExecution {
    /// Explicitly authorized client terminal owns the native child and its reap.
    ClientTerminal,
}

/// Native CLI observation of other login processes in this credential namespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeLoginInactivity {
    /// Native auth status has no login-process enumeration or inactivity evidence.
    Unknown,
}

/// Plugin-selected method evidence for the exact verified executable and namespace.
/// This is capability evidence, never a bearer grant or process-inactivity proof.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeLoginMethodEvidence {
    /// Fixed method accepted by this plugin; never selected by client input.
    pub method_id: String,
    /// Support for terminal handoff delivery only, not daemon-native login recovery.
    pub support: bcode_harness_models::HarnessSupportLevel,
    /// Native CLI retains credentials; no host vault enrollment.
    pub custody: bcode_provider_auth_models::managed::AuthCustodyMode,
    /// Required execution surface. Daemon stdio is never supported.
    pub execution: NativeLoginExecution,
    /// Independent of credential readiness and in-memory flow registration.
    pub native_inactivity: NativeLoginInactivity,
}

/// Non-secret execution plan for a client-owned native terminal, never daemon stdio.
///
/// Consumers must verify the selected executable version before explicit execution,
/// preserve the environment policy, and reinspect native auth status after exit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeLoginHandoffData {
    /// Independently evolving plugin-owned plan schema.
    pub schema_version: u16,
    /// Bound native credential namespace; not an account token.
    pub namespace: bcode_provider_auth_models::managed::AuthNamespace,
    /// Explicit method/surface evidence; omission in older plans is not support evidence.
    pub method: NativeLoginMethodEvidence,
    /// Explicit executable, never resolved from PATH by the consumer.
    pub executable: std::path::PathBuf,
    /// Exact version that the consumer must verify before launching.
    pub expected_version: String,
    /// Explicit working directory for the client terminal process.
    pub working_directory: std::path::PathBuf,
    /// Fixed arguments; execute directly, never interpolate into a shell command.
    pub arguments: Vec<String>,
    /// Complete allowlisted process environment. Clear ambient environment first.
    pub environment: BTreeMap<String, String>,
}

/// Prepare namespace-specific login handoff data without launching or reading credentials.
///
/// # Errors
/// Rejects unsupported versions/custody, mismatched namespace/executable, credential-bearing
/// environments, nonabsolute paths, and malformed identifiers. This does not verify a remote account.
pub fn native_login_handoff_data(
    instance: &HarnessInstance,
    options: &LaunchOptions,
) -> Result<NativeLoginHandoffData, HarnessFailure> {
    instance
        .validate()
        .map_err(|_| HarnessFailure::InvalidRequest)?;
    if instance.plugin_id != "bcode.claude-code"
        || instance.auth.provider_id != "claude-code"
        || instance.auth.custody != HarnessAuthCustody::NativeManaged
        || options.auth_support != NativeAuthSupport::VerifiedStatusAndLogin
        || options.billing_route != bcode_claude_code::BillingRoute::NativeAccount
        || instance.executable != options.executable.to_string_lossy()
        || instance.config_namespace != options.config_directory.to_string_lossy()
        || instance.auth.credential_namespace != instance.config_namespace
        || !options.executable.is_absolute()
        || !options.cwd.is_absolute()
        || !options.config_directory.is_absolute()
        || options.expected_version.is_empty()
        || options.expected_version.len() > 256
    {
        return Err(HarnessFailure::Policy);
    }
    let allowed = [
        "HOME",
        "USER",
        "PATH",
        "TMPDIR",
        "LANG",
        "LC_ALL",
        "SYSTEMROOT",
    ];
    if options
        .environment
        .0
        .keys()
        .any(|key| !allowed.contains(&key.as_str()))
    {
        return Err(HarnessFailure::Policy);
    }
    let mut environment = options.environment.0.clone();
    if environment
        .values()
        .any(|value| value.len() > MAX_HARNESS_TEXT_BYTES || value.contains('\0'))
    {
        return Err(HarnessFailure::InvalidRequest);
    }
    environment.insert(
        "CLAUDE_CONFIG_DIR".into(),
        instance.config_namespace.clone(),
    );
    Ok(NativeLoginHandoffData {
        schema_version: 1,
        namespace: namespace(instance),
        method: NativeLoginMethodEvidence {
            method_id: "native-login".into(),
            support: bcode_harness_models::HarnessSupportLevel::Supported,
            custody: AuthCustodyMode::NativeManaged,
            execution: NativeLoginExecution::ClientTerminal,
            native_inactivity: NativeLoginInactivity::Unknown,
        },
        executable: options.executable.clone(),
        expected_version: options.expected_version.clone(),
        working_directory: options.cwd.clone(),
        arguments: vec!["auth".into(), "login".into()],
        environment,
    })
}

use bcode_claude_code::{LaunchOptions, NativeAuthSupport};
use bcode_harness_models::{
    HarnessAuthCustody, HarnessAuthStep, HarnessFailure, HarnessInstance, HarnessRequest,
    HarnessRequestKind, HarnessServiceResponseKind, MAX_HARNESS_TEXT_BYTES,
};
use bcode_provider_auth_models::managed::{
    AuthCustodyMode, AuthNamespace, AuthReadiness, ManagedAuthStatus, SensitiveAuthInteraction,
};

#[derive(Default)]
pub struct AuthFlows {
    flows: BTreeMap<String, HarnessInstance>,
}
impl AuthFlows {
    pub async fn invoke(
        &mut self,
        request: &HarnessRequest,
        options: &LaunchOptions,
    ) -> Result<HarnessServiceResponseKind, HarnessFailure> {
        if options.billing_route == bcode_claude_code::BillingRoute::ApiKey {
            return api_readiness(request);
        }
        let status_only = matches!(
            request.kind,
            HarnessRequestKind::AuthStatus { verify: false }
        );
        if options.auth_support != NativeAuthSupport::VerifiedStatusAndLogin
            && !(options.auth_support == NativeAuthSupport::VerifiedStatus && status_only)
        {
            return Err(HarnessFailure::Unsupported);
        }
        match &request.kind {
            HarnessRequestKind::AuthStatus { verify } => {
                // Native `auth status` is local inspection, not a remote verification request.
                if *verify {
                    return Err(HarnessFailure::Unsupported);
                }
                Ok(HarnessServiceResponseKind::AuthStatus(
                    inspect(&request.instance, options).await?,
                ))
            }
            HarnessRequestKind::AuthBegin(flow) => {
                if flow.method_id != "native-login" || flow.input.is_some() {
                    return Err(HarnessFailure::Unsupported);
                }
                native_login_handoff_data(&request.instance, options)?;
                if let Some(instance) = self.flows.get(&flow.flow_id) {
                    if instance != &request.instance {
                        return Err(HarnessFailure::Conflict);
                    }
                } else {
                    if self.flows.len() >= 32 {
                        return Err(HarnessFailure::Unavailable);
                    }
                    self.flows
                        .insert(flow.flow_id.clone(), request.instance.clone());
                }
                let status = inspect(&request.instance, options).await?;
                let complete = status.readiness == AuthReadiness::Ready;
                if complete {
                    self.flows.remove(&flow.flow_id);
                }
                Ok(HarnessServiceResponseKind::AuthStep(HarnessAuthStep {
                    flow_id: flow.flow_id.clone(),
                    status,
                    complete,
                    interaction: (!complete).then(|| SensitiveAuthInteraction::NativeHandoff {
                        request_id: flow.flow_id.clone(),
                    }),
                }))
            }
            HarnessRequestKind::AuthContinue(flow) => {
                if flow.method_id != "native-login" || flow.input.is_some() {
                    return Err(HarnessFailure::Unsupported);
                }
                if self.flows.get(&flow.flow_id) != Some(&request.instance) {
                    return Err(HarnessFailure::Conflict);
                }
                let status = inspect(&request.instance, options).await?;
                let complete = status.readiness == AuthReadiness::Ready;
                if complete {
                    self.flows.remove(&flow.flow_id);
                }
                Ok(HarnessServiceResponseKind::AuthStep(HarnessAuthStep {
                    flow_id: flow.flow_id.clone(),
                    status,
                    complete,
                    interaction: (!complete).then(|| SensitiveAuthInteraction::NativeHandoff {
                        request_id: flow.flow_id.clone(),
                    }),
                }))
            }
            HarnessRequestKind::AuthCancel(flow) => {
                if flow.method_id != "native-login" || flow.input.is_some() {
                    return Err(HarnessFailure::Unsupported);
                }
                if self.flows.get(&flow.flow_id) != Some(&request.instance) {
                    return Err(HarnessFailure::Conflict);
                }
                self.flows.remove(&flow.flow_id);
                // Cancel the local flow, not shared native credentials or another waiter's login.
                Ok(HarnessServiceResponseKind::AuthStep(HarnessAuthStep {
                    flow_id: flow.flow_id.clone(),
                    complete: true,
                    interaction: None,
                    status: ManagedAuthStatus {
                        namespace: namespace(&request.instance),
                        custody: AuthCustodyMode::NativeManaged,
                        readiness: AuthReadiness::Unknown,
                    },
                }))
            }
            _ => Err(HarnessFailure::InvalidRequest),
        }
    }
}
fn api_readiness(request: &HarnessRequest) -> Result<HarnessServiceResponseKind, HarnessFailure> {
    if !matches!(
        request.kind,
        HarnessRequestKind::AuthStatus { verify: false }
    ) {
        return Err(HarnessFailure::Unsupported);
    }
    Ok(HarnessServiceResponseKind::AuthStatus(ManagedAuthStatus {
        namespace: namespace(&request.instance),
        custody: AuthCustodyMode::VaultManaged,
        readiness: AuthReadiness::Ready,
    }))
}

fn namespace(instance: &HarnessInstance) -> AuthNamespace {
    AuthNamespace {
        plugin_id: instance.plugin_id.clone(),
        credential_namespace: instance.auth.credential_namespace.clone(),
    }
}
async fn inspect(
    instance: &HarnessInstance,
    options: &LaunchOptions,
) -> Result<ManagedAuthStatus, HarnessFailure> {
    let native = bcode_claude_code::auth_status(options)
        .await
        .map_err(|_| HarnessFailure::Authentication)?;
    Ok(ManagedAuthStatus {
        namespace: namespace(instance),
        custody: AuthCustodyMode::NativeManaged,
        readiness: if native.logged_in {
            AuthReadiness::Ready
        } else {
            AuthReadiness::LoginRequired
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_claude_code::{
        BillingRoute, Limits, NativeEnvironment, SettingsInheritance, SettlementSupport,
    };
    use bcode_harness_models::{HARNESS_SCHEMA_VERSION, HarnessAuthBinding, HarnessAuthRequest};

    #[tokio::test]
    async fn status_only_admission_preserves_login_fences() {
        let instance = HarnessInstance {
            instance_id: "test".into(),
            plugin_id: "bcode.claude-code".into(),
            executable: "/nonexistent/bcode-auth-test/claude".into(),
            config_namespace: "/isolated/config".into(),
            auth: HarnessAuthBinding {
                provider_id: "claude-code".into(),
                profile: "native".into(),
                credential_namespace: "/isolated/config".into(),
                custody: HarnessAuthCustody::NativeManaged,
            },
        };
        for support in [
            NativeAuthSupport::Unknown,
            NativeAuthSupport::VerifiedStatus,
            NativeAuthSupport::VerifiedStatusAndLogin,
        ] {
            let options = LaunchOptions {
                executable: instance.executable.clone().into(),
                expected_version: "test-only".into(),
                cwd: "/isolated".into(),
                config_directory: instance.config_namespace.clone().into(),
                environment: NativeEnvironment::default(),
                billing_route: BillingRoute::NativeAccount,
                settings: SettingsInheritance::ExplicitSources,
                setting_sources: Vec::new(),
                resume_session_id: None,
                settlement_support: SettlementSupport::Unknown,
                auth_support: support,
                limits: Limits::default(),
            };
            let flow = || HarnessAuthRequest {
                flow_id: "flow".into(),
                method_id: "native-login".into(),
                input: None,
            };
            for kind in [
                HarnessRequestKind::AuthStatus { verify: false },
                HarnessRequestKind::AuthStatus { verify: true },
                HarnessRequestKind::AuthBegin(flow()),
                HarnessRequestKind::AuthContinue(flow()),
            ] {
                let local_status = matches!(kind, HarnessRequestKind::AuthStatus { verify: false });
                let remote_status = matches!(kind, HarnessRequestKind::AuthStatus { verify: true });
                let request = HarnessRequest {
                    schema_version: HARNESS_SCHEMA_VERSION,
                    request_id: "request".into(),
                    instance: instance.clone(),
                    kind,
                };
                let mut flows = AuthFlows::default();
                let result = flows.invoke(&request, &options).await;
                if support == NativeAuthSupport::Unknown
                    || remote_status
                    || (support == NativeAuthSupport::VerifiedStatus && !local_status)
                {
                    assert!(matches!(result, Err(HarnessFailure::Unsupported)));
                    assert!(flows.flows.is_empty());
                } else if local_status || matches!(request.kind, HarnessRequestKind::AuthBegin(_)) {
                    // The missing executable reaches sanitized status preflight failure,
                    // rather than the support gate. No native program can be launched.
                    assert!(matches!(result, Err(HarnessFailure::Authentication)));
                } else {
                    // Full-support continuation still requires a previously admitted flow.
                    assert!(matches!(result, Err(HarnessFailure::Conflict)));
                }
            }
        }
    }
}
