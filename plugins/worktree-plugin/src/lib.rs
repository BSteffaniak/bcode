#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Git worktree tool plugin for Bcode.

#[cfg(feature = "static-bundled")]
mod cli;

use bcode_command::{
    COMMAND_INTERFACE_ID, CommandAction, CommandContribution, CommandEffect, CommandOwner,
    CommandSurface, InvokeCommandRequest, InvokeCommandResponse, OP_INVOKE_COMMAND,
};
use bcode_plugin_sdk::path::display;
use bcode_plugin_sdk::prelude::*;
use bcode_tool::{
    ListToolsRequest, OP_INVOKE_TOOL, OP_LIST_TOOLS, TOOL_SERVICE_INTERFACE_ID, ToolArtifact,
    ToolDefinition, ToolInvocationRequest, ToolInvocationResponse, ToolInvocationResult, ToolList,
};
use bcode_tui_components::tool_card::{ToolCardStyle, push_tool_card_detail, tool_card_header};
use bcode_worktree_models::{
    WorktreeCreateRequest, WorktreeInfo, WorktreeListRequest, WorktreeRemoveRequest,
};
use bmux_keyboard::KeyCode;
use bmux_text_edit::TextEditBuffer;
use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::style::{Modifier, Style};
use bmux_tui::text::{Line, Span};
use bmux_tui_components::key_hint_bar::{KeyHint, KeyHintBar, KeyHintBarStyles};
use bmux_tui_components::selectable_list::{
    SelectableList, SelectableListItem, SelectableListOutcome, SelectableListState,
    SelectableListStyles,
};
use bmux_tui_components::text_input::{TextInputPolicy, TextInputState};
use bmux_tui_components::text_input_box::{
    TextInputBox, TextInputBoxOutcome, TextInputBoxPolicy, TextInputBoxStyles,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::PathBuf;
use std::str::FromStr;

const WORKTREE_PLUGIN_ID: &str = "bcode.worktree";
const WORKTREE_REQUEST_SCHEMA: &str = "bcode.worktree.request";
const WORKTREE_LIST_SCHEMA: &str = "bcode.worktree.list";
const WORKTREE_CREATE_SCHEMA: &str = "bcode.worktree.create_result";
const WORKTREE_REMOVE_SCHEMA: &str = "bcode.worktree.remove_result";

/// worktree plugin.
#[derive(Default)]
pub struct WorktreePlugin;

impl RustPlugin for WorktreePlugin {
    fn register_commands(&mut self, registrar: CommandRegistrar) -> Result<(), PluginError> {
        for command in worktree_command_contributions() {
            registrar
                .register(&command)
                .map_err(|error| PluginError::failed(error.to_string()))?;
        }
        Ok(())
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        match context.request.interface_id.as_str() {
            TOOL_SERVICE_INTERFACE_ID => invoke_tool_service(&context),
            COMMAND_INTERFACE_ID => invoke_command_service(&context.request),
            _ => ServiceResponse::error(
                "unsupported_interface",
                "unsupported worktree plugin service interface",
            ),
        }
    }
}

fn worktree_command_contributions() -> Vec<CommandContribution> {
    vec![
        worktree_command(
            "command.work-tree.list",
            "List Worktrees",
            "List Git worktrees for the current repository",
        ),
        worktree_command(
            "command.work-tree.createSession",
            "Create Session Worktree",
            "Create a worktree for the current session",
        ),
        worktree_command(
            "command.work-tree.attach",
            "Attach Worktree",
            "Attach current session to an existing worktree",
        ),
        worktree_command(
            "command.work-tree.remove",
            "Remove Worktree",
            "Remove a Git worktree",
        ),
    ]
}

fn worktree_command(id: &str, title: &str, description: &str) -> CommandContribution {
    CommandContribution {
        id: id.to_string(),
        title: title.to_string(),
        description: Some(description.to_string()),
        category: Some("worktree".to_string()),
        surfaces: std::collections::BTreeSet::from([CommandSurface::Palette]),
        slash: None,
        arguments: Vec::new(),
        session: bcode_command::CommandSessionRequirement::Optional,
        execution: bcode_command::CommandExecution::Normal,
        owner: CommandOwner::Plugin {
            plugin_id: "bcode.worktree".to_string(),
        },
        action: CommandAction::Plugin {
            plugin_id: "bcode.worktree".to_string(),
            command_id: id.to_string(),
        },
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct WorktreePreparationDescriptor {
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    remove_path: Option<PathBuf>,
}

fn worktree_workspace_root(
    request: &bcode_tool::ToolPreparationRequest,
) -> Result<Option<PathBuf>, String> {
    let mut matching = request
        .host_context
        .iter()
        .filter(|entry| entry.schema == bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA);
    let Some(entry) = matching.next() else {
        return Ok(None);
    };
    if matching.next().is_some() {
        return Err("duplicate Worktree workspace host context".to_owned());
    }
    if entry.schema_version != bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA_VERSION {
        return Err(format!(
            "unsupported Worktree workspace host context version {}; expected {}",
            entry.schema_version,
            bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA_VERSION
        ));
    }
    let root = entry
        .payload
        .get("working_directory")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "Worktree workspace host context working_directory is missing".to_owned())?;
    let root = PathBuf::from(root);
    if !root.is_absolute() {
        return Err("Worktree workspace working directory must be absolute".to_owned());
    }
    Ok(Some(root))
}

fn worktree_preparation_descriptor(
    request: &bcode_tool::ToolPreparationRequest,
) -> Result<WorktreePreparationDescriptor, String> {
    let workspace_root = worktree_workspace_root(request)?;
    let explicit_cwd = request
        .invocation
        .arguments
        .get("cwd")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from);
    let cwd = match explicit_cwd {
        Some(path) if path.is_absolute() => Some(path),
        Some(path) => Some(
            workspace_root
                .as_ref()
                .ok_or_else(|| "Worktree relative cwd requires workspace host context".to_owned())?
                .join(path),
        ),
        None => workspace_root,
    };
    let remove_path = request
        .invocation
        .arguments
        .get("path")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                Ok(path)
            } else {
                cwd.as_ref()
                    .ok_or_else(|| {
                        "Worktree relative remove path requires a prepared cwd".to_owned()
                    })
                    .map(|cwd| cwd.join(path))
            }
        })
        .transpose()?;
    Ok(WorktreePreparationDescriptor { cwd, remove_path })
}

