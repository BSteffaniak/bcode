#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Pi/OpenCode-style agent policy parsing and evaluation.

pub use bcode_agent_policy_models::{
    Action, AgentConfig, AgentPermissionConfig, PermissionConfig, default_external_directory_action,
};

use bcode_agent_profile::{AgentDecision, EvaluateToolCallRequest, EvaluateToolCallResponse};
use bcode_tool::{ToolArgumentKind, ToolSideEffect};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Built-in build agent ID.
pub const BUILD_AGENT: &str = "build";
/// Built-in plan agent ID.
pub const PLAN_AGENT: &str = "plan";

/// Compiled bash permission rule.
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
            tools: BTreeMap::from([
                ("shell.run".to_string(), true),
                ("filesystem.read".to_string(), true),
                ("filesystem.write".to_string(), true),
                ("filesystem.edit".to_string(), true),
                ("web.search".to_string(), true),
                ("web.fetch".to_string(), true),
                ("web.status".to_string(), true),
                ("web.inspect".to_string(), true),
                ("git.clone".to_string(), true),
                ("github.clone".to_string(), true),
                ("worktree.list".to_string(), true),
                ("worktree.create".to_string(), true),
                ("worktree.remove".to_string(), true),
                ("document.extract".to_string(), true),
            ]),
            permission: PermissionConfig {
                bash: BTreeMap::from([("*".to_string(), Action::Ask)]),
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
            tools: BTreeMap::from([
                ("shell.run".to_string(), true),
                ("filesystem.read".to_string(), true),
                ("filesystem.write".to_string(), false),
                ("filesystem.edit".to_string(), false),
                ("web.search".to_string(), true),
                ("web.fetch".to_string(), true),
                ("web.status".to_string(), true),
                ("web.inspect".to_string(), true),
                ("git.clone".to_string(), true),
                ("github.clone".to_string(), true),
                ("worktree.list".to_string(), true),
                ("worktree.create".to_string(), true),
                ("worktree.remove".to_string(), true),
                ("document.extract".to_string(), true),
            ]),
            permission: PermissionConfig {
                bash: BTreeMap::from([
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

/// Compute Bcode tool names visible for this agent.
#[must_use]
pub fn active_tools_for(config: &AgentConfig) -> Vec<String> {
    let mut tools = BTreeSet::from([
        "filesystem.read".to_string(),
        "filesystem.exists".to_string(),
        "filesystem.list".to_string(),
        "filesystem.find".to_string(),
        "filesystem.grep".to_string(),
        "filesystem.stat".to_string(),
        "web.search".to_string(),
        "web.fetch".to_string(),
        "web.status".to_string(),
        "web.inspect".to_string(),
    ]);
    for (tool, enabled) in &config.tools {
        if *enabled {
            tools.insert(tool.clone());
        }
    }
    tools.into_iter().collect()
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
    if has_argument_kind(request, ToolArgumentKind::Command) {
        return evaluate_shell(config, request);
    }
    if has_argument_kind(request, ToolArgumentKind::Url) {
        return evaluate_web_url(config, request);
    }
    if has_argument_kind(request, ToolArgumentKind::WritePath) {
        return evaluate_filesystem_path(config, request, write_path_rules(config, request));
    }
    if has_argument_kind(request, ToolArgumentKind::ReadPath) {
        return evaluate_filesystem_path(config, request, &config.permission.read);
    }
    evaluate_side_effect_fallback(config, request)
}

fn write_path_rules<'a>(
    config: &'a AgentConfig,
    request: &EvaluateToolCallRequest,
) -> &'a BTreeMap<String, Action> {
    match request.policy.permission_category.as_deref() {
        Some("edit") => &config.permission.edit,
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
            format!("{} agent asks before web.fetch", request.agent_id),
            None,
            None,
        )
    } else {
        evaluation(
            AgentDecision::Deny,
            format!(
                "{} agent denied web.fetch; enable the tool if web page reads are allowed",
                request.agent_id
            ),
            None,
            None,
        )
    }
}

fn evaluate_side_effect_fallback(
    config: &AgentConfig,
    request: &EvaluateToolCallRequest,
) -> PolicyEvaluation {
    match request.side_effect {
        ToolSideEffect::ReadOnly => evaluation(AgentDecision::Allow, String::new(), None, None),
        ToolSideEffect::WriteFiles | ToolSideEffect::ExecuteProcess => {
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

    evaluate_side_effect_fallback(config, request)
}

fn url_argument(request: &EvaluateToolCallRequest) -> Option<&str> {
    request
        .policy
        .argument_extractors
        .iter()
        .filter(|extractor| extractor.kind == ToolArgumentKind::Url)
        .find_map(|extractor| string_argument(&request.arguments, &extractor.argument))
        .or_else(|| string_argument(&request.arguments, "url"))
}

fn evaluate_shell(config: &AgentConfig, request: &EvaluateToolCallRequest) -> PolicyEvaluation {
    let Some(command) = command_argument(request) else {
        return evaluation(
            AgentDecision::Deny,
            format!(
                "{} agent denied shell command with missing command",
                request.agent_id
            ),
            None,
            None,
        );
    };
    let rules = compile_rules(config);
    if writes_disabled(config)
        && let Some(part) = mutating_shell_command_part(command, &rules)
    {
        return evaluation(
            AgentDecision::Deny,
            format!(
                "{} agent denied shell command '{}': shell command writes files; switch agents if implementation is needed",
                request.agent_id, part
            ),
            None,
            Some(part.to_string()),
        );
    }
    if let Some(denied) = denied_command_part(command, &rules) {
        let rule_pattern = denied.rule.as_ref().map(|rule| rule.pattern.clone());
        return match denied.action {
            Action::Allow => evaluation(AgentDecision::Allow, String::new(), rule_pattern, None),
            Action::Ask => evaluation(
                AgentDecision::Ask,
                format!(
                    "{} agent asks before shell command: {}",
                    request.agent_id, denied.command
                ),
                rule_pattern,
                Some(denied.command.to_string()),
            ),
            Action::Deny => evaluation(
                AgentDecision::Deny,
                format!(
                    "{} agent denied shell command '{}'{}",
                    request.agent_id,
                    denied.command,
                    denied
                        .rule
                        .as_ref()
                        .map_or_else(String::new, |rule| format!(" by rule '{}'", rule.pattern))
                ),
                rule_pattern,
                Some(denied.command.to_string()),
            ),
        };
    }
    evaluation(AgentDecision::Allow, String::new(), None, None)
}

fn command_argument(request: &EvaluateToolCallRequest) -> Option<&str> {
    request
        .policy
        .argument_extractors
        .iter()
        .filter(|extractor| extractor.kind == ToolArgumentKind::Command)
        .find_map(|extractor| string_argument(&request.arguments, &extractor.argument))
        .or_else(|| string_argument(&request.arguments, "command"))
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

fn has_argument_kind(request: &EvaluateToolCallRequest, kind: ToolArgumentKind) -> bool {
    request
        .policy
        .argument_extractors
        .iter()
        .any(|extractor| extractor.kind == kind)
}

/// Return candidate path arguments for tool policy checks.
#[must_use]
pub fn candidate_paths(request: &EvaluateToolCallRequest) -> Vec<String> {
    let metadata_paths = request
        .policy
        .argument_extractors
        .iter()
        .filter(|extractor| {
            matches!(
                extractor.kind,
                ToolArgumentKind::ReadPath | ToolArgumentKind::WritePath
            )
        })
        .filter_map(|extractor| {
            string_argument(&request.arguments, &extractor.argument).map(ToString::to_string)
        });
    let legacy_paths = ["path", "filePath"]
        .iter()
        .filter_map(|key| string_argument(&request.arguments, key).map(ToString::to_string));
    metadata_paths
        .chain(legacy_paths)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
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

fn writes_disabled(config: &AgentConfig) -> bool {
    ["write", "edit", "filesystem.write", "filesystem.edit"]
        .iter()
        .any(|tool| config.tools.get(*tool) == Some(&false))
}

fn mutating_shell_command_part<'a>(command: &'a str, rules: &[Rule]) -> Option<&'a str> {
    command_parts(command)
        .into_iter()
        .find(|part| shell_part_writes_files(part) && !explicitly_allows_shell_part(part, rules))
}

fn explicitly_allows_shell_part(part: &str, rules: &[Rule]) -> bool {
    matching_rule(part, rules).is_some_and(|rule| rule.action == Action::Allow)
}

fn shell_part_writes_files(part: &str) -> bool {
    has_unquoted_write_redirection(part) || starts_with_mutating_command(part)
}

fn has_unquoted_write_redirection(part: &str) -> bool {
    let mut escaped = false;
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let chars = part.chars().collect::<Vec<_>>();
    for (index, character) in chars.iter().copied().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' && !in_single_quote {
            escaped = true;
            continue;
        }
        match character {
            '\'' if !in_double_quote => in_single_quote = !in_single_quote,
            '"' if !in_single_quote => in_double_quote = !in_double_quote,
            '>' if !in_single_quote && !in_double_quote => {
                let previous = index
                    .checked_sub(1)
                    .and_then(|previous| chars.get(previous));
                if chars.get(index + 1) == Some(&'&') && previous != Some(&'&') {
                    continue;
                }
                return true;
            }
            _ => {}
        }
    }
    false
}

fn starts_with_mutating_command(part: &str) -> bool {
    let Some(command) = first_command_word(part) else {
        return false;
    };
    matches!(
        command,
        "cp" | "install" | "mkdir" | "mv" | "rm" | "rmdir" | "tee" | "touch" | "truncate"
    ) || command == "sed"
        && part
            .split_whitespace()
            .any(|arg| arg == "-i" || arg.starts_with("-i"))
        || command == "perl"
            && part
                .split_whitespace()
                .any(|arg| arg == "-pi" || arg.starts_with("-pi"))
}

fn first_command_word(part: &str) -> Option<&str> {
    part.split_whitespace()
        .find(|word| !word.contains('=') && *word != "env")
}

fn tool_aliases(request: &EvaluateToolCallRequest) -> Vec<String> {
    let mut aliases = vec![request.tool_name.clone()];
    if let Some(category) = &request.policy.permission_category {
        aliases.push(category.clone());
    }
    aliases.extend(request.policy.aliases.iter().cloned());
    aliases
}

/// Compile bash glob rules.
#[must_use]
pub fn compile_rules(config: &AgentConfig) -> Vec<Rule> {
    config
        .permission
        .bash
        .iter()
        .map(|(pattern, action)| Rule {
            pattern: pattern.clone(),
            action: *action,
            specificity: rule_specificity(pattern),
        })
        .collect()
}

#[derive(Debug, Clone)]
struct DeniedCommandPart<'a> {
    command: &'a str,
    rule: Option<Rule>,
    action: Action,
}

