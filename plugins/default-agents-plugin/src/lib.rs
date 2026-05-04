#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Bundled default agent profile policy plugin.

use bcode_agent_profile::{
    AGENT_PROFILE_INTERFACE_ID, AgentContextRequest, AgentContextResponse, AgentDecision,
    AgentInfo, AgentList, EvaluateToolCallRequest, EvaluateToolCallResponse, OP_AGENT_CONTEXT,
    OP_EVALUATE_TOOL_CALL, OP_LIST_AGENTS,
};
use bcode_plugin_sdk::prelude::*;
use bcode_tool::ToolSideEffect;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const MANIFEST: &str = include_str!("../bcode-plugin.toml");
const PLAN_AGENT: &str = "plan";
const BUILD_AGENT: &str = "build";

/// Default plan/build agent profile plugin.
#[derive(Default)]
pub struct DefaultAgentsPlugin;

impl RustPlugin for DefaultAgentsPlugin {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        if context.request.interface_id != AGENT_PROFILE_INTERFACE_ID {
            return ServiceResponse::error(
                "unsupported_interface",
                "unsupported agent profile service interface",
            );
        }
        match context.request.operation.as_str() {
            OP_LIST_AGENTS => json_response(&agent_list()),
            OP_AGENT_CONTEXT => agent_context(&context.request),
            OP_EVALUATE_TOOL_CALL => evaluate_tool_call(&context.request),
            _ => ServiceResponse::error(
                "unsupported_operation",
                "unsupported agent profile operation",
            ),
        }
    }
}

fn agent_list() -> AgentList {
    AgentList {
        agents: vec![
            AgentInfo {
                id: PLAN_AGENT.to_string(),
                name: "Plan".to_string(),
                description: "Read-only analysis agent with Pi/OpenCode-style command policy"
                    .to_string(),
                badge: Some("plan".to_string()),
                aliases: vec!["plan".to_string()],
                is_default: false,
            },
            AgentInfo {
                id: BUILD_AGENT.to_string(),
                name: "Build".to_string(),
                description: "Implementation agent with normal Bcode permission checkpoints"
                    .to_string(),
                badge: Some("build".to_string()),
                aliases: vec!["build".to_string()],
                is_default: true,
            },
        ],
    }
}

fn agent_context(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<AgentContextRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    let config = load_config();
    let mode = mode_config(&config, &request.agent_id);
    let enabled_tools = Some(active_tools_for(&mode));
    let system_prompt_suffix = Some(match request.agent_id.as_str() {
        PLAN_AGENT => "[PLAN AGENT ACTIVE]\n\nInspect, analyze, and plan only. You may use read-only tools and explicitly allowed read-only shell commands. Do not edit files, write files, or run mutating commands. If implementation is needed, ask the user to switch to the build agent.".to_string(),
        BUILD_AGENT => "[BUILD AGENT ACTIVE]\n\nImplementation is allowed subject to Bcode permissions, active agent policy, and project instructions. Use tools normally, keep changes focused, and report validation.".to_string(),
        other => format!("[AGENT ACTIVE: {other}]\n\nFollow this agent's configured tool and permission policy."),
    });
    json_response(&AgentContextResponse {
        system_prompt_suffix,
        enabled_tools,
    })
}

fn evaluate_tool_call(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<EvaluateToolCallRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    let config = load_config();
    let mode = mode_config(&config, &request.agent_id);
    let response = evaluate_policy(&mode, &request);
    json_response(&response)
}

