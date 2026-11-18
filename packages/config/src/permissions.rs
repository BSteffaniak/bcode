//! How a session steps into and out of permission bypass at runtime.

use serde::{Deserialize, Serialize};

/// When stepping a session out of permission bypass takes effect.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BypassStepOut {
    /// At the running turn's next tool call.
    #[default]
    Immediate,
    /// From the next turn; the running turn finishes as it started.
    NextTurn,
}

/// What stepping a session into permission bypass does with the permission prompts it is
/// already waiting on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BypassWaitingPrompts {
    /// Approve them (once each), so the running turn goes on.
    Approve,
    /// Offer to approve them.
    #[default]
    Ask,
    /// Leave them to be answered one by one.
    Leave,
}

/// Runtime permission bypass behavior (`[permissions]`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PermissionsConfig {
    /// When stepping out of bypass takes effect.
    pub bypass_step_out: BypassStepOut,
    /// What stepping into bypass does with prompts already waiting.
    pub bypass_waiting_prompts: BypassWaitingPrompts,
}

impl crate::ConfigDocSchema for PermissionsConfig {
    fn section_name() -> &'static str {
        "permissions"
    }
    fn section_description() -> &'static str {
        "Stepping sessions into and out of permission bypass at runtime."
    }
    fn field_docs() -> Vec<crate::FieldDoc> {
        vec![
            crate::config_field(
                "bypass_step_out",
                "string",
                "When stepping out of bypass applies: immediate (the running turn's next tool \
                 call) or next_turn; defaults immediate.",
            ),
            crate::config_field(
                "bypass_waiting_prompts",
                "string",
                "What stepping into bypass does with prompts already waiting: approve, ask, or \
                 leave; defaults ask.",
            ),
        ]
    }
    fn default_values() -> std::collections::BTreeMap<String, String> {
        std::collections::BTreeMap::from([
            ("bypass_step_out".into(), "immediate".into()),
            ("bypass_waiting_prompts".into(), "ask".into()),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stepping_out_is_immediate_and_waiting_prompts_are_offered_by_default() {
        let config: PermissionsConfig = toml::from_str("").unwrap();
        assert_eq!(config.bypass_step_out, BypassStepOut::Immediate);
        assert_eq!(config.bypass_waiting_prompts, BypassWaitingPrompts::Ask);
        let config: PermissionsConfig =
            toml::from_str("bypass_step_out = \"next_turn\"\nbypass_waiting_prompts = \"leave\"")
                .unwrap();
        assert_eq!(config.bypass_step_out, BypassStepOut::NextTurn);
        assert_eq!(config.bypass_waiting_prompts, BypassWaitingPrompts::Leave);
        assert!(toml::from_str::<PermissionsConfig>("bypass_step_out = \"later\"").is_err());
        assert!(toml::from_str::<PermissionsConfig>("future = true").is_err());
    }
}