fn denied_command_part<'a>(command: &'a str, rules: &[Rule]) -> Option<DeniedCommandPart<'a>> {
    command_parts(command).into_iter().find_map(|part| {
        let rule = matching_rule(part, rules);
        let action = rule.as_ref().map_or(Action::Deny, |rule| rule.action);
        (action != Action::Allow).then_some(DeniedCommandPart {
            command: part,
            rule,
            action,
        })
    })
}

/// Split shell command chains like the Pi `OpenCode` modes extension.
#[must_use]
pub fn command_parts(command: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let bytes = command.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        let separator_len = if bytes[index] == b'&' && bytes.get(index + 1) == Some(&b'&') {
            2
        } else if bytes[index] == b'|' {
            if bytes.get(index + 1) == Some(&b'|') {
                2
            } else {
                1
            }
        } else {
            usize::from(bytes[index] == b';')
        };
        if separator_len == 0 {
            index = index.saturating_add(1);
            continue;
        }
        let part = command[start..index].trim();
        if !part.is_empty() {
            parts.push(part);
        }
        index = index.saturating_add(separator_len);
        start = index;
    }
    let part = command[start..].trim();
    if !part.is_empty() {
        parts.push(part);
    }
    if parts.is_empty() {
        vec![command]
    } else {
        parts
    }
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

