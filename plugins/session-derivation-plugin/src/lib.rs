#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Plugin-owned fork and clone product workflows over generic session derivation.

use bcode_command::{
    COMMAND_INTERFACE_ID, CommandAction, CommandContribution, CommandEffect, CommandExecution,
    CommandOwner, CommandSessionRequirement, CommandSurface, InvokeCommandRequest,
    InvokeCommandResponse, OP_INVOKE_COMMAND, SessionOpenFocus, SlashCommandContribution,
};
use bcode_plugin_sdk::prelude::*;
use bcode_plugin_sdk::{
    SESSION_DERIVATION_INTERFACE_ID, ServiceBridgeRequest, ServiceBridgeResponse,
    SessionDerivationServiceRequest, SessionDerivationServiceResponse,
};
use bcode_session_models::{
    SESSION_DERIVATION_CONTRACT_VERSION, SessionDerivationLineage, SessionDerivationOperationId,
    SessionDerivationRequest, SessionDerivationSourcePolicy, SessionDerivationTerminalOutcome,
};
use bcode_tool::{ToolInvocationServiceRequest, ToolInvocationServiceResolution};
use bmux_keyboard::KeyCode;
use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::style::{Modifier, Style};
use bmux_tui::text::{Line, Span};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

const PLUGIN_ID: &str = "bcode.session-derivation";
const FORK_COMMAND_ID: &str = "session-derivation.fork";
const CLONE_COMMAND_ID: &str = "session-derivation.clone";

#[derive(Default)]
pub struct SessionDerivationPlugin;

impl RustPlugin for SessionDerivationPlugin {
    fn register_commands(&mut self, registrar: CommandRegistrar) -> Result<(), PluginError> {
        for command in commands() {
            registrar
                .register(&command)
                .map_err(|error| PluginError::failed(error.to_string()))?;
        }
        Ok(())
    }

    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        if context.request.interface_id != COMMAND_INTERFACE_ID
            || context.request.operation != OP_INVOKE_COMMAND
        {
            return ServiceResponse::error("unsupported_operation", "unsupported operation");
        }
        let request = match context.request.payload_json::<InvokeCommandRequest>() {
            Ok(request) => request,
            Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
        };
        invoke_command(&context, request)
    }
}

fn commands() -> Vec<CommandContribution> {
    vec![
        command(
            FORK_COMMAND_ID,
            "fork",
            "Fork Session",
            "Create a new session before a selected prompt sequence",
        ),
        command(
            CLONE_COMMAND_ID,
            "clone",
            "Clone Session",
            "Copy the stable current conversation into a new session",
        ),
    ]
}

fn command(id: &str, slash_name: &str, title: &str, description: &str) -> CommandContribution {
    CommandContribution {
        id: id.to_owned(),
        title: title.to_owned(),
        description: Some(description.to_owned()),
        category: Some("session".to_owned()),
        surfaces: BTreeSet::from([CommandSurface::Palette, CommandSurface::Slash]),
        slash: Some(SlashCommandContribution {
            name: slash_name.to_owned(),
            aliases: BTreeSet::new(),
        }),
        arguments: Vec::new(),
        session: CommandSessionRequirement::Required,
        execution: CommandExecution::Immediate,
        owner: CommandOwner::Plugin {
            plugin_id: PLUGIN_ID.to_owned(),
        },
        action: CommandAction::Plugin {
            plugin_id: PLUGIN_ID.to_owned(),
            command_id: id.to_owned(),
        },
    }
}

