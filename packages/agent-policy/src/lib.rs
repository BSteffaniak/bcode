#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Pi/OpenCode-style agent policy parsing and evaluation.

pub use bcode_agent_policy_models::{
    Action, AgentConfig, AgentPermissionConfig, PermissionConfig, default_external_directory_action,
};

use bcode_agent_profile::{
    AgentDecision, EvaluateToolCallRequest, EvaluateToolCallResponse, ShellPolicyDiagnostic,
    ShellPolicySubjectKind, ToolPolicyOperation,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

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
            Action::Ask => evaluation(
                AgentDecision::Ask,
                format!(
                    "{} agent asks before external directory access: {}",
                    request.agent_id, path
                ),
                None,
                None,
            ),
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
    let path = candidates.first().cloned();
    let compiled = compile_path_rules(rules);

    let rule_match = path
        .as_deref()
        .and_then(|path| matching_path_rule(&compiled, path));

    if let Some(rule) = rule_match {
        let rule_pattern = Some(rule.pattern.clone());
        let subject = path.unwrap_or_default();
        return match rule.action {
            Action::Allow => evaluation(AgentDecision::Allow, String::new(), rule_pattern, None),
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

    let rules = compile_rules(config);
    let mut results = Vec::new();
    for subject in &analysis.commands {
        if subject.match_candidates.is_empty() {
            return shell_fact_denied(request, "shell command has no policy match candidate");
        }
        let mut winning: Option<(Action, Rule, &str)> = None;
        for candidate in &subject.match_candidates {
            let Some(rule) = matching_rule(&candidate.subject, &rules) else {
                continue;
            };
            let replace = winning.as_ref().is_none_or(|(_, current, _)| {
                (rule.specificity, rule.pattern.len())
                    > (current.specificity, current.pattern.len())
            });
            if replace {
                winning = Some((rule.action, rule, candidate.subject.as_str()));
            }
        }
        let (action, rule, candidate) = winning.unwrap_or_else(|| {
            (
                Action::Deny,
                Rule {
                    pattern: "<missing-rule>".to_owned(),
                    action: Action::Deny,
                    specificity: 0,
                },
                subject.source.as_str(),
            )
        });
        results.push(ShellSubjectDecision {
            action,
            rule,
            candidate: candidate.to_owned(),
            subject: subject.source.clone(),
            span: Some(subject.span),
            kind: ShellPolicySubjectKind::Command,
        });
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
        });
    }

    let winner = results
        .into_iter()
        .max_by_key(|result| action_precedence(result.action))
        .expect("shell analysis contains at least one command result");
    let decision = agent_decision(winner.action);
    let aggregate_reason = format!(
        "{} shell program: {} '{}' at bytes {}..{} using candidate '{}' matched rule '{}' ({:?}); aggregate precedence deny > ask > allow selected {:?}",
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
        decision,
    );
    let diagnostic = ShellPolicyDiagnostic {
        original_source: analysis.source.clone(),
        subject: winner.subject.clone(),
        span: winner.span,
        dialect: analysis.dialect,
        match_candidate: winner.candidate,
        matched_rule: winner.rule.pattern.clone(),
        subject_kind: winner.kind,
        subject_decision: decision,
        aggregate_decision: decision,
        remember_patterns: shell_remember_patterns(winner.kind, &analysis.commands, winner.span),
        aggregate_reason: aggregate_reason.clone(),
    };
    shell_evaluation(
        decision,
        aggregate_reason,
        Some(winner.rule.pattern),
        Some(winner.subject),
        diagnostic,
    )
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
    fn git_reviewed_alias_uses_specificity_without_hiding_original_denies() {
        let config = AgentConfig {
            accent: None,
            tools: BTreeMap::from([("shell.run".to_owned(), true)]),
            permission: PermissionConfig {
                command: BTreeMap::from([
                    ("git *".to_owned(), Action::Deny),
                    ("git diff *".to_owned(), Action::Allow),
                ]),
                ..PermissionConfig::default()
            },
        };
        let allowed = evaluate_tool_call(
            &config,
            &request(BUILD_AGENT, "git --no-pager diff --stat"),
            Path::new("/tmp/project"),
        );
        assert_eq!(allowed.response.decision, AgentDecision::Allow);
        assert_eq!(allowed.matched_rule.as_deref(), Some("git diff *"));

        let config = AgentConfig {
            permission: PermissionConfig {
                command: BTreeMap::from([
                    ("git *".to_owned(), Action::Allow),
                    ("git --no-pager diff --stat".to_owned(), Action::Deny),
                    ("git diff *".to_owned(), Action::Allow),
                ]),
                ..PermissionConfig::default()
            },
            ..config
        };
        let denied = evaluate_tool_call(
            &config,
            &request(BUILD_AGENT, "git --no-pager diff --stat"),
            Path::new("/tmp/project"),
        );
        assert_eq!(denied.response.decision, AgentDecision::Deny);
        assert_eq!(
            denied.matched_rule.as_deref(),
            Some("git --no-pager diff --stat")
        );
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
        };

        let result = evaluate_tool_call(&config, &request, Path::new("/tmp/project"));

        assert_eq!(result.response.decision, AgentDecision::Deny);
        assert_eq!(result.matched_rule.as_deref(), Some("generated/**"));
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
