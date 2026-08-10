#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Ralph TUI plugin for Bcode.

#[cfg(feature = "static-bundled")]
mod cli;

use std::path::PathBuf;

use bcode_plugin_sdk::path::{display, display_from_current_dir};
use bcode_plugin_sdk::prelude::*;
use bcode_plugin_sdk::tui::{
    PluginTuiAction, PluginTuiHost, PluginTuiRegistry, PluginTuiSurface, PluginTuiSurfaceFactory,
    PluginTuiSurfaceFuture, PluginTuiSurfaceOpenRequest,
};
use bmux_keyboard::KeyCode;
use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
#[cfg(test)]
use bmux_tui::prelude::Color;
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::Modifier;
use bmux_tui_components::key_hint_bar::{KeyHint, KeyHintBar, KeyHintBarStyles};
use bmux_tui_components::selectable_list::{
    SelectableList, SelectableListItem, SelectableListOutcome, SelectableListState,
    SelectableListStyles,
};

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
    RalphAction::new("Plan/setup loop", RalphActionKind::Plan),
    RalphAction::new("Save setup draft", RalphActionKind::SaveDraft),
    RalphAction::new("View setup draft", RalphActionKind::ViewDraft),
    RalphAction::new("Revise setup draft", RalphActionKind::ReviseDraft),
    RalphAction::new("Rebuild loop context", RalphActionKind::RebuildLoopContext),
    RalphAction::new("Approve setup draft", RalphActionKind::ApproveDraft),
    RalphAction::new("Apply draft to loop", RalphActionKind::ApplyDraftToLoop),
    RalphAction::new("Create loop from draft", RalphActionKind::CreateFromDraft),
    RalphAction::new("Quick create loop", RalphActionKind::Start),
    RalphAction::new("Prepare run", RalphActionKind::Run),
    RalphAction::new("Approve/start run", RalphActionKind::Approve),
    RalphAction::new("Stop active run", RalphActionKind::Stop),
    RalphAction::new("Resume safely", RalphActionKind::Resume),
    RalphAction::new("Show status", RalphActionKind::Status),
    RalphAction::new("List runs", RalphActionKind::Runs),
    RalphAction::new("List iterations", RalphActionKind::Iterations),
    RalphAction::new("Open progress doc", RalphActionKind::Open),
    RalphAction::new("Audit alignment", RalphActionKind::Audit),
    RalphAction::new("Replan from charter", RalphActionKind::Replan),
    RalphAction::new("Goal workflow", RalphActionKind::Goal),
];

fn action_for_kind(kind: RalphActionKind) -> Option<&'static RalphAction> {
    RALPH_ACTIONS.iter().find(|action| action.kind == kind)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum RalphActionKind {
    Plan,
    SaveDraft,
    ViewDraft,
    ReviseDraft,
    RebuildLoopContext,
    ApproveDraft,
    ApplyDraftToLoop,
    CreateFromDraft,
    Start,
    Run,
    Approve,
    Stop,
    Resume,
    Status,
    Runs,
    Iterations,
    Open,
    Audit,
    Replan,
    Goal,
}

