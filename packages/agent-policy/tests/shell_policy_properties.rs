#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bcode_agent_policy::{Action, AgentConfig, PermissionConfig, evaluate_tool_call};
use bcode_agent_profile::{AgentDecision, EvaluateToolCallRequest, ToolPolicyOperation};
use bcode_shell_command_analysis::analyze;
use bcode_shell_command_analysis_models::ShellAnalysisRequest;
use proptest::prelude::*;
use std::collections::BTreeMap;
use std::path::Path;

fn request(source: &str) -> EvaluateToolCallRequest {
    EvaluateToolCallRequest {
        session_id: bcode_session_models::SessionId::new(),
        agent_id: "property".to_owned(),
        tool_name: "shell.run".to_owned(),
        operation: ToolPolicyOperation::Command {
            command: Some(source.to_owned()),
            analysis: analyze(&ShellAnalysisRequest::posix(source)).ok(),
            analysis_error: analyze(&ShellAnalysisRequest::posix(source)).err(),
        },
        aliases: Vec::new(),
        requires_permission: true,
        policy_profile: None,
        cwd: Some("/tmp/project".to_owned()),
    }
}

proptest! {
    #[test]
    fn any_denied_subject_denies_aggregate(
        allowed in "[a-z]{1,16}",
        denied in "[a-z]{1,16}",
        separator in prop_oneof![Just(";"), Just("\n"), Just(" & "), Just(" | "), Just(" && "), Just(" || ")],
    ) {
        let source = format!("printf {allowed}{separator}rm {denied}");
        let config = AgentConfig {
            accent: None,
            tools: BTreeMap::from([("shell.run".to_owned(), true)]),
            permission: PermissionConfig {
                command: BTreeMap::from([
                    ("printf *".to_owned(), Action::Allow),
                    ("rm *".to_owned(), Action::Deny),
                ]),
                ..PermissionConfig::default()
            },
        };
        let result = evaluate_tool_call(&config, &request(&source), Path::new("/tmp/project"));
        prop_assert_eq!(result.response.decision, AgentDecision::Deny);
        let expected = format!("rm {denied}");
        prop_assert_eq!(result.command_part.as_deref(), Some(expected.as_str()));
    }
}

#[test]
fn parser_errors_and_dynamic_commands_never_allow() {
    let config = AgentConfig {
        accent: None,
        tools: BTreeMap::from([("shell.run".to_owned(), true)]),
        permission: PermissionConfig {
            command: BTreeMap::from([("*".to_owned(), Action::Allow)]),
            ..PermissionConfig::default()
        },
    };
    for source in ["if true; then", "cmd=printf; \"$cmd\" ok"] {
        let result = evaluate_tool_call(&config, &request(source), Path::new("/tmp/project"));
        assert_ne!(result.response.decision, AgentDecision::Allow, "{source}");
    }
}