fn evaluate_policy(
    config: &ModeConfig,
    request: &EvaluateToolCallRequest,
) -> EvaluateToolCallResponse {
    if tool_enabled(config, request) == Some(false) {
        return deny(format!(
            "{} agent disabled tool {}",
            request.agent_id, request.tool_name
        ));
    }

    if request.tool_name == "shell.run" {
        return evaluate_shell(config, request);
    }

    match request.side_effect {
        ToolSideEffect::ReadOnly => EvaluateToolCallResponse {
            decision: AgentDecision::Allow,
            reason: None,
        },
        ToolSideEffect::WriteFiles | ToolSideEffect::ExecuteProcess => {
            if tool_enabled(config, request) == Some(true) {
                EvaluateToolCallResponse {
                    decision: AgentDecision::Ask,
                    reason: Some(format!(
                        "{} agent requires permission for {}",
                        request.agent_id, request.tool_name
                    )),
                }
            } else {
                deny(format!(
                    "{} agent denied mutating tool {}; switch agents if implementation is needed",
                    request.agent_id, request.tool_name
                ))
            }
        }
    }
}

fn evaluate_shell(
    config: &ModeConfig,
    request: &EvaluateToolCallRequest,
) -> EvaluateToolCallResponse {
    let Some(command) = string_argument(&request.arguments, "command") else {
        return deny(format!(
            "{} agent denied shell command with missing command",
            request.agent_id
        ));
    };
    let rules = compile_rules(config);
    if let Some(denied) = denied_command_part(command, &rules) {
        return match denied.action {
            Action::Allow => EvaluateToolCallResponse {
                decision: AgentDecision::Allow,
                reason: None,
            },
            Action::Ask => EvaluateToolCallResponse {
                decision: AgentDecision::Ask,
                reason: Some(format!(
                    "{} agent asks before shell command: {}",
                    request.agent_id, denied.command
                )),
            },
            Action::Deny => deny(format!(
                "{} agent denied shell command '{}'{}",
                request.agent_id,
                denied.command,
                denied
                    .rule
                    .map_or_else(String::new, |rule| format!(" by rule '{}'", rule.pattern))
            )),
        };
    }
    EvaluateToolCallResponse {
        decision: AgentDecision::Allow,
        reason: None,
    }
}

