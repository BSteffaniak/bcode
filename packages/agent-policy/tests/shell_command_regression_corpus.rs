#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bcode_agent_policy::{Action, AgentConfig, PermissionConfig, evaluate_tool_call};
use bcode_agent_profile::{AgentDecision, EvaluateToolCallRequest, ToolPolicyOperation};
use bcode_shell_command_analysis::analyze;
use bcode_shell_command_analysis_models::ShellAnalysisRequest;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Debug, Deserialize)]
struct Corpus {
    schema_version: u32,
    dialect: String,
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
struct Case {
    id: String,
    classification: String,
    source: String,
    policy: Policy,
    expected: Expected,
    #[serde(default)]
    notes: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Policy {
    rules: Vec<Rule>,
    default_action: String,
    #[serde(default)]
    read_rules: Vec<Rule>,
    #[serde(default)]
    write_rules: Vec<Rule>,
}

#[derive(Debug, Deserialize)]
struct Rule {
    pattern: String,
    action: String,
}

#[derive(Debug, Deserialize)]
struct Expected {
    commands: Vec<String>,
    #[serde(default)]
    executables: Vec<String>,
    #[serde(default)]
    assignments: Vec<String>,
    #[serde(default)]
    aliases: Vec<String>,
    redirections: Vec<Redirection>,
    completeness: String,
    policy: String,
}

#[derive(Debug, Deserialize)]
struct Redirection {
    kind: String,
    target: String,
    dynamic: bool,
}

fn corpus() -> Corpus {
    serde_json::from_str(include_str!(
        "../../../fixtures/shell-command-analysis/corpus.json"
    ))
    .expect("shell command regression corpus must be valid JSON")
}

#[test]
fn corpus_is_complete_and_well_formed() {
    let corpus = corpus();
    assert_eq!(corpus.schema_version, 1);
    assert_eq!(corpus.dialect, "posix");
    assert!(corpus.cases.len() >= 25);

    let mut ids = BTreeSet::new();
    let mut classifications = BTreeSet::new();
    for case in &corpus.cases {
        assert!(
            ids.insert(case.id.as_str()),
            "duplicate case ID: {}",
            case.id
        );
        assert!(!case.source.is_empty(), "empty source: {}", case.id);
        classifications.insert(case.classification.as_str());
        assert!(!case.policy.rules.is_empty(), "missing rules: {}", case.id);
        assert!(
            ["allow", "ask", "deny"].contains(&case.policy.default_action.as_str()),
            "invalid default action: {}",
            case.id
        );
        assert!(
            ["complete", "incomplete", "error"].contains(&case.expected.completeness.as_str()),
            "invalid completeness: {}",
            case.id
        );
        assert!(
            ["allow", "ask", "deny"].contains(&case.expected.policy.as_str()),
            "invalid policy result: {}",
            case.id
        );
        if case.expected.completeness == "complete" {
            assert!(
                !case.expected.commands.is_empty(),
                "complete case has no commands: {}",
                case.id
            );
        }
        for rule in case
            .policy
            .rules
            .iter()
            .chain(&case.policy.read_rules)
            .chain(&case.policy.write_rules)
        {
            assert!(!rule.pattern.is_empty(), "empty rule pattern: {}", case.id);
            assert!(
                ["allow", "ask", "deny"].contains(&rule.action.as_str()),
                "invalid rule action: {}",
                case.id
            );
        }
        for redirection in &case.expected.redirections {
            assert!(
                !redirection.kind.is_empty(),
                "empty redirection kind: {}",
                case.id
            );
            assert!(
                !redirection.target.is_empty(),
                "empty redirection target: {}",
                case.id
            );
            let _ = redirection.dynamic;
        }
        if case.classification == "dynamic_unsupported_fail_closed" {
            assert_ne!(case.expected.policy, "allow", "fail-open case: {}", case.id);
        }
        let _ = (
            &case.expected.executables,
            &case.expected.assignments,
            &case.expected.aliases,
            &case.notes,
        );
    }

    assert_eq!(
        classifications,
        BTreeSet::from([
            "canonicalization_defect",
            "dynamic_unsupported_fail_closed",
            "parser_boundary_defect",
            "policy_configuration_gap",
        ])
    );
}

#[test]
fn corpus_covers_confirmed_failure_classes() {
    let corpus = corpus();
    let ids = corpus
        .cases
        .iter()
        .map(|case| case.id.as_str())
        .collect::<BTreeSet<_>>();
    for required in [
        "newline-denied-sibling",
        "background-denied-sibling",
        "pipeline-denied-leaf",
        "and-or-denied-branches",
        "quoted-pipe-regex",
        "escaped-pipe-regex",
        "quoted-semicolon-printf",
        "quoted-semicolon-sql",
        "quoted-semicolon-awk",
        "quoted-semicolon-git-format",
        "quoted-semicolon-interpreter-argument",
        "heredoc-body-is-data",
        "git-no-pager-reviewed-alias",
        "environment-assignment-prefix",
        "missing-printf-policy",
        "command-v-policy-gap",
        "for-loop-body",
        "if-condition-and-body",
        "subshell-leaves",
        "command-substitution-leaf",
        "process-substitution-leaf",
        "dynamic-command-name",
        "eval-is-dynamic",
        "source-is-dynamic",
        "output-redirection-write",
        "input-redirection-read",
        "syntax-error",
    ] {
        assert!(
            ids.contains(required),
            "missing regression case: {required}"
        );
    }
}

#[test]
fn policy_gaps_are_distinct_from_parser_defects() {
    let corpus = corpus();
    for case in corpus
        .cases
        .iter()
        .filter(|case| case.classification == "policy_configuration_gap")
    {
        assert_eq!(case.expected.completeness, "complete", "{}", case.id);
        assert_ne!(case.expected.policy, "allow", "{}", case.id);
    }
}

fn action(value: &str) -> Action {
    match value {
        "allow" => Action::Allow,
        "ask" => Action::Ask,
        "deny" => Action::Deny,
        other => panic!("unknown action: {other}"),
    }
}

fn evaluate(case: &Case) -> AgentDecision {
    let analysis_result = analyze(&ShellAnalysisRequest::posix(&case.source));
    let (analysis, analysis_error) = match analysis_result {
        Ok(analysis) => (Some(analysis), None),
        Err(error) => (None, Some(error)),
    };
    let config = AgentConfig {
        accent: None,
        tools: BTreeMap::from([("shell.run".to_owned(), true)]),
        permission: PermissionConfig {
            command: case
                .policy
                .rules
                .iter()
                .map(|rule| (rule.pattern.clone(), action(&rule.action)))
                .chain(std::iter::once((
                    "*".to_owned(),
                    action(&case.policy.default_action),
                )))
                .collect(),
            read: case
                .policy
                .read_rules
                .iter()
                .map(|rule| (rule.pattern.clone(), action(&rule.action)))
                .collect(),
            write: case
                .policy
                .write_rules
                .iter()
                .map(|rule| (rule.pattern.clone(), action(&rule.action)))
                .collect(),
            ..PermissionConfig::default()
        },
    };
    let request = EvaluateToolCallRequest {
        session_id: bcode_session_models::SessionId::new(),
        agent_id: "corpus".to_owned(),
        tool_name: "shell.run".to_owned(),
        operation: ToolPolicyOperation::Command {
            command: Some(case.source.clone()),
            analysis,
            analysis_error,
        },
        aliases: Vec::new(),
        requires_permission: true,
        cwd: Some("/tmp/project".to_owned()),
    };
    evaluate_tool_call(&config, &request, Path::new("/tmp/project"))
        .response
        .decision
}

#[test]
fn structured_policy_matches_every_reviewed_corpus_outcome() {
    let corpus = corpus();
    for case in &corpus.cases {
        let expected = match case.expected.policy.as_str() {
            "allow" => AgentDecision::Allow,
            "ask" => AgentDecision::Ask,
            "deny" => AgentDecision::Deny,
            other => panic!("unknown expected decision: {other}"),
        };
        let actual = evaluate(case);
        assert_eq!(actual, expected, "corpus case {}", case.id);
    }
}

#[test]
fn decision_aggregation_and_parse_failure_invariants_are_encoded() {
    let corpus = corpus();
    for id in [
        "newline-denied-sibling",
        "background-denied-sibling",
        "pipeline-denied-leaf",
        "and-or-denied-branches",
        "subshell-leaves",
        "command-substitution-leaf",
        "output-redirection-write",
    ] {
        let case = corpus.cases.iter().find(|case| case.id == id).unwrap();
        assert_eq!(case.expected.policy, "deny", "{id}");
    }
    for case in corpus
        .cases
        .iter()
        .filter(|case| case.expected.completeness != "complete")
    {
        assert_ne!(case.expected.policy, "allow", "fail-open case: {}", case.id);
    }
}
