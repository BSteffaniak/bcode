#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Bundled default agent profile policy plugin.

use bcode_agent_policy::{
    AgentPermissionConfig, BUILD_AGENT, PLAN_AGENT, active_tools_for, agent_config, default_config,
    evaluate_tool_call,
};
use bcode_agent_profile::{
    AGENT_PROFILE_INTERFACE_ID, AgentContextRequest, AgentContextResponse, AgentInfo, AgentList,
    EvaluateToolCallRequest, OP_AGENT_CONTEXT, OP_EVALUATE_TOOL_CALL, OP_LIST_AGENTS,
    OP_POLICY_STATUS, PolicyStatusResponse,
};
use bcode_plugin_sdk::prelude::*;
use std::path::{Path, PathBuf};

const MANIFEST: &str = include_str!("../bcode-plugin.toml");

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
            OP_EVALUATE_TOOL_CALL => evaluate_tool(&context.request),
            OP_POLICY_STATUS => json_response(&policy_status()),
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
    let (config, _) = load_config();
    let agent = agent_config(&config, &request.agent_id);
    let enabled_tools = Some(active_tools_for(&agent));
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

fn evaluate_tool(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<EvaluateToolCallRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    let (config, _) = load_config();
    let agent = agent_config(&config, &request.agent_id);
    let cwd = request.cwd.as_deref().map_or_else(
        || std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        PathBuf::from,
    );
    let evaluation = evaluate_tool_call(&agent, &request, &cwd);
    json_response(&evaluation.response)
}

fn policy_status() -> PolicyStatusResponse {
    let (_, source) = load_config();
    PolicyStatusResponse {
        using_default: source.using_default,
        source: source.label,
    }
}

#[derive(Debug, Clone)]
struct PolicySource {
    label: String,
    using_default: bool,
}

fn load_config() -> (AgentPermissionConfig, PolicySource) {
    if let Some(path) = config_path()
        && let Ok(contents) = std::fs::read_to_string(&path)
        && let Ok(config) = serde_json::from_str::<AgentPermissionConfig>(&contents)
        && !config.agent.is_empty()
    {
        return (
            config,
            PolicySource {
                label: path.display().to_string(),
                using_default: false,
            },
        );
    }
    (
        default_config(),
        PolicySource {
            label: "built-in default agent policy".to_string(),
            using_default: true,
        },
    )
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
    use bcode_agent_profile::AgentDecision;
    use bcode_session_models::SessionId;
    use bcode_tool::ToolSideEffect;
    use serde_json::json;

    #[test]
    fn list_agents_contains_plan_and_build() {
        let agents = agent_list().agents;

        assert!(agents.iter().any(|agent| agent.id == PLAN_AGENT));
        assert!(agents.iter().any(|agent| agent.id == BUILD_AGENT));
    }

    #[test]
    fn plan_denies_unlisted_shell_command_parts() {
        let (config, _) = load_config();
        let agent = agent_config(&config, PLAN_AGENT);
        let request = EvaluateToolCallRequest {
            session_id: SessionId::new(),
            agent_id: PLAN_AGENT.to_string(),
            tool_name: "shell.run".to_string(),
            side_effect: ToolSideEffect::ExecuteProcess,
            arguments: json!({ "command": "git diff && git commit -m nope" }),
            cwd: Some("/tmp/project".to_string()),
        };

        let result = evaluate_tool_call(&agent, &request, Path::new("/tmp/project"));

        assert_eq!(result.response.decision, AgentDecision::Deny);
    }
}