fn worktree_policy_operation(
    request: &bcode_tool::ToolPreparationRequest,
    definition: &ToolDefinition,
) -> Result<bcode_plugin_sdk::ToolPolicyPreparation, String> {
    let descriptor = worktree_preparation_descriptor(request)?;
    let operation = match definition.name.as_str() {
        "worktree.list" => bcode_plugin_sdk::ToolPolicyOperation::ReadOnly,
        "worktree.create" => bcode_plugin_sdk::ToolPolicyOperation::Mutating,
        "worktree.remove" => bcode_plugin_sdk::ToolPolicyOperation::Write {
            paths: descriptor
                .remove_path
                .as_ref()
                .map(|path| path.display().to_string())
                .into_iter()
                .collect(),
            category: "worktree.remove".to_owned(),
        },
        name => return Err(format!("unsupported worktree policy operation: {name}")),
    };
    let category = match definition.name.as_str() {
        "worktree.list" => "worktree.read",
        "worktree.create" => "worktree.create",
        "worktree.remove" => "worktree.remove",
        name => return Err(format!("unsupported worktree policy operation: {name}")),
    };
    Ok(
        bcode_plugin_sdk::ToolPolicyPreparation::new(definition.name != "worktree.list", operation)
            .with_identity(bcode_plugin_sdk::ToolPolicyIdentity {
                aliases: Vec::new(),
                compatibility_aliases: Vec::new(),
                capabilities: vec![category.to_owned()],
                permission_category: Some(category.to_owned()),
            })
            .with_descriptor(serde_json::to_value(descriptor).map_err(|error| error.to_string())?),
    )
}

fn invoke_tool_service(context: &NativeServiceContext) -> ServiceResponse {
    let request = &context.request;
    match request.operation.as_str() {
        OP_LIST_TOOLS => list_tools(request),
        bcode_tool::OP_PREPARE_TOOL => prepare_tool_service_response(
            request,
            [list_definition(), create_definition(), remove_definition()],
            worktree_policy_operation,
        ),
        OP_INVOKE_TOOL => invoke_tool(context),
        _ => ServiceResponse::error(
            "unsupported_operation",
            "unsupported worktree tool service operation",
        ),
    }
}

fn invoke_command_service(request: &ServiceRequest) -> ServiceResponse {
    if request.operation != OP_INVOKE_COMMAND {
        return ServiceResponse::error(
            "unsupported_operation",
            "unsupported worktree command operation",
        );
    }
    let Ok(request) = serde_json::from_slice::<InvokeCommandRequest>(&request.payload) else {
        return ServiceResponse::error(
            "invalid_request",
            "invalid worktree command invocation request",
        );
    };
    match request.command_id.as_str() {
        "command.work-tree.list" => list_worktrees_command(&request),
        "command.work-tree.createSession"
        | "command.work-tree.attach"
        | "command.work-tree.remove" => command_route_response(&request),
        _ => ServiceResponse::error("unknown_command", "unknown worktree command"),
    }
}

fn list_worktrees_command(request: &InvokeCommandRequest) -> ServiceResponse {
    let Some(cwd) = request
        .context
        .as_ref()
        .map(|context| context.working_directory.clone())
    else {
        return ServiceResponse::error(
            "worktree_cwd_required",
            "worktree commands require canonical working-directory context",
        );
    };
    match bcode_worktree::list_worktrees(&cwd) {
        Ok(response) => {
            let mut lines = vec![format!(
                "Worktrees for {}",
                display(&response.repo_root, &cwd)
            )];
            lines.extend(response.worktrees.into_iter().map(|worktree| {
                let marker = if worktree.is_main { "main" } else { "linked" };
                let branch = worktree.branch.unwrap_or_else(|| "<detached>".to_owned());
                format!("* {marker} {branch} — {}", display(&worktree.path, &cwd))
            }));
            json_response(&InvokeCommandResponse {
                success: true,
                message: Some("shown worktrees".to_string()),
                updated_model: None,
                updated_provider: None,
                updated_thinking: None,
                effects: vec![CommandEffect::AppendText {
                    text: lines.join("\n"),
                    format: bcode_command::CommandTextFormat::Markdown,
                }],
            })
        }
        Err(error) => ServiceResponse::error("worktree_list_failed", error.to_string()),
    }
}

fn command_route_response(request: &InvokeCommandRequest) -> ServiceResponse {
    json_response(&InvokeCommandResponse {
        success: true,
        message: None,
        updated_model: None,
        updated_provider: None,
        updated_thinking: None,
        effects: vec![CommandEffect::OpenPluginSurface {
            surface_kind: request.command_id.clone(),
            instance_id: request.command_id.clone(),
            options: serde_json::to_value(&request.args).unwrap_or(serde_json::Value::Null),
        }],
    })
}

fn list_tools(request: &ServiceRequest) -> ServiceResponse {
    if let Err(error) = request.payload_json::<ListToolsRequest>() {
        return invalid_request(&error);
    }
    json_response(&ToolList {
        tools: vec![list_definition(), create_definition(), remove_definition()],
    })
}

fn invoke_tool(context: &NativeServiceContext) -> ServiceResponse {
    let request = &context.request;
    let invocation = match request.payload_json::<ToolInvocationRequest>() {
        Ok(invocation) => invocation,
        Err(error) => return invalid_request(&error),
    };
    let descriptor = match serde_json::from_value::<WorktreePreparationDescriptor>(
        invocation.preparation_descriptor.clone(),
    ) {
        Ok(descriptor) => descriptor,
        Err(error) => {
            return json_response(&tool_error(format!(
                "invalid Worktree preparation descriptor: {error}"
            )));
        }
    };
    if context.cancellation.is_cancelled() {
        return json_response(&tool_error("worktree tool cancelled".to_string()));
    }
    let mut presentation = PrimaryPresentationPublisher::with_limits_and_cancellation(
        context.events,
        &invocation.tool_call_id,
        WORKTREE_PLUGIN_ID,
        WORKTREE_REQUEST_SCHEMA,
        1,
        bcode_tool::ToolPresentationRetention::RetainLatest,
        context.transient_progress_limits,
        context.cancellation.clone(),
    );
    let _ = presentation.replace(&worktree_request_payload(
        &invocation.name,
        &invocation.arguments,
    ));
    let response = match invocation.name.as_str() {
        "worktree.list" => invoke_list(&invocation, &descriptor),
        "worktree.create" => invoke_create(&invocation, &descriptor),
        "worktree.remove" => invoke_remove(&invocation, &descriptor),
        _ => ToolInvocationResponse {
            output: format!("unsupported worktree tool: {}", invocation.name),
            is_error: true,
            content: Vec::new(),
            full_output: None,
            result: None,
        },
    };
    json_response(&response)
}

