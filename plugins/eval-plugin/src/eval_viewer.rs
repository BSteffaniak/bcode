//! Plugin-owned eval picker and run viewer surfaces.

use crate::eval_data::{
    EvalRunData, EvalRunSummary, best_variant, case_avg_metric, diff_variant_count, discover_runs,
    format_duration_ms, format_number, load_repetition_artifact, sum_variant_metric,
};
use bcode_eval_models::EvalRepetitionResult;
use bcode_plugin_sdk::tui::{PluginTuiAction, PluginTuiHost, PluginTuiSurface};
use bmux_keyboard::KeyCode;
use bmux_tui::event::{Event, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::{Line, Span};
use bmux_tui::style::{Color, Modifier, Style};
use bmux_tui_components::action_row::{ActionButton, ActionRow, ActionRowOutcome, ActionRowState};
use bmux_tui_components::bar_chart::{
    BarChart, BarChartItem, BarChartPolicy, BarChartStyles, BarChartValuePlacement,
};
use bmux_tui_components::button::ButtonStyles;
use bmux_tui_components::sparkline::{Sparkline, SparklinePolicy, SparklineStyles};
use bmux_tui_components::tab_bar::{TabBar, TabBarOutcome, TabBarState, TabBarStyles, TabItem};
use bmux_tui_components::table::{
    Table, TableAlign, TableColumn, TableOutcome, TableRow, TableState, TableStyles,
};
use std::path::PathBuf;

const TITLE_HEIGHT: u16 = 2;
const TAB_HEIGHT: u16 = 1;
const ACTION_HEIGHT: u16 = 1;
const STATUS_HEIGHT: u16 = 1;

const BG: Color = Color::Rgb(8, 13, 20);
const PANEL: Color = Color::Rgb(15, 23, 34);
const PANEL_ALT: Color = Color::Rgb(20, 31, 45);
const BORDER: Color = Color::Rgb(51, 65, 85);
const ACCENT: Color = Color::Rgb(56, 189, 248);
const ACCENT_STRONG: Color = Color::Rgb(14, 165, 233);
const TEXT: Color = Color::Rgb(226, 232, 240);
const MUTED: Color = Color::Rgb(148, 163, 184);
const SUCCESS: Color = Color::Rgb(34, 197, 94);
const DANGER: Color = Color::Rgb(248, 113, 113);
const WARNING: Color = Color::Rgb(251, 191, 36);
const PURPLE: Color = Color::Rgb(167, 139, 250);
const CARD_HEIGHT: u16 = 4;

/// Eval run picker surface.
pub struct EvalRunPickerSurface {
    runs_root: PathBuf,
    runs: Vec<EvalRunSummary>,
    table_state: TableState,
    action_state: ActionRowState,
    embedded_viewer: Option<EvalRunViewerSurface>,
    status: String,
    table_area: Rect,
    action_area: Rect,
}

impl EvalRunPickerSurface {
    /// Load picker from a runs root.
    #[must_use]
    pub fn load(runs_root: PathBuf) -> Self {
        let runs = discover_runs(&runs_root);
        let status = format!("{} runs in {}", runs.len(), runs_root.display());
        Self {
            runs_root,
            runs,
            table_state: TableState::new(Some(0)),
            action_state: ActionRowState::new(),
            embedded_viewer: None,
            status,
            table_area: Rect::new(0, 0, 0, 0),
            action_area: Rect::new(0, 0, 0, 0),
        }
    }

    fn refresh(&mut self) {
        self.runs = discover_runs(&self.runs_root);
        if self.runs.is_empty() {
            self.table_state.set_selected(None);
        } else if self
            .table_state
            .selected()
            .is_none_or(|index| index >= self.runs.len())
        {
            self.table_state.set_selected(Some(0));
        }
        self.status = format!("{} runs in {}", self.runs.len(), self.runs_root.display());
    }

    /// Open the currently selected run, if any.
    pub fn open_selected(&mut self) {
        let Some(index) = self.table_state.selected() else {
            self.status = "no run selected".to_string();
            return;
        };
        let Some(run) = self.runs.get(index) else {
            self.status = "selected run no longer exists".to_string();
            return;
        };
        match EvalRunViewerSurface::load(run.run_dir.clone()) {
            Ok(viewer) => {
                self.embedded_viewer = Some(viewer);
            }
            Err(error) => {
                self.status = format!("failed to open run: {error}");
            }
        }
    }

    fn handle_action(&mut self, action: &str) -> PluginTuiAction {
        match action {
            "open" => {
                self.open_selected();
                PluginTuiAction::Redraw
            }
            "refresh" => {
                self.refresh();
                PluginTuiAction::Redraw
            }
            "close" => PluginTuiAction::Close { outcome: None },
            _ => PluginTuiAction::None,
        }
    }
}

impl PluginTuiSurface for EvalRunPickerSurface {
    fn id(&self) -> &'static str {
        "bcode.eval-run-picker"
    }

    fn title(&self) -> &'static str {
        "Eval Runs"
    }

    fn render(&mut self, area: Rect, frame: &mut Frame<'_>) {
        if let Some(viewer) = self.embedded_viewer.as_mut() {
            viewer.render(area, frame);
            return;
        }
        render_header(area, frame, "Eval Runs", &self.status);
        let body = body_area(area);
        let (table_area, action_area, status_area) = split_body_actions(body);
        self.table_area = inset_top(table_area, 1);
        self.action_area = action_area;
        render_panel_title(table_area, frame, "Recent eval runs");
        let columns = picker_columns();
        let rows = picker_rows(&self.runs);
        render_eval_table(frame, self.table_area, &columns, &rows, &self.table_state);
        let actions = picker_actions();
        themed_action_row(&actions).render_state(action_area, &self.action_state, frame);
        render_status(
            status_area,
            frame,
            "Click a row, then Open. Enter also opens; r refreshes; q closes.",
        );
    }

    fn handle_event(&mut self, event: &Event, host: &dyn PluginTuiHost) -> PluginTuiAction {
        if let Some(viewer) = self.embedded_viewer.as_mut() {
            let action = viewer.handle_event(event, host);
            if matches!(action, PluginTuiAction::Close { .. }) {
                self.embedded_viewer = None;
                return PluginTuiAction::Redraw;
            }
            return action;
        }
        let columns = picker_columns();
        let rows = picker_rows(&self.runs);
        if handle_eval_table_event(
            self.table_area,
            &columns,
            &rows,
            &mut self.table_state,
            event,
        ) {
            return PluginTuiAction::Redraw;
        }
        let actions = picker_actions();
        match themed_action_row(&actions).handle_event(
            self.action_area,
            &mut self.action_state,
            event,
        ) {
            ActionRowOutcome::Activated { id, .. } => return self.handle_action(&id),
            outcome if outcome.needs_redraw() => return PluginTuiAction::Redraw,
            _ => {}
        }
        if let Event::Key(stroke) = event {
            match stroke.key {
                KeyCode::Enter => {
                    self.open_selected();
                    return PluginTuiAction::Redraw;
                }
                KeyCode::Char('r') => {
                    self.refresh();
                    return PluginTuiAction::Redraw;
                }
                KeyCode::Char('q') | KeyCode::Escape => {
                    return PluginTuiAction::Close { outcome: None };
                }
                _ => {}
            }
        }
        PluginTuiAction::None
    }
}

