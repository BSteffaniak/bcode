#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! skills command palette plugin for Bcode.

#[cfg(feature = "static-bundled")]
mod cli;

use bcode_command::{
    COMMAND_INTERFACE_ID, CommandAction, CommandContribution, CommandEffect, CommandOwner,
    CommandSurface, InvokeCommandRequest, InvokeCommandResponse, OP_INVOKE_COMMAND,
};
use bcode_plugin_sdk::prelude::*;
use bcode_skill::{SkillRegistry, SkillRegistryOptions, SkillSourceRoot};
use bcode_skill_models::SkillSourceKind;
use bmux_keyboard::KeyCode;
use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect};
use bmux_tui::style::{Modifier, Style};
use bmux_tui::text::{Line, Span};
use bmux_tui_components::pane::{Pane, PaneState, PaneStyles};
use bmux_tui_components::text_view::{TextView, TextViewPolicy, TextViewState, TextViewStyles};
use serde::Serialize;

/// skills command plugin.
#[derive(Default)]
pub struct SkillsPlugin;

impl RustPlugin for SkillsPlugin {
    fn register_commands(&mut self, registrar: CommandRegistrar) -> Result<(), PluginError> {
        for command in skills_palette_command_contributions() {
            registrar
                .register(&command)
                .map_err(|error| PluginError::failed(error.to_string()))?;
        }
        Ok(())
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        if context.request.interface_id != COMMAND_INTERFACE_ID {
            return ServiceResponse::error(
                "unsupported_interface",
                "unsupported skills plugin service interface",
            );
        }
        invoke_command_service(&context.request)
    }
}

