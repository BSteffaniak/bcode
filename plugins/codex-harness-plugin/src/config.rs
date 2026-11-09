//! Explicit, non-secret Codex adapter configuration. Never copies ambient credentials.
use crate::transport::CodexError;
use bcode_harness_models::{HarnessAuthCustody, HarnessInstance};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

/// Plugin configuration under `plugins.config."bcode.codex-harness"`.
#[derive(Debug, Default, Deserialize)]
pub struct Config {
    /// Absolute working directory for account inspection and login. Defaults to the instance's
    /// `CODEX_HOME` when absent.
    #[serde(default)]
    pub auth_working_directory: Option<String>,
    /// Per-instance launch selection written by setup discovery.
    #[serde(default)]
    pub instances: BTreeMap<String, InstanceConfig>,
}

/// Exact launch selection for one configured instance. It must match the generic instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstanceConfig {
    /// Executable invoked for this instance (the PATH spelling found during setup).
    pub executable: String,
    /// Absolute native `CODEX_HOME` (configuration and credential namespace).
    pub codex_home: String,
    /// Version reported by `codex --version` when the instance was prepared.
    pub exact_version: String,
}

impl InstanceConfig {
    /// Validate this selection against the generic instance; mismatches fail closed.
    ///
    /// # Errors
    /// Rejects relative paths, an empty version, a different executable or namespace, a
    /// non-Codex auth provider, or custody other than native/vault managed.
    pub fn validate_for(&self, instance: &HarnessInstance) -> Result<(), CodexError> {
        let paths_valid = Path::new(&self.executable).is_absolute()
            && Path::new(&self.codex_home).is_absolute()
            && !self.exact_version.trim().is_empty();
        // The adapter's CODEX_HOME is the generic instance's configuration namespace.
        let namespace = &instance.config_namespace;
        let same_selection =
            self.executable == instance.executable && self.codex_home == *namespace;
        let auth_valid = instance.auth.provider_id == crate::AUTH_PROVIDER_ID
            && instance.auth.credential_namespace == instance.config_namespace
            && instance.auth.custody != HarnessAuthCustody::External;
        if paths_valid && same_selection && auth_valid {
            Ok(())
        } else {
            Err(CodexError::Configuration)
        }
    }
}

impl Config {
    /// Decode the host-resolved plugin configuration; absent configuration is the default.
    ///
    /// # Errors
    /// Rejects configuration that does not decode into this shape.
    pub fn from_context(
        context: &bcode_plugin_sdk::NativeServiceContext,
    ) -> Result<Self, CodexError> {
        context
            .config_or_default::<Self>()
            .map_err(|_| CodexError::Configuration)
    }

    /// Validate the optional per-instance selection. Legacy configurations without an entry
    /// keep using the generic instance selection alone.
    ///
    /// # Errors
    /// Rejects a present entry that does not match the generic instance.
    pub fn validate_instance(&self, instance: &HarnessInstance) -> Result<(), CodexError> {
        self.instances
            .get(&instance.instance_id)
            .map_or(Ok(()), |selected| selected.validate_for(instance))
    }

    /// Absolute working directory for inspection and login, never the plugin process cwd.
    #[must_use]
    pub fn auth_working_directory<'a>(&'a self, instance: &'a HarnessInstance) -> &'a str {
        self.auth_working_directory
            .as_deref()
            .unwrap_or(&instance.config_namespace)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_harness_models::HarnessAuthBinding;

    fn instance() -> HarnessInstance {
        HarnessInstance {
            instance_id: "codex".into(),
            plugin_id: crate::PLUGIN_ID.into(),
            executable: "/bin/codex".into(),
            config_namespace: "/home/user/.codex".into(),
            auth: HarnessAuthBinding {
                provider_id: crate::AUTH_PROVIDER_ID.into(),
                profile: "native".into(),
                credential_namespace: "/home/user/.codex".into(),
                custody: HarnessAuthCustody::NativeManaged,
            },
        }
    }

    #[test]
    fn instance_selection_must_match_the_generic_instance() {
        let selected = InstanceConfig {
            executable: "/bin/codex".into(),
            codex_home: "/home/user/.codex".into(),
            exact_version: "0.164.0".into(),
        };
        assert!(selected.validate_for(&instance()).is_ok());
        let mut other = selected.clone();
        other.codex_home = "/elsewhere".into();
        assert_eq!(
            other.validate_for(&instance()),
            Err(CodexError::Configuration)
        );
        let mut relative = selected;
        relative.executable = "codex".into();
        assert_eq!(
            relative.validate_for(&instance()),
            Err(CodexError::Configuration)
        );
    }

    #[test]
    fn absent_configuration_decodes_and_defaults_auth_directory_to_codex_home() {
        let config: Config = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(config.validate_instance(&instance()).is_ok());
        assert_eq!(
            config.auth_working_directory(&instance()),
            "/home/user/.codex"
        );
        let config: Config = serde_json::from_value(serde_json::json!({
            "auth_working_directory": "/work",
            "instances": {"codex": {"executable": "/bin/codex",
                "codex_home": "/other", "exact_version": "0.164.0"}}
        }))
        .unwrap();
        assert_eq!(config.auth_working_directory(&instance()), "/work");
        assert_eq!(
            config.validate_instance(&instance()),
            Err(CodexError::Configuration)
        );
    }
}
