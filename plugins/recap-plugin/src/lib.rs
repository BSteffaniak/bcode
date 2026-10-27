#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
//! Direct Rust integration for grounded visual recaps; no recap subprocess.
mod diagnostics;
mod model;
mod operations;
mod process_admission;

use bcode_command::{
    CommandAction, CommandArgumentContribution, CommandArgumentKind, CommandContribution,
    CommandExecution, CommandOwner, CommandSessionRequirement, CommandSurface,
    InvokeCommandRequest, InvokeCommandResponse, SlashCommandContribution,
};
use bcode_plugin_sdk::prelude::*;
use bcode_plugin_sdk::{ToolPolicyOperation, ToolPolicyPreparation};
use bcode_tool::{
    ToolDefinition, ToolDiscoveryPolicy, ToolInvocationRequest, ToolInvocationResponse, ToolList,
};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

/// Plugin identity.
pub const PLUGIN_ID: &str = "bcode.recap";
/// Direct library-backed recap operations.
const OPERATIONS: [&str; 4] = ["create", "refine", "export", "inspect"];

/// User commands remain available when model-callable recap tools are disabled.
#[must_use]
pub fn commands() -> Vec<CommandContribution> {
    OPERATIONS
        .into_iter()
        .map(|operation| {
            let id = format!("recap.{operation}");
            let mut arguments = vec![CommandArgumentContribution {
                name: "id".into(),
                kind: CommandArgumentKind::String,
                required: true,
                description: Some("Stable recap identity".into()),
            }];
            for name in match operation {
                "create" => vec!["source", "focus", "transcript"],
                "refine" => vec!["focus"],
                _ => vec![],
            } {
                arguments.push(CommandArgumentContribution {
                    name: name.into(),
                    kind: CommandArgumentKind::String,
                    required: name == "source" || operation == "refine",
                    description: None,
                });
            }
            CommandContribution {
                id: id.clone(),
                title: format!("Recap: {operation}"),
                description: Some(
                    "Create or refine a visual recap; export one offline HTML file".into(),
                ),
                category: Some("recap".into()),
                surfaces: BTreeSet::from([CommandSurface::Palette, CommandSurface::Slash]),
                slash: Some(SlashCommandContribution {
                    name: format!("recap-{operation}"),
                    aliases: BTreeSet::new(),
                }),
                arguments,
                session: CommandSessionRequirement::Optional,
                execution: CommandExecution::Normal,
                owner: CommandOwner::Plugin {
                    plugin_id: PLUGIN_ID.into(),
                },
                action: CommandAction::Plugin {
                    plugin_id: PLUGIN_ID.into(),
                    command_id: id,
                },
            }
        })
        .collect()
}
fn definitions() -> Vec<ToolDefinition> {
    OPERATIONS.into_iter().map(|operation| {
        let mut properties = json!({"id":{"type":"string","description":"Stable recap identity"}});
        let mut required = vec!["id"];
        if operation == "create" { properties["source"] = json!({"type":"string","description":"Local video within the canonical workspace, or a YouTube URL"}); properties["transcript"] = json!({"type":"string","description":"Optional workspace-confined VTT/SRT file"}); required.push("source"); }
        if matches!(operation, "create" | "refine") { properties["focus"] = json!({"type":"string"}); }
        if operation == "refine" { required.push("focus"); }
        ToolDefinition { name: format!("recap.{operation}"), description: format!("{operation} a grounded visual recap using direct Rust library execution. Create/refine require explicitly admitted model-host inference. Local files stay within the canonical workspace; recap data lives in .recap."), input_schema: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}) }
    }).collect()
}
fn response(value: &impl serde::Serialize) -> ServiceResponse {
    serde_json::to_vec(value).map_or_else(
        |_| ServiceResponse::error("encode_failed", "recap result encoding failed"),
        ServiceResponse::ok,
    )
}
/// Bundled or dynamically loaded recap integration.
#[derive(Default)]
pub struct RecapPlugin;
impl RustPlugin for RecapPlugin {
    fn register_commands(&mut self, registrar: CommandRegistrar) -> Result<(), PluginError> {
        for command in commands() {
            registrar
                .register(&command)
                .map_err(|_| PluginError::failed("recap command registration failed"))?;
        }
        Ok(())
    }
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        match (
            context.request.interface_id.as_str(),
            context.request.operation.as_str(),
        ) {
            (bcode_command::COMMAND_INTERFACE_ID, bcode_command::OP_INVOKE_COMMAND) => {
                let Ok(request) = context.request.payload_json::<InvokeCommandRequest>() else {
                    return ServiceResponse::error("invalid_request", "invalid recap command");
                };
                let Some(command_context) = request.context else {
                    return ServiceResponse::error(
                        "context_required",
                        "recap requires canonical workspace context",
                    );
                };
                let result = operations::run(
                    &context,
                    &request.command_id,
                    &request.command_id,
                    &request.args,
                    &command_context.working_directory,
                );
                response(&InvokeCommandResponse {
                    success: result.is_ok(),
                    message: Some(result.unwrap_or_else(|error| error)),
                    updated_model: None,
                    updated_provider: None,
                    updated_thinking: None,
                    effects: Vec::new(),
                })
            }
            (bcode_tool::TOOL_SERVICE_INTERFACE_ID, bcode_tool::OP_LIST_TOOLS) => response(
                &ToolList::with_runtime_stable_discovery(definitions(), |definition| {
                    ToolDiscoveryPolicy {
                        version: 1,
                        read_only: definition.name == "recap.inspect",
                        capabilities: Vec::new(),
                    }
                }),
            ),
            (bcode_tool::TOOL_SERVICE_INTERFACE_ID, bcode_tool::OP_PREPARE_TOOL) => {
                prepare_tool_service_response(
                    &context.request,
                    definitions(),
                    |request, definition| {
                        let root = operations::preparation_root(request)?;
                        let args: BTreeMap<String, String> =
                            serde_json::from_value(request.invocation.arguments.clone())
                                .map_err(|_| "invalid recap arguments")?;
                        operations::validate(&definition.name, &args, &root)?;
                        let mut facts =
                            operations::policy_operations(&definition.name, &args, &root)?;
                        let operation = if facts.len() == 1 {
                            facts.remove(0)
                        } else {
                            ToolPolicyOperation::Compound { operations: facts }
                        };
                        Ok(ToolPolicyPreparation::new(
                            definition.name != "recap.inspect",
                            operation,
                        )
                        .with_descriptor(json!({"root":root})))
                    },
                )
            }
            (bcode_tool::TOOL_SERVICE_INTERFACE_ID, bcode_tool::OP_INVOKE_TOOL) => {
                let Ok(request) = context.request.payload_json::<ToolInvocationRequest>() else {
                    return ServiceResponse::error("invalid_request", "invalid recap tool");
                };
                let Some(root) = request
                    .preparation_descriptor
                    .get("root")
                    .and_then(serde_json::Value::as_str)
                else {
                    return ServiceResponse::error(
                        "preparation_required",
                        "recap tool requires authorized preparation",
                    );
                };
                let Ok(args) = serde_json::from_value(request.arguments) else {
                    return ServiceResponse::error("invalid_request", "invalid recap arguments");
                };
                let result = operations::run(
                    &context,
                    &request.name,
                    &request.tool_call_id,
                    &args,
                    std::path::Path::new(root),
                );
                response(&ToolInvocationResponse {
                    is_error: result.is_err(),
                    output: result.unwrap_or_else(|error| error),
                    content: Vec::new(),
                    full_output: None,
                    result: None,
                })
            }
            _ => ServiceResponse::error("unsupported_operation", "unsupported recap operation"),
        }
    }
}
/// Static registration; loading selection controls activation and command/tool discovery.
#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_plugin_vtable!(RecapPlugin, include_str!("../bcode-plugin.toml"))
}
#[cfg(not(feature = "static-bundled"))]
bcode_plugin_sdk::export_plugin!(RecapPlugin, include_str!("../bcode-plugin.toml"));

#[cfg(test)]
mod tests {
    #[test]
    fn commands_and_tools_are_portable_and_namespaced() {
        let commands = super::commands();
        assert_eq!(commands.len(), 4);
        assert!(commands.iter().all(|command| command.slash.is_some()));
        let tools = super::definitions();
        assert_eq!(tools.len(), 4);
        assert!(
            tools
                .iter()
                .all(|tool| tool.input_schema["additionalProperties"] == false)
        );
    }
}
