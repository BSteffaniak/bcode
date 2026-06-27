#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Git worktree tool plugin for Bcode.

use bcode_command::{
    COMMAND_INTERFACE_ID, CommandAction, CommandContribution, CommandEffect, CommandOwner,
    CommandSurface, InvokeCommandRequest, InvokeCommandResponse, OP_INVOKE_COMMAND,
};
use bcode_plugin_sdk::prelude::*;
use bcode_tool::{
    ListToolsRequest, OP_INVOKE_TOOL, OP_LIST_TOOLS, TOOL_SERVICE_INTERFACE_ID, ToolDefinition,
    ToolInvocationRequest, ToolInvocationResponse, ToolList, ToolPresentationField,
    ToolPresentationFieldKind, ToolRequestPresentationMetadata, ToolSideEffect,
};
use bcode_worktree_models::{
    WorktreeCreateRequest, WorktreeInfo, WorktreeListRequest, WorktreeRemoveRequest,
};
use bmux_keyboard::KeyCode;
use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui::text::{Line, Span};
use serde::Serialize;
use serde_json::json;
use std::path::PathBuf;
use std::str::FromStr;

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
        owner: CommandOwner::Plugin {
            plugin_id: "bcode.worktree".to_string(),
        },
        action: CommandAction::Plugin {
            plugin_id: "bcode.worktree".to_string(),
            command_id: id.to_string(),
        },
    }
}

