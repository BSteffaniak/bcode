#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! default agent profile policy plugin.

use bcode_agent_policy::{
    AgentConfig, AgentPermissionConfig, BUILD_AGENT, PLAN_AGENT, active_tools_for, agent_config,
    default_config as policy_default_config, evaluate_tool_call,
};
use bcode_agent_profile::{
    AGENT_PROFILE_INTERFACE_ID, AgentContextRequest, AgentContextResponse, AgentInfo, AgentList,
    EvaluateToolCallRequest, OP_AGENT_CONTEXT, OP_EVALUATE_TOOL_CALL, OP_LIST_AGENTS,
    OP_POLICY_STATUS, PolicyStatusResponse,
};
use bcode_plugin_sdk::prelude::*;
use serde::Deserialize;
use std::collections::BTreeSet;
use std::path::PathBuf;
use toml::{Table, Value};

const MANIFEST: &str = include_str!("../bcode-plugin.toml");

#[derive(Debug, Deserialize)]
struct DefaultAgentsManifestExtension {
    agent_defaults: AgentDefaultsManifestExtension,
}

#[derive(Debug, Deserialize)]
struct AgentDefaultsManifestExtension {
    build_tools: Vec<String>,
    plan_disabled_tools: Vec<String>,
}

fn manifest_defaults() -> Result<AgentDefaultsManifestExtension, toml::de::Error> {
    toml::from_str::<DefaultAgentsManifestExtension>(MANIFEST)
        .map(|extension| extension.agent_defaults)
}

#[must_use]
pub fn default_config() -> AgentPermissionConfig {
    let (config, _) = default_config_with_diagnostics();
    config
}

fn default_config_with_diagnostics() -> (AgentPermissionConfig, Vec<String>) {
    match manifest_defaults() {
        Ok(defaults) => (default_config_with_defaults(defaults), Vec::new()),
        Err(error) => (
            policy_default_config(),
            vec![format!(
                "failed to parse plugin agent_defaults; using policy defaults without plugin tool enablement: {error}"
            )],
        ),
    }
}

fn default_config_with_defaults(defaults: AgentDefaultsManifestExtension) -> AgentPermissionConfig {
    let mut config = policy_default_config();
    if let Some(build) = config.agent.get_mut(BUILD_AGENT) {
        set_default_tools(build, &defaults.build_tools, true);
    }
    if let Some(plan) = config.agent.get_mut(PLAN_AGENT) {
        set_default_tools(plan, &defaults.build_tools, true);
        for tool_id in defaults.plan_disabled_tools {
            plan.tools.insert(tool_id, false);
        }
    }
    config
}

fn set_default_tools(agent: &mut AgentConfig, tool_ids: &[String], enabled: bool) {
    agent
        .tools
        .extend(tool_ids.iter().map(|tool_id| (tool_id.clone(), enabled)));
}

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
    let mut agent = agent_config(&config, &request.agent_id);
    let tools_config = load_tools_config();
    apply_tool_selection(&mut agent, &tools_config, &request.available_tools);
    let enabled_tools = Some(enabled_tools_from_available_metadata(
        active_tools_for(&agent),
        &request.available_tools,
    ));
    let system_prompt_suffix = Some(match request.agent_id.as_str() {
        PLAN_AGENT => "[PLAN AGENT ACTIVE]\n\nInspect, analyze, and plan only. Do not edit source files or intentionally modify project state. You may run shell commands allowed by the active permission policy, including validation commands such as `cargo check` or `cargo test`, even if they create normal build/cache artifacts. If implementation changes are needed, ask the user to switch to the build agent. When the user asks what text an image or screenshot says, use `ocr.extract` instead of `filesystem.read`.".to_string(),
        BUILD_AGENT => "[BUILD AGENT ACTIVE]\n\nImplementation is allowed subject to Bcode permissions, active agent policy, and project instructions. Use tools normally, keep changes focused, and report validation. When the user asks what text an image or screenshot says, use `ocr.extract` instead of `filesystem.read`.".to_string(),
        other => format!("[AGENT ACTIVE: {other}]\n\nFollow this agent's configured tool and permission policy."),
    });
    json_response(&AgentContextResponse {
        system_prompt_suffix,
        enabled_tools,
    })
}

