#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! model and runtime command palette plugin for Bcode.

use bcode_command::{
    COMMAND_INTERFACE_ID, CommandAction, CommandContribution, CommandEffect, CommandOwner,
    CommandSurface, InvokeCommandRequest, InvokeCommandResponse, OP_INVOKE_COMMAND,
};
use bcode_plugin_sdk::prelude::*;
use bmux_keyboard::KeyCode;
use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect};
use bmux_tui::style::{Modifier, Style};
use bmux_tui::text::{Line, Span};
use bmux_tui_components::pane::{Pane, PaneState, PaneStyles};
use bmux_tui_components::text_view::{TextView, TextViewPolicy, TextViewState, TextViewStyles};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthSecurityInspectRequest {
    vault_path: PathBuf,
    profile: String,
    policy: String,
}

fn invoke_workflow_block(request: &ServiceRequest) -> ServiceResponse {
    if request.operation != "provider_auth.security.inspect" {
        return ServiceResponse::error(
            "unsupported_operation",
            "unsupported provider-auth security workflow block operation",
        );
    }
    let invocation = match request.payload_json::<bcode_workflow::WorkflowBlockInvocation>() {
        Ok(invocation) => invocation,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    let request = match invocation.typed_input::<AuthSecurityInspectRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error),
    };
    if request.profile.trim().is_empty() || !request.vault_path.is_absolute() {
        return ServiceResponse::error(
            "invalid_request",
            "security inspection requires an absolute vault path and non-empty profile",
        );
    }
    let policy = match request.policy.as_str() {
        "off" => bcode_provider_auth::security::AuthDeviceSealPolicy::Off,
        "preferred" => bcode_provider_auth::security::AuthDeviceSealPolicy::Preferred,
        "required" => bcode_provider_auth::security::AuthDeviceSealPolicy::Required,
        _ => {
            return ServiceResponse::error(
                "invalid_request",
                "security policy must be off, preferred, or required",
            );
        }
    };
    json_response(&bcode_provider_auth::security::inspect_auth_vault_security(
        &request.vault_path,
        &request.profile,
        policy,
    ))
}

/// model command plugin.
#[derive(Default)]
pub struct ModelPlugin;

impl RustPlugin for ModelPlugin {
    fn register_commands(&mut self, registrar: CommandRegistrar) -> Result<(), PluginError> {
        for command in model_palette_command_contributions() {
            registrar
                .register(&command)
                .map_err(|error| PluginError::failed(error.to_string()))?;
        }
        Ok(())
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        if context.request.interface_id == bcode_workflow::WORKFLOW_BLOCK_INTERFACE_ID {
            return invoke_workflow_block(&context.request);
        }
        if context.request.interface_id != COMMAND_INTERFACE_ID {
            return ServiceResponse::error(
                "unsupported_interface",
                "unsupported model plugin service interface",
            );
        }
        invoke_command_service(&context.request)
    }
}

fn invoke_command_service(request: &ServiceRequest) -> ServiceResponse {
    if request.operation != OP_INVOKE_COMMAND {
        return ServiceResponse::error(
            "unsupported_operation",
            "unsupported model command operation",
        );
    }
    let Ok(request) = serde_json::from_slice::<InvokeCommandRequest>(&request.payload) else {
        return ServiceResponse::error(
            "invalid_request",
            "invalid model command invocation request",
        );
    };
    match request.command_id.as_str() {
        "model.status" | "model.serverStatus" | "runtime.status" | "model.select" => {
            command_route_response(&request)
        }
        _ => ServiceResponse::error("unknown_command", "unknown model command"),
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

fn model_palette_command_contributions() -> Vec<CommandContribution> {
    vec![
        model_command(
            "model.status",
            "Model: Current Status",
            "Show configured provider/model status",
            "model",
        ),
        model_command(
            "model.serverStatus",
            "Model: Server Status",
            "Show server default provider/model status",
            "model",
        ),
        model_command(
            "runtime.status",
            "Runtime: Status",
            "Show active runtime work",
            "runtime",
        ),
        model_command(
            "model.select",
            "Model: Select",
            "Pick a model for this session",
            "model",
        ),
    ]
}

fn model_command(id: &str, title: &str, description: &str, category: &str) -> CommandContribution {
    CommandContribution {
        id: id.to_string(),
        title: title.to_string(),
        description: Some(description.to_string()),
        category: Some(category.to_string()),
        surfaces: std::collections::BTreeSet::from([CommandSurface::Palette]),
        slash: None,
        arguments: Vec::new(),
        session: bcode_command::CommandSessionRequirement::Optional,
        execution: bcode_command::CommandExecution::Normal,
        owner: CommandOwner::Plugin {
            plugin_id: "bcode.model".to_string(),
        },
        action: CommandAction::Plugin {
            plugin_id: "bcode.model".to_string(),
            command_id: id.to_string(),
        },
    }
}

fn json_response<T: Serialize>(value: &T) -> ServiceResponse {
    match ServiceResponse::json(value) {
        Ok(response) => response,
        Err(error) => ServiceResponse::error("encode_failed", error.to_string()),
    }
}

#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_plugin_vtable!(ModelPlugin, include_str!("../bcode-plugin.toml"))
}

#[must_use]
pub fn model_tui_registry() -> bcode_plugin_sdk::tui::PluginTuiRegistry {
    let mut registry = bcode_plugin_sdk::tui::PluginTuiRegistry::default();
    for (surface_kind, title) in [
        ("model.status", "Model Status"),
        ("model.serverStatus", "Server Model Status"),
        ("runtime.status", "Runtime Status"),
        ("model.select", "Select Model"),
    ] {
        registry.register_factory(Box::new(ModelCommandSurfaceFactory {
            surface_kind,
            title,
        }));
    }
    registry
}

struct ModelCommandSurfaceFactory {
    surface_kind: &'static str,
    title: &'static str,
}

impl bcode_plugin_sdk::tui::PluginTuiSurfaceFactory for ModelCommandSurfaceFactory {
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
            Ok(Box::new(ModelCommandSurface {
                id: surface_kind,
                title,
                lines: model_surface_lines(surface_kind, &request.options),
                text_view: TextViewState::new(),
            })
                as bcode_plugin_sdk::tui::BoxedPluginTuiSurface)
        })
    }
}

