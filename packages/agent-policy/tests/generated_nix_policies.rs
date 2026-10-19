#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Explicit cross-repository contract test. Input is a pure Nix evaluation of
//! the authoritative shared policies, not a second implementation of the DSL.

use bcode_agent_policy::{AgentPermissionConfig, evaluate_tool_call};
use bcode_agent_profile::{AgentDecision, EvaluateToolCallRequest, ToolPolicyOperation};
use bcode_shell_command_analysis::analyze;
use bcode_shell_command_analysis_models::ShellAnalysisRequest;
use std::collections::BTreeMap;
use std::path::Path;

fn decision(config: &AgentPermissionConfig, agent: &str, source: &str) -> AgentDecision {
    let request = EvaluateToolCallRequest {
        session_id: bcode_session_models::SessionId::new(),
        agent_id: agent.to_owned(),
        tool_name: "shell.run".to_owned(),
        operation: ToolPolicyOperation::Command {
            command: Some(source.to_owned()),
            analysis: Some(analyze(&ShellAnalysisRequest::posix(source)).unwrap()),
            analysis_error: None,
        },
        aliases: Vec::new(),
        requires_permission: true,
        policy_profile: None,
        cwd: Some("/tmp".to_owned()),
        effective_config_toml: None,
    };
    evaluate_tool_call(&config.agent[agent], &request, Path::new("/tmp"))
        .response
        .decision
}

#[test]
#[ignore = "requires BCODE_GENERATED_POLICIES_JSON from pure Nix evaluation; see docs/permissions.md"]
fn generated_nix_policy_variants_enforce_their_contract() {
    let path = std::env::var("BCODE_GENERATED_POLICIES_JSON").unwrap();
    let variants: BTreeMap<String, AgentPermissionConfig> =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    for name in ["normal", "restricted", "yolo"] {
        let config = &variants[name];
        for source in [
            "git status",
            "NAME=value git status",
            "printf '%s' \"$HOME\"",
        ] {
            assert_eq!(
                decision(config, "build", source),
                AgentDecision::Allow,
                "{name}: {source}"
            );
        }
        for source in [
            "git branch -D SYNTHETIC",
            "git config --global user.name SYNTHETIC",
        ] {
            assert_eq!(
                decision(config, "plan", source),
                AgentDecision::Deny,
                "{name}: {source}"
            );
        }
        for source in ["git branch --list", "git config --get user.name"] {
            assert_eq!(
                decision(config, "plan", source),
                AgentDecision::Allow,
                "{name}: {source}"
            );
        }
        if name != "yolo" {
            for source in [
                "git push --force origin",
                "git -C /tmp push --force origin",
                "git -C /tmp push origin --force",
                "git push origin -f",
                "env git push --force origin",
                "/usr/bin/git push --force origin",
                "NAME=value command git push --force origin",
            ] {
                assert_eq!(
                    decision(config, "build", source),
                    AgentDecision::Deny,
                    "{name}: {source}"
                );
            }
        }
    }
    let restricted = &variants["restricted"];
    for source in [
        "git push",
        "git -C /tmp push",
        "git -c color.ui=false push",
        "git --git-dir=/tmp/repo push",
        "git --no-optional-locks push",
        "env git push",
        "command -- git push",
        "exec git push",
        "/usr/bin/git push",
        "NAME=value git -C /tmp push",
        "git -C \"$HOME\" push",
        "git \"$ACTION\"",
        "git stash",
        "git stash -q",
        "git stash --keep-index",
        "git stash pop",
        "git status; command git push",
    ] {
        assert_eq!(
            decision(restricted, "build", source),
            AgentDecision::Deny,
            "restricted: {source}"
        );
    }
    for source in [
        "git stash list",
        "git --no-pager stash show",
        "git -C /tmp stash list",
    ] {
        assert_eq!(
            decision(restricted, "build", source),
            AgentDecision::Allow,
            "restricted: {source}"
        );
    }
    assert_eq!(
        decision(restricted, "plan", "git -C /tmp status"),
        AgentDecision::Allow
    );
    for name in ["normal", "yolo"] {
        assert_eq!(
            decision(&variants[name], "build", "git push"),
            AgentDecision::Allow,
            "{name}"
        );
    }
}
