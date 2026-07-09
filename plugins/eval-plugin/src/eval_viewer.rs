//! Plugin-owned eval picker and run viewer surfaces.

use crate::eval_data::{
    EvalCampaignData, EvalCampaignSummary, EvalRunData, EvalRunSummary, best_variant,
    case_avg_metric, diff_variant_count, discover_campaigns, discover_runs, format_duration_ms,
    format_number, load_repetition_artifact, run_avg_measurement, run_best_score, run_pass_rate,
    sum_variant_metric,
};
use bcode_eval_models::{EvalImprovementGeneration, EvalRepetitionResult};
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
    campaigns_root: PathBuf,
    runs: Vec<EvalRunSummary>,
    campaigns: Vec<EvalCampaignSummary>,
    table_state: TableState,
    action_state: ActionRowState,
    embedded_viewer: Option<EvalRunViewerSurface>,
    embedded_campaign: Option<EvalCampaignViewerSurface>,
    status: String,
    table_area: Rect,
    action_area: Rect,
}

impl EvalRunPickerSurface {
    /// Load picker from a runs root.
    #[must_use]
    pub fn load(runs_root: PathBuf) -> Self {
        let runs = discover_runs(&runs_root);
        let campaigns_root = runs_root.parent().map_or_else(
            || PathBuf::from("target/bcode-evals/improvements"),
            |parent| parent.join("improvements"),
        );
        let campaigns = discover_campaigns(&campaigns_root);
        let status = format!("{} runs, {} campaigns", runs.len(), campaigns.len());
        Self {
            runs_root,
            campaigns_root,
            runs,
            campaigns,
            table_state: TableState::new(Some(0)),
            action_state: ActionRowState::new(),
            embedded_viewer: None,
            embedded_campaign: None,
            status,
            table_area: Rect::new(0, 0, 0, 0),
            action_area: Rect::new(0, 0, 0, 0),
        }
    }

    fn refresh(&mut self) {
        self.runs = discover_runs(&self.runs_root);
        self.campaigns = discover_campaigns(&self.campaigns_root);
        let row_count = self.overview_rows().len();
        if row_count == 0 {
            self.table_state.set_selected(None);
        } else if self
            .table_state
            .selected()
            .is_none_or(|index| index >= row_count)
        {
            self.table_state.set_selected(Some(0));
        }
        self.status = format!(
            "{} runs in {}; {} campaigns in {}",
            self.runs.len(),
            self.runs_root.display(),
            self.campaigns.len(),
            self.campaigns_root.display()
        );
    }

    fn overview_rows(&self) -> Vec<OverviewRow> {
        let mut rows = self
            .campaigns
            .iter()
            .enumerate()
            .map(|(index, _)| OverviewRow::Campaign(index))
            .collect::<Vec<_>>();
        rows.extend(
            self.runs
                .iter()
                .enumerate()
                .map(|(index, _)| OverviewRow::Run(index)),
        );
        rows
    }

