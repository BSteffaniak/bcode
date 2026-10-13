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

/// Explicit versioned plugin strategy; collaboration requirements remain separate.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    clap::ValueEnum,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum Policy {
    #[default]
    Baseline,
    AdaptiveV1,
}

#[derive(Debug, Clone, Default, clap::Args)]
pub struct Arguments {
    /// Opt in to a versioned strategy without changing delegation requirements.
    #[arg(long, value_enum, default_value = "baseline")]
    pub swarm_policy: Policy,
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
    fn policy_selection_is_explicit_and_preserves_collaboration_requirements() {
        #[derive(clap::Parser)]
        struct Options {
            #[command(flatten)]
            swarm: Arguments,
        }
        use clap::Parser as _;
        let default = Options::try_parse_from(["goal"]).unwrap();
        assert_eq!(default.swarm.swarm_policy, Policy::Baseline);
        assert_eq!(default.swarm.mode(), Mode::Auto);
        for (args, mode) in [
            (vec!["goal", "--swarm-policy", "adaptive-v1"], Mode::Auto),
            (
                vec!["goal", "--swarm-policy", "adaptive-v1", "--collaborate"],
                Mode::Collaborate,
            ),
            (
                vec![
                    "goal",
                    "--swarm-policy",
                    "adaptive-v1",
                    "--agents",
                    "no-delegation",
                ],
                Mode::NoDelegation,
            ),
        ] {
            let parsed = Options::try_parse_from(args).unwrap();
            assert_eq!(parsed.swarm.swarm_policy, Policy::AdaptiveV1);
            assert_eq!(parsed.swarm.mode(), mode);
        }
        assert!(Options::try_parse_from(["goal", "--swarm-policy", "adaptive-v2"]).is_err());
        assert!(serde_json::from_value::<Policy>(serde_json::json!("adaptive-v2")).is_err());
        assert_eq!(
            serde_json::to_value(Policy::AdaptiveV1).unwrap(),
            "adaptive-v1"
        );
    }

    fn configuration(
        request: &crate::PluginWorkflowStartRequest,
        id: &str,
    ) -> bcode_workflow::WorkflowPromptConfiguration {
        serde_json::from_value(request.definition.nodes[id].configuration.clone()).unwrap()
    }

    #[test]
    fn baseline_assembly_preserves_existing_prompt_product() {
        use crate::{CollaborationMode, SetupKind};
        let input = crate::LoopWorkflowInput::new("implement".into(), "verify".into(), 2).unwrap();
        let base = crate::loop_workflow_spec(&input).unwrap();
        let base_prompt = |id: &str| {
            serde_json::from_value::<bcode_workflow::WorkflowPromptConfiguration>(
                base.definition().nodes[id].configuration.clone(),
            )
            .unwrap()
            .system_prompt
        };
        for progress in [false, true] {
            for mode in [CollaborationMode::Auto, CollaborationMode::Requested] {
                let legacy = crate::build_start_request(
                    &input,
                    SessionId::new(),
                    SetupKind::Goal,
                    mode,
                    progress,
                    None,
                )
                .unwrap();
                let explicit = crate::build_start_request_with_policy(
                    &input,
                    SessionId::new(),
                    SetupKind::Goal,
                    mode,
                    progress,
                    None,
                    Policy::Baseline,
                )
                .unwrap();
                assert_eq!(legacy.definition, explicit.definition);
                let mut implementation = format!(
                    "{}\n\n{}\n\n{}",
                    base_prompt("loop.implementation"),
                    include_str!("../prompts/goal-optional-coordination.md"),
                    include_str!("../prompts/goal-iteration-guidance.md")
                );
                let mut evaluation = format!(
                    "{}\n\n{}",
                    base_prompt("loop.evaluation"),
                    include_str!("../prompts/goal-stop-condition-guidance.md")
                );
                if mode == CollaborationMode::Requested {
                    implementation = format!(
                        "{implementation}\n\n{}\n\n{}",
                        include_str!("../prompts/goal-coordination.md"),
                        include_str!("../prompts/goal-iteration-guidance.md")
                    );
                    evaluation = format!(
                        "{evaluation}\n\n{}\n\n{}",
                        include_str!("../prompts/goal-collaboration-evaluation.md"),
                        include_str!("../prompts/goal-stop-condition-guidance.md")
                    );
                }
                assert_eq!(
                    configuration(&explicit, "loop.implementation").system_prompt,
                    implementation
                );
                assert_eq!(
                    configuration(&explicit, "loop.evaluation").system_prompt,
                    evaluation
                );
            }
        }
    }