#[allow(clippy::too_many_lines)]
fn invoke_command(
    context: &NativeServiceContext,
    request: InvokeCommandRequest,
) -> ServiceResponse {
    let Some(command_context) = request.context else {
        return ServiceResponse::error("session_required", "command requires session context");
    };
    let Some(session_id) = command_context.session_id else {
        return ServiceResponse::error("session_required", "command requires an active session");
    };
    let snapshot = match call_derivation_service(
        context,
        &request.command_id,
        SessionDerivationServiceRequest::Snapshot { session_id },
    ) {
        Ok(SessionDerivationServiceResponse::Snapshot { snapshot }) => snapshot,
        Ok(_) => {
            return ServiceResponse::error("unexpected_response", "unexpected snapshot response");
        }
        Err(error) => return ServiceResponse::error("snapshot_failed", error),
    };
    let args = parse_arguments(request.args.get("arguments").map_or("", String::as_str));
    let (operation_kind, cutoff_sequence, selected_source_sequence, default_prefix) = match request
        .command_id
        .as_str()
    {
        CLONE_COMMAND_ID => ("clone", snapshot.latest_sequence, None, "clone"),
        FORK_COMMAND_ID => {
            let Some(sequence) = args
                .get("sequence")
                .and_then(|value| value.parse::<u64>().ok())
            else {
                return prompt_selection_fallback(
                    context,
                    &request.command_id,
                    session_id,
                    snapshot.generation,
                );
            };
            ("fork", sequence.saturating_sub(1), Some(sequence), "fork")
        }
        _ => {
            return ServiceResponse::error("unknown_command", "unknown session derivation command");
        }
    };
    let destination_name = args.get("name").cloned().or_else(|| {
        snapshot
            .title
            .as_ref()
            .map(|title| format!("[{default_prefix}] {title}"))
    });
    let authoritative_draft = match selected_source_sequence {
        Some(sequence) => match call_derivation_service(
            context,
            &request.command_id,
            SessionDerivationServiceRequest::Prompt {
                session_id,
                generation: snapshot.generation,
                sequence,
            },
        ) {
            Ok(SessionDerivationServiceResponse::Prompt { text }) => Some(text),
            Ok(_) => {
                return ServiceResponse::error("unexpected_response", "unexpected prompt response");
            }
            Err(error) => return ServiceResponse::error("prompt_failed", error),
        },
        None => None,
    };
    let derive = SessionDerivationRequest {
        version: SESSION_DERIVATION_CONTRACT_VERSION,
        operation_id: SessionDerivationOperationId::new(),
        idempotency_key: format!("{}-{}", operation_kind, SessionDerivationOperationId::new()),
        source: snapshot,
        source_policy: SessionDerivationSourcePolicy::ExactGeneration,
        cutoff_sequence,
        destination_working_directory: None,
        destination_name,
        initial_draft: authoritative_draft,
        lineage: SessionDerivationLineage {
            producer: PLUGIN_ID.to_owned(),
            operation_kind: format!("{PLUGIN_ID}/{operation_kind}"),
            selected_source_sequence,
        },
    };
    let outcome = match call_derivation_service(
        context,
        &request.command_id,
        SessionDerivationServiceRequest::Derive {
            request: Box::new(derive),
        },
    ) {
        Ok(SessionDerivationServiceResponse::Derived { outcome }) => outcome,
        Ok(_) => {
            return ServiceResponse::error("unexpected_response", "unexpected derivation response");
        }
        Err(error) => return ServiceResponse::error("derivation_failed", error),
    };
    let SessionDerivationTerminalOutcome::Succeeded { session } = outcome else {
        return ServiceResponse::error(
            "derivation_incomplete",
            "session derivation did not succeed",
        );
    };
    json_response(&InvokeCommandResponse {
        success: true,
        message: Some(format!("Created {}", session.display_title())),
        updated_model: None,
        updated_provider: None,
        updated_thinking: None,
        effects: vec![CommandEffect::OpenSession {
            session_id: session.id,
            focus: if operation_kind == "fork" {
                SessionOpenFocus::Composer
            } else {
                SessionOpenFocus::Default
            },
        }],
    })
}

