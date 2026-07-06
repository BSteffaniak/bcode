//! Plugin-owned eval picker and run viewer surfaces.

use crate::eval_data::{
    EvalRunData, EvalRunSummary, best_variant, case_avg_metric, diff_variant_count, discover_runs,
    format_duration_ms, format_number, load_repetition_artifact, sum_variant_metric,
    variant_metrics,
};
use bcode_eval_models::EvalRepetitionResult;
use bcode_plugin_sdk::tui::{PluginTuiAction, PluginTuiHost, PluginTuiSurface};
use bmux_keyboard::KeyCode;
use bmux_tui::event::{Event, MouseEventKind};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::Line;
use bmux_tui_components::action_row::{ActionButton, ActionRow, ActionRowOutcome, ActionRowState};
use bmux_tui_components::tab_bar::{TabBar, TabBarOutcome, TabBarState, TabItem};
use bmux_tui_components::table::{
    Table, TableAlign, TableColumn, TableOutcome, TableRow, TableState,
};
use std::path::PathBuf;

const TITLE_HEIGHT: u16 = 1;
const TAB_HEIGHT: u16 = 1;
const ACTION_HEIGHT: u16 = 1;
const STATUS_HEIGHT: u16 = 1;

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

    fn open_selected(&mut self) {
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
        self.table_area = table_area;
        self.action_area = action_area;
        let columns = picker_columns();
        let rows = picker_rows(&self.runs);
        Table::new(&columns, &rows).render(table_area, &self.table_state, frame);
        let actions = picker_actions();
        ActionRow::new(&actions).render_state(action_area, &self.action_state, frame);
        frame.write_line(
            status_area,
            &Line::from("Click a row, then Open. Enter also opens; r refreshes; q closes."),
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
        let table = Table::new(&columns, &rows);
        match table.handle_event(self.table_area, &mut self.table_state, event) {
            TableOutcome::Selected(_) | TableOutcome::Focused(_) | TableOutcome::Redraw => {
                return PluginTuiAction::Redraw;
            }
            TableOutcome::Ignored => {}
        }
        let actions = picker_actions();
        match ActionRow::new(&actions).handle_event(self.action_area, &mut self.action_state, event)
        {
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
    overview_state: TableState,
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
            "passed={} winner={} run_dir={}",
            data.result.passed,
            winner,
            data.run_dir.display()
        );
        Ok(Self {
            data,
            tab_state: TabBarState::new(Some(0)),
            overview_state: TableState::new(Some(0)),
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
        TabBar::new(&tabs).render(tab_area, &self.tab_state, frame);
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
        }
        let actions = viewer_actions();
        ActionRow::new(&actions).render_state(action_area, &self.action_state, frame);
        frame.write_line(
            status_area,
            &Line::from("Mouse: click tabs/rows/buttons, wheel scroll. Keys: Tab, d, t, c, r, q."),
        );
    }

    fn handle_event(&mut self, event: &Event, _host: &dyn PluginTuiHost) -> PluginTuiAction {
        let tabs = viewer_tabs();
        match TabBar::new(&tabs).handle_event(self.tab_area, &mut self.tab_state, event) {
            TabBarOutcome::Selected(_) | TabBarOutcome::Redraw => return PluginTuiAction::Redraw,
            TabBarOutcome::Ignored => {}
        }
        match self.selected_tab() {
            ViewerTab::Overview => {
                let (columns, rows) = overview_table(&self.data);
                if table_action(Table::new(&columns, &rows).handle_event(
                    self.content_area,
                    &mut self.overview_state,
                    event,
                )) {
                    return PluginTuiAction::Redraw;
                }
            }
            ViewerTab::Cases => {
                let (columns, rows) = case_table(&self.data);
                if table_action(Table::new(&columns, &rows).handle_event(
                    self.content_area,
                    &mut self.case_state,
                    event,
                )) {
                    return PluginTuiAction::Redraw;
                }
            }
            ViewerTab::Tools => {
                let (columns, rows) = tool_table(&self.data);
                if table_action(Table::new(&columns, &rows).handle_event(
                    self.content_area,
                    &mut self.tool_state,
                    event,
                )) {
                    return PluginTuiAction::Redraw;
                }
            }
            ViewerTab::Repetitions => {
                let (columns, rows) = repetition_table(&self.data);
                if table_action(Table::new(&columns, &rows).handle_event(
                    self.content_area,
                    &mut self.rep_state,
                    event,
                )) {
                    return PluginTuiAction::Redraw;
                }
            }
            ViewerTab::Artifact => {}
        }
        let actions = viewer_actions();
        match ActionRow::new(&actions).handle_event(self.action_area, &mut self.action_state, event)
        {
            ActionRowOutcome::Activated { id, .. } => return self.handle_action(&id),
            outcome if outcome.needs_redraw() => return PluginTuiAction::Redraw,
            _ => {}
        }
        if let Event::Mouse(mouse) = event
            && self.selected_tab() == ViewerTab::Artifact
        {
            match mouse.kind {
                MouseEventKind::ScrollDown => {
                    self.artifact_scroll = self.artifact_scroll.saturating_add(1);
                    return PluginTuiAction::Redraw;
                }
                MouseEventKind::ScrollUp => {
                    self.artifact_scroll = self.artifact_scroll.saturating_sub(1);
                    return PluginTuiAction::Redraw;
                }
                _ => {}
            }
        }
        if let Event::Key(stroke) = event {
            match stroke.key {
                KeyCode::Tab => {
                    let next = (self.tab_state.selected().unwrap_or(0) + 1) % ViewerTab::COUNT;
                    self.tab_state.set_selected(Some(next));
                    return PluginTuiAction::Redraw;
                }
                KeyCode::Char('d') => return self.handle_action("diff"),
                KeyCode::Char('t') => return self.handle_action("transcript"),
                KeyCode::Char('c') => return self.handle_action("tools"),
                KeyCode::Char('r') => return self.handle_action("refresh"),
                KeyCode::Char('q') | KeyCode::Escape => {
                    return PluginTuiAction::Close { outcome: None };
                }
                KeyCode::Down if self.selected_tab() == ViewerTab::Artifact => {
                    self.artifact_scroll = self.artifact_scroll.saturating_add(1);
                    return PluginTuiAction::Redraw;
                }
                KeyCode::Up if self.selected_tab() == ViewerTab::Artifact => {
                    self.artifact_scroll = self.artifact_scroll.saturating_sub(1);
                    return PluginTuiAction::Redraw;
                }
                _ => {}
            }
        }
        PluginTuiAction::None
    }
}

impl EvalRunViewerSurface {
    fn render_overview(&self, area: Rect, frame: &mut Frame<'_>) {
        let (columns, rows) = overview_table(&self.data);
        Table::new(&columns, &rows).render(area, &self.overview_state, frame);
    }

    fn render_cases(&self, area: Rect, frame: &mut Frame<'_>) {
        let (columns, rows) = case_table(&self.data);
        Table::new(&columns, &rows).render(area, &self.case_state, frame);
    }

    fn render_tools(&self, area: Rect, frame: &mut Frame<'_>) {
        let (columns, rows) = tool_table(&self.data);
        Table::new(&columns, &rows).render(area, &self.tool_state, frame);
    }

    fn render_repetitions(&self, area: Rect, frame: &mut Frame<'_>) {
        let (columns, rows) = repetition_table(&self.data);
        Table::new(&columns, &rows).render(area, &self.rep_state, frame);
    }

    fn render_artifact(&self, area: Rect, frame: &mut Frame<'_>) {
        let Some((title, text)) = &self.artifact else {
            frame.write_line(
                area,
                &Line::from("Select a repetition, then click Diff, Transcript, or Tool Calls."),
            );
            return;
        };
        frame.write_line(
            Rect::new(area.x, area.y, area.width, 1),
            &Line::from(title.as_str()),
        );
        for (row, line) in text
            .lines()
            .skip(self.artifact_scroll)
            .take(usize::from(area.height.saturating_sub(1)))
            .enumerate()
        {
            frame.write_line(
                Rect::new(
                    area.x,
                    area.y.saturating_add(1).saturating_add(usize_to_u16(row)),
                    area.width,
                    1,
                ),
                &Line::from(line),
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
}

impl ViewerTab {
    const COUNT: usize = 5;

    const fn index(self) -> usize {
        match self {
            Self::Overview => 0,
            Self::Cases => 1,
            Self::Tools => 2,
            Self::Repetitions => 3,
            Self::Artifact => 4,
        }
    }

    const fn from_index(index: usize) -> Self {
        match index {
            1 => Self::Cases,
            2 => Self::Tools,
            3 => Self::Repetitions,
            4 => Self::Artifact,
            _ => Self::Overview,
        }
    }
}

fn render_header(area: Rect, frame: &mut Frame<'_>, title: &str, status: &str) {
    if area.height == 0 {
        return;
    }
    let line = Line::from(format!("{title} — {status}"));
    frame.write_line(Rect::new(area.x, area.y, area.width, 1), &line);
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
                if run.passed { "yes" } else { "no" }.to_string(),
                run.variants.to_string(),
                run.winner.clone().unwrap_or_else(|| "n/a".to_string()),
            ])
        })
        .collect()
}

