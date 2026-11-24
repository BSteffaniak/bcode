//! Explicit, non-secret Antigravity adapter configuration. Never copies ambient credentials.
use crate::transport::AcpError;
use bcode_harness_models::{HarnessAuthCustody, HarnessInstance};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

/// Plugin configuration under `plugins.config."bcode.antigravity-harness"`.
#[derive(Debug, Default, Deserialize)]
pub struct Config {
    /// Absolute working directory for account inspection and login. Defaults to the instance's
    /// `gemini_dir` when absent.
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
    /// Absolute native Gemini directory (`--gemini-dir`, configuration and credential namespace).
    pub gemini_dir: String,
    /// Version reported by `agy --version` when the instance was prepared.
    pub exact_version: String,
}

impl InstanceConfig {
    /// Validate this selection against the generic instance; mismatches fail closed.
    ///
    /// # Errors
    /// * Rejects relative paths, an empty version, a different executable or namespace,
    ///   a non-Antigravity auth provider, or custody other than native managed.
    pub fn validate_for(&self, instance: &HarnessInstance) -> Result<(), AcpError> {
        let paths_valid = Path::new(&self.executable).is_absolute()
            && Path::new(&self.gemini_dir).is_absolute()
            && !self.exact_version.trim().is_empty();
        let namespace = &instance.config_namespace;
        let same_selection =
            self.executable == instance.executable && self.gemini_dir == *namespace;
        let auth_valid = instance.auth.provider_id == crate::AUTH_PROVIDER_ID
            && instance.auth.credential_namespace == instance.config_namespace
            && instance.auth.custody == HarnessAuthCustody::NativeManaged;
        if paths_valid && same_selection && auth_valid {
            Ok(())
        } else {
            Err(AcpError::Configuration)
        }
    }
}

impl Config {
    /// Decode the host-resolved plugin configuration; absent configuration is the default.
    ///
    /// # Errors
    /// * Rejects configuration that does not decode into this shape.
    pub fn from_context(
        context: &bcode_plugin_sdk::NativeServiceContext,
    ) -> Result<Self, AcpError> {
        context
            .config_or_default::<Self>()
            .map_err(|_| AcpError::Configuration)
    }

    /// Validate the optional per-instance selection. Configurations without an entry keep using
    /// the generic instance selection alone.
    ///
    /// # Errors
    /// * Rejects a present entry that does not match the generic instance.
    pub fn validate_instance(&self, instance: &HarnessInstance) -> Result<(), AcpError> {
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
            instance_id: "antigravity".into(),
            plugin_id: crate::PLUGIN_ID.into(),
            executable: "/bin/agy".into(),
            config_namespace: "/home/user/.gemini".into(),
            auth: HarnessAuthBinding {
                provider_id: crate::AUTH_PROVIDER_ID.into(),
                profile: "native".into(),
                credential_namespace: "/home/user/.gemini".into(),
                custody: HarnessAuthCustody::NativeManaged,
            },
        }
    }

    #[test]
    fn instance_selection_must_match_the_generic_instance() {
        let selected = InstanceConfig {
            executable: "/bin/agy".into(),
            gemini_dir: "/home/user/.gemini".into(),
            exact_version: "1.5.15".into(),
        };
        assert!(selected.validate_for(&instance()).is_ok());

        let mut other = selected.clone();
        other.gemini_dir = "/elsewhere".into();
        assert_eq!(
            other.validate_for(&instance()),
            Err(AcpError::Configuration)
        );

        let mut relative = selected;
        relative.executable = "agy".into();
        assert_eq!(
            relative.validate_for(&instance()),
            Err(AcpError::Configuration)
        );
    }

    #[test]
    fn absent_configuration_decodes_and_defaults_auth_directory_to_gemini_dir() {
        let config: Config = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(config.validate_instance(&instance()).is_ok());
        assert_eq!(
            config.auth_working_directory(&instance()),
            "/home/user/.gemini"
        );

        let config: Config = serde_json::from_value(serde_json::json!({
            "auth_working_directory": "/work",
            "instances": {
                "antigravity": {
                    "executable": "/bin/agy",
                    "gemini_dir": "/other",
                    "exact_version": "1.5.15"
                }
            }
        }))
        .unwrap();
        assert_eq!(config.auth_working_directory(&instance()), "/work");
        assert_eq!(
            config.validate_instance(&instance()),
            Err(AcpError::Configuration)
        );
    }
}
