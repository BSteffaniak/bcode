#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Pi/OpenCode-style agent policy parsing and evaluation.

mod compiled_commands;
mod environment;
mod execution_views;
mod legacy_symbolic;
mod shell_subject;
mod word_patterns;

use compiled_commands::CompiledCommandLayer;

pub use bcode_agent_policy_models::{
    Action, AgentConfig, AgentPermissionConfig, CommandPolicy, CommandPolicyLayer,
    PermissionConfig, default_external_directory_action,
};

use bcode_agent_profile::{
    AgentDecision, EvaluateToolCallRequest, EvaluateToolCallResponse, ShellPolicyDiagnostic,
    ShellPolicySubjectKind, ToolPolicyOperation,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Version of shell matching semantics included in pinned policy identities.
pub const SHELL_POLICY_SEMANTICS_VERSION: u32 = 5;

/// Built-in build agent ID.
pub const BUILD_AGENT: &str = "build";
/// Built-in plan agent ID.
pub const PLAN_AGENT: &str = "plan";

/// Compiled command permission rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub pattern: String,
    pub action: Action,
    pub specificity: usize,
}

/// Policy evaluation detail useful for debugging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyEvaluation {
    pub response: EvaluateToolCallResponse,
    pub matched_rule: Option<String>,
    pub command_part: Option<String>,
    /// Secret-safe command rule locators collected during evaluation.
    pub matches: Vec<bcode_agent_profile::PreviewCommandMatch>,
}

/// Secret-safe read-only preview. No command source, arguments, paths, or rule
/// text are copied into this public diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyPreview {
    /// The decision returned by normal tool-call evaluation.
    pub decision: AgentDecision,
    /// Whether evaluation selected a configured rule.
    pub matched_rule: bool,
    /// Matching configured rules, without rule text or command values.
    pub matches: Vec<bcode_agent_profile::PreviewCommandMatch>,
}

/// Preview a normalized operation through the exact enforcement evaluation path.
#[must_use]
pub fn preview_tool_call(
    config: &AgentConfig,
    request: &EvaluateToolCallRequest,
    cwd: &Path,
) -> PolicyPreview {
    let result = evaluate_tool_call(config, request, cwd);
    PolicyPreview {
        decision: result.response.decision,
        matched_rule: result.matched_rule.is_some(),
        matches: result.matches,
    }
}

/// Return the built-in fallback plan/build config.
#[must_use]
pub fn default_config() -> AgentPermissionConfig {
    let mut agent = BTreeMap::new();
    agent.insert(
        BUILD_AGENT.to_string(),
        AgentConfig {
            accent: None,
            tools: BTreeMap::new(),
            permission: PermissionConfig {
                command: BTreeMap::from([("*".to_string(), Action::Ask)]),
                external_directory: Action::Allow,
                web: BTreeMap::from([("*".to_string(), Action::Ask)]),
                ..PermissionConfig::default()
            },
        },
    );
    agent.insert(
        PLAN_AGENT.to_string(),
        AgentConfig {
            accent: None,
            tools: BTreeMap::new(),
            permission: PermissionConfig {
                command: BTreeMap::from([
                    ("*".to_string(), Action::Deny),
                    ("cargo check".to_string(), Action::Allow),
                    ("cargo check *".to_string(), Action::Allow),
                    ("cargo test".to_string(), Action::Allow),
                    ("cargo test *".to_string(), Action::Allow),
                    ("git diff".to_string(), Action::Allow),
                    ("git diff *".to_string(), Action::Allow),
                    ("git status".to_string(), Action::Allow),
                    ("git status *".to_string(), Action::Allow),
                    ("ls".to_string(), Action::Allow),
                    ("ls *".to_string(), Action::Allow),
                    ("rg *".to_string(), Action::Allow),
                ]),
                external_directory: Action::Allow,
                web: BTreeMap::from([("*".to_string(), Action::Ask)]),
                ..PermissionConfig::default()
            },
        },
    );
    AgentPermissionConfig { agent }
}

/// Return an agent config from a loaded config, falling back to build/default config.
#[must_use]
pub fn agent_config(config: &AgentPermissionConfig, agent_id: &str) -> AgentConfig {
    config
        .agent
        .get(agent_id)
        .or_else(|| config.agent.get(BUILD_AGENT))
        .cloned()
        .unwrap_or_else(|| {
            default_config()
                .agent
                .remove(BUILD_AGENT)
                .unwrap_or_default()
        })
}

/// Compute exact model-callable tool IDs visible for this agent.
#[must_use]
pub fn active_tools_for(config: &AgentConfig) -> Vec<String> {
    config
        .tools
        .iter()
        .filter_map(|(tool, enabled)| enabled.then_some(tool.clone()))
        .collect()
}

/// Evaluate a Bcode tool call against an agent policy.
#[must_use]
pub fn evaluate_tool_call(
    config: &AgentConfig,
    request: &EvaluateToolCallRequest,
    cwd: &Path,
) -> PolicyEvaluation {
    if tool_enabled(config, request) == Some(false) {
        return evaluation(
            AgentDecision::Deny,
            format!(
                "{} agent disabled tool {}",
                request.agent_id, request.tool_name
            ),
            None,
            None,
        );
    }

    if let Some(path) = external_path(config, request, cwd) {
        return match config.permission.external_directory {
            Action::Allow => evaluate_after_path(config, request),
            Action::Ask => {
                // Preserve single-path precedence, but never let an external-path prompt
                // mask a denial on another resource in a batch.
                let paths = candidate_paths(request);
                if paths.len() > 1 {
                    let path_decision = evaluate_after_path(config, request);
                    if path_decision.response.decision == AgentDecision::Deny {
                        return path_decision;
                    }
                }
                evaluation(
                    AgentDecision::Ask,
                    format!(
                        "{} agent asks before external directory access: {}",
                        request.agent_id, path
                    ),
                    None,
                    None,
                )
            }
            Action::Deny => evaluation(
                AgentDecision::Deny,
                format!(
                    "{} agent blocks external directory access: {}",
                    request.agent_id, path
                ),
                None,
                None,
            ),
        };
    }

    evaluate_after_path(config, request)
}

fn evaluate_after_path(
    config: &AgentConfig,
    request: &EvaluateToolCallRequest,
) -> PolicyEvaluation {
    match &request.operation {
        ToolPolicyOperation::Command { .. } => evaluate_shell(config, request),
        ToolPolicyOperation::Web { .. } => evaluate_web_url(config, request),
        ToolPolicyOperation::Write { category, .. } => {
            evaluate_filesystem_path(config, request, write_path_rules(config, category))
        }
        ToolPolicyOperation::Read { .. } => {
            evaluate_filesystem_path(config, request, &config.permission.read)
        }
        ToolPolicyOperation::ReadOnly => {
            evaluation(AgentDecision::Allow, String::new(), None, None)
        }
        ToolPolicyOperation::Mutating => evaluate_mutating_fallback(config, request),
    }
}

fn write_path_rules<'a>(config: &'a AgentConfig, category: &str) -> &'a BTreeMap<String, Action> {
    match category {
        "edit" => &config.permission.edit,
        _ => &config.permission.write,
    }
}

fn evaluate_web_url(config: &AgentConfig, request: &EvaluateToolCallRequest) -> PolicyEvaluation {
    let url = url_argument(request).unwrap_or("*");
    let rules = compile_path_rules(&config.permission.web);
    if let Some(rule) = matching_path_rule(&rules, url) {
        return match rule.action {
            Action::Allow => evaluation(
                AgentDecision::Allow,
                String::new(),
                Some(rule.pattern.clone()),
                None,
            ),
            Action::Ask => evaluation(
                AgentDecision::Ask,
                format!("{} agent asks before web URL: {}", request.agent_id, url),
                Some(rule.pattern.clone()),
                None,
            ),
            Action::Deny => evaluation(
                AgentDecision::Deny,
                format!(
                    "{} agent denied web URL '{}' by rule '{}'",
                    request.agent_id, url, rule.pattern
                ),
                Some(rule.pattern.clone()),
                None,
            ),
        };
    }
    if tool_enabled(config, request) == Some(true) {
        evaluation(
            AgentDecision::Ask,
            format!("{} agent asks before web URL", request.agent_id),
            None,
            None,
        )
    } else {
        evaluation(
            AgentDecision::Deny,
            format!(
                "{} agent denied web URL access; enable the tool if web page reads are allowed",
                request.agent_id
            ),
            None,
            None,
        )
    }
}

fn evaluate_mutating_fallback(
    config: &AgentConfig,
    request: &EvaluateToolCallRequest,
) -> PolicyEvaluation {
    if tool_enabled(config, request) == Some(true) {
        if let Some(action) = config.permission.tools.get(&request.tool_name) {
            let decision = match action {
                Action::Allow => AgentDecision::Allow,
                Action::Ask => AgentDecision::Ask,
                Action::Deny => AgentDecision::Deny,
            };
            return evaluation(
                decision,
                format!(
                    "{} agent tool rule for {}: {action:?}",
                    request.agent_id, request.tool_name
                ),
                Some(request.tool_name.clone()),
                None,
            );
        }
        evaluation(
            AgentDecision::Ask,
            format!(
                "{} agent asks before {}",
                request.agent_id, request.tool_name
            ),
            None,
            None,
        )
    } else {
        evaluation(
            AgentDecision::Deny,
            format!(
                "{} agent denied mutating tool {}; switch agents if implementation is needed",
                request.agent_id, request.tool_name
            ),
            None,
            None,
        )
    }
}

fn evaluate_filesystem_path(
    config: &AgentConfig,
    request: &EvaluateToolCallRequest,
    rules: &BTreeMap<String, Action>,
) -> PolicyEvaluation {
    let candidates = candidate_paths(request);
    let compiled = compile_path_rules(rules);
    let evaluate_path = |path: Option<&str>| {
        let rule_match = path.and_then(|path| matching_path_rule(&compiled, path));

        if let Some(rule) = rule_match {
            let rule_pattern = Some(rule.pattern.clone());
            let subject = path.unwrap_or_default().to_owned();
            return match rule.action {
                Action::Allow => {
                    evaluation(AgentDecision::Allow, String::new(), rule_pattern, None)
                }
                Action::Ask => evaluation(
                    AgentDecision::Ask,
                    format!(
                        "{} agent asks before {} on {}",
                        request.agent_id, request.tool_name, subject
                    ),
                    rule_pattern,
                    Some(subject),
                ),
                Action::Deny => evaluation(
                    AgentDecision::Deny,
                    format!(
                        "{} agent denied {} on '{}' by rule '{}'",
                        request.agent_id, request.tool_name, subject, rule.pattern
                    ),
                    rule_pattern,
                    Some(subject),
                ),
            };
        }

        match request.operation {
            ToolPolicyOperation::Read { .. } => {
                evaluation(AgentDecision::Allow, String::new(), None, None)
            }
            ToolPolicyOperation::Write { .. } => evaluate_mutating_fallback(config, request),
            _ => evaluation(AgentDecision::Allow, String::new(), None, None),
        }
    };
    let mut paths = candidates.iter();
    let mut result = evaluate_path(paths.next().map(String::as_str));
    for path in paths {
        let candidate = evaluate_path(Some(path));
        if decision_priority(candidate.response.decision)
            > decision_priority(result.response.decision)
        {
            result = candidate;
        }
    }
    result
}

const fn decision_priority(decision: AgentDecision) -> u8 {
    match decision {
        AgentDecision::Allow => 0,
        AgentDecision::Ask => 1,
        AgentDecision::Deny => 2,
    }
}

fn url_argument(request: &EvaluateToolCallRequest) -> Option<&str> {
    match &request.operation {
        ToolPolicyOperation::Web { url } => url.as_deref(),
        _ => None,
    }
}

#[derive(Debug)]
struct ShellSubjectDecision {
    action: Action,
    rule: Rule,
    candidate: String,
    subject: String,
    span: Option<bcode_shell_command_analysis_models::ShellSourceSpan>,
    kind: ShellPolicySubjectKind,
    locator: Option<(usize, bool, usize)>,
}

// Defaults apply to the original subject only. Reapplying a catch-all after
// dropping a prefix would invalidate an explicitly authorized original command.
fn applies_to_subject(rule: &Rule, restriction_only: bool) -> bool {
    !restriction_only || !rule.pattern.chars().all(|ch| ch == '*')
}