const fn deny(reason: String) -> EvaluateToolCallResponse {
    EvaluateToolCallResponse {
        decision: AgentDecision::Deny,
        reason: Some(reason),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Action {
    Allow,
    Deny,
    Ask,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ModeConfig {
    #[serde(default)]
    tools: BTreeMap<String, bool>,
    #[serde(default)]
    permission: PermissionConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct PermissionConfig {
    #[serde(default)]
    bash: BTreeMap<String, Action>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct AgentPermissionConfig {
    #[serde(default)]
    agent: BTreeMap<String, ModeConfig>,
}

#[derive(Debug, Clone)]
struct Rule {
    pattern: String,
    action: Action,
    specificity: usize,
}

#[derive(Debug, Clone)]
struct DeniedCommandPart<'a> {
    command: &'a str,
    rule: Option<Rule>,
    action: Action,
}

fn load_config() -> AgentPermissionConfig {
    config_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|contents| serde_json::from_str::<AgentPermissionConfig>(&contents).ok())
        .filter(|config| !config.agent.is_empty())
        .unwrap_or_else(default_config)
}

fn config_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("BCODE_AGENT_PERMISSIONS") {
        return Some(PathBuf::from(path));
    }
    std::env::var_os("HOME").map(|home| {
        Path::new(&home)
            .join(".pi")
            .join("agent")
            .join("opencode-permissions.json")
    })
}

fn default_config() -> AgentPermissionConfig {
    let mut agent = BTreeMap::new();
    agent.insert(
        BUILD_AGENT.to_string(),
        ModeConfig {
            tools: BTreeMap::from([
                ("shell.run".to_string(), true),
                ("filesystem.read".to_string(), true),
                ("filesystem.write".to_string(), true),
                ("filesystem.edit".to_string(), true),
            ]),
            permission: PermissionConfig {
                bash: BTreeMap::from([("*".to_string(), Action::Ask)]),
            },
        },
    );
    agent.insert(
        PLAN_AGENT.to_string(),
        ModeConfig {
            tools: BTreeMap::from([
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
            },
        },
    );
    AgentPermissionConfig { agent }
}

fn mode_config(config: &AgentPermissionConfig, agent_id: &str) -> ModeConfig {
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

fn active_tools_for(config: &ModeConfig) -> Vec<String> {
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

fn tool_enabled(config: &ModeConfig, request: &EvaluateToolCallRequest) -> Option<bool> {
    let aliases = tool_aliases(&request.tool_name);
    aliases
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
        _ => {}
    }
    aliases
}

fn normalize_tool_names(tool: &str) -> Vec<String> {
    match tool {
        "bash" => vec!["shell.run".to_string()],
        "read" | "grep" | "find" | "ls" => {
            vec![
                "filesystem.read".to_string(),
                "filesystem.exists".to_string(),
            ]
        }
        "write" => vec!["filesystem.write".to_string()],
        "edit" => vec!["filesystem.edit".to_string()],
        other => vec![other.to_string()],
    }
}

fn compile_rules(config: &ModeConfig) -> Vec<Rule> {
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

fn command_parts(command: &str) -> Vec<&str> {
    let parts = command
        .split([';', '|'])
        .flat_map(|part| part.split("&&"))
        .flat_map(|part| part.split("||"))
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if parts.is_empty() {
        vec![command]
    } else {
        parts
    }
}

fn matching_rule(command: &str, rules: &[Rule]) -> Option<Rule> {
    rules
        .iter()
        .filter(|rule| glob_matches(&rule.pattern, command))
        .max_by_key(|rule| (rule.specificity, rule.pattern.len()))
        .cloned()
}

fn glob_matches(pattern: &str, value: &str) -> bool {
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

fn rule_specificity(pattern: &str) -> usize {
    let exact_bonus = if pattern.contains('*') { 0 } else { 1_000 };
    exact_bonus + pattern.chars().filter(|char| *char != '*').count()
}

fn string_argument<'a>(arguments: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    arguments.get(key).and_then(serde_json::Value::as_str)
}

fn json_response<T: serde::Serialize>(value: &T) -> ServiceResponse {
    ServiceResponse::json(value)
        .unwrap_or_else(|error| ServiceResponse::error("serialization_failed", error.to_string()))
}

fn invalid_request(error: &serde_json::Error) -> ServiceResponse {
    ServiceResponse::error("invalid_request", error.to_string())
}

export_plugin!(DefaultAgentsPlugin, MANIFEST);

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_models::SessionId;
    use serde_json::json;

    #[test]
    fn glob_rules_pick_most_specific_match() {
        let rules = vec![
            Rule {
                pattern: "*".to_string(),
                action: Action::Deny,
                specificity: 0,
            },
            Rule {
                pattern: "git diff *".to_string(),
                action: Action::Allow,
                specificity: rule_specificity("git diff *"),
            },
        ];

        assert_eq!(
            matching_rule("git diff HEAD", &rules).map(|rule| rule.action),
            Some(Action::Allow)
        );
    }

    #[test]
    fn plan_denies_unlisted_shell_command_parts() {
        let config = default_config();
        let mode = mode_config(&config, PLAN_AGENT);
        let request = EvaluateToolCallRequest {
            session_id: SessionId::new(),
            agent_id: PLAN_AGENT.to_string(),
            tool_name: "shell.run".to_string(),
            side_effect: ToolSideEffect::ExecuteProcess,
            arguments: json!({ "command": "git diff && git commit -m nope" }),
        };

        let response = evaluate_policy(&mode, &request);

        assert_eq!(response.decision, AgentDecision::Deny);
        assert!(
            response
                .reason
                .is_some_and(|reason| reason.contains("git commit"))
        );
    }

    #[test]
    fn build_asks_for_shell_by_default() {
        let config = default_config();
        let mode = mode_config(&config, BUILD_AGENT);
        let request = EvaluateToolCallRequest {
            session_id: SessionId::new(),
            agent_id: BUILD_AGENT.to_string(),
            tool_name: "shell.run".to_string(),
            side_effect: ToolSideEffect::ExecuteProcess,
            arguments: json!({ "command": "cargo check" }),
        };

        let response = evaluate_policy(&mode, &request);

        assert_eq!(response.decision, AgentDecision::Ask);
    }
}
