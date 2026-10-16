//! Explicit opt-in configuration for paid session milestone generation.

use serde::{Deserialize, Serialize};

/// Plugin-owned milestone policy inputs. Selection uses the ordinary model profile resolver.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MilestonesConfig {
    /// Paid generation opt-in; false prohibits all discovery and inference.
    pub enabled: bool,
    /// Follow the newest accepted milestone unless the session has an explicit name.
    pub follow_title: bool,
    /// Named conversational model profile; no implicit small-model guess is made.
    pub model_profile: Option<String>,
    /// Settled conversational turns before the first evaluation.
    pub first_turns: u32,
    /// Newly settled conversational turns between subsequent evaluations.
    pub subsequent_turns: u32,
    /// Alternative first/subsequent threshold, counting only eligible normalized context tokens.
    pub subsequent_tokens: u64,
    /// Minimum interval between evaluations, including no-milestone results.
    pub min_interval_secs: u64,
    /// Bounded request context budget (not the complete conversational context).
    pub max_input_tokens: u32,
    /// Maximum generated output tokens.
    pub max_output_tokens: u32,
    /// Global concurrently admitted auxiliary operations; excess work is not queued.
    pub max_concurrent: u32,
    /// Durable per-session paid request allowance; zero deliberately prevents admission.
    pub max_requests_per_session: u64,
}

impl Default for MilestonesConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            follow_title: true,
            model_profile: None,
            first_turns: 3,
            subsequent_turns: 5,
            subsequent_tokens: 8_000,
            min_interval_secs: 300,
            max_input_tokens: 4_000,
            max_output_tokens: 300,
            max_concurrent: 2,
            max_requests_per_session: 100,
        }
    }
}

impl MilestonesConfig {
    /// Check limits before any paid work. Invalid settings fail closed.
    #[must_use]
    pub fn valid(&self) -> bool {
        self.first_turns > 0
            && self.subsequent_turns > 0
            && self.subsequent_tokens > 0
            && self.max_input_tokens > 0
            && self.max_input_tokens <= 16_000
            && self.max_output_tokens > 0
            && self.max_output_tokens <= 1_000
            && self.max_concurrent > 0
            && self.max_concurrent <= 32
            && self
                .model_profile
                .as_ref()
                .is_none_or(|name| !name.trim().is_empty())
    }
}

impl crate::ConfigDocSchema for MilestonesConfig {
    fn section_name() -> &'static str {
        "milestones"
    }
    fn section_description() -> &'static str {
        "Explicit opt-in paid session milestone generation."
    }
    fn field_docs() -> Vec<crate::FieldDoc> {
        vec![
            crate::config_field(
                "enabled",
                "boolean",
                "Paid generation opt-in; defaults false.",
            ),
            crate::config_field(
                "follow_title",
                "boolean",
                "Follow accepted milestone headers unless explicitly named; defaults true.",
            ),
            crate::config_field(
                "model_profile",
                "string",
                "Required explicit centrally resolved model profile.",
            ),
            crate::config_field(
                "first_turns",
                "integer",
                "First evaluation settled conversational turn threshold; defaults 3.",
            ),
            crate::config_field(
                "subsequent_turns",
                "integer",
                "Subsequent new settled turn threshold; defaults 5.",
            ),
            crate::config_field(
                "subsequent_tokens",
                "integer",
                "Alternative new eligible token threshold; defaults 8000.",
            ),
            crate::config_field(
                "min_interval_secs",
                "integer",
                "Automatic evaluation minimum interval; defaults 300 seconds.",
            ),
            crate::config_field(
                "max_input_tokens",
                "integer",
                "Complete request admission budget; defaults 4000 tokens.",
            ),
            crate::config_field(
                "max_output_tokens",
                "integer",
                "Generated output cap; defaults 300 tokens.",
            ),
            crate::config_field(
                "max_requests_per_session",
                "integer",
                "Durable per-session request budget; defaults 100; zero disables admission.",
            ),
            crate::config_field(
                "max_concurrent",
                "integer",
                "Concurrent auxiliary operation bound; defaults 2, excess not queued.",
            ),
        ]
    }
    fn default_values() -> std::collections::BTreeMap<String, String> {
        std::collections::BTreeMap::from([
            ("enabled".into(), "false".into()),
            ("follow_title".into(), "true".into()),
            ("first_turns".into(), "3".into()),
            ("subsequent_turns".into(), "5".into()),
            ("subsequent_tokens".into(), "8000".into()),
            ("min_interval_secs".into(), "300".into()),
            ("max_input_tokens".into(), "4000".into()),
            ("max_output_tokens".into(), "300".into()),
            ("max_concurrent".into(), "2".into()),
            ("max_requests_per_session".into(), "100".into()),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paid_generation_is_explicit_and_title_following_defaults_on() {
        let config: MilestonesConfig = toml::from_str("").unwrap();
        assert!(!config.enabled);
        assert!(config.follow_title);
        assert!(config.valid());
        assert_eq!(
            config,
            toml::from_str(&toml::to_string(&config).unwrap()).unwrap()
        );
    }

    #[test]
    fn invalid_limits_and_unknown_configuration_fail_closed() {
        assert!(
            !MilestonesConfig {
                max_input_tokens: 0,
                ..MilestonesConfig::default()
            }
            .valid()
        );
        assert!(
            !MilestonesConfig {
                model_profile: Some(" ".into()),
                ..MilestonesConfig::default()
            }
            .valid()
        );
        assert!(toml::from_str::<MilestonesConfig>("future = true").is_err());
    }
}