struct ModelCommandSurface {
    id: &'static str,
    title: &'static str,
    lines: Vec<String>,
    text_view: TextViewState,
}

struct SurfaceTheme {
    canvas: Style,
    text: Style,
    muted: Style,
    focused: Style,
    component: bmux_tui_components::theme::ComponentTheme,
}

impl SurfaceTheme {
    fn resolve(theme: Option<&bcode_plugin_sdk::tui::PluginTuiTheme>) -> Self {
        theme.map_or_else(
            || {
                let component = bmux_tui_components::theme::ComponentTheme::default();
                Self {
                    canvas: component.canvas,
                    text: component.text,
                    muted: component.muted,
                    focused: component.focused,
                    component,
                }
            },
            |theme| {
                let component = theme.component_theme().unwrap_or_default();
                Self {
                    canvas: theme.canvas,
                    text: theme.text,
                    muted: theme.muted,
                    focused: theme.focused,
                    component,
                }
            },
        )
    }
}

impl ModelCommandSurface {
    fn render_themed(
        &self,
        area: Rect,
        frame: &mut Frame<'_>,
        theme: Option<&bcode_plugin_sdk::tui::PluginTuiTheme>,
    ) {
        let theme = SurfaceTheme::resolve(theme);
        frame.fill(area, " ", theme.canvas);
        let pane = Pane::new()
            .title(Line::from_spans(vec![Span::styled(
                self.title,
                theme.focused.add_modifier(Modifier::BOLD),
            )]))
            .padding(Insets::new(1, 1, 1, 1))
            .styles(PaneStyles {
                background: Some(theme.canvas),
                border: theme.component.border,
                focused_border: theme.focused,
            });
        let pane_state = PaneState::new(area);
        pane.render(&pane_state, frame);
        let content = pane.inner_area(&pane_state);
        if content.is_empty() {
            return;
        }
        let footer = Rect::new(
            content.x,
            content.bottom().saturating_sub(1),
            content.width,
            1,
        );
        let body = Rect::new(
            content.x,
            content.y,
            content.width,
            content.height.saturating_sub(1),
        );
        let lines = self
            .lines
            .iter()
            .map(|line| Line::from_spans(vec![Span::styled(line.clone(), theme.text)]))
            .collect::<Vec<_>>();
        TextView::new(&lines)
            .policy(TextViewPolicy::bare())
            .styles(TextViewStyles {
                text: theme.text,
                empty: theme.muted,
                background: theme.canvas,
            })
            .empty("No model status available")
            .render(body, &self.text_view, frame);
        frame.write_line_with_fallback_style(
            footer,
            &Line::from_spans(vec![Span::styled("Enter/Esc/q closes", theme.muted)]),
            theme.canvas,
        );
    }
}

impl bcode_plugin_sdk::tui::PluginTuiSurface for ModelCommandSurface {
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
        match event {
            Event::Key(key)
                if matches!(
                    key.key,
                    KeyCode::Enter | KeyCode::Escape | KeyCode::Char('q')
                ) =>
            {
                bcode_plugin_sdk::tui::PluginTuiAction::Close { outcome: None }
            }
            _ => bcode_plugin_sdk::tui::PluginTuiAction::None,
        }
    }
}