fn invoke_tool_service(context: &NativeServiceContext) -> ServiceResponse {
    let request = &context.request;
    match request.operation.as_str() {
        OP_LIST_TOOLS => list_tools(request),
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
    let cwd = request
        .args
        .get("cwd")
        .map_or_else(current_dir, PathBuf::from);
    match bcode_worktree::list_worktrees(&cwd) {
        Ok(response) => {
            let mut lines = vec![format!("Worktrees for {}", response.repo_root.display())];
            lines.extend(response.worktrees.into_iter().map(|worktree| {
                let marker = if worktree.is_main { "main" } else { "linked" };
                let branch = worktree.branch.unwrap_or_else(|| "<detached>".to_owned());
                format!("* {marker} {branch} — {}", worktree.path.display())
            }));
            json_response(&InvokeCommandResponse {
                success: true,
                message: Some("shown worktrees".to_string()),
                updated_model: None,
                updated_provider: None,
                updated_thinking: None,
                effects: vec![CommandEffect::AppendText {
                    text: lines.join("\n"),
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
    if context.cancellation.is_cancelled() {
        return json_response(&tool_error("worktree tool cancelled".to_string()));
    }
    let response = match invocation.name.as_str() {
        "worktree.list" => invoke_list(&invocation),
        "worktree.create" => invoke_create(&invocation),
        "worktree.remove" => invoke_remove(&invocation),
        _ => ToolInvocationResponse {
            output: format!("unsupported worktree tool: {}", invocation.name),
            is_error: true,
            content: Vec::new(),
            full_output: None,
            host_action: None,
            result: None,
        },
    };
    json_response(&response)
}

fn invoke_list(invocation: &ToolInvocationRequest) -> ToolInvocationResponse {
    let request = match serde_json::from_value::<WorktreeListRequest>(invocation.arguments.clone())
    {
        Ok(request) => request,
        Err(error) => return tool_error(error.to_string()),
    };
    let cwd = request
        .cwd
        .or_else(|| invocation.cwd.clone())
        .unwrap_or_else(current_dir);
    match bcode_worktree::list_worktrees(&cwd) {
        Ok(response) => json_tool_response(&response),
        Err(error) => tool_error(error.to_string()),
    }
}

fn invoke_create(invocation: &ToolInvocationRequest) -> ToolInvocationResponse {
    let mut request =
        match serde_json::from_value::<WorktreeCreateRequest>(invocation.arguments.clone()) {
            Ok(request) => request,
            Err(error) => return tool_error(error.to_string()),
        };
    if request.cwd.is_none() {
        request.cwd.clone_from(&invocation.cwd);
    }
    let cwd = request.cwd.clone().unwrap_or_else(current_dir);
    let config = match bcode_config::load_config() {
        Ok(config) => config,
        Err(error) => return tool_error(error.to_string()),
    };
    match bcode_worktree::create_worktree(&config, &request, &cwd) {
        Ok(response) => json_tool_response(&response),
        Err(error) => tool_error(error.to_string()),
    }
}

fn invoke_remove(invocation: &ToolInvocationRequest) -> ToolInvocationResponse {
    let request =
        match serde_json::from_value::<WorktreeRemoveRequest>(invocation.arguments.clone()) {
            Ok(request) => request,
            Err(error) => return tool_error(error.to_string()),
        };
    let cwd = request
        .cwd
        .clone()
        .or_else(|| invocation.cwd.clone())
        .unwrap_or_else(current_dir);
    match bcode_worktree::remove_worktree(&cwd, &request.path, request.force) {
        Ok(response) => json_tool_response(&response),
        Err(error) => tool_error(error.to_string()),
    }
}

fn tool_ui(
    activity_label: &str,
    title: &str,
    fields: Vec<ToolPresentationField>,
) -> bcode_tool::ToolUiMetadata {
    bcode_tool::ToolUiMetadata {
        activity_label: Some(activity_label.to_string()),
        live_argument_preview: None,

        request_presentation: Some(ToolRequestPresentationMetadata {
            title: title.to_string(),
            fields,
            preview: None,
        }),
    }
}

fn field(
    label: &str,
    argument: &str,
    kind: ToolPresentationFieldKind,
    optional: bool,
) -> ToolPresentationField {
    ToolPresentationField {
        label: label.to_string(),
        argument: argument.to_string(),
        kind,
        optional,
    }
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
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: false,
        policy: bcode_tool::ToolPolicyMetadata {
            aliases: vec!["worktree.read".to_string()],
            compatibility_aliases: Vec::new(),
            capabilities: Vec::new(),
            permission_category: Some("worktree.read".to_string()),
            argument_extractors: Vec::new(),
        },
        ui: tool_ui(
            "listing worktrees",
            "List worktrees",
            vec![field(
                "Working directory",
                "cwd",
                ToolPresentationFieldKind::Path,
                true,
            )],
        ),
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
        side_effect: ToolSideEffect::ExecuteProcess,
        requires_permission: true,
        policy: bcode_tool::ToolPolicyMetadata {
            aliases: vec!["worktree.create".to_string()],
            compatibility_aliases: Vec::new(),
            capabilities: Vec::new(),
            permission_category: Some("worktree.create".to_string()),
            argument_extractors: Vec::new(),
        },
        ui: tool_ui(
            "creating worktree",
            "Create worktree",
            vec![
                field("Name", "name", ToolPresentationFieldKind::Text, false),
                field("Path", "path", ToolPresentationFieldKind::Path, true),
                field("Branch", "branch", ToolPresentationFieldKind::Text, true),
                field(
                    "New branch",
                    "new_branch",
                    ToolPresentationFieldKind::Text,
                    true,
                ),
            ],
        ),
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
        side_effect: ToolSideEffect::ExecuteProcess,
        requires_permission: true,
        policy: bcode_tool::ToolPolicyMetadata {
            aliases: vec!["worktree.remove".to_string()],
            compatibility_aliases: Vec::new(),
            capabilities: Vec::new(),
            permission_category: Some("worktree.remove".to_string()),
            argument_extractors: vec![bcode_tool::ToolArgumentExtractor {
                kind: bcode_tool::ToolArgumentKind::WritePath,
                argument: "path".to_string(),
            }],
        },
        ui: tool_ui(
            "removing worktree",
            "Remove worktree",
            vec![
                field("Path", "path", ToolPresentationFieldKind::Path, false),
                field("Force", "force", ToolPresentationFieldKind::Boolean, true),
            ],
        ),
    }
}

fn current_dir() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
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

fn json_tool_response<T: Serialize>(value: &T) -> ToolInvocationResponse {
    match serde_json::to_string_pretty(value) {
        Ok(output) => ToolInvocationResponse {
            output,
            is_error: false,
            content: Vec::new(),
            full_output: None,
            host_action: None,
            result: None,
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
        host_action: None,
        result: None,
    }
}

#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    let mut vtable = bcode_plugin_sdk::static_plugin_vtable!(
        WorktreePlugin,
        include_str!("../bcode-plugin.toml")
    );
    vtable.tui_registry = Some(worktree_tui_registry);
    vtable
}

fn worktree_tui_registry() -> bcode_plugin_sdk::tui::PluginTuiRegistry {
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
    registry
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
            let repo_path = request.repo_path.unwrap_or_else(current_dir);
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
                status: None,
                create_name: "new-session".to_string(),
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
    status: Option<String>,
    create_name: String,
    session_id: Option<bcode_session_models::SessionId>,
}

impl bcode_plugin_sdk::tui::PluginTuiSurface for WorktreeCommandSurface {
    fn id(&self) -> &'static str {
        self.id
    }

    fn title(&self) -> &'static str {
        self.title
    }

    fn render(&mut self, area: Rect, frame: &mut Frame<'_>) {
        frame.fill(area, " ", Style::new().fg(Color::White).bg(Color::Black));
        write_line(
            frame,
            area,
            area.y,
            Line::from_spans(vec![Span::styled(
                self.title,
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            )]),
        );
        write_line(
            frame,
            area,
            area.y.saturating_add(1),
            Line::from(format!("Repo: {}", self.repo_path.display())),
        );
        let mut y = area.y.saturating_add(3);
        for (index, line) in self.lines.iter().enumerate() {
            let display_line = if self.is_selectable() && index > 0 {
                let marker = if self.selected == index.saturating_sub(1) {
                    "› "
                } else {
                    "  "
                };
                format!("{marker}{line}")
            } else {
                line.clone()
            };
            write_line(frame, area, y, Line::from(display_line));
            y = y.saturating_add(1);
        }
        if self.id == "command.work-tree.createSession" {
            write_line(
                frame,
                area,
                y,
                Line::from(format!("Name: {}", self.create_name)),
            );
        }
        if let Some(status) = &self.status {
            write_line(
                frame,
                area,
                area.y.saturating_add(area.height.saturating_sub(2)),
                Line::from(status.clone()),
            );
        }
        write_line(
            frame,
            area,
            area.y.saturating_add(area.height.saturating_sub(1)),
            Line::from("Enter/Esc/q closes"),
        );
    }

    fn handle_event(
        &mut self,
        event: &Event,
        _host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
    ) -> bcode_plugin_sdk::tui::PluginTuiAction {
        match event {
            Event::Key(key) if matches!(key.key, KeyCode::Escape | KeyCode::Char('q')) => {
                bcode_plugin_sdk::tui::PluginTuiAction::Close { outcome: None }
            }
            Event::Key(key) if self.id == "command.work-tree.createSession" => {
                self.handle_create_key(key.key)
            }
            Event::Key(key) if matches!(key.key, KeyCode::Up | KeyCode::Char('k')) => {
                self.select_previous();
                bcode_plugin_sdk::tui::PluginTuiAction::Redraw
            }
            Event::Key(key) if matches!(key.key, KeyCode::Down | KeyCode::Char('j')) => {
                self.select_next();
                bcode_plugin_sdk::tui::PluginTuiAction::Redraw
            }
            Event::Key(key) if key.key == KeyCode::Enter => self.activate_selected(),
            _ => bcode_plugin_sdk::tui::PluginTuiAction::None,
        }
    }
}

impl WorktreeCommandSurface {
    fn handle_create_key(&mut self, key: KeyCode) -> bcode_plugin_sdk::tui::PluginTuiAction {
        match key {
            KeyCode::Enter => self.create_worktree(),
            KeyCode::Backspace => {
                self.create_name.pop();
                bcode_plugin_sdk::tui::PluginTuiAction::Redraw
            }
            KeyCode::Char(value) => {
                self.create_name.push(value);
                bcode_plugin_sdk::tui::PluginTuiAction::Redraw
            }
            _ => bcode_plugin_sdk::tui::PluginTuiAction::None,
        }
    }

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
        let config = match bcode_config::load_config() {
            Ok(config) => config,
            Err(error) => {
                self.status = Some(format!("worktree config unavailable: {error}"));
                return bcode_plugin_sdk::tui::PluginTuiAction::Redraw;
            }
        };
        match bcode_worktree::create_worktree(&config, &request, &self.repo_path) {
            Ok(response) => bcode_plugin_sdk::tui::PluginTuiAction::Close {
                outcome: Some(serde_json::json!({
                    "status": format!("created worktree {}", response.path.display()),
                    "append_text": format!("Created worktree: {}", response.path.display()),
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

    fn select_previous(&mut self) {
        if !self.is_selectable() {
            return;
        }
        self.selected = self.selected.saturating_sub(1);
    }

    fn select_next(&mut self) {
        if !self.is_selectable() {
            return;
        }
        self.selected = (self.selected + 1).min(self.worktrees.len().saturating_sub(1));
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
                "status": format!("attaching worktree {}", worktree.path.display()),
                "append_text": format!("Attaching session to worktree: {}", worktree.path.display()),
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
                    "status": format!("removed worktree {}", response.path.display()),
                    "append_text": format!("Removed worktree: {}", response.path.display()),
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
                format!("* {marker} {branch} — {}", worktree.path.display())
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

bcode_plugin_sdk::export_plugin!(WorktreePlugin, include_str!("../bcode-plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;

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