fn enabled_tools_from_available_metadata(
    configured_enabled_tools: Vec<String>,
    available_tools: &[bcode_tool::ToolDefinition],
) -> Vec<String> {
    if available_tools.is_empty() {
        return configured_enabled_tools;
    }
    let available_tool_names = available_tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    configured_enabled_tools
        .into_iter()
        .filter(|tool_id| available_tool_names.contains(tool_id.as_str()))
        .collect()
}

fn load_tools_config() -> bcode_config::ToolsConfig {
    bcode_config::load_config()
        .map(|config| config.tools)
        .unwrap_or_default()
}

fn apply_tool_selection(
    agent: &mut AgentConfig,
    tools_config: &bcode_config::ToolsConfig,
    available_tools: &[bcode_tool::ToolDefinition],
) {
    let mut selected = match tools_config.default {
        bcode_config::ToolDefaultMode::Agent => active_tools_for(agent).into_iter().collect(),
        bcode_config::ToolDefaultMode::None => BTreeSet::new(),
        bcode_config::ToolDefaultMode::All => available_tools
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<BTreeSet<_>>(),
    };
    selected.extend(tools_config.enabled.iter().cloned());
    for tool_id in &tools_config.disabled {
        selected.remove(tool_id);
    }

    let mut known = agent.tools.keys().cloned().collect::<BTreeSet<_>>();
    known.extend(selected.iter().cloned());
    known.extend(tools_config.disabled.iter().cloned());
    if tools_config.default == bcode_config::ToolDefaultMode::All {
        known.extend(available_tools.iter().map(|tool| tool.name.clone()));
    }
    for tool_id in known {
        agent
            .tools
            .insert(tool_id.clone(), selected.contains(&tool_id));
    }
}

fn apply_tool_selection_for_evaluation(
    agent: &mut AgentConfig,
    tools_config: &bcode_config::ToolsConfig,
    requested_tool: &str,
) {
    apply_tool_selection(agent, tools_config, &[]);
    if tools_config.default == bcode_config::ToolDefaultMode::All
        && !tools_config.disabled.contains(requested_tool)
    {
        agent.tools.insert(requested_tool.to_string(), true);
    }
}

fn evaluate_tool(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<EvaluateToolCallRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    let (config, _) = load_config();
    let mut agent = agent_config(&config, &request.agent_id);
    let tools_config = load_tools_config();
    apply_tool_selection_for_evaluation(&mut agent, &tools_config, &request.tool_name);
    let cwd = request.cwd.as_deref().map_or_else(
        || std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        PathBuf::from,
    );
    let evaluation = evaluate_tool_call(&agent, &request, &cwd);
    json_response(&evaluation.response)
}

fn policy_status() -> PolicyStatusResponse {
    let (config, source) = load_config();
    let mut build = agent_config(&config, BUILD_AGENT);
    let mut plan = agent_config(&config, PLAN_AGENT);
    let tools_config = load_tools_config();
    apply_tool_selection(&mut build, &tools_config, &[]);
    apply_tool_selection(&mut plan, &tools_config, &[]);
    PolicyStatusResponse {
        using_default: source.using_default,
        source: source.label,
        build_enabled_tools: active_tools_for(&build),
        plan_enabled_tools: active_tools_for(&plan),
        diagnostics: source.diagnostics,
    }
}

#[derive(Debug, Clone)]
struct PolicySource {
    label: String,
    using_default: bool,
    diagnostics: Vec<String>,
}

