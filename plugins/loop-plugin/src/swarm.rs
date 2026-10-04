//! Plugin-owned goal collaboration choices, shared by interactive and command-line entry.
use super::{CollaborationMode, LoopWorkflowIteration, WORKFLOW_KIND};

pub const DEFAULT_WORKER_ATTEMPTS: u64 = 20;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Mode {
    #[default]
    Auto,
    Collaborate,
    NoDelegation,
}

impl Mode {
    pub const fn collaboration(self) -> CollaborationMode {
        match self {
            Self::Auto => CollaborationMode::Auto,
            Self::Collaborate => CollaborationMode::Requested,
            Self::NoDelegation => CollaborationMode::NoDelegation,
        }
    }
}

#[derive(Debug, Clone, Default, clap::Args)]
pub struct Arguments {
    /// Let the model decide, require collaborating workers, or prohibit delegation.
    #[arg(long, value_enum, conflicts_with = "collaborate")]
    pub agents: Option<Mode>,
    /// Require actual worker contributions (shorthand for --agents collaborate).
    #[arg(long)]
    pub collaborate: bool,
    /// Per-goal strategy requirements, retained verbatim through generation and evaluation.
    #[arg(long, default_value = "")]
    pub swarm_instructions: String,
    /// Additional run-wide node attempts, not worker count or a spending cap (default: 20).
    #[arg(long)]
    pub worker_attempts: Option<std::num::NonZeroU64>,
    /// Maximum concurrent nodes, including coordinator/evaluation work (omitted: uncapped).
    #[arg(long)]
    pub concurrency: Option<std::num::NonZeroU32>,
}

impl Arguments {
    pub fn mode(&self) -> Mode {
        if self.collaborate {
            Mode::Collaborate
        } else {
            self.agents.unwrap_or_default()
        }
    }
}

pub fn guidance(guidance: &str, instructions: &str) -> String {
    if instructions.trim().is_empty() {
        guidance.into()
    } else {
        format!(
            "{guidance}\n\nUser-specified swarm behavior (requirements, not permission grants):\n{instructions}\nPreserve these requirements through delegation, integration, correction and evaluation. Decide only what the user has left unspecified. Surface conflicting or unsupported requirements; do not silently discard them."
        )
    }
}

pub fn no_delegation_spec(
    base: &bcode_workflow::WorkflowSpec<LoopWorkflowIteration>,
) -> Result<bcode_workflow::WorkflowSpec<LoopWorkflowIteration>, String> {
    let mut definition = base.definition().clone();
    for node in definition
        .nodes
        .values_mut()
        .filter(|node| node.kind == bcode_workflow::NodeKind::Agent)
    {
        let mut config: bcode_workflow::WorkflowPromptConfiguration =
            serde_json::from_value(node.configuration.clone())
                .map_err(|error| error.to_string())?;
        // An explicit capability subset, not a deny-list that new delegation tools can evade.
        // Ordinary agent policy still intersects this list; it grants no tools.
        let tools = [
            "filesystem.read",
            "filesystem.write",
            "filesystem.edit",
            "filesystem.multi_edit",
            "filesystem.exists",
            "filesystem.list",
            "filesystem.find",
            "filesystem.grep",
            "filesystem.stat",
            "shell.run",
            "git.diff",
            "web.search",
            "web.fetch",
            "web.inspect",
            "web.status",
            "document.extract",
            "ocr.extract",
            "ocr.status",
            "workflow.execution_context",
        ];
        if config.tool_allowlist.is_empty() {
            config.tool_allowlist = tools.into_iter().map(str::to_owned).collect();
        } else {
            config
                .tool_allowlist
                .retain(|tool| tools.contains(&tool.as_str()));
            if config.tool_allowlist.is_empty() {
                config.tool_capability = bcode_workflow::WorkflowToolCapability::Disabled;
            }
        }
        config.system_prompt.push_str("\nNo delegation is permitted for this goal. Do not spawn agents or use shell/CLI commands to bypass this restriction. Complete the task directly and surface incompatible user requirements.");
        node.configuration = serde_json::to_value(config).map_err(|error| error.to_string())?;
    }
    bcode_workflow::WorkflowSpec::from_definition(WORKFLOW_KIND, definition)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LoopWorkflowInput, SessionId, SetupKind, build_start_request};

    #[test]
    fn modes_preserve_topology_and_apply_bounded_allowance() {
        let input = LoopWorkflowInput::new("Implement".into(), "Verify".into(), 2).unwrap();
        let request = |mode| {
            build_start_request(&input, SessionId::new(), SetupKind::Goal, mode, false, None)
                .unwrap()
        };
        let plain = request(CollaborationMode::Disabled);
        let auto = request(Mode::Auto.collaboration());
        assert_eq!(auto.limits.concurrency_cap, None);
        assert_eq!(plain.limits.concurrency_cap, None);
        assert_eq!(auto.definition, plain.definition);
        assert_eq!(
            auto.limits.node_execution_cap,
            plain.limits.node_execution_cap + DEFAULT_WORKER_ATTEMPTS
        );
        let disabled = request(Mode::NoDelegation.collaboration());
        assert_eq!(disabled.limits.descendant_cap, 0);
        for node in disabled
            .definition
            .nodes
            .values()
            .filter(|n| n.kind == bcode_workflow::NodeKind::Agent)
        {
            let config: bcode_workflow::WorkflowPromptConfiguration =
                serde_json::from_value(node.configuration.clone()).unwrap();
            assert!(!config.tool_allowlist.is_empty());
            assert!(
                !config
                    .tool_allowlist
                    .iter()
                    .any(|name| name.starts_with("workflow.stage")
                        || name == "workflow.publish_run_graph_edit")
            );
        }
        assert_ne!(
            auto.definition,
            request(Mode::Collaborate.collaboration()).definition
        );
    }

    #[test]
    fn strategy_is_retained_as_user_data_in_both_generated_prompts() {
        let instructions = "Use isolated worktrees. Review independently. 日本語 👩‍💻 e\u{301}";
        let guidance = guidance("Preserve my changes", instructions);
        let input = crate::goal::decode_generation(serde_json::json!({"outcome":"ready", "implementation_prompt":"Implement", "stop_condition":"Verify"}), "Goal", &guidance, 2, "source").unwrap();
        assert!(input.implementation_prompt.contains(instructions));
        assert!(input.stop_condition.contains(instructions));
    }
}