/// Eval run viewer surface.
pub struct EvalRunViewerSurface {
    data: EvalRunData,
    tab_state: TabBarState,
    case_state: TableState,
    tool_state: TableState,
    rep_state: TableState,
    action_state: ActionRowState,
    artifact_scroll: usize,
    artifact: Option<(String, String)>,
    status: String,
    tab_area: Rect,
    content_area: Rect,
    action_area: Rect,
}

impl EvalRunViewerSurface {
    /// Load viewer for a run path.
    ///
    /// # Errors
    ///
    /// Returns an error when the run cannot be loaded.
    pub fn load(path: PathBuf) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let data = EvalRunData::load(path)?;
        let winner =
            best_variant(&data.result).map_or("none", |variant| variant.variant_id.as_str());
        let status = format!(
            "{}  winner={}  {}",
            pass_label(data.result.passed),
            winner,
            data.run_dir.display()
        );
        Ok(Self {
            data,
            tab_state: TabBarState::new(Some(0)),
            case_state: TableState::new(Some(0)),
            tool_state: TableState::new(Some(0)),
            rep_state: TableState::new(Some(0)),
            action_state: ActionRowState::new(),
            artifact_scroll: 0,
            artifact: None,
            status,
            tab_area: Rect::new(0, 0, 0, 0),
            content_area: Rect::new(0, 0, 0, 0),
            action_area: Rect::new(0, 0, 0, 0),
        })
    }

    fn selected_tab(&self) -> ViewerTab {
        ViewerTab::from_index(self.tab_state.selected().unwrap_or(0))
    }

    fn selected_repetition(&self) -> Option<&EvalRepetitionResult> {
        let repetitions = self.data.repetitions();
        repetitions
            .get(self.rep_state.selected().unwrap_or(0))
            .copied()
    }

    fn open_artifact(&mut self, kind: &str) {
        if self.selected_tab() != ViewerTab::Repetitions {
            self.tab_state
                .set_selected(Some(ViewerTab::Repetitions.index()));
            self.status = "select a repetition, then open an artifact".to_string();
            return;
        }
        let Some(repetition) = self.selected_repetition() else {
            self.status = "select a repetition first".to_string();
            return;
        };
        if let Some(artifact) = load_repetition_artifact(&self.data.run_dir, repetition, kind) {
            self.artifact = Some((artifact.title, artifact.text));
            self.tab_state
                .set_selected(Some(ViewerTab::Artifact.index()));
            self.artifact_scroll = 0;
        } else {
            self.status = format!("no {kind} artifact for selected repetition");
        }
    }

    fn handle_action(&mut self, action: &str) -> PluginTuiAction {
        match action {
            "diff" => self.open_artifact("diff"),
            "transcript" => self.open_artifact("transcript"),
            "tools" => self.open_artifact("tool_calls"),
            "refresh" => match EvalRunData::load(&self.data.run_dir) {
                Ok(data) => {
                    self.data = data;
                    self.status = "reloaded run".to_string();
                }
                Err(error) => self.status = format!("reload failed: {error}"),
            },
            "repetitions" => {
                self.tab_state
                    .set_selected(Some(ViewerTab::Repetitions.index()));
            }
            "back" | "close" => return PluginTuiAction::Close { outcome: None },
            _ => {}
        }
        PluginTuiAction::Redraw
    }
}

