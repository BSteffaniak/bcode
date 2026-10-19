#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]

use bcode_agent_policy::{
    Action, AgentConfig, PermissionConfig, evaluate_tool_call, preview_tool_call,
};
use bcode_agent_profile::{AgentDecision, EvaluateToolCallRequest, ToolPolicyOperation};
use bcode_shell_command_analysis::analyze;
use bcode_shell_command_analysis_models::ShellAnalysisRequest;
use std::{collections::BTreeMap, path::Path};

fn config(legacy: &[(&str, Action)], words: &[(&str, Action)]) -> AgentConfig {
    AgentConfig {
        permission: PermissionConfig {
            command: legacy.iter().map(|(s, a)| ((*s).to_owned(), *a)).collect(),
            command_patterns: words.iter().map(|(s, a)| ((*s).to_owned(), *a)).collect(),
            write: BTreeMap::from([("**".to_owned(), Action::Allow)]),
            ..PermissionConfig::default()
        },
        ..AgentConfig::default()
    }
}

fn evaluate(config: &AgentConfig, source: &str) -> AgentDecision {
    let analysis = analyze(&ShellAnalysisRequest::posix(source));
    let request = EvaluateToolCallRequest {
        session_id: bcode_session_models::SessionId::new(),
        agent_id: "build".to_owned(),
        tool_name: "shell.run".to_owned(),
        operation: ToolPolicyOperation::Command {
            command: Some(source.to_owned()),
            analysis: analysis.as_ref().ok().cloned(),
            analysis_error: analysis.err(),
        },
        aliases: Vec::new(),
        requires_permission: true,
        policy_profile: None,
        cwd: None,
        effective_config_toml: None,
    };
    let result = evaluate_tool_call(config, &request, Path::new("/tmp"));
    assert_eq!(
        preview_tool_call(config, &request, Path::new("/tmp")).decision,
        result.response.decision
    );
    result.response.decision
}

#[test]
fn broad_rules_authorize_data_without_implicit_asks() {
    for action in [Action::Allow, Action::Ask, Action::Deny] {
        let policy = config(&[("*", action)], &[]);
        let expected = match action {
            Action::Allow => AgentDecision::Allow,
            Action::Ask => AgentDecision::Ask,
            Action::Deny => AgentDecision::Deny,
        };
        for source in [
            "exit \"$result\"",
            "exit $?",
            "printf '%s' \"$value\"",
            "printf %s $values",
            "printf %s \"$@\"",
            "env MODE=test printf %s \"$value\"",
            "cargo test > /tmp/test.log 2>&1; result=$?; tail -n 18 /tmp/test.log; exit \"$result\"",
        ] {
            assert_eq!(evaluate(&policy, source), expected, "{source}");
        }
    }
}

#[test]
fn dsl_cardinality_and_restrictions_are_preserved() {
    for (pattern, source, expected) in [
        ("exit <arg>", "exit \"$result\"", AgentDecision::Allow),
        ("exit <arg>", "exit $result", AgentDecision::Deny),
        ("exit 0", "exit \"$result\"", AgentDecision::Deny),
        ("exit /[0-9]+/", "exit \"$result\"", AgentDecision::Deny),
        ("printf ...", "printf \"$@\"", AgentDecision::Allow),
        ("git <arg>", "git \"$@\"", AgentDecision::Deny),
    ] {
        assert_eq!(
            evaluate(
                &config(&[("*", Action::Deny)], &[(pattern, Action::Allow)]),
                source
            ),
            expected,
            "{pattern}: {source}"
        );
    }
    let policy = config(
        &[("*", Action::Allow)],
        &[
            ("git stash ...", Action::Deny),
            ("git push ...", Action::Ask),
        ],
    );
    for (source, expected) in [
        ("git \"$op\"", AgentDecision::Deny),
        ("git diff \"$args\"", AgentDecision::Allow),
        ("git \"diff-$name\"", AgentDecision::Allow),
        ("git push \"$remote\"", AgentDecision::Ask),
    ] {
        assert_eq!(evaluate(&policy, source), expected, "{source}");
    }
    assert_eq!(
        evaluate(
            &config(&[("*", Action::Allow)], &[("git push ...", Action::Ask)]),
            "git \"$op\""
        ),
        AgentDecision::Ask
    );
}

#[test]
fn legacy_restrictions_and_nested_execution_cannot_be_laundered() {
    let policy = config(
        &[
            ("*", Action::Allow),
            ("rm -rf *", Action::Deny),
            ("dangerous", Action::Deny),
        ],
        &[],
    );
    for source in [
        "rm $args",
        "printf '%s' \"$(dangerous)\"",
        "MODE=$(dangerous) printf ok",
        "printf ok; dangerous",
        "printf '${x}'; dangerous",
    ] {
        assert_eq!(evaluate(&policy, source), AgentDecision::Deny, "{source}");
    }
    for source in [
        "$EXEC ok",
        "eval \"$SCRIPT\"",
        "printf \"${x:-$(dangerous)}\"",
        "printf \"$((1 + $(dangerous)))\"",
    ] {
        assert_eq!(evaluate(&policy, source), AgentDecision::Deny, "{source}");
    }
}