fn prompt_selection_fallback(
    context: &NativeServiceContext,
    invocation_id: &str,
    session_id: bcode_session_models::SessionId,
    generation: u64,
) -> ServiceResponse {
    let page = match call_derivation_service(
        context,
        invocation_id,
        SessionDerivationServiceRequest::Prompts {
            session_id,
            query: bcode_session_models::SessionDerivationPromptQuery {
                generation,
                before_sequence: None,
                limit: 50,
            },
        },
    ) {
        Ok(SessionDerivationServiceResponse::Prompts { page }) => page,
        Ok(_) => {
            return ServiceResponse::error(
                "unexpected_response",
                "unexpected prompt page response",
            );
        }
        Err(error) => return ServiceResponse::error("prompts_failed", error),
    };
    if page.candidates.is_empty() {
        return ServiceResponse::error("no_prompts", "no user prompts are available to fork");
    }
    let mut text = String::from("Select a prompt sequence and run `/fork sequence=<n>`:\n\n");
    for candidate in &page.candidates {
        let preview = candidate.preview.replace('\n', " ");
        let _ = writeln!(text, "* `{}` — {}", candidate.sequence, preview);
    }
    json_response(&InvokeCommandResponse {
        success: true,
        message: Some("Select a prompt to fork".to_owned()),
        updated_model: None,
        updated_provider: None,
        updated_thinking: None,
        effects: vec![
            CommandEffect::AppendText {
                text,
                format: bcode_command::CommandTextFormat::Markdown,
            },
            CommandEffect::OpenPluginSurface {
                surface_kind: "session-derivation.fork-select".to_owned(),
                instance_id: format!("fork-select-{session_id}"),
                options: serde_json::json!({
                    "session_id": session_id,
                    "generation": generation,
                    "candidates": page.candidates,
                }),
            },
        ],
    })
}

fn call_derivation_service(
    context: &NativeServiceContext,
    invocation_id: &str,
    request: SessionDerivationServiceRequest,
) -> Result<SessionDerivationServiceResponse, String> {
    let payload = serde_json::to_value(request).map_err(|error| error.to_string())?;
    let response = context
        .bridge
        .request(&ServiceBridgeRequest::InvokeService(
            ToolInvocationServiceRequest {
                invocation_id: invocation_id.to_owned(),
                request_id: format!("{invocation_id}-session-derivation"),
                route_id: None,
                interface_id: SESSION_DERIVATION_INTERFACE_ID.to_owned(),
                operation: "request".to_owned(),
                payload,
            },
        ))
        .map_err(|error| error.to_string())?;
    match response {
        ServiceBridgeResponse::Service(ToolInvocationServiceResolution::Responded { payload }) => {
            serde_json::from_value(payload).map_err(|error| error.to_string())
        }
        ServiceBridgeResponse::Service(ToolInvocationServiceResolution::Failed {
            message, ..
        }) => Err(message),
        ServiceBridgeResponse::Service(ToolInvocationServiceResolution::Cancelled) => {
            Err("session derivation cancelled".to_owned())
        }
        _ => Err("session derivation service unavailable".to_owned()),
    }
}