fn invoke_list(
    invocation: &ToolInvocationRequest,
    descriptor: &WorktreePreparationDescriptor,
) -> ToolInvocationResponse {
    let _request = match serde_json::from_value::<WorktreeListRequest>(invocation.arguments.clone())
    {
        Ok(request) => request,
        Err(error) => return tool_error(error.to_string()),
    };
    let Some(cwd) = descriptor.cwd.as_ref() else {
        return tool_error("worktree.list preparation descriptor is missing cwd".to_string());
    };
    match bcode_worktree::list_worktrees(cwd) {
        Ok(response) => json_tool_response_with_artifact(
            &response,
            &invocation.tool_call_id,
            "list",
            WORKTREE_LIST_SCHEMA,
            "Worktrees",
        ),
        Err(error) => tool_error(error.to_string()),
    }
}

fn invoke_create(
    invocation: &ToolInvocationRequest,
    descriptor: &WorktreePreparationDescriptor,
) -> ToolInvocationResponse {
    let mut request =
        match serde_json::from_value::<WorktreeCreateRequest>(invocation.arguments.clone()) {
            Ok(request) => request,
            Err(error) => return tool_error(error.to_string()),
        };
    let Some(cwd) = descriptor.cwd.clone() else {
        return tool_error("worktree.create preparation descriptor is missing cwd".to_string());
    };
    request.cwd = Some(cwd.clone());
    let config_paths = bcode_config::default_config_paths_from(&cwd);
    let config = match bcode_config::load_config_from_paths(&config_paths) {
        Ok(config) => config,
        Err(error) => return tool_error(error.to_string()),
    };
    match bcode_worktree::create_worktree(&config, &request, &cwd) {
        Ok(response) => json_tool_response_with_artifact(
            &response,
            &invocation.tool_call_id,
            "create",
            WORKTREE_CREATE_SCHEMA,
            "Worktree created",
        ),
        Err(error) => tool_error(error.to_string()),
    }
}

fn invoke_remove(
    invocation: &ToolInvocationRequest,
    descriptor: &WorktreePreparationDescriptor,
) -> ToolInvocationResponse {
    let mut request =
        match serde_json::from_value::<WorktreeRemoveRequest>(invocation.arguments.clone()) {
            Ok(request) => request,
            Err(error) => return tool_error(error.to_string()),
        };
    let Some(cwd) = descriptor.cwd.clone() else {
        return tool_error("worktree.remove preparation descriptor is missing cwd".to_string());
    };
    let Some(path) = descriptor.remove_path.clone() else {
        return tool_error(
            "worktree.remove preparation descriptor is missing remove path".to_string(),
        );
    };
    request.cwd = Some(cwd.clone());
    request.path = path;
    match bcode_worktree::remove_worktree(&cwd, &request.path, request.force) {
        Ok(response) => json_tool_response_with_artifact(
            &response,
            &invocation.tool_call_id,
            "remove",
            WORKTREE_REMOVE_SCHEMA,
            "Worktree removed",
        ),
        Err(error) => tool_error(error.to_string()),
    }
}

fn worktree_request_payload(operation: &str, arguments: &serde_json::Value) -> serde_json::Value {
    arguments.as_object().map_or_else(
        || json!({"operation": operation, "arguments": arguments}),
        |arguments| {
            let mut payload = arguments.clone();
            payload.insert(
                "operation".to_owned(),
                serde_json::Value::String(operation.to_owned()),
            );
            serde_json::Value::Object(payload)
        },
    )
}

fn list_definition() -> ToolDefinition {
    ToolDefinition {
        name: "worktree.list".to_string(),
        description: "List Git worktrees for the current repository.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "cwd": { "type": "string", "description": "Optional repository discovery directory" }
            }
        }),
    }
}

fn create_definition() -> ToolDefinition {
    ToolDefinition {
        name: "worktree.create".to_string(),
        description: "Create a Git worktree using Bcode worktree configuration.".to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["name"],
            "properties": {
                "name": { "type": "string" },
                "cwd": { "type": "string" },
                "path": { "type": "string" },
                "branch": { "type": "string" },
                "new_branch": { "type": "string" },
                "base_ref": { "type": "string", "enum": ["auto", "default_branch", "head"] },
                "detach": { "type": "boolean" },
                "force": { "type": "boolean" },
                "no_setup": { "type": "boolean" }
            }
        }),
    }
}

fn remove_definition() -> ToolDefinition {
    ToolDefinition {
        name: "worktree.remove".to_string(),
        description: "Remove a registered Git worktree without deleting its branch.".to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "cwd": { "type": "string" },
                "path": { "type": "string" },
                "force": { "type": "boolean" }
            }
        }),
    }
}

fn json_response<T: Serialize>(value: &T) -> ServiceResponse {
    match ServiceResponse::json(value) {
        Ok(response) => response,
        Err(error) => ServiceResponse::error("encode_failed", error.to_string()),
    }
}

fn invalid_request(error: &serde_json::Error) -> ServiceResponse {
    ServiceResponse::error("invalid_request", error.to_string())
}

fn json_tool_response_with_artifact<T: Serialize>(
    value: &T,
    tool_call_id: &str,
    artifact_suffix: &str,
    schema: &str,
    title: &str,
) -> ToolInvocationResponse {
    match serde_json::to_string_pretty(value).and_then(|output| {
        let payload = serde_json::to_value(value)?;
        Ok((output, payload))
    }) {
        Ok((output, payload)) => ToolInvocationResponse {
            output,
            is_error: false,
            content: Vec::new(),
            full_output: None,
            result: Some(ToolInvocationResult::Artifact {
                artifact: Box::new(ToolArtifact {
                    artifact_id: format!("{tool_call_id}-worktree-{artifact_suffix}"),
                    producer_plugin_id: WORKTREE_PLUGIN_ID.to_string(),
                    schema: schema.to_string(),
                    schema_version: 1,
                    tool_call_id: Some(tool_call_id.to_string()),
                    title: Some(title.to_string()),
                    metadata: payload,
                    refs: Vec::new(),
                }),
            }),
        },
        Err(error) => tool_error(error.to_string()),
    }
}

const fn tool_error(output: String) -> ToolInvocationResponse {
    ToolInvocationResponse {
        output,
        is_error: true,
        content: Vec::new(),
        full_output: None,
        result: None,
    }
}

#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    let vtable = bcode_plugin_sdk::static_plugin_vtable!(
        WorktreePlugin,
        include_str!("../bcode-plugin.toml")
    );
    #[cfg(feature = "static-bundled")]
    let vtable = {
        let mut vtable = vtable;
        vtable.cli_registration = Some(cli::registration);
        vtable
    };
    vtable
}

#[must_use]
pub fn worktree_tui_registry() -> bcode_plugin_sdk::tui::PluginTuiRegistry {
    let mut registry = bcode_plugin_sdk::tui::PluginTuiRegistry::default();
    registry.register_factory(Box::new(WorktreeCommandSurfaceFactory {
        surface_kind: "command.work-tree.attach",
        title: "Attach Worktree",
    }));
    registry.register_factory(Box::new(WorktreeCommandSurfaceFactory {
        surface_kind: "command.work-tree.createSession",
        title: "Create Worktree Session",
    }));
    registry.register_factory(Box::new(WorktreeCommandSurfaceFactory {
        surface_kind: "command.work-tree.remove",
        title: "Remove Worktree",
    }));
    registry.register_visual_adapter(
        [
            "worktree-request-card",
            "worktree-list-card",
            "worktree-create-result-card",
            "worktree-remove-result-card",
        ],
        Box::new(WorktreeTuiVisualAdapter),
    );
    registry
}