fn invoke_command_service(request: &ServiceRequest) -> ServiceResponse {
    if request.operation != OP_INVOKE_COMMAND {
        return ServiceResponse::error(
            "unsupported_operation",
            "unsupported skills command operation",
        );
    }
    let Ok(request) = serde_json::from_slice::<InvokeCommandRequest>(&request.payload) else {
        return ServiceResponse::error(
            "invalid_request",
            "invalid skills command invocation request",
        );
    };
    match request.command_id.as_str() {
        "skills.list" | "skills.active" => command_route_response(&request),
        _ => ServiceResponse::error("unknown_command", "unknown skills command"),
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

fn skills_palette_command_contributions() -> Vec<CommandContribution> {
    vec![
        skills_command("skills.list", "Skills: Available", "List available skills"),
        skills_command(
            "skills.active",
            "Skills: Active",
            "Show active session skills",
        ),
    ]
}

fn skills_command(id: &str, title: &str, description: &str) -> CommandContribution {
    CommandContribution {
        id: id.to_string(),
        title: title.to_string(),
        description: Some(description.to_string()),
        category: Some("skills".to_string()),
        surfaces: std::collections::BTreeSet::from([CommandSurface::Palette]),
        slash: None,
        arguments: Vec::new(),
        session: bcode_command::CommandSessionRequirement::Optional,
        execution: bcode_command::CommandExecution::Normal,
        owner: CommandOwner::Plugin {
            plugin_id: "bcode.skills".to_string(),
        },
        action: CommandAction::Plugin {
            plugin_id: "bcode.skills".to_string(),
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
    let vtable =
        bcode_plugin_sdk::static_plugin_vtable!(SkillsPlugin, include_str!("../bcode-plugin.toml"));
    #[cfg(feature = "static-bundled")]
    let vtable = {
        let mut vtable = vtable;
        vtable.cli_registration = Some(cli::registration);
        vtable
    };
    vtable
}

#[must_use]
pub fn skills_tui_registry() -> bcode_plugin_sdk::tui::PluginTuiRegistry {
    let mut registry = bcode_plugin_sdk::tui::PluginTuiRegistry::default();
    for (surface_kind, title) in [
        ("skills.list", "Available Skills"),
        ("skills.active", "Active Skills"),
    ] {
        registry.register_factory(Box::new(SkillsCommandSurfaceFactory {
            surface_kind,
            title,
        }));
    }
    registry
}

struct SkillsCommandSurfaceFactory {
    surface_kind: &'static str,
    title: &'static str,
}

impl bcode_plugin_sdk::tui::PluginTuiSurfaceFactory for SkillsCommandSurfaceFactory {
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
            Ok(Box::new(SkillsCommandSurface {
                id: surface_kind,
                title,
                lines: skills_surface_lines(surface_kind, &request.options),
                text_view: TextViewState::new(),
            })
                as bcode_plugin_sdk::tui::BoxedPluginTuiSurface)
        })
    }
}

struct SkillsCommandSurface {
    id: &'static str,
    title: &'static str,
    lines: Vec<String>,
    text_view: TextViewState,
}

struct SkillsSurfaceTheme {
    canvas: Style,
    text: Style,
    muted: Style,
    focused: Style,
    component: bmux_tui_components::theme::ComponentTheme,
}

impl SkillsSurfaceTheme {
    fn resolve(theme: Option<bcode_plugin_sdk::tui::PluginTuiTheme>) -> Self {
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

impl SkillsCommandSurface {
    fn render_themed(
        &self,
        area: Rect,
        frame: &mut Frame<'_>,
        theme: Option<bcode_plugin_sdk::tui::PluginTuiTheme>,
    ) {
        let theme = SkillsSurfaceTheme::resolve(theme);
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
            .empty("No skills available")
            .render(body, &self.text_view, frame);
        frame.write_line_with_fallback_style(
            footer,
            &Line::from_spans(vec![Span::styled("Enter/Esc/q closes", theme.muted)]),
            theme.canvas,
        );
    }
}

impl bcode_plugin_sdk::tui::PluginTuiSurface for SkillsCommandSurface {
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
        theme: Option<bcode_plugin_sdk::tui::PluginTuiTheme>,
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

fn skills_surface_lines(surface_kind: &str, options: &serde_json::Value) -> Vec<String> {
    match surface_kind {
        "skills.list" => available_skills_lines(),
        "skills.active" => active_skills_lines(options),
        _ => vec!["Skills command surface".to_string()],
    }
}

fn active_skills_lines(options: &serde_json::Value) -> Vec<String> {
    let Some(skills) = options
        .get("active_skills")
        .and_then(serde_json::Value::as_array)
    else {
        return vec!["No active session skills are available.".to_string()];
    };
    let mut lines = vec![format!("Active skills: {}", skills.len())];
    lines.extend(skills.iter().filter_map(|skill| {
        let skill_id = skill.get("skill_id")?.as_str()?;
        let bytes_loaded = skill
            .get("bytes_loaded")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default();
        let source = skill
            .get("source")
            .and_then(|source| source.get("label"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown source");
        let truncated = skill
            .get("truncated")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or_default();
        let suffix = if truncated { " truncated" } else { "" };
        Some(format!(
            "* {skill_id} — {bytes_loaded} bytes{suffix} from {source}"
        ))
    }));
    lines
}

fn available_skills_lines() -> Vec<String> {
    let config = match bcode_config::load_config() {
        Ok(config) => config,
        Err(error) => return vec![format!("skills config unavailable: {error}")],
    };
    let Some(registry) = build_skill_registry(&config) else {
        return vec!["skills are disabled".to_string()];
    };
    let list = registry.list();
    if list.skills.is_empty() {
        return vec!["No skills are available.".to_string()];
    }
    std::iter::once(format!("Available skills: {}", list.skills.len()))
        .chain(list.skills.into_iter().map(|skill| {
            let description = skill
                .description
                .unwrap_or_else(|| "no description".to_string());
            format!("* {} — {description} ({})", skill.id, skill.source.label)
        }))
        .collect()
}

fn build_skill_registry(config: &bcode_config::BcodeConfig) -> Option<SkillRegistry> {
    if !config.skills.enabled {
        return None;
    }
    let mut roots = Vec::new();
    if config.skills.include_repo_skills {
        roots.push(SkillSourceRoot::new(
            std::path::PathBuf::from(".bcode/skills"),
            SkillSourceKind::Repository,
            "repo:.bcode/skills",
            10,
        ));
    }
    if config.skills.include_generic_repo_skills {
        roots.push(SkillSourceRoot::new(
            std::path::PathBuf::from("skills"),
            SkillSourceKind::Repository,
            "repo:skills",
            15,
        ));
    }
    if config.skills.include_compat_claude_skills {
        roots.push(SkillSourceRoot::new(
            std::path::PathBuf::from(".claude/skills"),
            SkillSourceKind::Compatibility,
            "repo:.claude/skills",
            20,
        ));
    }
    if config.skills.include_user_skills {
        roots.push(SkillSourceRoot::new(
            bcode_config::default_config_dir().join("skills"),
            SkillSourceKind::User,
            "user-config:skills",
            30,
        ));
        roots.push(SkillSourceRoot::new(
            bcode_config::default_state_dir().join("skills"),
            SkillSourceKind::User,
            "user-state:skills",
            35,
        ));
    }
    for (index, path) in config.skills.sources.paths.iter().enumerate() {
        roots.push(SkillSourceRoot::new(
            path.clone(),
            SkillSourceKind::Configured,
            format!("configured:{index}"),
            40 + u16::try_from(index).unwrap_or(u16::MAX - 40),
        ));
    }
    let options = SkillRegistryOptions {
        max_skill_file_bytes: config.skills.max_skill_file_bytes.get(),
        max_context_bytes: config
            .skills
            .max_context_bytes
            .map(std::num::NonZeroUsize::get),
        follow_symlinks: config.skills.follow_symlinks,
        disabled_ids: config.skills.disabled_skill_ids(),
    };
    SkillRegistry::discover(&roots, options).ok()
}

#[cfg(not(feature = "static-bundled"))]
bcode_plugin_sdk::export_plugin!(SkillsPlugin, include_str!("../bcode-plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skills_plugin_registers_palette_commands_from_plugin_code() {
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

        let mut plugin = SkillsPlugin;
        let mut registry = bcode_command::CommandRegistry::new();
        plugin
            .register_commands(CommandRegistrar::new(
                Some(register_command),
                std::ptr::from_mut(&mut registry).cast::<std::ffi::c_void>(),
            ))
            .expect("skills plugin should register commands");

        let commands = registry.commands_for_surface(&CommandSurface::Palette);

        assert!(commands.iter().any(|command| command.id == "skills.list"));
        assert!(commands.iter().any(|command| command.id == "skills.active"));
    }
}