fn string_argument<'a>(arguments: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    arguments.get(key).and_then(serde_json::Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_agent_profile::EvaluateToolCallRequest;
    use serde_json::json;

    fn command_policy() -> bcode_tool::ToolPolicyMetadata {
        bcode_tool::ToolPolicyMetadata {
            aliases: vec!["bash".to_string()],
            permission_category: Some("bash".to_string()),
            argument_extractors: vec![bcode_tool::ToolArgumentExtractor {
                kind: ToolArgumentKind::Command,
                argument: "command".to_string(),
            }],
        }
    }

    fn path_policy(category: &str, kind: ToolArgumentKind) -> bcode_tool::ToolPolicyMetadata {
        bcode_tool::ToolPolicyMetadata {
            aliases: vec![category.to_string()],
            permission_category: Some(category.to_string()),
            argument_extractors: vec![bcode_tool::ToolArgumentExtractor {
                kind,
                argument: "path".to_string(),
            }],
        }
    }

    fn request(agent_id: &str, command: &str) -> EvaluateToolCallRequest {
        EvaluateToolCallRequest {
            session_id: bcode_session_models::SessionId::new(),
            agent_id: agent_id.to_string(),
            tool_name: "shell.run".to_string(),
            side_effect: ToolSideEffect::ExecuteProcess,
            policy: command_policy(),
            arguments: json!({ "command": command }),
            cwd: Some("/tmp/project".to_string()),
        }
    }

    #[test]
    fn active_tools_include_typed_read_only_tools_by_default() {
        let config = default_config();
        let plan = agent_config(&config, PLAN_AGENT);
        let tools = active_tools_for(&plan);

        assert!(tools.contains(&"filesystem.read".to_string()));
        assert!(tools.contains(&"filesystem.list".to_string()));
        assert!(tools.contains(&"filesystem.find".to_string()));
        assert!(tools.contains(&"filesystem.grep".to_string()));
        assert!(tools.contains(&"filesystem.stat".to_string()));
        assert!(!tools.contains(&"filesystem.write".to_string()));
    }

    #[test]
    fn command_parts_match_pi_splitters() {
        assert_eq!(
            command_parts("git diff && git status"),
            vec!["git diff", "git status"]
        );
        assert_eq!(
            command_parts("cargo check || git reset --hard"),
            vec!["cargo check", "git reset --hard"]
        );
        assert_eq!(command_parts("echo hi | wc -c"), vec!["echo hi", "wc -c"]);
        assert_eq!(
            command_parts("git status; git push"),
            vec!["git status", "git push"]
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
                        ("bash".to_string(), true),
                        ("write".to_string(), false),
                        ("edit".to_string(), false),
                    ]),
                    permission: PermissionConfig {
                        bash: BTreeMap::from([
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

        assert_eq!(redirected.response.decision, AgentDecision::Allow);
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
                    tools: BTreeMap::from([("bash".to_string(), true)]),
                    permission: PermissionConfig {
                        bash: BTreeMap::from([
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
                bash: BTreeMap::new(),
                external_directory: Action::Deny,
                ..PermissionConfig::default()
            },
        };
        let request = EvaluateToolCallRequest {
            session_id: bcode_session_models::SessionId::new(),
            agent_id: BUILD_AGENT.to_string(),
            tool_name: "filesystem.write".to_string(),
            side_effect: ToolSideEffect::WriteFiles,
            policy: path_policy("write", ToolArgumentKind::WritePath),
            arguments: json!({ "path": "../outside.txt" }),
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
            side_effect: match tool_name {
                "filesystem.write" | "filesystem.edit" => ToolSideEffect::WriteFiles,
                _ => ToolSideEffect::ReadOnly,
            },
            policy: match tool_name {
                "filesystem.write" => path_policy("write", ToolArgumentKind::WritePath),
                "filesystem.edit" => path_policy("edit", ToolArgumentKind::WritePath),
                _ => path_policy("read", ToolArgumentKind::ReadPath),
            },
            arguments: json!({ "path": path }),
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
            tools: BTreeMap::from([("bash".to_string(), true)]),
            permission: PermissionConfig {
                bash: BTreeMap::from([("cargo check".to_string(), Action::Allow)]),
                ..PermissionConfig::default()
            },
        };
        let request = EvaluateToolCallRequest {
            session_id: bcode_session_models::SessionId::new(),
            agent_id: BUILD_AGENT.to_string(),
            tool_name: "custom.exec".to_string(),
            side_effect: ToolSideEffect::ExecuteProcess,
            policy: bcode_tool::ToolPolicyMetadata {
                aliases: vec!["bash".to_string()],
                permission_category: Some("bash".to_string()),
                argument_extractors: vec![bcode_tool::ToolArgumentExtractor {
                    kind: ToolArgumentKind::Command,
                    argument: "cmd".to_string(),
                }],
            },
            arguments: json!({ "cmd": "cargo check" }),
            cwd: Some("/tmp/project".to_string()),
        };

        let result = evaluate_tool_call(&config, &request, Path::new("/tmp/project"));

        assert_eq!(result.response.decision, AgentDecision::Allow);
        assert!(result.matched_rule.is_none());
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
            side_effect: ToolSideEffect::ReadOnly,
            policy: bcode_tool::ToolPolicyMetadata {
                aliases: vec!["web".to_string()],
                permission_category: Some("web".to_string()),
                argument_extractors: vec![bcode_tool::ToolArgumentExtractor {
                    kind: ToolArgumentKind::Url,
                    argument: "target".to_string(),
                }],
            },
            arguments: json!({ "target": "https://example.com/docs" }),
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
            side_effect: ToolSideEffect::WriteFiles,
            policy: bcode_tool::ToolPolicyMetadata {
                aliases: vec!["write".to_string()],
                permission_category: Some("write".to_string()),
                argument_extractors: vec![bcode_tool::ToolArgumentExtractor {
                    kind: ToolArgumentKind::WritePath,
                    argument: "target_path".to_string(),
                }],
            },
            arguments: json!({ "target_path": "generated/out.rs" }),
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
            side_effect: ToolSideEffect::WriteFiles,
            policy: bcode_tool::ToolPolicyMetadata {
                aliases: vec!["edit".to_string()],
                permission_category: Some("edit".to_string()),
                argument_extractors: vec![bcode_tool::ToolArgumentExtractor {
                    kind: ToolArgumentKind::WritePath,
                    argument: "file".to_string(),
                }],
            },
            arguments: json!({ "file": "src/lib.rs" }),
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
            side_effect: ToolSideEffect::WriteFiles,
            policy: bcode_tool::ToolPolicyMetadata {
                aliases: vec!["write".to_string()],
                permission_category: Some("write".to_string()),
                argument_extractors: vec![bcode_tool::ToolArgumentExtractor {
                    kind: ToolArgumentKind::WritePath,
                    argument: "output".to_string(),
                }],
            },
            arguments: json!({ "output": "../outside.txt" }),
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
            side_effect: ToolSideEffect::WriteFiles,
            policy: bcode_tool::ToolPolicyMetadata {
                aliases: Vec::new(),
                permission_category: Some("custom-category".to_string()),
                argument_extractors: Vec::new(),
            },
            arguments: json!({}),
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