fn load_config() -> (AgentPermissionConfig, PolicySource) {
    let (base_config, diagnostics) = default_config_with_diagnostics();
    let declarative = match bcode_config::load_composed_config_value() {
        Ok(value) => value,
        Err(error) => {
            eprintln!(
                "bcode.default-agents: failed to load declarative config ({error}); using built-in defaults"
            );
            return (
                base_config,
                PolicySource {
                    label: "built-in default agent policy".to_string(),
                    using_default: true,
                    diagnostics,
                },
            );
        }
    };

    let state = match bcode_config::load_permissions_state_value() {
        Ok(state) => state,
        Err(error) => {
            eprintln!(
                "bcode.default-agents: failed to load runtime permissions state ({error}); using declarative config only"
            );
            None
        }
    };

    let declarative_empty = agent_table_is_empty(&declarative);
    let state_empty = state.as_ref().is_none_or(agent_table_is_empty);

    if declarative_empty && state_empty {
        return (
            base_config,
            PolicySource {
                label: "built-in default agent policy".to_string(),
                using_default: true,
                diagnostics,
            },
        );
    }

    let mut merged = match Value::try_from(base_config.clone()) {
        Ok(value) => value,
        Err(error) => {
            eprintln!(
                "bcode.default-agents: failed to encode built-in agent policy ({error}); using built-in defaults"
            );
            return (
                base_config,
                PolicySource {
                    label: "built-in default agent policy".to_string(),
                    using_default: true,
                    diagnostics,
                },
            );
        }
    };
    bcode_config::merge_config_values(&mut merged, agent_only_config_value(&declarative));
    if let Some(state) = state.as_ref() {
        bcode_config::merge_config_values(&mut merged, agent_only_config_value(state));
    }

    let config = match merged.try_into() {
        Ok(config) => config,
        Err(error) => {
            eprintln!(
                "bcode.default-agents: failed to decode composed agent policy ({error}); using built-in defaults"
            );
            return (
                base_config,
                PolicySource {
                    label: "built-in default agent policy".to_string(),
                    using_default: true,
                    diagnostics,
                },
            );
        }
    };

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
        config,
        PolicySource {
            label,
            using_default: false,
            diagnostics,
        },
    )
}

fn agent_table_is_empty(value: &Value) -> bool {
    value
        .get("agent")
        .and_then(Value::as_table)
        .is_none_or(Table::is_empty)
}