struct WorktreeTuiVisualAdapter;

impl bcode_plugin_sdk::tui::PluginTuiVisualAdapter for WorktreeTuiVisualAdapter {
    fn supports(&self, kind: &str) -> bool {
        matches!(
            kind,
            WORKTREE_REQUEST_SCHEMA
                | WORKTREE_LIST_SCHEMA
                | WORKTREE_CREATE_SCHEMA
                | WORKTREE_REMOVE_SCHEMA
        )
    }

    fn render_mode(
        &self,
        _kind: &str,
        _payload: &serde_json::Value,
    ) -> bcode_plugin_sdk::tui::PluginTuiVisualRenderMode {
        bcode_plugin_sdk::tui::PluginTuiVisualRenderMode::TranscriptBlock
    }

    fn rows(
        &self,
        kind: &str,
        payload: &serde_json::Value,
        context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
    ) -> Vec<Line> {
        let style = worktree_tool_card_style(context);
        match kind {
            WORKTREE_REQUEST_SCHEMA => worktree_request_rows(payload, context, style),
            WORKTREE_LIST_SCHEMA => worktree_list_rows(payload, context, style),
            WORKTREE_CREATE_SCHEMA => {
                worktree_result_rows("Worktree created", payload, context, style)
            }
            WORKTREE_REMOVE_SCHEMA => {
                worktree_result_rows("Worktree removed", payload, context, style)
            }
            _ => Vec::new(),
        }
    }
}

fn worktree_request_rows(
    payload: &serde_json::Value,
    context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
    style: ToolCardStyle,
) -> Vec<Line> {
    let arguments = payload.get("arguments").unwrap_or(payload);
    let mut rows = worktree_header("Worktree request", style);
    for key in [
        "operation",
        "cwd",
        "name",
        "path",
        "branch",
        "new_branch",
        "base_ref",
        "detach",
        "force",
        "no_setup",
    ] {
        push_visual_kv(
            &mut rows,
            key,
            if matches!(key, "cwd" | "path") {
                visual_text(arguments, key).map(|path| context.display_path(path).to_string())
            } else {
                visual_value(arguments, key)
            },
            style,
        );
    }
    rows
}

fn worktree_list_rows(
    payload: &serde_json::Value,
    context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
    style: ToolCardStyle,
) -> Vec<Line> {
    let values = payload
        .get("worktrees")
        .or_else(|| payload.get("entries"))
        .and_then(serde_json::Value::as_array);
    let count = values.map_or(0, Vec::len);
    let mut rows = worktree_header(&format!("Worktrees ({count})"), style);
    if let Some(values) = values {
        for value in values.iter().take(20) {
            let path = visual_text(value, "path").unwrap_or("<path>");
            let branch = visual_text(value, "branch").or_else(|| visual_text(value, "name"));
            rows.push(Line::from_spans(vec![
                Span::styled("  ◆ ", style.accent),
                Span::styled(context.display_path(path).to_string(), style.value),
                Span::styled(
                    branch.map_or_else(String::new, |branch| format!("  {branch}")),
                    style.muted,
                ),
            ]));
        }
        if values.len() > 20 {
            rows.push(Line::from_spans(vec![Span::styled(
                format!("  … {} more worktrees", values.len() - 20),
                style.muted,
            )]));
        }
    }
    rows
}

fn worktree_result_rows(
    title: &str,
    payload: &serde_json::Value,
    context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
    style: ToolCardStyle,
) -> Vec<Line> {
    let mut rows = worktree_header(title, style);
    for key in ["path", "branch", "name", "session_id", "removed", "force"] {
        push_visual_kv(
            &mut rows,
            key,
            if key == "path" {
                visual_text(payload, key).map(|path| context.display_path(path).to_string())
            } else {
                visual_value(payload, key)
            },
            style,
        );
    }
    rows
}

fn worktree_header(title: &str, style: ToolCardStyle) -> Vec<Line> {
    vec![tool_card_header(
        Span::styled("◆ ", style.accent),
        Span::styled(title.to_string(), style.title),
    )]
}

fn push_visual_kv<T>(rows: &mut Vec<Line>, key: &str, value: Option<T>, style: ToolCardStyle)
where
    T: Into<String>,
{
    if let Some(value) = value.map(Into::into) {
        push_tool_card_detail(rows, key, Some(&value), style.muted, style.value);
    }
}

fn visual_value(payload: &serde_json::Value, key: &str) -> Option<String> {
    payload.get(key).and_then(|value| {
        value
            .as_str()
            .map(ToOwned::to_owned)
            .or_else(|| {
                value
                    .as_bool()
                    .map(|value| if value { "yes" } else { "no" }.to_string())
            })
            .or_else(|| value.as_u64().map(|value| value.to_string()))
    })
}

fn visual_text<'a>(payload: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(serde_json::Value::as_str)
}

fn worktree_tool_card_style(
    context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
) -> ToolCardStyle {
    ToolCardStyle::from_component_theme(context.theme().and_then(|theme| theme.component_theme()))
}

struct WorktreeCommandSurfaceFactory {
    surface_kind: &'static str,
    title: &'static str,
}

impl bcode_plugin_sdk::tui::PluginTuiSurfaceFactory for WorktreeCommandSurfaceFactory {
    fn surface_kind(&self) -> &'static str {
        self.surface_kind
    }

    fn open(
        &self,
        request: bcode_plugin_sdk::tui::PluginTuiSurfaceOpenRequest,
    ) -> bcode_plugin_sdk::tui::PluginTuiSurfaceFuture {
        let surface_kind = self.surface_kind;
        let title = self.title;
        Box::pin(async move {
            let Some(repo_path) = request.repo_path else {
                return Err("worktree surfaces require an explicit repo path".into());
            };
            let (lines, worktrees) = worktree_surface_state(surface_kind, &repo_path);
            let session_id = request
                .options
                .get("session_id")
                .and_then(serde_json::Value::as_str)
                .and_then(|value| bcode_session_models::SessionId::from_str(value).ok());
            Ok(Box::new(WorktreeCommandSurface {
                id: surface_kind,
                title,
                repo_path,
                lines,
                worktrees,
                selected: 0,
                list_area: Rect::new(0, 0, 0, 0),
                status: None,
                create_name: "new-session".to_string(),
                create_input: TextInputState::new(TextEditBuffer::from_text("new-session")),
                input_area: Rect::new(0, 0, 0, 0),
                session_id,
            })
                as bcode_plugin_sdk::tui::BoxedPluginTuiSurface)
        })
    }
}