    /// Open the currently selected run or campaign, if any.
    pub fn open_selected(&mut self) {
        let Some(index) = self.table_state.selected() else {
            self.status = "no run selected".to_string();
            return;
        };
        let Some(row) = self.overview_rows().get(index).copied() else {
            self.status = "selected item no longer exists".to_string();
            return;
        };
        match row {
            OverviewRow::Run(run_index) => {
                let Some(run) = self.runs.get(run_index) else {
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
            OverviewRow::Campaign(campaign_index) => {
                let Some(campaign) = self.campaigns.get(campaign_index) else {
                    self.status = "selected campaign no longer exists".to_string();
                    return;
                };
                match EvalCampaignViewerSurface::load(campaign.campaign_dir.clone()) {
                    Ok(viewer) => {
                        self.embedded_campaign = Some(viewer);
                    }
                    Err(error) => {
                        self.status = format!("failed to open campaign: {error}");
                    }
                }
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
        if let Some(viewer) = self.embedded_campaign.as_mut() {
            viewer.render(area, frame);
            return;
        }
        render_header(area, frame, "Eval Overview", &self.status);
        let body = body_area(area);
        let (table_area, action_area, status_area) = split_body_actions(body);
        self.table_area = inset_top(table_area, 1);
        self.action_area = action_area;
        render_panel_title(table_area, frame, "Eval runs and improvement campaigns");
        let columns = overview_columns();
        let rows = overview_table_rows(&self.runs, &self.campaigns);
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
        if let Some(viewer) = self.embedded_campaign.as_mut() {
            let action = viewer.handle_event(event, host);
            if matches!(action, PluginTuiAction::Close { .. }) {
                self.embedded_campaign = None;
                return PluginTuiAction::Redraw;
            }
            return action;
        }
        let columns = overview_columns();
        let rows = overview_table_rows(&self.runs, &self.campaigns);
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

#[derive(Debug, Clone, Copy)]
enum OverviewRow {
    Run(usize),
    Campaign(usize),
}

/// Eval improvement campaign viewer surface.
pub struct EvalCampaignViewerSurface {
    data: EvalCampaignData,
    generation_state: TableState,
    action_state: ActionRowState,
    selected_run_viewer: Option<EvalRunViewerSurface>,
    status: String,
    table_area: Rect,
    action_area: Rect,
}

impl EvalCampaignViewerSurface {
    /// Load viewer for a campaign path.
    ///
    /// # Errors
    ///
    /// Returns an error when the campaign cannot be loaded.
    pub fn load(path: PathBuf) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let data = EvalCampaignData::load(path)?;
        let status = format!(
            "{} generations; best={}; latest={}",
            data.generations.len(),
            data.campaign
                .best_generation_id
                .clone()
                .unwrap_or_else(|| "n/a".to_string()),
            data.campaign
                .latest_generation_id
                .clone()
                .unwrap_or_else(|| "n/a".to_string())
        );
        Ok(Self {
            data,
            generation_state: TableState::new(Some(0)),
            action_state: ActionRowState::new(),
            selected_run_viewer: None,
            status,
            table_area: Rect::new(0, 0, 0, 0),
            action_area: Rect::new(0, 0, 0, 0),
        })
    }

    fn selected_generation(&self) -> Option<&EvalImprovementGeneration> {
        self.generation_state
            .selected()
            .and_then(|index| self.data.generations.get(index))
    }

    fn open_selected_run(&mut self) {
        let Some(generation) = self.selected_generation() else {
            self.status = "select a generation first".to_string();
            return;
        };
        let Some(run_dir) = generation.run_dir.clone() else {
            self.status = "selected generation has no run".to_string();
            return;
        };
        match EvalRunViewerSurface::load(run_dir) {
            Ok(viewer) => self.selected_run_viewer = Some(viewer),
            Err(error) => self.status = format!("failed to open generation run: {error}"),
        }
    }

    fn handle_action(&mut self, action: &str) -> PluginTuiAction {
        match action {
            "open-run" => self.open_selected_run(),
            "refresh" => match EvalCampaignData::load(&self.data.campaign_dir) {
                Ok(data) => {
                    self.data = data;
                    self.status = "reloaded campaign".to_string();
                }
                Err(error) => self.status = format!("reload failed: {error}"),
            },
            "back" | "close" => return PluginTuiAction::Close { outcome: None },
            _ => {}
        }
        PluginTuiAction::Redraw
    }
}

impl PluginTuiSurface for EvalCampaignViewerSurface {
    fn id(&self) -> &'static str {
        "bcode.eval-campaign-viewer"
    }

    fn title(&self) -> &'static str {
        "Eval Improvement Campaign"
    }

    fn render(&mut self, area: Rect, frame: &mut Frame<'_>) {
        if let Some(viewer) = self.selected_run_viewer.as_mut() {
            viewer.render(area, frame);
            return;
        }
        render_header(
            area,
            frame,
            &format!("Eval Campaign: {}", self.data.campaign.id),
            &self.status,
        );
        let body = body_area(area);
        let (table_area, action_area, status_area) = split_body_actions(body);
        self.table_area = inset_top(table_area, 1);
        self.action_area = action_area;
        render_panel_title(
            table_area,
            frame,
            "Generation timeline — A=parent, B=current",
        );
        let columns = campaign_columns();
        let rows = campaign_rows(&self.data);
        render_eval_table(
            frame,
            self.table_area,
            &columns,
            &rows,
            &self.generation_state,
        );
        themed_action_row(&campaign_actions()).render_state(action_area, &self.action_state, frame);
        render_status(
            status_area,
            frame,
            "Enter opens the B run. Rows show deltas, A/B pass and score changes. Esc returns.",
        );
    }

    fn handle_event(&mut self, event: &Event, host: &dyn PluginTuiHost) -> PluginTuiAction {
        if let Some(viewer) = self.selected_run_viewer.as_mut() {
            let action = viewer.handle_event(event, host);
            if matches!(action, PluginTuiAction::Close { .. }) {
                self.selected_run_viewer = None;
                return PluginTuiAction::Redraw;
            }
            return action;
        }
        let columns = campaign_columns();
        let rows = campaign_rows(&self.data);
        if handle_eval_table_event(
            self.table_area,
            &columns,
            &rows,
            &mut self.generation_state,
            event,
        ) {
            return PluginTuiAction::Redraw;
        }
        match themed_action_row(&campaign_actions()).handle_event(
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
                    self.open_selected_run();
                    return PluginTuiAction::Redraw;
                }
                KeyCode::Char('r') => return self.handle_action("refresh"),
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
            ViewerTab::Analysis => self.render_analysis(content_area, frame),
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
            ViewerTab::Overview
            | ViewerTab::Analysis
            | ViewerTab::Artifact
            | ViewerTab::Derivations => false,
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

    fn render_analysis(&self, area: Rect, frame: &mut Frame<'_>) {
        render_panel_title(area, frame, "Deep analysis and recommendations");
        let area = inset_top(area, 1);
        let top_height = area.height.min(6);
        self.render_recommendations(Rect::new(area.x, area.y, area.width, top_height), frame);

        let lower_y = area.y.saturating_add(top_height).saturating_add(1);
        let lower = Rect::new(
            area.x,
            lower_y,
            area.width,
            area.height.saturating_sub(top_height).saturating_sub(1),
        );
        let columns = split_columns(lower, 2, 2);
        if let Some(left) = columns.first().copied() {
            self.render_analysis_charts(left, frame);
        }
        if let Some(right) = columns.get(1).copied() {
            self.render_analysis_tables(right, frame);
        }
    }

    fn render_recommendations(&self, area: Rect, frame: &mut Frame<'_>) {
        frame.fill(area, " ", Style::new().bg(PANEL_ALT));
        let lines = recommendation_lines(&self.data);
        for (row, line) in lines.iter().take(usize::from(area.height)).enumerate() {
            frame.write_line_with_fallback_style(
                Rect::new(
                    area.x,
                    area.y.saturating_add(usize_to_u16(row)),
                    area.width,
                    1,
                ),
                line,
                Style::new().bg(PANEL_ALT),
            );
        }
    }

    fn render_analysis_charts(&self, area: Rect, frame: &mut Frame<'_>) {
        render_panel_title(area, frame, "Core graph stack");
        let area = inset_top(area, 1);
        let sections = split_rows(area, 4, 1);
        if let Some(section) = sections.first().copied() {
            self.render_score_profile(section, frame);
        }
        if let Some(section) = sections.get(1).copied() {
            Self::render_dense_bar_panel(
                section,
                frame,
                "Pass rate by variant",
                &variant_pass_items(&self.data),
                Some(100),
            );
        }
        if let Some(section) = sections.get(2).copied() {
            Self::render_dense_bar_panel(
                section,
                frame,
                "Cost frontier — total tokens",
                &variant_token_items(&self.data),
                None,
            );
        }
        if let Some(section) = sections.get(3).copied() {
            Self::render_dense_bar_panel(
                section,
                frame,
                "Latency frontier — avg wall time",
                &variant_latency_items(&self.data),
                None,
            );
        }
    }

    fn render_score_profile(&self, area: Rect, frame: &mut Frame<'_>) {
        let title = best_variant(&self.data.result).map_or_else(
            || "Score profile".to_string(),
            |variant| format!("Score profile — {}", variant.variant_id),
        );
        let items = score_profile_items(&self.data);
        Self::render_dense_bar_panel(area, frame, &title, &items, Some(100));
    }

    fn render_dense_bar_panel(
        area: Rect,
        frame: &mut Frame<'_>,
        title: &str,
        items: &[BarChartItem<'_>],
        max: Option<u64>,
    ) {
        frame.fill(area, " ", Style::new().bg(PANEL));
        render_panel_title(area, frame, title);
        let area = inset_top(area, 1);
        BarChart::new(items)
            .policy(
                BarChartPolicy::with_values()
                    .max(max)
                    .value_placement(BarChartValuePlacement::Right),
            )
            .styles(eval_bar_chart_styles())
            .empty("No graph data")
            .render(area, frame);
    }

    fn render_analysis_tables(&self, area: Rect, frame: &mut Frame<'_>) {
        render_panel_title(area, frame, "Failure intelligence");
        let area = inset_top(area, 1);
        let half = area.height / 2;
        let (flaky_columns, flaky_rows) = flakiness_table(&self.data);
        render_panel_title(
            Rect::new(area.x, area.y, area.width, 1),
            frame,
            "Most flaky cases",
        );
        render_eval_table(
            frame,
            Rect::new(
                area.x,
                area.y.saturating_add(1),
                area.width,
                half.saturating_sub(1),
            ),
            &flaky_columns,
            &flaky_rows,
            &TableState::new(None),
        );
        let (outlier_columns, outlier_rows) = outlier_table(&self.data);
        let outlier_y = area.y.saturating_add(half).saturating_add(1);
        render_panel_title(
            Rect::new(area.x, outlier_y, area.width, 1),
            frame,
            "Outliers",
        );
        render_eval_table(
            frame,
            Rect::new(
                area.x,
                outlier_y.saturating_add(1),
                area.width,
                area.height.saturating_sub(half).saturating_sub(2),
            ),
            &outlier_columns,
            &outlier_rows,
            &TableState::new(None),
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
    Analysis,
    Cases,
    Tools,
    Repetitions,
    Artifact,
    Derivations,
}

impl ViewerTab {
    const COUNT: usize = 7;

    const fn index(self) -> usize {
        match self {
            Self::Overview => 0,
            Self::Analysis => 1,
            Self::Cases => 2,
            Self::Tools => 3,
            Self::Repetitions => 4,
            Self::Artifact => 5,
            Self::Derivations => 6,
        }
    }

    const fn from_index(index: usize) -> Self {
        match index {
            1 => Self::Analysis,
            2 => Self::Cases,
            3 => Self::Tools,
            4 => Self::Repetitions,
            5 => Self::Artifact,
            6 => Self::Derivations,
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

fn split_rows(area: Rect, rows: u16, gap: u16) -> Vec<Rect> {
    if rows == 0 {
        return Vec::new();
    }
    let total_gap = gap.saturating_mul(rows.saturating_sub(1));
    let height = area.height.saturating_sub(total_gap) / rows;
    (0..rows)
        .map(|index| {
            let y = area
                .y
                .saturating_add(index.saturating_mul(height.saturating_add(gap)));
            Rect::new(
                area.x,
                y,
                area.width,
                if index + 1 == rows {
                    area.bottom().saturating_sub(y)
                } else {
                    height
                },
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

fn variant_latency_items(data: &EvalRunData) -> Vec<BarChartItem<'_>> {
    data.result
        .variants
        .iter()
        .map(|variant| {
            BarChartItem::new(
                variant.variant_id.as_str(),
                metric_to_u64(avg_variant_metric(variant, "wall_time_ms")),
            )
        })
        .collect()
}

fn score_profile_items(data: &EvalRunData) -> Vec<BarChartItem<'static>> {
    let Some(variant) = best_variant(&data.result) else {
        return Vec::new();
    };
    vec![
        BarChartItem::new("overall", score_percent(variant.score.overall)),
        BarChartItem::new("correct", score_percent(variant.score.correctness)),
        BarChartItem::new("speed", score_percent(variant.score.speed)),
        BarChartItem::new("cost", score_percent(variant.score.cost)),
        BarChartItem::new("stable", score_percent(variant.score.stability)),
        BarChartItem::new("efficient", score_percent(variant.score.efficiency)),
    ]
}

fn score_percent(score: f64) -> u64 {
    metric_to_u64(score * 100.0)
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

fn recommendation_lines(data: &EvalRunData) -> Vec<Line> {
    let metrics = run_dashboard_metrics(data);
    let winner = metrics.winner.as_deref().unwrap_or("none");
    let cheapest = cheapest_variant(data).unwrap_or_else(|| "none".to_string());
    let fastest = fastest_variant(data).unwrap_or_else(|| "none".to_string());
    let flaky = flakiness_records(data).into_iter().next();
    vec![
        Line::from_spans(vec![
            Span::styled(
                "  ◆ Recommendation  ",
                Style::new()
                    .fg(ACCENT)
                    .bg(PANEL_ALT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("winner={winner}"),
                Style::new()
                    .fg(TEXT)
                    .bg(PANEL_ALT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  risk={}", metrics.risk_label),
                Style::new()
                    .fg(metrics.risk_color)
                    .bg(PANEL_ALT)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from_spans(vec![
            Span::styled("  Why: ", Style::new().fg(MUTED).bg(PANEL_ALT)),
            Span::styled(
                format!(
                    "pass rate {:.1}%, avg {}, {} total tokens",
                    metrics.pass_rate * 100.0,
                    format_duration_ms(metrics.avg_wall_time_ms),
                    format_number(metrics.total_tokens)
                ),
                Style::new().fg(TEXT).bg(PANEL_ALT),
            ),
        ]),
        Line::from_spans(vec![
            Span::styled("  Frontier: ", Style::new().fg(MUTED).bg(PANEL_ALT)),
            Span::styled(
                format!("cheapest={cheapest}  fastest={fastest}"),
                Style::new().fg(TEXT).bg(PANEL_ALT),
            ),
        ]),
        Line::from_spans(vec![
            Span::styled(
                "  Watch: ",
                Style::new()
                    .fg(WARNING)
                    .bg(PANEL_ALT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                flaky.map_or_else(
                    || "no flaky case detected from repetitions".to_string(),
                    |record| {
                        format!(
                            "{} / {} has {:.0}% disagreement",
                            record.case_id,
                            record.variant_id,
                            record.flakiness * 100.0
                        )
                    },
                ),
                Style::new().fg(TEXT).bg(PANEL_ALT),
            ),
        ]),
    ]
}

fn cheapest_variant(data: &EvalRunData) -> Option<String> {
    data.result
        .variants
        .iter()
        .min_by(|left, right| {
            sum_variant_metric(left, "total_tokens")
                .total_cmp(&sum_variant_metric(right, "total_tokens"))
        })
        .map(|variant| variant.variant_id.clone())
}

fn fastest_variant(data: &EvalRunData) -> Option<String> {
    data.result
        .variants
        .iter()
        .min_by(|left, right| {
            avg_variant_metric(left, "wall_time_ms")
                .total_cmp(&avg_variant_metric(right, "wall_time_ms"))
        })
        .map(|variant| variant.variant_id.clone())
}

fn avg_variant_metric(variant: &bcode_eval_models::EvalVariantRunResult, metric_name: &str) -> f64 {
    let mut total = 0.0;
    let mut count = 0_usize;
    for repetition in variant
        .cases
        .iter()
        .flat_map(|case| case.repetitions.iter())
    {
        total += metric(repetition, metric_name);
        count = count.saturating_add(1);
    }
    average(total, count)
}

#[derive(Debug, Clone)]
struct FlakinessRecord {
    case_id: String,
    variant_id: String,
    pass_pattern: String,
    flakiness: f64,
}

fn flakiness_records(data: &EvalRunData) -> Vec<FlakinessRecord> {
    let mut records = Vec::new();
    for variant in &data.result.variants {
        for case in &variant.cases {
            if case.repetitions.is_empty() {
                continue;
            }
            let passed = case
                .repetitions
                .iter()
                .filter(|repetition| repetition.passed)
                .count();
            let pass_rate = usize_as_f64(passed) / usize_as_f64(case.repetitions.len());
            let flakiness = pass_rate.min(1.0 - pass_rate) * 2.0;
            let pass_pattern = case
                .repetitions
                .iter()
                .map(|repetition| if repetition.passed { '✓' } else { '✗' })
                .collect();
            records.push(FlakinessRecord {
                case_id: case.case_id.clone(),
                variant_id: variant.variant_id.clone(),
                pass_pattern,
                flakiness,
            });
        }
    }
    records.sort_by(|left, right| right.flakiness.total_cmp(&left.flakiness));
    records
}

fn flakiness_table(data: &EvalRunData) -> (Vec<TableColumn<'static>>, Vec<TableRow>) {
    let columns = vec![
        TableColumn::new("Case").flex(2),
        TableColumn::new("Variant").flex(2),
        TableColumn::new("Pattern").flex(1),
        TableColumn::new("Flaky").fixed(8).align(TableAlign::Right),
    ];
    let rows = flakiness_records(data)
        .into_iter()
        .take(8)
        .map(|record| {
            string_row(vec![
                record.case_id,
                record.variant_id,
                record.pass_pattern,
                format!("{:.0}%", record.flakiness * 100.0),
            ])
        })
        .collect();
    (columns, rows)
}

fn outlier_table(data: &EvalRunData) -> (Vec<TableColumn<'static>>, Vec<TableRow>) {
    let repetitions = data.repetitions();
    let median_latency = median_metric(&repetitions, "wall_time_ms");
    let median_tokens = median_metric(&repetitions, "total_tokens");
    let columns = vec![
        TableColumn::new("Case").flex(2),
        TableColumn::new("Variant").flex(2),
        TableColumn::new("Rep").fixed(5).align(TableAlign::Right),
        TableColumn::new("Signal").flex(2),
        TableColumn::new("Ratio").fixed(8).align(TableAlign::Right),
    ];
    let mut rows = Vec::new();
    for repetition in repetitions {
        let latency_ratio = ratio(metric(repetition, "wall_time_ms"), median_latency);
        let token_ratio = ratio(metric(repetition, "total_tokens"), median_tokens);
        let (signal, ratio_value) = if latency_ratio >= token_ratio {
            ("latency", latency_ratio)
        } else {
            ("tokens", token_ratio)
        };
        if ratio_value >= 1.5 {
            rows.push(string_row(vec![
                repetition.case_id.clone(),
                repetition.variant_id.clone(),
                repetition.repetition.to_string(),
                signal.to_string(),
                format!("{ratio_value:.1}x"),
            ]));
        }
    }
    rows.truncate(8);
    (columns, rows)
}

fn median_metric(repetitions: &[&EvalRepetitionResult], metric_name: &str) -> f64 {
    let mut values = repetitions
        .iter()
        .map(|repetition| metric(repetition, metric_name))
        .filter(|value| value.is_finite())
        .collect::<Vec<_>>();
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn ratio(value: f64, baseline: f64) -> f64 {
    if baseline <= 0.0 {
        0.0
    } else {
        value / baseline
    }
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

fn overview_columns<'a>() -> Vec<TableColumn<'a>> {
    vec![
        TableColumn::new("Type").fixed(10),
        TableColumn::new("Name").flex(3),
        TableColumn::new("Suite").flex(2),
        TableColumn::new("Status").flex(2),
        TableColumn::new("Best/Latest").flex(2),
    ]
}

fn overview_table_rows(
    runs: &[EvalRunSummary],
    campaigns: &[EvalCampaignSummary],
) -> Vec<TableRow> {
    let mut rows = campaigns
        .iter()
        .map(|campaign| {
            string_row(vec![
                "campaign".to_string(),
                campaign.campaign_id.clone(),
                campaign.suite_id.clone(),
                format!("{} generations", campaign.generations),
                format!(
                    "{}/{}",
                    campaign
                        .best_generation_id
                        .clone()
                        .unwrap_or_else(|| "n/a".to_string()),
                    campaign
                        .latest_generation_id
                        .clone()
                        .unwrap_or_else(|| "n/a".to_string())
                ),
            ])
        })
        .collect::<Vec<_>>();
    rows.extend(runs.iter().map(|run| {
        string_row(vec![
            "run".to_string(),
            run.run_id.clone(),
            run.suite_id.clone(),
            pass_label(run.passed),
            run.winner.clone().unwrap_or_else(|| "n/a".to_string()),
        ])
    }));
    rows
}

fn campaign_columns<'a>() -> Vec<TableColumn<'a>> {
    vec![
        TableColumn::new("Gen").fixed(8),
        TableColumn::new("Parent").fixed(8),
        TableColumn::new("Delta").flex(3),
        TableColumn::new("A Pass").fixed(8).align(TableAlign::Right),
        TableColumn::new("B Pass").fixed(8).align(TableAlign::Right),
        TableColumn::new("Score Δ")
            .fixed(9)
            .align(TableAlign::Right),
        TableColumn::new("Cost Δ").fixed(9).align(TableAlign::Right),
        TableColumn::new("Tokens Δ")
            .fixed(10)
            .align(TableAlign::Right),
        TableColumn::new("Verdict").fixed(12),
    ]
}

fn campaign_rows(data: &EvalCampaignData) -> Vec<TableRow> {
    data.generations
        .iter()
        .map(|generation| {
            let current = data.generation_run(generation);
            let parent = data
                .parent_generation(generation)
                .and_then(|parent| data.generation_run(parent));
            let a_pass = parent.as_ref().map_or_else(
                || "—".to_string(),
                |run| format!("{:.1}%", run_pass_rate(&run.result) * 100.0),
            );
            let b_pass = current.as_ref().map_or_else(
                || "—".to_string(),
                |run| format!("{:.1}%", run_pass_rate(&run.result) * 100.0),
            );
            let score_delta = match (parent.as_ref(), current.as_ref()) {
                (Some(parent), Some(current)) => {
                    format_signed(run_best_score(&current.result) - run_best_score(&parent.result))
                }
                _ => "—".to_string(),
            };
            let cost_delta = metric_delta(parent.as_ref(), current.as_ref(), "estimated_cost_usd")
                .map_or_else(|| "—".to_string(), format_signed);
            let token_delta = metric_delta(parent.as_ref(), current.as_ref(), "total_tokens")
                .map_or_else(|| "—".to_string(), format_signed_number);
            string_row(vec![
                generation.id.clone(),
                generation
                    .parent_id
                    .clone()
                    .unwrap_or_else(|| "—".to_string()),
                generation.delta.summary.clone(),
                a_pass,
                b_pass,
                score_delta,
                cost_delta,
                token_delta,
                format!("{:?}", generation.verdict.status),
            ])
        })
        .collect()
}

fn campaign_actions() -> Vec<ActionButton> {
    vec![
        ActionButton::new("open-run", "Enter Open B Run"),
        ActionButton::new("refresh", "R Refresh"),
        ActionButton::new("back", "Esc Back"),
    ]
}

fn metric_delta(
    parent: Option<&EvalRunData>,
    current: Option<&EvalRunData>,
    metric: &str,
) -> Option<f64> {
    let current = run_avg_measurement(&current?.result, metric)?;
    let parent = run_avg_measurement(&parent?.result, metric)?;
    Some(current - parent)
}

fn format_signed(value: f64) -> String {
    if value >= 0.0 {
        format!("+{value:.3}")
    } else {
        format!("{value:.3}")
    }
}

fn format_signed_number(value: f64) -> String {
    if value >= 0.0 {
        format!("+{}", format_number(value))
    } else {
        format!("-{}", format_number(value.abs()))
    }
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
        TabItem::new("analysis", "Analysis"),
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
        ViewerTab::Overview
        | ViewerTab::Analysis
        | ViewerTab::Cases
        | ViewerTab::Tools
        | ViewerTab::Derivations => vec![
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