fn picker_actions() -> Vec<ActionButton> {
    vec![
        ActionButton::new("open", "Open"),
        ActionButton::new("refresh", "Refresh"),
        ActionButton::new("close", "Close"),
    ]
}

fn viewer_tabs() -> Vec<TabItem<'static>> {
    vec![
        TabItem::new("overview", "Overview"),
        TabItem::new("cases", "Cases"),
        TabItem::new("tools", "Tools"),
        TabItem::new("repetitions", "Repetitions"),
        TabItem::new("artifact", "Artifact"),
    ]
}

fn viewer_actions() -> Vec<ActionButton> {
    vec![
        ActionButton::new("diff", "Diff"),
        ActionButton::new("transcript", "Transcript"),
        ActionButton::new("tools", "Tool Calls"),
        ActionButton::new("refresh", "Refresh"),
        ActionButton::new("back", "Back"),
        ActionButton::new("close", "Close"),
    ]
}

fn overview_table(data: &EvalRunData) -> (Vec<TableColumn<'static>>, Vec<TableRow>) {
    let columns = vec![
        TableColumn::new("Variant").flex(2),
        TableColumn::new("Pass").fixed(8).align(TableAlign::Right),
        TableColumn::new("Score").fixed(8).align(TableAlign::Right),
        TableColumn::new("Avg Wall")
            .fixed(10)
            .align(TableAlign::Right),
        TableColumn::new("Avg Tokens")
            .fixed(11)
            .align(TableAlign::Right),
        TableColumn::new("Total Tokens")
            .fixed(12)
            .align(TableAlign::Right),
        TableColumn::new("Tools/Rep")
            .fixed(10)
            .align(TableAlign::Right),
        TableColumn::new("Errors").fixed(8).align(TableAlign::Right),
    ];
    let rows = data
        .result
        .variants
        .iter()
        .map(|variant| {
            let metrics = variant_metrics(variant);
            string_row(vec![
                variant.variant_id.clone(),
                format!("{:.1}%", variant.pass_rate * 100.0),
                format!("{:.3}", variant.score.overall),
                format_duration_ms(metrics.avg_wall_ms),
                format_number(metrics.avg_tokens),
                format_number(metrics.total_tokens),
                format!("{:.1}", metrics.avg_tool_calls),
                format_number(metrics.tool_errors),
            ])
        })
        .collect();
    (columns, rows)
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
                if repetition.passed { "yes" } else { "no" }.to_string(),
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