struct WorktreeCommandSurface {
    id: &'static str,
    title: &'static str,
    repo_path: PathBuf,
    lines: Vec<String>,
    worktrees: Vec<WorktreeInfo>,
    selected: usize,
    list_area: Rect,
    status: Option<String>,
    create_name: String,
    create_input: TextInputState,
    input_area: Rect,
    session_id: Option<bcode_session_models::SessionId>,
}

struct WorktreeSurfaceTheme {
    canvas: Style,
    text: Style,
    muted: Style,
    focused: Style,
    selection: Style,
}

impl WorktreeSurfaceTheme {
    fn resolve(theme: Option<&bcode_plugin_sdk::tui::PluginTuiTheme>) -> Self {
        theme.map_or_else(
            || Self {
                canvas: Style::new(),
                text: Style::new(),
                muted: Style::new().add_modifier(Modifier::DIM),
                focused: Style::new().add_modifier(Modifier::BOLD),
                selection: Style::new().add_modifier(Modifier::REVERSED),
            },
            |theme| Self {
                canvas: theme.canvas,
                text: theme.text,
                muted: theme.muted,
                focused: theme.focused,
                selection: theme.selection,
            },
        )
    }
}

impl WorktreeCommandSurface {
    fn render_themed(
        &mut self,
        area: Rect,
        frame: &mut Frame<'_>,
        theme: Option<&bcode_plugin_sdk::tui::PluginTuiTheme>,
    ) {
        let theme = WorktreeSurfaceTheme::resolve(theme);
        frame.fill(area, " ", theme.canvas);
        write_line(
            frame,
            area,
            area.y,
            Line::from_spans(vec![Span::styled(
                self.title,
                theme.focused.add_modifier(Modifier::BOLD),
            )]),
        );
        write_line(
            frame,
            area,
            area.y.saturating_add(1),
            Line::from_spans(vec![Span::styled(
                format!("Repo: {}", display(&self.repo_path, &self.repo_path)),
                theme.muted,
            )]),
        );
        let mut y = area.y.saturating_add(3);
        if self.is_selectable() {
            write_line(
                frame,
                area,
                y,
                Line::from_spans(vec![Span::styled(&self.lines[0], theme.text)]),
            );
            y = y.saturating_add(1);
            let items = self
                .worktrees
                .iter()
                .enumerate()
                .map(|(index, _)| {
                    SelectableListItem::new(index.to_string(), self.lines[index + 1].clone())
                })
                .collect::<Vec<_>>();
            let list = SelectableList::new(&items).styles(worktree_list_styles(&theme));
            let list_height = u16::try_from(items.len()).unwrap_or(u16::MAX);
            self.list_area = Rect::new(
                area.x,
                y,
                area.width,
                list_height.min(area.bottom().saturating_sub(y).saturating_sub(2)),
            );
            let state = worktree_list_state(self.selected);
            list.render_with_fallback_style(self.list_area, &state, frame, theme.canvas);
        } else {
            self.list_area = Rect::new(0, 0, 0, 0);
            for line in &self.lines {
                write_line(
                    frame,
                    area,
                    y,
                    Line::from_spans(vec![Span::styled(line, theme.text)]),
                );
                y = y.saturating_add(1);
            }
        }
        if self.id == "command.work-tree.createSession" {
            let input = TextInputBox::new(single_line_text_policy())
                .label("Name")
                .policy(TextInputBoxPolicy::field().focused(true).rows(1, Some(1)))
                .styles(worktree_input_styles(&theme));
            self.input_area = Rect::new(
                area.x,
                y,
                area.width,
                4.min(area.bottom().saturating_sub(y).saturating_sub(2)),
            );
            input.render(self.input_area, &mut self.create_input, frame);
            self.create_name = self.create_input.buffer().text().to_owned();
        } else {
            self.input_area = Rect::new(0, 0, 0, 0);
        }
        self.render_footer(area, frame, &theme);
    }

    fn render_footer(&self, area: Rect, frame: &mut Frame<'_>, theme: &WorktreeSurfaceTheme) {
        if let Some(status) = &self.status {
            write_line(
                frame,
                area,
                area.y.saturating_add(area.height.saturating_sub(2)),
                Line::from_spans(vec![Span::styled(status.clone(), theme.focused)]),
            );
        }
        let hints = if self.is_selectable() {
            vec![
                KeyHint::new("↑/↓", "select"),
                KeyHint::new("Enter", "activate"),
                KeyHint::new("Esc/q", "close"),
            ]
        } else if self.id == "command.work-tree.createSession" {
            vec![
                KeyHint::new("Enter", "create"),
                KeyHint::new("Esc", "close"),
            ]
        } else {
            vec![KeyHint::new("Enter/Esc/q", "close")]
        };
        KeyHintBar::new(&hints)
            .styles(worktree_hint_styles(theme))
            .render(
                Rect::new(
                    area.x,
                    area.y.saturating_add(area.height.saturating_sub(1)),
                    area.width,
                    1,
                ),
                frame,
            );
    }
}

const fn worktree_list_styles(theme: &WorktreeSurfaceTheme) -> SelectableListStyles {
    SelectableListStyles {
        normal: theme.text,
        focused: theme.selection,
        selected: theme.selection,
        hovered: theme.focused,
        pressed: theme.selection.add_modifier(Modifier::BOLD),
        disabled: theme.muted,
    }
}

const fn worktree_list_state(selected: usize) -> SelectableListState {
    let mut state = SelectableListState::new(None);
    state.set_focused(Some(selected));
    state
}

const fn single_line_text_policy() -> TextInputPolicy {
    let mut policy = TextInputPolicy::chat_composer();
    policy.keyboard.shift_enter = None;
    policy.viewport.max_rows = Some(1);
    policy
}

const fn worktree_input_styles(theme: &WorktreeSurfaceTheme) -> TextInputBoxStyles {
    TextInputBoxStyles {
        text: theme.text,
        focused_text: theme.focused,
        disabled_text: theme.muted,
        placeholder: theme.muted,
        selection: theme.selection,
        border: theme.muted,
        focused_border: theme.focused,
        background: theme.canvas,
        focused_background: theme.canvas,
        disabled_background: theme.canvas,
    }
}

const fn worktree_hint_styles(theme: &WorktreeSurfaceTheme) -> KeyHintBarStyles {
    KeyHintBarStyles {
        key: theme.focused,
        label: theme.text,
        separator: theme.muted,
        disabled: theme.muted,
        background: theme.canvas,
    }
}

