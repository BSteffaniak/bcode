//! Versioned, renderer-neutral shell command presentation policy.

use hyperchad_docs_config_derive::ConfigDoc;
use serde::{Deserialize, Deserializer, Serialize};

/// Current shell output presentation policy compatibility version.
pub const SHELL_OUTPUT_PRESENTATION_POLICY_VERSION: u32 = 1;

/// Presentation-only parsing limits and runtime switches.
///
/// These options never change executed source or permission operation facts. Unknown versions
/// are rejected rather than interpreted using current defaults. Missing fields retain v1 defaults.
// Independent user-facing switches are not a state machine: every combination is meaningful.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ConfigDoc)]
#[config_doc(section = "presentation")]
#[serde(default, deny_unknown_fields)]
pub struct ShellOutputPresentationPolicy {
    /// Compatibility version; only version 1 is supported.
    #[serde(deserialize_with = "deserialize_version")]
    pub version: u32,
    /// Enable command presentation. Effective resolution also respects `format_commands`.
    pub enabled: bool,
    /// Master switch for embedded source presentation.
    pub embedded: bool,
    /// Present statically identified Python source.
    pub python: bool,
    /// Present statically identified JavaScript source.
    pub javascript: bool,
    /// Present statically identified nested shell source.
    pub nested_shell: bool,
    /// Maximum source bytes examined for one command document; zero disables extraction.
    pub max_source_bytes: usize,
    /// Maximum embedded regions admitted per command document; zero disables extraction.
    pub max_regions: usize,
    /// Maximum nested extraction depth; zero disables extraction.
    pub max_depth: usize,
}

impl Default for ShellOutputPresentationPolicy {
    fn default() -> Self {
        Self {
            version: SHELL_OUTPUT_PRESENTATION_POLICY_VERSION,
            enabled: true,
            embedded: true,
            python: true,
            javascript: true,
            nested_shell: true,
            max_source_bytes: 64 * 1024,
            max_regions: 32,
            max_depth: 4,
        }
    }
}

fn deserialize_version<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u32, D::Error> {
    let version = u32::deserialize(deserializer)?;
    if version == SHELL_OUTPUT_PRESENTATION_POLICY_VERSION {
        Ok(version)
    } else {
        Err(serde::de::Error::custom(
            "unsupported shell presentation policy version",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ShellToolOutputConfig;

    #[test]
    fn defaults_and_partial_configuration_round_trip() {
        let defaults = ShellOutputPresentationPolicy::default();
        assert!(defaults.enabled && defaults.embedded);
        assert!(defaults.python && defaults.javascript && defaults.nested_shell);
        let policy: ShellOutputPresentationPolicy =
            toml::from_str("python = false\nmax_regions = 0").unwrap();
        assert!(!policy.python);
        assert!(policy.javascript && policy.nested_shell);
        assert_eq!(policy.max_regions, 0);
        assert_eq!(
            toml::from_str::<ShellOutputPresentationPolicy>(&toml::to_string(&policy).unwrap())
                .unwrap(),
            policy
        );
    }

    #[test]
    fn command_override_preserves_existing_format_commands_precedence() {
        let mut config = ShellToolOutputConfig::default();
        assert!(config.resolve_presentation_policy(None).enabled);
        assert!(!config.resolve_presentation_policy(Some(false)).enabled);
        config.format_commands = false;
        assert!(!config.resolve_presentation_policy(None).enabled);
        assert!(config.resolve_presentation_policy(Some(true)).enabled);
        config.presentation.embedded = false;
        let resolved = config.resolve_presentation_policy(Some(true));
        assert!(resolved.enabled);
        assert!(!resolved.embedded);
        config.presentation.enabled = false;
        assert!(!config.resolve_presentation_policy(Some(true)).enabled);
    }

    #[test]
    fn unsupported_policy_versions_and_fields_fail_closed() {
        assert!(toml::from_str::<ShellOutputPresentationPolicy>("version = 2").is_err());
        assert!(toml::from_str::<ShellOutputPresentationPolicy>("future_runtime = true").is_err());
    }
}