fn parse_arguments(arguments: &str) -> BTreeMap<String, String> {
    arguments
        .split_whitespace()
        .filter_map(|part| part.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

fn json_response<T: serde::Serialize>(value: &T) -> ServiceResponse {
    ServiceResponse::json(value)
        .unwrap_or_else(|error| ServiceResponse::error("encode_failed", error.to_string()))
}

#[must_use]
pub fn session_derivation_tui_registry() -> bcode_plugin_sdk::tui::PluginTuiRegistry {
    let mut registry = bcode_plugin_sdk::tui::PluginTuiRegistry::default();
    registry.register_factory(Box::new(ForkSelectSurfaceFactory));
    registry
}

struct ForkSelectSurfaceFactory;

impl bcode_plugin_sdk::tui::PluginTuiSurfaceFactory for ForkSelectSurfaceFactory {
    fn surface_kind(&self) -> &'static str {
        "session-derivation.fork-select"
    }

    fn open(
        &self,
        request: bcode_plugin_sdk::tui::PluginTuiSurfaceOpenRequest,
    ) -> bcode_plugin_sdk::tui::PluginTuiSurfaceFuture {
        Box::pin(async move {
            let candidates = serde_json::from_value::<
                Vec<bcode_session_models::SessionDerivationPromptCandidate>,
            >(
                request
                    .options
                    .get("candidates")
                    .cloned()
                    .unwrap_or_default(),
            )?;
            Ok(Box::new(ForkSelectSurface {
                candidates,
                selected: 0,
            })
                as bcode_plugin_sdk::tui::BoxedPluginTuiSurface)
        })
    }
}

struct ForkSelectSurface {
    candidates: Vec<bcode_session_models::SessionDerivationPromptCandidate>,
    selected: usize,
}

impl ForkSelectSurface {
    fn selected_outcome(&self) -> Option<serde_json::Value> {
        let candidate = self.candidates.get(self.selected)?;
        let outcome = bcode_plugin_sdk::tui::PluginTuiSurfaceOutcome {
            status: Some("creating fork…".to_owned()),
            append_text: None,
            invoke_command: Some(CommandAction::Plugin {
                plugin_id: PLUGIN_ID.to_owned(),
                command_id: FORK_COMMAND_ID.to_owned(),
            }),
            command_args: BTreeMap::from([("sequence".to_owned(), candidate.sequence.to_string())]),
            set_session_working_directory: None,
        };
        serde_json::to_value(outcome).ok()
    }
}

impl bcode_plugin_sdk::tui::PluginTuiSurface for ForkSelectSurface {
    fn id(&self) -> &'static str {
        "session-derivation.fork-select"
    }

    fn title(&self) -> &'static str {
        "Fork Session"
    }

    fn render(&mut self, area: Rect, frame: &mut Frame<'_>) {
        frame.fill(area, " ", Style::new());
        frame.write_line(
            Rect::new(area.x, area.y, area.width, 1),
            &Line::from_spans(vec![Span::styled(
                "Select the prompt to edit in the fork",
                Style::new().add_modifier(Modifier::BOLD),
            )]),
        );
        for (index, candidate) in self
            .candidates
            .iter()
            .take(usize::from(area.height.saturating_sub(2)))
            .enumerate()
        {
            let style = if index == self.selected {
                Style::new().add_modifier(Modifier::REVERSED)
            } else {
                Style::new()
            };
            frame.write_line(
                Rect::new(
                    area.x,
                    area.y
                        .saturating_add(u16::try_from(index + 2).unwrap_or(u16::MAX)),
                    area.width,
                    1,
                ),
                &Line::from_spans(vec![Span::styled(
                    format!(
                        "{}  {}",
                        candidate.sequence,
                        candidate.preview.replace('\n', " ")
                    ),
                    style,
                )]),
            );
        }
    }

    fn render_with_theme(
        &mut self,
        area: Rect,
        frame: &mut Frame<'_>,
        theme: Option<bcode_plugin_sdk::tui::PluginTuiTheme>,
    ) {
        let canvas = theme.map_or_else(Style::new, |theme| theme.canvas);
        let text = theme.map_or_else(Style::new, |theme| theme.text);
        let selection = theme.map_or_else(
            || Style::new().add_modifier(Modifier::REVERSED),
            |theme| theme.selection,
        );
        frame.fill(area, " ", canvas);
        frame.write_line(
            Rect::new(area.x, area.y, area.width, 1),
            &Line::from_spans(vec![Span::styled(
                "Select the prompt to edit in the fork",
                text.add_modifier(Modifier::BOLD),
            )]),
        );
        for (index, candidate) in self
            .candidates
            .iter()
            .take(usize::from(area.height.saturating_sub(2)))
            .enumerate()
        {
            let style = if index == self.selected {
                selection
            } else {
                text
            };
            frame.write_line(
                Rect::new(
                    area.x,
                    area.y
                        .saturating_add(u16::try_from(index + 2).unwrap_or(u16::MAX)),
                    area.width,
                    1,
                ),
                &Line::from_spans(vec![Span::styled(
                    format!(
                        "{}  {}",
                        candidate.sequence,
                        candidate.preview.replace('\n', " ")
                    ),
                    style,
                )]),
            );
        }
    }

    fn handle_event(
        &mut self,
        event: &Event,
        _host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
    ) -> bcode_plugin_sdk::tui::PluginTuiAction {
        let Event::Key(key) = event else {
            return bcode_plugin_sdk::tui::PluginTuiAction::None;
        };
        match key.key {
            KeyCode::Escape | KeyCode::Char('q') => {
                bcode_plugin_sdk::tui::PluginTuiAction::Close { outcome: None }
            }
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                bcode_plugin_sdk::tui::PluginTuiAction::Redraw
            }
            KeyCode::Down => {
                self.selected = self
                    .selected
                    .saturating_add(1)
                    .min(self.candidates.len().saturating_sub(1));
                bcode_plugin_sdk::tui::PluginTuiAction::Redraw
            }
            KeyCode::Enter => {
                if self.candidates.get(self.selected).is_none() {
                    return bcode_plugin_sdk::tui::PluginTuiAction::None;
                }
                bcode_plugin_sdk::tui::PluginTuiAction::Close {
                    outcome: self.selected_outcome(),
                }
            }
            _ => bcode_plugin_sdk::tui::PluginTuiAction::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_plugin_owned_first_class_entries() {
        let commands = commands();
        assert_eq!(commands.len(), 2);
        for (command, slash) in commands.iter().zip(["fork", "clone"]) {
            assert_eq!(command.slash_name(), Some(slash));
            assert_eq!(command.session, CommandSessionRequirement::Required);
            assert!(command.surfaces.contains(&CommandSurface::Palette));
            assert!(command.surfaces.contains(&CommandSurface::Slash));
            assert!(matches!(
                &command.action,
                CommandAction::Plugin { plugin_id, .. } if plugin_id == PLUGIN_ID
            ));
        }
    }

    #[test]
    fn fork_surface_selection_returns_plugin_command_continuation() {
        let mut surface = ForkSelectSurface {
            candidates: vec![
                bcode_session_models::SessionDerivationPromptCandidate {
                    sequence: 4,
                    timestamp_ms: 1,
                    preview: "first".to_owned(),
                    truncated: false,
                },
                bcode_session_models::SessionDerivationPromptCandidate {
                    sequence: 9,
                    timestamp_ms: 2,
                    preview: "second".to_owned(),
                    truncated: false,
                },
            ],
            selected: 0,
        };
        surface.selected = 1;
        let outcome: bcode_plugin_sdk::tui::PluginTuiSurfaceOutcome =
            serde_json::from_value(surface.selected_outcome().expect("outcome"))
                .expect("typed outcome");
        assert_eq!(
            outcome.command_args.get("sequence").map(String::as_str),
            Some("9")
        );
        assert!(matches!(
            outcome.invoke_command,
            Some(CommandAction::Plugin { ref plugin_id, ref command_id })
                if plugin_id == PLUGIN_ID && command_id == FORK_COMMAND_ID
        ));
    }

    #[test]
    fn renderer_neutral_argument_parser_preserves_sequence_and_name() {
        assert_eq!(
            parse_arguments("sequence=42 name=branch"),
            BTreeMap::from([
                ("name".to_owned(), "branch".to_owned()),
                ("sequence".to_owned(), "42".to_owned()),
            ])
        );
    }
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_plugin_vtable!(
        SessionDerivationPlugin,
        include_str!("../bcode-plugin.toml")
    )
}

#[cfg(not(feature = "static-bundled"))]
bcode_plugin_sdk::export_plugin!(
    SessionDerivationPlugin,
    include_str!("../bcode-plugin.toml")
);