impl bcode_plugin_sdk::tui::PluginTuiSurface for WorktreeCommandSurface {
    fn id(&self) -> &'static str {
        self.id
    }

    fn title(&self) -> &'static str {
        self.title
    }

    fn render(&mut self, area: Rect, frame: &mut Frame<'_>) {
        self.render_themed(area, frame, None);
    }

    fn render_with_theme(
        &mut self,
        area: Rect,
        frame: &mut Frame<'_>,
        theme: Option<&bcode_plugin_sdk::tui::PluginTuiTheme>,
    ) {
        self.render_themed(area, frame, theme);
    }

    fn handle_event(
        &mut self,
        event: &Event,
        _host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
    ) -> bcode_plugin_sdk::tui::PluginTuiAction {
        if let Event::Key(key) = event
            && matches!(key.key, KeyCode::Escape | KeyCode::Char('q'))
        {
            return bcode_plugin_sdk::tui::PluginTuiAction::Close { outcome: None };
        }
        if self.id == "command.work-tree.createSession" {
            let input = TextInputBox::new(single_line_text_policy())
                .policy(TextInputBoxPolicy::field().focused(true).rows(1, Some(1)));
            return match input.handle_event(self.input_area, &mut self.create_input, event) {
                TextInputBoxOutcome::Submitted => {
                    self.create_name = self.create_input.buffer().text().to_owned();
                    self.create_worktree()
                }
                TextInputBoxOutcome::Edited | TextInputBoxOutcome::Redraw => {
                    self.create_name = self.create_input.buffer().text().to_owned();
                    bcode_plugin_sdk::tui::PluginTuiAction::Redraw
                }
                TextInputBoxOutcome::Ignored
                | TextInputBoxOutcome::EdgeUp
                | TextInputBoxOutcome::EdgeDown => bcode_plugin_sdk::tui::PluginTuiAction::None,
            };
        }
        if self.is_selectable() {
            let items = self
                .worktrees
                .iter()
                .enumerate()
                .map(|(index, _)| {
                    SelectableListItem::new(index.to_string(), self.lines[index + 1].clone())
                })
                .collect::<Vec<_>>();
            let list = SelectableList::new(&items);
            let list_event = match event {
                Event::Key(key) if key.key == KeyCode::Char('k') => {
                    Event::Key(bmux_keyboard::KeyStroke::simple(KeyCode::Up))
                }
                Event::Key(key) if key.key == KeyCode::Char('j') => {
                    Event::Key(bmux_keyboard::KeyStroke::simple(KeyCode::Down))
                }
                _ => event.clone(),
            };
            let mut state = worktree_list_state(self.selected);
            return match list.handle_event(self.list_area, &mut state, &list_event) {
                SelectableListOutcome::Focused(index) => {
                    self.selected = index;
                    bcode_plugin_sdk::tui::PluginTuiAction::Redraw
                }
                SelectableListOutcome::Redraw => {
                    self.selected = state.focused().unwrap_or(self.selected);
                    bcode_plugin_sdk::tui::PluginTuiAction::Redraw
                }
                SelectableListOutcome::Selected(index) => {
                    self.selected = index;
                    self.activate_selected()
                }
                SelectableListOutcome::Ignored => bcode_plugin_sdk::tui::PluginTuiAction::None,
            };
        }
        match event {
            Event::Key(key) if key.key == KeyCode::Enter => {
                bcode_plugin_sdk::tui::PluginTuiAction::Close { outcome: None }
            }
            _ => bcode_plugin_sdk::tui::PluginTuiAction::None,
        }
    }
}

impl WorktreeCommandSurface {
    fn create_worktree(&mut self) -> bcode_plugin_sdk::tui::PluginTuiAction {
        let name = self.create_name.trim().to_string();
        if name.is_empty() {
            self.status = Some("worktree name is required".to_string());
            return bcode_plugin_sdk::tui::PluginTuiAction::Redraw;
        }
        let request = WorktreeCreateRequest {
            name,
            cwd: Some(self.repo_path.clone()),
            path: None,
            branch: None,
            new_branch: None,
            base_ref: Some(bcode_worktree_models::WorktreeBaseRef::Head),
            detach: false,
            force: false,
            attach_session_id: self.session_id,
            new_session: self.session_id.is_none(),
            no_setup: false,
        };
        let config_paths = bcode_config::default_config_paths_from(&self.repo_path);
        let config = match bcode_config::load_config_from_paths(&config_paths) {
            Ok(config) => config,
            Err(error) => {
                self.status = Some(format!("worktree config unavailable: {error}"));
                return bcode_plugin_sdk::tui::PluginTuiAction::Redraw;
            }
        };
        match bcode_worktree::create_worktree(&config, &request, &self.repo_path) {
            Ok(response) => bcode_plugin_sdk::tui::PluginTuiAction::Close {
                outcome: Some(serde_json::json!({
                    "status": format!("created worktree {}", display(&response.path, &self.repo_path)),
                    "append_text": format!("Created worktree: {}", display(&response.path, &self.repo_path)),
                    "set_session_working_directory": response.path.display().to_string(),
                })),
            },
            Err(error) => {
                self.status = Some(format!("worktree create failed: {error}"));
                bcode_plugin_sdk::tui::PluginTuiAction::Redraw
            }
        }
    }

    fn is_selectable(&self) -> bool {
        matches!(
            self.id,
            "command.work-tree.attach" | "command.work-tree.remove"
        ) && !self.worktrees.is_empty()
    }

    fn activate_selected(&mut self) -> bcode_plugin_sdk::tui::PluginTuiAction {
        match self.id {
            "command.work-tree.remove" => self.remove_selected(),
            "command.work-tree.attach" => self.attach_selected(),
            _ => bcode_plugin_sdk::tui::PluginTuiAction::Close { outcome: None },
        }
    }

    fn attach_selected(&self) -> bcode_plugin_sdk::tui::PluginTuiAction {
        let Some(worktree) = self.worktrees.get(self.selected) else {
            return bcode_plugin_sdk::tui::PluginTuiAction::None;
        };
        bcode_plugin_sdk::tui::PluginTuiAction::Close {
            outcome: Some(serde_json::json!({
                "status": format!("attaching worktree {}", display(&worktree.path, &self.repo_path)),
                "append_text": format!("Attaching session to worktree: {}", display(&worktree.path, &self.repo_path)),
                "set_session_working_directory": worktree.path.display().to_string(),
            })),
        }
    }

