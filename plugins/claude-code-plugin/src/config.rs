//! Explicit, non-secret launch configuration. Never copy ambient authentication variables.
use bcode_claude_code::{
    BillingRoute, LaunchOptions, Limits, NativeAuthSupport, NativeEnvironment, SettingsInheritance,
    SettlementSupport,
};
use bcode_harness_models::{HarnessAuthCustody, HarnessFailure, HarnessInstance};
use serde::Deserialize;
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub instances: BTreeMap<String, InstanceConfig>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstanceConfig {
    pub executable: PathBuf,
    pub config_directory: PathBuf,
    pub exact_version: String,
    /// Operator attestation for this exact executable/version, not a version-number guess.
    #[serde(default)]
    pub verified_session_state: bool,
    #[serde(default)]
    pub verified_native_auth: bool,
    /// Explicit inherited settings sources; None preserves native defaults and managed policy.
    pub setting_sources: Option<Vec<String>>,
}
impl InstanceConfig {
    pub fn options(
        &self,
        instance: &HarnessInstance,
        cwd: PathBuf,
        resume: Option<String>,
    ) -> Result<LaunchOptions, HarnessFailure> {
        if instance.executable != self.executable.to_string_lossy()
            || instance.config_namespace != self.config_directory.to_string_lossy()
            || instance.auth.provider_id != "claude-code"
            || instance.auth.credential_namespace != instance.config_namespace
            || instance.auth.custody != HarnessAuthCustody::NativeManaged
            || !self.executable.is_absolute()
            || !self.config_directory.is_absolute()
            || !cwd.is_absolute()
            || self.exact_version.is_empty()
            || self.exact_version.len() > 256
        {
            return Err(HarnessFailure::Policy);
        }
        // Fail closed instead of silently removing an ambient route and spending on another account.
        for key in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_PROFILE",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
        ] {
            if std::env::var_os(key).is_some() {
                return Err(HarnessFailure::Policy);
            }
        }
        let mut environment = BTreeMap::new();
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
                environment.insert(key.into(), value);
            }
        }
        Ok(LaunchOptions {
            executable: self.executable.clone(),
            expected_version: self.exact_version.clone(),
            cwd,
            config_directory: self.config_directory.clone(),
            environment: NativeEnvironment(environment),
            billing_route: BillingRoute::NativeAccount,
            settings: if self.setting_sources.is_some() {
                SettingsInheritance::ExplicitSources
            } else {
                SettingsInheritance::Native
            },
            setting_sources: self.setting_sources.clone().unwrap_or_default(),
            resume_session_id: resume,
            settlement_support: if self.verified_session_state {
                SettlementSupport::VerifiedSessionState
            } else {
                SettlementSupport::Unknown
            },
            auth_support: if self.verified_native_auth {
                NativeAuthSupport::VerifiedStatusAndLogin
            } else {
                NativeAuthSupport::Unknown
            },
            limits: Limits::default(),
        })
    }
}