fn model_surface_lines(surface_kind: &str, options: &serde_json::Value) -> Vec<String> {
    match surface_kind {
        "model.status" => model_status_lines(options, false),
        "model.serverStatus" => model_status_lines(options, true),
        "runtime.status" => runtime_status_lines(options),
        "model.select" => model_select_lines(),
        _ => vec!["Model command surface".to_string()],
    }
}

fn runtime_status_lines(options: &serde_json::Value) -> Vec<String> {
    let Some(status) = options.get("server_status") else {
        return vec!["Runtime status unavailable.".to_string()];
    };
    let mut lines = vec!["Runtime status".to_string()];
    if let Some(version) = status.get("version").and_then(serde_json::Value::as_str) {
        lines.push(format!("Version: {version}"));
    }
    if let Some(uptime) = status.get("uptime_ms").and_then(serde_json::Value::as_u64) {
        lines.push(format!("Uptime: {uptime} ms"));
    }
    if let Some(plugins) = status
        .get("plugin_runtime")
        .and_then(serde_json::Value::as_array)
    {
        let running = plugins
            .iter()
            .filter_map(|plugin| plugin.get("running").and_then(serde_json::Value::as_u64))
            .sum::<u64>();
        let queued = plugins
            .iter()
            .filter_map(|plugin| plugin.get("queued").and_then(serde_json::Value::as_u64))
            .sum::<u64>();
        lines.push(format!("Plugin work: {running} running, {queued} queued"));
        lines.extend(plugins.iter().filter_map(|plugin| {
            let plugin_id = plugin.get("plugin_id")?.as_str()?;
            let running = plugin.get("running")?.as_u64()?;
            let queued = plugin.get("queued")?.as_u64()?;
            (running > 0 || queued > 0)
                .then(|| format!("* {plugin_id}: {running} running, {queued} queued"))
        }));
    }
    lines
}