    fn remove_selected(&mut self) -> bcode_plugin_sdk::tui::PluginTuiAction {
        let Some(worktree) = self.worktrees.get(self.selected) else {
            return bcode_plugin_sdk::tui::PluginTuiAction::None;
        };
        if worktree.is_main {
            self.status = Some("refusing to remove main worktree".to_string());
            return bcode_plugin_sdk::tui::PluginTuiAction::Redraw;
        }
        match bcode_worktree::remove_worktree(&self.repo_path, &worktree.path, false) {
            Ok(response) => bcode_plugin_sdk::tui::PluginTuiAction::Close {
                outcome: Some(serde_json::json!({
                    "status": format!("removed worktree {}", display(&response.path, &self.repo_path)),
                    "append_text": format!("Removed worktree: {}", display(&response.path, &self.repo_path)),
                })),
            },
            Err(error) => {
                self.status = Some(format!("worktree remove failed: {error}"));
                bcode_plugin_sdk::tui::PluginTuiAction::Redraw
            }
        }
    }
}

fn worktree_surface_state(
    surface_kind: &str,
    repo_path: &std::path::Path,
) -> (Vec<String>, Vec<WorktreeInfo>) {
    match bcode_worktree::list_worktrees(repo_path) {
        Ok(response) => {
            let worktrees = response.worktrees;
            let mut lines = match surface_kind {
                "command.work-tree.attach" => vec!["Select a worktree to attach:".to_string()],
                "command.work-tree.remove" => vec!["Select a worktree to remove:".to_string()],
                "command.work-tree.createSession" => vec![
                    "Enter worktree name, then press Enter to create.".to_string(),
                    "Backspace edits · Esc/q cancels".to_string(),
                ],
                _ => vec!["Worktree command surface".to_string()],
            };
            lines.extend(worktrees.iter().map(|worktree| {
                let marker = if worktree.is_main { "main" } else { "linked" };
                let branch = worktree.branch.as_deref().unwrap_or("<detached>");
                format!(
                    "* {marker} {branch} — {}",
                    display(&worktree.path, repo_path)
                )
            }));
            (lines, worktrees)
        }
        Err(error) => (vec![format!("worktrees unavailable: {error}")], Vec::new()),
    }
}

fn write_line(frame: &mut Frame<'_>, area: Rect, y: u16, line: impl Into<Line>) {
    if y >= area.y.saturating_add(area.height) {
        return;
    }
    frame.write_line(Rect::new(area.x, y, area.width, 1), &line.into());
}