impl PluginTuiSurface for EvalRunViewerSurface {
    fn id(&self) -> &'static str {
        "bcode.eval-run-viewer"
    }

    fn title(&self) -> &'static str {
        "Eval Run"
    }

    fn render(&mut self, area: Rect, frame: &mut Frame<'_>) {
        render_header(
            area,
            frame,
            &format!("Eval Run: {}", self.data.result.manifest.run_id),
            &self.status,
        );
        let tabs = viewer_tabs();
        let tab_area = Rect::new(
            area.x,
            area.y.saturating_add(TITLE_HEIGHT),
            area.width,
            TAB_HEIGHT,
        );
        self.tab_area = tab_area;
        TabBar::new(&tabs)
            .styles(eval_tab_styles())
            .render(tab_area, &self.tab_state, frame);
        let body = Rect::new(
            area.x,
            area.y.saturating_add(TITLE_HEIGHT + TAB_HEIGHT),
            area.width,
            area.height.saturating_sub(TITLE_HEIGHT + TAB_HEIGHT),
        );
        let (content_area, action_area, status_area) = split_body_actions(body);
        self.content_area = content_area;
        self.action_area = action_area;
        match self.selected_tab() {
            ViewerTab::Overview => self.render_overview(content_area, frame),
            ViewerTab::Cases => self.render_cases(content_area, frame),
            ViewerTab::Tools => self.render_tools(content_area, frame),
            ViewerTab::Repetitions => self.render_repetitions(content_area, frame),
            ViewerTab::Artifact => self.render_artifact(content_area, frame),
            ViewerTab::Derivations => self.render_derivations(content_area, frame),
        }
        let actions = viewer_actions(self.selected_tab());
        themed_action_row(&actions).render_state(action_area, &self.action_state, frame);
        render_status(
            status_area,
            frame,
            "Mouse: click tabs/rows/buttons, wheel scroll artifacts. Keys: Tab, d diff, t transcript, c tools, r refresh, q close.",
        );
    }

    fn handle_event(&mut self, event: &Event, _host: &dyn PluginTuiHost) -> PluginTuiAction {
        let tabs = viewer_tabs();
        match TabBar::new(&tabs).styles(eval_tab_styles()).handle_event(
            self.tab_area,
            &mut self.tab_state,
            event,
        ) {
            TabBarOutcome::Selected(_) | TabBarOutcome::Redraw => return PluginTuiAction::Redraw,
            TabBarOutcome::Ignored => {}
        }
        if self.handle_selected_table_event(event) {
            return PluginTuiAction::Redraw;
        }
        let actions = viewer_actions(self.selected_tab());
        match themed_action_row(&actions).handle_event(
            self.action_area,
            &mut self.action_state,
            event,
        ) {
            ActionRowOutcome::Activated { id, .. } => return self.handle_action(&id),
            outcome if outcome.needs_redraw() => return PluginTuiAction::Redraw,
            _ => {}
        }
        if let Some(action) = self.handle_artifact_mouse_event(event) {
            return action;
        }
        if let Some(action) = self.handle_key_event(event) {
            return action;
        }
        PluginTuiAction::None
    }
}
impl EvalRunViewerSurface {
    fn handle_selected_table_event(&mut self, event: &Event) -> bool {
        match self.selected_tab() {
            ViewerTab::Overview | ViewerTab::Artifact | ViewerTab::Derivations => false,
            ViewerTab::Cases => {
                let (columns, rows) = case_table(&self.data);
                handle_eval_table_event(
                    self.content_area,
                    &columns,
                    &rows,
                    &mut self.case_state,
                    event,
                )
            }
            ViewerTab::Tools => {
                let (columns, rows) = tool_table(&self.data);
                handle_eval_table_event(
                    self.content_area,
                    &columns,
                    &rows,
                    &mut self.tool_state,
                    event,
                )
            }
            ViewerTab::Repetitions => {
                let (columns, rows) = repetition_table(&self.data);
                handle_eval_table_event(
                    self.content_area,
                    &columns,
                    &rows,
                    &mut self.rep_state,
                    event,
                )
            }
        }
    }

    fn handle_artifact_mouse_event(&mut self, event: &Event) -> Option<PluginTuiAction> {
        let Event::Mouse(mouse) = event else {
            return None;
        };
        if self.selected_tab() != ViewerTab::Artifact {
            return None;
        }
        match mouse.kind {
            MouseEventKind::ScrollDown => {
                self.artifact_scroll = self.artifact_scroll.saturating_add(1);
                Some(PluginTuiAction::Redraw)
            }
            MouseEventKind::ScrollUp => {
                self.artifact_scroll = self.artifact_scroll.saturating_sub(1);
                Some(PluginTuiAction::Redraw)
            }
            _ => None,
        }
    }

    fn handle_key_event(&mut self, event: &Event) -> Option<PluginTuiAction> {
        let Event::Key(stroke) = event else {
            return None;
        };
        match stroke.key {
            KeyCode::Tab => {
                let next = (self.tab_state.selected().unwrap_or(0) + 1) % ViewerTab::COUNT;
                self.tab_state.set_selected(Some(next));
                Some(PluginTuiAction::Redraw)
            }
            KeyCode::Char('d') => Some(self.handle_action("diff")),
            KeyCode::Char('t') => Some(self.handle_action("transcript")),
            KeyCode::Char('c') => Some(self.handle_action("tools")),
            KeyCode::Char('r') => Some(self.handle_action("refresh")),
            KeyCode::Char('q') | KeyCode::Escape => Some(PluginTuiAction::Close { outcome: None }),
            KeyCode::Down if self.selected_tab() == ViewerTab::Artifact => {
                self.artifact_scroll = self.artifact_scroll.saturating_add(1);
                Some(PluginTuiAction::Redraw)
            }
            KeyCode::Up if self.selected_tab() == ViewerTab::Artifact => {
                self.artifact_scroll = self.artifact_scroll.saturating_sub(1);
                Some(PluginTuiAction::Redraw)
            }
            _ => None,
        }
    }

    fn render_overview(&self, area: Rect, frame: &mut Frame<'_>) {
        render_panel_title(area, frame, "Run command center");
        let area = inset_top(area, 1);
        let card_area = Rect::new(area.x, area.y, area.width, CARD_HEIGHT);
        self.render_kpi_cards(card_area, frame);

        let charts_y = area.y.saturating_add(CARD_HEIGHT).saturating_add(1);
        let charts_height = area.height.saturating_sub(CARD_HEIGHT).saturating_sub(1);
        let chart_columns =
            split_columns(Rect::new(area.x, charts_y, area.width, charts_height), 2, 2);
        if let Some(left) = chart_columns.first().copied() {
            self.render_variant_charts(left, frame);
        }
        if let Some(right) = chart_columns.get(1).copied() {
            self.render_repetition_trends(right, frame);
        }
    }

    fn render_kpi_cards(&self, area: Rect, frame: &mut Frame<'_>) {
        let metrics = run_dashboard_metrics(&self.data);
        let cards = split_columns(area, 4, 1);
        if let Some(card) = cards.first().copied() {
            render_kpi_card(
                frame,
                card,
                "Pass rate",
                &format!("{:.0}%", metrics.pass_rate * 100.0),
                &format!(
                    "{} / {} repetitions",
                    metrics.passed_repetitions, metrics.total_repetitions
                ),
                if self.data.result.passed {
                    SUCCESS
                } else {
                    DANGER
                },
            );
        }
        if let Some(card) = cards.get(1).copied() {
            render_kpi_card(
                frame,
                card,
                "Winner",
                metrics.winner.as_deref().unwrap_or("none"),
                "highest score / pass rate",
                ACCENT,
            );
        }
        if let Some(card) = cards.get(2).copied() {
            render_kpi_card(
                frame,
                card,
                "Tokens",
                &format_number(metrics.total_tokens),
                &format!("avg {} / repetition", format_number(metrics.avg_tokens)),
                PURPLE,
            );
        }
        if let Some(card) = cards.get(3).copied() {
            render_kpi_card(
                frame,
                card,
                "Risk",
                &metrics.risk_label,
                &format!("{} tool errors", format_number(metrics.tool_errors)),
                metrics.risk_color,
            );
        }
    }

