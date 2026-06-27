#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Bundled skills command palette plugin for Bcode.

use bcode_command::{
    COMMAND_INTERFACE_ID, CommandAction, CommandContribution, CommandEffect, CommandOwner,
    CommandSurface, InvokeCommandRequest, InvokeCommandResponse, OP_INVOKE_COMMAND,
};
use bcode_plugin_sdk::prelude::*;
use bmux_keyboard::KeyCode;
use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui::text::{Line, Span};
use serde::Serialize;

/// Bundled skills command plugin.
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
        "skills.list" | "skills.active" => command_route_response(&request.command_id),
        _ => ServiceResponse::error("unknown_command", "unknown skills command"),
    }
}

fn command_route_response(route: &str) -> ServiceResponse {
    json_response(&InvokeCommandResponse {
        success: true,
        message: None,
        updated_model: None,
        updated_provider: None,
        updated_thinking: None,
        effects: vec![CommandEffect::OpenPluginSurface {
            surface_kind: route.to_string(),
            instance_id: route.to_string(),
            options: serde_json::Value::Null,
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
    let mut vtable =
        bcode_plugin_sdk::static_plugin_vtable!(SkillsPlugin, include_str!("../bcode-plugin.toml"));
    vtable.tui_registry = Some(skills_tui_registry);
    vtable
}

fn skills_tui_registry() -> bcode_plugin_sdk::tui::PluginTuiRegistry {
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
        _request: bcode_plugin_sdk::tui::PluginTuiSurfaceOpenRequest,
    ) -> bcode_plugin_sdk::tui::PluginTuiSurfaceFuture {
        let surface_kind = self.surface_kind;
        let title = self.title;
        Box::pin(async move {
            Ok(Box::new(SkillsCommandSurface {
                id: surface_kind,
                title,
                lines: skills_surface_lines(surface_kind),
            })
                as bcode_plugin_sdk::tui::BoxedPluginTuiSurface)
        })
    }
}

struct SkillsCommandSurface {
    id: &'static str,
    title: &'static str,
    lines: Vec<String>,
}

impl bcode_plugin_sdk::tui::PluginTuiSurface for SkillsCommandSurface {
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
        let mut y = area.y.saturating_add(2);
        for line in &self.lines {
            write_line(frame, area, y, Line::from(line.clone()));
            y = y.saturating_add(1);
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

fn skills_surface_lines(surface_kind: &str) -> Vec<String> {
    match surface_kind {
        "skills.list" => vec!["Available skills are owned by the skills plugin.".to_string()],
        "skills.active" => vec!["Active skills are owned by the skills plugin.".to_string()],
        _ => vec!["Skills command surface".to_string()],
    }
}

fn write_line(frame: &mut Frame<'_>, area: Rect, y: u16, line: impl Into<Line>) {
    if y >= area.y.saturating_add(area.height) {
        return;
    }
    frame.write_line(Rect::new(area.x, y, area.width, 1), &line.into());
}

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
