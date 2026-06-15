#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Bundled Ralph TUI plugin for Bcode.

use std::path::PathBuf;

use bcode_plugin_sdk::prelude::*;
use bcode_plugin_sdk::tui::{
    PluginTuiAction, PluginTuiHost, PluginTuiRegistry, PluginTuiSurface, PluginTuiSurfaceFactory,
    PluginTuiSurfaceFuture, PluginTuiSurfaceOpenRequest,
};
use bmux_keyboard::KeyCode;
use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};

/// Ralph home native TUI surface kind.
pub const RALPH_HOME_SURFACE_KIND: &str = "ralph-home";

/// Register native TUI surfaces contributed by the Ralph plugin.
#[must_use]
pub fn tui_registry() -> PluginTuiRegistry {
    let mut registry = PluginTuiRegistry::default();
    registry.register_factory(Box::new(RalphHomeSurfaceFactory));
    registry
}

const RALPH_ACTIONS: &[RalphAction] = &[
    RalphAction::new("Start/setup loop", "/ralph start"),
    RalphAction::new("Run autonomous loop", "/ralph run"),
    RalphAction::new("Approve prepared run", "/ralph approve"),
    RalphAction::new("Stop active run", "/ralph stop"),
    RalphAction::new("Resume safely", "/ralph resume"),
    RalphAction::new("Show status", "/ralph status"),
    RalphAction::new("List runs", "/ralph runs"),
    RalphAction::new("List iterations", "/ralph iterations"),
    RalphAction::new("Open progress doc", "/ralph open"),
    RalphAction::new("Build audit prompt", "/ralph audit"),
    RalphAction::new("Build replan prompt", "/ralph replan"),
    RalphAction::new("Goal workflow", "/goal"),
];

#[derive(Debug, Clone, Copy)]
struct RalphAction {
    label: &'static str,
    command: &'static str,
}

impl RalphAction {
    const fn new(label: &'static str, command: &'static str) -> Self {
        Self { label, command }
    }
}

/// Ralph plugin implementation.
#[derive(Debug, Default)]
pub struct RalphPlugin;

impl RustPlugin for RalphPlugin {
    fn activate(&mut self) -> Result<(), PluginError> {
        Ok(())
    }

    fn deactivate(&mut self) -> Result<(), PluginError> {
        Ok(())
    }
}

#[derive(Debug, Default)]
struct RalphHomeSurfaceFactory;

impl PluginTuiSurfaceFactory for RalphHomeSurfaceFactory {
    fn surface_kind(&self) -> &'static str {
        RALPH_HOME_SURFACE_KIND
    }

    fn open(&self, request: PluginTuiSurfaceOpenRequest) -> PluginTuiSurfaceFuture {
        Box::pin(async move {
            let repo_path = request
                .repo_path
                .ok_or("Ralph home surface requires repo_path")?;
            Ok(Box::new(RalphHomeSurface::load(repo_path))
                as bcode_plugin_sdk::tui::BoxedPluginTuiSurface)
        })
    }
}

#[derive(Debug, Clone)]
struct RalphHomeSurface {
    repo_path: PathBuf,
    loop_summary: Option<bcode_ralph::RalphLoopSummary>,
    runs: Vec<bcode_ralph::RalphRunRecord>,
    selected_action: usize,
    status_message: Option<String>,
}

impl RalphHomeSurface {
    fn load(repo_path: PathBuf) -> Self {
        let mut surface = Self {
            repo_path,
            loop_summary: None,
            runs: Vec::new(),
            selected_action: 0,
            status_message: None,
        };
        surface.refresh();
        surface
    }

    fn refresh(&mut self) {
        match bcode_ralph::latest_loop(&self.repo_path) {
            Ok(Some(summary)) => {
                self.runs =
                    bcode_ralph::list_runs_for_loop(&summary.state_dir).unwrap_or_else(|error| {
                        self.status_message = Some(format!("failed to list Ralph runs: {error}"));
                        Vec::new()
                    });
                self.loop_summary = Some(summary);
                if self.status_message.is_none() {
                    self.status_message = Some("Ralph status refreshed".to_owned());
                }
            }
            Ok(None) => {
                self.loop_summary = None;
                self.runs.clear();
                self.status_message = Some("No Ralph loop found for this repository".to_owned());
            }
            Err(error) => {
                self.loop_summary = None;
                self.runs.clear();
                self.status_message = Some(format!("failed to load Ralph status: {error}"));
            }
        }
    }
    fn render_actions(&self, frame: &mut Frame<'_>, area: Rect, mut y: u16) -> u16 {
        write_line(
            frame,
            area,
            y,
            Line::from_spans(vec![Span::styled(
                "Actions",
                Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
            )]),
        );
        y = y.saturating_add(1);
        for (index, action) in RALPH_ACTIONS.iter().enumerate() {
            let selected = index == self.selected_action;
            let marker = if selected { "›" } else { " " };
            let style = if selected {
                Style::new().fg(Color::Black).bg(Color::White)
            } else {
                Style::new().fg(Color::White).bg(Color::Black)
            };
            write_line(
                frame,
                area,
                y,
                Line::from_spans(vec![Span::styled(
                    format!("{marker} {:<24} {}", action.label, action.command),
                    style,
                )]),
            );
            y = y.saturating_add(1);
        }
        y.saturating_add(1)
    }
}