    fn render_variant_charts(&self, area: Rect, frame: &mut Frame<'_>) {
        render_panel_title(area, frame, "Variant comparison");
        let area = inset_top(area, 1);
        let half = area.height / 2;
        let pass_area = Rect::new(area.x, area.y, area.width, half);
        let cost_area = Rect::new(
            area.x,
            area.y.saturating_add(half).saturating_add(1),
            area.width,
            area.height.saturating_sub(half).saturating_sub(1),
        );
        let pass_items = variant_pass_items(&self.data);
        BarChart::new(&pass_items)
            .policy(BarChartPolicy::with_values().value_placement(BarChartValuePlacement::Right))
            .styles(eval_bar_chart_styles())
            .empty("No variants")
            .render(pass_area, frame);
        let token_items = variant_token_items(&self.data);
        BarChart::new(&token_items)
            .policy(BarChartPolicy::with_values().value_placement(BarChartValuePlacement::Right))
            .styles(eval_bar_chart_styles())
            .empty("No token data")
            .render(cost_area, frame);
    }

    fn render_repetition_trends(&self, area: Rect, frame: &mut Frame<'_>) {
        render_panel_title(area, frame, "Repetition telemetry");
        let area = inset_top(area, 1);
        let latency = repetition_samples(&self.data, "wall_time_ms");
        let tokens = repetition_samples(&self.data, "total_tokens");
        let tools = repetition_samples(&self.data, "tool_call_count");
        render_sparkline_block(
            frame,
            Rect::new(area.x, area.y, area.width, 3),
            "Latency",
            &latency,
        );
        render_sparkline_block(
            frame,
            Rect::new(area.x, area.y.saturating_add(4), area.width, 3),
            "Tokens",
            &tokens,
        );
        render_sparkline_block(
            frame,
            Rect::new(area.x, area.y.saturating_add(8), area.width, 3),
            "Tool calls",
            &tools,
        );
    }

    fn render_cases(&self, area: Rect, frame: &mut Frame<'_>) {
        render_panel_title(area, frame, "Case performance");
        let area = inset_top(area, 1);
        let (columns, rows) = case_table(&self.data);
        render_eval_table(frame, area, &columns, &rows, &self.case_state);
    }

    fn render_tools(&self, area: Rect, frame: &mut Frame<'_>) {
        render_panel_title(area, frame, "Tool usage");
        let area = inset_top(area, 1);
        let (columns, rows) = tool_table(&self.data);
        render_eval_table(frame, area, &columns, &rows, &self.tool_state);
    }

    fn render_repetitions(&self, area: Rect, frame: &mut Frame<'_>) {
        render_panel_title(
            area,
            frame,
            "Repetitions — select a row, then open artifacts",
        );
        let area = inset_top(area, 1);
        let (columns, rows) = repetition_table(&self.data);
        render_eval_table(frame, area, &columns, &rows, &self.rep_state);
    }

    fn render_artifact(&self, area: Rect, frame: &mut Frame<'_>) {
        let Some((title, text)) = &self.artifact else {
            render_panel_title(area, frame, "Artifact viewer");
            render_status(
                inset_top(area, 1),
                frame,
                "Select a repetition, then use Diff, Transcript, or Tool Calls.",
            );
            return;
        };
        render_panel_title(area, frame, title);
        for (row, line) in text
            .lines()
            .skip(self.artifact_scroll)
            .take(usize::from(area.height.saturating_sub(1)))
            .enumerate()
        {
            let y = area.y.saturating_add(1).saturating_add(usize_to_u16(row));
            frame.write_line_with_fallback_style(
                Rect::new(area.x, y, area.width, 1),
                &artifact_line(line),
                Style::new().bg(PANEL),
            );
        }
    }

