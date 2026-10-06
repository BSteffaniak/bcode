//! Optional judgement policy. Off is checked before discovery or verification.

use bcode_model::judgement::{Feature, SetupSelection};
use serde::{Deserialize, Serialize};

/// Automatic optional use, or a strict zero-call opt-out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JudgementMode {
    /// Invoke an eligible verified provider only at a caller-owned trigger.
    #[default]
    Auto,
    /// No provider calls, including discovery and verification.
    Off,
}

/// Global and per-feature optional judgement policy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct JudgementConfig {
    /// Global opt-out.
    pub mode: JudgementMode,
    /// Optional exact provider/model/account pin, honored without switching accounts.
    pub selection: Option<SetupSelection>,
    /// Tool-result classification.
    pub tool_result_triage: JudgementMode,
    /// Retrieved-context reranking.
    pub context_reranking: JudgementMode,
    /// Advisory progress monitoring.
    pub progress_monitoring: JudgementMode,
    /// Skill and tool recommendations.
    pub recommendations: JudgementMode,
    /// Session categorization.
    pub session_categorization: JudgementMode,
    /// Evaluation categorization.
    pub eval_categorization: JudgementMode,
}

impl JudgementConfig {
    /// Whether this feature may perform optional provider work.
    #[must_use]
    pub const fn enabled(&self, feature: Feature) -> bool {
        let mode = match feature {
            Feature::ToolResultTriage => self.tool_result_triage,
            Feature::ContextReranking => self.context_reranking,
            Feature::ProgressMonitoring => self.progress_monitoring,
            Feature::Recommendations => self.recommendations,
            Feature::SessionCategorization => self.session_categorization,
            Feature::EvalCategorization => self.eval_categorization,
        };
        matches!(self.mode, JudgementMode::Auto) && matches!(mode, JudgementMode::Auto)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_opt_outs() {
        let features = [
            Feature::ToolResultTriage,
            Feature::ContextReranking,
            Feature::ProgressMonitoring,
            Feature::Recommendations,
            Feature::SessionCategorization,
            Feature::EvalCategorization,
        ];
        let mut config = JudgementConfig::default();
        assert!(features.iter().all(|feature| config.enabled(*feature)));
        config.context_reranking = JudgementMode::Off;
        assert!(!config.enabled(Feature::ContextReranking));
        assert!(config.enabled(Feature::Recommendations));
        config.mode = JudgementMode::Off;
        assert!(features.iter().all(|feature| !config.enabled(*feature)));
    }

    #[test]
    fn policy_round_trip_and_unknown_modes_fail_closed() {
        let config: JudgementConfig =
            toml::from_str("mode = 'off'\nrecommendations = 'off'").unwrap();
        assert_eq!(config.mode, JudgementMode::Off);
        assert_eq!(
            toml::from_str::<JudgementConfig>(&toml::to_string(&config).unwrap()).unwrap(),
            config
        );
        assert!(toml::from_str::<JudgementConfig>("mode = 'future'").is_err());
    }
}