impl PluginTuiSurface for RalphHomeSurface {
    fn id(&self) -> &'static str {
        RALPH_HOME_SURFACE_KIND
    }

    fn title(&self) -> &'static str {
        "Ralph"
    }

    fn render(&mut self, area: Rect, frame: &mut Frame<'_>) {
        frame.fill(area, " ", Style::new().fg(Color::White).bg(Color::Black));
        let mut y = area.y;
        write_line(
            frame,
            area,
            y,
            Line::from_spans(vec![Span::styled(
                "Ralph autonomous workflow",
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            )]),
        );
        y = y.saturating_add(2);
        write_line(
            frame,
            area,
            y,
            Line::from(format!("Repo: {}", self.repo_path.display())),
        );
        y = y.saturating_add(2);
        y = self.render_actions(frame, area, y);

        if let Some(summary) = &self.loop_summary {
            write_line(
                frame,
                area,
                y,
                Line::from(format!("Loop: {}", summary.loop_name)),
            );
            y = y.saturating_add(1);
            write_line(
                frame,
                area,
                y,
                Line::from(format!("State: {}", summary.state_dir.display())),
            );
            y = y.saturating_add(1);
            write_line(
                frame,
                area,
                y,
                Line::from(format!(
                    "Limits: max iterations {}, no-progress {}",
                    summary.max_iterations, summary.no_progress_limit
                )),
            );
            y = y.saturating_add(2);
            write_line(
                frame,
                area,
                y,
                Line::from_spans(vec![Span::styled(
                    "Recent runs",
                    Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                )]),
            );
            y = y.saturating_add(1);
            if self.runs.is_empty() {
                write_line(frame, area, y, Line::from("  <none>"));
            } else {
                for run in self.runs.iter().take(8) {
                    write_line(
                        frame,
                        area,
                        y,
                        Line::from(format!(
                            "  {}  {}{}{}",
                            run.run_id,
                            run.status,
                            run.stop_reason
                                .as_deref()
                                .map_or_else(String::new, |reason| format!(" ({reason})")),
                            if run.cancel_requested {
                                " [cancel requested]"
                            } else {
                                ""
                            }
                        )),
                    );
                    y = y.saturating_add(1);
                }
            }
        } else {
            write_line(frame, area, y, Line::from("No Ralph loop configured."));
        }

        let status_y = area.y.saturating_add(area.height.saturating_sub(2));
        write_line(
            frame,
            area,
            status_y,
            Line::from("Keys: ↑/↓ select · Enter run · r refresh · q close"),
        );
        if let Some(message) = &self.status_message {
            write_line(
                frame,
                area,
                status_y.saturating_add(1),
                Line::from(message.clone()),
            );
        }
    }

    fn handle_event(&mut self, event: &Event, _host: &dyn PluginTuiHost) -> PluginTuiAction {
        let Event::Key(key) = event else {
            return PluginTuiAction::None;
        };
        match key.key {
            KeyCode::Char('q') | KeyCode::Escape => PluginTuiAction::Close { outcome: None },
            KeyCode::Char('r') => {
                self.refresh();
                PluginTuiAction::Redraw
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.selected_action = self.selected_action.saturating_sub(1);
                PluginTuiAction::Redraw
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.selected_action = (self.selected_action + 1).min(RALPH_ACTIONS.len() - 1);
                PluginTuiAction::Redraw
            }
            KeyCode::Enter => PluginTuiAction::RunCommand {
                command: RALPH_ACTIONS[self.selected_action].command.to_owned(),
            },
            KeyCode::Char('s') => PluginTuiAction::RunCommand {
                command: "/ralph status".to_owned(),
            },
            KeyCode::Char('g') => PluginTuiAction::RunCommand {
                command: "/goal".to_owned(),
            },
            _ => PluginTuiAction::None,
        }
    }
}

fn write_line(frame: &mut Frame<'_>, area: Rect, y: u16, line: impl Into<Line>) {
    if y >= area.y.saturating_add(area.height) {
        return;
    }
    let line = line.into();
    frame.write_line(Rect::new(area.x, y, area.width, 1), &line);
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    let mut vtable =
        bcode_plugin_sdk::static_plugin_vtable!(RalphPlugin, include_str!("../bcode-plugin.toml"));
    vtable.tui_registry = Some(tui_registry);
    vtable
}

bcode_plugin_sdk::export_plugin!(RalphPlugin, include_str!("../bcode-plugin.toml"));
