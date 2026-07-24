#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use serde::Deserialize;
use std::collections::BTreeSet;

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