fn model_status_lines(options: &serde_json::Value, server_defaults: bool) -> Vec<String> {
    let hydrated = options
        .get(if server_defaults {
            "default_model_status"
        } else {
            "session_model_status"
        })
        .or_else(|| options.get("default_model_status"));
    if let Some(status) = hydrated {
        let provider = status
            .get("provider_plugin_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("default provider");
        let model = status
            .get("model_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("default model");
        let mut lines = vec![if server_defaults {
            "Server default model status".to_string()
        } else {
            "Session model status".to_string()
        }];
        lines.push(format!("Provider: {provider}"));
        lines.push(format!("Model: {model}"));
        for key in [
            "context_window",
            "max_output_tokens",
            "reasoning_effort",
            "reasoning_summary",
            "prompt_cache_mode",
            "conversation_reuse_mode",
            "compaction_mode",
        ] {
            if let Some(value) = status.get(key).filter(|value| !value.is_null()) {
                lines.push(format!("{key}: {value}"));
            }
        }
        return lines;
    }

    let config = match bcode_config::load_config() {
        Ok(config) => config,
        Err(error) => return vec![format!("model config unavailable: {error}")],
    };
    let provider = config
        .model
        .provider_plugin_id
        .as_deref()
        .unwrap_or("default provider");
    let model = config.model.model_id.as_deref().unwrap_or("default model");
    let mut lines = vec![if server_defaults {
        "Server default model configuration".to_string()
    } else {
        "Configured model status".to_string()
    }];
    lines.push(format!("Provider: {provider}"));
    lines.push(format!("Model: {model}"));
    if let Some(profile) = &config.model.profile {
        lines.push(format!("Profile: {profile}"));
    }
    if let Some(thinking) = config.model.default_thinking_level {
        lines.push(format!("Default thinking: {thinking:?}"));
    }
    lines.push(format!("Profiles: {}", config.model.profiles.len()));
    lines.push(format!("Aliases: {}", config.model.aliases.len()));
    lines
}

fn model_select_lines() -> Vec<String> {
    let config = match bcode_config::load_config() {
        Ok(config) => config,
        Err(error) => return vec![format!("model config unavailable: {error}")],
    };
    let mut lines = vec!["Configured model choices".to_string()];
    lines.extend(
        config
            .model
            .aliases
            .keys()
            .map(|alias| format!("* alias: {alias}")),
    );
    lines.extend(
        config
            .model
            .profiles
            .keys()
            .map(|profile| format!("* profile: {profile}")),
    );
    if lines.len() == 1 {
        lines.push("No aliases or profiles configured.".to_string());
    }
    lines
}

#[cfg(not(feature = "static-bundled"))]
bcode_plugin_sdk::export_plugin!(ModelPlugin, include_str!("../bcode-plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_surface_uses_shared_pane_and_text_view_with_host_theme() {
        use bcode_plugin_sdk::tui::{
            PluginTuiDiffTheme, PluginTuiSourceTheme, PluginTuiSyntaxColor, PluginTuiSyntaxTheme,
            PluginTuiTheme,
        };
        use bmux_tui::buffer::Buffer;
        use bmux_tui::style::Color;

        let style = Style::new();
        let syntax_color = PluginTuiSyntaxColor::from_tui(Color::Default);
        let theme = PluginTuiTheme {
            component_theme_version: bcode_plugin_sdk::tui::PLUGIN_TUI_COMPONENT_THEME_VERSION,
            canvas: style,
            text: style.fg(Color::Green),
            muted: style.fg(Color::BrightBlack),
            border: style.fg(Color::Blue),
            focused: style.fg(Color::Magenta),
            selection: style,
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
                text: syntax_color,
                comment: syntax_color,
                keyword: syntax_color,
                function: syntax_color,
                variable: syntax_color,
                string: syntax_color,
                number: syntax_color,
                type_name: syntax_color,
                operator: syntax_color,
                punctuation: syntax_color,
                heading: syntax_color,
                link: syntax_color,
                raw: syntax_color,
            },
        };
        let surface = ModelCommandSurface {
            id: "model.status",
            title: "Model Status",
            lines: vec!["Provider: example".to_owned()],
            text_view: TextViewState::new(),
        };
        let area = Rect::new(0, 0, 40, 8);
        let mut buffer = Buffer::empty(area);
        surface.render_themed(area, &mut Frame::new(&mut buffer), Some(&theme));

        assert_eq!(
            buffer
                .get(bmux_tui::geometry::Point::new(0, 0))
                .expect("top border")
                .style
                .fg,
            Some(Color::Blue)
        );
        let rendered = buffer
            .cells()
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect::<String>();
        assert!(rendered.contains("Provider: example"));
        assert!(
            buffer
                .cells()
                .iter()
                .any(|cell| cell.symbol == "P" && cell.style.fg == Some(Color::Green))
        );
    }

    #[test]
    fn provider_auth_security_block_is_read_only_and_reports_missing_vault() {
        let manifest: bcode_plugin::PluginManifest =
            toml::from_str(include_str!("../bcode-plugin.toml")).expect("manifest");
        let block = &manifest.services[0].workflow_blocks[0];
        assert_eq!(block.block_id, "provider_auth.security.inspect");
        assert_eq!(block.effect, bcode_workflow::WorkflowBlockEffect::ReadOnly);
        assert_eq!(
            block.authorization.capability,
            bcode_workflow::WorkflowToolCapability::Disabled
        );
        let missing = std::env::temp_dir().join(format!(
            "bcode-missing-auth-vault-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let response = invoke_workflow_block(&ServiceRequest {
            interface_id: bcode_workflow::WORKFLOW_BLOCK_INTERFACE_ID.to_string(),
            operation: "provider_auth.security.inspect".to_string(),
            payload: serde_json::to_vec(&bcode_workflow::WorkflowBlockInvocation {
                version: bcode_workflow::WorkflowBlockInvocation::VERSION,
                dispatch_identity: "test-dispatch".to_string(),
                workspace_root: std::env::temp_dir(),
                input: serde_json::json!({
                    "vault_path": missing,
                    "profile": "openai",
                    "policy": "required",
                }),
                preparation: None,
            })
            .expect("request"),
        });
        assert_eq!(response.error, None);
        let status: bcode_provider_auth::security::AuthSecurityStatus =
            serde_json::from_slice(&response.payload).expect("status");
        assert!(!status.vault_exists);
        assert!(!status.policy_satisfied);
        assert!(status.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == "auth_vault_missing"
                && diagnostic.severity
                    == bcode_provider_auth::security::AuthSecurityDiagnosticSeverity::Warning
        }));
    }

    #[test]
    fn model_plugin_registers_palette_commands_from_plugin_code() {
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

        let mut plugin = ModelPlugin;
        let mut registry = bcode_command::CommandRegistry::new();
        plugin
            .register_commands(CommandRegistrar::new(
                Some(register_command),
                std::ptr::from_mut(&mut registry).cast::<std::ffi::c_void>(),
            ))
            .expect("model plugin should register commands");

        let commands = registry.commands_for_surface(&CommandSurface::Palette);

        assert!(commands.iter().any(|command| command.id == "model.status"));
        assert!(
            commands
                .iter()
                .any(|command| command.id == "model.serverStatus")
        );
        assert!(
            commands
                .iter()
                .any(|command| command.id == "runtime.status")
        );
        assert!(commands.iter().any(|command| command.id == "model.select"));
    }
}