    fn render_derivations(&self, area: Rect, frame: &mut Frame<'_>) {
        render_panel_title(area, frame, "Metric derivations and scoring model");
        let metrics = run_dashboard_metrics(&self.data);
        let lines = derivation_lines(&metrics);
        for (row, line) in lines
            .iter()
            .take(usize::from(area.height.saturating_sub(1)))
            .enumerate()
        {
            frame.write_line_with_fallback_style(
                Rect::new(
                    area.x,
                    area.y.saturating_add(1).saturating_add(usize_to_u16(row)),
                    area.width,
                    1,
                ),
                line,
                Style::new().bg(PANEL),
            );
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewerTab {
    Overview,
    Cases,
    Tools,
    Repetitions,
    Artifact,
    Derivations,
}

impl ViewerTab {
    const COUNT: usize = 6;

    const fn index(self) -> usize {
        match self {
            Self::Overview => 0,
            Self::Cases => 1,
            Self::Tools => 2,
            Self::Repetitions => 3,
            Self::Artifact => 4,
            Self::Derivations => 5,
        }
    }

    const fn from_index(index: usize) -> Self {
        match index {
            1 => Self::Cases,
            2 => Self::Tools,
            3 => Self::Repetitions,
            4 => Self::Artifact,
            5 => Self::Derivations,
            _ => Self::Overview,
        }
    }
}

fn render_header(area: Rect, frame: &mut Frame<'_>, title: &str, status: &str) {
    if area.height == 0 {
        return;
    }
    let title_line = Line::from_spans(vec![
        Span::styled(
            " ◆ ",
            Style::new().fg(ACCENT).bg(BG).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            title,
            Style::new().fg(TEXT).bg(BG).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ", Style::new().bg(BG)),
        Span::styled(status, Style::new().fg(MUTED).bg(BG)),
    ]);
    frame.write_line_with_fallback_style(
        Rect::new(area.x, area.y, area.width, 1),
        &title_line,
        Style::new().bg(BG),
    );
    if area.height > 1 {
        frame.write_line_with_fallback_style(
            Rect::new(area.x, area.y.saturating_add(1), area.width, 1),
            &Line::from_spans(vec![Span::styled(
                "─".repeat(usize::from(area.width)),
                Style::new().fg(BORDER).bg(BG),
            )]),
            Style::new().bg(BG),
        );
    }
}

fn render_status(area: Rect, frame: &mut Frame<'_>, text: &str) {
    frame.write_line_with_fallback_style(
        area,
        &Line::from_spans(vec![
            Span::styled("  ", Style::new().bg(BG)),
            Span::styled(text, Style::new().fg(MUTED).bg(BG)),
        ]),
        Style::new().bg(BG),
    );
}

fn render_panel_title(area: Rect, frame: &mut Frame<'_>, title: &str) {
    if area.height == 0 {
        return;
    }
    frame.write_line_with_fallback_style(
        Rect::new(area.x, area.y, area.width, 1),
        &Line::from_spans(vec![
            Span::styled(
                " ▸ ",
                Style::new()
                    .fg(ACCENT)
                    .bg(PANEL)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                title,
                Style::new().fg(TEXT).bg(PANEL).add_modifier(Modifier::BOLD),
            ),
        ]),
        Style::new().bg(PANEL),
    );
}

const fn eval_table_styles() -> TableStyles {
    TableStyles {
        header: Style::new()
            .fg(ACCENT)
            .bg(PANEL)
            .add_modifier(Modifier::BOLD),
        row: Style::new().fg(TEXT).bg(PANEL),
        selected: Style::new()
            .fg(Color::Black)
            .bg(ACCENT)
            .add_modifier(Modifier::BOLD),
        selected_column: Style::new().fg(Color::Black).bg(ACCENT_STRONG),
        selected_cell: Style::new()
            .fg(Color::Black)
            .bg(WARNING)
            .add_modifier(Modifier::BOLD),
        hovered: Style::new().fg(Color::White).bg(PANEL_ALT),
        disabled: Style::new().fg(MUTED).bg(PANEL),
        separator: Style::new().fg(BORDER).bg(PANEL),
        empty: Style::new().fg(MUTED).bg(PANEL),
    }
}

const fn eval_tab_styles() -> TabBarStyles {
    TabBarStyles {
        normal: Style::new().fg(MUTED).bg(BG),
        selected: Style::new()
            .fg(Color::Black)
            .bg(ACCENT)
            .add_modifier(Modifier::BOLD),
        focused: Style::new()
            .fg(TEXT)
            .bg(PANEL_ALT)
            .add_modifier(Modifier::UNDERLINE),
        hovered: Style::new().fg(TEXT).bg(PANEL_ALT),
        pressed: Style::new()
            .fg(Color::Black)
            .bg(ACCENT_STRONG)
            .add_modifier(Modifier::BOLD),
        disabled: Style::new().fg(BORDER).bg(BG),
        separator: Style::new().fg(BORDER).bg(BG),
    }
}

const fn eval_button_styles() -> ButtonStyles {
    ButtonStyles {
        normal: Style::new().fg(TEXT).bg(PANEL_ALT),
        focused: Style::new()
            .fg(Color::Black)
            .bg(ACCENT)
            .add_modifier(Modifier::BOLD),
        hovered: Style::new().fg(Color::Black).bg(ACCENT),
        pressed: Style::new()
            .fg(Color::Black)
            .bg(ACCENT_STRONG)
            .add_modifier(Modifier::BOLD),
        disabled: Style::new().fg(MUTED).bg(PANEL),
    }
}

fn themed_action_row(actions: &[ActionButton]) -> ActionRow<'_> {
    ActionRow::new(actions)
        .styles(eval_button_styles())
        .spacing(2)
}

const fn eval_table<'a>(columns: &'a [TableColumn<'a>], rows: &'a [TableRow]) -> Table<'a> {
    Table::new(columns, rows).styles(eval_table_styles())
}

fn render_eval_table(
    frame: &mut Frame<'_>,
    area: Rect,
    columns: &[TableColumn<'_>],
    rows: &[TableRow],
    state: &TableState,
) {
    eval_table(columns, rows).render(area, state, frame);
}

fn handle_eval_table_event(
    area: Rect,
    columns: &[TableColumn<'_>],
    rows: &[TableRow],
    state: &mut TableState,
    event: &Event,
) -> bool {
    table_action(eval_table(columns, rows).handle_event(area, state, event))
}

const fn eval_bar_chart_styles() -> BarChartStyles {
    BarChartStyles {
        label: Style::new().fg(TEXT).bg(PANEL),
        bar: Style::new()
            .fg(ACCENT)
            .bg(PANEL)
            .add_modifier(Modifier::BOLD),
        empty: Style::new().fg(BORDER).bg(PANEL),
        value: Style::new().fg(MUTED).bg(PANEL),
        empty_message: Style::new().fg(MUTED).bg(PANEL),
    }
}

const fn eval_sparkline_styles() -> SparklineStyles {
    SparklineStyles {
        normal: Style::new().fg(ACCENT).bg(PANEL_ALT),
        latest: Style::new()
            .fg(WARNING)
            .bg(PANEL_ALT)
            .add_modifier(Modifier::BOLD),
        first: Style::new().fg(PURPLE).bg(PANEL_ALT),
        high: Style::new()
            .fg(SUCCESS)
            .bg(PANEL_ALT)
            .add_modifier(Modifier::BOLD),
        low: Style::new().fg(DANGER).bg(PANEL_ALT),
        empty: Style::new().fg(MUTED).bg(PANEL_ALT),
        background: Style::new().bg(PANEL_ALT),
    }
}

fn render_kpi_card(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    value: &str,
    detail: &str,
    accent: Color,
) {
    frame.fill(area, " ", Style::new().bg(PANEL_ALT));
    frame.write_line_with_fallback_style(
        Rect::new(area.x, area.y, area.width, 1),
        &Line::from_spans(vec![
            Span::styled("  ", Style::new().bg(PANEL_ALT)),
            Span::styled(
                label,
                Style::new()
                    .fg(MUTED)
                    .bg(PANEL_ALT)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Style::new().bg(PANEL_ALT),
    );
    frame.write_line_with_fallback_style(
        Rect::new(area.x, area.y.saturating_add(1), area.width, 1),
        &Line::from_spans(vec![
            Span::styled("  ", Style::new().bg(PANEL_ALT)),
            Span::styled(
                value,
                Style::new()
                    .fg(accent)
                    .bg(PANEL_ALT)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Style::new().bg(PANEL_ALT),
    );
    frame.write_line_with_fallback_style(
        Rect::new(area.x, area.y.saturating_add(2), area.width, 1),
        &Line::from_spans(vec![
            Span::styled("  ", Style::new().bg(PANEL_ALT)),
            Span::styled(detail, Style::new().fg(MUTED).bg(PANEL_ALT)),
        ]),
        Style::new().bg(PANEL_ALT),
    );
}

fn split_columns(area: Rect, columns: u16, gap: u16) -> Vec<Rect> {
    if columns == 0 {
        return Vec::new();
    }
    let total_gap = gap.saturating_mul(columns.saturating_sub(1));
    let width = area.width.saturating_sub(total_gap) / columns;
    (0..columns)
        .map(|index| {
            Rect::new(
                area.x
                    .saturating_add(index.saturating_mul(width.saturating_add(gap))),
                area.y,
                if index + 1 == columns {
                    area.right().saturating_sub(
                        area.x
                            .saturating_add(index.saturating_mul(width.saturating_add(gap))),
                    )
                } else {
                    width
                },
                area.height,
            )
        })
        .collect()
}

#[derive(Debug, Clone)]
struct DashboardMetrics {
    pass_rate: f64,
    passed_repetitions: usize,
    total_repetitions: usize,
    winner: Option<String>,
    total_tokens: f64,
    avg_tokens: f64,
    tool_errors: f64,
    avg_wall_time_ms: f64,
    risk_label: String,
    risk_color: Color,
}

fn run_dashboard_metrics(data: &EvalRunData) -> DashboardMetrics {
    let repetitions = data.repetitions();
    let total_repetitions = repetitions.len();
    let passed_repetitions = repetitions
        .iter()
        .filter(|repetition| repetition.passed)
        .count();
    let pass_rate = if total_repetitions == 0 {
        0.0
    } else {
        usize_as_f64(passed_repetitions) / usize_as_f64(total_repetitions)
    };
    let total_tokens = repetitions
        .iter()
        .map(|repetition| metric(repetition, "total_tokens"))
        .sum::<f64>();
    let tool_errors = repetitions
        .iter()
        .map(|repetition| metric(repetition, "tool_error_count"))
        .sum::<f64>();
    let wall_time = repetitions
        .iter()
        .map(|repetition| metric(repetition, "wall_time_ms"))
        .sum::<f64>();
    let avg_tokens = average(total_tokens, total_repetitions);
    let avg_wall_time_ms = average(wall_time, total_repetitions);
    let (risk_label, risk_color) = risk_badge(pass_rate, tool_errors, avg_wall_time_ms);
    DashboardMetrics {
        pass_rate,
        passed_repetitions,
        total_repetitions,
        winner: best_variant(&data.result).map(|variant| variant.variant_id.clone()),
        total_tokens,
        avg_tokens,
        tool_errors,
        avg_wall_time_ms,
        risk_label,
        risk_color,
    }
}

fn metric(repetition: &EvalRepetitionResult, key: &str) -> f64 {
    repetition
        .measurements
        .get(key)
        .copied()
        .unwrap_or_else(|| {
            if key == "wall_time_ms" {
                repetition
                    .wall_time_ms
                    .to_string()
                    .parse::<f64>()
                    .unwrap_or(0.0)
            } else {
                0.0
            }
        })
}

fn average(total: f64, count: usize) -> f64 {
    if count == 0 {
        0.0
    } else {
        total / usize_as_f64(count)
    }
}

fn usize_as_f64(value: usize) -> f64 {
    f64::from(u32::try_from(value).unwrap_or(u32::MAX))
}

fn metric_to_u64(value: f64) -> u64 {
    if !value.is_finite() || value <= 0.0 {
        0
    } else {
        value.round().to_string().parse().unwrap_or(u64::MAX)
    }
}

fn risk_badge(pass_rate: f64, tool_errors: f64, avg_wall_time_ms: f64) -> (String, Color) {
    if pass_rate < 1.0 {
        ("FAILING".to_string(), DANGER)
    } else if tool_errors > 0.0 {
        ("TOOL-RISK".to_string(), WARNING)
    } else if avg_wall_time_ms > 30_000.0 {
        ("SLOW".to_string(), WARNING)
    } else {
        ("HEALTHY".to_string(), SUCCESS)
    }
}

fn variant_pass_items(data: &EvalRunData) -> Vec<BarChartItem<'_>> {
    data.result
        .variants
        .iter()
        .map(|variant| {
            BarChartItem::new(
                variant.variant_id.as_str(),
                metric_to_u64(variant.pass_rate * 100.0),
            )
        })
        .collect()
}

fn variant_token_items(data: &EvalRunData) -> Vec<BarChartItem<'_>> {
    data.result
        .variants
        .iter()
        .map(|variant| {
            BarChartItem::new(
                variant.variant_id.as_str(),
                metric_to_u64(sum_variant_metric(variant, "total_tokens")),
            )
        })
        .collect()
}

fn repetition_samples(data: &EvalRunData, metric_name: &str) -> Vec<u64> {
    data.repetitions()
        .iter()
        .map(|repetition| metric_to_u64(metric(repetition, metric_name)))
        .collect()
}

fn render_sparkline_block(frame: &mut Frame<'_>, area: Rect, title: &str, samples: &[u64]) {
    if area.height == 0 {
        return;
    }
    frame.write_line_with_fallback_style(
        Rect::new(area.x, area.y, area.width, 1),
        &Line::from_spans(vec![
            Span::styled("  ", Style::new().bg(PANEL_ALT)),
            Span::styled(
                title,
                Style::new()
                    .fg(MUTED)
                    .bg(PANEL_ALT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "  latest={} max={}",
                    samples.last().copied().unwrap_or(0),
                    samples.iter().copied().max().unwrap_or(0)
                ),
                Style::new().fg(MUTED).bg(PANEL_ALT),
            ),
        ]),
        Style::new().bg(PANEL_ALT),
    );
    let mut policy = SparklinePolicy::compact();
    policy.background = true;
    policy.highlight_high = true;
    policy.highlight_low = true;
    Sparkline::new(samples)
        .policy(policy)
        .styles(eval_sparkline_styles())
        .empty("No telemetry")
        .render(
            Rect::new(area.x, area.y.saturating_add(1), area.width, 1),
            frame,
        );
}

fn derivation_lines(metrics: &DashboardMetrics) -> Vec<Line> {
    vec![
        derivation_heading("Run health"),
        derivation_line(
            "Pass rate",
            "passed repetitions / total repetitions",
            &format!(
                "{} / {} = {:.1}%",
                metrics.passed_repetitions,
                metrics.total_repetitions,
                metrics.pass_rate * 100.0
            ),
        ),
        derivation_line(
            "Winner",
            "best variant by aggregate score and pass quality",
            metrics.winner.as_deref().unwrap_or("none"),
        ),
        derivation_line(
            "Risk",
            "failing runs, tool errors, or slow latency raise risk",
            &metrics.risk_label,
        ),
        derivation_heading("Cost and latency"),
        derivation_line(
            "Total tokens",
            "sum(total_tokens) across all repetitions",
            &format_number(metrics.total_tokens),
        ),
        derivation_line(
            "Average tokens",
            "total_tokens / total repetitions",
            &format_number(metrics.avg_tokens),
        ),
        derivation_line(
            "Average wall time",
            "sum(wall_time_ms) / total repetitions",
            &format_duration_ms(metrics.avg_wall_time_ms),
        ),
        derivation_heading("Charts"),
        derivation_line(
            "Variant pass chart",
            "variant.pass_rate * 100",
            "higher is better",
        ),
        derivation_line(
            "Variant token chart",
            "sum(total_tokens) per variant",
            "lower is cheaper",
        ),
        derivation_line(
            "Telemetry sparklines",
            "repetition metrics in execution order",
            "shape shows variance and outliers",
        ),
    ]
}

fn derivation_heading(text: &str) -> Line {
    Line::from_spans(vec![
        Span::styled(
            "  ▸ ",
            Style::new()
                .fg(ACCENT)
                .bg(PANEL)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            text.to_string(),
            Style::new().fg(TEXT).bg(PANEL).add_modifier(Modifier::BOLD),
        ),
    ])
}

fn derivation_line(label: &str, formula: &str, value: &str) -> Line {
    Line::from_spans(vec![
        Span::styled("    ", Style::new().bg(PANEL)),
        Span::styled(
            format!("{label:<18}"),
            Style::new()
                .fg(ACCENT)
                .bg(PANEL)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("{formula:<58}"), Style::new().fg(MUTED).bg(PANEL)),
        Span::styled(value.to_string(), Style::new().fg(TEXT).bg(PANEL)),
    ])
}

fn pass_label(passed: bool) -> String {
    if passed { "PASS" } else { "FAIL" }.to_string()
}

fn artifact_line(text: &str) -> Line {
    let style = if text.starts_with('+') && !text.starts_with("+++") {
        Style::new().fg(SUCCESS).bg(PANEL)
    } else if text.starts_with('-') && !text.starts_with("---") {
        Style::new().fg(DANGER).bg(PANEL)
    } else if text.starts_with("@@") {
        Style::new()
            .fg(WARNING)
            .bg(PANEL)
            .add_modifier(Modifier::BOLD)
    } else if text.starts_with("diff ") || text.starts_with("+++") || text.starts_with("---") {
        Style::new()
            .fg(ACCENT)
            .bg(PANEL)
            .add_modifier(Modifier::BOLD)
    } else if text.trim().is_empty() {
        Style::new().bg(PANEL)
    } else {
        Style::new().fg(TEXT).bg(PANEL)
    };
    Line::from_spans(vec![
        Span::styled("  ", Style::new().bg(PANEL)),
        Span::styled(text.to_string(), style),
    ])
}

const fn inset_top(area: Rect, rows: u16) -> Rect {
    Rect::new(
        area.x,
        area.y.saturating_add(rows),
        area.width,
        area.height.saturating_sub(rows),
    )
}

const fn body_area(area: Rect) -> Rect {
    Rect::new(
        area.x,
        area.y.saturating_add(TITLE_HEIGHT),
        area.width,
        area.height.saturating_sub(TITLE_HEIGHT),
    )
}

const fn split_body_actions(area: Rect) -> (Rect, Rect, Rect) {
    let reserved = ACTION_HEIGHT + STATUS_HEIGHT;
    let content_height = area.height.saturating_sub(reserved);
    let content = Rect::new(area.x, area.y, area.width, content_height);
    let action = Rect::new(
        area.x,
        area.y.saturating_add(content_height),
        area.width,
        ACTION_HEIGHT,
    );
    let status = Rect::new(
        area.x,
        area.y.saturating_add(content_height + ACTION_HEIGHT),
        area.width,
        STATUS_HEIGHT,
    );
    (content, action, status)
}

fn picker_columns<'a>() -> Vec<TableColumn<'a>> {
    vec![
        TableColumn::new("Run").flex(3),
        TableColumn::new("Suite").flex(2),
        TableColumn::new("Passed").fixed(8).align(TableAlign::Right),
        TableColumn::new("Variants")
            .fixed(9)
            .align(TableAlign::Right),
        TableColumn::new("Winner").flex(2),
    ]
}

fn picker_rows(runs: &[EvalRunSummary]) -> Vec<TableRow> {
    runs.iter()
        .map(|run| {
            string_row(vec![
                run.run_id.clone(),
                run.suite_id.clone(),
                pass_label(run.passed),
                run.variants.to_string(),
                run.winner.clone().unwrap_or_else(|| "n/a".to_string()),
            ])
        })
        .collect()
}

fn picker_actions() -> Vec<ActionButton> {
    vec![
        ActionButton::new("open", "Enter Open"),
        ActionButton::new("refresh", "R Refresh"),
        ActionButton::new("close", "Esc Close"),
    ]
}

fn viewer_tabs() -> Vec<TabItem<'static>> {
    vec![
        TabItem::new("overview", "Overview"),
        TabItem::new("cases", "Cases"),
        TabItem::new("tools", "Tools"),
        TabItem::new("repetitions", "Repetitions"),
        TabItem::new("artifact", "Artifact"),
        TabItem::new("derivations", "Derivations"),
    ]
}

fn viewer_actions(tab: ViewerTab) -> Vec<ActionButton> {
    match tab {
        ViewerTab::Repetitions => vec![
            ActionButton::new("diff", "D Diff"),
            ActionButton::new("transcript", "T Transcript"),
            ActionButton::new("tools", "C Tool Calls"),
            ActionButton::new("refresh", "R Refresh"),
            ActionButton::new("close", "Esc Close"),
        ],
        ViewerTab::Artifact => vec![
            ActionButton::new("repetitions", "Back to Repetitions"),
            ActionButton::new("refresh", "R Refresh"),
            ActionButton::new("close", "Esc Close"),
        ],
        ViewerTab::Overview | ViewerTab::Cases | ViewerTab::Tools | ViewerTab::Derivations => vec![
            ActionButton::new("repetitions", "Open Repetitions"),
            ActionButton::new("refresh", "R Refresh"),
            ActionButton::new("close", "Esc Close"),
        ],
    }
}

fn case_table(data: &EvalRunData) -> (Vec<TableColumn<'static>>, Vec<TableRow>) {
    let columns = vec![
        TableColumn::new("Case").flex(2),
        TableColumn::new("Variant").flex(2),
        TableColumn::new("Pass").fixed(8).align(TableAlign::Right),
        TableColumn::new("Reps").fixed(6).align(TableAlign::Right),
        TableColumn::new("Avg Wall")
            .fixed(10)
            .align(TableAlign::Right),
        TableColumn::new("Avg Tokens")
            .fixed(11)
            .align(TableAlign::Right),
        TableColumn::new("Diffs").fixed(7).align(TableAlign::Right),
    ];
    let mut rows = Vec::new();
    for variant in &data.result.variants {
        for case in &variant.cases {
            rows.push(string_row(vec![
                case.case_id.clone(),
                variant.variant_id.clone(),
                format!("{:.1}%", case.pass_rate * 100.0),
                case.repetitions.len().to_string(),
                format_duration_ms(case_avg_metric(&case.repetitions, "wall_time_ms")),
                format_number(case_avg_metric(&case.repetitions, "total_tokens")),
                diff_variant_count(&data.run_dir, &case.repetitions).to_string(),
            ]));
        }
    }
    (columns, rows)
}

fn tool_table(data: &EvalRunData) -> (Vec<TableColumn<'static>>, Vec<TableRow>) {
    let tool_metrics = data.tool_metric_names();
    let mut columns = vec![
        TableColumn::new("Variant").flex(2),
        TableColumn::new("Total").fixed(8).align(TableAlign::Right),
        TableColumn::new("Errors").fixed(8).align(TableAlign::Right),
    ];
    for tool in &tool_metrics {
        columns.push(
            TableColumn::new(Box::leak(
                tool.trim_start_matches("tool_call_count.")
                    .to_string()
                    .into_boxed_str(),
            ))
            .fixed(14)
            .align(TableAlign::Right),
        );
    }
    let rows = data
        .result
        .variants
        .iter()
        .map(|variant| {
            let mut cells = vec![
                variant.variant_id.clone(),
                format_number(sum_variant_metric(variant, "tool_call_count")),
                format_number(sum_variant_metric(variant, "tool_error_count")),
            ];
            for tool in &tool_metrics {
                cells.push(format_number(sum_variant_metric(variant, tool)));
            }
            string_row(cells)
        })
        .collect();
    (columns, rows)
}

fn repetition_table(data: &EvalRunData) -> (Vec<TableColumn<'static>>, Vec<TableRow>) {
    let columns = vec![
        TableColumn::new("Variant").flex(2),
        TableColumn::new("Case").flex(2),
        TableColumn::new("Rep").fixed(5).align(TableAlign::Right),
        TableColumn::new("Passed").fixed(8).align(TableAlign::Right),
        TableColumn::new("Wall").fixed(10).align(TableAlign::Right),
        TableColumn::new("Tokens")
            .fixed(10)
            .align(TableAlign::Right),
        TableColumn::new("Tools").fixed(8).align(TableAlign::Right),
        TableColumn::new("Errors").fixed(8).align(TableAlign::Right),
    ];
    let rows = data
        .repetitions()
        .iter()
        .map(|repetition| {
            string_row(vec![
                repetition.variant_id.clone(),
                repetition.case_id.clone(),
                repetition.repetition.to_string(),
                pass_label(repetition.passed),
                format_duration_ms(
                    repetition
                        .measurements
                        .get("wall_time_ms")
                        .copied()
                        .unwrap_or_default(),
                ),
                format_number(
                    repetition
                        .measurements
                        .get("total_tokens")
                        .copied()
                        .unwrap_or_default(),
                ),
                format_number(
                    repetition
                        .measurements
                        .get("tool_call_count")
                        .copied()
                        .unwrap_or_default(),
                ),
                format_number(
                    repetition
                        .measurements
                        .get("tool_error_count")
                        .copied()
                        .unwrap_or_default(),
                ),
            ])
        })
        .collect();
    (columns, rows)
}

fn usize_to_u16(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

fn string_row(cells: Vec<String>) -> TableRow {
    TableRow::rich(cells.into_iter().map(Line::from).collect::<Vec<_>>())
}

const fn table_action(outcome: TableOutcome) -> bool {
    matches!(
        outcome,
        TableOutcome::Selected(_) | TableOutcome::Focused(_) | TableOutcome::Redraw
    )
}