fn evaluate_ordered_subject(
    layers: &[CompiledCommandLayer],
    subject: &shell_subject::Subject<'_>,
    restriction_only: bool,
    classified: &[word_patterns::ClassifiedWords<'_>],
) -> ShellSubjectDecision {
    let mut winner: Option<(Rule, (usize, bool, usize))> = None;
    // Environment assignments are retained as facts; all layers match the effective command.
    for (layer_index, layer) in layers.iter().enumerate() {
        let candidates: BTreeSet<_> = subject
            .candidates
            .iter()
            .flat_map(|candidate| {
                layer
                    .legacy_candidates(candidate)
                    .filter(move |(_, rule)| glob_matches(&rule.pattern, candidate))
                    .map(|(index, _)| index)
            })
            .collect();
        let legacy = candidates
            .into_iter()
            .map(|index| (index, &layer.legacy[index]))
            .filter(|(_, rule)| applies_to_subject(rule, restriction_only))
            .map(|(index, rule)| (rule.clone(), (layer_index, false, index)));
        let word = classified[layer_index]
            .matching()
            .into_iter()
            .chain(classified[layer_index].possible_restriction(Action::Deny))
            .chain(classified[layer_index].possible_restriction(Action::Ask))
            .map(|rule| {
                (
                    Rule {
                        pattern: rule.pattern.clone(),
                        action: rule.action,
                        specificity: usize::MAX,
                    },
                    (layer_index, true, rule.index),
                )
            });
        if let Some(rule) = legacy
            .chain(word)
            .chain(
                legacy_symbolic::restrictions(layer, subject)
                    .map(|(index, rule)| (rule.clone(), (layer_index, false, index))),
            )
            .max_by_key(|(rule, _)| {
                (
                    action_precedence(rule.action),
                    rule.specificity,
                    rule.pattern.len(),
                )
            })
        {
            winner = Some(rule);
        }
    }
    let locator = winner.as_ref().map(|(_, locator)| *locator);
    let mut rule = winner.map_or_else(
        || Rule {
            pattern: "<missing-rule>".to_owned(),
            action: if restriction_only {
                Action::Allow
            } else {
                Action::Deny
            },
            specificity: 0,
        },
        |(rule, _)| rule,
    );
    // A later exception cannot erase an earlier restriction whose applicability is unresolved.
    let mut locator = locator;
    for (index, layer) in layers.iter().enumerate() {
        for (rule_index, possible) in legacy_symbolic::restrictions(layer, subject) {
            if action_precedence(possible.action) > action_precedence(rule.action) {
                rule = possible.clone();
                locator = Some((index, false, rule_index));
            }
        }
        for action in [Action::Ask, Action::Deny] {
            if let Some(possible) = classified[index].possible_restriction(action)
                && action_precedence(action) > action_precedence(rule.action)
            {
                rule = Rule {
                    pattern: possible.pattern.clone(),
                    action,
                    specificity: usize::MAX,
                };
                locator = Some((index, true, possible.index));
            }
        }
    }
    let action = rule.action;
    ShellSubjectDecision {
        action,
        rule,
        candidate: subject.source.to_owned(),
        subject: subject.source.to_owned(),
        span: Some(subject.span),
        kind: ShellPolicySubjectKind::Command,
        locator,
    }
}

fn preview_command_matches(
    layers: &[CompiledCommandLayer],
    restriction_only: bool,
    subject: &shell_subject::Subject<'_>,
    subject_index: usize,
    locator: Option<(usize, bool, usize)>,
    classified: &[word_patterns::ClassifiedWords<'_>],
) -> Vec<bcode_agent_profile::PreviewCommandMatch> {
    let mut matches = Vec::new();
    for (layer_index, layer) in layers.iter().enumerate() {
        let candidates: BTreeSet<_> = subject
            .candidates
            .iter()
            .flat_map(|candidate| {
                layer
                    .legacy_candidates(candidate)
                    .filter(move |(_, rule)| glob_matches(&rule.pattern, candidate))
                    .map(|(index, _)| index)
            })
            .collect();
        for rule_index in candidates {
            let rule = &layer.legacy[rule_index];
            if !applies_to_subject(rule, restriction_only) {
                continue;
            }
            matches.push(bcode_agent_profile::PreviewCommandMatch {
                subject_index,
                layer_index,
                rule_index,
                rule_kind: "command".to_owned(),
                winning: locator == Some((layer_index, false, rule_index)),
                decision: agent_decision(rule.action),
                broad: rule.pattern.contains('*'),
                regex_flags: Vec::new(),
                uncertain: false,
            });
        }
        for (rule_index, rule) in legacy_symbolic::restrictions(layer, subject) {
            matches.push(bcode_agent_profile::PreviewCommandMatch {
                subject_index,
                layer_index,
                rule_index,
                rule_kind: "command".to_owned(),
                winning: locator == Some((layer_index, false, rule_index)),
                decision: agent_decision(rule.action),
                broad: rule.pattern.contains('*'),
                regex_flags: Vec::new(),
                uncertain: true,
            });
        }
        for &(rule_index, rule, kind) in &classified[layer_index].matches {
            let certain = kind == word_patterns::Match::Definite;
            let uncertain = kind == word_patterns::Match::Possible;
            if !certain && !uncertain {
                continue;
            }
            let (broad, regex_flags) = rule.preview_metadata();
            matches.push(bcode_agent_profile::PreviewCommandMatch {
                subject_index,
                layer_index,
                rule_index,
                rule_kind: "command_patterns".to_owned(),
                winning: locator == Some((layer_index, true, rule_index)),
                decision: agent_decision(rule.action),
                broad,
                regex_flags,
                uncertain,
            });
        }
    }
    matches
}

fn evaluate_base_subject(
    layer: &CompiledCommandLayer,
    subject: &shell_subject::Subject<'_>,
    restriction_only: bool,
    classified: &[word_patterns::ClassifiedWords<'_>],
) -> ShellSubjectDecision {
    let word_rules = &classified[0];
    let mut winning_locator = None;
    let mut winning: Option<(Action, Rule, &str)> = None;
    for candidate in &subject.candidates {
        let Some((index, rule)) = layer
            .legacy_candidates(candidate)
            .filter(|(_, rule)| applies_to_subject(rule, restriction_only))
            .filter(|(_, rule)| glob_matches(&rule.pattern, candidate))
            .max_by_key(|(_, rule)| (rule.specificity, rule.pattern.len()))
        else {
            continue;
        };
        let replace = winning.as_ref().is_none_or(|(_, current, _)| {
            (rule.specificity, rule.pattern.len()) > (current.specificity, current.pattern.len())
        });
        if replace {
            winning_locator = Some((0, false, index));
            winning = Some((rule.action, rule.clone(), candidate.as_ref()));
        }
    }
    let exact_legacy_deny = matches!(
        &winning,
        Some((Action::Deny, legacy_rule, _)) if !legacy_rule.pattern.contains('*')
    );
    let mut locator = winning_locator;
    let (mut action, mut rule, mut candidate) = winning.unwrap_or_else(|| {
        (
            if restriction_only {
                Action::Allow
            } else {
                Action::Deny
            },
            Rule {
                pattern: "<missing-rule>".to_owned(),
                action: if restriction_only {
                    Action::Allow
                } else {
                    Action::Deny
                },
                specificity: 0,
            },
            subject.source,
        )
    });
    if let Some(word_rule) = word_rules.matching() {
        // A structured rule can refine a legacy wildcard, but cannot
        // override an exact legacy denial.
        if !exact_legacy_deny {
            locator = Some((0, true, word_rule.index));
            action = word_rule.action;
            rule = Rule {
                pattern: word_rule.pattern.clone(),
                action,
                specificity: usize::MAX,
            };
            candidate = subject.source;
        }
    }
    if let Some(uncertain_deny) = word_rules.possible_restriction(Action::Deny) {
        locator = Some((0, true, uncertain_deny.index));
        action = Action::Deny;
        rule = Rule {
            pattern: uncertain_deny.pattern.clone(),
            action,
            specificity: usize::MAX,
        };
        candidate = subject.source;
    }
    if let Some(possible_ask) = word_rules.possible_restriction(Action::Ask)
        && action == Action::Allow
    {
        locator = Some((0, true, possible_ask.index));
        action = Action::Ask;
        rule = Rule {
            pattern: possible_ask.pattern.clone(),
            action,
            specificity: usize::MAX,
        };
        candidate = subject.source;
    }
    for (index, possible) in legacy_symbolic::restrictions(layer, subject) {
        if action_precedence(possible.action) > action_precedence(action) {
            locator = Some((0, false, index));
            action = possible.action;
            rule = possible.clone();
            candidate = subject.source;
        }
    }
    ShellSubjectDecision {
        action,
        rule,
        candidate: candidate.to_owned(),
        subject: subject.source.to_owned(),
        span: Some(subject.span),
        kind: ShellPolicySubjectKind::Command,
        locator,
    }
}

fn evaluate_subject(
    compiled: &compiled_commands::CompiledCommandPolicy,
    subject: &shell_subject::Subject<'_>,
    restriction_only: bool,
    subject_index: usize,
) -> (
    ShellSubjectDecision,
    Vec<bcode_agent_profile::PreviewCommandMatch>,
) {
    let classified = compiled
        .layers
        .iter()
        .map(|layer| layer.words.classified(subject))
        .collect::<Vec<_>>();
    let result = if compiled.ordered {
        evaluate_ordered_subject(&compiled.layers, subject, restriction_only, &classified)
    } else {
        evaluate_base_subject(&compiled.layers[0], subject, restriction_only, &classified)
    };
    let matches = preview_command_matches(
        &compiled.layers,
        restriction_only,
        subject,
        subject_index,
        result.locator,
        &classified,
    );
    (result, matches)
}

#[allow(clippy::too_many_lines)]
fn evaluate_shell(config: &AgentConfig, request: &EvaluateToolCallRequest) -> PolicyEvaluation {
    let ToolPolicyOperation::Command {
        command,
        analysis,
        analysis_error,
    } = &request.operation
    else {
        return shell_fact_denied(request, "shell policy operation is not a command");
    };
    let Some(command) = command.as_deref() else {
        return shell_fact_denied(request, "shell command is missing");
    };
    if analysis_error.is_some() {
        return shell_fact_denied(request, "shell analysis failed");
    }
    let Some(analysis) = analysis.as_ref() else {
        return shell_fact_denied(request, "structured shell analysis is missing");
    };
    if analysis.schema_version != bcode_shell_command_analysis_models::SHELL_ANALYSIS_SCHEMA_VERSION
        || analysis.source != command
        || analysis.dialect != bcode_shell_command_analysis_models::ShellDialect::Posix
        || !analysis.completeness.is_complete()
    {
        return shell_fact_denied(
            request,
            "structured shell analysis is invalid or incomplete",
        );
    }
    if analysis.commands.is_empty() {
        return shell_fact_denied(request, "shell analysis contains no executable command");
    }

    let compiled = match compiled_commands::cached(config) {
        Ok(compiled) => compiled,
        Err(error) => {
            return shell_fact_denied(request, &format!("invalid command policy: {error}"));
        }
    };
    let mut results = Vec::new();
    let mut matches = Vec::new();
    for (subject_index, original) in analysis.commands.iter().enumerate() {
        let subject = match environment::effective_command(original) {
            Ok(subject) => subject,
            Err(reason) => return shell_fact_denied(request, reason),
        };
        let subject = &subject;
        if !matches!(
            *subject.executable,
            bcode_shell_command_analysis_models::ShellWord::Static { .. }
        ) {
            return shell_fact_denied(request, "effective executable identity is unresolved");
        }
        if subject.candidates.is_empty() {
            return shell_fact_denied(request, "shell command has no policy match candidate");
        }
        let (mut result, mut subject_matches) =
            evaluate_subject(&compiled, subject, false, subject_index);
        if let Err(error) = execution_views::visit(subject, |view| {
            let (mut restriction, view_matches) =
                evaluate_subject(&compiled, view, true, subject_index);
            let replaces = restriction.locator.is_some()
                && action_precedence(restriction.action) > action_precedence(result.action);
            if replaces {
                subject.source.clone_into(&mut restriction.subject);
                result = restriction;
                for matched in &mut subject_matches {
                    matched.winning = false;
                }
            }
            for mut matched in view_matches {
                matched.winning &= replaces;
                if matched.decision != AgentDecision::Allow && !subject_matches.contains(&matched) {
                    subject_matches.push(matched);
                }
            }
        }) {
            return shell_fact_denied(request, &error);
        }
        matches.extend(subject_matches);
        results.push(result);
    }

    for redirect in &analysis.redirections {
        let Some(path) = redirect.static_path.as_deref() else {
            if !matches!(
                redirect.kind,
                bcode_shell_command_analysis_models::ShellRedirectionKind::HereDocument
                    | bcode_shell_command_analysis_models::ShellRedirectionKind::Duplicate
                    | bcode_shell_command_analysis_models::ShellRedirectionKind::Close
            ) {
                return shell_fact_denied(request, "shell redirection target is dynamic");
            }
            continue;
        };
        let (rules, fallback) = match redirect.kind {
            bcode_shell_command_analysis_models::ShellRedirectionKind::Input => {
                (&config.permission.read, Action::Allow)
            }
            bcode_shell_command_analysis_models::ShellRedirectionKind::OutputTruncate
            | bcode_shell_command_analysis_models::ShellRedirectionKind::OutputAppend
            | bcode_shell_command_analysis_models::ShellRedirectionKind::InputOutput => {
                (&config.permission.write, Action::Deny)
            }
            bcode_shell_command_analysis_models::ShellRedirectionKind::Duplicate
            | bcode_shell_command_analysis_models::ShellRedirectionKind::Close
            | bcode_shell_command_analysis_models::ShellRedirectionKind::HereDocument => continue,
            bcode_shell_command_analysis_models::ShellRedirectionKind::HereString => {
                return shell_fact_denied(request, "unsupported shell redirection");
            }
            _ => return shell_fact_denied(request, "unknown shell redirection kind"),
        };
        let compiled = compile_path_rules(rules);
        let action = matching_path_rule(&compiled, path).map_or(fallback, |rule| rule.action);
        results.push(ShellSubjectDecision {
            action,
            rule: Rule {
                pattern: format!("redirection:{path}"),
                action,
                specificity: usize::MAX,
            },
            candidate: path.to_owned(),
            subject: path.to_owned(),
            span: Some(redirect.span),
            kind: ShellPolicySubjectKind::Redirection,
            locator: None,
        });
    }

    let winner = results
        .into_iter()
        .max_by_key(|result| action_precedence(result.action))
        .expect("shell analysis contains at least one command result");
    let decision = agent_decision(winner.action);
    let winner_index = analysis
        .commands
        .iter()
        .position(|command| Some(command.span) == winner.span && command.source == winner.subject);
    let possible = matches.iter().any(|matched| {
        Some(matched.subject_index) == winner_index && matched.winning && matched.uncertain
    });
    let certainty = if possible { "possible" } else { "definite" };
    let aggregate_reason = format!(
        "{} shell program: {} '{}' at bytes {}..{} using candidate '{}' matched rule '{}' ({:?}); configured action {:?}, {} match; aggregate precedence deny > ask > allow selected {:?}",
        match decision {
            AgentDecision::Allow => "allowed",
            AgentDecision::Ask => "asks before",
            AgentDecision::Deny => "denied",
        },
        match winner.kind {
            ShellPolicySubjectKind::Command => "command subject",
            ShellPolicySubjectKind::Redirection => "redirection",
        },
        winner.subject,
        winner.span.map_or(0, |span| span.start),
        winner.span.map_or(0, |span| span.end),
        winner.candidate,
        winner.rule.pattern,
        analysis.dialect,
        agent_decision(winner.rule.action),
        certainty,
        decision,
    );
    let diagnostic = ShellPolicyDiagnostic {
        original_source: analysis.source.clone(),
        subject: winner.subject.clone(),
        span: winner.span,
        dialect: analysis.dialect,
        match_candidate: winner.candidate,
        matched_rule: winner.rule.pattern.clone(),
        rule_action: Some(agent_decision(winner.rule.action)),
        match_certainty: Some(certainty.to_owned()),
        subject_kind: winner.kind,
        subject_decision: decision,
        aggregate_decision: decision,
        remember_patterns: shell_remember_patterns(winner.kind, &analysis.commands, winner.span),
        aggregate_reason: aggregate_reason.clone(),
    };
    let mut result = shell_evaluation(
        decision,
        aggregate_reason,
        Some(winner.rule.pattern),
        Some(winner.subject),
        diagnostic,
    );
    result.matches = matches;
    result
}

fn shell_remember_patterns(
    kind: ShellPolicySubjectKind,
    commands: &[bcode_shell_command_analysis_models::ShellCommand],
    span: Option<bcode_shell_command_analysis_models::ShellSourceSpan>,
) -> Vec<String> {
    if kind != ShellPolicySubjectKind::Command {
        return Vec::new();
    }
    let Some(command) = span.and_then(|span| commands.iter().find(|command| command.span == span))
    else {
        return Vec::new();
    };
    let bcode_shell_command_analysis_models::ShellWord::Static { value, .. } = &command.executable
    else {
        return Vec::new();
    };
    if !command.assignments.is_empty() || value.is_empty() {
        return Vec::new();
    }
    vec![command.source.clone(), format!("{value} *")]
}

const fn agent_decision(action: Action) -> AgentDecision {
    match action {
        Action::Allow => AgentDecision::Allow,
        Action::Ask => AgentDecision::Ask,
        Action::Deny => AgentDecision::Deny,
    }
}

const fn action_precedence(action: Action) -> u8 {
    match action {
        Action::Allow => 0,
        Action::Ask => 1,
        Action::Deny => 2,
    }
}

fn shell_fact_denied(request: &EvaluateToolCallRequest, reason: &str) -> PolicyEvaluation {
    evaluation(
        AgentDecision::Deny,
        format!("{} agent denied shell command: {reason}", request.agent_id),
        None,
        None,
    )
}

const fn shell_evaluation(
    decision: AgentDecision,
    reason: String,
    matched_rule: Option<String>,
    command_part: Option<String>,
    shell: ShellPolicyDiagnostic,
) -> PolicyEvaluation {
    PolicyEvaluation {
        response: EvaluateToolCallResponse {
            decision,
            reason: Some(reason),
            shell: Some(shell),
        },
        matched_rule,
        command_part,
        matches: Vec::new(),
    }
}

fn evaluation(
    decision: AgentDecision,
    reason: String,
    matched_rule: Option<String>,
    command_part: Option<String>,
) -> PolicyEvaluation {
    PolicyEvaluation {
        response: EvaluateToolCallResponse {
            decision,
            reason: (!reason.is_empty()).then_some(reason),
            shell: None,
        },
        matched_rule,
        command_part,
        matches: Vec::new(),
    }
}

fn external_path(
    config: &AgentConfig,
    request: &EvaluateToolCallRequest,
    cwd: &Path,
) -> Option<String> {
    if config.permission.external_directory == Action::Allow {
        return None;
    }
    candidate_paths(request)
        .into_iter()
        .find(|path| is_external_path(path, cwd))
}

/// Return candidate path resources produced by the tool owner.
#[must_use]
pub fn candidate_paths(request: &EvaluateToolCallRequest) -> Vec<String> {
    match &request.operation {
        ToolPolicyOperation::Read { paths } | ToolPolicyOperation::Write { paths, .. } => {
            paths.clone()
        }
        _ => Vec::new(),
    }
}

/// Return true when a path resolves outside `cwd`.
#[must_use]
pub fn is_external_path(path: &str, cwd: &Path) -> bool {
    let resolved_cwd = absolutize(cwd, Path::new("."));
    let input = Path::new(path);
    let resolved_path = if input.is_absolute() {
        normalize_path(input)
    } else {
        absolutize(cwd, input)
    };
    resolved_path != resolved_cwd && !resolved_path.starts_with(&resolved_cwd)
}

fn absolutize(cwd: &Path, path: &Path) -> PathBuf {
    normalize_path(&cwd.join(path))
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn tool_enabled(config: &AgentConfig, request: &EvaluateToolCallRequest) -> Option<bool> {
    tool_aliases(request)
        .iter()
        .find_map(|name| config.tools.get(name).copied())
}

fn tool_aliases(request: &EvaluateToolCallRequest) -> Vec<String> {
    std::iter::once(request.tool_name.clone())
        .chain(request.aliases.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Compile command glob rules.
#[must_use]
pub fn compile_rules(config: &AgentConfig) -> Vec<Rule> {
    config
        .permission
        .command
        .iter()
        .map(|(pattern, action)| Rule {
            pattern: pattern.clone(),
            action: *action,
            specificity: rule_specificity(pattern),
        })
        .collect()
}

/// Return the most specific matching rule for a command.
#[must_use]
pub fn matching_rule(command: &str, rules: &[Rule]) -> Option<Rule> {
    rules
        .iter()
        .filter(|rule| glob_matches(&rule.pattern, command))
        .max_by_key(|rule| (rule.specificity, rule.pattern.len()))
        .cloned()
}

/// Match a Pi/OpenCode-style glob. Only `*` is special.
#[must_use]
pub fn glob_matches(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let mut remainder = value;
    let parts = pattern.split('*');
    let mut first = true;
    for part in parts {
        if part.is_empty() {
            continue;
        }
        if first && !pattern.starts_with('*') {
            let Some(next) = remainder.strip_prefix(part) else {
                return false;
            };
            remainder = next;
        } else if let Some(index) = remainder.find(part) {
            remainder = &remainder[index + part.len()..];
        } else {
            return false;
        }
        first = false;
    }
    pattern.ends_with('*') || remainder.is_empty()
}

/// Return Pi/OpenCode-style rule specificity.
#[must_use]
pub fn rule_specificity(pattern: &str) -> usize {
    let exact_bonus = if pattern.contains('*') { 0 } else { 1_000 };
    exact_bonus + pattern.chars().filter(|char| *char != '*').count()
}

/// Compiled filesystem path rule.
#[derive(Debug, Clone)]
pub struct PathRule {
    pub pattern: String,
    pub action: Action,
    pub specificity: usize,
    matcher: globset::GlobMatcher,
}

impl PartialEq for PathRule {
    fn eq(&self, other: &Self) -> bool {
        self.pattern == other.pattern
            && self.action == other.action
            && self.specificity == other.specificity
    }
}

impl Eq for PathRule {}

/// Compile a map of path glob patterns into matchable rules.
///
/// Invalid glob patterns are silently skipped so a single malformed entry
/// cannot disable the rest of an agent's policy.
#[must_use]
pub fn compile_path_rules(rules: &BTreeMap<String, Action>) -> Vec<PathRule> {
    let mut compiled: Vec<PathRule> = rules
        .iter()
        .filter_map(|(pattern, action)| {
            let glob = globset::GlobBuilder::new(pattern)
                .literal_separator(false)
                .build()
                .ok()?;
            Some(PathRule {
                pattern: pattern.clone(),
                action: *action,
                specificity: path_rule_specificity(pattern),
                matcher: glob.compile_matcher(),
            })
        })
        .collect();
    compiled.sort_by(|lhs, rhs| {
        rhs.specificity
            .cmp(&lhs.specificity)
            .then_with(|| rhs.pattern.len().cmp(&lhs.pattern.len()))
            .then_with(|| lhs.pattern.cmp(&rhs.pattern))
    });
    compiled
}

/// Return the highest-specificity compiled path rule matching `path`.
#[must_use]
pub fn matching_path_rule<'a>(rules: &'a [PathRule], path: &str) -> Option<&'a PathRule> {
    rules.iter().find(|rule| rule.matcher.is_match(path))
}

/// Return path-rule specificity.
///
/// Scores each pattern by the count of literal characters. Patterns without
/// glob metacharacters receive a large exact-match bonus so a literal path
/// always outranks a wildcard pattern of equivalent length.
#[must_use]
pub fn path_rule_specificity(pattern: &str) -> usize {
    let has_meta = pattern
        .chars()
        .any(|char| matches!(char, '*' | '?' | '[' | ']' | '{' | '}'));
    let literal_count = pattern
        .chars()
        .filter(|char| !matches!(char, '*' | '?' | '[' | ']' | '{' | '}'))
        .count();
    if has_meta {
        literal_count
    } else {
        1_000 + literal_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_agent_profile::{EvaluateToolCallRequest, ToolPolicyOperation};

    fn ordered_config(layers: Vec<CommandPolicyLayer>) -> AgentConfig {
        AgentConfig {
            permission: PermissionConfig {
                command: BTreeMap::from([("*".to_owned(), Action::Deny)]),
                command_policy: Some(CommandPolicy { version: 1, layers }),
                ..PermissionConfig::default()
            },
            ..AgentConfig::default()
        }
    }

    fn layer(command: &[(&str, Action)], words: &[(&str, Action)]) -> CommandPolicyLayer {
        CommandPolicyLayer {
            command: command.iter().map(|(p, a)| ((*p).to_owned(), *a)).collect(),
            command_patterns: words.iter().map(|(p, a)| ((*p).to_owned(), *a)).collect(),
        }
    }

    fn decision(config: &AgentConfig, source: &str) -> AgentDecision {
        let request = request("build", source);
        let enforced = evaluate_tool_call(config, &request, Path::new("/tmp"));
        assert_eq!(
            preview_tool_call(config, &request, Path::new("/tmp")).decision,
            enforced.response.decision
        );
        enforced.response.decision
    }

    #[test]
    fn ordered_last_matching_layer_and_exact_base_carveout() {
        let mut config = ordered_config(vec![
            layer(&[("git *", Action::Deny)], &[]),
            layer(&[], &[("git status", Action::Allow)]),
            layer(&[("cargo *", Action::Ask)], &[]),
        ]);
        config
            .permission
            .command
            .insert("git status".to_owned(), Action::Deny);
        assert_eq!(decision(&config, "git status"), AgentDecision::Allow);
        assert_eq!(decision(&config, "git diff"), AgentDecision::Deny);
        assert_eq!(decision(&config, "cargo test"), AgentDecision::Ask);
        config.permission.command_policy = None;
        config
            .permission
            .command_patterns
            .insert("git status".to_owned(), Action::Allow);
        assert_eq!(decision(&config, "git status"), AgentDecision::Deny);
    }

    #[test]
    fn ordered_same_layer_action_precedence_and_no_match_inheritance() {
        let config = ordered_config(vec![layer(
            &[("git *", Action::Deny), ("git status", Action::Allow)],
            &[("git status", Action::Ask)],
        )]);
        assert_eq!(decision(&config, "git status"), AgentDecision::Deny);
        let config = ordered_config(vec![layer(
            &[("git *", Action::Ask)],
            &[("git status", Action::Allow)],
        )]);
        assert_eq!(decision(&config, "git status"), AgentDecision::Ask);
        assert_eq!(decision(&config, "echo ok"), AgentDecision::Deny);
    }

    #[test]
    fn ordered_siblings_and_redirections_remain_aggregate_restrictive() {
        let mut config = ordered_config(vec![layer(&[], &[("git status", Action::Allow)])]);
        assert_eq!(
            decision(&config, "git status && git diff"),
            AgentDecision::Deny
        );
        assert_eq!(
            decision(&config, "git status > result"),
            AgentDecision::Deny
        );
        config.permission.write.insert("*".to_owned(), Action::Ask);
        assert_eq!(decision(&config, "git status > result"), AgentDecision::Ask);
        config
            .permission
            .write
            .insert("*".to_owned(), Action::Allow);
        assert_eq!(
            decision(&config, "git status > result"),
            AgentDecision::Allow
        );
    }

    #[test]
    fn explicit_grants_survive_restriction_views_but_not_command_restrictions() {
        for source in [
            "MODE=test git status",
            "env MODE=test git status",
            "command git status",
            "exec git status",
            "/usr/bin/git status",
        ] {
            for fallback in [Action::Ask, Action::Deny] {
                let mut base = AgentConfig::default();
                base.permission.command = BTreeMap::from([
                    ("*".to_owned(), fallback),
                    (source.to_owned(), Action::Allow),
                ]);
                let ordered = ordered_config(vec![
                    layer(&[("*", fallback)], &[]),
                    layer(&[(source, Action::Allow)], &[]),
                ]);
                for mut config in [base, ordered] {
                    assert_eq!(decision(&config, source), AgentDecision::Allow, "{source}");
                    if let Some(policy) = config.permission.command_policy.as_mut() {
                        policy
                            .layers
                            .push(layer(&[], &[("git status", Action::Deny)]));
                    } else {
                        config
                            .permission
                            .command_patterns
                            .insert("git status".to_owned(), Action::Deny);
                    }
                    assert_eq!(decision(&config, source), AgentDecision::Deny, "{source}");
                }
            }
        }
        let mut config = AgentConfig::default();
        config.permission.command = BTreeMap::from([
            ("*".to_owned(), Action::Deny),
            ("command git push".to_owned(), Action::Allow),
            ("git push".to_owned(), Action::Deny),
        ]);
        assert_eq!(decision(&config, "command git push"), AgentDecision::Deny);
    }

    #[test]
    fn every_launcher_view_rejects_unresolved_source_and_executables() {
        let configs = [
            AgentConfig {
                permission: PermissionConfig {
                    command: BTreeMap::from([("*".to_owned(), Action::Allow)]),
                    ..PermissionConfig::default()
                },
                ..AgentConfig::default()
            },
            ordered_config(vec![layer(&[("*", Action::Allow)], &[])]),
        ];
        for config in &configs {
            for prefix in [
                "",
                "command ",
                "command -p -- ",
                "command command ",
                "command env ",
                "env command ",
                "exec command ",
            ] {
                for executable in ["eval", ".", "source", "\"$tool\""] {
                    let source = format!("{prefix}{executable} 'printf ok'");
                    let result = evaluate_tool_call(
                        config,
                        &request(BUILD_AGENT, &source),
                        Path::new("/tmp"),
                    );
                    assert_eq!(result.response.decision, AgentDecision::Deny, "{source}");
                    assert_eq!(
                        preview_tool_call(
                            config,
                            &request(BUILD_AGENT, &source),
                            Path::new("/tmp")
                        )
                        .decision,
                        AgentDecision::Deny,
                        "{source}"
                    );
                }
            }
            for source in [
                "command -v eval",
                "command -V eval",
                "printf %s eval",
                "command printf %s eval",
            ] {
                assert_eq!(decision(config, source), AgentDecision::Allow, "{source}");
            }
        }
    }

    #[test]
    fn enforcement_and_diagnostics_share_each_symbolic_classification() {
        let patterns = [
            ("cmd ...", Action::Allow),
            ("cmd bad ...", Action::Deny),
            ("cmd push ...", Action::Ask),
        ];
        let base = AgentConfig {
            permission: PermissionConfig {
                command: BTreeMap::from([("*".to_owned(), Action::Allow)]),
                command_patterns: patterns
                    .iter()
                    .map(|(pattern, action)| ((*pattern).to_owned(), *action))
                    .collect(),
                ..PermissionConfig::default()
            },
            ..AgentConfig::default()
        };
        let ordered = ordered_config(vec![
            layer(&[("*", Action::Allow)], &patterns),
            layer(&[], &patterns),
        ]);
        for (config, expected_count) in [(&base, 3), (&ordered, 6)] {
            for source in ["cmd \"$arg\"", "command env cmd \"$arg\""] {
                word_patterns::CLASSIFICATIONS.with(|count| count.set(0));
                let result =
                    evaluate_tool_call(config, &request(BUILD_AGENT, source), Path::new("/tmp"));
                assert_eq!(result.response.decision, AgentDecision::Deny);
                word_patterns::CLASSIFICATIONS
                    .with(|count| assert_eq!(count.get(), expected_count, "{source}"));
                let winners = result
                    .matches
                    .iter()
                    .filter(|matched| matched.winning)
                    .collect::<Vec<_>>();
                assert_eq!(winners.len(), 1);
                assert_eq!(winners[0].decision, AgentDecision::Deny);
                assert!(winners[0].uncertain);
            }
        }
    }

    #[test]
    fn excessive_wrapper_work_fails_closed_before_matching_all_suffixes() {
        let config = AgentConfig {
            permission: PermissionConfig {
                command: BTreeMap::from([("*".to_owned(), Action::Allow)]),
                ..PermissionConfig::default()
            },
            ..AgentConfig::default()
        };
        assert_eq!(
            decision(&config, &format!("{}printf ok", "command ".repeat(100))),
            AgentDecision::Allow
        );
        let source = format!("{}printf ok", "command ".repeat(3200));
        let result = evaluate_tool_call(&config, &request(BUILD_AGENT, &source), Path::new("/tmp"));
        assert_eq!(result.response.decision, AgentDecision::Deny);
        assert!(result.response.reason.unwrap().contains("work allowance"));
        // Env-only chains are consumed in a single forward pass, not rejected by depth.
        assert_eq!(
            decision(&config, &format!("{}printf ok", "env ".repeat(3200))),
            AgentDecision::Allow
        );
    }

    #[test]
    fn env_launcher_options_are_consistent_inside_and_outside_wrappers() {
        for prefix in ["", "command ", "exec "] {
            for env in [
                "env -C /tmp",
                "env --chdir=/tmp",
                "env -i -u HOME MODE=test",
                "env -- MODE=test",
            ] {
                let mut config = AgentConfig::default();
                config
                    .permission
                    .command
                    .insert("*".to_owned(), Action::Allow);
                let source = format!("{prefix}{env} git status");
                assert_eq!(decision(&config, &source), AgentDecision::Allow, "{source}");
                config
                    .permission
                    .command_patterns
                    .insert("git status".to_owned(), Action::Deny);
                assert_eq!(decision(&config, &source), AgentDecision::Deny, "{source}");
            }
            for env in [
                "env -C",
                "env -C \"$DIR\"",
                "env -u",
                "env -S 'git status'",
                "env $ARGS",
            ] {
                let mut config = AgentConfig::default();
                config
                    .permission
                    .command
                    .insert("*".to_owned(), Action::Allow);
                let source = format!("{prefix}{env}");
                assert_eq!(decision(&config, &source), AgentDecision::Deny, "{source}");
            }
        }
    }

    #[test]
    fn execution_views_keep_symbolic_restrictions_and_specificity() {
        for source in ["command git \"$ACTION\"", "exec /usr/bin/git \"$ACTION\""] {
            for action in [Action::Ask, Action::Deny] {
                for words in [false, true] {
                    let mut config = AgentConfig::default();
                    config
                        .permission
                        .command
                        .insert("*".to_owned(), Action::Allow);
                    if words {
                        config
                            .permission
                            .command_patterns
                            .insert("git push ...".to_owned(), action);
                    } else {
                        config
                            .permission
                            .command
                            .insert("git push *".to_owned(), action);
                    }
                    let expected = agent_decision(action);
                    let result = evaluate_tool_call(
                        &config,
                        &request(BUILD_AGENT, source),
                        Path::new("/tmp"),
                    );
                    assert_eq!(result.response.decision, expected, "{source}");
                    assert_eq!(
                        result.response.shell.unwrap().match_certainty.as_deref(),
                        Some("possible")
                    );
                    assert_eq!(decision(&config, source), expected, "{source}");
                }
            }
        }
        let mut config = AgentConfig::default();
        config.permission.command = BTreeMap::from([
            ("*".to_owned(), Action::Deny),
            ("command git status".to_owned(), Action::Allow),
        ]);
        config
            .permission
            .command
            .insert("git *".to_owned(), Action::Deny);
        config
            .permission
            .command
            .insert("git status".to_owned(), Action::Allow);
        assert_eq!(
            decision(&config, "command git status"),
            AgentDecision::Allow
        );
    }

    #[test]
    fn configured_allows_do_not_prompt_but_execution_views_preserve_denials() {
        let mut config = AgentConfig::default();
        config
            .permission
            .command
            .insert("*".to_owned(), Action::Allow);
        config
            .permission
            .command_patterns
            .insert("git <options>* push ...".to_owned(), Action::Deny);
        for source in [
            "git push",
            "git -C /tmp push",
            "git -c key=value push",
            "git --git-dir=/tmp/repo push",
            "git --no-optional-locks push",
            "env git push",
            "env -i NAME=value /usr/bin/git -C /tmp push",
            "env -- NAME=value git push",
            "command -- git push",
            "command -p git push",
            "exec git push",
            "exec -a name git push",
            "/usr/bin/git push",
            "./git push",
            "NAME=value git push",
            "NAME=value /usr/bin/git push",
            "env -C /tmp command /usr/bin/git push",
            "git -C \"$HOME\" push",
            "git \"$OPERATION\"",
            "git status; env git push",
        ] {
            let result =
                evaluate_tool_call(&config, &request(BUILD_AGENT, source), Path::new("/tmp"));
            assert_eq!(result.response.decision, AgentDecision::Deny, "{source}");
        }
        for source in [
            "BCODE_CONFIG=/tmp/config bcode permission check --help",
            "NAME=value git status",
            "printf '%s' \"$HOME\"",
            "env NAME=value git status",
            "command -v git",
            "exec git status",
            "git status",
        ] {
            assert_eq!(decision(&config, source), AgentDecision::Allow, "{source}");
        }
        for source in [
            "env -S 'git push'",
            "env $ARGS",
            "command $EXEC",
            "exec -a $NAME git push",
        ] {
            assert_eq!(decision(&config, source), AgentDecision::Deny, "{source}");
        }
        let mut restrictive = AgentConfig::default();
        restrictive
            .permission
            .command
            .insert("*".to_owned(), Action::Deny);
        restrictive
            .permission
            .command_patterns
            .insert("git status".to_owned(), Action::Allow);
        for source in [
            "/usr/bin/git status",
            "command git status",
            "exec git status",
        ] {
            assert_eq!(
                decision(&restrictive, source),
                AgentDecision::Deny,
                "{source}"
            );
        }
        for source in ["env git status", "NAME=value git status"] {
            assert_eq!(
                decision(&restrictive, source),
                AgentDecision::Allow,
                "{source}"
            );
        }
        config.permission.command_policy = Some(CommandPolicy::default());
        assert_eq!(
            decision(&config, "env git -C /tmp push"),
            AgentDecision::Deny
        );
        assert_eq!(
            decision(&config, "NAME=value git status"),
            AgentDecision::Allow
        );
    }

    #[test]
    fn ordered_dynamic_commands_remain_conservative_and_assignments_are_transparent() {
        let config = ordered_config(vec![layer(&[("*", Action::Allow)], &[])]);
        for source in ["$EXEC status", "\"$EXEC\" status"] {
            assert_ne!(decision(&config, source), AgentDecision::Allow, "{source}");
        }
        for source in ["git $ARGS", "NAME=value git status"] {
            assert_eq!(decision(&config, source), AgentDecision::Allow, "{source}");
        }
        let config = ordered_config(vec![layer(
            &[("*", Action::Allow)],
            &[("git status", Action::Deny)],
        )]);
        assert_eq!(
            decision(&config, "NAME=value git status"),
            AgentDecision::Deny
        );
        let config = ordered_config(vec![
            layer(&[], &[("git status", Action::Deny)]),
            layer(&[("*", Action::Allow)], &[("git status", Action::Allow)]),
        ]);
        assert_eq!(
            decision(&config, "NAME=value git status"),
            AgentDecision::Allow
        );
        assert_eq!(decision(&config, "git status"), AgentDecision::Allow);
    }

    #[test]
    fn environment_prefixes_preserve_all_command_actions_and_preview() {
        for action in [Action::Allow, Action::Ask, Action::Deny] {
            let configs = [
                AgentConfig {
                    permission: PermissionConfig {
                        command: BTreeMap::from([
                            ("*".to_owned(), Action::Deny),
                            ("git status".to_owned(), action),
                        ]),
                        ..PermissionConfig::default()
                    },
                    ..AgentConfig::default()
                },
                ordered_config(vec![
                    layer(&[("*", Action::Deny)], &[]),
                    layer(&[], &[("git status", action)]),
                ]),
            ];
            for config in configs {
                for source in [
                    "git status",
                    "MODE=test git status",
                    "MODE=$VALUE git status",
                    "A=1 B=2 git 'status'",
                    "env MODE=test git status",
                    "env MODE=$VALUE git status",
                    "env 'MODE=test value' git status",
                    "/usr/bin/env -i MODE=test git status",
                    "env -u MODE -- git status",
                    "env --unset=MODE git status",
                    "env env MODE=test git status",
                ] {
                    let request = request(BUILD_AGENT, source);
                    let result = evaluate_tool_call(&config, &request, Path::new("/tmp"));
                    assert_eq!(result.response.decision, agent_decision(action), "{source}");
                    let preview = preview_tool_call(&config, &request, Path::new("/tmp"));
                    assert_eq!(preview.decision, result.response.decision, "{source}");
                    assert!(
                        preview
                            .matches
                            .iter()
                            .any(|rule| rule.winning && rule.decision == agent_decision(action)),
                        "{source}"
                    );
                }
            }
        }
    }

    #[test]
    fn explicit_environment_globs_and_nested_execution_remain_enforced() {
        let config = AgentConfig {
            permission: PermissionConfig {
                command: BTreeMap::from([
                    ("*".to_owned(), Action::Allow),
                    ("MODE=restricted *".to_owned(), Action::Deny),
                    ("env MODE=review *".to_owned(), Action::Ask),
                    ("dangerous".to_owned(), Action::Deny),
                ]),
                ..PermissionConfig::default()
            },
            ..AgentConfig::default()
        };
        assert_eq!(
            decision(&config, "MODE=restricted git status"),
            AgentDecision::Deny
        );
        assert_eq!(
            decision(&config, "env MODE=review git status"),
            AgentDecision::Ask
        );
        for source in [
            "MODE=$(dangerous) git status",
            "MODE=$(dangerous); git status",
            "env MODE=$(dangerous) git status",
            "MODE=`dangerous` git status",
            "env MODE=x dangerous",
            "MODE=x git status; dangerous",
            "env -S 'dangerous'",
            "env MODE=x eval dangerous",
            "env MODE=x source script",
        ] {
            assert_eq!(decision(&config, source), AgentDecision::Deny, "{source}");
        }
    }

    #[test]
    fn ordered_dynamic_restrictions_cannot_be_erased_by_later_exceptions() {
        for action in [Action::Deny, Action::Ask] {
            let config = ordered_config(vec![
                layer(&[("*", Action::Allow)], &[("git stash ...", action)]),
                layer(&[], &[("git ...", Action::Allow)]),
            ]);
            assert_eq!(decision(&config, "git \"$op\""), agent_decision(action));
            assert_eq!(
                decision(&config, "git diff \"$args\""),
                AgentDecision::Allow
            );
            // Known argv retains the existing last-matching-layer exception semantics.
            assert_eq!(decision(&config, "git stash show"), AgentDecision::Allow);
        }
        let config = ordered_config(vec![
            layer(&[("*", Action::Allow), ("git stash *", Action::Deny)], &[]),
            layer(&[("git *", Action::Allow)], &[]),
        ]);
        assert_eq!(decision(&config, "git $args"), AgentDecision::Deny);
    }

    #[test]
    fn symbolic_decision_diagnostics_distinguish_possible_restrictions() {
        let config = AgentConfig {
            permission: PermissionConfig {
                command: BTreeMap::from([("*".to_owned(), Action::Allow)]),
                command_patterns: BTreeMap::from([("git push ...".to_owned(), Action::Ask)]),
                ..PermissionConfig::default()
            },
            ..AgentConfig::default()
        };
        let result = evaluate_tool_call(
            &config,
            &request(BUILD_AGENT, "git \"$op\""),
            Path::new("/tmp"),
        );
        assert_eq!(result.response.decision, AgentDecision::Ask);
        assert!(
            result
                .matches
                .iter()
                .any(|rule| rule.winning && rule.uncertain && rule.decision == AgentDecision::Ask)
        );
        assert!(result.response.reason.unwrap().contains("possible match"));
    }

    #[test]
    fn ordered_invalid_policy_denies_even_when_nonmatching() {
        let mut config = ordered_config(vec![layer(&[("*", Action::Allow)], &[])]);
        config.permission.command_policy.as_mut().unwrap().version = 2;
        assert_eq!(decision(&config, "git status"), AgentDecision::Deny);
        config.permission.command_policy.as_mut().unwrap().version = 1;
        config
            .permission
            .command_policy
            .as_mut()
            .unwrap()
            .layers
            .push(layer(&[], &[("[", Action::Deny)]));
        assert_eq!(decision(&config, "git status"), AgentDecision::Deny);
    }

    #[test]
    fn large_base_policy_remains_enforced_after_cached_configuration_changes() {
        let mut config = AgentConfig::default();
        config
            .permission
            .command
            .insert("*".to_owned(), Action::Allow);
        config.permission.command_patterns = (0..2048)
            .map(|i| (format!("unused-{i} ..."), Action::Deny))
            .collect();
        assert_eq!(decision(&config, "git status"), AgentDecision::Allow);
        assert_eq!(decision(&config, "unused-2047"), AgentDecision::Deny);
        config
            .permission
            .command_patterns
            .insert("git status".to_owned(), Action::Deny);
        assert_eq!(decision(&config, "git status"), AgentDecision::Deny);
        config
            .permission
            .command_patterns
            .insert("cmd []".to_owned(), Action::Allow);
        let failed =
            evaluate_tool_call(&config, &request("build", "git status"), Path::new("/tmp"));
        assert_eq!(failed.response.decision, AgentDecision::Deny);
        assert!(
            failed
                .response
                .reason
                .as_deref()
                .unwrap()
                .contains("command policy layer 0")
        );
    }

    #[test]
    fn ordered_policy_accepts_large_rule_sets_and_many_layers() {
        let oversized = CommandPolicyLayer {
            command: (0..2048)
                .map(|i| (format!("unused-{i}"), Action::Allow))
                .collect(),
            command_patterns: (0..512)
                .map(|i| (format!("unused-{i} ..."), Action::Deny))
                .collect(),
        };
        let mut layers = vec![CommandPolicyLayer::default(); 80];
        layers.push(oversized);
        layers.push(layer(&[("*", Action::Allow)], &[]));
        let config = ordered_config(layers);
        assert_eq!(decision(&config, "git status"), AgentDecision::Allow);
        assert_eq!(decision(&config, "unused-511"), AgentDecision::Allow);
    }

    #[test]
    fn preview_matches_use_safe_locators_and_per_leaf_winners() {
        let config = ordered_config(vec![
            layer(&[("git *", Action::Deny)], &[]),
            layer(&[], &[("git status", Action::Allow)]),
        ]);
        let preview = preview_tool_call(
            &config,
            &request("build", "git status && git diff"),
            Path::new("/tmp"),
        );
        assert_eq!(preview.decision, AgentDecision::Deny);
        let winners: Vec<_> = preview.matches.iter().filter(|m| m.winning).collect();
        assert_eq!(winners.len(), 2);
        assert_eq!(
            (
                winners[0].subject_index,
                winners[0].layer_index,
                winners[0].rule_index
            ),
            (0, 2, 0)
        );
        assert_eq!(winners[0].rule_kind, "command_patterns");
        assert_eq!(winners[0].decision, AgentDecision::Allow);
        assert!(!winners[0].broad);
        assert_eq!((winners[1].subject_index, winners[1].layer_index), (1, 1));
        assert!(winners[1].broad);
        assert!(
            preview
                .matches
                .iter()
                .all(|m| m.rule_kind == "command" || m.rule_kind == "command_patterns")
        );
    }

    #[test]
    fn preview_matches_report_uncertainty_and_regex_flags_without_text() {
        let config = ordered_config(vec![layer(&[], &[("git /stash/i ...", Action::Deny)])]);
        let preview = preview_tool_call(
            &config,
            &request("build", "git $SECRET_ARGUMENT"),
            Path::new("/tmp"),
        );
        let matched = preview
            .matches
            .iter()
            .find(|m| m.rule_kind == "command_patterns")
            .unwrap();
        assert_eq!(matched.regex_flags, ["i"]);
        assert!(matched.uncertain);
        assert!(matched.winning);
        assert!(!format!("{preview:?}").contains("SECRET_ARGUMENT"));
        assert!(!format!("{preview:?}").contains("stash"));
    }

    #[test]
    fn preview_exact_legacy_deny_category_is_not_replaced_by_identical_word_rule() {
        let config = AgentConfig {
            permission: PermissionConfig {
                command: BTreeMap::from([("git status".to_owned(), Action::Deny)]),
                command_patterns: BTreeMap::from([("git status".to_owned(), Action::Deny)]),
                ..PermissionConfig::default()
            },
            ..AgentConfig::default()
        };
        let preview =
            preview_tool_call(&config, &request("build", "git status"), Path::new("/tmp"));
        let winners: Vec<_> = preview.matches.iter().filter(|m| m.winning).collect();
        assert_eq!(winners.len(), 1);
        assert_eq!(winners[0].rule_kind, "command");
        assert_eq!(winners[0].rule_index, 0);
    }

    #[test]
    fn preview_regex_metadata_handles_escaped_delimiters_and_quoted_literals() {
        for (pattern, source, flags, broad) in [
            (r"cmd /a\/b/im", "cmd a/b", vec!["im".to_owned()], true),
            (r"cmd /[/]/s", "cmd /", vec!["s".to_owned()], true),
            (r"cmd '/secret/i'", "cmd /secret/i", Vec::new(), false),
        ] {
            let config = ordered_config(vec![layer(&[], &[(pattern, Action::Allow)])]);
            let preview = preview_tool_call(&config, &request("build", source), Path::new("/tmp"));
            let winner = preview
                .matches
                .iter()
                .find(|m| m.winning)
                .expect("word rule matched");
            assert_eq!(winner.regex_flags, flags, "{pattern}");
            assert_eq!(winner.broad, broad, "{pattern}");
        }
    }

    fn request(agent_id: &str, command: &str) -> EvaluateToolCallRequest {
        EvaluateToolCallRequest {
            session_id: bcode_session_models::SessionId::new(),
            agent_id: agent_id.to_string(),
            tool_name: "shell.run".to_string(),
            operation: ToolPolicyOperation::Command {
                command: Some(command.to_string()),
                analysis: bcode_shell_command_analysis::analyze(
                    &bcode_shell_command_analysis_models::ShellAnalysisRequest::posix(command),
                )
                .ok(),
                analysis_error: None,
            },
            aliases: vec!["command".to_string()],
            requires_permission: true,
            policy_profile: None,
            cwd: Some("/tmp/project".to_string()),
            effective_config_toml: None,
        }
    }

    #[test]
    fn active_tools_return_exact_enabled_tool_ids() {
        let config = AgentConfig {
            accent: None,
            tools: BTreeMap::from([
                ("example.read".to_string(), true),
                ("example.write".to_string(), false),
            ]),
            permission: PermissionConfig::default(),
        };
        let tools = active_tools_for(&config);

        assert_eq!(tools, vec!["example.read".to_string()]);
    }

    #[test]
    fn shell_diagnostic_contains_complete_winning_context() {
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
        let source = "printf ok; rm generated";
        let result = evaluate_tool_call(
            &config,
            &request(BUILD_AGENT, source),
            Path::new("/tmp/project"),
        );
        let diagnostic = result.response.shell.expect("shell diagnostic");
        assert_eq!(diagnostic.original_source, source);
        assert_eq!(diagnostic.subject, "rm generated");
        assert_eq!(
            diagnostic
                .span
                .map(|span| &source[span.start as usize..span.end as usize]),
            Some("rm generated")
        );
        assert_eq!(
            diagnostic.dialect,
            bcode_shell_command_analysis_models::ShellDialect::Posix
        );
        assert_eq!(diagnostic.match_candidate, "rm generated");
        assert_eq!(diagnostic.matched_rule, "rm *");
        assert_eq!(diagnostic.subject_kind, ShellPolicySubjectKind::Command);
        assert_eq!(diagnostic.subject_decision, AgentDecision::Deny);
        assert_eq!(diagnostic.aggregate_decision, AgentDecision::Deny);
        assert_eq!(diagnostic.remember_patterns, vec!["rm generated", "rm *"]);
        assert!(diagnostic.aggregate_reason.contains("deny > ask > allow"));
        assert_eq!(result.command_part.as_deref(), Some("rm generated"));
    }

    #[test]
    fn remembered_patterns_require_static_unprefixed_executable_subjects() {
        let config = AgentConfig {
            accent: None,
            tools: BTreeMap::from([("shell.run".to_owned(), true)]),
            permission: PermissionConfig {
                command: BTreeMap::from([("*".to_owned(), Action::Ask)]),
                ..PermissionConfig::default()
            },
        };
        let static_result = evaluate_tool_call(
            &config,
            &request(BUILD_AGENT, "printf ok"),
            Path::new("/tmp/project"),
        );
        assert_eq!(
            static_result.response.shell.unwrap().remember_patterns,
            vec!["printf ok", "printf *"]
        );
        for source in ["GIT_PAGER=cat git show HEAD", "\"$cmd\" ok"] {
            let result = evaluate_tool_call(
                &config,
                &request(BUILD_AGENT, source),
                Path::new("/tmp/project"),
            );
            assert!(
                result
                    .response
                    .shell
                    .is_none_or(|diagnostic| diagnostic.remember_patterns.is_empty()),
                "unsafe remembered pattern for {source}"
            );
        }
    }

    #[test]
    fn canonical_words_do_not_drop_options_or_override_more_specific_original_rules() {
        let config = AgentConfig {
            accent: None,
            tools: BTreeMap::from([("shell.run".to_owned(), true)]),
            permission: PermissionConfig {
                command: BTreeMap::from([
                    ("*".to_owned(), Action::Allow),
                    ("git diff *".to_owned(), Action::Deny),
                ]),
                ..PermissionConfig::default()
            },
        };
        let denied = evaluate_tool_call(
            &config,
            &request(BUILD_AGENT, "git 'diff' --stat"),
            Path::new("/tmp/project"),
        );
        assert_eq!(denied.response.decision, AgentDecision::Deny);
        assert_eq!(denied.matched_rule.as_deref(), Some("git diff *"));

        let allowed = evaluate_tool_call(
            &config,
            &request(BUILD_AGENT, "git --no-pager diff --stat"),
            Path::new("/tmp/project"),
        );
        assert_eq!(allowed.response.decision, AgentDecision::Allow);

        let config = AgentConfig {
            permission: PermissionConfig {
                command: BTreeMap::from([
                    ("*".to_owned(), Action::Allow),
                    ("git 'diff' --stat".to_owned(), Action::Deny),
                    ("git diff *".to_owned(), Action::Allow),
                ]),
                ..PermissionConfig::default()
            },
            ..config
        };
        let denied = evaluate_tool_call(
            &config,
            &request(BUILD_AGENT, "git 'diff' --stat"),
            Path::new("/tmp/project"),
        );
        assert_eq!(denied.response.decision, AgentDecision::Deny);
        assert_eq!(denied.matched_rule.as_deref(), Some("git 'diff' --stat"));
    }

    #[test]
    fn configured_denials_match_quoted_static_arguments_for_any_command() {
        let config = AgentConfig {
            accent: None,
            tools: BTreeMap::new(),
            permission: PermissionConfig {
                command: BTreeMap::from([
                    ("*".to_owned(), Action::Allow),
                    ("git stash".to_owned(), Action::Deny),
                    ("git stash pop".to_owned(), Action::Deny),
                    ("docker run *".to_owned(), Action::Deny),
                ]),
                ..PermissionConfig::default()
            },
        };
        for source in [
            "git stash",
            "git 'stash'",
            "git st\"ash\"",
            "git 'stash' pop",
            "docker 'run' image",
        ] {
            let result = evaluate_tool_call(
                &config,
                &request(BUILD_AGENT, source),
                Path::new("/tmp/project"),
            );
            assert_eq!(result.response.decision, AgentDecision::Deny, "{source}");
        }
        // An exact rule does not cover unlisted subcommands under glob semantics.
        let result = evaluate_tool_call(
            &config,
            &request(BUILD_AGENT, "git stash push"),
            Path::new("/tmp/project"),
        );
        assert_eq!(result.response.decision, AgentDecision::Allow);
    }

    #[test]
    fn word_patterns_match_static_words_with_explicit_precedence() {
        let config = AgentConfig {
            accent: None,
            tools: BTreeMap::new(),
            permission: PermissionConfig {
                command: BTreeMap::from([("*".to_owned(), Action::Allow)]),
                command_patterns: BTreeMap::from([
                    ("git stash ...".to_owned(), Action::Deny),
                    ("git [--no-pager] diff ...".to_owned(), Action::Allow),
                    ("git [--no-pager] stash ...".to_owned(), Action::Deny),
                ]),
                ..PermissionConfig::default()
            },
        };
        for source in [
            "git stash",
            "git stash push",
            "git 'stash' pop",
            "git --no-pager stash",
        ] {
            assert_eq!(
                evaluate_tool_call(
                    &config,
                    &request(BUILD_AGENT, source),
                    Path::new("/tmp/project")
                )
                .response
                .decision,
                AgentDecision::Deny,
                "{source}"
            );
        }
        for source in [
            "git diff",
            "git --no-pager diff --stat",
            "git 'diff' --stat",
        ] {
            assert_eq!(
                evaluate_tool_call(
                    &config,
                    &request(BUILD_AGENT, source),
                    Path::new("/tmp/project")
                )
                .response
                .decision,
                AgentDecision::Allow,
                "{source}"
            );
        }
        let config = AgentConfig {
            permission: PermissionConfig {
                command: BTreeMap::from([("*".to_owned(), Action::Deny)]),
                command_patterns: BTreeMap::from([(
                    "git [--no-pager] diff ...".to_owned(),
                    Action::Allow,
                )]),
                ..PermissionConfig::default()
            },
            ..config
        };
        assert_eq!(
            evaluate_tool_call(
                &config,
                &request(BUILD_AGENT, "git --no-pager diff --stat"),
                Path::new("/tmp/project")
            )
            .response
            .decision,
            AgentDecision::Allow
        );
        assert_eq!(
            evaluate_tool_call(
                &config,
                &request(BUILD_AGENT, "git -C elsewhere diff"),
                Path::new("/tmp/project")
            )
            .response
            .decision,
            AgentDecision::Deny
        );
    }

    #[test]
    fn exact_legacy_denial_cannot_be_overridden_by_word_allow() {
        let legacy_rule = "git --no-pager diff --stat";
        let config = AgentConfig {
            accent: None,
            tools: BTreeMap::new(),
            permission: PermissionConfig {
                command: BTreeMap::from([(legacy_rule.to_owned(), Action::Deny)]),
                command_patterns: BTreeMap::from([(
                    "git [--no-pager] diff ...".to_owned(),
                    Action::Allow,
                )]),
                ..PermissionConfig::default()
            },
        };
        assert_eq!(
            evaluate_tool_call(
                &config,
                &request(BUILD_AGENT, "git --no-pager diff --stat"),
                Path::new("/tmp/project")
            )
            .response
            .decision,
            AgentDecision::Deny
        );
    }

    #[test]
    fn word_patterns_cover_dynamic_arguments_with_rest() {
        let config = AgentConfig {
            accent: None,
            tools: BTreeMap::new(),
            permission: PermissionConfig {
                command: BTreeMap::from([("*".to_owned(), Action::Deny)]),
                command_patterns: BTreeMap::from([("printf ...".to_owned(), Action::Allow)]),
                ..PermissionConfig::default()
            },
        };
        for source in [
            "printf \"$VALUE\"",
            "printf $VALUE",
            "X=1 printf \"$VALUE\"",
        ] {
            assert_eq!(
                evaluate_tool_call(
                    &config,
                    &request(BUILD_AGENT, source),
                    Path::new("/tmp/project")
                )
                .response
                .decision,
                AgentDecision::Allow,
                "{source}"
            );
        }
    }

    #[test]
    fn word_pattern_optional_words_do_not_require_greedy_matching() {
        let config = AgentConfig {
            accent: None,
            tools: BTreeMap::new(),
            permission: PermissionConfig {
                command: BTreeMap::from([("*".to_owned(), Action::Deny)]),
                command_patterns: BTreeMap::from([("cmd [x] x".to_owned(), Action::Allow)]),
                ..PermissionConfig::default()
            },
        };
        for source in ["cmd x", "cmd x x"] {
            assert_eq!(
                evaluate_tool_call(
                    &config,
                    &request(BUILD_AGENT, source),
                    Path::new("/tmp/project")
                )
                .response
                .decision,
                AgentDecision::Allow,
                "{source}"
            );
        }
        assert_eq!(
            evaluate_tool_call(
                &config,
                &request(BUILD_AGENT, "cmd x x x"),
                Path::new("/tmp/project")
            )
            .response
            .decision,
            AgentDecision::Deny
        );
    }

    #[test]
    fn word_pattern_configuration_decodes_from_toml() {
        let config: AgentPermissionConfig = toml::from_str(
            "[agent.build.permission.command]\n\"*\" = \"allow\"\n[agent.build.permission.command_patterns]\n\"git stash ...\" = \"deny\"\n",
        )
        .expect("word patterns decode alongside legacy globs");
        let build = agent_config(&config, BUILD_AGENT);
        assert_eq!(build.permission.command.get("*"), Some(&Action::Allow));
        assert_eq!(
            build.permission.command_patterns.get("git stash ..."),
            Some(&Action::Deny)
        );
    }

    #[test]
    fn potential_dynamic_blacklist_match_denies_under_allow_fallback() {
        let config = AgentConfig {
            accent: None,
            tools: BTreeMap::new(),
            permission: PermissionConfig {
                command: BTreeMap::from([("*".to_owned(), Action::Allow)]),
                command_patterns: BTreeMap::from([("git stash ...".to_owned(), Action::Deny)]),
                ..PermissionConfig::default()
            },
        };
        for source in [
            "git \"$operation\"",
            "git stash \"$options\"",
            "git diff \"$options\"",
        ] {
            let result = evaluate_tool_call(
                &config,
                &request(BUILD_AGENT, source),
                Path::new("/tmp/project"),
            );
            assert_eq!(
                result.response.decision,
                if source.starts_with("git diff") {
                    AgentDecision::Allow
                } else {
                    AgentDecision::Deny
                },
                "{source}"
            );
        }
    }

    #[test]
    fn overlapping_word_denies_win_even_when_allow_is_more_specific() {
        let config = AgentConfig {
            accent: None,
            tools: BTreeMap::new(),
            permission: PermissionConfig {
                command: BTreeMap::from([("*".to_owned(), Action::Allow)]),
                command_patterns: BTreeMap::from([
                    ("cmd dangerous ...".to_owned(), Action::Deny),
                    ("cmd dangerous reviewed ...".to_owned(), Action::Allow),
                ]),
                ..PermissionConfig::default()
            },
        };
        let result = evaluate_tool_call(
            &config,
            &request(BUILD_AGENT, "cmd dangerous reviewed"),
            Path::new("/tmp/project"),
        );
        assert_eq!(result.response.decision, AgentDecision::Deny);
        assert_eq!(result.matched_rule.as_deref(), Some("cmd dangerous ..."));
    }

    #[test]
    fn malformed_word_patterns_fail_closed() {
        for pattern in ["[git] diff", "git ... diff", "git [oops", "git *", "git []"] {
            let config = AgentConfig {
                accent: None,
                tools: BTreeMap::new(),
                permission: PermissionConfig {
                    command: BTreeMap::from([("*".to_owned(), Action::Allow)]),
                    command_patterns: BTreeMap::from([(pattern.to_owned(), Action::Allow)]),
                    ..PermissionConfig::default()
                },
            };
            assert_eq!(
                evaluate_tool_call(
                    &config,
                    &request(BUILD_AGENT, "git diff"),
                    Path::new("/tmp/project")
                )
                .response
                .decision,
                AgentDecision::Deny,
                "{pattern}"
            );
        }
    }

    #[test]
    fn shell_analysis_mismatch_and_incompleteness_fail_closed() {
        let config = agent_config(&default_config(), BUILD_AGENT);
        let mut mismatched = request(BUILD_AGENT, "printf ok");
        let ToolPolicyOperation::Command { analysis, .. } = &mut mismatched.operation else {
            unreachable!();
        };
        analysis.as_mut().unwrap().source = "printf tampered".to_owned();
        assert_eq!(
            evaluate_tool_call(&config, &mismatched, Path::new("/tmp/project"))
                .response
                .decision,
            AgentDecision::Deny
        );

        let dynamic = request(BUILD_AGENT, "cmd=printf; \"$cmd\" ok");
        assert_eq!(
            evaluate_tool_call(&config, &dynamic, Path::new("/tmp/project"))
                .response
                .decision,
            AgentDecision::Deny
        );
    }

    #[test]
    fn specificity_prefers_exact_and_longer_patterns() {
        let rules = vec![
            Rule {
                pattern: "git *".to_string(),
                action: Action::Deny,
                specificity: rule_specificity("git *"),
            },
            Rule {
                pattern: "git status".to_string(),
                action: Action::Allow,
                specificity: rule_specificity("git status"),
            },
            Rule {
                pattern: "git status *".to_string(),
                action: Action::Deny,
                specificity: rule_specificity("git status *"),
            },
        ];

        assert_eq!(
            matching_rule("git status", &rules).map(|rule| rule.action),
            Some(Action::Allow)
        );
        assert_eq!(
            matching_rule("git status --short", &rules).map(|rule| rule.action),
            Some(Action::Deny)
        );
    }

    #[test]
    fn default_plan_allows_validation_commands() {
        let config = default_config();
        let plan = agent_config(&config, PLAN_AGENT);

        for command in ["cargo check", "cargo test", "cargo test --workspace"] {
            let result = evaluate_tool_call(
                &plan,
                &request(PLAN_AGENT, command),
                Path::new("/tmp/project"),
            );
            assert_eq!(
                result.response.decision,
                AgentDecision::Allow,
                "{command} should be allowed"
            );
        }
    }

    #[test]
    fn plan_denies_mutable_git_command_in_chain() {
        let config = default_config();
        let plan = agent_config(&config, PLAN_AGENT);
        let result = evaluate_tool_call(
            &plan,
            &request(PLAN_AGENT, "git diff && git commit -m nope"),
            Path::new("/tmp/project"),
        );

        assert_eq!(result.response.decision, AgentDecision::Deny);
        assert_eq!(result.command_part.as_deref(), Some("git commit -m nope"));
    }

    #[test]
    fn plan_denies_allowed_validation_chain_with_mutating_part() {
        let config = default_config();
        let plan = agent_config(&config, PLAN_AGENT);
        let result = evaluate_tool_call(
            &plan,
            &request(PLAN_AGENT, "cargo test && touch generated.txt"),
            Path::new("/tmp/project"),
        );

        assert_eq!(result.response.decision, AgentDecision::Deny);
        assert_eq!(result.command_part.as_deref(), Some("touch generated.txt"));
    }

    #[test]
    fn plan_allows_explicit_shell_rule_even_when_command_may_write() {
        let config = AgentPermissionConfig {
            agent: BTreeMap::from([(
                PLAN_AGENT.to_string(),
                AgentConfig {
                    accent: None,
                    tools: BTreeMap::from([
                        ("shell.run".to_string(), true),
                        ("filesystem.write".to_string(), false),
                        ("filesystem.edit".to_string(), false),
                    ]),
                    permission: PermissionConfig {
                        command: BTreeMap::from([
                            ("*".to_string(), Action::Deny),
                            ("echo *".to_string(), Action::Allow),
                        ]),
                        external_directory: Action::Allow,
                        ..PermissionConfig::default()
                    },
                },
            )]),
        };
        let plan_config = agent_config(&config, PLAN_AGENT);

        let redirected = evaluate_tool_call(
            &plan_config,
            &request(PLAN_AGENT, "echo \"hello\" > test.txt"),
            Path::new("/tmp/project"),
        );
        let plain_echo = evaluate_tool_call(
            &plan_config,
            &request(PLAN_AGENT, "echo hello"),
            Path::new("/tmp/project"),
        );

        assert_eq!(redirected.response.decision, AgentDecision::Deny);
        assert_eq!(plain_echo.response.decision, AgentDecision::Allow);
    }

    #[test]
    fn plan_denies_mutating_shell_command_without_explicit_allow() {
        let config = default_config();
        let plan = agent_config(&config, PLAN_AGENT);

        let denied = evaluate_tool_call(
            &plan,
            &request(PLAN_AGENT, "touch test.txt"),
            Path::new("/tmp/project"),
        );

        assert_eq!(denied.response.decision, AgentDecision::Deny);
        assert_eq!(denied.command_part.as_deref(), Some("touch test.txt"));
    }

    #[test]
    fn build_allows_or_denies_by_specific_rules() {
        let config = AgentPermissionConfig {
            agent: BTreeMap::from([(
                BUILD_AGENT.to_string(),
                AgentConfig {
                    accent: None,
                    tools: BTreeMap::from([("shell.run".to_string(), true)]),
                    permission: PermissionConfig {
                        command: BTreeMap::from([
                            ("*".to_string(), Action::Allow),
                            ("git commit *".to_string(), Action::Deny),
                        ]),
                        external_directory: Action::Allow,
                        ..PermissionConfig::default()
                    },
                },
            )]),
        };
        let build = agent_config(&config, BUILD_AGENT);

        assert_eq!(
            evaluate_tool_call(
                &build,
                &request(BUILD_AGENT, "cargo check"),
                Path::new("/tmp/project")
            )
            .response
            .decision,
            AgentDecision::Allow
        );
        assert_eq!(
            evaluate_tool_call(
                &build,
                &request(BUILD_AGENT, "git commit -m nope"),
                Path::new("/tmp/project")
            )
            .response
            .decision,
            AgentDecision::Deny
        );
    }

    #[test]
    fn external_directory_policy_blocks_outside_paths() {
        let config = AgentConfig {
            accent: None,
            tools: BTreeMap::from([("write".to_string(), true)]),
            permission: PermissionConfig {
                command: BTreeMap::new(),
                external_directory: Action::Deny,
                ..PermissionConfig::default()
            },
        };
        let request = EvaluateToolCallRequest {
            session_id: bcode_session_models::SessionId::new(),
            agent_id: BUILD_AGENT.to_string(),
            tool_name: "filesystem.write".to_string(),
            operation: ToolPolicyOperation::Write {
                paths: vec!["../outside.txt".to_string()],
                category: "write".to_string(),
            },
            aliases: vec!["write".to_string()],
            requires_permission: true,
            policy_profile: None,
            cwd: Some("/tmp/project".to_string()),
            effective_config_toml: None,
        };

        let result = evaluate_tool_call(&config, &request, Path::new("/tmp/project"));

        assert_eq!(result.response.decision, AgentDecision::Deny);
    }

    fn path_request(tool_name: &str, path: &str) -> EvaluateToolCallRequest {
        EvaluateToolCallRequest {
            session_id: bcode_session_models::SessionId::new(),
            agent_id: BUILD_AGENT.to_string(),
            tool_name: tool_name.to_string(),
            operation: match tool_name {
                "filesystem.write" => ToolPolicyOperation::Write {
                    paths: vec![path.to_string()],
                    category: "write".to_string(),
                },
                "filesystem.edit" => ToolPolicyOperation::Write {
                    paths: vec![path.to_string()],
                    category: "edit".to_string(),
                },
                _ => ToolPolicyOperation::Read {
                    paths: vec![path.to_string()],
                },
            },
            aliases: vec![
                tool_name
                    .split('.')
                    .next_back()
                    .unwrap_or(tool_name)
                    .to_string(),
            ],
            requires_permission: tool_name != "filesystem.read",
            policy_profile: None,
            cwd: Some("/tmp/project".to_string()),
            effective_config_toml: None,
        }
    }

    fn build_with_permission(permission: PermissionConfig) -> AgentConfig {
        AgentConfig {
            accent: None,
            tools: BTreeMap::from([
                ("filesystem.read".to_string(), true),
                ("filesystem.write".to_string(), true),
                ("filesystem.edit".to_string(), true),
            ]),
            permission,
        }
    }

    #[test]
    fn filesystem_write_allow_glob_skips_ask() {
        let config = build_with_permission(PermissionConfig {
            write: BTreeMap::from([("target/**".to_string(), Action::Allow)]),
            ..PermissionConfig::default()
        });

        let result = evaluate_tool_call(
            &config,
            &path_request("filesystem.write", "target/release/out.log"),
            Path::new("/tmp/project"),
        );

        assert_eq!(result.response.decision, AgentDecision::Allow);
        assert_eq!(result.matched_rule.as_deref(), Some("target/**"));
    }

    #[test]
    fn filesystem_write_deny_glob_blocks() {
        let config = build_with_permission(PermissionConfig {
            write: BTreeMap::from([(".ssh/**".to_string(), Action::Deny)]),
            ..PermissionConfig::default()
        });

        let result = evaluate_tool_call(
            &config,
            &path_request("filesystem.write", ".ssh/id_rsa"),
            Path::new("/tmp/project"),
        );

        assert_eq!(result.response.decision, AgentDecision::Deny);
        assert_eq!(result.matched_rule.as_deref(), Some(".ssh/**"));
    }

    #[test]
    fn filesystem_edit_specificity_picks_most_specific_rule() {
        let config = build_with_permission(PermissionConfig {
            edit: BTreeMap::from([
                ("**".to_string(), Action::Ask),
                ("src/**/*.rs".to_string(), Action::Allow),
                ("src/generated/**".to_string(), Action::Deny),
            ]),
            ..PermissionConfig::default()
        });

        let generated = evaluate_tool_call(
            &config,
            &path_request("filesystem.edit", "src/generated/bindings.rs"),
            Path::new("/tmp/project"),
        );
        let regular = evaluate_tool_call(
            &config,
            &path_request("filesystem.edit", "src/main.rs"),
            Path::new("/tmp/project"),
        );
        let other = evaluate_tool_call(
            &config,
            &path_request("filesystem.edit", "README.md"),
            Path::new("/tmp/project"),
        );

        assert_eq!(generated.response.decision, AgentDecision::Deny);
        assert_eq!(generated.matched_rule.as_deref(), Some("src/generated/**"));
        assert_eq!(regular.response.decision, AgentDecision::Allow);
        assert_eq!(regular.matched_rule.as_deref(), Some("src/**/*.rs"));
        assert_eq!(other.response.decision, AgentDecision::Ask);
        assert_eq!(other.matched_rule.as_deref(), Some("**"));
    }

    #[test]
    fn filesystem_read_unmatched_falls_back_to_allow_when_enabled() {
        let config = build_with_permission(PermissionConfig::default());

        let result = evaluate_tool_call(
            &config,
            &path_request("filesystem.read", "README.md"),
            Path::new("/tmp/project"),
        );

        assert_eq!(result.response.decision, AgentDecision::Allow);
        assert!(result.matched_rule.is_none());
    }

    #[test]
    fn filesystem_write_unmatched_falls_back_to_ask_when_enabled() {
        let config = build_with_permission(PermissionConfig::default());

        let result = evaluate_tool_call(
            &config,
            &path_request("filesystem.write", "notes.md"),
            Path::new("/tmp/project"),
        );

        assert_eq!(result.response.decision, AgentDecision::Ask);
        assert!(result.matched_rule.is_none());
    }

    #[test]
    fn filesystem_write_falls_back_to_deny_when_tool_disabled() {
        let config = AgentConfig {
            accent: None,
            tools: BTreeMap::from([("filesystem.write".to_string(), false)]),
            permission: PermissionConfig::default(),
        };

        let result = evaluate_tool_call(
            &config,
            &path_request("filesystem.write", "notes.md"),
            Path::new("/tmp/project"),
        );

        assert_eq!(result.response.decision, AgentDecision::Deny);
    }

    #[test]
    fn metadata_command_tool_uses_declared_argument_and_alias_rules() {
        let config = AgentConfig {
            accent: None,
            tools: BTreeMap::from([("shell.run".to_string(), true)]),
            permission: PermissionConfig {
                command: BTreeMap::from([("cargo check".to_string(), Action::Allow)]),
                ..PermissionConfig::default()
            },
        };
        let request = EvaluateToolCallRequest {
            session_id: bcode_session_models::SessionId::new(),
            agent_id: BUILD_AGENT.to_string(),
            tool_name: "custom.exec".to_string(),
            operation: ToolPolicyOperation::Command {
                command: Some("cargo check".to_string()),
                analysis: bcode_shell_command_analysis::analyze(
                    &bcode_shell_command_analysis_models::ShellAnalysisRequest::posix(
                        "cargo check",
                    ),
                )
                .ok(),
                analysis_error: None,
            },
            aliases: vec!["command".to_string()],
            requires_permission: true,
            policy_profile: None,
            cwd: Some("/tmp/project".to_string()),
            effective_config_toml: None,
        };

        let result = evaluate_tool_call(&config, &request, Path::new("/tmp/project"));

        assert_eq!(result.response.decision, AgentDecision::Allow);
        assert_eq!(result.matched_rule.as_deref(), Some("cargo check"));
    }

    #[test]
    fn metadata_url_tool_uses_declared_argument_and_web_rules() {
        let config = AgentConfig {
            accent: None,
            tools: BTreeMap::new(),
            permission: PermissionConfig {
                web: BTreeMap::from([("https://example.com/*".to_string(), Action::Allow)]),
                ..PermissionConfig::default()
            },
        };
        let request = EvaluateToolCallRequest {
            session_id: bcode_session_models::SessionId::new(),
            agent_id: PLAN_AGENT.to_string(),
            tool_name: "custom.fetch".to_string(),
            operation: ToolPolicyOperation::Web {
                url: Some("https://example.com/docs".to_string()),
            },
            aliases: vec!["web".to_string()],
            requires_permission: false,
            policy_profile: None,
            cwd: Some("/tmp/project".to_string()),
            effective_config_toml: None,
        };

        let result = evaluate_tool_call(&config, &request, Path::new("/tmp/project"));

        assert_eq!(result.response.decision, AgentDecision::Allow);
        assert_eq!(
            result.matched_rule.as_deref(),
            Some("https://example.com/*")
        );
    }

    #[test]
    fn metadata_write_path_tool_uses_declared_argument_and_category_rules() {
        let config = AgentConfig {
            accent: None,
            tools: BTreeMap::from([("write".to_string(), true)]),
            permission: PermissionConfig {
                write: BTreeMap::from([("generated/**".to_string(), Action::Deny)]),
                ..PermissionConfig::default()
            },
        };
        let request = EvaluateToolCallRequest {
            session_id: bcode_session_models::SessionId::new(),
            agent_id: BUILD_AGENT.to_string(),
            tool_name: "custom.write".to_string(),
            operation: ToolPolicyOperation::Write {
                paths: vec!["generated/out.rs".to_string()],
                category: "write".to_string(),
            },
            aliases: vec!["write".to_string()],
            requires_permission: true,
            policy_profile: None,
            cwd: Some("/tmp/project".to_string()),
            effective_config_toml: None,
        };

        let result = evaluate_tool_call(&config, &request, Path::new("/tmp/project"));

        assert_eq!(result.response.decision, AgentDecision::Deny);
        assert_eq!(result.matched_rule.as_deref(), Some("generated/**"));
    }

    #[test]
    fn batch_paths_aggregate_rules_independently_of_order() {
        let config = AgentConfig {
            accent: None,
            tools: BTreeMap::new(),
            permission: PermissionConfig {
                edit: BTreeMap::from([
                    ("src/**".to_owned(), Action::Allow),
                    ("review/**".to_owned(), Action::Ask),
                    ("secret/**".to_owned(), Action::Deny),
                ]),
                ..PermissionConfig::default()
            },
        };
        for (paths, expected) in [
            (vec!["src/a", "src/b"], AgentDecision::Allow),
            (vec!["src/a", "review/b"], AgentDecision::Ask),
            (vec!["review/b", "src/a"], AgentDecision::Ask),
            (vec!["src/a", "secret/b"], AgentDecision::Deny),
            (vec!["secret/b", "src/a"], AgentDecision::Deny),
            (vec!["review/a", "secret/b"], AgentDecision::Deny),
            (vec!["../external", "secret/b"], AgentDecision::Deny),
            (vec!["src/a", "unmatched/b"], AgentDecision::Deny),
        ] {
            let request = EvaluateToolCallRequest {
                session_id: bcode_session_models::SessionId::new(),
                agent_id: BUILD_AGENT.to_owned(),
                tool_name: "custom.batch".to_owned(),
                operation: ToolPolicyOperation::Write {
                    paths: paths.iter().map(ToString::to_string).collect(),
                    category: "edit".to_owned(),
                },
                aliases: vec!["edit".to_owned()],
                requires_permission: true,
                policy_profile: None,
                cwd: Some("/tmp/project".to_owned()),
                effective_config_toml: None,
            };
            let result = evaluate_tool_call(&config, &request, Path::new("/tmp/project"));
            assert_eq!(result.response.decision, expected, "{paths:?}");
        }
    }

    #[test]
    fn batch_path_categories_preserve_specificity_fallback_and_external_access() {
        let rules = BTreeMap::from([
            ("src/**".to_owned(), Action::Allow),
            ("src/private/**".to_owned(), Action::Deny),
            ("src/private/public.rs".to_owned(), Action::Allow),
            ("../external/**".to_owned(), Action::Allow),
        ]);
        for category in ["read", "write", "edit"] {
            let config = AgentConfig {
                accent: None,
                tools: BTreeMap::from([("custom.batch".to_owned(), true)]),
                permission: PermissionConfig {
                    read: rules.clone(),
                    write: rules.clone(),
                    edit: rules.clone(),
                    external_directory: Action::Allow,
                    ..PermissionConfig::default()
                },
            };
            for (paths, expected, winning_rule) in [
                (
                    vec!["src/a", "src/private/secret.rs"],
                    AgentDecision::Deny,
                    Some("src/private/**"),
                ),
                (
                    vec!["src/private/public.rs", "src/a"],
                    AgentDecision::Allow,
                    Some("src/private/public.rs"),
                ),
                (
                    vec!["src/a", "../external/b"],
                    AgentDecision::Allow,
                    Some("src/**"),
                ),
                (
                    vec!["src/a", "unmatched/b"],
                    if category == "read" {
                        AgentDecision::Allow
                    } else {
                        AgentDecision::Ask
                    },
                    if category == "read" {
                        Some("src/**")
                    } else {
                        None
                    },
                ),
            ] {
                let paths = paths.into_iter().map(str::to_owned).collect();
                let request = EvaluateToolCallRequest {
                    session_id: bcode_session_models::SessionId::new(),
                    agent_id: BUILD_AGENT.to_owned(),
                    tool_name: "custom.batch".to_owned(),
                    operation: if category == "read" {
                        ToolPolicyOperation::Read { paths }
                    } else {
                        ToolPolicyOperation::Write {
                            paths,
                            category: category.to_owned(),
                        }
                    },
                    aliases: Vec::new(),
                    requires_permission: category != "read",
                    policy_profile: None,
                    cwd: Some("/tmp/project".to_owned()),
                    effective_config_toml: None,
                };
                let result = evaluate_tool_call(&config, &request, Path::new("/tmp/project"));
                assert_eq!(
                    result.response.decision, expected,
                    "{category}: {:?}",
                    request.operation
                );
                assert_eq!(result.matched_rule.as_deref(), winning_rule);
            }
        }
    }

    #[test]
    fn metadata_edit_category_uses_edit_rules_for_write_paths() {
        let config = AgentConfig {
            accent: None,
            tools: BTreeMap::from([("edit".to_string(), true)]),
            permission: PermissionConfig {
                edit: BTreeMap::from([("src/**".to_string(), Action::Allow)]),
                ..PermissionConfig::default()
            },
        };
        let request = EvaluateToolCallRequest {
            session_id: bcode_session_models::SessionId::new(),
            agent_id: BUILD_AGENT.to_string(),
            tool_name: "custom.patch".to_string(),
            operation: ToolPolicyOperation::Write {
                paths: vec!["src/lib.rs".to_string()],
                category: "edit".to_string(),
            },
            aliases: vec!["edit".to_string()],
            requires_permission: true,
            policy_profile: None,
            cwd: Some("/tmp/project".to_string()),
            effective_config_toml: None,
        };

        let result = evaluate_tool_call(&config, &request, Path::new("/tmp/project"));

        assert_eq!(result.response.decision, AgentDecision::Allow);
        assert_eq!(result.matched_rule.as_deref(), Some("src/**"));
    }

    #[test]
    fn external_directory_uses_metadata_path_arguments() {
        let config = AgentConfig {
            accent: None,
            tools: BTreeMap::from([("write".to_string(), true)]),
            permission: PermissionConfig {
                external_directory: Action::Deny,
                ..PermissionConfig::default()
            },
        };
        let request = EvaluateToolCallRequest {
            session_id: bcode_session_models::SessionId::new(),
            agent_id: BUILD_AGENT.to_string(),
            tool_name: "custom.write".to_string(),
            operation: ToolPolicyOperation::Write {
                paths: vec!["../outside.txt".to_string()],
                category: "write".to_string(),
            },
            aliases: vec!["write".to_string()],
            requires_permission: true,
            policy_profile: None,
            cwd: Some("/tmp/project".to_string()),
            effective_config_toml: None,
        };

        let result = evaluate_tool_call(&config, &request, Path::new("/tmp/project"));

        assert_eq!(result.response.decision, AgentDecision::Deny);
    }

    #[test]
    fn metadata_alias_can_enable_unknown_mutating_tool_fallback() {
        let config = AgentConfig {
            accent: None,
            tools: BTreeMap::from([("custom-category".to_string(), true)]),
            permission: PermissionConfig::default(),
        };
        let request = EvaluateToolCallRequest {
            session_id: bcode_session_models::SessionId::new(),
            agent_id: BUILD_AGENT.to_string(),
            tool_name: "custom.side-effect".to_string(),
            operation: ToolPolicyOperation::Mutating,
            aliases: vec!["custom-category".to_string()],
            requires_permission: true,
            policy_profile: None,
            cwd: Some("/tmp/project".to_string()),
            effective_config_toml: None,
        };

        let result = evaluate_tool_call(&config, &request, Path::new("/tmp/project"));

        assert_eq!(result.response.decision, AgentDecision::Ask);
    }

    #[test]
    fn external_directory_short_circuits_before_path_rules() {
        let config = AgentConfig {
            accent: None,
            tools: BTreeMap::from([("filesystem.write".to_string(), true)]),
            permission: PermissionConfig {
                external_directory: Action::Deny,
                write: BTreeMap::from([("**".to_string(), Action::Allow)]),
                ..PermissionConfig::default()
            },
        };

        let result = evaluate_tool_call(
            &config,
            &path_request("filesystem.write", "../outside.txt"),
            Path::new("/tmp/project"),
        );

        assert_eq!(result.response.decision, AgentDecision::Deny);
    }
}
