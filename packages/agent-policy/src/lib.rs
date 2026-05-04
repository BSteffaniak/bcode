#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Pi/OpenCode-style agent policy parsing and evaluation.

use bcode_agent_profile::{AgentDecision, EvaluateToolCallRequest, EvaluateToolCallResponse};
use bcode_tool::ToolSideEffect;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Built-in build agent ID.
pub const BUILD_AGENT: &str = "build";
/// Built-in plan agent ID.
pub const PLAN_AGENT: &str = "plan";

/// Agent action loaded from Pi/OpenCode-style permission config.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// Allow the operation without prompting.
    Allow,
    /// Deny the operation.
    Deny,
    /// Ask via the host permission prompt.
    #[default]
    Ask,
}

/// Agent mode/profile configuration.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AgentConfig {
    #[serde(default)]
    pub tools: BTreeMap<String, bool>,
    #[serde(default)]
    pub permission: PermissionConfig,
}

/// Permission configuration for an agent.
#[derive(Debug, Clone, Deserialize)]
pub struct PermissionConfig {
    #[serde(default)]
    pub bash: BTreeMap<String, Action>,
    #[serde(default = "default_external_directory_action")]
    pub external_directory: Action,
}

impl Default for PermissionConfig {
    fn default() -> Self {
        Self {
            bash: BTreeMap::new(),
            external_directory: default_external_directory_action(),
        }
    }
}

const fn default_external_directory_action() -> Action {
    Action::Allow
}

/// Pi/OpenCode-style top-level permission config.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AgentPermissionConfig {
    #[serde(default)]
    pub agent: BTreeMap<String, AgentConfig>,
}

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
            tools: BTreeMap::from([
                ("bash".to_string(), true),
                ("read".to_string(), true),
                ("write".to_string(), true),
                ("edit".to_string(), true),
                ("shell.run".to_string(), true),
                ("filesystem.read".to_string(), true),
                ("filesystem.write".to_string(), true),
                ("filesystem.edit".to_string(), true),
            ]),
            permission: PermissionConfig {
                bash: BTreeMap::from([("*".to_string(), Action::Ask)]),
                external_directory: Action::Allow,
            },
        },
    );
    agent.insert(
        PLAN_AGENT.to_string(),
        AgentConfig {
            tools: BTreeMap::from([
                ("bash".to_string(), true),
                ("read".to_string(), true),
                ("write".to_string(), false),
                ("edit".to_string(), false),
                ("shell.run".to_string(), true),
                ("filesystem.read".to_string(), true),
                ("filesystem.write".to_string(), false),
                ("filesystem.edit".to_string(), false),
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
    ]);
    for (tool, enabled) in &config.tools {
        if *enabled {
            tools.extend(normalize_tool_names(tool));
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
    if request.tool_name == "shell.run" {
        return evaluate_shell(config, request);
    }

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

fn evaluate_shell(config: &AgentConfig, request: &EvaluateToolCallRequest) -> PolicyEvaluation {
    let Some(command) = string_argument(&request.arguments, "command") else {
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
    candidate_paths(&request.tool_name, &request.arguments)
        .into_iter()
        .find(|path| is_external_path(path, cwd))
}

/// Return candidate path arguments for tool policy checks.
#[must_use]
pub fn candidate_paths(tool_name: &str, arguments: &serde_json::Value) -> Vec<String> {
    let _ = tool_name;
    let keys: &[&str] = &["path", "filePath"];
    keys.iter()
        .filter_map(|key| string_argument(arguments, key).map(ToString::to_string))
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
    tool_aliases(&request.tool_name)
        .iter()
        .find_map(|name| config.tools.get(name).copied())
}

fn tool_aliases(tool_name: &str) -> Vec<String> {
    let mut aliases = vec![tool_name.to_string()];
    match tool_name {
        "shell.run" => aliases.push("bash".to_string()),
        "filesystem.write" => aliases.push("write".to_string()),
        "filesystem.edit" => aliases.push("edit".to_string()),
        "filesystem.read" | "filesystem.exists" => aliases.push("read".to_string()),
        "filesystem.grep" => aliases.push("grep".to_string()),
        "filesystem.find" => aliases.push("find".to_string()),
        "filesystem.list" => aliases.push("ls".to_string()),
        _ => {}
    }
    aliases
}

/// Normalize Pi/OpenCode tool names to Bcode tool names.
#[must_use]
pub fn normalize_tool_names(tool: &str) -> Vec<String> {
    match tool {
        "bash" => vec!["shell.run".to_string()],
        "read" => vec![
            "filesystem.read".to_string(),
            "filesystem.exists".to_string(),
        ],
        "grep" => vec!["filesystem.grep".to_string()],
        "find" => vec!["filesystem.find".to_string()],
        "ls" => vec!["filesystem.list".to_string()],
        "write" => vec!["filesystem.write".to_string()],
        "edit" => vec!["filesystem.edit".to_string()],
        other => vec![other.to_string()],
    }
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

fn string_argument<'a>(arguments: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    arguments.get(key).and_then(serde_json::Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_agent_profile::EvaluateToolCallRequest;
    use serde_json::json;

    fn request(agent_id: &str, command: &str) -> EvaluateToolCallRequest {
        EvaluateToolCallRequest {
            session_id: bcode_session_models::SessionId::new(),
            agent_id: agent_id.to_string(),
            tool_name: "shell.run".to_string(),
            side_effect: ToolSideEffect::ExecuteProcess,
            arguments: json!({ "command": command }),
            cwd: Some("/tmp/project".to_string()),
        }
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
    fn build_allows_or_denies_by_specific_rules() {
        let config = AgentPermissionConfig {
            agent: BTreeMap::from([(
                BUILD_AGENT.to_string(),
                AgentConfig {
                    tools: BTreeMap::from([("bash".to_string(), true)]),
                    permission: PermissionConfig {
                        bash: BTreeMap::from([
                            ("*".to_string(), Action::Allow),
                            ("git commit *".to_string(), Action::Deny),
                        ]),
                        external_directory: Action::Allow,
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
            tools: BTreeMap::from([("write".to_string(), true)]),
            permission: PermissionConfig {
                bash: BTreeMap::new(),
                external_directory: Action::Deny,
            },
        };
        let request = EvaluateToolCallRequest {
            session_id: bcode_session_models::SessionId::new(),
            agent_id: BUILD_AGENT.to_string(),
            tool_name: "filesystem.write".to_string(),
            side_effect: ToolSideEffect::WriteFiles,
            arguments: json!({ "path": "../outside.txt" }),
            cwd: Some("/tmp/project".to_string()),
        };

        let result = evaluate_tool_call(&config, &request, Path::new("/tmp/project"));

        assert_eq!(result.response.decision, AgentDecision::Deny);
    }
}