fn agent_only_config_value(value: &Value) -> Value {
    let Some(agent) = value.get("agent").cloned() else {
        return Value::Table(Table::new());
    };
    let mut root = Table::new();
    root.insert("agent".to_string(), agent);
    Value::Table(root)
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
    fn manifest_agent_defaults_are_valid() {
        let defaults = manifest_defaults().expect("plugin manifest defaults should parse");

        assert!(!defaults.build_tools.is_empty());
        for disabled_tool in &defaults.plan_disabled_tools {
            assert!(
                defaults.build_tools.contains(disabled_tool),
                "plan-disabled tool {disabled_tool} should be declared in build defaults"
            );
        }
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
            policy: bcode_tool::ToolPolicyMetadata {
                aliases: Vec::new(),
                compatibility_aliases: Vec::new(),
                capabilities: Vec::new(),
                permission_category: Some("command".to_string()),
                argument_extractors: vec![bcode_tool::ToolArgumentExtractor {
                    kind: bcode_tool::ToolArgumentKind::Command,
                    argument: "command".to_string(),
                }],
            },
            arguments: json!({ "command": "git diff && git commit -m nope" }),
            cwd: Some("/tmp/project".to_string()),
        };

        let result = evaluate_tool_call(&agent, &request, Path::new("/tmp/project"));

        assert_eq!(result.response.decision, AgentDecision::Deny);
    }

    #[test]
    fn default_plan_tools_are_plugin_owned_and_disable_writes() {
        let config = default_config();
        let plan = agent_config(&config, PLAN_AGENT);
        let tools = active_tools_for(&plan);

        assert!(tools.contains(&"filesystem.read".to_string()));
        assert!(tools.contains(&"filesystem.list".to_string()));
        assert!(tools.contains(&"filesystem.find".to_string()));
        assert!(tools.contains(&"filesystem.grep".to_string()));
        assert!(tools.contains(&"filesystem.stat".to_string()));
        assert!(!tools.contains(&"filesystem.write".to_string()));
        assert!(!tools.contains(&"filesystem.edit".to_string()));
    }

    #[test]
    fn runtime_permission_state_preserves_build_default_tools_and_merges_agent_metadata() {
        let _guard = ENV_LOCK.lock().expect("env lock should not be poisoned");
        let root = unique_temp_dir();
        std::fs::create_dir_all(&root).expect("temp root should be created");
        let config_path = root.join("bcode.toml");
        let state_path = root.join("permissions.toml");
        std::fs::write(
            &config_path,
            r##"
[agent.build]
accent = "#22d3ee"
"##,
        )
        .expect("config should be written");
        std::fs::write(
            &state_path,
            r#"
[agent.build.permission]
command = { "python3 *" = "allow" }
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
        assert_eq!(build.accent.as_deref(), Some("#22d3ee"));
        assert_eq!(
            build.permission.command.get("python3 *"),
            Some(&Action::Allow)
        );
        assert!(matches!(
            source.label.as_str(),
            "built-in default agent policy + runtime permissions state"
                | "built-in default agent policy + bcode.toml [agent] + runtime permissions state"
        ));
        assert!(!source.using_default);

        restore_env("BCODE_CONFIG", previous_config);
        restore_env("BCODE_PERMISSIONS_STATE", previous_state);
    }

    #[test]
    fn tool_selection_none_exposes_only_explicit_tools() {
        let mut agent = agent_config(&default_config(), BUILD_AGENT);
        let tools_config = bcode_config::ToolsConfig {
            default: bcode_config::ToolDefaultMode::None,
            enabled: BTreeSet::from(["filesystem.read".to_string()]),
            disabled: BTreeSet::new(),
            ..bcode_config::ToolsConfig::default()
        };

        apply_tool_selection(&mut agent, &tools_config, &[]);
        let tools = active_tools_for(&agent);

        assert_eq!(tools, vec!["filesystem.read".to_string()]);
    }

    #[test]
    fn tool_selection_disabled_wins_over_enabled_and_agent_defaults() {
        let mut agent = agent_config(&default_config(), BUILD_AGENT);
        let tools_config = bcode_config::ToolsConfig {
            default: bcode_config::ToolDefaultMode::Agent,
            enabled: BTreeSet::from(["shell.run".to_string()]),
            disabled: BTreeSet::from(["shell.run".to_string()]),
            ..bcode_config::ToolsConfig::default()
        };

        apply_tool_selection(&mut agent, &tools_config, &[]);
        let tools = active_tools_for(&agent);

        assert!(!tools.contains(&"shell.run".to_string()));
    }

    #[test]
    fn tool_selection_all_uses_available_tool_metadata() {
        let mut agent = agent_config(&default_config(), BUILD_AGENT);
        let tools_config = bcode_config::ToolsConfig {
            default: bcode_config::ToolDefaultMode::All,
            enabled: BTreeSet::new(),
            disabled: BTreeSet::from(["filesystem.write".to_string()]),
            ..bcode_config::ToolsConfig::default()
        };
        let available_tools = vec![
            bcode_tool::ToolDefinition {
                name: "filesystem.read".to_string(),
                description: String::new(),
                input_schema: json!({}),
                side_effect: ToolSideEffect::ReadOnly,
                requires_permission: false,
                policy: bcode_tool::ToolPolicyMetadata::default(),
                ui: bcode_tool::ToolUiMetadata::default(),
            },
            bcode_tool::ToolDefinition {
                name: "filesystem.write".to_string(),
                description: String::new(),
                input_schema: json!({}),
                side_effect: ToolSideEffect::WriteFiles,
                requires_permission: true,
                policy: bcode_tool::ToolPolicyMetadata::default(),
                ui: bcode_tool::ToolUiMetadata::default(),
            },
        ];

        apply_tool_selection(&mut agent, &tools_config, &available_tools);
        let tools = active_tools_for(&agent);

        assert!(tools.contains(&"filesystem.read".to_string()));
        assert!(!tools.contains(&"filesystem.write".to_string()));
        assert!(!tools.contains(&"shell.run".to_string()));
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
