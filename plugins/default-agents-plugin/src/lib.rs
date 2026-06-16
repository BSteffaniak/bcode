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
use std::path::PathBuf;

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
    let (config, _) = load_config();
    let plan = agent_config(&config, PLAN_AGENT);
    let build = agent_config(&config, BUILD_AGENT);
    AgentList {
        agents: vec![
            AgentInfo {
                id: PLAN_AGENT.to_string(),
                name: "Plan".to_string(),
                description: "Read-only analysis agent with Pi/OpenCode-style command policy"
                    .to_string(),
                badge: Some("plan".to_string()),
                accent: plan.accent,
                aliases: vec!["plan".to_string()],
                is_default: false,
            },
            AgentInfo {
                id: BUILD_AGENT.to_string(),
                name: "Build".to_string(),
                description: "Implementation agent with normal Bcode permission checkpoints"
                    .to_string(),
                badge: Some("build".to_string()),
                accent: build.accent,
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
        PLAN_AGENT => "[PLAN AGENT ACTIVE]\n\nInspect, analyze, and plan only. Do not edit source files or intentionally modify project state. You may run shell commands allowed by the active permission policy, including validation commands such as `cargo check` or `cargo test`, even if they create normal build/cache artifacts. If implementation changes are needed, ask the user to switch to the build agent.".to_string(),
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
    let declarative = match bcode_config::load_config() {
        Ok(cfg) => cfg,
        Err(error) => {
            eprintln!(
                "bcode.default-agents: failed to load declarative config ({error}); using built-in defaults"
            );
            return (
                default_config(),
                PolicySource {
                    label: "built-in default agent policy".to_string(),
                    using_default: true,
                },
            );
        }
    };

    let state = match bcode_config::load_permissions_state() {
        Ok(state) => state,
        Err(error) => {
            eprintln!(
                "bcode.default-agents: failed to load runtime permissions state ({error}); using declarative config only"
            );
            std::collections::BTreeMap::new()
        }
    };

    let declarative_empty = declarative.agent.is_empty();
    let state_empty = state.is_empty();

    if declarative_empty && state_empty {
        return (
            default_config(),
            PolicySource {
                label: "built-in default agent policy".to_string(),
                using_default: true,
            },
        );
    }

    let mut agents = default_config().agent;
    if !declarative_empty {
        bcode_config::merge_agent_configs(&mut agents, declarative.agent);
    }
    if !state_empty {
        bcode_config::merge_agent_configs(&mut agents, state);
    }

    let label = match (declarative_empty, state_empty) {
        (false, false) => {
            "built-in default agent policy + bcode.toml [agent] + runtime permissions state"
                .to_string()
        }
        (false, true) => "built-in default agent policy + bcode.toml [agent]".to_string(),
        (true, false) => "built-in default agent policy + runtime permissions state".to_string(),
        (true, true) => unreachable!("handled above"),
    };

    (
        AgentPermissionConfig { agent: agents },
        PolicySource {
            label,
            using_default: false,
        },
    )
}

fn json_response<T: serde::Serialize>(value: &T) -> ServiceResponse {
    ServiceResponse::json(value)
        .unwrap_or_else(|error| ServiceResponse::error("serialization_failed", error.to_string()))
}

fn invalid_request(error: &serde_json::Error) -> ServiceResponse {
    ServiceResponse::error("invalid_request", error.to_string())
}

export_plugin!(DefaultAgentsPlugin, MANIFEST);

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_plugin_vtable!(
        DefaultAgentsPlugin,
        include_str!("../bcode-plugin.toml")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_agent_policy::Action;
    use bcode_agent_profile::AgentDecision;
    use bcode_session_models::SessionId;
    use bcode_tool::ToolSideEffect;
    use serde_json::json;
    use std::path::Path;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

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

    #[test]
    fn runtime_permission_state_preserves_build_default_tools() {
        let _guard = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let root = unique_temp_dir();
        std::fs::create_dir_all(&root).expect("temp root should be created");
        let config_path = root.join("bcode.toml");
        let state_path = root.join("permissions.toml");
        std::fs::write(&config_path, "").expect("config should be written");
        std::fs::write(
            &state_path,
            r#"
[agent.build.permission]
bash = { "python3 *" = "allow" }
"#,
        )
        .expect("state should be written");
        let previous_config = std::env::var_os("BCODE_CONFIG");
        let previous_state = std::env::var_os("BCODE_PERMISSIONS_STATE");
        unsafe {
            std::env::set_var("BCODE_CONFIG", &config_path);
            std::env::set_var("BCODE_PERMISSIONS_STATE", &state_path);
        }

        let (config, source) = load_config();
        let build = agent_config(&config, BUILD_AGENT);
        let tools = active_tools_for(&build);

        assert!(tools.contains(&"filesystem.write".to_string()));
        assert!(tools.contains(&"filesystem.edit".to_string()));
        assert!(tools.contains(&"shell.run".to_string()));
        assert_eq!(build.permission.bash.get("python3 *"), Some(&Action::Allow));
        assert!(matches!(
            source.label.as_str(),
            "built-in default agent policy + runtime permissions state"
                | "built-in default agent policy + bcode.toml [agent] + runtime permissions state"
        ));
        assert!(!source.using_default);

        restore_env("BCODE_CONFIG", previous_config);
        restore_env("BCODE_PERMISSIONS_STATE", previous_state);
    }

    fn restore_env(name: &str, value: Option<std::ffi::OsString>) {
        unsafe {
            if let Some(value) = value {
                std::env::set_var(name, value);
            } else {
                std::env::remove_var(name);
            }
        }
    }

    fn unique_temp_dir() -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("bcode-default-agents-test-{nanos}"))
    }
}