#[cfg(not(feature = "static-bundled"))]
bcode_plugin_sdk::export_plugin!(WorktreePlugin, include_str!("../bcode-plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn test_plugin_theme() -> bcode_plugin_sdk::tui::PluginTuiTheme {
        use bcode_plugin_sdk::tui::{
            PluginTuiDiffTheme, PluginTuiSourceTheme, PluginTuiSyntaxColor, PluginTuiSyntaxTheme,
            PluginTuiTheme,
        };
        let style = Style::new();
        let syntax = PluginTuiSyntaxColor::from_tui(bmux_tui::style::Color::Default);
        PluginTuiTheme {
            component_theme_version: bcode_plugin_sdk::tui::PLUGIN_TUI_COMPONENT_THEME_VERSION,
            canvas: style.bg(bmux_tui::style::Color::Blue),
            text: style.fg(bmux_tui::style::Color::White),
            muted: style.fg(bmux_tui::style::Color::Yellow),
            border: style.fg(bmux_tui::style::Color::Cyan),
            focused: style.fg(bmux_tui::style::Color::Magenta),
            selection: style
                .fg(bmux_tui::style::Color::Black)
                .bg(bmux_tui::style::Color::Green),
            source: PluginTuiSourceTheme {
                source: style,
                border: style,
                gutter: style,
                truncated: style,
            },
            diff: PluginTuiDiffTheme {
                text: style,
                muted: style,
                title: style,
                label: style,
                added: style,
                removed: style,
                hunk: style,
                added_row: style,
                removed_row: style,
                added_emphasis: style,
                removed_emphasis: style,
            },
            syntax: PluginTuiSyntaxTheme {
                text: syntax,
                comment: syntax,
                keyword: syntax,
                function: syntax,
                variable: syntax,
                string: syntax,
                number: syntax,
                type_name: syntax,
                operator: syntax,
                punctuation: syntax,
                heading: syntax,
                link: syntax,
                raw: syntax,
            },
        }
    }

    #[test]
    fn worktree_surface_uses_host_theme_at_narrow_sizes() {
        let worktree = WorktreeInfo {
            path: PathBuf::from("/tmp/repo"),
            is_main: true,
            branch: Some("main".to_owned()),
            commit: Some("abc123".to_owned()),
        };
        let mut surface = WorktreeCommandSurface {
            id: "command.work-tree.attach",
            title: "Attach worktree",
            repo_path: PathBuf::from("/tmp/repo"),
            lines: vec!["Choose a worktree".to_owned(), "main".to_owned()],
            worktrees: vec![worktree],
            selected: 0,
            list_area: Rect::new(0, 0, 0, 0),
            status: None,
            create_name: String::new(),
            create_input: TextInputState::new(TextEditBuffer::from_text("")),
            input_area: Rect::new(0, 0, 0, 0),
            session_id: None,
        };
        let area = Rect::new(0, 0, 18, 8);
        let mut buffer = bmux_tui::buffer::Buffer::empty(area);
        let theme = test_plugin_theme();
        surface.render_with_theme(area, &mut Frame::new(&mut buffer), Some(&theme));

        assert!(
            buffer
                .cells()
                .iter()
                .any(|cell| cell.style.bg == theme.canvas.bg)
        );
        assert!(
            buffer
                .cells()
                .iter()
                .any(|cell| cell.style.bg == theme.selection.bg)
        );
        assert!(surface.list_area.width <= area.width);
        assert!(surface.list_area.height <= area.height);
    }

    #[test]
    fn worktree_selectable_list_supports_mouse_activation() {
        let items = [
            SelectableListItem::new("first", "first"),
            SelectableListItem::new("second", "second"),
        ];
        let list = SelectableList::new(&items);
        let mut state = worktree_list_state(0);
        let area = Rect::new(4, 3, 20, 2);
        let point = bmux_tui::geometry::Point::new(6, 4);

        assert_eq!(
            list.handle_event(
                area,
                &mut state,
                &Event::Mouse(bmux_tui::event::MouseEvent::new(
                    bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left,),
                    point,
                )),
            ),
            SelectableListOutcome::Redraw
        );
        assert_eq!(
            list.handle_event(
                area,
                &mut state,
                &Event::Mouse(bmux_tui::event::MouseEvent::new(
                    bmux_tui::event::MouseEventKind::Up(bmux_tui::event::MouseButton::Left),
                    point,
                )),
            ),
            SelectableListOutcome::Selected(1)
        );
    }

    #[test]
    fn worktree_name_input_handles_unicode_editing() {
        let mut state = TextInputState::new(TextEditBuffer::from_text("tree"));
        let input = TextInputBox::new(single_line_text_policy())
            .policy(TextInputBoxPolicy::field().focused(true).rows(1, Some(1)));
        let area = Rect::new(0, 0, 24, 3);

        assert_eq!(
            input.handle_event(area, &mut state, &Event::Paste("-🙂".to_owned())),
            TextInputBoxOutcome::Edited
        );
        assert_eq!(state.buffer().text(), "tree-🙂");
    }

    #[test]
    fn worktree_requests_use_durable_generic_contributions_without_legacy_visuals() {
        let arguments = serde_json::json!({"name": "feature", "base_ref": "head"});
        let payload = worktree_request_payload("worktree.create", &arguments);
        assert_eq!(payload["operation"], "worktree.create");
        assert_eq!(payload["name"], "feature");
    }

    fn workspace_context(path: &Path) -> Vec<bcode_tool::ToolHostContextEntry> {
        vec![bcode_tool::ToolHostContextEntry {
            schema: bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA.to_owned(),
            schema_version: bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA_VERSION,
            payload: serde_json::json!({"working_directory": path}),
        }]
    }

    #[test]
    fn worktree_owner_prepares_mixed_permission_policy() {
        for (definition, arguments, expected_permission) in [
            (list_definition(), serde_json::Value::Null, false),
            (
                create_definition(),
                serde_json::json!({"name": "feature"}),
                true,
            ),
            (
                remove_definition(),
                serde_json::json!({"path": "/tmp/worktree"}),
                true,
            ),
        ] {
            let request = bcode_tool::ToolPreparationRequest {
                invocation: bcode_tool::ToolInvocationDescriptor {
                    invocation_id: "call".to_owned(),
                    tool_name: definition.name.clone(),
                    arguments,
                },
                host_context: workspace_context(Path::new("/tmp/workspace")),
            };
            let policy = worktree_policy_operation(&request, &definition).expect("worktree policy");
            assert_eq!(policy.requires_permission, expected_permission);
            let descriptor =
                serde_json::from_value::<WorktreePreparationDescriptor>(policy.descriptor)
                    .expect("Worktree descriptor");
            assert_eq!(descriptor.cwd, Some(PathBuf::from("/tmp/workspace")));
            if definition.name == "worktree.remove" {
                assert_eq!(descriptor.remove_path, Some(PathBuf::from("/tmp/worktree")));
                assert_eq!(
                    policy.operation,
                    bcode_plugin_sdk::ToolPolicyOperation::Write {
                        paths: vec!["/tmp/worktree".to_owned()],
                        category: "worktree.remove".to_owned(),
                    }
                );
            }
        }
    }

    #[test]
    fn worktree_preparation_preserves_explicit_cwd_precedence_and_resolves_remove_path() {
        let definition = remove_definition();
        let request = bcode_tool::ToolPreparationRequest {
            invocation: bcode_tool::ToolInvocationDescriptor {
                invocation_id: "call".to_owned(),
                tool_name: definition.name.clone(),
                arguments: serde_json::json!({
                    "cwd": "nested/repository",
                    "path": "../worktree"
                }),
            },
            host_context: workspace_context(Path::new("/tmp/workspace")),
        };

        let prepared =
            worktree_policy_operation(&request, &definition).expect("Worktree preparation");
        let descriptor =
            serde_json::from_value::<WorktreePreparationDescriptor>(prepared.descriptor)
                .expect("Worktree descriptor");

        assert_eq!(
            descriptor.cwd,
            Some(PathBuf::from("/tmp/workspace/nested/repository"))
        );
        assert_eq!(
            descriptor.remove_path,
            Some(PathBuf::from(
                "/tmp/workspace/nested/repository/../worktree"
            ))
        );
        assert_eq!(
            prepared.operation,
            bcode_plugin_sdk::ToolPolicyOperation::Write {
                paths: vec!["/tmp/workspace/nested/repository/../worktree".to_owned()],
                category: "worktree.remove".to_owned(),
            }
        );
    }

    #[test]
    fn worktree_relative_cwd_requires_workspace_context() {
        let definition = list_definition();
        let request = bcode_tool::ToolPreparationRequest {
            invocation: bcode_tool::ToolInvocationDescriptor {
                invocation_id: "call".to_owned(),
                tool_name: definition.name.clone(),
                arguments: serde_json::json!({"cwd": "repository"}),
            },
            host_context: Vec::new(),
        };

        let error = worktree_policy_operation(&request, &definition)
            .expect_err("relative cwd without workspace");

        assert!(error.contains("requires workspace host context"));
    }

    #[test]
    fn worktree_request_adapter_renders_generic_contribution_payload() {
        let arguments = serde_json::json!({"name": "feature", "base_ref": "head"});
        let payload = worktree_request_payload("worktree.create", &arguments);
        let rows = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::rows(
            &WorktreeTuiVisualAdapter,
            WORKTREE_REQUEST_SCHEMA,
            &payload,
            &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
                80,
                bcode_plugin_sdk::tui::PluginTuiDiffLayout::Unified,
                None,
            ),
        );
        let rendered = rows
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_str())
            .collect::<String>();
        assert!(rendered.contains("worktree.create"));
        assert!(rendered.contains("feature"));
        assert!(rendered.contains("head"));
    }

    #[test]
    fn worktree_plugin_registers_palette_commands_from_plugin_code() {
        extern "C" fn register_command(
            payload: *const u8,
            payload_len: usize,
            user_data: *mut std::ffi::c_void,
        ) {
            assert!(!payload.is_null());
            assert!(!user_data.is_null());
            let bytes = unsafe { std::slice::from_raw_parts(payload, payload_len) };
            let contribution = serde_json::from_slice::<CommandContribution>(bytes)
                .expect("command contribution should decode");
            let registry = unsafe { &mut *(user_data.cast::<bcode_command::CommandRegistry>()) };
            registry.register(contribution);
        }

        let mut plugin = WorktreePlugin;
        let mut registry = bcode_command::CommandRegistry::new();
        plugin
            .register_commands(CommandRegistrar::new(
                Some(register_command),
                std::ptr::from_mut(&mut registry).cast::<std::ffi::c_void>(),
            ))
            .expect("worktree plugin should register commands");

        let commands = registry.commands_for_surface(&CommandSurface::Palette);

        assert!(commands.iter().any(|command| {
            command.id == "command.work-tree.list"
                && command.action
                    == CommandAction::Plugin {
                        plugin_id: "bcode.worktree".to_string(),
                        command_id: "command.work-tree.list".to_string(),
                    }
        }));
        assert!(
            commands
                .iter()
                .any(|command| command.id == "command.work-tree.createSession")
        );
        assert!(
            commands
                .iter()
                .any(|command| command.id == "command.work-tree.attach")
        );
        assert!(
            commands
                .iter()
                .any(|command| command.id == "command.work-tree.remove")
        );
    }
}
