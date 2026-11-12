//! Explicit, non-secret launch configuration. Never copy ambient authentication variables.
use bcode_claude_code::{
    BillingRoute, LaunchOptions, Limits, NativeAuthSupport, NativeEnvironment, SettingsInheritance,
    SettlementSupport,
};
use bcode_harness_models::{HarnessAuthCustody, HarnessFailure, HarnessInstance};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

/// Resolved plugin configuration; contains selections and attestations, not secrets.
#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Configured harness instances keyed by stable instance identity.
    #[serde(default)]
    pub instances: BTreeMap<String, InstanceConfig>,
}
/// Explicit configuration for the existing managed-harness execution path.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InstanceConfig {
    /// Native is the compatibility default. API mode requires an exact host credential grant.
    #[serde(default)]
    pub billing: ApiBilling,

    /// Absolute native executable path.
    pub executable: PathBuf,
    /// Absolute native credential/configuration namespace.
    pub config_directory: PathBuf,
    /// Expected exact trimmed native version output; not capability evidence.
    pub exact_version: String,
    /// Operator attestation for this exact executable/version, not a version-number guess.
    #[serde(default)]
    pub verified_session_state: bool,
    /// Exact-version local status attestation only; never authorizes login handoff.
    #[serde(default)]
    pub verified_native_auth_status: bool,
    /// Exact-version combined local status and login handoff attestation.
    #[serde(default)]
    pub verified_native_auth: bool,
    /// Explicit inherited settings sources; None preserves native defaults and managed policy.
    pub setting_sources: Option<Vec<String>>,
}
/// Billing selection; never implicitly falls back between native and API credentials.
#[derive(Default, Deserialize, Serialize)]
#[serde(tag = "route", rename_all = "snake_case", deny_unknown_fields)]
pub enum ApiBilling {
    /// Existing native credential custody, with no token export.
    #[default]
    Native,
    /// Explicit host-owned API credential selection.
    ApiKey {
        /// Declared provider identity.
        provider_id: String,
        /// Selected credential profile.
        profile: String,
        /// Host credential namespace.
        credential_namespace: String,
    },
}

impl InstanceConfig {
    /// Resolve non-secret selection only; these fields never grant credential access.
    /// # Errors
    /// Rejects any custody, provider, profile or namespace mismatch.
    pub fn credential_selection(
        &self,
        instance: &HarnessInstance,
    ) -> Result<Option<bcode_plugin_sdk::PluginCredentialSelection>, HarnessFailure> {
        match &self.billing {
            ApiBilling::Native
                if instance.auth.custody == HarnessAuthCustody::NativeManaged
                    && instance.auth.provider_id == "claude-code"
                    && instance.auth.credential_namespace == instance.config_namespace =>
            {
                Ok(None)
            }
            ApiBilling::ApiKey {
                provider_id,
                profile,
                credential_namespace,
            } if instance.auth.custody == HarnessAuthCustody::VaultManaged
                && provider_id == "claude-code"
                && instance.auth.provider_id == *provider_id
                && instance.auth.profile == *profile
                && instance.auth.credential_namespace == *credential_namespace =>
            {
                let selection = bcode_plugin_sdk::PluginCredentialSelection {
                    instance_id: instance.instance_id.clone(),
                    provider_id: provider_id.clone(),
                    profile: profile.clone(),
                    credential_namespace: credential_namespace.clone(),
                    custody: instance.auth.custody,
                    credential_id: "api_key".into(),
                };
                selection.validate().map_err(|_| HarnessFailure::Policy)?;
                Ok(Some(selection))
            }
            _ => Err(HarnessFailure::Policy),
        }
    }

    /// Validate non-secret launch selection for catalog inspection, without fetching credentials.
    /// # Errors
    /// Rejects mismatched launch selection or unsupported ambient authentication routes.
    pub fn inspection_options(
        &self,
        instance: &HarnessInstance,
    ) -> Result<LaunchOptions, HarnessFailure> {
        self.options_selected(instance, self.config_directory.clone(), None)
    }

    /// Build the exact launch route with invocation-only credential delivery.
    /// # Errors
    /// Rejects invalid configuration, missing/revoked host grants or unavailable credentials.
    pub fn options_with_context(
        &self,
        instance: &HarnessInstance,
        cwd: PathBuf,
        resume: Option<String>,
        context: &bcode_plugin_sdk::NativeServiceContext,
    ) -> Result<LaunchOptions, HarnessFailure> {
        let selection = self.credential_selection(instance)?;
        let mut options = self.options_selected(instance, cwd, resume)?;
        if let Some(selection) = selection {
            Self::inject_api_credential(&mut options, context, &selection)?;
        }
        Ok(options)
    }

    fn inject_api_credential(
        options: &mut LaunchOptions,
        context: &bcode_plugin_sdk::NativeServiceContext,
        selection: &bcode_plugin_sdk::PluginCredentialSelection,
    ) -> Result<(), HarnessFailure> {
        if options.billing_route != BillingRoute::ApiKey
            || options.environment.0.contains_key("ANTHROPIC_API_KEY")
        {
            return Err(HarnessFailure::Policy);
        }
        let key = context
            .credential(selection)
            .map_err(|_| HarnessFailure::Authentication)?;
        options
            .environment
            .0
            .insert("ANTHROPIC_API_KEY".into(), key.expose().into());
        Ok(())
    }

    /// Exact-version native auth capability selected by explicit attestation.
    #[must_use]
    pub const fn native_auth_support(&self) -> NativeAuthSupport {
        if self.verified_native_auth {
            NativeAuthSupport::VerifiedStatusAndLogin
        } else if self.verified_native_auth_status {
            NativeAuthSupport::VerifiedStatus
        } else {
            NativeAuthSupport::Unknown
        }
    }

    /// Resolve a native-account launch without requesting host API credentials.
    /// # Errors
    /// Rejects API billing, mismatched instance identity, invalid paths or ambient auth routes.
    pub fn options(
        &self,
        instance: &HarnessInstance,
        cwd: PathBuf,
        resume: Option<String>,
    ) -> Result<LaunchOptions, HarnessFailure> {
        if self.credential_selection(instance)?.is_some() {
            return Err(HarnessFailure::Authentication);
        }
        self.options_selected(instance, cwd, resume)
    }

    fn options_selected(
        &self,
        instance: &HarnessInstance,
        cwd: PathBuf,
        resume: Option<String>,
    ) -> Result<LaunchOptions, HarnessFailure> {
        let api = self.credential_selection(instance)?.is_some();
        if instance.executable != self.executable.to_string_lossy()
            || instance.config_namespace != self.config_directory.to_string_lossy()
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
            billing_route: if api {
                BillingRoute::ApiKey
            } else {
                BillingRoute::NativeAccount
            },
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
            auth_support: self.native_auth_support(),
            limits: Limits::default(),
        })
    }
}

#[cfg(test)]
#[path = "api_configuration_tests.rs"]
mod api_configuration_tests;