    #[test]
    fn adaptive_prompt_product_preserves_user_requirements_and_evidence_gaps() {
        use crate::{CollaborationMode, SetupKind};
        let objective =
            "Alice implements first; Bob independently verifies after Alice. Preserve my files.";
        let input = crate::LoopWorkflowInput::new(
            objective.into(),
            "Verify every original requirement".into(),
            2,
        )
        .unwrap();
        for progress in [false, true] {
            for mode in [
                CollaborationMode::Auto,
                CollaborationMode::Requested,
                CollaborationMode::NoDelegation,
            ] {
                let request = crate::build_start_request_with_policy(
                    &input,
                    SessionId::new(),
                    SetupKind::Goal,
                    mode,
                    progress,
                    None,
                    Policy::AdaptiveV1,
                )
                .unwrap();
                assert_eq!(request.input["implementation_prompt"], objective);
                assert_eq!(
                    request.input["stop_condition"],
                    "Verify every original requirement"
                );
                let implementation = configuration(&request, "loop.implementation");
                let evaluator = configuration(&request, "loop.evaluation");
                for clause in [
                    "User-designated roles, ordering, timing",
                    "keep tiny local work local",
                    "Fan out genuinely independent tasks together",
                    "exact required artifact/document locators",
                    "finish this activation normally",
                    "targeted correction addresses only specific unresolved requirements",
                ] {
                    assert!(
                        implementation.system_prompt.contains(clause),
                        "missing behavior: {clause}"
                    );
                }
                for clause in [
                    "Start with the contribution actually supplied",
                    "not the complete worker-result envelope",
                    "only for a concrete coverage",
                    "checksum-verified",
                    "Low confidence alone",
                ] {
                    assert!(
                        evaluator.system_prompt.contains(clause),
                        "missing behavior: {clause}"
                    );
                }
                assert!(evaluator.read_only);
                assert_eq!(
                    evaluator.tool_capability,
                    bcode_workflow::WorkflowToolCapability::ReadOnly
                );
                if mode == CollaborationMode::Requested {
                    assert!(implementation.system_prompt.contains(
                        "Collaboration is required: obtain actual useful worker contributions"
                    ));
                }
                if mode == CollaborationMode::NoDelegation {
                    assert_eq!(request.limits.descendant_cap, 0);
                    for node in request
                        .definition
                        .nodes
                        .values()
                        .filter(|node| node.kind == bcode_workflow::NodeKind::Agent)
                    {
                        let config: bcode_workflow::WorkflowPromptConfiguration =
                            serde_json::from_value(node.configuration.clone()).unwrap();
                        assert!(!config.tool_allowlist.is_empty());
                        assert!(
                            !config
                                .tool_allowlist
                                .iter()
                                .any(|tool| tool.starts_with("workflow.stage")
                                    || tool == "workflow.publish_run_graph_edit")
                        );
                        assert!(config.system_prompt.contains("No delegation is permitted"));
                    }
                }
            }
        }
    }

    #[test]
    fn generation_policy_preserves_baseline_and_original_task_data() {
        let objective = "Alice implements first; Bob reviews afterward";
        for mode in [
            CollaborationMode::Auto,
            CollaborationMode::Requested,
            CollaborationMode::NoDelegation,
        ] {
            let baseline =
                crate::goal::generation_request(objective, "Keep my changes", false, mode);
            let explicit = crate::goal::generation_request_with_policy(
                objective,
                "Keep my changes",
                false,
                mode,
                Policy::Baseline,
            );
            assert_eq!(baseline.system_prompt, explicit.system_prompt);
            assert_eq!(baseline.prompt, explicit.prompt);
            assert_eq!(baseline.output_schema, explicit.output_schema);
            let candidate = crate::goal::generation_request_with_policy(
                objective,
                "Keep my changes",
                false,
                mode,
                Policy::AdaptiveV1,
            );
            assert_eq!(baseline.prompt, candidate.prompt);
            assert_eq!(baseline.output_schema, candidate.output_schema);
            for clause in [
                "Selected strategy: adaptive-v1",
                "without overriding user requirements, roles or timing",
                "No-delegation prohibits workers",
                "summaries are not full worker results",
            ] {
                assert!(candidate.system_prompt.contains(clause));
            }
        }
        let command =
            crate::goal::start_response("--swarm-policy adaptive-v1 --agents no-delegation");
        assert!(command.success);
        match &command.effects[0] {
            crate::CommandEffect::OpenPluginSurface { options, .. } => {
                assert_eq!(options["swarm_policy"], "adaptive-v1");
                assert_eq!(options["agents"], "no-delegation");
            }
            _ => panic!("goal command must open goal setup"),
        }
        assert!(!crate::goal::start_response("--swarm-policy adaptive-v2").success);
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