impl RalphActionKind {
    const fn command_label(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::SaveDraft => "save-draft",
            Self::ViewDraft => "view-draft",
            Self::ReviseDraft => "revise-draft",
            Self::RebuildLoopContext => "rebuild-loop-context",
            Self::ApproveDraft => "approve-draft",
            Self::ApplyDraftToLoop => "apply-draft-to-loop",
            Self::CreateFromDraft => "create-from-draft",
            Self::Start => "start",
            Self::Run => "run",
            Self::Approve => "approve",
            Self::Stop => "stop",
            Self::Resume => "resume",
            Self::Status => "status",
            Self::Runs => "runs",
            Self::Iterations => "iterations",
            Self::Open => "open",
            Self::Audit => "audit",
            Self::Replan => "replan",
            Self::Goal => "goal",
        }
    }

    const fn description(self) -> &'static str {
        match self {
            Self::Plan => {
                "start guided LLM setup: clarify goal, draft charter/progress, then approve"
            }
            Self::SaveDraft => {
                "capture latest assistant charter/progress draft for review/approval"
            }
            Self::ViewDraft => "show saved setup draft paths and content previews",
            Self::ReviseDraft => "ask the assistant to revise the saved setup draft",
            Self::RebuildLoopContext => "restart guided context building for the existing loop",
            Self::ApproveDraft => "approve saved charter/progress as ready for loop creation",
            Self::ApplyDraftToLoop => {
                "replace existing loop charter/progress from approved rebuild draft"
            }
            Self::CreateFromDraft => "create loop files/worktree from the approved setup draft",
            Self::Start => {
                "quick-create starter docs/worktree from recent context; advanced fallback"
            }
            Self::Run => "prepare an approval-gated autonomous run; does not start work yet",
            Self::Approve => "approve/start the prepared run",
            Self::Stop => "request cancellation for an active run",
            Self::Resume => "resume an interrupted run safely",
            Self::Status => "show the latest loop and active-run status in chat",
            Self::Runs => "list recent runs and their states",
            Self::Iterations => "list iterations, validation, and stop reasons",
            Self::Open => "open/copy the mutable progress doc path",
            Self::Audit => "check repo/progress alignment against the immutable charter",
            Self::Replan => "recalibrate the progress doc against the immutable charter",
            Self::Goal => "prepare a run from the current goal workflow",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct RalphAction {
    label: &'static str,
    kind: RalphActionKind,
}

impl RalphAction {
    const fn new(label: &'static str, kind: RalphActionKind) -> Self {
        Self { label, kind }
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
            let flash_message = request
                .options
                .get("flash_message")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned);
            Ok(Box::new(RalphHomeSurface::load(repo_path, flash_message))
                as bcode_plugin_sdk::tui::BoxedPluginTuiSurface)
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RalphHomeScreen {
    Dashboard,
    RebuildIntro,
}

struct RalphSurfaceTheme {
    canvas: Style,
    text: Style,
    focused: Style,
    selection: Style,
}

impl RalphSurfaceTheme {
    fn resolve(theme: Option<bcode_plugin_sdk::tui::PluginTuiTheme>) -> Self {
        theme.map_or_else(
            || Self {
                canvas: Style::new(),
                text: Style::new(),
                focused: Style::new().add_modifier(Modifier::BOLD),
                selection: Style::new().add_modifier(Modifier::REVERSED),
            },
            |theme| Self {
                canvas: theme.canvas,
                text: theme.text,
                focused: theme.focused,
                selection: theme.selection,
            },
        )
    }
}

#[derive(Debug, Clone)]
struct RalphHomeSurface {
    repo_path: PathBuf,
    loop_summary: Option<bcode_ralph::RalphLoopSummary>,
    setup_draft: Option<bcode_ralph::RalphSetupDraft>,
    runs: Vec<bcode_ralph::RalphRunRecord>,
    selected_action: usize,
    action_area: Rect,
    status_message: Option<String>,
    screen: RalphHomeScreen,
}

impl RalphHomeSurface {
    fn load(repo_path: PathBuf, flash_message: Option<String>) -> Self {
        let mut surface = Self {
            repo_path,
            loop_summary: None,
            setup_draft: None,
            runs: Vec::new(),
            selected_action: 0,
            action_area: Rect::new(0, 0, 0, 0),
            status_message: flash_message,
            screen: RalphHomeScreen::Dashboard,
        };
        surface.refresh();
        surface
    }

    fn refresh(&mut self) {
        self.setup_draft = bcode_ralph::latest_setup_draft(&self.repo_path)
            .ok()
            .flatten()
            .filter(|draft| {
                !matches!(
                    draft.status,
                    bcode_ralph::RalphSetupDraftStatus::Canceled
                        | bcode_ralph::RalphSetupDraftStatus::ConvertedToLoop
                )
            });
        match bcode_ralph::latest_loop(&self.repo_path) {
            Ok(Some(summary)) => {
                self.runs =
                    bcode_ralph::list_runs_for_loop(&summary.state_dir).unwrap_or_else(|error| {
                        self.status_message = Some(format!("failed to list Ralph runs: {error}"));
                        Vec::new()
                    });
                self.loop_summary = Some(summary);
                self.selected_action = self.selected_action.min(self.action_order().len() - 1);
                if self.status_message.is_none() {
                    self.status_message = Some("Ralph status refreshed".to_owned());
                }
            }
            Ok(None) => {
                self.loop_summary = None;
                self.runs.clear();
                self.selected_action = 0;
                self.status_message = Some("No Ralph loop found for this repository".to_owned());
            }
            Err(error) => {
                self.loop_summary = None;
                self.runs.clear();
                self.selected_action = 0;
                self.status_message = Some(format!("failed to load Ralph status: {error}"));
            }
        }
    }

    fn latest_run(&self) -> Option<&bcode_ralph::RalphRunRecord> {
        self.runs
            .iter()
            .max_by_key(|run| (run.updated_at_ms, run.started_at_ms))
    }

    fn next_step(&self) -> &'static str {
        if let Some(draft) = &self.setup_draft {
            return match draft.status {
                bcode_ralph::RalphSetupDraftStatus::CollectingContext
                | bcode_ralph::RalphSetupDraftStatus::Clarifying => {
                    "Guided setup draft exists. Next: answer the assistant's clarifying questions in chat."
                }
                bcode_ralph::RalphSetupDraftStatus::Drafting => {
                    "Guided setup is drafting. Next: review the generated charter/progress draft."
                }
                bcode_ralph::RalphSetupDraftStatus::DraftReady => {
                    "Setup draft is saved. Next: review it, then approve setup draft."
                }
                bcode_ralph::RalphSetupDraftStatus::Approved => {
                    "Setup draft is approved. Next: create the loop from the approved draft."
                }
                bcode_ralph::RalphSetupDraftStatus::Canceled
                | bcode_ralph::RalphSetupDraftStatus::ConvertedToLoop => {
                    "Start a new guided setup draft or use the existing loop below."
                }
            };
        }
        let Some(_summary) = &self.loop_summary else {
            return "Plan/setup a loop with the assistant; quick-create is only an advanced fallback.";
        };
        let Some(run) = self.latest_run() else {
            return "Setup is complete. Next: prepare a run. Preparing does not start work until you approve it.";
        };
        if run.cancel_requested {
            return "Cancellation is requested. Next: refresh status, then resume, audit, or replan.";
        }
        match run.status.as_str() {
            "awaiting_approval" | "prepared" | "queued" => {
                "A run is prepared. Next: approve/start the prepared run."
            }
            "running" => "A run is active. Next: watch status/iterations, or stop if needed.",
            "interrupted" | "blocked" | "failed" | "stopped" => {
                "The latest run is not running. Next: resume safely, audit alignment, or replan from the charter."
            }
            "completed" | "done" => {
                "Latest run is complete. Next: audit alignment before considering the loop done."
            }
            _ => "Review the latest run status, then choose the safest available action below.",
        }
    }

    fn action_order(&self) -> Vec<&'static RalphAction> {
        let no_loop = self.loop_summary.is_none();
        let has_draft = self.setup_draft.is_some();
        let latest_status = self.latest_run().map(|run| run.status.as_str());
        let mut kinds = Vec::new();

        if has_draft {
            kinds.extend([
                RalphActionKind::SaveDraft,
                RalphActionKind::ViewDraft,
                RalphActionKind::ReviseDraft,
                RalphActionKind::ApproveDraft,
            ]);
            if no_loop {
                kinds.push(RalphActionKind::CreateFromDraft);
            } else {
                kinds.push(RalphActionKind::ApplyDraftToLoop);
            }
        }

        if no_loop {
            kinds.extend([
                RalphActionKind::Plan,
                RalphActionKind::Start,
                RalphActionKind::Status,
            ]);
        } else {
            kinds.push(RalphActionKind::RebuildLoopContext);
            match latest_status {
                None => kinds.extend([
                    RalphActionKind::Run,
                    RalphActionKind::Open,
                    RalphActionKind::Status,
                    RalphActionKind::Audit,
                    RalphActionKind::Replan,
                    RalphActionKind::Runs,
                    RalphActionKind::Iterations,
                    RalphActionKind::Start,
                ]),
                Some("awaiting_approval" | "prepared" | "queued") => kinds.extend([
                    RalphActionKind::Approve,
                    RalphActionKind::Open,
                    RalphActionKind::Status,
                    RalphActionKind::Runs,
                    RalphActionKind::Iterations,
                    RalphActionKind::Stop,
                    RalphActionKind::Audit,
                    RalphActionKind::Replan,
                ]),
                Some("running") => kinds.extend([
                    RalphActionKind::Status,
                    RalphActionKind::Iterations,
                    RalphActionKind::Stop,
                    RalphActionKind::Open,
                    RalphActionKind::Runs,
                    RalphActionKind::Audit,
                ]),
                Some("interrupted" | "blocked" | "failed" | "stopped") => kinds.extend([
                    RalphActionKind::Resume,
                    RalphActionKind::Audit,
                    RalphActionKind::Replan,
                    RalphActionKind::Status,
                    RalphActionKind::Iterations,
                    RalphActionKind::Open,
                    RalphActionKind::Run,
                ]),
                Some("completed" | "done") => kinds.extend([
                    RalphActionKind::Audit,
                    RalphActionKind::Open,
                    RalphActionKind::Status,
                    RalphActionKind::Iterations,
                    RalphActionKind::Replan,
                    RalphActionKind::Run,
                ]),
                Some(_) => kinds.extend([
                    RalphActionKind::Status,
                    RalphActionKind::Runs,
                    RalphActionKind::Iterations,
                    RalphActionKind::Open,
                    RalphActionKind::Run,
                    RalphActionKind::Audit,
                    RalphActionKind::Replan,
                ]),
            }
        }

        kinds.into_iter().filter_map(action_for_kind).collect()
    }

    fn render_current_draft(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        mut y: u16,
        theme: &RalphSurfaceTheme,
    ) -> u16 {
        let Some(draft) = &self.setup_draft else {
            return y;
        };
        let readiness = draft.readiness();
        write_line(
            frame,
            area,
            y,
            Line::from_spans(vec![Span::styled(
                "Setup draft",
                theme.focused.add_modifier(Modifier::BOLD),
            )]),
        );
        y = y.saturating_add(1);
        for line in [
            format!("  Draft: {}", draft.draft_id),
            format!("  Status: {}", draft.status),
            format!("  Mode: {}", draft.mode),
            format!("  Proposed loop: {}", draft.loop_name),
            format!(
                "  Branch: {}",
                draft.branch.as_deref().unwrap_or("<default>")
            ),
            format!(
                "  Worktree: {}",
                draft.work_area_path.as_ref().map_or_else(
                    || "<default>".to_owned(),
                    |path| display(path, &self.repo_path).to_string()
                )
            ),
            format!(
                "  Ready: charter={} progress={} approved={}",
                readiness.has_charter, readiness.has_progress, readiness.approved
            ),
            format!("  Path: {}", display(&draft.draft_path, &self.repo_path)),
        ] {
            write_line(frame, area, y, Line::from(line));
            y = y.saturating_add(1);
        }
        y
    }

    fn render_runs(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        mut y: u16,
        theme: &RalphSurfaceTheme,
    ) -> u16 {
        write_line(
            frame,
            area,
            y,
            Line::from_spans(vec![Span::styled(
                "Runs",
                theme.focused.add_modifier(Modifier::BOLD),
            )]),
        );
        y = y.saturating_add(1);
        if self.runs.is_empty() {
            write_line(frame, area, y, Line::from("  <none yet>"));
            return y.saturating_add(1);
        }
        for run in self.runs.iter().take(6) {
            write_line(
                frame,
                area,
                y,
                Line::from(format!(
                    "  {}  {}{}{}  session {}",
                    run.run_id,
                    run.status,
                    run.stop_reason
                        .as_deref()
                        .map_or_else(String::new, |reason| format!(" ({reason})")),
                    if run.cancel_requested {
                        " [cancel requested]"
                    } else {
                        ""
                    },
                    run.session_id.as_deref().unwrap_or("<none>")
                )),
            );
            y = y.saturating_add(1);
            if let Some(message) = &run.error_message {
                write_line(
                    frame,
                    area,
                    y,
                    Line::from(format!("    error: {}", compact_message(message))),
                );
                y = y.saturating_add(1);
            }
        }
        y
    }

    fn render_current_loop(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        mut y: u16,
        theme: &RalphSurfaceTheme,
    ) -> u16 {
        write_line(
            frame,
            area,
            y,
            Line::from_spans(vec![Span::styled(
                "Current loop",
                theme.focused.add_modifier(Modifier::BOLD),
            )]),
        );
        y = y.saturating_add(1);
        let Some(summary) = &self.loop_summary else {
            write_line(frame, area, y, Line::from("  <none configured>"));
            return y.saturating_add(1);
        };
        for line in [
            format!("  Name: {}", summary.loop_name),
            format!("  Lifecycle: {}", summary.status),
            format!(
                "  State dir: {}",
                display(&summary.state_dir, &self.repo_path)
            ),
            format!(
                "  Charter: {}",
                display(&summary.charter_doc_path, &self.repo_path)
            ),
            format!(
                "  Progress: {}",
                display(&summary.progress_doc_path, &self.repo_path)
            ),
            format!(
                "  Work area: {}",
                summary.work_area_path.as_ref().map_or_else(
                    || "<not created>".to_owned(),
                    |path| display(path, &self.repo_path).to_string()
                )
            ),
            format!(
                "  Session: {}",
                summary.session_id.as_deref().unwrap_or("<none>")
            ),
            format!(
                "  Limits: max iterations {}, no-progress {}",
                summary.max_iterations, summary.no_progress_limit
            ),
        ] {
            write_line(frame, area, y, Line::from(line));
            y = y.saturating_add(1);
        }
        y
    }

    fn activate_action(&mut self, index: usize) -> PluginTuiAction {
        self.selected_action = index;
        let action = self.action_order()[index].kind;
        if action == RalphActionKind::RebuildLoopContext && self.loop_summary.is_some() {
            self.screen = RalphHomeScreen::RebuildIntro;
            self.status_message =
                Some("Review the rebuild flow, then press Enter to prepare the prompt.".to_owned());
            return PluginTuiAction::Redraw;
        }
        PluginTuiAction::Close {
            outcome: Some(serde_json::json!({
                "ralph_action": action,
            })),
        }
    }

    fn render_actions(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        mut y: u16,
        theme: &RalphSurfaceTheme,
    ) -> u16 {
        write_line(
            frame,
            area,
            y,
            Line::from_spans(vec![Span::styled(
                "Actions",
                theme.focused.add_modifier(Modifier::BOLD),
            )]),
        );
        y = y.saturating_add(1);
        let actions = self.action_order();
        let items = actions
            .iter()
            .map(|action| {
                SelectableListItem::new(
                    action.kind.command_label(),
                    format!(
                        "{:<22} /ralph {:<10} — {}",
                        action.label,
                        action.kind.command_label(),
                        action.kind.description()
                    ),
                )
            })
            .collect::<Vec<_>>();
        let list = SelectableList::new(&items).styles(ralph_list_styles(theme));
        let list_height = u16::try_from(items.len()).unwrap_or(u16::MAX);
        self.action_area = Rect::new(
            area.x,
            y,
            area.width,
            list_height.min(area.bottom().saturating_sub(y).saturating_sub(2)),
        );
        let state = ralph_list_state(self.selected_action);
        list.render_with_fallback_style(self.action_area, &state, frame, theme.canvas);
        self.action_area.bottom().saturating_add(1)
    }
    fn render_rebuild_intro(&self, frame: &mut Frame<'_>, area: Rect, theme: &RalphSurfaceTheme) {
        let mut y = area.y;
        write_line(
            frame,
            area,
            y,
            Line::from_spans(vec![Span::styled(
                "Rebuild Ralph loop context",
                theme.focused.add_modifier(Modifier::BOLD),
            )]),
        );
        y = y.saturating_add(2);
        write_line(
            frame,
            area,
            y,
            Line::from("Create a fresh replacement charter/progress draft for the existing loop."),
        );
        y = y.saturating_add(1);
        write_line(
            frame,
            area,
            y,
            Line::from("Nothing is overwritten until you review, approve, and apply the draft."),
        );
        y = y.saturating_add(2);

        if let Some(summary) = &self.loop_summary {
            for line in [
                format!("Loop: {}", summary.loop_name),
                format!("State: {}", display(&summary.state_dir, &self.repo_path)),
                format!(
                    "Charter: {}",
                    display(&summary.charter_doc_path, &self.repo_path)
                ),
                format!(
                    "Progress: {}",
                    display(&summary.progress_doc_path, &self.repo_path)
                ),
            ] {
                write_line(frame, area, y, Line::from(line));
                y = y.saturating_add(1);
            }
        } else {
            write_line(
                frame,
                area,
                y,
                Line::from("No existing Ralph loop was found."),
            );
            y = y.saturating_add(1);
        }

        y = y.saturating_add(1);
        for line in [
            "Flow:",
            "  1. Bcode will open a focused rebuild prompt in the composer.",
            "  2. Add any extra context/goals/constraints before submitting it.",
            "  3. Ralph will ask clarifying questions or output a replacement draft.",
            "  4. Use Save/View/Revise/Approve setup draft.",
            "  5. Use Apply draft to loop to overwrite files with backups.",
            "",
            "Affected on apply: charter.md, progress.md, validation commands.",
            "Preserved: loop state dir, run history, iteration history.",
            "Backups: <loop-state>/backups/rebuild-<timestamp>/",
        ] {
            write_line(frame, area, y, Line::from(line));
            y = y.saturating_add(1);
        }

        let status_y = area.y.saturating_add(area.height.saturating_sub(2));
        write_line(
            frame,
            area,
            status_y,
            Line::from("Keys: Enter prepare rebuild prompt · Esc back · q close"),
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
    fn render_themed(
        &mut self,
        area: Rect,
        frame: &mut Frame<'_>,
        theme: Option<bcode_plugin_sdk::tui::PluginTuiTheme>,
    ) {
        let theme = RalphSurfaceTheme::resolve(theme);
        frame.fill(area, " ", theme.canvas);
        if self.screen == RalphHomeScreen::RebuildIntro {
            self.render_rebuild_intro(frame, area, &theme);
            return;
        }
        let mut y = area.y;
        write_line(
            frame,
            area,
            y,
            Line::from_spans(vec![Span::styled(
                "Ralph autonomous workflow",
                theme.focused.add_modifier(Modifier::BOLD),
            )]),
        );
        y = y.saturating_add(2);
        write_line(
            frame,
            area,
            y,
            Line::from_spans(vec![Span::styled(
                format!("Repo: {}", display_from_current_dir(&self.repo_path)),
                theme.text,
            )]),
        );
        y = y.saturating_add(1);
        write_line(
            frame,
            area,
            y,
            Line::from_spans(vec![
                Span::styled("Next: ", theme.focused.add_modifier(Modifier::BOLD)),
                Span::styled(self.next_step(), theme.text),
            ]),
        );
        y = y.saturating_add(2);
        y = self.render_current_draft(frame, area, y, &theme);
        if self.setup_draft.is_some() {
            y = y.saturating_add(1);
        }
        y = self.render_current_loop(frame, area, y, &theme);
        y = y.saturating_add(1);
        y = self.render_runs(frame, area, y, &theme);
        y = y.saturating_add(1);
        let _ = self.render_actions(frame, area, y, &theme);

        let status_y = area.y.saturating_add(area.height.saturating_sub(2));
        let hints = [
            KeyHint::new("↑/↓", "select"),
            KeyHint::new("Enter", "run"),
            KeyHint::new("r", "refresh"),
            KeyHint::new("q", "close"),
        ];
        KeyHintBar::new(&hints)
            .styles(ralph_hint_styles(&theme))
            .render(Rect::new(area.x, status_y, area.width, 1), frame);
        if let Some(message) = &self.status_message {
            write_line(
                frame,
                area,
                status_y.saturating_add(1),
                Line::from_spans(vec![Span::styled(message.clone(), theme.focused)]),
            );
        }
    }
}

const fn ralph_list_styles(theme: &RalphSurfaceTheme) -> SelectableListStyles {
    SelectableListStyles {
        normal: theme.text,
        focused: theme.selection,
        selected: theme.selection,
        hovered: theme.focused,
        pressed: theme.selection.add_modifier(Modifier::BOLD),
        disabled: theme.text.add_modifier(Modifier::DIM),
    }
}

const fn ralph_list_state(selected: usize) -> SelectableListState {
    let mut state = SelectableListState::new(None);
    state.set_focused(Some(selected));
    state
}

const fn ralph_hint_styles(theme: &RalphSurfaceTheme) -> KeyHintBarStyles {
    KeyHintBarStyles {
        key: theme.focused,
        label: theme.text,
        separator: theme.text.add_modifier(Modifier::DIM),
        disabled: theme.text.add_modifier(Modifier::DIM),
        background: theme.canvas,
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

    fn handle_event(&mut self, event: &Event, _host: &dyn PluginTuiHost) -> PluginTuiAction {
        if self.screen == RalphHomeScreen::RebuildIntro {
            let Event::Key(key) = event else {
                return PluginTuiAction::None;
            };
            return match key.key {
                KeyCode::Char('q') => PluginTuiAction::Close { outcome: None },
                KeyCode::Escape => {
                    self.screen = RalphHomeScreen::Dashboard;
                    PluginTuiAction::Redraw
                }
                KeyCode::Enter => PluginTuiAction::Close {
                    outcome: Some(serde_json::json!({
                        "ralph_action": RalphActionKind::RebuildLoopContext,
                    })),
                },
                _ => PluginTuiAction::None,
            };
        }
        if let Event::Key(key) = event {
            match key.key {
                KeyCode::Char('q') | KeyCode::Escape => {
                    return PluginTuiAction::Close { outcome: None };
                }
                KeyCode::Char('r') => {
                    self.refresh();
                    return PluginTuiAction::Redraw;
                }
                KeyCode::Char('s') => {
                    return PluginTuiAction::Close {
                        outcome: Some(
                            serde_json::json!({ "ralph_action": RalphActionKind::Status }),
                        ),
                    };
                }
                KeyCode::Char('g') => {
                    return PluginTuiAction::Close {
                        outcome: Some(serde_json::json!({ "ralph_action": RalphActionKind::Goal })),
                    };
                }
                _ => {}
            }
        }
        let actions = self.action_order();
        let items = actions
            .iter()
            .map(|action| SelectableListItem::new(action.kind.command_label(), action.label))
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
        let mut state = ralph_list_state(self.selected_action);
        match list.handle_event(self.action_area, &mut state, &list_event) {
            SelectableListOutcome::Focused(index) => {
                self.selected_action = index;
                PluginTuiAction::Redraw
            }
            SelectableListOutcome::Redraw => {
                self.selected_action = state.focused().unwrap_or(self.selected_action);
                PluginTuiAction::Redraw
            }
            SelectableListOutcome::Selected(index) => self.activate_action(index),
            SelectableListOutcome::Ignored => PluginTuiAction::None,
        }
    }
}

fn compact_message(message: &str) -> String {
    const MAX_LEN: usize = 140;
    let single_line = message.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.len() <= MAX_LEN {
        single_line
    } else {
        format!("{}…", &single_line[..MAX_LEN])
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
    vtable.cli_registration = Some(cli::registration);
    vtable
}

#[cfg(not(feature = "static-bundled"))]
bcode_plugin_sdk::export_plugin!(RalphPlugin, include_str!("../bcode-plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;

    fn test_plugin_theme() -> bcode_plugin_sdk::tui::PluginTuiTheme {
        use bcode_plugin_sdk::tui::{
            PluginTuiDiffTheme, PluginTuiSourceTheme, PluginTuiSyntaxColor, PluginTuiSyntaxTheme,
            PluginTuiTheme,
        };
        let style = Style::new();
        let syntax = PluginTuiSyntaxColor::from_tui(Color::Default);
        PluginTuiTheme {
            component_theme_version: bcode_plugin_sdk::tui::PLUGIN_TUI_COMPONENT_THEME_VERSION,
            canvas: style.bg(Color::Blue),
            text: style.fg(Color::White),
            muted: style.fg(Color::Yellow),
            border: style.fg(Color::Cyan),
            focused: style.fg(Color::Magenta),
            selection: style.fg(Color::Black).bg(Color::Green),
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
            },
        }
    }

    #[test]
    fn ralph_surface_uses_host_theme_at_narrow_sizes() {
        let mut surface = RalphHomeSurface::load(PathBuf::from("/tmp/repo"), None);
        let area = Rect::new(0, 0, 20, 24);
        let mut buffer = bmux_tui::buffer::Buffer::empty(area);
        let theme = test_plugin_theme();
        surface.render_with_theme(area, &mut Frame::new(&mut buffer), Some(theme));

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
        assert!(surface.action_area.width <= area.width);
        assert!(surface.action_area.height <= area.height);
    }

    #[test]
    fn ralph_action_list_supports_keyboard_and_mouse_selection() {
        let items = [
            SelectableListItem::new("plan", "Plan"),
            SelectableListItem::new("run", "Run"),
        ];
        let list = SelectableList::new(&items);
        let area = Rect::new(2, 5, 30, 2);
        let mut state = ralph_list_state(0);

        assert_eq!(
            list.handle_event(
                area,
                &mut state,
                &Event::Key(bmux_keyboard::KeyStroke::simple(KeyCode::Down)),
            ),
            SelectableListOutcome::Focused(1)
        );
        let point = bmux_tui::geometry::Point::new(4, 5);
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
            SelectableListOutcome::Selected(0)
        );
    }
}
